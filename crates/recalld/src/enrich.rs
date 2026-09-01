//! The background enrichment pass (GRAPH.md Tier 3).
//!
//! One background thread, walking conversations nobody has looked at yet,
//! newest first. It does two things per conversation: ask the model whether
//! anybody promised anything ([`crate::llm::Llm::commitment`]) and ask it what
//! the conversation was about. Everything it writes is an annotation
//! referencing segments, never a modification of one.
//!
//! ## Why it no longer waits for an idle machine (0.7.2)
//!
//! It used to. Through 0.7.1 there was a fifth gate — "no allowed application
//! has an open stream", §4's own game detection, reused — and the gate defeated
//! the feature it was protecting. A pass that stands down the moment VRChat
//! opens a stream is a pass that never runs while there is anything to enrich:
//! the conversations worth reading happen *during* the evening, and their
//! promises would surface hours after the evening they were made in, if the
//! machine ever went idle at all.
//!
//! **The protection was never the schedule. It is the jail.** Every model call
//! is a child process pinned to `[runtime] inference_cpus` and dropped to nice
//! 19 between fork and exec ([`crate::llm`]), so llama.cpp's own threads
//! inherit both. It cannot take a core the capture path is pinned away from,
//! and on the cores it does share it loses every scheduling contest it enters
//! by construction — which is the actual content of the rule "analysis never
//! wins against a VR frame". Standing down entirely was a second, cruder copy
//! of a promise the scheduler was already keeping, and it cost the feature its
//! reason to exist. How much of the machine it may use is now a setting
//! (`[graph].llm_threads`, live over `graph.set`), which is the honest shape of
//! that trade: enabled means running.
//!
//! ## The gates, in the order they are checked
//!
//! Every one of these is re-checked **between conversations**, not once at the
//! top of the loop, so the worker stands down within one model call of anything
//! changing:
//!
//! 1. **`[graph].enabled`** — off by default, and the switch is live.
//! 2. **The model is installed.** An optional 1.9 GB download; not having it is
//!    a normal state and the worker says so rather than erroring.
//! 3. **Capture is not paused.** Pause means nothing is written down. A
//!    background pass writing derived rows through a pause would make that
//!    sentence false, and it is the sentence the panic button rests on.
//! 4. **The capture queue is short.** A burst of turns waiting for ASR means
//!    the machine is busy being a tape recorder, which is the job that matters.
//!    Unlike the gate that went, this one is transient by construction: it
//!    clears as soon as the queue drains, and it is re-checked between
//!    conversations rather than deciding the worker's whole evening.
//!
//! ## Budget and cancellation
//!
//! Work is done in bounded batches (`[graph].batch_threads`), with a pause
//! between them, and the loop checks its stop flag and its gates between every
//! conversation. A model call that wedges is killed by its own timeout
//! (`crate::llm`), so the longest a stop can take is one timeout — never
//! forever.
//!
//! Progress goes out on the ops topic, the same channel a bulk delete uses, so
//! a client renders it with the code it already has.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use serde_json::{Value, json};
use tracing::{debug, info, warn};

use crate::bus::{Bus, Topic};
use crate::clock::utc_now_ns;
use crate::config::GraphConfig;
use crate::control::Control;
use crate::llm::{Line, Llm};
use crate::store::{NewCommitment, Store, commitment_source};

/// Windows per conversation the commitment pass will spend. A window is
/// `[graph].llm_window_turns` turns, so the default budget covers a
/// seventy-turn conversation — well past where a single conversation stops
/// being one.
const MAX_WINDOWS: usize = 6;

/// Turns of overlap between windows, so a promise and the request that prompted
/// it are never split across a boundary.
const WINDOW_OVERLAP: usize = 2;

/// What a Tier 3 row's `confidence` records.
///
/// **The bake-off's measured precision, not a per-answer score.** The model
/// does not report one, and inventing a per-row number would be worse than
/// reporting the one that was actually measured: 9/9 trap rejections and 9/9 on
/// who-and-what over the gold set (`spike/graph_bench`). It sits above every
/// rule match by construction, which is what the upgrade rule in
/// `Store::upsert_commitment` acts on.
pub const LLM_CONFIDENCE: f64 = 0.75;

