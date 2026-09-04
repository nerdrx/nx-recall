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

/// How far back `truth.status` and the scope rule look when asking how busy a
/// bridge is. Five minutes, the same reach [`TRUTH_REACH_NS`] uses, so "spans
/// per minute" and "was the plugin running" are measured over one window.
pub const BRIDGE_RECENT_NS: i64 = TRUTH_REACH_NS;

// ---------------------------------------------------------------------------
// 0.12.3: whose spans
// ---------------------------------------------------------------------------

/// Which bridge's spans a mixed Discord source's turns are judged against.
///
/// The picker is optional because every pass here has to work with the daemon's
/// live state absent — `recalld truth rejudge` on a copied database has no
/// socket and no `Picker` — and its absence changes nothing except that a
/// manual override cannot be read.
pub fn scope_of(
    source: &str,
    bridges: &[crate::bridge::BridgeSeen],
    bridge: Option<&crate::bridge::Picker>,
) -> crate::bridge::Scope {
    let account = bridge.and_then(|b| b.role_account(source));
    crate::bridge::scope_for_source(source, bridges, account.as_deref())
}

/// The account a scope names, for [`Audible`]: the bridge's own microphone
/// cannot be in its own client's output.
pub fn scope_account(scope: &crate::bridge::Scope) -> Option<&str> {
    match scope {
        crate::bridge::Scope::Account(a) => Some(a.as_str()),
        // `Every` is either one bridge (in which case it may be an old plugin
        // that never said which account it was) or an ambiguous pair. Neither
        // supports a claim about whose microphone the stream cannot hold, so
        // nothing is claimed.
        crate::bridge::Scope::Every | crate::bridge::Scope::Legacy => None,
    }
}

// ---------------------------------------------------------------------------
// the arithmetic
// ---------------------------------------------------------------------------

/// Which of Discord's accounts the audio in front of us can physically
/// contain (0.12.1, FINDINGS §34).
///
/// Discord's speaking rings are the truth about *the call*. They are not the
/// truth about *the recording*, and on one account the two disagree
/// systematically: a Discord client never plays your own microphone back to
/// you, so on application audio captured from that client yours is the one
/// voice the stream cannot hold (§17 finding 1, and measured again in §33 —
/// the user's own prototype, the best-attested in the bank, wins 12 of 1,306
/// turns Discord says they were talking across).
///
/// Counting your own ring as presence therefore does not merely add noise, it
/// relabels single-speaker audio: 1,306 of this install's 1,509 two-user
/// `overlap` verdicts were you-plus-somebody, i.e. one voice wearing an
/// `overlap` label, and every number computed against `overlap` was diluted
/// eight-fold by them.
///
/// A microphone is the exact opposite and the rule must not touch it: on
/// `mic` audio the user's own account is the only voice that *can* be there.
/// So the rule is one line — drop the own account, and only on application
/// audio — and it is spelled as a type rather than a bare `bool` so that
/// neither caller can pass the wrong one by accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Audible<'a> {
    /// `sources.kind` for the session the segment belongs to.
    kind: &'a str,
    /// Every Discord account linked to the pinned "You" voice. A slice
    /// because a person may have two, and an alt is as inaudible as the main.
    ///
    /// Two accounts reach this slice today with no extra machinery:
    /// `Store::discord_user_ids_for_speaker` resolves through
    /// `speaker_resolved`, so *every* `discord_users` row pointing at the
    /// pinned voice (or at a voice merged into it) is here. Linking the second
    /// account to the same "You" voice is the whole of the setup.
    own: &'a [String],
    /// The account the bridge whose call this recording carries is signed in
    /// as (0.12.3).
    ///
    /// A **stronger** rule than `own` and it needs no link at all: this is the
    /// local user of the very client whose output was tapped, and a client
    /// never plays your own microphone back to you. `own` is "an account the
    /// user has told us is theirs"; this is "the account this recording is
    /// physically made from". The second one is a fact about the stream, which
    /// is exactly what [`Audible`] is for.
    bridge_account: Option<&'a str>,
}

impl<'a> Audible<'a> {
    pub fn new(kind: &'a str, own: &'a [String]) -> Self {
        Self {
            kind,
            own,
            bridge_account: None,
        }
    }

    /// Name the bridge whose client made this recording (0.12.3). `None`
    /// leaves the rule exactly as 0.12.1 wrote it.
    pub fn with_bridge_account(mut self, account: Option<&'a str>) -> Self {
        self.bridge_account = account.filter(|a| !a.is_empty());
        self
    }

    /// A stream nothing is known to be missing from: every account counts.
    /// What a caller with no linked "You" account has, and what every test
    /// that is not about this rule wants.
    pub fn everyone() -> Self {
        Self {
            kind: crate::store::KIND_APP,
            own: &[],
            bridge_account: None,
        }
    }

    /// Can this account's voice be in this recording at all?
    pub fn hears(&self, user_id: &str) -> bool {
        !self.silences(user_id)
    }

    fn silences(&self, user_id: &str) -> bool {
        self.kind == crate::store::KIND_APP
            && (self.own.iter().any(|u| u == user_id) || self.bridge_account == Some(user_id))
    }

