//! The assistant round's idle worker (0.9.0): two model passes, one thread.
//!
//! [`crate::digest`] and [`crate::translate`] are the same 1.9 GB local model
//! under the same jail as [`crate::enrich`], so they share a thread and they
//! share its gates. Giving each its own would mean three copies of llama-cli
//! able to be in flight at once on a machine whose whole budget for this is
//! four pinned cores.
//!
//! ## Gates, and why they are `enrich`'s and not new ones
//!
//! [`crate::enrich::gate`] is called directly rather than copied. The quality
//! worker has its own because it has its own queue ceiling in `[asr]`; these
//! two run the *same binary on the same model* as the enrichment pass, so a
//! second budget would be a second set of numbers describing one cost.
//!
//! On top of it, one rule of its own: **the enrichment queue comes first.**
//! Commitments are what somebody is waiting on; a paragraph about last night
//! and a translation of a turn already on screen are not. So the worker looks
//! at `graph_counts().threads_pending` and stands down while there is anything
//! left to enrich — which is what "runs after the enrichment queue is empty"
//! means in code.
//!
//! ## Order within the tick
//!
//! **Live translations, then digests, then the fair-share translation batch.**
//!
//! The middle two are 0.9.0's order and its reason stands: a digest is one call
//! per *conversation* and a translation is one per *turn*, so a backlog of
//! translations would otherwise starve the digest for as long as the backlog
//! lasted.
//!
//! The first is 0.11.0. A turn committed in a language the reader cannot read
//! goes onto `translate`'s live queue and rings [`wake`], and that queue is
//! drained before [`gate`] is consulted at all — because `gate` contains the
//! fair-share rule, and the fair-share rule can withhold the model for five
//! minutes. Five minutes is nothing for a paragraph about last night and it is
//! the difference between a caption and a footnote for the line on screen. The
//! gates that are about the *machine* rather than about fairness — the model
//! switched off, capture paused, a transcription backlog — are re-checked in
//! full before the live pass runs, and between every line inside it.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tracing::{debug, info, warn};

use crate::bus::Bus;
use crate::control::Control;
use crate::llm::Llm;
use crate::store::Store;

#[derive(Default)]
pub struct AssistStop(AtomicBool);