/// What the worker is doing, as a client sees it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Phase {
    /// `[graph].enabled` is false. The shipped state.
    #[default]
    Off,
    /// On, but the model is not installed — `models fetch --graph` installs it.
    Unavailable,
    /// On and installed, but a gate is closed. `reason` says which.
    Blocked,
    /// On, installed, nothing in the way, and nothing left to do.
    Idle,
    /// Working through a batch.
    Running,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Off => "off",
            Phase::Unavailable => "unavailable",
            Phase::Blocked => "blocked",
            Phase::Idle => "idle",
            Phase::Running => "running",
        }
    }
}

/// The worker's report on itself. Read by `graph.summary` and pushed as a
/// `graph` event whenever it changes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphState {
    pub phase: Phase,
    /// Why the phase is what it is, in a sentence a person can act on. Always
    /// present for `Blocked` and `Unavailable`.
    pub reason: Option<String>,
    /// The conversation being worked on, if any.
    pub thread_id: Option<i64>,
    pub batch_done: usize,
    pub batch_total: usize,
    /// Conversations walked since the daemon started.
    pub walked: u64,
    /// Commitments the model wrote, and rule guesses it took back.
    pub found: u64,
    pub retracted: u64,
    pub labelled: u64,
    /// The last thing that went wrong, kept so a client can show it once rather
    /// than a log nobody reads.
    pub last_error: Option<String>,
    pub last_run_utc_ns: Option<i64>,
}

impl GraphState {
    /// Would a client render these two identically? Progress within a batch
    /// counts as a change; the counters alone do not, because they only move
    /// when something else already did.
    pub fn same_to_a_client(&self, other: &Self) -> bool {
        self.phase == other.phase
            && self.reason == other.reason
            && self.thread_id == other.thread_id
            && self.batch_done == other.batch_done
            && self.batch_total == other.batch_total
            && self.last_error == other.last_error
    }

    pub fn to_json(&self) -> Value {
        json!({
            "phase": self.phase.as_str(),
            "reason": self.reason,
            "thread": self.thread_id,
            "batch_done": self.batch_done,
            "batch_total": self.batch_total,
            "walked": self.walked,
            "found": self.found,
            "retracted": self.retracted,
            "labelled": self.labelled,
            "last_error": self.last_error,
            "last_run_utc_ns": self.last_run_utc_ns.map(|v| v.to_string()),
        })
    }

    fn blocked(reason: impl Into<String>, carry: &GraphState) -> Self {
        Self {
            phase: Phase::Blocked,
            reason: Some(reason.into()),
            thread_id: None,
            batch_done: 0,
            batch_total: 0,
            ..carry.clone()
        }
    }
}

#[derive(Default)]
pub struct EnrichStop(AtomicBool);

