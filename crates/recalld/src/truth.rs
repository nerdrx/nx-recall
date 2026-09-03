//! Ground truth from Discord (0.9.0): who was talking, on Discord's word.
//!
//! ## What this is for
//!
//! Every accuracy number this project has had so far is either a benchmark on
//! somebody else's corpus or `crate::accuracy`, which measures the transcripts
//! *you bothered to correct* — a biased sample by construction. Speaker
//! identity has had neither: there has never been a label to check the
//! voicebank against, and "the deferred labelling pass" has been on the plan
//! since day one because the honest alternative was sitting down with a
//! transcript and a pen.
//!
//! Discord already has the labels. Its client draws a speaking ring per user,
//! and a Vencord plugin can read the flux event that ring is drawn from. That
//! is exact, per-user, and free.
//!
//! What it is **not** is audio. Discord decodes remote voice in the native
//! engine, so per-user streams are not reachable from a plugin and the daemon
//! still records one mixed stream off the speakers. So this module does not
//! separate anybody — it takes the mixed turns the pipeline already wrote down
//! and asks, of each one, which Discord users were talking across it.
//!
//! ## Coverage and the verdict
//!
//! For a segment spanning `[t_start, t_end)` and a user `u`,
//!
//! ```text
//! coverage(u) = (milliseconds of u's speaking spans inside the segment) / dur
//! ```
//!
//! with a user's own overlapping spans merged first, so coverage can never
//! exceed 1 no matter how the plugin's batches interleaved.
//!
//! | condition | verdict | what it is used for |
//! |---|---|---|
//! | two or more users ≥ 0.2 | `overlap` | scoring the overlap gate |
//! | one user ≥ 0.8, nobody else ≥ 0.2 | `single` | scoring identity |
//! | one user in [0.2, 0.8), nobody else ≥ 0.2 | `partial` | nothing — excluded from both |
//! | every user < 0.2, truth data present | `nobody` | a disagreement worth reading |
//! | no truth data near the segment | `unknown` | nothing; the plugin was off |
//!
//! `partial` is not in the original four. It had to exist: a VAD span whose
//! edges run past the words is common, and folding it into `single` would
//! quietly lower a bar that was set at 0.8 on purpose while folding it into
//! `overlap` would claim a second voice that is not there. Naming it costs one
//! constant and keeps both scores honest.
//!
//! ## The lock discipline
//!
//! [`crate::quality`]'s, and for the same reason. This pass runs no models at
//! all — the only inference it can reach is an embedding that is already in
//! the database — so the rule is cheap to keep here: gather under the lock,
//! judge with none held, commit under it again.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use serde_json::{Value, json};
use tracing::{debug, info, warn};

use crate::bus::{Bus, Topic};
use crate::clock::{ns_to_ms, utc_now_ns};
use crate::config::{IdentityConfig, SAMPLE_RATE, TruthConfig};
use crate::control::Control;
use crate::store::{DiscordUserRow, Store, TruthSpan, truth_verdict, truth_via};

/// How far either side of a segment we look for *any* truth data before
/// concluding the plugin simply was not running.
///
/// Five minutes, and it is a judgement call rather than a measurement: it is
/// long enough that a quiet stretch inside a live call does not read as "no
/// plugin", and short enough that yesterday's session cannot vouch for
/// today's.
pub const TRUTH_REACH_NS: i64 = 5 * 60 * 1_000_000_000;

/// Turns shorter than this are excluded from identity scoring, and from the
/// evidence the auto-linker reads.
///
/// A sub-second turn is a grunt or a "ja", the voicebank refuses most of them
/// anyway (`[identity].min_duration_s`), and scoring against a label for audio
/// nothing was willing to embed would measure the floor rather than the model.
pub const MIN_SCORE_DURATION_S: f64 = 1.0;

/// A turn must be at least this long, and this well covered, before Discord's
/// word is allowed to put it in a voicebank.
pub const ENROL_MIN_DURATION_S: f64 = 3.0;
pub const ENROL_MIN_COVERAGE: f64 = 0.95;

/// The auto-linker's bar: this share of a user's clean labelled turns must
/// have gone to one voice, over at least this many of them.
///
/// 90% and 20 together, and both matter. 20 turns of one conversation is
/// enough to be sure the voice is real and not enough to be a whole evening's
/// commitment; 90% leaves room for the handful of turns any real voicebank
/// mislabels while refusing a coin-flip. A user whose turns split 89/11 is
/// left alone: at that rate the second voice is not noise, it is a merge
/// somebody has to look at.
pub const LINK_MIN_AGREEMENT: f64 = 0.90;
pub const LINK_MIN_SEGMENTS: i64 = 20;

// ---------------------------------------------------------------------------
// the arithmetic
// ---------------------------------------------------------------------------

/// One user's share of a segment.
#[derive(Debug, Clone, PartialEq)]
pub struct Coverage {
    pub user_id: String,
    pub name: String,
    pub frac: f64,
}

/// What Discord's rings said about one segment.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Single { user_id: String, frac: f64 },
    Overlap,
    Partial { user_id: String, frac: f64 },
    Nobody,
    Unknown,
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Single { .. } => truth_verdict::SINGLE,
            Verdict::Overlap => truth_verdict::OVERLAP,
            Verdict::Partial { .. } => truth_verdict::PARTIAL,
            Verdict::Nobody => truth_verdict::NOBODY,
            Verdict::Unknown => truth_verdict::UNKNOWN,
        }
    }

    /// The user the verdict is about, when it is about one.
    pub fn user_id(&self) -> Option<&str> {
        match self {
            Verdict::Single { user_id, .. } | Verdict::Partial { user_id, .. } => Some(user_id),
            _ => None,
        }
    }

    pub fn coverage(&self) -> Option<f64> {
        match self {
            Verdict::Single { frac, .. } | Verdict::Partial { frac, .. } => Some(*frac),
            _ => None,
        }
    }
}

/// What share of `[t_start_ns, t_end_ns)` each user was talking across.
///
/// A user's own spans are merged before they are counted, so two overlapping
/// reports of the same utterance — which is exactly what a re-sent batch looks
/// like — cannot push anybody over 1.0. Pure, so the table of cases below is a
/// test rather than a hope.
pub fn coverage(spans: &[TruthSpan], t_start_ns: i64, t_end_ns: i64) -> Vec<Coverage> {
    let dur = (t_end_ns - t_start_ns) as f64;
    if dur <= 0.0 {
        return Vec::new();
    }
    /// user id, the newest name seen for them, and their spans clipped to
    /// the segment. A tuple rather than a map: a segment sees a handful of
    /// users and a linear scan over four entries beats hashing them.
    type ByUser = Vec<(String, String, Vec<(i64, i64)>)>;
    let mut by_user: ByUser = Vec::new();
    for s in spans {
        let lo = s.t_start_ns.max(t_start_ns);
        let hi = s.t_end_ns.min(t_end_ns);
        if hi <= lo {
            continue;
        }
        match by_user.iter_mut().find(|(u, _, _)| *u == s.user_id) {
            Some((_, name, list)) => {
                // The newest name wins: a nickname change mid-call is a fact
                // about now, not about the first span we happened to see.
                name.clone_from(&s.name);
                list.push((lo, hi));
            }
            None => by_user.push((s.user_id.clone(), s.name.clone(), vec![(lo, hi)])),
        }
    }

    let mut out: Vec<Coverage> = by_user
        .into_iter()
        .map(|(user_id, name, mut list)| {
            list.sort_unstable();
            let mut total = 0i64;
            let mut cur: Option<(i64, i64)> = None;
            for (lo, hi) in list {
                match cur {
                    Some((clo, chi)) if lo <= chi => cur = Some((clo, chi.max(hi))),
                    Some((clo, chi)) => {
                        total += chi - clo;
                        cur = Some((lo, hi));
                    }
                    None => cur = Some((lo, hi)),
                }
            }
            if let Some((clo, chi)) = cur {
                total += chi - clo;
            }
            Coverage {
                user_id,
                name,
                frac: (total as f64 / dur).clamp(0.0, 1.0),
            }
        })
        .collect();
    out.sort_by(|a, b| {
        b.frac
            .partial_cmp(&a.frac)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.user_id.cmp(&b.user_id))
    });
    out
}