impl AssistStop {
    pub fn stop(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn stopped(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// What the two passes have done, for `status`.
#[derive(Debug, Default)]
pub struct AssistStats {
    /// When the assistant last got a turn (UTC ns), for the fair-share rule in
    /// [`gate`]. Zero until it has run once.
    pub last_pass_ns: std::sync::atomic::AtomicI64,
    pub digests_written: std::sync::atomic::AtomicU64,
    pub digests_refused: std::sync::atomic::AtomicU64,
    pub translated: std::sync::atomic::AtomicU64,
    pub translation_declined: std::sync::atomic::AtomicU64,
    pub reminders_fired: std::sync::atomic::AtomicU64,
}

impl AssistStats {
    pub fn to_json(&self) -> Value {
        json!({
            "digests_written": self.digests_written.load(Ordering::Relaxed),
            "digests_refused": self.digests_refused.load(Ordering::Relaxed),
            "translated": self.translated.load(Ordering::Relaxed),
            "translation_declined": self.translation_declined.load(Ordering::Relaxed),
            "reminders_fired": self.reminders_fired.load(Ordering::Relaxed),
            // 0.11.0. Not a stored fact like the others, so it is read from the
            // queue rather than from the rows: how many turns are waiting for a
            // live translation, and how many were dropped because more arrived
            // than `translate::LIVE_CAP`. A number that climbs means the model
            // is slower than the lobby is talking.
            "live_queued": crate::translate::live_queued(),
            "live_dropped": crate::translate::live_dropped(),
        })
    }
}

/// Why the worker may not run right now, or `None`.
///
/// `enrich`'s two rules plus one: promises before paragraphs.
pub fn gate(store: &Arc<std::sync::Mutex<Store>>, control: &Arc<Control>) -> Option<String> {
    let graph = control.graph();
    if !graph.enabled {
        return Some("the local model is switched off".to_string());
    }
    if let Some(reason) = crate::enrich::gate(control, &graph) {
        return Some(reason);
    }
    let pending = {
        let guard = store.lock().unwrap_or_else(|p| p.into_inner());
        guard.graph_counts().map(|c| c.threads_pending).unwrap_or(0)
    };
    // Promises first — but not promises ONLY. 0.10.1: with a few hundred
    // conversations queued and new ones arriving all evening, "stand down while
    // anything is left to enrich" meant the assistant never ran at all (four
    // hours live: 0 translations, 0 digests, 421 English turns waiting). So the
    // enrichment queue wins for `SHARE_EVERY_S` after each assistant pass, and
    // then the assistant gets one pass whatever the queue says.
    if pending > 0 {
        let last = control.assist_stats.last_pass_ns.load(Ordering::Relaxed);
        let since_s = (crate::clock::utc_now_ns().saturating_sub(last)) / 1_000_000_000;
        if last > 0 && since_s < SHARE_EVERY_S {
            return Some(format!(
                "{pending} conversation{} still waiting to be read for commitments — \
                 those come first; the assistant's next turn is in {}s",
                if pending == 1 { "" } else { "s" },
                SHARE_EVERY_S - since_s
            ));
        }
    }
    None
}

/// How long the enrichment queue may keep the assistant waiting between its
/// passes while it has work of its own.
pub const SHARE_EVERY_S: i64 = 300;

// ---- 0.11.0, waking on demand ----------------------------------------------
//
// The worker used to sleep `batch_pause_s` between looks, which is fine for
// work that is for tomorrow morning and useless for a caption. A committed turn
// in a language the reader cannot read now *rings this bell* — see
// `translate::push_live` — and the sleep below ends the moment it does.
//
// A condvar rather than a channel because the thing being signalled is "look
// again", not "here is an item": the item is in `translate`'s queue, and two
// turns arriving during one model call must be one wake-up, not two.

static WAKE: std::sync::LazyLock<(std::sync::Mutex<bool>, std::sync::Condvar)> =
    std::sync::LazyLock::new(Default::default);

/// Tell the worker to look now rather than at the end of its sleep.
///
/// Sticky: a wake that arrives while the worker is busy is remembered, so the
/// turn it was rung for cannot sit through a whole `batch_pause_s` because it
/// arrived a microsecond before the worker went back to sleep.
pub fn wake() {
    let (lock, cv) = &*WAKE;
    *lock.lock().unwrap_or_else(|p| p.into_inner()) = true;
    cv.notify_all();
}

/// Sleep until woken, stopped, or `pause` has passed. `true` if it was woken.
fn wait_for_work(pause: Duration, stop: &AssistStop) -> bool {
    let (lock, cv) = &*WAKE;
    let mut rung = lock.lock().unwrap_or_else(|p| p.into_inner());
    let deadline = std::time::Instant::now() + pause;
    while !*rung && !stop.stopped() {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break;
        }
        // Capped so a `stop` that arrives without a wake is still noticed
        // promptly — the same 200 ms the old sleep loop checked at.
        let (guard, _) = cv
            .wait_timeout(rung, left.min(Duration::from_millis(200)))
            .unwrap_or_else(|p| p.into_inner());
        rung = guard;
    }
    std::mem::replace(&mut *rung, false)
}

/// The background thread. Started whether or not anything here is enabled: all
/// three switches are live, so something has to be watching them.
#[allow(clippy::too_many_arguments)]
pub fn run(
    store: Arc<std::sync::Mutex<Store>>,
    control: Arc<Control>,
    bus: Arc<Bus>,
    models_root: Option<PathBuf>,
    runtime: crate::config::RuntimeConfig,
    cfg: crate::config::AssistConfig,
    stats: Arc<AssistStats>,
    stop: Arc<AssistStop>,
) {
    // The project's rule, applied here too: analysis never wins a scheduling
    // contest against a VR frame. The model calls are children that carry the
    // same nice and the same pin from `crate::llm`; this is for the thread that
    // spawns them and does the SQL between.
    crate::pipeline::deprioritise_current_thread(runtime.inference_nice, &runtime.inference_cpus);

    let mut llm: Option<Llm> = None;
    let mut resolved_for: Option<(String, String)> = None;
    let mut said_unavailable = false;
    let stopped = {
        let stop = Arc::clone(&stop);
        move || stop.stopped()
    };

    loop {
        if stop.stopped() {
            debug!("the assistant worker stopped");
            return;
        }
        let graph = control.graph();
        let mut worked = false;
        let resolve = |llm: &mut Option<Llm>, resolved_for: &mut Option<(String, String)>| {
            let key = (graph.llm_model.clone(), graph.llama_dir.clone());
            if resolved_for.as_ref() != Some(&key) || llm.is_none() {
                *llm = models_root
                    .as_deref()
                    .and_then(|root| Llm::resolve(root, &graph, &runtime));
                *resolved_for = Some(key);
            }
        };

        // ---- 0.11.0: the live queue, before anything else ------------------
        //
        // Ahead of `gate` on purpose, and it is the one ordering decision in
        // this file worth arguing about. `gate` is two different things wearing
        // one name: the machine's rules (the model is off, capture is paused,
        // the transcription queue is backed up) and the *fairness* rule that
        // hands the enrichment queue five minutes at a time. A translation of
        // the line on screen must obey the first and must not wait on the
        // second — a caption that arrives after the conversation has moved on
        // is not a caption. So the machine's rules are re-checked here in full,
        // via `enrich::gate` and the same `graph.enabled` switch `gate` reads,
        // and only the fair share is skipped.
        if graph.enabled
            && crate::translate::enabled()
            && crate::translate::live_queued() > 0
            && crate::enrich::gate(&control, &graph).is_none()
        {
            resolve(&mut llm, &mut resolved_for);
            // 0.11.0: the dedicated translator needs no graph model, so this
            // runs with `None` when NLLB is selected and the graph model is
            // absent; the Qwen path inside stands down on its own without one.
            if llm.is_some() || crate::translate::nllb_selected() {
                match crate::translate::drain_live(&store, &control, &bus, llm.as_ref(), &stopped) {
                    Ok(did) => worked |= did,
                    Err(e) => warn!("a live translation pass failed: {e:#}"),
                }
                refresh(&store, &cfg, &stats);
            }
        }

        if let Some(reason) = gate(&store, &control) {
            debug!("the assistant worker is standing down: {reason}");
        } else {
            resolve(&mut llm, &mut resolved_for);
            match llm.as_ref() {
                None => {
                    if !said_unavailable {
                        info!(
                            "the assistant's digests need the local model — \
                             `recalld models fetch --graph` installs it"
                        );
                        said_unavailable = true;
                    }
                    // 0.11.0: translation does not — the dedicated translator
                    // is its own model. Same batch, no graph model handed in.
                    if crate::translate::enabled() && crate::translate::nllb_selected() {
                        match crate::translate::batch(&store, &control, &bus, None, &cfg, &stopped)
                        {
                            Ok(did) => worked |= did,
                            Err(e) => warn!("a translation batch failed: {e:#}"),
                        }
                        refresh(&store, &cfg, &stats);
                        stats
                            .last_pass_ns
                            .store(crate::clock::utc_now_ns(), Ordering::Relaxed);
                    }
                }
                Some(llm) => {
                    // Digests first: one call per conversation against one call
                    // per turn, so a translation backlog cannot starve them.
                    if cfg.digest {
                        match crate::digest::batch(&store, &control, &bus, llm, &cfg, &stopped) {
                            Ok(did) => worked |= did,
                            Err(e) => warn!("a digest batch failed: {e:#}"),
                        }
                    }
                    // 0.10.2: the live switch, not `cfg`'s. `cfg` is a snapshot
                    // taken when this thread was spawned, and translation
                    // turned on from the Memory view has to start without a
                    // restart — see `translate::LIVE`.
                    if crate::translate::enabled() {
                        match crate::translate::batch(
                            &store,
                            &control,
                            &bus,
                            Some(llm),
                            &cfg,
                            &stopped,
                        ) {
                            Ok(did) => worked |= did,
                            Err(e) => warn!("a translation batch failed: {e:#}"),
                        }
                    }
                    refresh(&store, &cfg, &stats);
                    stats
                        .last_pass_ns
                        .store(crate::clock::utc_now_ns(), Ordering::Relaxed);
                }
            }
        }

        let pause = Duration::from_secs(cfg.batch_pause_s.max(1));
        // A worker that did something looks again sooner: a backlog should
        // drain at the pace of the model, not at the pace of the sleep.
        let pause = if worked { pause / 2 } else { pause };
        // …and a turn that needs translating now does not wait for either.
        wait_for_work(pause, &stop);
    }
}

/// Read the counters back out of the rows.
///
/// Read back rather than incremented in the passes: they are facts about what
/// is stored, and a counter kept in memory would disagree with the database
/// after a restart.
fn refresh(
    store: &Arc<std::sync::Mutex<Store>>,
    cfg: &crate::config::AssistConfig,
    stats: &AssistStats,
) {
    let guard = store.lock().unwrap_or_else(|p| p.into_inner());
    let settled = crate::clock::utc_now_ns() - cfg.digest_settle_min.max(0) * 60 * 1_000_000_000;
    if let Ok((written, refused, _)) = guard.digest_counts(settled, cfg.digest_min_turns.max(2)) {
        stats
            .digests_written
            .store(written as u64, Ordering::Relaxed);
        stats
            .digests_refused
            .store(refused as u64, Ordering::Relaxed);
    }
    if let Ok((done, declined)) = guard.translation_counts() {
        stats.translated.store(done as u64, Ordering::Relaxed);
        stats
            .translation_declined
            .store(declined as u64, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::allowlist::Allowlist;
    use crate::config::GraphConfig;
    use crate::store::SegmentAnalysis;

    const SEC: i64 = 1_000_000_000;

    fn rig() -> (Arc<std::sync::Mutex<Store>>, Arc<Control>) {
        let store = Store::open_in_memory().unwrap();
        store.upsert_source("VRChat.exe", "VRChat", 0).unwrap();
        let control = Control::new(
            PathBuf::from("/nonexistent"),
            None,
            &Allowlist::from_rules([("VRChat.exe", true)]),
        );
        (Arc::new(std::sync::Mutex::new(store)), control)
    }

    #[test]
    fn the_shipped_state_is_off_because_the_model_is() {
        let (store, control) = rig();
        let reason = gate(&store, &control).expect("blocked");
        assert!(reason.contains("switched off"), "{reason}");
    }

    #[test]
    fn commitments_come_before_paragraphs() {
        let (store, control) = rig();
        control.set_graph_enabled(true);
        assert_eq!(gate(&store, &control), None, "an empty database is clear");

        // A conversation nobody has enriched yet. The enrichment worker has
        // work, so this one has none: a promise is what somebody is waiting on.
        {
            let guard = store.lock().unwrap();
            let src = guard.upsert_source("VRChat.exe", "VRChat", 0).unwrap();
            let sess = guard.begin_session(src, 0).unwrap();
            let a = guard.mint_speaker(0).unwrap();
            let b = guard.mint_speaker(0).unwrap();
            for i in 0..4i64 {
                let t = i * 5 * SEC;
                let id = guard
                    .insert_segment(sess, t, t + 3 * SEC, "x.wav", t)
                    .unwrap();
                guard
                    .set_segment_analysis(
                        id,
                        &SegmentAnalysis {
                            text: Some(format!("das ist der einzige weg {i}")),
                            ..Default::default()
                        },
                    )
                    .unwrap();
                guard
                    .set_segment_speaker(id, Some(if i % 2 == 0 { a } else { b }), Some(0.8))
                    .unwrap();
                crate::threads::assign(&guard, &GraphConfig::default(), id).unwrap();
            }
            assert!(guard.graph_counts().unwrap().threads_pending > 0);
        }
        // Fresh daemon, never ran: the assistant gets its first pass at once
        // (0.10.1) — the old absolute priority starved it for ever.
        assert_eq!(
            gate(&store, &control),
            None,
            "a first pass is never withheld"
        );
        // Having just run, it yields to the enrichment queue…
        control
            .assist_stats
            .last_pass_ns
            .store(crate::clock::utc_now_ns(), Ordering::Relaxed);
        let reason = gate(&store, &control).expect("blocked");
        assert!(reason.contains("commitments"), "{reason}");
        // …until its share comes round again.
        control.assist_stats.last_pass_ns.store(
            crate::clock::utc_now_ns() - (SHARE_EVERY_S + 1) * 1_000_000_000,
            Ordering::Relaxed,
        );
        assert_eq!(gate(&store, &control), None, "the share came round");
        control
            .assist_stats
            .last_pass_ns
            .store(crate::clock::utc_now_ns(), Ordering::Relaxed);

        // Once the enrichment pass has walked it, the assistant may run.
        {
            let guard = store.lock().unwrap();
            for id in guard.unenriched_threads(0, 100).unwrap() {
                guard.mark_thread_enriched(id, 1).unwrap();
            }
        }
        assert_eq!(gate(&store, &control), None);
    }

    #[test]
    fn a_paused_daemon_writes_nothing_including_these() {
        let (store, control) = rig();
        control.set_graph_enabled(true);
        control.pause();
        let reason = gate(&store, &control).expect("blocked");
        assert!(reason.contains("paused"), "{reason}");
        control.resume();
        assert_eq!(gate(&store, &control), None);
    }

    #[test]
    fn a_turn_that_needs_translating_now_does_not_wait_out_the_sleep() {
        let stop = AssistStop::default();
        // Nothing rang: the sleep runs its course and says so.
        let t = std::time::Instant::now();
        assert!(!wait_for_work(Duration::from_millis(120), &stop));
        assert!(t.elapsed() >= Duration::from_millis(100), "it did sleep");

        // A wake that arrives *before* the wait is remembered rather than
        // missed — which is the whole race: the turn is committed on the
        // capture thread while this one is still finishing a model call.
        wake();
        let t = std::time::Instant::now();
        assert!(wait_for_work(Duration::from_secs(30), &stop));
        assert!(t.elapsed() < Duration::from_secs(1), "it woke at once");

        // And the bell is cleared by the wake it caused, not left ringing.
        assert!(!wait_for_work(Duration::from_millis(50), &stop));

        // A stopping worker does not sleep out its pause either.
        stop.stop();
        let t = std::time::Instant::now();
        wait_for_work(Duration::from_secs(30), &stop);
        assert!(t.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn the_live_queue_answers_to_the_machines_gates_and_not_to_the_fair_share() {
        let (store, control) = rig();
        control.set_graph_enabled(true);
        let graph = control.graph();
        // Nothing wrong with the machine: the live pass may run.
        assert_eq!(crate::enrich::gate(&control, &graph), None);
        // Now make the fair-share rule bite by pretending the assistant just
        // had its turn while conversations wait. `gate` refuses…
        {
            let guard = store.lock().unwrap();
            let src = guard.upsert_source("VRChat.exe", "VRChat", 0).unwrap();
            let sess = guard.begin_session(src, 0).unwrap();
            let a = guard.mint_speaker(0).unwrap();
            for i in 0..4i64 {
                let t = i * 5 * SEC;
                let id = guard
                    .insert_segment(sess, t, t + 3 * SEC, "x.wav", t)
                    .unwrap();
                guard
                    .set_segment_analysis(
                        id,
                        &SegmentAnalysis {
                            text: Some(format!("das ist der einzige weg {i}")),
                            ..Default::default()
                        },
                    )
                    .unwrap();
                guard.set_segment_speaker(id, Some(a), Some(0.8)).unwrap();
                crate::threads::assign(&guard, &GraphConfig::default(), id).unwrap();
            }
        }
        control
            .assist_stats
            .last_pass_ns
            .store(crate::clock::utc_now_ns(), Ordering::Relaxed);
        assert!(
            gate(&store, &control).is_some_and(|r| r.contains("commitments")),
            "the fair share is in force"
        );
        // …and the live pass, which reads `enrich::gate` rather than `gate`,
        // still runs. A digest is for tomorrow morning; a caption is not.
        assert_eq!(crate::enrich::gate(&control, &control.graph()), None);

        // Pause, though, stops both. There is no such thing as work a paused
        // daemon may do.
        control.pause();
        assert!(crate::enrich::gate(&control, &control.graph()).is_some());
        assert!(gate(&store, &control).is_some());
    }

    #[test]
    fn the_counters_are_read_back_from_the_rows() {
        let stats = AssistStats::default();
        let v = stats.to_json();
        for key in [
            "digests_written",
            "digests_refused",
            "translated",
            "translation_declined",
            "reminders_fired",
            "live_queued",
            "live_dropped",
        ] {
            assert_eq!(v[key], json!(0), "{key}");
        }
    }
}