impl EnrichStop {
    pub fn stop(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn stopped(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Why the worker may not run right now, or `None` for "go ahead".
///
/// Separated from the loop so the rules are one readable function that a test
/// can drive directly — the scheduling is the feature here, and a scheduling
/// rule buried in a loop is a scheduling rule nobody can check.
///
/// Two rules, both transient. Nothing here asks what the machine is *doing*:
/// live capture is exactly when there is most to enrich, and the pinned cores
/// and nice 19 in [`crate::llm`] are what keeps that affordable (see the module
/// note above).
pub fn gate(control: &Control, cfg: &GraphConfig) -> Option<String> {
    if control.is_paused() {
        return Some("capture is paused — nothing is written down, including this".to_string());
    }
    let queued = control
        .queue
        .as_ref()
        .map(|q| (q.queued_samples() as f64 / crate::config::SAMPLE_RATE as f64).round() as i64)
        .unwrap_or(0);
    if queued > cfg.max_queue_seconds {
        return Some(format!(
            "{queued}s of audio is still waiting to be transcribed — capture comes first"
        ));
    }
    None
}

/// One conversation, enriched. Returns `(commitments written, guesses
/// retracted, topic written)`.
pub fn enrich_thread(
    store: &Store,
    llm: &Llm,
    cfg: &GraphConfig,
    thread_id: i64,
    at_utc_ns: i64,
) -> Result<(usize, usize, bool)> {
    let lines: Vec<Line> = store
        .thread_lines(thread_id)?
        .into_iter()
        .map(|l| Line {
            segment_id: l.segment_id,
            speaker_id: l.speaker_id,
            t_start_ns: l.t_start_ns,
            text: l.text,
        })
        .collect();
    if lines.is_empty() {
        store.mark_thread_enriched(thread_id, at_utc_ns)?;
        return Ok((0, 0, false));
    }

    let width = cfg.llm_window_turns.max(4);
    let step = width.saturating_sub(WINDOW_OVERLAP).max(1);
    let mut found = 0usize;
    let mut retracted = 0usize;

    for window in lines
        .chunks(1)
        .step_by(step)
        .take(MAX_WINDOWS)
        .enumerate()
        .filter_map(|(i, _)| {
            let start = i * step;
            (start < lines.len()).then(|| &lines[start..(start + width).min(lines.len())])
        })
    {
        match llm.commitment(window) {
            Ok(Some(found_it)) => {
                let roster = crate::llm::roster(window);
                let Some(&who) = roster.get(found_it.who) else {
                    continue;
                };
                // The promise is the LAST thing that speaker said in the
                // window. Short windows are the regime this model is strong in
                // and the regime the bench measured, so "last" is almost always
                // "the line it just read"; picking the first would attach a
                // promise to whatever they happened to open with.
                let Some(line) = window.iter().rev().find(|l| l.speaker_id == Some(who)) else {
                    continue;
                };
                // The due phrase comes back in the language it was said in, so
                // it is resolved by the same Tier 2 parser, against the same
                // capture time. One clock, one calendar, two tiers.
                let due = found_it
                    .due
                    .as_deref()
                    .map(|phrase| crate::timeref::extract(phrase, line.t_start_ns))
                    .and_then(|refs| refs.into_iter().next());
                let others: Vec<i64> = roster.iter().copied().filter(|s| *s != who).collect();
                store.upsert_commitment(
                    &NewCommitment {
                        segment_id: line.segment_id,
                        thread_id: Some(thread_id),
                        who_speaker_id: Some(who),
                        to_speaker_id: (others.len() == 1).then(|| others[0]),
                        what: found_it.what.clone(),
                        due_utc_ns: due.as_ref().map(|d| d.resolved_utc_ns),
                        // The model's phrase, not the parser's match: what the
                        // person actually said is the evidence, and the parser
                        // may have found only part of it.
                        due_raw: found_it.due.clone(),
                        due_kind: due.as_ref().map(|d| d.kind.to_string()),
                        source: commitment_source::LLM,
                        model_id: Some(llm.model_id().to_string()),
                        confidence: LLM_CONFIDENCE,
                    },
                    at_utc_ns,
                )?;
                found += 1;
            }
            Ok(None) => {
                // The refusal is worth as much as the extraction — arguably
                // more. Every rule guess in this window that a person has not
                // touched goes, because the model looked at the same words and
                // said there was nothing there.
                for line in window {
                    if store.retract_rule_candidate(line.segment_id)? {
                        retracted += 1;
                    }
                }
            }
            Err(e) => {
                // One window failing is not a reason to abandon the
                // conversation, but it IS a reason not to mark it walked.
                warn!(thread_id, "the model failed on a window: {e:#}");
                return Err(e);
            }
        }
    }

    // One label per conversation, from its opening window: what a conversation
    // is about is set early, and paying for a model call per window to
    // re-decide it would spend the budget on a question nobody asked.
    let head = &lines[..cfg.llm_window_turns.max(4).min(lines.len())];
    let topic = llm.topic(head)?;
    store.set_thread_topic(thread_id, topic.as_deref(), llm.model_id(), at_utc_ns)?;
    Ok((found, retracted, topic.is_some()))
}

/// The background thread. Started by the daemon whether or not Tier 3 is
/// enabled: the switch is live, so something has to be watching it.
pub fn run(
    store: Arc<std::sync::Mutex<Store>>,
    control: Arc<Control>,
    bus: Arc<Bus>,
    models_root: Option<PathBuf>,
    runtime: crate::config::RuntimeConfig,
    stop: Arc<EnrichStop>,
) {
    let mut state = GraphState::default();
    let mut ops = 0u64;
    // Resolved lazily and re-resolved when the settings move: the model may be
    // fetched while the daemon is running, and a user who has just run
    // `models fetch --graph` should not have to restart anything.
    let mut llm: Option<Llm> = None;
    let mut resolved_for: Option<(String, String)> = None;

    loop {
        if stop.stopped() {
            debug!("enrichment worker stopped");
            return;
        }
        let cfg = control.graph();

        let next = if !cfg.enabled {
            GraphState {
                phase: Phase::Off,
                reason: None,
                thread_id: None,
                batch_done: 0,
                batch_total: 0,
                ..state.clone()
            }
        } else {
            let key = (cfg.llm_model.clone(), cfg.llama_dir.clone());
            if resolved_for.as_ref() != Some(&key) || llm.is_none() {
                llm = models_root
                    .as_deref()
                    .and_then(|root| Llm::resolve(root, &cfg, &runtime));
                resolved_for = Some(key);
            }
            match (&llm, gate(&control, &cfg)) {
                (None, _) => GraphState {
                    phase: Phase::Unavailable,
                    reason: Some(
                        "the local model is not installed — `recalld models fetch --graph` \
                         downloads it (1.9 GB)"
                            .into(),
                    ),
                    thread_id: None,
                    batch_done: 0,
                    batch_total: 0,
                    ..state.clone()
                },
                (Some(_), Some(reason)) => GraphState::blocked(reason, &state),
                (Some(llm), None) => {
                    match batch(
                        &store, &control, &bus, llm, &cfg, &stop, &mut state, &mut ops,
                    ) {
                        Ok(true) => state.clone(),
                        Ok(false) => GraphState {
                            phase: Phase::Idle,
                            reason: None,
                            thread_id: None,
                            batch_done: 0,
                            batch_total: 0,
                            ..state.clone()
                        },
                        Err(e) => {
                            warn!("enrichment batch failed: {e:#}");
                            GraphState {
                                phase: Phase::Idle,
                                reason: None,
                                thread_id: None,
                                batch_done: 0,
                                batch_total: 0,
                                last_error: Some(format!("{e:#}")),
                                ..state.clone()
                            }
                        }
                    }
                }
            }
        };
        publish(&control, &bus, &mut state, next);

        // Background work has no deadline; being invisible matters more. The sleep is
        // in small steps so a stop is honoured promptly.
        let pause = Duration::from_secs(cfg.batch_pause_s.max(1));
        let step = Duration::from_millis(200);
        let mut slept = Duration::ZERO;
        while slept < pause {
            if stop.stopped() || control.graph().enabled != cfg.enabled {
                break;
            }
            std::thread::sleep(step);
            slept += step;
        }
    }
}

/// One batch. `Ok(true)` means work was done, `Ok(false)` that there was none.
#[allow(clippy::too_many_arguments)]
fn batch(
    store: &Arc<std::sync::Mutex<Store>>,
    control: &Arc<Control>,
    bus: &Bus,
    llm: &Llm,
    cfg: &GraphConfig,
    stop: &EnrichStop,
    state: &mut GraphState,
    ops: &mut u64,
) -> Result<bool> {
    let ids = {
        let guard = store.lock().unwrap_or_else(|p| p.into_inner());
        guard.unenriched_threads(cfg.min_thread_segments, cfg.batch_threads.max(1))?
    };
    if ids.is_empty() {
        return Ok(false);
    }

    *ops += 1;
    let op = format!("graph_{ops}");
    let total = ids.len();
    state.phase = Phase::Running;
    state.batch_total = total;
    state.batch_done = 0;
    state.reason = None;

    for (i, thread_id) in ids.into_iter().enumerate() {
        // Every gate, between every conversation. This is what "budgeted and
        // interruptible" means in practice: the worst case for standing down is
        // one model call, and a model call has its own timeout.
        if stop.stopped() || !control.graph().enabled {
            break;
        }
        if let Some(reason) = gate(control, cfg) {
            info!(%op, "standing down mid-batch: {reason}");
            bus.publish(
                Topic::Ops,
                "op.done",
                json!({
                    "op": op, "kind": "graph.enrich",
                    "done": i, "total": total, "stood_down": reason,
                }),
            );
            state.batch_total = 0;
            state.batch_done = 0;
            return Ok(true);
        }
        state.thread_id = Some(thread_id);
        state.batch_done = i;
        publish_state(control, bus, state);

        // `-t` is an argument to an invocation, so the live setting is read
        // here rather than at resolve time: turning the model threads down
        // applies to the next conversation, and never to one already open.
        let tuned = llm.with_threads(control.graph().llm_threads);
        let at = utc_now_ns();
        let outcome = {
            let guard = store.lock().unwrap_or_else(|p| p.into_inner());
            enrich_thread(&guard, &tuned, cfg, thread_id, at)
        };
        match outcome {
            Ok((found, retracted, labelled)) => {
                state.walked += 1;
                state.found += found as u64;
                state.retracted += retracted as u64;
                state.labelled += u64::from(labelled);
                state.last_run_utc_ns = Some(at);
                state.last_error = None;
            }
            Err(e) => {
                // The conversation is left unmarked so a later pass retries it.
                warn!(thread_id, "could not enrich a conversation: {e:#}");
                state.last_error = Some(format!("{e:#}"));
            }
        }
        state.batch_done = i + 1;
        bus.publish(
            Topic::Ops,
            "op.progress",
            json!({
                "op": op,
                "kind": "graph.enrich",
                "done": state.batch_done,
                "total": total,
                "frac": state.batch_done as f64 / total as f64,
            }),
        );
        publish_state(control, bus, state);
    }

    info!(
        %op,
        walked = state.walked,
        found = state.found,
        retracted = state.retracted,
        "enrichment batch"
    );
    bus.publish(
        Topic::Ops,
        "op.done",
        json!({
            "op": op, "kind": "graph.enrich",
            "done": state.batch_done, "total": total,
            "found": state.found, "retracted": state.retracted, "labelled": state.labelled,
        }),
    );
    state.thread_id = None;
    state.batch_total = 0;
    state.batch_done = 0;
    Ok(true)
}

fn publish(control: &Control, bus: &Bus, state: &mut GraphState, next: GraphState) {
    *state = next;
    publish_state(control, bus, state);
}

/// Push the worker's state, but only when a client would render it differently.
/// A loop that ticks every twenty seconds must not push a hundred and eighty
/// identical events an hour into everybody's replay buffer.
fn publish_state(control: &Control, bus: &Bus, state: &GraphState) {
    if control.set_graph_state(state.clone()) {
        bus.publish(Topic::Status, "graph", state.to_json());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::allowlist::Allowlist;
    use crate::store::KIND_MIC;

    fn control(store: &Store) -> Arc<Control> {
        let _ = store;
        Control::new(
            PathBuf::from("/nonexistent"),
            None,
            &Allowlist::from_rules([("VRChat.exe", true), ("firefox", false)]),
        )
    }

    fn store_with_sources() -> Store {
        let store = Store::open_in_memory().expect("store");
        store.upsert_source("VRChat.exe", "VRChat", 0).expect("src");
        store.upsert_source("firefox", "Firefox", 0).expect("src");
        store
            .upsert_source_kind("mic", "Microphone", KIND_MIC, 0)
            .expect("mic");
        store
    }

    /// The 0.7.2 correction, asserted rather than remembered.
    ///
    /// Through 0.7.1 an allowed application with an open stream stood the
    /// worker down — and that is precisely the hour the feature exists for. A
    /// promise is made in a live lobby; extracting it after the lobby empties
    /// is extracting it too late. The cost is paid by the pinned cores and
    /// nice 19 in `crate::llm`, not by refusing to run.
    #[test]
    fn a_captured_application_with_an_open_stream_does_not_stop_the_worker() {
        let store = store_with_sources();
        let control = control(&store);
        let cfg = GraphConfig::default();
        assert_eq!(gate(&control, &cfg), None, "an idle machine is clear");

        // The exact condition 0.7.1 blocked on: an allowed app, capturing, with
        // an open stream. Asserted here so the test fails if the situation it
        // is about stops being reachable.
        let src = store.upsert_source("VRChat.exe", "VRChat", 0).unwrap();
        let session = store.begin_session(src, 0).unwrap();
        let sources = store.list_sources().unwrap();
        let allowlist = control.allowlist();
        assert!(
            sources.iter().any(|s| s.match_key == "VRChat.exe"
                && s.kind == crate::store::KIND_APP
                && s.streams > 0
                && allowlist.decide(&s.match_key).captures()),
            "the test no longer sets up a captured app with an open stream"
        );
        assert!(!control.is_paused());

        assert_eq!(
            gate(&control, &cfg),
            None,
            "enrichment stood down while the user was in a lobby — that is the \
             hour it exists for"
        );
        store.end_session(session, 1).unwrap();
        assert_eq!(gate(&control, &cfg), None);
    }

    /// Pause means nothing is written down. That has to include this.
    #[test]
    fn a_paused_daemon_writes_nothing_including_derived_rows() {
        let store = store_with_sources();
        let control = control(&store);
        control.pause();
        let reason = gate(&control, &GraphConfig::default()).expect("blocked");
        assert!(reason.contains("paused"), "{reason}");
        control.resume();
        assert_eq!(gate(&control, &GraphConfig::default()), None);
    }

    /// A microphone session is not a reason to stand down either — it is open
    /// exactly when somebody is talking, which is when there is most to enrich.
    #[test]
    fn the_microphone_being_open_is_not_a_reason_to_stand_down() {
        let store = store_with_sources();
        let control = control(&store);
        store.set_allowed("mic", true, 0).unwrap();
        let src = store
            .upsert_source_kind("mic", "Microphone", KIND_MIC, 0)
            .unwrap();
        store.begin_session(src, 0).unwrap();
        assert_eq!(gate(&control, &GraphConfig::default()), None);
    }

    #[test]
    fn a_backlog_of_audio_waiting_for_asr_blocks_the_worker() {
        let queue = crate::queue::EventQueue::for_seconds(30.0, crate::config::SAMPLE_RATE);
        let control = Control::new(
            PathBuf::from("/nonexistent"),
            None,
            &Allowlist::from_rules([("VRChat.exe", true)]),
        )
        .with_pipeline(
            Arc::clone(&queue),
            Arc::new(crate::pipeline::Stats::default()),
            Arc::new(crate::analysis::AnalysisStats::default()),
        );
        let cfg = GraphConfig::default();
        assert_eq!(gate(&control, &cfg), None);

        // Ten seconds of audio queued, against a five-second ceiling.
        queue.push(crate::queue::CaptureEvent::Audio(
            crate::queue::AudioChunk {
                session_id: 1,
                capture_mono_ns: 0,
                samples: vec![0.0f32; crate::config::SAMPLE_RATE as usize * 10],
            },
        ));
        let reason = gate(&control, &cfg).expect("blocked");
        assert!(reason.contains("transcribed"), "{reason}");
    }

    #[test]
    fn the_switch_is_live_and_off_is_the_shipped_state() {
        let store = store_with_sources();
        let control = control(&store);
        assert!(!control.graph().enabled);
        assert!(control.set_graph_enabled(true).enabled);
        assert!(control.graph().enabled);
        assert!(!control.set_graph_enabled(false).enabled);

        // The tuning knobs are clamped rather than trusted: a client that asks
        // for zero threads must not get a daemon that cannot run the model, and
        // one that asks for two hundred must not get a daemon that tries.
        let tuned = control.set_graph_tuning(Some(0), Some(-4));
        assert_eq!(tuned.llm_threads, *crate::control::GRAPH_THREADS.start());
        assert_eq!(tuned.llm_threads, 1);
        assert_eq!(tuned.gpu_layers, 0);
        assert_eq!(
            control.set_graph_tuning(Some(200), None).llm_threads,
            *crate::control::GRAPH_THREADS.end(),
        );
        assert_eq!(*crate::control::GRAPH_THREADS.end(), 32);
        assert_eq!(control.set_graph_tuning(Some(6), None).llm_threads, 6);
        // The default is inside the range a client will offer, or the stepper
        // would open on a value it cannot show.
        assert!(GraphConfig::default().llm_threads <= *crate::control::GRAPH_THREADS.end());
    }

    #[test]
    fn the_state_only_counts_as_changed_when_a_client_would_see_it() {
        let store = store_with_sources();
        let control = control(&store);
        // The worker starts in the state the daemon starts in, so its first
        // report is usually not news — a client that has just connected asks
        // `graph.summary` rather than waiting for an event.
        let off = GraphState::default();
        assert!(!control.set_graph_state(off.clone()));
        assert!(!control.set_graph_state(off.clone()));

        let mut counting = off.clone();
        counting.walked = 12;
        assert!(
            !control.set_graph_state(counting),
            "a counter moving is not something a client re-renders for"
        );

        let mut running = off.clone();
        running.phase = Phase::Running;
        running.batch_total = 4;
        assert!(control.set_graph_state(running));
    }

    #[test]
    fn every_phase_has_a_wire_name_and_the_payload_carries_it() {
        for (phase, name) in [
            (Phase::Off, "off"),
            (Phase::Unavailable, "unavailable"),
            (Phase::Blocked, "blocked"),
            (Phase::Idle, "idle"),
            (Phase::Running, "running"),
        ] {
            assert_eq!(phase.as_str(), name);
        }
        let state = GraphState {
            phase: Phase::Blocked,
            reason: Some("VRChat is running".into()),
            batch_total: 4,
            batch_done: 1,
            walked: 9,
            ..Default::default()
        };
        let v = state.to_json();
        assert_eq!(v["phase"], json!("blocked"));
        assert_eq!(v["reason"], json!("VRChat is running"));
        assert_eq!(v["batch_total"], json!(4));
        assert_eq!(v["walked"], json!(9));
        assert_eq!(v["last_run_utc_ns"], Value::Null);
    }

    // ---- the pass, against the real model ---------------------------------

    #[test]
    fn the_real_model_walks_a_conversation_and_upgrades_what_the_rules_guessed() {
        let Some(root) = std::env::var("NXR_GRAPH_MODELS")
            .ok()
            .filter(|s| !s.is_empty())
        else {
            eprintln!("skipping the enrichment pass: set NXR_GRAPH_MODELS=<models dir> to run it");
            return;
        };
        let cfg = GraphConfig::default();
        let llm = Llm::resolve(
            std::path::Path::new(&root),
            &cfg,
            &crate::config::RuntimeConfig::default(),
        )
        .expect("the staged model");

        let store = Store::open_in_memory().expect("store");
        let src = store.upsert_source("VRChat.exe", "VRChat", 0).unwrap();
        let session = store.begin_session(src, 0).unwrap();
        let a = store.mint_speaker(0).unwrap();
        let b = store.mint_speaker(0).unwrap();

        let say = |at_s: i64, who: i64, text: &str| {
            let t = at_s * 1_000_000_000;
            let id = store
                .insert_segment(session, t, t + 3_000_000_000, "", t)
                .unwrap();
            store
                .set_segment_analysis(
                    id,
                    &crate::store::SegmentAnalysis {
                        text: Some(text.into()),
                        ..Default::default()
                    },
                )
                .unwrap();
            store.set_segment_speaker(id, Some(who), Some(0.7)).unwrap();
            crate::threads::assign(&store, &cfg, id).unwrap();
            crate::commitment::extract(&store, id, 1).unwrap();
            id
        };

        say(0, a, "hast du das Video von gestern noch?");
        let promise = say(5, b, "ja klar, ich schick dir morgen den Link");
        say(10, a, "perfekt, danke");

        // Tier 2 got there first, and it is a guess.
        let before = &store.commitments(None, 10).unwrap()[0];
        assert_eq!(before.source, commitment_source::RULES);
        assert!(before.confidence.unwrap() < 0.5);
        let id_before = before.id;

        let thread_id = store
            .segment_row(promise)
            .unwrap()
            .unwrap()
            .thread_id
            .unwrap();
        let (found, _, labelled) = enrich_thread(&store, &llm, &cfg, thread_id, 2).unwrap();
        assert_eq!(found, 1, "the model did not find the promise");
        assert!(labelled, "the conversation was not named");

        let after = &store.commitments(None, 10).unwrap()[0];
        assert_eq!(
            after.id, id_before,
            "the row moved instead of being upgraded"
        );
        assert_eq!(after.source, commitment_source::LLM);
        assert_eq!(after.model_id.as_deref(), Some(llm.model_id()));
        assert!(after.confidence.unwrap() > before.confidence.unwrap());
        assert_eq!(after.who_speaker_id, Some(b));
        assert_eq!(after.to_speaker_id, Some(a));
        assert!(
            after
                .due_raw
                .as_deref()
                .is_some_and(|d| d.contains("morgen")),
            "{after:?}"
        );
        assert!(after.due_utc_ns.is_some(), "the due phrase did not resolve");
        eprintln!(
            "llm commitment: who={:?} what={:?} due={:?}",
            after.who_speaker_id, after.what, after.due_raw
        );

        let topics = store.topics(5).unwrap();
        assert_eq!(topics.len(), 1);
        eprintln!("topic: {:?}", topics[0].topic);
        assert!(store.graph_counts().unwrap().threads_pending == 0);
    }

    /// The refusal path, which is the one the bake-off was really about: the
    /// model looks at a rule guess, says there is nothing there, and the guess
    /// goes.
    #[test]
    fn the_real_model_retracts_a_rule_guess_it_disagrees_with() {
        let Some(root) = std::env::var("NXR_GRAPH_MODELS")
            .ok()
            .filter(|s| !s.is_empty())
        else {
            eprintln!("skipping the retraction: set NXR_GRAPH_MODELS=<models dir> to run it");
            return;
        };
        let cfg = GraphConfig::default();
        let llm = Llm::resolve(
            std::path::Path::new(&root),
            &cfg,
            &crate::config::RuntimeConfig::default(),
        )
        .expect("the staged model");

        let store = Store::open_in_memory().expect("store");
        let src = store.upsert_source("VRChat.exe", "VRChat", 0).unwrap();
        let session = store.begin_session(src, 0).unwrap();
        let a = store.mint_speaker(0).unwrap();
        let b = store.mint_speaker(0).unwrap();
        let say = |at_s: i64, who: i64, text: &str| {
            let t = at_s * 1_000_000_000;
            let id = store
                .insert_segment(session, t, t + 3_000_000_000, "", t)
                .unwrap();
            store
                .set_segment_analysis(
                    id,
                    &crate::store::SegmentAnalysis {
                        text: Some(text.into()),
                        ..Default::default()
                    },
                )
                .unwrap();
            store.set_segment_speaker(id, Some(who), Some(0.7)).unwrap();
            crate::threads::assign(&store, &cfg, id).unwrap();
            crate::commitment::extract(&store, id, 1).unwrap();
            id
        };

        // In-game banter: the rules cannot see it, and the bench says the model
        // can (spike/graph_bench, 9/9 traps).
        say(0, a, "you are absolutely dead next round");
        let banter = say(5, b, "I will kill you next round, watch");
        say(10, a, "sure you will");

        assert_eq!(
            store.commitments(None, 10).unwrap().len(),
            1,
            "the rules were supposed to file this false positive"
        );
        let thread_id = store
            .segment_row(banter)
            .unwrap()
            .unwrap()
            .thread_id
            .unwrap();
        let (found, retracted, _) = enrich_thread(&store, &llm, &cfg, thread_id, 2).unwrap();
        eprintln!("banter window: found={found} retracted={retracted}");
        assert_eq!(found, 0, "the model invented an obligation out of banter");
        assert_eq!(retracted, 1);
        assert!(store.commitments(None, 10).unwrap().is_empty());
    }
}