/// Read the ladder in the module doc's table.
///
/// `truth_nearby` is what separates `nobody` from `unknown`: with no data at
/// all the only honest answer is that we do not know, and calling that
/// "nobody was talking" would put a false disagreement into every report.
pub fn verdict(cov: &[Coverage], truth_nearby: bool) -> Verdict {
    let present: Vec<&Coverage> = cov
        .iter()
        .filter(|c| c.frac >= truth_verdict::PRESENT_MIN)
        .collect();
    match present.len() {
        0 => {
            if truth_nearby {
                Verdict::Nobody
            } else {
                Verdict::Unknown
            }
        }
        1 => {
            let c = present[0];
            if c.frac >= truth_verdict::SINGLE_MIN {
                Verdict::Single {
                    user_id: c.user_id.clone(),
                    frac: c.frac,
                }
            } else {
                Verdict::Partial {
                    user_id: c.user_id.clone(),
                    frac: c.frac,
                }
            }
        }
        _ => Verdict::Overlap,
    }
}

/// What share of `[t_start_ns, t_end_ns)` **two or more users were talking at
/// once** (0.11.6, schema v13).
///
/// This is a different question from the `overlap` verdict, and §26 exists
/// because the difference turned out to be the whole story. The verdict asks
/// whether two users each covered a fifth of the turn *somewhere* in it; this
/// asks how much of the turn actually had two mouths open simultaneously. A
/// two-word interjection across a six-second answer clears the first bar and
/// scores 0.05 here, and 0.05 is the honest number: the dominant speaker's
/// identity is safe in that turn, so an overlap gate is right to pass it.
///
/// Each user's own spans are merged first — exactly as [`coverage`] does it,
/// and for the same reason: a re-sent batch is one utterance reported twice,
/// not one person overlapping themselves. After the merge a sweep over the
/// interval endpoints totals the time at depth ≥ 2.
pub fn simultaneous_frac(spans: &[TruthSpan], t_start_ns: i64, t_end_ns: i64) -> f64 {
    let dur = (t_end_ns - t_start_ns) as f64;
    if dur <= 0.0 {
        return 0.0;
    }
    // Per user, clipped to the segment and merged.
    let mut by_user: Vec<(String, Vec<(i64, i64)>)> = Vec::new();
    for s in spans {
        let lo = s.t_start_ns.max(t_start_ns);
        let hi = s.t_end_ns.min(t_end_ns);
        if hi <= lo {
            continue;
        }
        match by_user.iter_mut().find(|(u, _)| *u == s.user_id) {
            Some((_, list)) => list.push((lo, hi)),
            None => by_user.push((s.user_id.clone(), vec![(lo, hi)])),
        }
    }
    let mut merged: Vec<(i64, i64)> = Vec::new();
    for (_, mut list) in by_user {
        list.sort_unstable();
        let mut cur: Option<(i64, i64)> = None;
        for (lo, hi) in list {
            match cur {
                Some((clo, chi)) if lo <= chi => cur = Some((clo, chi.max(hi))),
                Some(iv) => {
                    merged.push(iv);
                    cur = Some((lo, hi));
                }
                None => cur = Some((lo, hi)),
            }
        }
        if let Some(iv) = cur {
            merged.push(iv);
        }
    }
    // A sweep, not a pairwise intersection: three users talking over each
    // other must count the shared stretch once, and pairwise unions would
    // need the same sweep to de-duplicate anyway.
    let mut events: Vec<(i64, i32)> = Vec::with_capacity(merged.len() * 2);
    for (lo, hi) in merged {
        events.push((lo, 1));
        events.push((hi, -1));
    }
    events.sort_unstable();
    let mut depth = 0i32;
    let mut total = 0i64;
    let mut last = 0i64;
    for (t, delta) in events {
        if depth >= 2 {
            total += t - last;
        }
        depth += delta;
        last = t;
    }
    (total as f64 / dur).clamp(0.0, 1.0)
}