    /// The accounts this stream cannot contain — empty on a microphone, and
    /// empty when nothing is linked to "You" and no bridge is named.
    pub fn silent(&self) -> Vec<String> {
        if self.kind != crate::store::KIND_APP {
            return Vec::new();
        }
        let mut out: Vec<String> = self.own.to_vec();
        if let Some(a) = self.bridge_account
            && !out.iter().any(|u| u == a)
        {
            out.push(a.to_string());
        }
        out
    }
}

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
///
/// `audible` is [`Audible`]'s one rule: on application audio the user's own
/// account is not *presence*, because that stream cannot carry their voice.
/// It is applied to presence rather than to [`coverage`] so the arithmetic
/// stays a description of the call and only the verdict — the claim about the
/// recording — is corrected. A turn where the user talked over somebody thus
/// reads `single` for that somebody, and a turn where only the user talked
/// reads `nobody`, which is what `nobody` has always meant: truth covers this
/// moment and none of the voices this recording can hold was in it.
pub fn verdict(cov: &[Coverage], truth_nearby: bool, audible: Audible<'_>) -> Verdict {
    let present: Vec<&Coverage> = cov
        .iter()
        .filter(|c| c.frac >= truth_verdict::PRESENT_MIN && audible.hears(&c.user_id))
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
///
/// `audible` for the same reason the verdict takes it (0.12.1): this number
/// is read as "how much of *this recording* holds two voices", and a mouth
/// the stream cannot carry is not a second voice in it. Counting the user's
/// own ring here is what made §26's median `overlap` turn look 47%
/// simultaneous when seven of every eight such turns were one person talking.
pub fn simultaneous_frac(
    spans: &[TruthSpan],
    t_start_ns: i64,
    t_end_ns: i64,
    audible: Audible<'_>,
) -> f64 {
    let dur = (t_end_ns - t_start_ns) as f64;
    if dur <= 0.0 {
        return 0.0;
    }
    // Per user, clipped to the segment and merged.
    let mut by_user: Vec<(String, Vec<(i64, i64)>)> = Vec::new();
    for s in spans {
        if !audible.hears(&s.user_id) {
            continue;
        }
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
                    // v13 runs before v17's columns exist, and it is a
                    // backfill over rows that predate every bridge there will
                    // ever be a second of: unscoped is the only honest answer
                    // and `Audible::everyone()` beside it is the matching one.
                    account_id: None,
                    client_kind: None,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if rows.is_empty() {
            continue;
        }
        // `Audible::everyone()`: a migration runs on a raw connection and has
        // no business resolving merged speakers to find the "You" account.
        // The 0.12.1 re-verdict pass (`truth::rejudge`) restamps exactly this
        // set of rows with the own-account rule applied, so a backfill and a
        // re-judge on the same upgrade end at the same number.
        set.execute(rusqlite::params![
            id,
            simultaneous_frac(&rows, a, b, Audible::everyone())
        ])?;
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
    /// 0.12.0: turns named from a `single` verdict that the ladder left blank.
    pub retro_labelled: AtomicU64,
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
            "retro_labelled": g(&self.retro_labelled),
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
    bridge: Option<&crate::bridge::Picker>,
) -> Result<bool> {
    // ---- gather (lock held) ----
    //
    // The own accounts come with the batch rather than per row: it is two
    // small queries, it cannot change inside one pass in any way that matters,
    // and asking once means the rule is read from the same place for every row
    // the batch judges.
    //
    // The bridges come with it, and for the same reasons: which client's spans
    // this source's audio carries cannot change inside one pass in any way that
    // matters, and reading it once means every row in the batch is judged
    // against the same answer.
    let (candidates, own, bridges) = {
        let guard = store.lock().unwrap_or_else(|p| p.into_inner());
        (
            guard.segments_for_truth(&cfg.sources, cfg.batch_segments)?,
            guard.own_discord_user_ids()?,
            guard.truth_bridges(utc_now_ns(), BRIDGE_RECENT_NS)?,
        )
    };
    if candidates.is_empty() {
        return Ok(false);
    }

    for c in candidates {
        if stop.stopped() || gate(control, cfg).is_some() {
            break;
        }
        // Whose spans this turn may be judged against (0.12.3). Resolved per
        // row because a batch spans several sources, and it is the difference
        // between a verdict about this call and a verdict about both of them.
        let scope = scope_of(&c.source, &bridges, bridge);

        // ---- gather (lock held) ----
        let (spans, nearby) = {
            let guard = store.lock().unwrap_or_else(|p| p.into_inner());
            let spans = guard.truth_spans_between_scoped(c.t_start_ns, c.t_end_ns, &scope)?;
            // "Was the plugin running at all around here?" — asked separately
            // and over a wider window, because a segment with no overlapping
            // span is the interesting case and the answer decides whether it
            // is `nobody` or `unknown`. Scoped too, and that is the point: with
            // the other client's call five minutes away, an unscoped reach turns
            // "this bridge saw nothing" into a confident `nobody`.
            let nearby = !guard
                .truth_spans_between_scoped(
                    c.t_start_ns - TRUTH_REACH_NS,
                    c.t_end_ns + TRUTH_REACH_NS,
                    &scope,
                )?
                .is_empty();
            (spans, nearby)
        };

        // ---- judge (no lock) ----
        let audible = Audible::new(&c.kind, &own).with_bridge_account(scope_account(&scope));
        let cov = coverage(&spans, c.t_start_ns, c.t_end_ns);
        let v = verdict(&cov, nearby, audible);
        // How much of the turn had two mouths open at once (v13). Stored
        // beside the verdict rather than derived later: the spans it is
        // computed from are subject to retention, the verdict is not.
        let simul = (!spans.is_empty())
            .then(|| simultaneous_frac(&spans, c.t_start_ns, c.t_end_ns, audible));

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

// ---- 0.12.1: the re-verdict pass ------------------------------------------

/// `operations.op` for one chunk of re-judged verdicts.
pub const OP_REJUDGE: &str = "truth.rejudge";

/// `settings` key holding the last re-judge's result, as JSON.
///
/// A settings row rather than a column or a migration flag: it is one small
/// fact about a pass that runs once, `truth report` is the only thing that
/// reads it, and its presence is also what stops the automatic run from
/// happening twice.
pub const REJUDGE_KEY: &str = "truth_rejudge";

/// How many rows one automatic re-judge will take before it stops for the
/// night. Ten thousand is more than the whole verdict table on the install
/// this was written for and small enough that a pathological archive cannot
/// turn first-start into an hour of SQLite.
pub const REJUDGE_AUTO_LIMIT: usize = 10_000;

/// One verdict the rule changed.
#[derive(Debug, Clone, PartialEq)]
pub struct Rejudged {
    pub segment_id: i64,
    pub t_start_ns: i64,
    pub from: String,
    pub to: String,
    /// The user the new verdict is about, when it is about one.
    pub user_id: Option<String>,
    pub coverage: Option<f64>,
    /// The re-measured simultaneous share, or `None` where the spans are gone
    /// and the old number has to stand.
    pub overlap_frac: Option<f64>,
    /// False when the answer came from the stored columns rather than from
    /// the speaking spans, because the spans are no longer on disk.
    pub from_spans: bool,
}

/// What a re-judge did, or would do.
#[derive(Debug, Clone, Default)]
pub struct RejudgeReport {
    /// Verdicts looked at: every `single`, `overlap` and `partial` on disk.
    pub examined: usize,
    /// The ones the rule moves.
    pub changed: Vec<Rejudged>,
    /// Rows re-judged from the stored columns because the spans are gone.
    pub from_columns: usize,
    /// `overlap` rows whose spans are gone: the one case the rule can neither
    /// confirm nor correct, because the verdict never wrote down *who*.
    pub unresolvable: usize,
    /// Rows whose stored `truth_overlap_frac` was re-measured.
    pub restamped: usize,
}

impl RejudgeReport {
    /// `overlap` verdicts the rule took away — the eight-fold number §34 is
    /// about, and the one `truth report` prints.
    pub fn overlap_reassigned(&self) -> usize {
        self.changed
            .iter()
            .filter(|r| r.from == truth_verdict::OVERLAP)
            .count()
    }

    /// `from -> to`, counted, biggest first. What a person actually reads.
    pub fn moves(&self) -> Vec<(String, String, usize)> {
        let mut out: Vec<(String, String, usize)> = Vec::new();
        for r in &self.changed {
            match out.iter_mut().find(|(f, t, _)| *f == r.from && *t == r.to) {
                Some((_, _, n)) => *n += 1,
                None => out.push((r.from.clone(), r.to.clone(), 1)),
            }
        }
        out.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
        out
    }

    pub fn to_json(&self, at_ns: i64) -> Value {
        json!({
            "at_ms": ns_to_ms(at_ns),
            "examined": self.examined,
            "changed": self.changed.len(),
            "overlap_reassigned": self.overlap_reassigned(),
            "from_columns": self.from_columns,
            "unresolvable": self.unresolvable,
            "restamped": self.restamped,
            "moves": self.moves()
                .iter()
                .map(|(f, t, n)| json!({"from": f, "to": t, "n": n}))
                .collect::<Vec<_>>(),
        })
    }
}

/// Re-judge the verdicts already on disk under the own-account rule
/// (0.12.1, FINDINGS §34).
///
/// ## Why a pass and not a migration
///
/// The rule changed what a verdict *means*, and the verdicts on disk were
/// written by the old meaning. 1,791 `overlap` rows on the install this was
/// measured on; 1,341 of them were the user talking over one other person on
/// audio that cannot carry the user, i.e. one voice wearing an `overlap`
/// label. Everything downstream — the overlap gate's precision and recall,
/// the calibration corpus, `identity calibrate`'s held-out gate curve — reads
/// those rows, so leaving them is not "old data", it is a wrong answer key
/// that keeps being marked against.
///
/// ## What it can and cannot re-derive
///
/// The verdict is recomputed from the speaking spans, exactly as
/// [`label_batch`] would compute it today. Where the spans are gone the pass
/// falls back to the stored columns and says which rows it did that for:
///
/// * a `single` or `partial` naming an account this stream cannot contain is
///   `nobody` with no spans needed — the verdict itself records that nobody
///   else reached the presence bar, which is the whole question;
/// * a `single` or `partial` naming anybody else cannot move, for the same
///   reason read the other way round;
/// * an **`overlap` row with no surviving spans cannot be re-judged at all**.
///   It records that two accounts were present and not which two, and the
///   answer is one of them. Those rows are left exactly as they are and
///   counted in [`RejudgeReport::unresolvable`], because a pass that guessed
///   here would be inventing the thing it exists to correct.
///
/// `apply` false computes everything and writes nothing.
pub fn rejudge(
    store: &Arc<std::sync::Mutex<Store>>,
    limit: usize,
    apply: bool,
    at_ns: i64,
    bridge: Option<&crate::bridge::Picker>,
) -> Result<RejudgeReport> {
    // ---- gather (lock held) ----
    //
    // The own accounts first and on their own: nothing linked to "You" means
    // nothing is known to be inaudible, and a pass with no rule to apply must
    // not touch a row — nor walk the verdict table to discover that.
    let own = {
        let guard = store.lock().unwrap_or_else(|p| p.into_inner());
        guard.own_discord_user_ids()?
    };
    if own.is_empty() {
        return Ok(RejudgeReport::default());
    }
    let (candidates, bridges) = {
        let guard = store.lock().unwrap_or_else(|p| p.into_inner());
        (
            guard.segments_for_rejudge(limit)?,
            // 0.12.3. On an archive written before the field existed this is
            // empty, `scope_of` answers `Every`, and the pass re-derives
            // exactly what 0.12.1 measured — which is the contract: a verdict
            // already on disk is never re-scoped by a bridge that arrived
            // afterwards.
            guard.truth_bridges(at_ns, BRIDGE_RECENT_NS)?,
        )
    };
    let mut report = RejudgeReport {
        examined: candidates.len(),
        ..Default::default()
    };

    for c in candidates {
        let scope = scope_of(&c.source, &bridges, bridge);
        let audible = Audible::new(&c.kind, &own).with_bridge_account(scope_account(&scope));
        // ---- gather (lock held) ----
        let (spans, nearby) = {
            let guard = store.lock().unwrap_or_else(|p| p.into_inner());
            let spans = guard.truth_spans_between_scoped(c.t_start_ns, c.t_end_ns, &scope)?;
            let nearby = !guard
                .truth_spans_between_scoped(
                    c.t_start_ns - TRUTH_REACH_NS,
                    c.t_end_ns + TRUTH_REACH_NS,
                    &scope,
                )?
                .is_empty();
            (spans, nearby)
        };

        // ---- judge (no lock) ----
        let (v, simul, from_spans) = if spans.is_empty() {
            let silenced = c.user_id.as_deref().is_some_and(|u| !audible.hears(u));
            match (c.verdict.as_str(), silenced) {
                // The verdict is its own evidence: it says one account was
                // present and this stream cannot hold that account. `nobody`
                // is what is left, and truth demonstrably covered this moment
                // once — a `single` is not written over a gap.
                (truth_verdict::SINGLE | truth_verdict::PARTIAL, true) => {
                    report.from_columns += 1;
                    (Verdict::Nobody, None, false)
                }
                (truth_verdict::SINGLE | truth_verdict::PARTIAL, false) => continue,
                // `overlap` never wrote down who. Left alone, and counted.
                _ => {
                    report.unresolvable += 1;
                    continue;
                }
            }
        } else {
            let cov = coverage(&spans, c.t_start_ns, c.t_end_ns);
            let v = verdict(&cov, nearby, audible);
            let f = simultaneous_frac(&spans, c.t_start_ns, c.t_end_ns, audible);
            (v, Some(f), true)
        };

        let moved = v.as_str() != c.verdict
            || v.user_id().map(str::to_string) != c.user_id
            || v.coverage() != c.coverage;
        let restamp =
            simul.is_some_and(|f| c.overlap_frac.is_none_or(|old| (old - f).abs() > 1e-9));
        if !moved && !restamp {
            continue;
        }
        if moved {
            report.changed.push(Rejudged {
                segment_id: c.id,
                t_start_ns: c.t_start_ns,
                from: c.verdict.clone(),
                to: v.as_str().to_string(),
                user_id: v.user_id().map(str::to_string),
                coverage: v.coverage(),
                overlap_frac: simul,
                from_spans,
            });
        }
        if restamp {
            report.restamped += 1;
        }

        // ---- commit (lock held) ----
        if apply {
            let guard = store.lock().unwrap_or_else(|p| p.into_inner());
            if moved {
                guard.set_segment_truth(c.id, v.user_id(), v.as_str(), v.coverage())?;
            }
            if let Some(f) = simul
                && restamp
            {
                guard.set_segment_truth_overlap(c.id, f)?;
            }
        }
    }

    if apply {
        let guard = store.lock().unwrap_or_else(|p| p.into_inner());
        // The prior state, chunked like `truth.label`'s and for the same
        // reason: one `operations` row carrying four thousand segments is a
        // row nothing can read back.
        for chunk in report.changed.chunks(LABEL_CHUNK) {
            let targets: Vec<i64> = chunk.iter().map(|r| r.segment_id).collect();
            let prior = json!({
                "segments": chunk.iter().map(|r| json!({
                    "segment_id": r.segment_id,
                    "truth_verdict": r.from,
                    "to_verdict": r.to,
                    "to_user_id": r.user_id,
                    "to_coverage": r.coverage,
                    "from_spans": r.from_spans,
                })).collect::<Vec<_>>(),
            });
            guard.log_operation(
                OP_REJUDGE,
                &serde_json::to_string(&targets)?,
                &prior.to_string(),
                at_ns,
            )?;
        }
        guard.set_setting(REJUDGE_KEY, &report.to_json(at_ns).to_string())?;
    }
    Ok(report)
}

/// The automatic half: re-judge once, on the first start after the upgrade,
/// and never again.
///
/// It runs here rather than in a migration because it is not one — it reads
/// the speaking spans, it can take thousands of small queries, and a
/// migration that does that runs while the user is waiting for the daemon to
/// come up. The worker already has the discipline this needs: gather under
/// the lock, judge with none held, and stand down while capture is busy.
///
/// It is bounded three ways — [`REJUDGE_AUTO_LIMIT`] rows, only the verdicts
/// that assert somebody was present, and only once, on a `settings` row it
/// writes itself. And it is gated on `[truth].label` with everything else in
/// this worker: the switch that lets the daemon write verdicts is the switch
/// that lets it correct them. With labelling off, `recalld truth rejudge`
/// is the route and the operator drives it.
fn rejudge_once(
    store: &Arc<std::sync::Mutex<Store>>,
    bridge: Option<&crate::bridge::Picker>,
) -> Result<()> {
    {
        let guard = store.lock().unwrap_or_else(|p| p.into_inner());
        if guard.setting(REJUDGE_KEY)?.is_some() {
            return Ok(());
        }
    }
    let report = rejudge(store, REJUDGE_AUTO_LIMIT, true, utc_now_ns(), bridge)?;
    info!(
        examined = report.examined,
        changed = report.changed.len(),
        overlap_reassigned = report.overlap_reassigned(),
        unresolvable = report.unresolvable,
        "re-judged the verdicts on disk: your own account is not presence on \
         audio your own client produced"
    );
    Ok(())
}

// ---- end 0.12.1 ------------------------------------------------------------

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

// ---- 0.12.0: retro-labelling from ground truth -----------------------------

/// How many rows one `truth label` pass will move at most.
///
/// Two hundred, matching `service`'s delete chunk rather than
/// `[truth].batch_segments`, because the number that matters here is how many
/// segments one `operations` row is allowed to describe. The nightly caller
/// loops until the pass returns zero, so this bounds the transaction and the
/// undo record, never the work.
pub const LABEL_CHUNK: usize = 200;

/// The `operations` op name for a retro-labelling pass. One row per chunk,
/// carrying every segment's prior state, so the whole pass is reversible the
/// way `speakers.split` is.
pub const OP_LABEL: &str = "truth.label";

/// One row moved by [`label_from_truth`], for the report and the undo record.
#[derive(Debug, Clone, PartialEq)]
pub struct Relabelled {
    pub segment_id: i64,
    pub speaker_id: i64,
    pub user_id: String,
    pub user_name: String,
    pub t_start_ns: i64,
    pub duration_s: f64,
    pub coverage: Option<f64>,
}

/// Put Discord's name on the turns the voicebank left blank.
///
/// ## Why this exists
///
/// The identity ladder's failure mode is not usually a *wrong* name, it is *no*
/// name: a turn whose top candidate missed the label bar keeps its transcript
/// and its embedding and stays speaker-NULL. On the install §29 was measured
/// on that is 168 turns — and for every one of them Discord had already written
/// down who was talking, in a `single` verdict at ≥ 0.8 coverage, for a user
/// that the auto-linker had already tied to a voice. Two facts the database
/// held all along, never joined.
///
/// ## What it will not do
///
/// * **It never overwrites a label.** The `WHERE` demands `speaker_id IS NULL`
///   in both the gather and the commit. A row the ladder named, a row a person
///   named, a row proximity inherited — all untouched. Discord's word is used
///   only where nothing else had a word at all, so this pass cannot lower the
///   93.9% identity precision `truth report` measures: every row it writes was
///   previously counted `unlabelled`, and none was counted `correct`.
/// * **It never enrols.** Not one prototype comes out of this, however clean
///   the turn. `calib.rs` and `identity.rs` are the only things that have ever
///   decided a recording is worth keeping, and the enrol bar is deliberately
///   not learned; a pass that added prototypes on Discord's word alone would be
///   the enrol bar learning itself through the side door. `truth::enrol_batch`
///   is the supervised route and it stays behind `[truth] enrol`.
/// * **It never mints.** Only users the auto-linker already tied to an existing
///   voice are read, so no new identity can appear here.
///
/// Returns the rows it moved. With `apply` false it returns exactly the same
/// list and writes nothing — the preview is the same computation, not a
/// second one that could disagree with it.
pub fn label_from_truth(
    store: &Arc<std::sync::Mutex<Store>>,
    limit: usize,
    apply: bool,
    at_ns: i64,
) -> Result<Vec<Relabelled>> {
    // ---- gather (lock held) ----
    let candidates = {
        let guard = store.lock().unwrap_or_else(|p| p.into_inner());
        guard.segments_for_truth_label(guard.you_speaker_id()?, limit)?
    };
    if candidates.is_empty() {
        return Ok(Vec::new());
    }

    let mut moved = Vec::new();
    // ---- commit (lock held) ----
    //
    // One acquisition for the whole chunk. The pass writes at most
    // `LABEL_CHUNK` single-row UPDATEs against a primary key and then one
    // `operations` insert, which is nothing beside the embedding decode
    // `calibrate_pass` was warned about; splitting it per row would only widen
    // the window in which a row can be labelled underneath us.
    let guard = store.lock().unwrap_or_else(|p| p.into_inner());
    for c in candidates {
        // Losing the race to the live ladder is the right outcome, so the
        // preview counts a row it could still lose and the apply does not.
        if apply && !guard.label_segment_from_truth(c.id, c.speaker_id)? {
            continue;
        }
        moved.push(Relabelled {
            segment_id: c.id,
            speaker_id: c.speaker_id,
            user_id: c.user_id,
            user_name: c.user_name,
            t_start_ns: c.t_start_ns,
            duration_s: (c.t_end_ns - c.t_start_ns) as f64 / 1e9,
            coverage: c.coverage,
        });
    }

    if apply {
        // Every row this pass touches had `speaker_id NULL, match_score NULL,
        // label_via NULL` — that is the query's precondition, not an
        // assumption — so the prior state is fully described by the id list.
        // It is written out per row anyway, in `speakers.split`'s shape,
        // because an undo that has to re-derive a precondition from the op name
        // is an undo that breaks the day the precondition changes.
        //
        // Chunked at `LABEL_CHUNK` for `speakers.delete`'s reason rather than
        // this pass's: the nightly caller never hands us more than a chunk, but
        // `recalld truth label --apply` with no `--limit` hands us the whole
        // backlog, and one `operations` row carrying ten thousand segments is a
        // row nothing can read back.
        for chunk in moved.chunks(LABEL_CHUNK) {
            let targets: Vec<i64> = chunk.iter().map(|m| m.segment_id).collect();
            let prior = json!({
                "segments": chunk.iter().map(|m| json!({
                    "segment_id": m.segment_id,
                    "speaker_id": Value::Null,
                    "match_score": Value::Null,
                    "label_via": Value::Null,
                    "to_speaker_id": m.speaker_id,
                    "truth_user_id": m.user_id,
                })).collect::<Vec<_>>(),
            });
            guard.log_operation(
                OP_LABEL,
                &serde_json::to_string(&targets)?,
                &prior.to_string(),
                at_ns,
            )?;
        }
    }
    Ok(moved)
}

/// The nightly half: keep going until there is nothing left, then stop.
///
/// Gated on `[truth].label` with the verdict pass, and for the same reason —
/// this reads verdicts that pass writes, so running it while labelling is off
/// would work through a backlog that has stopped growing and then spin.
fn label_from_truth_pass(store: &Arc<std::sync::Mutex<Store>>, stats: &TruthStats) -> Result<()> {
    loop {
        let moved = label_from_truth(store, LABEL_CHUNK, true, utc_now_ns())?;
        if moved.is_empty() {
            return Ok(());
        }
        stats
            .retro_labelled
            .fetch_add(moved.len() as u64, Ordering::Relaxed);
        info!(
            n = moved.len(),
            "named turns the voicebank had left blank, on Discord's word"
        );
        if moved.len() < LABEL_CHUNK {
            return Ok(());
        }
    }
}

// ---- end 0.12.0 ------------------------------------------------------------

/// Enrol the cleanest of a linked user's turns into that voice's bank.
///
/// The bar is Discord's *and* the voicebank's: a turn has to be a `single`
/// verdict at ≥ 0.95 coverage and ≥ 3 s **and** pass
/// [`crate::identity::decide`]'s existing enrol rules against the bank as it
/// stands. Ground truth says whose voice it is; it does not say the recording
/// is worth keeping, and the four conditions in `identity.rs` are the only
/// thing that ever decided that.
///
/// **The scale rule** (§36): the score those conditions read has to come off
/// the same aggregate the live ladder uses. Until 0.12.2 this pass called
/// [`crate::identity::rank`] — a hard-coded max over a voice's prototypes —
/// while `analysis` ranked with the *learned* aggregate and the per-voice
/// thresholds were fitted on that aggregate's scale. On an install that had
/// learned `top-3`, the label half of the decision compared a max-cosine score
/// against a top-3 threshold, and the enrol half compared it against a 0.55
/// that means something else on each scale. §32 wrote the rule down for the
/// projection and PLDA-lite; this is the same rule, applied to the one caller
/// that had been missed. It is behind `[truth] enrol` and that switch has
/// never been on, so nothing on disk was ever written by the old behaviour.
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
                    // ---- 0.12.2: the same scale as the ladder ----------
                    // Read the same way `analysis::learned_aggregate` reads
                    // it, and for the same reason: an unreadable value costs
                    // the improvement, never the decision.
                    let aggregate = if identity.learn {
                        guard
                            .learned_aggregate()
                            .unwrap_or(crate::calib::Aggregate::Max)
                    } else {
                        crate::calib::Aggregate::Max
                    };
                    // ---- end 0.12.2 ------------------------------------
                    Some((embedding, bank, thresholds, aggregate))
                }
                // Nothing was ever embedded — refused at the identity gate, or
                // recorded before the models were installed. Stamped so it is
                // not asked about again.
                None => None,
            }
        };
        let Some((embedding, bank, thresholds, aggregate)) = gathered else {
            let guard = store.lock().unwrap_or_else(|p| p.into_inner());
            guard.mark_truth_enrol_considered(c.id, at)?;
            continue;
        };

        // ---- judge (no lock) ----
        let ranked = crate::identity::rank_with(&embedding, &bank, aggregate)?;
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
    // The live instance picker (0.12.3), for the one thing the database
    // cannot answer: a manual `bridge:<account_id>` override the user set
    // this evening, which has to reach the verdict scope without a restart.
    bridge: Option<Arc<crate::bridge::Picker>>,
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
                // ---- 0.12.1: the re-verdict, once ------------------------
                // Before the labelling batch and before everything that reads
                // a verdict: the corrected verdicts are what the auto-linker,
                // the retro-labeller and the calibration pass should see on
                // the very first evening after the upgrade, not on the second.
                if let Err(e) = rejudge_once(&store, bridge.as_deref()) {
                    warn!("the ground-truth re-verdict pass failed: {e:#}");
                }
                // ---- end 0.12.1 ------------------------------------------
                if let Err(e) =
                    label_batch(&store, &control, &cfg, &stats, &stop, bridge.as_deref())
                {
                    warn!("a ground-truth labelling batch failed: {e:#}");
                }
                if let Err(e) = link_batch(&store, &bus, &stats) {
                    warn!("the ground-truth auto-linker failed: {e:#}");
                }
                // ---- 0.12.0: retro-labelling -----------------------------
                // After the linker and never before it: the pass can only act
                // on users that are already linked, so running it first would
                // do nothing on the evening a link is made and leave the
                // backlog for tomorrow.
                if let Err(e) = label_from_truth_pass(&store, &stats) {
                    warn!("the ground-truth retro-labelling pass failed: {e:#}");
                }
                // ---- end 0.12.0 ------------------------------------------
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
        // ---- 0.12.0: the two queues, said out loud ----
        //
        // §29 went looking for a bug in `enrol_batch` and found an off switch:
        // `segments.truth_enrol_ns` was NULL on all 1,461 `single` rows because
        // `[truth] enrol` defaults to false and had never been turned on, while
        // 137 turns sat queued for it. Nothing anybody could run said so — the
        // report showed enrolment neither working nor waiting. These two counts
        // are the cure, and they are counts rather than a flag on purpose: "off"
        // is a setting, "off with 137 turns waiting" is a decision.
        "enrol": {
            "on": cfg.enrol,
            "waiting": store.truth_enrol_waiting(ENROL_MIN_DURATION_S, ENROL_MIN_COVERAGE)?,
            "min_duration_ms": (ENROL_MIN_DURATION_S * 1000.0) as i64,
            "min_coverage": ENROL_MIN_COVERAGE,
        },
        "retro_label": {
            "waiting": store
                .segments_for_truth_label(store.you_speaker_id()?, usize::MAX)?
                .len() as i64,
        },
        // ---- end 0.12.0 ----
        // ---- 0.12.1: what the own-account rule took off the `overlap` pile
        //
        // Reported rather than merely done, because "there are 450 overlap
        // rows" and "there are 450 overlap rows, and 1,341 more used to be
        // counted here" are different facts about the same install, and every
        // measurement anybody made before the rule read the second number
        // without knowing it. `null` until the pass has run: nothing was
        // reassigned is a claim, and an unrun pass has not made it.
        "rejudge": store
            .setting(REJUDGE_KEY)?
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .unwrap_or(Value::Null),
        // ---- end 0.12.1 ----
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
        span_from(user, from_ms, to_ms, None)
    }

    /// The same, from a named bridge (0.12.3).
    fn span_from(user: &str, from_ms: i64, to_ms: i64, account: Option<&str>) -> TruthSpan {
        TruthSpan {
            user_id: user.to_string(),
            name: format!("{user}-nick"),
            t_start_ns: from_ms * 1_000_000,
            t_end_ns: to_ms * 1_000_000,
            account_id: account.map(str::to_string),
            client_kind: account.map(|_| "vesktop".to_string()),
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
        simultaneous_frac(spans, SEG.0, SEG.1, Audible::everyone())
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
        assert!(matches!(
            verdict(&cov, true, Audible::everyone()),
            Verdict::Overlap
        ));
    }

    #[test]
    fn the_interjection_case_reads_as_the_interjection_it_is() {
        // Six seconds of one person, half a second of another across it.
        let spans = [span("a", 0, 6000), span("b", 2000, 2500)];
        let f = simultaneous_frac(&spans, 0, 6_000_000_000, Audible::everyone());
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
        let f = simultaneous_frac(
            &[span("a", -1000, 250), span("b", -1000, 250)],
            0,
            SEG.1,
            Audible::everyone(),
        );
        assert!((f - 0.25).abs() < 1e-9, "{f}");
        // Entirely outside contributes nothing.
        assert_eq!(simul(&[span("a", 2000, 3000), span("b", 2000, 3000)]), 0.0);
    }

    #[test]
    fn a_zero_length_segment_has_no_simultaneity_rather_than_a_division_by_zero() {
        assert_eq!(
            simultaneous_frac(&[span("a", 0, 100)], 500, 500, Audible::everyone()),
            0.0
        );
        assert_eq!(
            simultaneous_frac(&[span("a", 0, 100)], 500, 400, Audible::everyone()),
            0.0
        );
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
            verdict(
                &coverage(spans, seg_start, seg_end),
                nearby,
                Audible::everyone(),
            )
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

    // ---- 0.12.1: the own-account rule (FINDINGS §34) ---------------------

    const ME: &str = "me";

    /// The user's own account, as `Audible` gets it from the store.
    fn own() -> Vec<String> {
        vec![ME.to_string()]
    }

    fn judge_on(kind: &str, own: &[String], spans: &[TruthSpan], nearby: bool) -> Verdict {
        verdict(
            &coverage(spans, 0, 1_000_000_000),
            nearby,
            Audible::new(kind, own),
        )
    }

    #[test]
    fn your_own_ring_is_not_a_second_voice_on_audio_your_own_client_made() {
        // 1,306 of this install's 1,509 two-user `overlap` verdicts are this
        // exact picture: somebody talking across most of the turn, the user
        // talking over them, and a recording that cannot contain the user
        // (§17 finding 1, §33). It is single-speaker audio wearing an
        // `overlap` label, and the label is what has to go.
        let spans = [span("aspen", 0, 900), span(ME, 200, 700)];
        assert_eq!(
            judge_on(crate::store::KIND_APP, &own(), &spans, true),
            Verdict::Single {
                user_id: "aspen".into(),
                frac: 0.9,
            },
            "with the user's ring dropped, Aspen is alone and over the bar"
        );
        // The arithmetic is untouched: `coverage` still describes the CALL,
        // and the user is still in it at half of the turn. Only presence —
        // the claim about the recording — changed.
        let cov = coverage(&spans, 0, 1_000_000_000);
        assert!(cov.iter().any(|c| c.user_id == ME && c.frac > 0.4));
    }

    #[test]
    fn on_the_microphone_your_own_account_is_the_only_voice_that_can_be_there() {
        // The mirror image, and the reason the rule is about the SOURCE and
        // not about the account. A microphone hears the user and nobody
        // else's Discord audio; dropping them there would delete the one
        // verdict that stream can produce.
        let alone = [span(ME, 0, 1000)];
        assert_eq!(
            judge_on(crate::store::KIND_MIC, &own(), &alone, true),
            Verdict::Single {
                user_id: ME.into(),
                frac: 1.0,
            }
        );
        // And a room mic hears whoever is in the room, the user included.
        assert!(matches!(
            judge_on(crate::store::KIND_ROOM, &own(), &alone, true),
            Verdict::Single { .. }
        ));
        // Two people on a microphone is still two people.
        let both = [span("aspen", 0, 900), span(ME, 200, 700)];
        assert_eq!(
            judge_on(crate::store::KIND_MIC, &own(), &both, true),
            Verdict::Overlap
        );
        // On application audio, the same two rings are one voice.
        assert!(matches!(
            judge_on(crate::store::KIND_APP, &own(), &both, true),
            Verdict::Single { .. }
        ));
    }

    #[test]
    fn dropping_your_ring_can_leave_a_partial_or_nobody_and_says_so() {
        // Aspen only reaches half the turn: `partial`, which is the verdict
        // that exists so a half-covered turn is neither scored as clean nor
        // claimed as overlapped. Promoting these to `single` would lower the
        // 0.8 bar through the back door.
        let half = [span("aspen", 0, 500), span(ME, 0, 900)];
        assert!(matches!(
            judge_on(crate::store::KIND_APP, &own(), &half, true),
            Verdict::Partial { ref user_id, frac } if user_id == "aspen" && (frac - 0.5).abs() < 1e-9
        ));
        // Nobody but the user: `nobody`, and that is exactly what `nobody`
        // has always meant — truth covers this moment and none of the voices
        // this recording can hold was in it.
        let only_me = [span(ME, 0, 1000)];
        assert_eq!(
            judge_on(crate::store::KIND_APP, &own(), &only_me, true),
            Verdict::Nobody
        );
        // With no truth data anywhere near, it is still `unknown`: the rule
        // removes a voice, it does not manufacture coverage.
        assert_eq!(
            judge_on(crate::store::KIND_APP, &own(), &only_me, false),
            Verdict::Unknown
        );
        // A user under the presence bar was never present, and the rule does
        // not change that either.
        assert_eq!(
            judge_on(crate::store::KIND_APP, &own(), &[span(ME, 0, 100)], true),
            Verdict::Nobody
        );
    }

    #[test]
    fn with_nothing_linked_to_you_every_ring_still_counts() {
        // `Audible` with an empty own-list is the state of an install that
        // has never linked an account: nothing is known to be inaudible, and
        // a rule with no subject must not fire.
        let spans = [span("aspen", 0, 900), span(ME, 200, 700)];
        assert_eq!(
            judge_on(crate::store::KIND_APP, &[], &spans, true),
            Verdict::Overlap
        );
        assert_eq!(
            verdict(
                &coverage(&spans, 0, 1_000_000_000),
                true,
                Audible::everyone()
            ),
            Verdict::Overlap
        );
    }

    #[test]
    fn an_alt_account_is_as_inaudible_as_the_main_one() {
        let two = vec![ME.to_string(), "me-alt".to_string()];
        let spans = [span("aspen", 0, 900), span("me-alt", 200, 700)];
        assert!(matches!(
            judge_on(crate::store::KIND_APP, &two, &spans, true),
            Verdict::Single { ref user_id, .. } if user_id == "aspen"
        ));
    }

    #[test]
    fn the_simultaneous_share_does_not_count_a_mouth_the_stream_cannot_carry() {
        // §26 read the median `overlap` turn as 47% simultaneous. Seven of
        // every eight of those turns were one person talking, and the second
        // "mouth" was the user's own — which is why this number takes the
        // same rule the verdict does.
        let spans = [span("aspen", 0, 1000), span(ME, 0, 500)];
        assert_eq!(
            simultaneous_frac(
                &spans,
                0,
                1_000_000_000,
                Audible::new(crate::store::KIND_APP, &own())
            ),
            0.0
        );
        // On a microphone the same two rings really are two mouths.
        assert!(
            (simultaneous_frac(
                &spans,
                0,
                1_000_000_000,
                Audible::new(crate::store::KIND_MIC, &own())
            ) - 0.5)
                .abs()
                < 1e-9
        );
        // And a real second voice is untouched by the rule.
        let real = [
            span("aspen", 0, 1000),
            span("rowan", 0, 500),
            span(ME, 0, 900),
        ];
        assert!(
            (simultaneous_frac(
                &real,
                0,
                1_000_000_000,
                Audible::new(crate::store::KIND_APP, &own())
            ) - 0.5)
                .abs()
                < 1e-9
        );
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

    // ---- 0.12.2: the enrol pass reads the ladder's own scale --------------

    /// One linked voice, one enrolable turn, and a bank whose *best* prototype
    /// is a near-perfect match while the voice's record as a whole is not.
    ///
    /// Max cosine 1.00; top-3 mean (1.00 + 0.20 + 0.20) / 3 = 0.467. The label
    /// bar (0.35) is cleared on either scale, the enrol bar (0.55) only on the
    /// max one — so the two scales give opposite answers on this one turn,
    /// which is the whole point of the fixture.
    fn a_store_with_one_enrolable_turn() -> (Store, i64) {
        use crate::embed::Embedding;
        use crate::store::truth_via;
        let s = Store::open_in_memory().unwrap();
        let src = s.upsert_source("Discord", "Discord", 1).unwrap();
        let sess = s.begin_session(src, 0).unwrap();
        let sp = s.mint_speaker(0).unwrap();
        s.upsert_discord_user("u1", "u1", 0).unwrap();
        s.set_discord_link("u1", Some(sp), Some(truth_via::MANUAL), 0)
            .unwrap();
        let far = (1.0f32 - 0.2 * 0.2).sqrt();
        for v in [
            vec![1.0f32, 0.0, 0.0],
            vec![0.2, far, 0.0],
            vec![0.2, 0.0, far],
        ] {
            s.add_prototype(sp, &Embedding::new("m@1", v), None, false, 20, 0)
                .unwrap();
        }
        let sec = 1_000_000_000i64;
        let seg = s
            .insert_segment(sess, 10 * sec, 15 * sec, "a.wav", 0)
            .unwrap();
        s.store_embedding(seg, &Embedding::new("m@1", vec![1.0, 0.0, 0.0]))
            .unwrap();
        s.set_segment_truth(seg, Some("u1"), "single", Some(0.99))
            .unwrap();
        (s, sp)
    }

    fn enrol_once(store: Store, speaker: i64, learn: bool) -> (i64, u64) {
        let store = Arc::new(std::sync::Mutex::new(store));
        let control = Control::new(
            std::path::PathBuf::from("/nonexistent"),
            None,
            &crate::allowlist::Allowlist::default(),
        );
        let identity = IdentityConfig {
            learn,
            ..Default::default()
        };
        let stats = TruthStats::default();
        let stop = TruthStop::default();
        enrol_batch(
            &store,
            &control,
            &TruthConfig::default(),
            &identity,
            &stats,
            &stop,
        )
        .unwrap();
        let guard = store.lock().unwrap();
        (
            guard.prototype_count(speaker).unwrap(),
            stats.enrolled.load(Ordering::Relaxed),
        )
    }

    #[test]
    fn the_enrol_pass_scores_on_the_aggregate_the_ladder_learned() {
        // The bar it has to clear is 0.55 on the scale the *thresholds* were
        // fitted on. Under `top-3` this voice scores 0.467, so the turn is a
        // match the pass must decline to enrol — under the max the pass used
        // to hard-code it scores 1.00 and sails through, which is a 0.55 that
        // means something else.
        let (s, sp) = a_store_with_one_enrolable_turn();
        s.set_learned_aggregate(crate::calib::Aggregate::TopK(3))
            .unwrap();
        let (protos, enrolled) = enrol_once(s, sp, true);
        assert_eq!(protos, 3, "no prototype is added under the learned scale");
        assert_eq!(enrolled, 0);
    }

    #[test]
    fn the_same_turn_still_enrols_where_the_bank_is_scored_on_its_best() {
        // The companion, so the test above cannot pass by refusing everything:
        // with nothing learned the aggregate is `max`, and the same fixture
        // enrols.
        let (s, sp) = a_store_with_one_enrolable_turn();
        let (protos, enrolled) = enrol_once(s, sp, true);
        assert_eq!(protos, 4, "the max scale enrols this turn");
        assert_eq!(enrolled, 1);
    }

    #[test]
    fn a_store_that_has_learned_an_aggregate_is_ignored_when_learning_is_off() {
        // `[identity] learn = false` means every learned value is ignored, and
        // the aggregate is one, exactly as the threshold table already is.
        let (s, sp) = a_store_with_one_enrolable_turn();
        s.set_learned_aggregate(crate::calib::Aggregate::TopK(3))
            .unwrap();
        let (protos, enrolled) = enrol_once(s, sp, false);
        assert_eq!(protos, 4);
        assert_eq!(enrolled, 1);
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

    // ---- 0.12.0: retro-labelling from ground truth -------------------------

    /// A store with one linked user and one unlinked one, four `single` turns
    /// each, and every turn left unlabelled by the ladder.
    fn a_store_to_relabel() -> Arc<std::sync::Mutex<Store>> {
        use crate::store::truth_via;
        let s = Store::open_in_memory().unwrap();
        let src = s.upsert_source("vesktop", "Vesktop", 1).unwrap();
        let sess = s.begin_session(src, 0).unwrap();
        let sec = 1_000_000_000i64;
        let linked = s.mint_speaker(0).unwrap();
        s.upsert_discord_user("linked", "Aspen", 0).unwrap();
        s.set_discord_link("linked", Some(linked), Some(truth_via::MANUAL), 0)
            .unwrap();
        // A user the auto-linker has NOT tied to any voice. Its turns are
        // exactly as well attested and must still be left alone.
        s.upsert_discord_user("loose", "Nobody's Voice", 0).unwrap();
        for (i, user) in ["linked", "loose"].iter().enumerate() {
            for k in 0..4i64 {
                let t = (i as i64 * 100 + k + 1) * 10 * sec;
                let seg = s.insert_segment(sess, t, t + 2 * sec, "a.wav", 0).unwrap();
                s.set_segment_truth(seg, Some(user), "single", Some(0.95))
                    .unwrap();
            }
        }
        Arc::new(std::sync::Mutex::new(s))
    }

    fn label_of(store: &Arc<std::sync::Mutex<Store>>, seg: i64) -> (Option<i64>, Option<String>) {
        let g = store.lock().unwrap();
        g.conn()
            .query_row(
                "SELECT speaker_id, label_via FROM segments WHERE id = ?1",
                [seg],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    }

    #[test]
    fn a_preview_names_nothing() {
        let store = a_store_to_relabel();
        let moved = label_from_truth(&store, usize::MAX, false, 1).unwrap();
        assert_eq!(moved.len(), 4, "the four turns of the linked user");
        for m in &moved {
            assert_eq!(label_of(&store, m.segment_id), (None, None));
        }
        // And it is the same list the apply would move — the preview is the
        // same computation, not a second one that could disagree with it.
        let applied = label_from_truth(&store, usize::MAX, true, 1).unwrap();
        assert_eq!(
            applied.iter().map(|m| m.segment_id).collect::<Vec<_>>(),
            moved.iter().map(|m| m.segment_id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn discords_word_names_the_blank_turns_of_a_linked_user_only() {
        let store = a_store_to_relabel();
        let moved = label_from_truth(&store, usize::MAX, true, 1).unwrap();
        assert_eq!(moved.len(), 4);
        let linked = moved[0].speaker_id;
        for m in &moved {
            assert_eq!(m.user_id, "linked");
            assert_eq!(
                label_of(&store, m.segment_id),
                (
                    Some(linked),
                    Some(crate::store::label_via::TRUTH.to_string())
                ),
                "named on Discord's word, and said so"
            );
        }
        // The unlinked user's turns are just as well attested and stay blank:
        // there is no voice to point them at, and this pass never mints one.
        let g = store.lock().unwrap();
        let loose: i64 = g
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM segments WHERE truth_user_id = 'loose' AND speaker_id IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(loose, 4);
    }

    #[test]
    fn a_turn_that_already_has_a_speaker_is_never_touched() {
        let store = a_store_to_relabel();
        // Give one of the four a speaker the way the live ladder would, and a
        // different one from the linked voice, so an overwrite would be loud.
        let (first, other) = {
            let g = store.lock().unwrap();
            let first: i64 = g
                .conn()
                .query_row(
                    "SELECT MIN(id) FROM segments WHERE truth_user_id = 'linked'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            let other = g.mint_speaker(0).unwrap();
            g.set_segment_speaker(first, Some(other), Some(0.9))
                .unwrap();
            (first, other)
        };
        let moved = label_from_truth(&store, usize::MAX, true, 1).unwrap();
        assert_eq!(moved.len(), 3, "the labelled one is not a candidate");
        assert!(moved.iter().all(|m| m.segment_id != first));
        assert_eq!(
            label_of(&store, first),
            (
                Some(other),
                Some(crate::store::label_via::MATCH.to_string())
            ),
            "the ladder's label survived intact"
        );
    }

    #[test]
    fn the_pass_is_reversible_because_it_wrote_down_what_it_changed() {
        let store = a_store_to_relabel();
        let moved = label_from_truth(&store, usize::MAX, true, 4242).unwrap();
        let g = store.lock().unwrap();
        let ops = g.operations_of(OP_LABEL, 10).unwrap();
        assert_eq!(ops.len(), 1, "one row for the one chunk");
        assert_eq!(ops[0].at_utc_ns, 4242);
        let targets: Vec<i64> = serde_json::from_str(&ops[0].target_ids).unwrap();
        assert_eq!(
            targets,
            moved.iter().map(|m| m.segment_id).collect::<Vec<_>>()
        );
        let prior: Value = serde_json::from_str(&ops[0].prior_state).unwrap();
        let rows = prior["segments"].as_array().unwrap();
        assert_eq!(rows.len(), moved.len());
        for row in rows {
            // The whole point: what to put back, not merely what changed.
            assert!(row["speaker_id"].is_null());
            assert!(row["label_via"].is_null());
            assert!(row["to_speaker_id"].as_i64().is_some());
        }
    }

    #[test]
    fn a_second_pass_has_nothing_left_to_do() {
        let store = a_store_to_relabel();
        assert_eq!(
            label_from_truth(&store, usize::MAX, true, 1).unwrap().len(),
            4
        );
        assert!(
            label_from_truth(&store, usize::MAX, true, 2)
                .unwrap()
                .is_empty(),
            "the rows it named are no longer blank, so they are no longer candidates"
        );
        // …and the nightly wrapper therefore terminates rather than spinning.
        let stats = TruthStats::default();
        label_from_truth_pass(&store, &stats).unwrap();
        assert_eq!(stats.retro_labelled.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_short_turn_is_named_like_any_other() {
        // The one pass in this file with no duration bar, deliberately. It
        // measures nothing, so the reason every other pass has one does not
        // apply — and the shortest turns are the ones no other route can name.
        let store = a_store_to_relabel();
        let seg = {
            let g = store.lock().unwrap();
            let src = g.upsert_source("vesktop", "Vesktop", 1).unwrap();
            let sess = g.begin_session(src, 0).unwrap();
            // 300 ms: under `MIN_SCORE_DURATION_S` and under the embedder's bar.
            let seg = g
                .insert_segment(sess, 9_000, 309_000_000, "s.wav", 0)
                .unwrap();
            g.set_segment_truth(seg, Some("linked"), "single", Some(0.9))
                .unwrap();
            seg
        };
        let moved = label_from_truth(&store, usize::MAX, true, 1).unwrap();
        assert!(
            moved.iter().any(|m| m.segment_id == seg),
            "a 300 ms turn Discord is sure about is still a turn Discord is sure about"
        );
    }

    #[test]
    fn your_own_account_never_names_a_turn_however_sure_discord_is() {
        // The rule 0.10.1 found by measurement and this pass has to inherit: a
        // Discord client does not play your microphone back to you, so a turn
        // captured from its output is the one place your voice cannot be. A
        // `single` verdict on your own account means "you were talking over
        // this", and writing it as a label would put your name on somebody
        // else's voice — 17 times on the install §29 measured.
        let store = a_store_to_relabel();
        {
            let g = store.lock().unwrap();
            let you = g.ensure_you_speaker(0).unwrap();
            g.upsert_discord_user("me", "nerdrx", 0).unwrap();
            g.set_discord_link("me", Some(you), Some(crate::store::truth_via::MANUAL), 0)
                .unwrap();
            let src = g.upsert_source("vesktop", "Vesktop", 1).unwrap();
            let sess = g.begin_session(src, 0).unwrap();
            for k in 0..3i64 {
                let t = (500 + k) * 10 * 1_000_000_000;
                let seg = g
                    .insert_segment(sess, t, t + 4_000_000_000, "me.wav", 0)
                    .unwrap();
                // As clean as ground truth ever gets, and still refused.
                g.set_segment_truth(seg, Some("me"), "single", Some(1.0))
                    .unwrap();
            }
        }
        let moved = label_from_truth(&store, usize::MAX, true, 1).unwrap();
        assert!(
            moved.iter().all(|m| m.user_id != "me"),
            "your own account is not evidence about audio that cannot contain you"
        );
        assert_eq!(moved.len(), 4, "the other user's four turns still land");
    }

    #[test]
    fn only_a_single_verdict_counts_as_discords_word() {
        let store = a_store_to_relabel();
        let seg = {
            let g = store.lock().unwrap();
            let src = g.upsert_source("vesktop", "Vesktop", 1).unwrap();
            let sess = g.begin_session(src, 0).unwrap();
            let seg = g
                .insert_segment(sess, 5_000_000_000, 7_000_000_000, "p.wav", 0)
                .unwrap();
            // `partial` is the verdict that exists precisely because it is not
            // good enough to be `single`. It must not become a label.
            g.set_segment_truth(seg, Some("linked"), "partial", Some(0.5))
                .unwrap();
            seg
        };
        let moved = label_from_truth(&store, usize::MAX, true, 1).unwrap();
        assert!(moved.iter().all(|m| m.segment_id != seg));
        assert_eq!(label_of(&store, seg), (None, None));
    }

    // ---- 0.12.1: the re-verdict pass -------------------------------------

    const SEC: i64 = 1_000_000_000;

    /// An install shaped like the one §34 measured: a Discord app session and
    /// a microphone session, the user's account linked to the pinned "You"
    /// voice, and a friend linked to another.
    fn a_store_to_rejudge() -> (Arc<std::sync::Mutex<Store>>, i64, i64) {
        use crate::store::truth_via;
        let s = Store::open_in_memory().unwrap();
        let you = s.ensure_you_speaker(0).unwrap();
        let aspen = s.mint_speaker(0).unwrap();
        s.upsert_discord_user("me", "nerdrx", 0).unwrap();
        s.set_discord_link("me", Some(you), Some(truth_via::MANUAL), 0)
            .unwrap();
        s.upsert_discord_user("aspen", "Aspen", 0).unwrap();
        s.set_discord_link("aspen", Some(aspen), Some(truth_via::MANUAL), 0)
            .unwrap();
        let app = s.upsert_source("vesktop", "Vesktop", 1).unwrap();
        let app_sess = s.begin_session(app, 0).unwrap();
        let mic = s
            .upsert_source_kind("mic", "Microphone", crate::store::KIND_MIC, 1)
            .unwrap();
        let mic_sess = s.begin_session(mic, 0).unwrap();
        (Arc::new(std::sync::Mutex::new(s)), app_sess, mic_sess)
    }

    /// A turn with its spans and the verdict the OLD rule wrote for it.
    fn old_verdict_turn(
        store: &Arc<std::sync::Mutex<Store>>,
        session: i64,
        at_s: i64,
        rings: &[(&str, i64, i64)],
    ) -> i64 {
        let g = store.lock().unwrap();
        let (a, b) = (at_s * SEC, at_s * SEC + 2 * SEC);
        let seg = g.insert_segment(session, a, b, "t.wav", 0).unwrap();
        let mut spans = Vec::new();
        for (u, from_ms, to_ms) in rings {
            g.truth_speaking_start(
                u,
                u,
                None,
                a + from_ms * 1_000_000,
                &crate::bridge::ClientRef::default(),
            )
            .unwrap();
            g.truth_speaking_stop(
                u,
                a + to_ms * 1_000_000,
                &crate::bridge::ClientRef::default(),
            )
            .unwrap();
            spans.push(TruthSpan {
                user_id: (*u).to_string(),
                name: (*u).to_string(),
                t_start_ns: a + from_ms * 1_000_000,
                t_end_ns: a + to_ms * 1_000_000,
                account_id: None,
                client_kind: None,
            });
        }
        // Exactly what the pass would have written before the rule existed.
        let cov = coverage(&spans, a, b);
        let v = verdict(&cov, true, Audible::everyone());
        g.set_segment_truth(seg, v.user_id(), v.as_str(), v.coverage())
            .unwrap();
        g.set_segment_truth_overlap(seg, simultaneous_frac(&spans, a, b, Audible::everyone()))
            .unwrap();
        seg
    }

    fn verdict_of(store: &Arc<std::sync::Mutex<Store>>, seg: i64) -> (String, Option<String>) {
        let g = store.lock().unwrap();
        let t = g.segment_truth(seg).unwrap().unwrap();
        (t.verdict.unwrap(), t.user_id)
    }

    #[test]
    fn the_re_verdict_takes_the_phantom_overlaps_back() {
        let (store, app, mic) = a_store_to_rejudge();
        // The phantom: Aspen across the turn, the user talking over her, on
        // Discord's own output.
        let phantom = old_verdict_turn(&store, app, 10, &[("aspen", 0, 1800), ("me", 400, 1400)]);
        // The real thing: two other people. Nothing to correct.
        let real = old_verdict_turn(&store, app, 20, &[("aspen", 0, 1800), ("rowan", 400, 1400)]);
        // The user alone on application audio: a `single` about a voice the
        // recording cannot hold.
        let ghost = old_verdict_turn(&store, app, 30, &[("me", 0, 1900)]);
        // The same picture on the microphone, where it is simply true.
        let real_mic = old_verdict_turn(&store, mic, 40, &[("me", 0, 1900)]);
        assert_eq!(verdict_of(&store, phantom).0, truth_verdict::OVERLAP);
        assert_eq!(verdict_of(&store, ghost).0, truth_verdict::SINGLE);

        // ---- the preview writes nothing ----
        let preview = rejudge(&store, usize::MAX, false, 1, None).unwrap();
        assert_eq!(preview.examined, 4);
        assert_eq!(preview.changed.len(), 2);
        assert_eq!(preview.overlap_reassigned(), 1);
        assert_eq!(verdict_of(&store, phantom).0, truth_verdict::OVERLAP);

        // ---- and the apply writes exactly what it previewed ----
        let done = rejudge(&store, usize::MAX, true, 1, None).unwrap();
        assert_eq!(done.changed, preview.changed, "preview is the same pass");
        assert_eq!(
            verdict_of(&store, phantom),
            (truth_verdict::SINGLE.to_string(), Some("aspen".to_string())),
            "one voice, one label"
        );
        assert_eq!(verdict_of(&store, real).0, truth_verdict::OVERLAP);
        assert_eq!(verdict_of(&store, ghost).0, truth_verdict::NOBODY);
        assert_eq!(
            verdict_of(&store, real_mic),
            (truth_verdict::SINGLE.to_string(), Some("me".to_string())),
            "the microphone is the one place the user's own account IS the answer"
        );

        // The simultaneous share is re-measured on the same rule: the phantom
        // holds one mouth, the real overlap still holds two.
        let g = store.lock().unwrap();
        assert_eq!(g.segment_truth_overlap(phantom).unwrap(), Some(0.0));
        assert!(g.segment_truth_overlap(real).unwrap().unwrap() > 0.0);
    }

    #[test]
    fn a_second_re_verdict_has_nothing_left_to_do() {
        let (store, app, _) = a_store_to_rejudge();
        old_verdict_turn(&store, app, 10, &[("aspen", 0, 1800), ("me", 400, 1400)]);
        assert_eq!(
            rejudge(&store, usize::MAX, true, 1, None)
                .unwrap()
                .changed
                .len(),
            1
        );
        let again = rejudge(&store, usize::MAX, true, 2, None).unwrap();
        assert!(again.changed.is_empty(), "idempotent, like every pass here");
        assert_eq!(again.restamped, 0);
    }

    #[test]
    fn the_re_verdict_is_written_down_and_readable_back() {
        let (store, app, _) = a_store_to_rejudge();
        let seg = old_verdict_turn(&store, app, 10, &[("aspen", 0, 1800), ("me", 400, 1400)]);
        let r = rejudge(&store, usize::MAX, true, 77, None).unwrap();
        let g = store.lock().unwrap();
        let ops = g.operations_of(OP_REJUDGE, 10).unwrap();
        assert_eq!(ops.len(), 1);
        assert!(ops[0].target_ids.contains(&seg.to_string()));
        assert!(ops[0].prior_state.contains(truth_verdict::OVERLAP));
        // And the count `truth report` prints survives the pass that made it.
        let stored: Value =
            serde_json::from_str(&g.setting(REJUDGE_KEY).unwrap().unwrap()).unwrap();
        assert_eq!(stored["overlap_reassigned"], json!(1));
        assert_eq!(stored["changed"], json!(r.changed.len()));
    }

    #[test]
    fn a_verdict_whose_spans_are_gone_is_re_derived_only_where_that_is_honest() {
        let (store, app, _) = a_store_to_rejudge();
        let (mine, hers, over) = {
            let g = store.lock().unwrap();
            let mk = |at_s: i64, v: &str, user: Option<&str>, cov: Option<f64>| {
                let a = at_s * SEC;
                let seg = g.insert_segment(app, a, a + 2 * SEC, "t.wav", 0).unwrap();
                g.set_segment_truth(seg, user, v, cov).unwrap();
                seg
            };
            (
                // A `single` about the user, with no span left anywhere.
                mk(10, truth_verdict::SINGLE, Some("me"), Some(0.95)),
                // A `single` about somebody else: nothing the rule can move.
                mk(20, truth_verdict::SINGLE, Some("aspen"), Some(0.95)),
                // An `overlap`, which never wrote down WHO.
                mk(30, truth_verdict::OVERLAP, None, None),
            )
        };
        let r = rejudge(&store, usize::MAX, true, 1, None).unwrap();
        assert_eq!(r.from_columns, 1);
        assert_eq!(r.unresolvable, 1, "the one row that cannot be re-judged");
        assert_eq!(
            verdict_of(&store, mine).0,
            truth_verdict::NOBODY,
            "the verdict itself is the evidence: nobody else reached the bar"
        );
        assert_eq!(verdict_of(&store, hers).0, truth_verdict::SINGLE);
        assert_eq!(
            verdict_of(&store, over).0,
            truth_verdict::OVERLAP,
            "guessing here would invent the thing the pass exists to correct"
        );
    }

    #[test]
    fn with_no_account_linked_to_you_the_pass_refuses_to_touch_anything() {
        let s = Store::open_in_memory().unwrap();
        let app = s.upsert_source("vesktop", "Vesktop", 1).unwrap();
        let sess = s.begin_session(app, 0).unwrap();
        let store = Arc::new(std::sync::Mutex::new(s));
        let seg = old_verdict_turn(&store, sess, 10, &[("aspen", 0, 1800), ("me", 400, 1400)]);
        let r = rejudge(&store, usize::MAX, true, 1, None).unwrap();
        assert_eq!(r.examined, 0, "no subject, no rule, no walk of the table");
        assert_eq!(verdict_of(&store, seg).0, truth_verdict::OVERLAP);
    }

    #[test]
    fn the_automatic_re_verdict_runs_once_and_only_once() {
        let (store, app, _) = a_store_to_rejudge();
        old_verdict_turn(&store, app, 10, &[("aspen", 0, 1800), ("me", 400, 1400)]);
        rejudge_once(&store, None).unwrap();
        assert_eq!(verdict_of(&store, 1).0, truth_verdict::SINGLE);
        // A second turn arrives with an old-rule verdict — a row the live
        // pass would never write now, so the only way it exists is somebody
        // putting it there. The automatic pass is done and does not re-run;
        // `recalld truth rejudge` is the route.
        let late = old_verdict_turn(&store, app, 20, &[("aspen", 0, 1800), ("me", 400, 1400)]);
        rejudge_once(&store, None).unwrap();
        assert_eq!(verdict_of(&store, late).0, truth_verdict::OVERLAP);
        assert_eq!(
            rejudge(&store, usize::MAX, true, 1, None)
                .unwrap()
                .changed
                .len(),
            1
        );
    }

    // ---- 0.12.3: two bridges ----------------------------------------------

    /// The second own account, which is the case the user actually has: two
    /// Discord accounts, both theirs, in two clients. Both are linked to the
    /// pinned "You" voice, and `Audible` must silence both on application
    /// audio and neither on a microphone.
    #[test]
    fn every_account_linked_to_you_is_silent_on_app_audio() {
        let own = vec!["me-main".to_string(), "me-alt".to_string()];
        let app = Audible::new(crate::store::KIND_APP, &own);
        assert!(!app.hears("me-main"));
        assert!(!app.hears("me-alt"), "an alt is as inaudible as the main");
        assert!(app.hears("aspen"));
        assert_eq!(app.silent().len(), 2);

        // A microphone hears the room, and in the room the user is the one
        // voice that certainly IS there.
        let mic = Audible::new(crate::store::KIND_MIC, &own);
        assert!(mic.hears("me-main"));
        assert!(mic.hears("me-alt"));
        assert!(mic.silent().is_empty());
    }

    /// The stronger rule, and the one that needs no link at all: whatever the
    /// user has told us, the bridge's own account is the local user of the very
    /// client whose output this recording is, and a client never plays your
    /// microphone back to you.
    #[test]
    fn the_bridges_own_account_is_silent_on_its_own_clients_audio() {
        let none: Vec<String> = Vec::new();
        let app = Audible::new(crate::store::KIND_APP, &none).with_bridge_account(Some("me-alt"));
        assert!(!app.hears("me-alt"));
        assert!(app.hears("aspen"));
        assert_eq!(app.silent(), vec!["me-alt".to_string()]);

        // Still not on a microphone.
        let mic = Audible::new(crate::store::KIND_MIC, &none).with_bridge_account(Some("me-alt"));
        assert!(mic.hears("me-alt"));

        // And a turn where only the bridge's own account was ringing is
        // `nobody`, which is what `nobody` has always meant.
        let cov = coverage(&[span("me-alt", 0, 2_000)], 0, 2_000 * 1_000_000);
        assert_eq!(verdict(&cov, true, app), Verdict::Nobody);
    }

    /// Two calls at once, one segment. Unscoped, the two bridges' spans read
    /// as two people talking across the turn and the verdict is `overlap`;
    /// scoped to the bridge whose client made the recording, it is the `single`
    /// it always was.
    #[test]
    fn scoping_the_spans_is_the_difference_between_single_and_overlap() {
        let seg = (0i64, 2_000i64 * 1_000_000);
        let mixed = [
            span_from("aspen", 0, 1_900, Some("acct-vesktop")),
            span_from("someone-else", 100, 1_800, Some("acct-discord")),
        ];
        // What 0.12.2 would have computed: two calls braided into one verdict.
        let cov = coverage(&mixed, seg.0, seg.1);
        assert_eq!(verdict(&cov, true, Audible::everyone()), Verdict::Overlap);

        // What the daemon computes once the spans are scoped — and the scope
        // is what the query does, so the test does it the same way.
        let mine: Vec<TruthSpan> = mixed
            .iter()
            .filter(|s| s.account_id.as_deref() == Some("acct-vesktop"))
            .cloned()
            .collect();
        let cov = coverage(&mine, seg.0, seg.1);
        assert!(matches!(
            verdict(&cov, true, Audible::everyone()),
            Verdict::Single { ref user_id, .. } if user_id == "aspen"
        ));
        // …and the simultaneous fraction stops claiming a collision that was
        // never in this recording.
        assert!(simultaneous_frac(&mine, seg.0, seg.1, Audible::everyone()) < 1e-9);
        assert!(simultaneous_frac(&mixed, seg.0, seg.1, Audible::everyone()) > 0.5);
    }

    /// `scope_of` with no picker and no bridges is `Every`, which is what
    /// keeps `recalld truth rejudge` on an archive doing exactly what 0.12.1
    /// measured.
    #[test]
    fn an_archive_with_no_scoped_span_is_judged_exactly_as_before() {
        assert_eq!(scope_of("vesktop", &[], None), crate::bridge::Scope::Every);
        assert_eq!(scope_account(&crate::bridge::Scope::Every), None);
        assert_eq!(scope_account(&crate::bridge::Scope::Legacy), None);
        assert_eq!(
            scope_account(&crate::bridge::Scope::Account("a1".into())),
            Some("a1")
        );
    }
}