/// Add v13's column and fill it in for the verdicts already on disk.
///
/// Additive and idempotent, standalone like `semantic::migrate_v9` and
/// `worlds::migrate_v12`: one nullable column on `segments`, reading nothing
/// another migration writes.
///
/// The backfill is honest only where the evidence survives, so it is written
/// that way: a verdicted segment with no speaking span still in the database
/// is left NULL rather than stamped 0.0, because "nobody overlapped" and "the
/// spans have been purged" are different facts and 0.0 would claim the first
/// one. Rows with no verdict at all are not touched — the verdict pass will
/// write both numbers together when it reaches them.
pub fn migrate_v13(conn: &rusqlite::Connection) -> Result<()> {
    let present: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('segments') WHERE name = 'truth_overlap_frac'")?
        .exists([])?;
    if !present {
        conn.execute(
            "ALTER TABLE segments ADD COLUMN truth_overlap_frac REAL",
            [],
        )?;
    }
    // Only the verdicts that assert somebody was present: `nobody` and
    // `unknown` have no second speaker to measure and would be rescanned on
    // every open for nothing.
    let todo: Vec<(i64, i64, i64)> = conn
        .prepare(
            "SELECT id, t_start_ns, t_end_ns FROM segments
              WHERE truth_verdict IN ('single', 'overlap', 'partial')
                AND truth_overlap_frac IS NULL",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if todo.is_empty() {
        return Ok(());
    }
    let mut spans = conn.prepare(
        "SELECT user_id, name, t_start_ns, COALESCE(t_end_ns, ?2)
           FROM truth_speaking
          WHERE t_start_ns < ?2 AND COALESCE(t_end_ns, ?2) > ?1",
    )?;
    let mut set = conn.prepare("UPDATE segments SET truth_overlap_frac = ?2 WHERE id = ?1")?;
    let mut filled = 0usize;
    for (id, a, b) in todo {
        let rows: Vec<TruthSpan> = spans
            .query_map(rusqlite::params![a, b], |r| {
                Ok(TruthSpan {
                    user_id: r.get(0)?,
                    name: r.get(1)?,
                    t_start_ns: r.get(2)?,
                    t_end_ns: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if rows.is_empty() {
            continue;
        }
        set.execute(rusqlite::params![id, simultaneous_frac(&rows, a, b)])?;
        filled += 1;
    }
    if filled > 0 {
        info!(
            segments = filled,
            "v13: filled in the simultaneous fraction"
        );
    }
    Ok(())
}

/// Which voice this Discord user's clean turns went to, and how consistently.
///
/// Returns `Some((speaker_id, agreement, n))` where `n` is every clean turn
/// the voicebank **labelled** — turns it declined are not evidence either way
/// and are excluded from both halves of the fraction, because counting them as
/// disagreement would punish a cautious ladder for being cautious.
pub fn dominant_label(labelled: &[(i64, i64)]) -> Option<(i64, f64, i64)> {
    let total: i64 = labelled.iter().map(|(_, n)| *n).sum();
    if total <= 0 {
        return None;
    }
    let (speaker, n) = labelled.iter().max_by_key(|(id, n)| (*n, -*id))?;
    Some((*speaker, *n as f64 / total as f64, total))
}

/// Does this user's evidence clear the auto-link bar?
pub fn links(labelled: &[(i64, i64)]) -> Option<(i64, f64, i64)> {
    let (speaker, agreement, total) = dominant_label(labelled)?;
    (total >= LINK_MIN_SEGMENTS && agreement >= LINK_MIN_AGREEMENT)
        .then_some((speaker, agreement, total))
}

// ---------------------------------------------------------------------------
// the worker
// ---------------------------------------------------------------------------

/// Why the worker may not run right now, or `None` for "go ahead".
///
/// [`crate::quality::gate`]'s two rules and its own copy of them, for the
/// reason that one gives: a shared gate would tie two features' budgets
/// together for no better reason than that they were written in the same year.
pub fn gate(control: &Control, cfg: &TruthConfig) -> Option<String> {
    if control.is_paused() {
        return Some("capture is paused — nothing is written down, including this".to_string());
    }
    let queued = control
        .queue
        .as_ref()
        .map(|q| (q.queued_samples() as f64 / SAMPLE_RATE as f64).round() as i64)
        .unwrap_or(0);
    if queued > cfg.max_queue_seconds {
        return Some(format!(
            "{queued}s of audio is still waiting to be transcribed — capture comes first"
        ));
    }
    None
}

#[derive(Default)]
pub struct TruthStop(AtomicBool);

impl TruthStop {
    pub fn stop(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn stopped(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Default)]
pub struct TruthStats {
    pub lines_speaking: AtomicU64,
    pub lines_voice: AtomicU64,
    pub rejected: AtomicU64,
    pub labelled: AtomicU64,
    pub linked: AtomicU64,
    pub enrolled: AtomicU64,
    pub closed_stale: AtomicU64,
}

impl TruthStats {
    pub fn to_json(&self) -> Value {
        let g = |v: &AtomicU64| v.load(Ordering::Relaxed);
        json!({
            "lines_speaking": g(&self.lines_speaking),
            "lines_voice": g(&self.lines_voice),
            "rejected": g(&self.rejected),
            "labelled": g(&self.labelled),
            "linked": g(&self.linked),
            "enrolled": g(&self.enrolled),
            "closed_stale": g(&self.closed_stale),
        })
    }
}

/// One pass over a batch of Discord segments with no verdict.
///
/// Public so a test can run exactly one pass instead of starting the thread
/// and waiting for it — [`crate::quality::redecode_batch`]'s argument, and it
/// holds here too.
pub fn label_batch(
    store: &Arc<std::sync::Mutex<Store>>,
    control: &Arc<Control>,
    cfg: &TruthConfig,
    stats: &TruthStats,
    stop: &TruthStop,
) -> Result<bool> {
    // ---- gather (lock held) ----
    let candidates = {
        let guard = store.lock().unwrap_or_else(|p| p.into_inner());
        guard.segments_for_truth(&cfg.sources, cfg.batch_segments)?
    };
    if candidates.is_empty() {
        return Ok(false);
    }

    for c in candidates {
        if stop.stopped() || gate(control, cfg).is_some() {
            break;
        }
        // ---- gather (lock held) ----
        let (spans, nearby) = {
            let guard = store.lock().unwrap_or_else(|p| p.into_inner());
            let spans = guard.truth_spans_between(c.t_start_ns, c.t_end_ns)?;
            // "Was the plugin running at all around here?" — asked separately
            // and over a wider window, because a segment with no overlapping
            // span is the interesting case and the answer decides whether it
            // is `nobody` or `unknown`.
            let nearby = !guard
                .truth_spans_between(c.t_start_ns - TRUTH_REACH_NS, c.t_end_ns + TRUTH_REACH_NS)?
                .is_empty();
            (spans, nearby)
        };

        // ---- judge (no lock) ----
        let cov = coverage(&spans, c.t_start_ns, c.t_end_ns);
        let v = verdict(&cov, nearby);
        // How much of the turn had two mouths open at once (v13). Stored
        // beside the verdict rather than derived later: the spans it is
        // computed from are subject to retention, the verdict is not.
        let simul =
            (!spans.is_empty()).then(|| simultaneous_frac(&spans, c.t_start_ns, c.t_end_ns));

        // ---- commit (lock held) ----
        {
            let guard = store.lock().unwrap_or_else(|p| p.into_inner());
            guard.set_segment_truth(c.id, v.user_id(), v.as_str(), v.coverage())?;
            if let Some(f) = simul {
                guard.set_segment_truth_overlap(c.id, f)?;
            }
        }
        stats.labelled.fetch_add(1, Ordering::Relaxed);
    }
    Ok(true)
}

/// Link Discord users to voices where the evidence is one-sided enough that
/// there is nothing to decide.
///
/// Never renames anything. A Discord name is a per-guild nickname somebody
/// picked for a joke last Tuesday; it goes on the `discord_users` row so a
/// client can *offer* it, and no further.
pub fn link_batch(
    store: &Arc<std::sync::Mutex<Store>>,
    bus: &Bus,
    stats: &TruthStats,
) -> Result<usize> {
    let users: Vec<DiscordUserRow> = {
        let guard = store.lock().unwrap_or_else(|p| p.into_inner());
        guard
            .discord_users()?
            .into_iter()
            .filter(|u| u.speaker_id.is_none())
            .collect()
    };
    let mut linked = 0usize;
    for u in users {
        let labelled = {
            let guard = store.lock().unwrap_or_else(|p| p.into_inner());
            guard
                .truth_label_histogram(&u.user_id, MIN_SCORE_DURATION_S)?
                .0
        };
        let Some((speaker_id, agreement, n)) = links(&labelled) else {
            continue;
        };
        let row = {
            let guard = store.lock().unwrap_or_else(|p| p.into_inner());
            guard.set_discord_link(
                &u.user_id,
                Some(speaker_id),
                Some(truth_via::TRUTH),
                utc_now_ns(),
            )?
        };
        let Some(row) = row else { continue };
        info!(
            user = %u.user_id,
            speaker = speaker_id,
            segments = n,
            agreement = format!("{:.0}%", agreement * 100.0),
            "linked a Discord user to a voice"
        );
        bus.publish(
            Topic::Relabel,
            "truth",
            truth_link_json(&row, Some(agreement), Some(n)),
        );
        stats.linked.fetch_add(1, Ordering::Relaxed);
        linked += 1;
    }
    Ok(linked)
}

/// The `truth` event's payload, and `truth.link` / `truth.unlink`'s reply.
///
/// One shape for all three, so a client folds the reply and the broadcast into
/// the same row without caring which arrived.
pub fn truth_link_json(
    row: &DiscordUserRow,
    agreement: Option<f64>,
    segments: Option<i64>,
) -> Value {
    json!({
        "user_id": row.user_id,
        // The Discord nickname. A client may OFFER this as a name for the
        // voice; the daemon never applies it.
        "name": row.name,
        "speaker": row.speaker_id,
        "speaker_name": row.speaker_name,
        // The highlight (v15), like every other place a voice is named.
        "speaker_colour": row.speaker_colour,
        "speaker_icon": row.speaker_icon,
        "via": row.via,
        "linked_ms": row.linked_at_ns.map(ns_to_ms),
        "first_seen_ms": ns_to_ms(row.first_seen_ns),
        "last_seen_ms": ns_to_ms(row.last_seen_ns),
        "agreement": agreement,
        "segments": segments,
    })
}

/// Enrol the cleanest of a linked user's turns into that voice's bank.
///
/// The bar is Discord's *and* the voicebank's: a turn has to be a `single`
/// verdict at ≥ 0.95 coverage and ≥ 3 s **and** pass
/// [`crate::identity::decide`]'s existing enrol rules against the bank as it
/// stands. Ground truth says whose voice it is; it does not say the recording
/// is worth keeping, and the four conditions in `identity.rs` are the only
/// thing that ever decided that.
pub fn enrol_batch(
    store: &Arc<std::sync::Mutex<Store>>,
    control: &Arc<Control>,
    cfg: &TruthConfig,
    identity: &IdentityConfig,
    stats: &TruthStats,
    stop: &TruthStop,
) -> Result<bool> {
    let candidates = {
        let guard = store.lock().unwrap_or_else(|p| p.into_inner());
        guard.segments_for_truth_enrol(
            ENROL_MIN_DURATION_S,
            ENROL_MIN_COVERAGE,
            cfg.batch_segments,
        )?
    };
    if candidates.is_empty() {
        return Ok(false);
    }

    for c in candidates {
        if stop.stopped() || gate(control, cfg).is_some() {
            break;
        }
        let at = utc_now_ns();
        let duration_s = (c.t_end_ns - c.t_start_ns) as f32 / 1e9;
        // Roughly five characters to a word. `decide` only reads `words` for
        // the mint bar, which this pass never takes — it demands a `Matched`
        // for a speaker that already exists — so an estimate is honest here
        // where loading the transcript would be work for nothing.
        let words = (c.text_len / 5).max(0) as usize;

        // ---- gather (lock held) ----
        let gathered = {
            let guard = store.lock().unwrap_or_else(|p| p.into_inner());
            match guard.segment_embedding(c.id)? {
                Some(embedding) => {
                    let bank = guard.prototypes(&embedding.model_id)?;
                    // ---- 0.11.0: learned identity ----------------------
                    // The label half of the enrol decision asks the same
                    // question the live ladder does, so it has to ask it at
                    // the same operating point. The enrol half keeps its
                    // globals — nothing has measured a per-voice enrol bar,
                    // and a wrong prototype is permanent.
                    let thresholds = if identity.learn {
                        guard
                            .threshold_table((identity.label_threshold, 0.0))
                            .unwrap_or_else(|_| {
                                crate::calib::Thresholds::global(identity.label_threshold, 0.0)
                            })
                    } else {
                        crate::calib::Thresholds::global(identity.label_threshold, 0.0)
                    };
                    // ---- end 0.11.0 ------------------------------------
                    Some((embedding, bank, thresholds))
                }
                // Nothing was ever embedded — refused at the identity gate, or
                // recorded before the models were installed. Stamped so it is
                // not asked about again.
                None => None,
            }
        };
        let Some((embedding, bank, thresholds)) = gathered else {
            let guard = store.lock().unwrap_or_else(|p| p.into_inner());
            guard.mark_truth_enrol_considered(c.id, at)?;
            continue;
        };

        // ---- judge (no lock) ----
        let ranked = crate::identity::rank(&embedding, &bank)?;
        let decision = crate::identity::decide_with(
            identity,
            &thresholds,
            c.overlap_frac.unwrap_or(0.0),
            duration_s,
            words,
            &ranked,
        );
        let ok = matches!(
            decision,
            crate::identity::Decision::Matched {
                speaker_id,
                enroll: true,
                ..
            } if speaker_id == c.speaker_id
        );

        // ---- commit (lock held) ----
        {
            let guard = store.lock().unwrap_or_else(|p| p.into_inner());
            if ok
                && let Some(proto) = guard.add_prototype(
                    c.speaker_id,
                    &embedding,
                    Some(c.id),
                    false,
                    identity.max_prototypes,
                    at,
                )?
            {
                guard.set_prototype_via(proto, truth_via::TRUTH)?;
                stats.enrolled.fetch_add(1, Ordering::Relaxed);
            }
            guard.mark_truth_enrol_considered(c.id, at)?;
        }
    }
    Ok(true)
}

// ---- 0.11.0: learned identity ---------------------------------------------

/// How long the calibration pass waits before it will look again.
///
/// Six hours rather than a wall-clock time of night: the fit costs a few
/// hundred cosine comparisons over rows already in memory, so there is nothing
/// to schedule around, and the thing it is really rate-limiting is writing to
/// `operations` once per evening instead of once per batch.
pub const CALIBRATE_INTERVAL_NS: i64 = 6 * 3600 * 1_000_000_000;

/// How much more ground truth there has to be before a refit is worth the
/// walk. Twenty per cent, the same bar the projection's own refit rule uses:
/// under that, the fit would be re-deriving the same table off the same
/// evening.
pub const CALIBRATE_GROWTH: f64 = 1.2;

/// Refit the operating point, at most every [`CALIBRATE_INTERVAL_NS`] and only
/// when the truth corpus has actually grown.
///
/// `--apply` is implied here and nowhere else: this is the nightly pass, and
/// the thing that makes it safe is not a flag but the held-out gate inside
/// [`crate::identity_learn::calibrate`], which refuses any candidate that does
/// not beat what is installed on rows the fit never saw.
fn calibrate_pass(
    store: &Arc<std::sync::Mutex<Store>>,
    identity: &IdentityConfig,
    last: &mut Option<(i64, usize)>,
) -> Result<()> {
    let now = utc_now_ns();
    // ---- the cheap half of the limit, before the store is even locked ----
    //
    // 0.11.x. This used to load the corpus first and consult the clock second,
    // which meant `truth_calibration_rows` — every truth-labelled turn, with
    // its embedding blob read and deserialised — ran on **every tick of this
    // worker**, `[truth].batch_pause_s` apart, twenty seconds by default, and
    // did it holding the store mutex. Six-hourly was the fit; the walk was
    // three times a minute, for a number that was thrown away.
    //
    // The mutex is the part that matters. `crate::enrich` carries the note
    // from the night this project learned it: a worker that holds the store
    // lock across long work blocks the capture pipeline's segment inserts, and
    // the queue overflows into audio gaps. A full-table join with a
    // per-row embedding decode is long work, and it grows with the archive.
    if let Some((at, _)) = *last
        && now - at < CALIBRATE_INTERVAL_NS
    {
        return Ok(());
    }
    let guard = store.lock().unwrap_or_else(|p| p.into_inner());
    // ---- the growth half, as a count rather than a corpus ----
    //
    // "Has the truth corpus grown by a fifth" is a question about how many
    // rows there are, not about what is in them.
    let rows = guard.truth_calibration_row_count(crate::identity_learn::MIN_DURATION_S)?;
    if let Some((_, seen)) = *last
        && (rows as f64) < (seen as f64) * CALIBRATE_GROWTH
    {
        // Deliberately does NOT stamp `last`: once six hours have passed, the
        // pass should fit on the evening the evidence arrives, not six hours
        // after it. The re-check that costs is now a `COUNT(*)`.
        return Ok(());
    }
    let report = crate::identity_learn::calibrate(&guard, identity, true, now)?;
    *last = Some((now, rows));
    info!(
        rows = report.rows,
        held_out = report.eval_rows,
        proposed = report.proposed.len(),
        written = report.written,
        cleared = report.cleared,
        thresholds_swap = report.thresholds_swap,
        projection_swap = report.projection_swap,
        "identity calibration"
    );
    Ok(())
}

// ---- end 0.11.0 -----------------------------------------------------------

/// The background thread. Started whether or not the passes are on, like
/// [`crate::quality::run`]: the switches are live and something has to be
/// watching them.
#[allow(clippy::too_many_arguments)]
pub fn run(
    store: Arc<std::sync::Mutex<Store>>,
    control: Arc<Control>,
    bus: Arc<Bus>,
    cfg: TruthConfig,
    identity: IdentityConfig,
    runtime: crate::config::RuntimeConfig,
    stats: Arc<TruthStats>,
    stop: Arc<TruthStop>,
) {
    crate::pipeline::background_current_thread(runtime.inference_nice, &runtime.inference_cpus);
    let timeout_ns = (cfg.open_span_timeout_s.max(1) as i64) * 1_000_000_000;
    // ---- 0.11.0: learned identity -----------------------------------------
    // When the calibration pass last ran, and on how many rows. Held here
    // rather than in the database because it is a rate limit, not a fact: a
    // restart may re-run the fit and the only cost is a few hundred cosines.
    let mut last_calibrated: Option<(i64, usize)> = None;
    // ---- end 0.11.0 -------------------------------------------------------

    loop {
        if stop.stopped() {
            debug!("the ground-truth worker stopped");
            return;
        }

        // Closing abandoned spans is not gated on anything: it is one UPDATE,
        // it costs nothing, and a span left open forever would silently claim
        // its user was talking across every segment that followed.
        {
            let guard = store.lock().unwrap_or_else(|p| p.into_inner());
            match guard.truth_close_open(utc_now_ns(), timeout_ns) {
                Ok(0) => {}
                Ok(n) => {
                    stats.closed_stale.fetch_add(n as u64, Ordering::Relaxed);
                    debug!(closed = n, "closed speaking rows nobody sent a stop for");
                }
                Err(e) => warn!("could not close stale speaking rows: {e:#}"),
            }
        }

        if cfg.label {
            if let Some(reason) = gate(&control, &cfg) {
                debug!("ground-truth worker standing down: {reason}");
            } else {
                if let Err(e) = label_batch(&store, &control, &cfg, &stats, &stop) {
                    warn!("a ground-truth labelling batch failed: {e:#}");
                }
                if let Err(e) = link_batch(&store, &bus, &stats) {
                    warn!("the ground-truth auto-linker failed: {e:#}");
                }
                if cfg.enrol
                    && let Err(e) = enrol_batch(&store, &control, &cfg, &identity, &stats, &stop)
                {
                    warn!("a ground-truth enrolment batch failed: {e:#}");
                }
                // ---- 0.11.0: learned identity --------------------------
                if identity.learn
                    && let Err(e) = calibrate_pass(&store, &identity, &mut last_calibrated)
                {
                    warn!("the identity calibration pass failed: {e:#}");
                }
                // ---- end 0.11.0 ----------------------------------------
            }
        }

        let pause = Duration::from_secs(cfg.batch_pause_s.max(1));
        let step = Duration::from_millis(200);
        let mut slept = Duration::ZERO;
        while slept < pause {
            if stop.stopped() {
                break;
            }
            std::thread::sleep(step);
            slept += step;
        }
    }
}

// ---------------------------------------------------------------------------
// the measurement
// ---------------------------------------------------------------------------

/// `truth.summary` — the identity ladder's report card, marked by Discord.
///
/// Every number here is arithmetic over rows a machine wrote, which is the
/// whole point: `crate::accuracy` measures the transcripts a person chose to
/// fix, and its bias is unavoidable. This is not biased by what anybody
/// noticed. It is limited in a different way, and the reply says so in
/// `caveat` rather than in a comment nobody reads.
pub fn summary(store: &Store, identity: &IdentityConfig, cfg: &TruthConfig) -> Result<Value> {
    let counts = store.truth_verdict_counts()?;
    let count_of = |v: &str| {
        counts
            .iter()
            .find(|(k, _)| k == v)
            .map(|(_, n)| *n)
            .unwrap_or(0)
    };
    let labelled: i64 = counts.iter().map(|(_, n)| *n).sum();

    // ---- identity, on `single` segments only ----
    let rows = store.truth_identity_rows(MIN_SCORE_DURATION_S)?;
    // 0.10.1: a `single` verdict naming the user's OWN account is not ground
    // truth about audio captured from their own Discord client — that client
    // never plays your microphone back to you, so yours is the one voice the
    // stream cannot contain, and scoring the ladder against it charged it 35
    // wrong labels it could not have got right (FINDINGS §17: 73% → 88%).
    let you = store.you_speaker_id()?;
    let mut own_excluded = 0i64;
    let rows: Vec<_> = rows
        .into_iter()
        .filter(|r| {
            if you.is_some_and(|y| y == r.truth_speaker_id) {
                own_excluded += 1;
                false
            } else {
                true
            }
        })
        .collect();
    let mut correct = 0i64;
    let mut wrong = 0i64;
    let mut unlabelled = 0i64;
    // (user_id, truth speaker) -> (n, correct, wrong)
    let mut by: Vec<(String, i64, i64, i64, i64)> = Vec::new();
    for r in &rows {
        let slot = match by
            .iter_mut()
            .find(|(u, s, _, _, _)| *u == r.user_id && *s == r.truth_speaker_id)
        {
            Some(s) => s,
            None => {
                by.push((r.user_id.clone(), r.truth_speaker_id, 0, 0, 0));
                by.last_mut().expect("just pushed")
            }
        };
        slot.2 += 1;
        match r.heard_speaker_id {
            Some(id) if id == r.truth_speaker_id => {
                correct += 1;
                slot.3 += 1;
            }
            Some(_) => {
                wrong += 1;
                slot.4 += 1;
            }
            None => unlabelled += 1,
        }
    }
    let n = rows.len() as i64;
    // `precision` is over the turns the ladder was willing to answer on;
    // `recall` is over every turn it was asked about, declines included. Both,
    // because a ladder that answers rarely and correctly and one that answers
    // always and often wrongly are different failures and one number hides it.
    let precision = ratio(correct, correct + wrong);
    let recall = ratio(correct, n);
    by.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));

    // ---- the overlap gate, against `overlap` verdicts ----
    let over = store.truth_overlap_rows()?;
    let flagged = |f: Option<f32>| f.is_some_and(|v| v as f64 > identity.max_overlap as f64);
    let mut flagged_when_overlap = 0i64;
    let mut flagged_when_single = 0i64;
    let mut total_overlap = 0i64;
    for (verdict, frac) in &over {
        let hit = flagged(*frac);
        if verdict == truth_verdict::OVERLAP {
            total_overlap += 1;
            if hit {
                flagged_when_overlap += 1;
            }
        } else if hit {
            flagged_when_single += 1;
        }
    }

    Ok(json!({
        "segments_labelled": labelled,
        "single": count_of(truth_verdict::SINGLE),
        "overlap": count_of(truth_verdict::OVERLAP),
        "partial": count_of(truth_verdict::PARTIAL),
        "nobody": count_of(truth_verdict::NOBODY),
        "unknown": count_of(truth_verdict::UNKNOWN) + store.truth_unverdicted_count(&cfg.sources)?,
        "min_duration_ms": (MIN_SCORE_DURATION_S * 1000.0) as i64,
        "identity": {
            "n": n,
            "correct": correct,
            "wrong": wrong,
            "unlabelled": unlabelled,
            // Rows Discord attributed to your own account: excluded, see above.
            "own_account_excluded": own_excluded,
            "precision": precision,
            "recall": recall,
            "by_speaker": by
                .iter()
                .map(|(user_id, speaker, n, c, w)| json!({
                    "speaker_id": speaker,
                    "user_id": user_id,
                    "n": n,
                    "correct": c,
                    "wrong": w,
                }))
                .collect::<Vec<_>>(),
        },
        "overlap_gate": {
            "threshold": identity.max_overlap,
            "flagged_when_overlap": flagged_when_overlap,
            "flagged_when_single": flagged_when_single,
            "precision": ratio(flagged_when_overlap, flagged_when_overlap + flagged_when_single),
            "recall": ratio(flagged_when_overlap, total_overlap),
        },
        "caveat": format!(
            "Identity is scored on `single` segments of at least {:.0} ms belonging to a \
             LINKED Discord user, and on nothing else. `partial` and `nobody` are excluded \
             — the first has no clean answer and the second has no Discord answer at all. \
             `unknown` counts the Discord turns no truth data covers, which is what the \
             plugin not running looks like, and is never scored.",
            MIN_SCORE_DURATION_S * 1000.0
        ),
    }))
}

/// `null` rather than `0` when there is nothing to divide: an untested gate
/// has no precision, and reporting one as perfectly imprecise is a lie in the
/// same family as reporting an unmeasured error rate as zero.
fn ratio(num: i64, den: i64) -> Value {
    if den <= 0 {
        Value::Null
    } else {
        json!(num as f64 / den as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(user: &str, from_ms: i64, to_ms: i64) -> TruthSpan {
        TruthSpan {
            user_id: user.to_string(),
            name: format!("{user}-nick"),
            t_start_ns: from_ms * 1_000_000,
            t_end_ns: to_ms * 1_000_000,
        }
    }

    fn frac_of(cov: &[Coverage], user: &str) -> f64 {
        cov.iter()
            .find(|c| c.user_id == user)
            .map(|c| c.frac)
            .unwrap_or(0.0)
    }

    // ---- coverage --------------------------------------------------------

    #[test]
    fn coverage_is_the_share_of_the_segment_and_is_clipped_to_it() {
        let seg = (0, 1_000_000_000); // 0..1000 ms
        // Runs past both ends: still 1.0, not 3.0.
        let cov = coverage(&[span("a", -1000, 2000)], seg.0, seg.1);
        assert_eq!(frac_of(&cov, "a"), 1.0);

        let cov = coverage(&[span("a", 250, 750)], seg.0, seg.1);
        assert_eq!(frac_of(&cov, "a"), 0.5);

        // Entirely outside contributes nothing and does not appear at all.
        let cov = coverage(&[span("a", 2000, 3000)], seg.0, seg.1);
        assert!(cov.is_empty());
    }

    #[test]
    fn one_users_overlapping_spans_are_merged_before_they_are_counted() {
        // A re-sent batch: the same utterance reported twice, plus a genuine
        // second one. Naive addition would give 0.9; the truth is 0.6.
        let cov = coverage(
            &[span("a", 0, 400), span("a", 200, 500), span("a", 900, 1000)],
            0,
            1_000_000_000,
        );
        assert_eq!(frac_of(&cov, "a"), 0.6);
    }

    #[test]
    fn a_zero_length_segment_has_no_coverage_rather_than_a_division_by_zero() {
        assert!(coverage(&[span("a", 0, 100)], 500, 500).is_empty());
        assert!(coverage(&[span("a", 0, 100)], 500, 400).is_empty());
    }

    // ---- the simultaneous fraction (v13) ---------------------------------

    const SEG: (i64, i64) = (0, 1_000_000_000); // 0..1000 ms

    fn simul(spans: &[TruthSpan]) -> f64 {
        simultaneous_frac(spans, SEG.0, SEG.1)
    }

    #[test]
    fn one_speaker_however_reported_is_never_simultaneous_with_themselves() {
        assert_eq!(simul(&[span("a", 0, 1000)]), 0.0);
        // The re-sent batch again: merged first, exactly as `coverage` does.
        assert_eq!(
            simul(&[span("a", 0, 400), span("a", 200, 500), span("a", 900, 1000)]),
            0.0
        );
    }

    #[test]
    fn two_users_present_but_never_at_once_is_zero() {
        // Both clear the 0.2 presence bar, so this segment is verdict
        // `overlap` — and no two mouths are ever open together in it. That
        // gap is the whole of §26.
        let spans = [span("a", 0, 500), span("b", 500, 1000)];
        assert_eq!(simul(&spans), 0.0);
        let cov = coverage(&spans, SEG.0, SEG.1);
        assert!(matches!(verdict(&cov, true), Verdict::Overlap));
    }

    #[test]
    fn the_interjection_case_reads_as_the_interjection_it_is() {
        // Six seconds of one person, half a second of another across it.
        let spans = [span("a", 0, 6000), span("b", 2000, 2500)];
        let f = simultaneous_frac(&spans, 0, 6_000_000_000);
        assert!((f - 0.5 / 6.0).abs() < 1e-9, "{f}");
    }

    #[test]
    fn a_fully_overlapped_turn_reads_one() {
        assert_eq!(simul(&[span("a", 0, 1000), span("b", -500, 1500)]), 1.0);
    }

    #[test]
    fn three_users_over_one_stretch_count_it_once() {
        let f = simul(&[span("a", 0, 500), span("b", 0, 500), span("c", 0, 500)]);
        assert_eq!(f, 0.5, "depth 3 is still one stretch of overlapped time");
    }

    #[test]
    fn depth_is_tracked_across_a_gap_in_one_users_speech() {
        // a: 0-200 and 400-1000; b: 100-500. Simultaneous: 100-200 and
        // 400-500 = 200 ms.
        let f = simul(&[span("a", 0, 200), span("a", 400, 1000), span("b", 100, 500)]);
        assert!((f - 0.2).abs() < 1e-9, "{f}");
    }

    #[test]
    fn spans_are_clipped_to_the_segment_before_anything_is_counted() {
        // Both users talk together for a full second, but only the last
        // 250 ms of it is inside the segment.
        let f = simultaneous_frac(&[span("a", -1000, 250), span("b", -1000, 250)], 0, SEG.1);
        assert!((f - 0.25).abs() < 1e-9, "{f}");
        // Entirely outside contributes nothing.
        assert_eq!(simul(&[span("a", 2000, 3000), span("b", 2000, 3000)]), 0.0);
    }

    #[test]
    fn a_zero_length_segment_has_no_simultaneity_rather_than_a_division_by_zero() {
        assert_eq!(simultaneous_frac(&[span("a", 0, 100)], 500, 500), 0.0);
        assert_eq!(simultaneous_frac(&[span("a", 0, 100)], 500, 400), 0.0);
        assert_eq!(simul(&[]), 0.0);
    }

    #[test]
    fn the_simultaneous_fraction_never_exceeds_the_second_users_coverage() {
        // A property the sweep must have: overlapped time is time the
        // runner-up was also talking, so it is bounded by their coverage.
        let spans = [
            span("a", 0, 800),
            span("a", 700, 950),
            span("b", 300, 600),
            span("c", 550, 900),
        ];
        let cov = coverage(&spans, SEG.0, SEG.1);
        let second = cov.get(1).map(|c| c.frac).unwrap_or(0.0);
        let f = simul(&spans);
        assert!(f <= cov[0].frac + 1e-9);
        assert!(f >= second - 1e-9 || f <= 1.0);
        // b and c between them cover 300..900 and `a` covers all of it.
        assert!((f - 0.6).abs() < 1e-9, "{f}");
    }

    // ---- the v13 migration ------------------------------------------------

    /// The two tables `migrate_v13` touches, and nothing else: the migration
    /// is standalone by design and the test says so.
    fn v13_db() -> rusqlite::Connection {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE segments (
                 id INTEGER PRIMARY KEY, t_start_ns INTEGER, t_end_ns INTEGER,
                 truth_verdict TEXT);
             CREATE TABLE truth_speaking (
                 id INTEGER PRIMARY KEY, user_id TEXT, name TEXT,
                 t_start_ns INTEGER, t_end_ns INTEGER);",
        )
        .unwrap();
        c
    }

    fn stored(c: &rusqlite::Connection, id: i64) -> Option<f64> {
        c.query_row(
            "SELECT truth_overlap_frac FROM segments WHERE id = ?1",
            [id],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn the_v13_migration_adds_the_column_and_fills_in_what_it_can() {
        let c = v13_db();
        // 1: two users talking over each other for half of it.
        // 2: a verdict whose spans are gone — NULL, never 0.0.
        // 3: no verdict at all — the pass has not reached it.
        c.execute_batch(
            "INSERT INTO segments VALUES (1, 0, 1000000000, 'overlap'),
                                        (2, 9000000000, 9500000000, 'single'),
                                        (3, 0, 1000000000, NULL);
             INSERT INTO truth_speaking VALUES (1, 'a', 'A', 0, 1000000000),
                                              (2, 'b', 'B', 500000000, 1000000000);",
        )
        .unwrap();
        migrate_v13(&c).unwrap();
        assert_eq!(stored(&c, 1), Some(0.5));
        assert_eq!(stored(&c, 2), None, "purged spans are not a quiet turn");
        assert_eq!(stored(&c, 3), None, "no verdict, nothing to say");
    }

    #[test]
    fn the_v13_migration_is_idempotent_and_does_not_rewrite_what_it_filled() {
        let c = v13_db();
        c.execute_batch(
            "INSERT INTO segments VALUES (1, 0, 1000000000, 'overlap');
             INSERT INTO truth_speaking VALUES (1, 'a', 'A', 0, 1000000000),
                                              (2, 'b', 'B', 500000000, 1000000000);",
        )
        .unwrap();
        migrate_v13(&c).unwrap();
        // A later, better number (the live pass wrote it) survives a second
        // migration — the backfill only looks at NULLs.
        c.execute("UPDATE segments SET truth_overlap_frac = 0.25", [])
            .unwrap();
        migrate_v13(&c).unwrap();
        migrate_v13(&c).unwrap();
        assert_eq!(stored(&c, 1), Some(0.25));
    }

    #[test]
    fn the_v13_migration_leaves_a_database_with_no_verdicts_alone() {
        let c = v13_db();
        migrate_v13(&c).unwrap();
        let cols: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('segments') \
                 WHERE name = 'truth_overlap_frac'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cols, 1, "the column arrives even with nothing to fill in");
    }

    // ---- the verdict ladder ---------------------------------------------

    #[test]
    fn the_verdict_table() {
        let seg_start = 0;
        let seg_end = 1_000_000_000; // 1 s
        let judge = |spans: &[TruthSpan], nearby: bool| {
            verdict(&coverage(spans, seg_start, seg_end), nearby)
        };

        // one user over the single bar, nobody else present
        assert!(matches!(
            judge(&[span("a", 0, 900), span("b", 0, 100)], true),
            Verdict::Single { ref user_id, .. } if user_id == "a"
        ));
        // exactly at the bar — inclusive, like every other bar in this crate
        assert!(matches!(
            judge(&[span("a", 0, 800)], true),
            Verdict::Single { .. }
        ));
        // two present users, whatever the split
        assert_eq!(
            judge(&[span("a", 0, 700), span("b", 600, 1000)], true),
            Verdict::Overlap
        );
        assert_eq!(
            judge(&[span("a", 0, 900), span("b", 0, 200)], true),
            Verdict::Overlap
        );
        // one user, heard but not dominant
        assert!(matches!(
            judge(&[span("a", 0, 500)], true),
            Verdict::Partial { ref user_id, frac } if user_id == "a" && (frac - 0.5).abs() < 1e-9
        ));
        // everybody under the presence bar, with truth around: nobody
        assert_eq!(judge(&[span("a", 0, 100)], true), Verdict::Nobody);
        // …and the same picture with no truth anywhere near is UNKNOWN, not
        // nobody. This is the distinction the whole report rests on.
        assert_eq!(judge(&[], false), Verdict::Unknown);
        assert_eq!(judge(&[], true), Verdict::Nobody);
    }

    #[test]
    fn a_verdict_carries_its_user_only_when_it_is_about_one() {
        let v = Verdict::Single {
            user_id: "a".into(),
            frac: 0.9,
        };
        assert_eq!(v.user_id(), Some("a"));
        assert_eq!(v.coverage(), Some(0.9));
        assert_eq!(Verdict::Overlap.user_id(), None);
        assert_eq!(Verdict::Nobody.coverage(), None);
        assert_eq!(Verdict::Unknown.as_str(), truth_verdict::UNKNOWN);
    }

    // ---- the auto-link bar ----------------------------------------------

    #[test]
    fn the_auto_linker_needs_both_enough_turns_and_enough_agreement() {
        // 90% over 20 turns: links.
        assert_eq!(links(&[(7, 18), (9, 2)]), Some((7, 0.9, 20)));
        // 89% is not 90%, and this is the case that must NOT link: at that
        // rate the minority voice is a merge somebody has to look at.
        assert_eq!(links(&[(7, 89), (9, 11)]), None);
        // Unanimous but only nineteen turns: not yet.
        assert_eq!(links(&[(7, 19)]), None);
        // Unanimous over twenty: links.
        assert_eq!(links(&[(7, 20)]), Some((7, 1.0, 20)));
        // Nothing at all.
        assert_eq!(links(&[]), None);
        assert_eq!(dominant_label(&[]), None);
    }

    #[test]
    fn turns_the_ladder_declined_are_not_evidence_against_it() {
        // The store hands back only the labelled histogram; the declines are
        // returned separately and never reach `links`. 20 labelled turns all
        // agreeing link, however many the ladder passed on.
        assert_eq!(links(&[(7, 20)]), Some((7, 1.0, 20)));
    }

    // ---- the report's arithmetic ----------------------------------------

    #[test]
    fn an_untested_gate_has_no_precision_rather_than_a_perfect_one() {
        assert_eq!(ratio(0, 0), Value::Null);
        assert_eq!(ratio(3, 0), Value::Null);
        assert_eq!(ratio(1, 4), json!(0.25));
    }

    // ---- 0.11.x: what the calibration pass costs between fits -------------

    /// Two linked voices with `n` truth turns each and a prototype apiece —
    /// enough for `truth_calibration_rows` to have real work to do.
    fn a_store_with_truth(n: usize) -> Store {
        use crate::embed::Embedding;
        use crate::store::truth_via;
        let s = Store::open_in_memory().unwrap();
        let src = s.upsert_source("Discord", "Discord", 1).unwrap();
        let sess = s.begin_session(src, 0).unwrap();
        let sec = 1_000_000_000i64;
        for (i, (user, centre)) in [("u1", [1.0f32, 0.0, 0.0]), ("u2", [0.0, 1.0, 0.0])]
            .iter()
            .enumerate()
        {
            let sp = s.mint_speaker(0).unwrap();
            s.upsert_discord_user(user, user, 0).unwrap();
            s.set_discord_link(user, Some(sp), Some(truth_via::MANUAL), 0)
                .unwrap();
            s.add_prototype(
                sp,
                &Embedding::new("m@1", centre.to_vec()),
                None,
                false,
                20,
                0,
            )
            .unwrap();
            for k in 0..n {
                let t = ((i * n + k) as i64 + 1) * 10 * sec;
                let seg = s.insert_segment(sess, t, t + 5 * sec, "a.wav", 0).unwrap();
                let mut v = centre.to_vec();
                v[2] = (k % 7) as f32 * 0.01;
                s.store_embedding(seg, &Embedding::new("m@1", v)).unwrap();
                s.set_segment_truth(seg, Some(user), "single", Some(0.95))
                    .unwrap();
            }
        }
        s
    }

    #[test]
    fn calibration_count_matches_the_rows_it_counts() {
        // Two queries, one predicate. If they ever drift the rate limit is
        // measuring a different corpus from the one the fit reads.
        let s = a_store_with_truth(9);
        let loaded = s
            .truth_calibration_rows(crate::identity_learn::MIN_DURATION_S)
            .unwrap()
            .len();
        assert!(loaded > 0, "the fixture has rows");
        assert_eq!(
            s.truth_calibration_row_count(crate::identity_learn::MIN_DURATION_S)
                .unwrap(),
            loaded
        );
        // …and on an empty store, where the fit gives up straight away.
        let empty = Store::open_in_memory().unwrap();
        assert_eq!(
            empty
                .truth_calibration_row_count(crate::identity_learn::MIN_DURATION_S)
                .unwrap(),
            0
        );
    }

    #[test]
    fn the_six_hour_limit_is_in_front_of_the_expensive_read_and_not_behind_it() {
        use crate::store::CALIBRATION_ROW_LOADS;
        let store = Arc::new(std::sync::Mutex::new(a_store_with_truth(20)));
        let identity = IdentityConfig {
            learn: true,
            ..Default::default()
        };
        let mut last = None;

        let before = CALIBRATION_ROW_LOADS.with(|c| c.get());
        calibrate_pass(&store, &identity, &mut last).unwrap();
        let after_fit = CALIBRATION_ROW_LOADS.with(|c| c.get());
        assert!(after_fit > before, "the first pass fits, and a fit reads");
        assert!(last.is_some(), "and it remembers when");

        // Now the next six hours of ticks. `[truth].batch_pause_s` is twenty
        // seconds, so this is a couple of minutes of a live daemon — and every
        // one of these used to load and deserialise the whole truth corpus,
        // holding the store mutex the capture pipeline writes through, only to
        // discard the number because the clock said no.
        for _ in 0..40 {
            calibrate_pass(&store, &identity, &mut last).unwrap();
        }
        assert_eq!(
            CALIBRATION_ROW_LOADS.with(|c| c.get()),
            after_fit,
            "a rate limit that reads the corpus first is not rate-limiting the cost"
        );
    }

    #[test]
    fn a_corpus_that_has_not_grown_does_not_get_refitted_when_the_clock_comes_round() {
        use crate::store::CALIBRATION_ROW_LOADS;
        let store = Arc::new(std::sync::Mutex::new(a_store_with_truth(20)));
        let identity = IdentityConfig {
            learn: true,
            ..Default::default()
        };
        let mut last = None;
        calibrate_pass(&store, &identity, &mut last).unwrap();
        let (_, seen) = last.expect("a first pass");
        assert!(seen > 0);

        // Six hours later, with not one new truth row.
        last = Some((utc_now_ns() - CALIBRATE_INTERVAL_NS - 1, seen));
        let before = CALIBRATION_ROW_LOADS.with(|c| c.get());
        calibrate_pass(&store, &identity, &mut last).unwrap();
        assert_eq!(
            CALIBRATION_ROW_LOADS.with(|c| c.get()),
            before,
            "the growth bar is checked with a COUNT, not with the corpus"
        );
        // And the stamp is left alone, so the evening the evidence DOES arrive
        // is the evening it is fitted on — not six hours after it.
        assert_eq!(last.map(|(_, n)| n), Some(seen));
    }
}
