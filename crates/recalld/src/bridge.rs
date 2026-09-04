//! Which Discord client is the bridge's (0.12.2).
//!
//! ## The bug this deletes
//!
//! 0.12.1 wrote the de-duplication rule against a **source key**: while any
//! per-user stream is live, every session whose source matches
//! `[truth].sources` is muted for analysis. On an install with one Discord
//! client that is exactly right. On an install with two it is a silence:
//!
//! * Vesktop carries the RecallBridge plugin and is sending per-user audio.
//! * A second client — the official Discord, a second Vesktop, the web app in a
//!   browser — is in a **different call**, with different people, and has no
//!   plugin and therefore no per-user streams.
//! * The rule mutes both, because both match `[truth].sources`. The second
//!   call goes unrecorded for as long as the first one lasts.
//!
//! The mute has to be aimed at one **instance**, and something has to decide
//! which. That is this module.
//!
//! ## What identifies an instance
//!
//! A live one: the **session**. `sessions` is already per-PipeWire-node — two
//! copies of an application already open two concurrent session rows against
//! the one source row — and v14 put `instance_key` (`serial:<object.serial>`,
//! or `pid:<pid>`) on it. So "mute this instance" is "mute this session", and
//! the session id is the key this module works in.
//!
//! A *relaunched* one: nothing. `object.serial` is per-boot and a pid is per
//! launch, so an override keyed on either would expire the next time the user
//! restarted Discord — which is precisely when they would want it to still be
//! there. The override is therefore keyed on the **source match key**, which is
//! the thing that survives a relaunch, and on this machine the two clients
//! already differ there (FINDINGS §37): Vesktop's playback node is
//! `application.process.binary = vesktop`, `node.name = vesktop`; the official
//! client's is `application.process.binary = Discord`, `node.name =
//! application.name = WEBRTC VoiceEngine`. Two source rows, `vesktop` and
//! `Discord`. When the two clients are two copies of the *same* binary they
//! share one source row and the override cannot separate them — that case is
//! what the automatic rule is for, and the override then speaks about both.
//!
//! ## The automatic rule
//!
//! While per-user streams are live, the bridge's client is the mixed instance
//! **whose speech is explained by the streams**. Both taps carry the same call,
//! so the plugin's client is loud exactly when some per-user stream is loud;
//! the other client's call is loud on its own schedule and the streams know
//! nothing about it.
//!
//! Measured as a share over a rolling window: quantise time into 100 ms
//! buckets, mark a bucket for an instance when that instance's audio was above
//! the activity floor in it, mark a bucket globally when *some* per-user stream
//! was, and take
//!
//! ```text
//! share = |instance buckets that are within ±SKEW of a stream bucket| / |instance buckets|
//! ```
//!
//! The ±`SKEW` dilation covers the offset between the two paths — the stream is
//! pre-mix out of the client, the mixed tap is post-mix through the sink — and
//! it is [`SKEW_BUCKETS`] rather than something more generous for a measured
//! reason, which that constant carries.
//!
//! The bridge's client is the instance with the highest share, if it clears
//! [`SHARE_BAR`] *and* is [`SHARE_MARGIN`] clear of the next candidate. Every
//! other instance keeps recording. When nothing clears both, **nothing is
//! muted** — two calls that are both busy score alike, and a guess there costs
//! somebody a whole call.
//!
//! ## Until it knows, it mutes nothing
//!
//! A verdict needs [`MIN_ACTIVE_MS`] of *active* audio from an instance inside
//! the window. Until then no instance is muted, which means the first seconds
//! of a call can be recorded twice.
//!
//! **That trade is deliberate and it is not symmetric.** A duplicate is two
//! transcripts of one sentence: ugly, findable, deletable, and it costs the
//! user nothing they cannot undo. A wrong mute is *audio that was never
//! recorded* — the other call's first twenty seconds, gone, with no row to say
//! it happened. So the rule waits for evidence, and while it is waiting it errs
//! towards recording.
//!
//! The same asymmetry is why a **new instance is never muted by inheritance**.
//! An instance that appears mid-call starts with an empty window; it is a
//! candidate like any other and it has to earn the verdict on its own buckets.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Mutex;

use serde_json::{Value, json};

use crate::config::SAMPLE_RATE;

/// How wide one bucket of the timeline is. 100 ms is a syllable: fine enough
/// that a share means something, coarse enough that a 25 s window is 250 bits
/// per instance and the whole structure fits in a cache line's worth of sets.
pub const BUCKET_MS: u64 = 100;

/// How far back the rolling window reaches. Long enough to survive one person
/// finishing a sentence, short enough that the answer follows a call which
/// changed under it (somebody left, somebody joined, the plugin restarted).
pub const WINDOW_MS: u64 = 25_000;

/// How far apart an instance bucket and a stream bucket may be and still count
/// as the same speech. ±300 ms.
///
/// **This one was measured and it was wrong the first time** (FINDINGS §37).
/// ±500 ms looks harmless — it is "the buffering, with room to spare" — but
/// every stream span is dilated by a whole second, and against two calls that
/// are both busy the dilation swallows the silences the rule reads. At ±500 ms
/// an unrelated second call scored 0.88–1.00 and there was nowhere to put a
/// threshold. The real skew is tens of milliseconds — `node.latency` is
/// 512/48000 for Vesktop and 360/48000 for the official client, and the frame's
/// `t_ms` is taken in the renderer that decoded it — so ±300 ms is already
/// generous, and a clock further out than that is a broken clock and is
/// `PerUser::wall_to_mono`'s problem.
pub const SKEW_BUCKETS: u64 = 3;

/// How much *active* audio an instance must have contributed to the window
/// before its share is worth believing. Three seconds is a couple of
/// sentences; below it a single cough can read as a share of 1.0 or of 0.0.
pub const MIN_ACTIVE_MS: u64 = 3_000;

/// The share at or above which an instance may be the bridge's.
///
/// Not tight, on purpose: the mixed tap also carries what the streams never
/// will — join and leave chimes, notification pings, a shared screen's audio —
/// and every one of those is speech the streams cannot explain. The tightness
/// lives in [`SHARE_MARGIN`] instead, which only applies when there is
/// something to be tight *about*.
pub const SHARE_BAR: f64 = 0.85;

/// How far the winner must be ahead of every other candidate before one of two
/// live Discord clients is muted.
///
/// The share on its own does not separate them. FINDINGS §37: two independent
/// 20 s calls at 50% talk density score 1.00 (the bridge's own client, which is
/// the same audio) against 0.85 (a call that merely happens to be busy at the
/// same times), and at 70% density the second one reaches 0.98 and the rule
/// cannot tell them apart at all. A margin turns "cannot tell" into "does not
/// answer": below it nothing is muted, both calls are recorded, and the cost is
/// a duplicate rather than a call nobody has a recording of. That is the same
/// asymmetry the evidence bar is built on, and it is where the manual override
/// earns its place.
pub const SHARE_MARGIN: f64 = 0.10;

/// RMS above which 100 ms of audio counts as active. −46 dBFS: comfortably
/// above a quiet line's noise floor and far below speech. This is a *presence*
/// detector and deliberately not the Silero VAD — the question here is "was
/// this instance carrying anything", asked of audio the pipeline may be about
/// to throw away, and paying for a neural frame on discarded audio would be a
/// tax on the muted path.
pub const ACTIVE_RMS: f32 = 0.005;

/// How long a decision is reused before the timelines are looked at again.
/// `is_muted` is asked once per buffer — fifty times a second per instance —
/// and the answer cannot change faster than a bucket.
const DECIDE_EVERY_MS: u64 = 250;

/// An instance is forgotten this long after its last buffer, so a client that
/// quit does not sit in `truth.status` forever.
const FORGET_MS: u64 = WINDOW_MS * 2;

// ---------------------------------------------------------------------------
// the override
// ---------------------------------------------------------------------------

/// What the user has said about a Discord source, if anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Role {
    /// Let the automatic rule decide. The default, and the only value that is
    /// not stored.
    #[default]
    Auto,
    /// This client carries the plugin: mute it whenever per-user streams are
    /// live, evidence or no evidence.
    Bridge,
    /// This client does not carry the plugin: **never** mute it, even when the
    /// automatic rule is sure it is the bridge. The user is allowed to be
    /// right about their own machine, and the failure mode of obeying them is
    /// a duplicate rather than a silence.
    Other,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Auto => "auto",
            Role::Bridge => "bridge",
            Role::Other => "other",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Role::Auto),
            "bridge" => Some(Role::Bridge),
            "other" => Some(Role::Other),
            _ => None,
        }
    }
}

/// The three values `sources.instance_role` takes, for the one sentence a
/// refusal is.
pub const ROLES: [&str; 3] = ["auto", "bridge", "other"];

// ---------------------------------------------------------------------------
// one instance's timeline
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Instance {
    source: String,
    instance_key: Option<String>,
    /// Buckets in which this instance's audio was above the floor.
    active: BTreeSet<u64>,
    last_bucket: u64,
}

/// What the picker decided about one mixed Discord instance.
#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub session_id: i64,
    pub source: String,
    pub instance_key: Option<String>,
    pub role: Role,
    /// `None` until the instance has [`MIN_ACTIVE_MS`] of active audio in the
    /// window. A share of "not enough evidence" is not a share of zero, and
    /// rendering it as one would make a client draw a confident "not the
    /// bridge" over a question nobody has answered yet.
    pub share: Option<f64>,
    pub active_ms: u64,
    pub muted: bool,
    /// One sentence for `truth report` and the Sources card.
    pub why: String,
}

impl Verdict {
    pub fn to_json(&self) -> Value {
        json!({
            "session_id": self.session_id,
            "source": self.source,
            "instance_key": self.instance_key,
            "role": self.role.as_str(),
            "share": self.share,
            "active_ms": self.active_ms,
            "muted": self.muted,
            "why": self.why,
        })
    }
}

// ---------------------------------------------------------------------------
// the picker
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Inner {
    /// Buckets in which SOME per-user stream was carrying speech.
    stream: BTreeSet<u64>,
    mixed: HashMap<i64, Instance>,
    /// Manual overrides, keyed on the source match key because that is the
    /// only thing here that survives a relaunch.
    roles: BTreeMap<String, Role>,
    /// The last decision and the bucket it was taken in.
    decided_at: u64,
    verdicts: Vec<Verdict>,
    /// Whether the last decision was taken with streams live. A decision taken
    /// while nothing was arriving mutes nothing, and must not be reused once
    /// something is.
    decided_live: bool,
    have_decided: bool,
}

/// The rolling-window evidence and the verdict it supports.
///
/// Shared: the pipeline writes to it from the inference thread and the socket
/// reads it for `truth.status`. One mutex, held for microseconds — the work
/// inside it is set insertions and, once a quarter-second, a few hundred
/// integer comparisons.
#[derive(Debug, Default)]
pub struct Picker {
    inner: Mutex<Inner>,
}

impl Picker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the overrides from `[truth] bridge_roles` at start-up. Unknown
    /// values are dropped rather than guessed: a typo in a config file must not
    /// silently become a mute.
    pub fn load_roles(&self, roles: &BTreeMap<String, String>) {
        let mut inner = self.lock();
        inner.roles = roles
            .iter()
            .filter_map(|(k, v)| {
                let role = Role::parse(v)?;
                (role != Role::Auto).then(|| (k.clone(), role))
            })
            .collect();
        inner.have_decided = false;
    }

    /// Set one override live. `Auto` removes it, so the map only ever holds
    /// decisions somebody actually made.
    pub fn set_role(&self, source: &str, role: Role) {
        let mut inner = self.lock();
        if role == Role::Auto {
            inner.roles.remove(source);
        } else {
            inner.roles.insert(source.to_string(), role);
        }
        // The next `is_muted` re-decides rather than serving the cache: a role
        // the user just set has to act on the next buffer, not a quarter of a
        // second later.
        inner.have_decided = false;
    }

    pub fn role(&self, source: &str) -> Role {
        self.lock().roles.get(source).copied().unwrap_or_default()
    }

    /// Every override that is set, for the wire and for the config file.
    pub fn roles(&self) -> BTreeMap<String, String> {
        self.lock()
            .roles
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().to_string()))
            .collect()
    }

    /// A buffer of one mixed Discord instance's audio.
    ///
    /// `mono_ns` is the capture instant of its first sample — the same clock
    /// the per-user frames are placed on, which is the whole reason the two
    /// timelines can be compared at all.
    pub fn observe_mixed(
        &self,
        session_id: i64,
        source: &str,
        instance_key: Option<&str>,
        mono_ns: u64,
        samples: &[f32],
    ) {
        let buckets = active_buckets(mono_ns, samples);
        let now = bucket_of(mono_ns);
        let mut inner = self.lock();
        // An instance nobody has seen before invalidates the cached decision
        // rather than waiting out its quarter-second: the arrival of a
        // candidate is exactly the moment the answer can change.
        if !inner.mixed.contains_key(&session_id) {
            inner.have_decided = false;
        }
        let entry = inner.mixed.entry(session_id).or_default();
        if entry.source.is_empty() {
            entry.source = source.to_string();
        }
        if entry.instance_key.is_none() {
            entry.instance_key = instance_key.map(str::to_string);
        }
        entry.last_bucket = entry.last_bucket.max(now);
        entry.active.extend(buckets);
        inner.prune(now);
    }

    /// A buffer of one per-user stream's audio.
    pub fn observe_stream(&self, mono_ns: u64, samples: &[f32]) {
        let buckets = active_buckets(mono_ns, samples);
        let now = bucket_of(mono_ns);
        let mut inner = self.lock();
        inner.stream.extend(buckets);
        inner.prune(now);
    }

    /// Forget a session. Called when it ends, so a reused row id cannot
    /// inherit a verdict.
    pub fn forget(&self, session_id: i64) {
        let mut inner = self.lock();
        if inner.mixed.remove(&session_id).is_some() {
            inner.have_decided = false;
        }
    }

    /// Should this mixed instance's audio be discarded?
    ///
    /// `streams_live` is the caller's answer to "is any per-user stream
    /// arriving right now" — [`crate::peruser::PerUser::any_live`]. It is
    /// passed in rather than read here so this module has no opinion about
    /// where audio comes from and can be tested on timelines alone.
    pub fn is_muted(&self, session_id: i64, now_mono_ns: u64, streams_live: bool) -> bool {
        let mut inner = self.lock();
        inner.decide(bucket_of(now_mono_ns), streams_live);
        inner
            .verdicts
            .iter()
            .any(|v| v.session_id == session_id && v.muted)
    }

    /// Every instance and what was decided about it, for `truth.status`.
    pub fn verdicts(&self, now_mono_ns: u64, streams_live: bool) -> Vec<Verdict> {
        let mut inner = self.lock();
        inner.decide(bucket_of(now_mono_ns), streams_live);
        inner.verdicts.clone()
    }

    /// `truth.status.audio.mute`.
    pub fn status(&self, now_mono_ns: u64, streams_live: bool) -> Value {
        let verdicts = self.verdicts(now_mono_ns, streams_live);
        let muted: Vec<i64> = verdicts
            .iter()
            .filter(|v| v.muted)
            .map(|v| v.session_id)
            .collect();
        json!({
            "share_bar": SHARE_BAR,
            "share_margin": SHARE_MARGIN,
            "window_s": WINDOW_MS as f64 / 1_000.0,
            "min_active_s": MIN_ACTIVE_MS as f64 / 1_000.0,
            "streams_live": streams_live,
            "muted_sessions": muted,
            "instances": verdicts.iter().map(Verdict::to_json).collect::<Vec<_>>(),
            "roles": self.roles(),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl Inner {
    fn prune(&mut self, now: u64) {
        let cutoff = now.saturating_sub(WINDOW_MS / BUCKET_MS);
        // `split_off` keeps the tail, which is the half we want.
        self.stream = self.stream.split_off(&cutoff);
        let forget = now.saturating_sub(FORGET_MS / BUCKET_MS);
        self.mixed.retain(|_, i| {
            i.active = i.active.split_off(&cutoff);
            i.last_bucket >= forget
        });
    }

    /// The whole rule, in one place.
    fn decide(&mut self, now: u64, streams_live: bool) {
        if self.have_decided
            && self.decided_live == streams_live
            && now.saturating_sub(self.decided_at) < DECIDE_EVERY_MS / BUCKET_MS
        {
            return;
        }
        self.decided_at = now;
        self.decided_live = streams_live;
        self.have_decided = true;
        self.prune(now);

        let stream = std::mem::take(&mut self.stream);
        let mut rows: Vec<Verdict> = Vec::with_capacity(self.mixed.len());
        for (session_id, inst) in &self.mixed {
            let role = self.roles.get(&inst.source).copied().unwrap_or(Role::Auto);
            let active_ms = inst.active.len() as u64 * BUCKET_MS;
            let share = (active_ms >= MIN_ACTIVE_MS).then(|| {
                let hits = inst
                    .active
                    .iter()
                    .filter(|b| {
                        stream
                            .range(b.saturating_sub(SKEW_BUCKETS)..=(*b + SKEW_BUCKETS))
                            .next()
                            .is_some()
                    })
                    .count();
                hits as f64 / inst.active.len().max(1) as f64
            });
            rows.push(Verdict {
                session_id: *session_id,
                source: inst.source.clone(),
                instance_key: inst.instance_key.clone(),
                role,
                share,
                active_ms,
                muted: false,
                why: String::new(),
            });
        }
        self.stream = stream;

        // The automatic winner: the highest share at or above the bar, among
        // instances the user has not already spoken about — and, when there is
        // more than one candidate, only if it is [`SHARE_MARGIN`] clear of the
        // next one. Two calls that are both busy score alike, and the honest
        // answer there is no answer (§37). Ties break on the lower session id
        // so a status that is polled does not flicker.
        rows.sort_by_key(|v| v.session_id);
        let mut ranked: Vec<(f64, i64)> = rows
            .iter()
            .filter(|v| v.role == Role::Auto)
            .filter_map(|v| v.share.map(|s| (s, v.session_id)))
            .collect();
        ranked.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.1.cmp(&b.1))
        });
        let mut too_close = false;
        // There is one plugin, so there is one bridge client. If the user has
        // named it, the measurement does not get to name a second one — an
        // override that left another instance muted "as well" would answer the
        // user's question with half a yes.
        let named = rows.iter().any(|v| v.role == Role::Bridge);
        let winner = match ranked.as_slice() {
            _ if named => None,
            [(share, id), rest @ ..] if *share >= SHARE_BAR => {
                if rest
                    .first()
                    .is_some_and(|(next, _)| share - next < SHARE_MARGIN)
                {
                    too_close = true;
                    None
                } else {
                    Some(*id)
                }
            }
            _ => None,
        };

        for v in &mut rows {
            let pct = |s: f64| format!("{:.0}%", s * 100.0);
            match v.role {
                Role::Other => {
                    v.muted = false;
                    v.why =
                        "you set this client to “not the bridge”, so it is never muted".to_string();
                }
                Role::Bridge => {
                    v.muted = streams_live;
                    v.why = if streams_live {
                        "you set this client to “has the plugin”, and streams are live".to_string()
                    } else {
                        "you set this client to “has the plugin”; nothing is arriving, so it \
                         records as normal"
                            .to_string()
                    };
                }
                Role::Auto => {
                    v.muted = streams_live && winner == Some(v.session_id);
                    v.why = match (streams_live, v.share) {
                        (false, _) => {
                            "no per-user stream is arriving; nothing is muted".to_string()
                        }
                        (true, None) => format!(
                            "not enough evidence yet ({:.1} s of speech, {:.1} s needed) — \
                             recording, because a duplicate is cheaper than a lost call",
                            v.active_ms as f64 / 1_000.0,
                            MIN_ACTIVE_MS as f64 / 1_000.0,
                        ),
                        (true, Some(s)) if v.muted => format!(
                            "{} of this client's speech is explained by the per-user streams, \
                             so it is the bridge's client and is muted",
                            pct(s)
                        ),
                        (true, Some(s)) if named => format!(
                            "{} matches the streams, but you have already said which client \
                             has the plugin — recording",
                            pct(s)
                        ),
                        (true, Some(s)) if too_close => format!(
                            "{} matches the streams, and so does another client — too close to \
                             call, so both keep recording; say which one has the plugin to \
                             settle it",
                            pct(s)
                        ),
                        (true, Some(s)) if s >= SHARE_BAR => format!(
                            "{} matches the streams, but another client matches more — recording",
                            pct(s)
                        ),
                        (true, Some(s)) => format!(
                            "only {} of this client's speech is explained by the per-user \
                             streams, so it is a different call — recording",
                            pct(s)
                        ),
                    };
                }
            }
        }
        self.verdicts = rows;
    }
}

/// Which 100 ms buckets of this buffer were above the activity floor.
///
/// Sub-divided rather than judged whole: a per-user frame is 500 ms and a
/// mixed buffer is 20, and marking five buckets because one of them had a word
/// in it would inflate every share by the length of the longest buffer.
fn active_buckets(mono_ns: u64, samples: &[f32]) -> Vec<u64> {
    let per = (SAMPLE_RATE as u64 * BUCKET_MS / 1_000) as usize;
    if per == 0 || samples.is_empty() {
        return Vec::new();
    }
    let start_ms = mono_ns / 1_000_000;
    let mut out = Vec::new();
    for (i, win) in samples.chunks(per).enumerate() {
        if rms(win) < ACTIVE_RMS {
            continue;
        }
        out.push((start_ms + i as u64 * BUCKET_MS) / BUCKET_MS);
    }
    out
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|s| f64::from(*s) * f64::from(*s)).sum();
    (sum / samples.len() as f64).sqrt() as f32
}

fn bucket_of(mono_ns: u64) -> u64 {
    mono_ns / 1_000_000 / BUCKET_MS
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    fn loud() -> Vec<f32> {
        vec![0.2; (SAMPLE_RATE as u64 * BUCKET_MS / 1_000) as usize]
    }
    fn quiet() -> Vec<f32> {
        vec![0.0001; (SAMPLE_RATE as u64 * BUCKET_MS / 1_000) as usize]
    }

    /// Play a synthetic timeline: each span is `(start_ms, duration_ms)` of
    /// somebody talking. Silence between spans is not played at all, because
    /// silence contributes to neither side of the share — which is itself why
    /// the rule is measured on VAD-active time rather than on wall time.
    ///
    /// `session` is `None` for the per-user streams' combined timeline.
    fn speak(p: &Picker, session: Option<(i64, &str)>, spans: &[(u64, u64)]) {
        for (start, dur) in spans {
            let mut t = *start;
            while t < start + dur {
                match session {
                    Some((id, src)) => p.observe_mixed(id, src, Some("serial:1"), t * MS, &loud()),
                    None => p.observe_stream(t * MS, &loud()),
                }
                t += BUCKET_MS;
            }
        }
    }

    fn share_of(p: &Picker, session: i64, now_ms: u64) -> Option<f64> {
        p.verdicts(now_ms * MS, true)
            .into_iter()
            .find(|v| v.session_id == session)
            .and_then(|v| v.share)
    }

    /// The timeline the rule exists for: the plugin's client says the same
    /// thing the streams say, and a second client is in a different call.
    ///
    /// The mixed tap is 200 ms behind the streams throughout — the client's
    /// mixer plus the sink's buffer.
    const STREAM_SPANS: [(u64, u64); 5] = [
        (10_000, 1_500),
        (13_000, 2_000),
        (17_000, 1_200),
        (20_000, 2_500),
        (25_000, 1_500),
    ];
    const BRIDGE_SPANS: [(u64, u64); 5] = [
        (10_200, 1_500),
        (13_200, 2_000),
        (17_200, 1_200),
        (20_200, 2_500),
        (25_200, 1_500),
    ];
    /// The other call: mostly in the streams' silences, with one 600 ms
    /// coincidence, because two calls do sometimes talk at once.
    const OTHER_SPANS: [(u64, u64); 6] = [
        (12_050, 400),
        (15_600, 800),
        (18_800, 600),
        (23_100, 1_300),
        (24_800, 600),
        (27_200, 2_000),
    ];

    #[test]
    fn the_bridges_own_client_is_explained_by_the_streams_and_the_other_call_is_not() {
        let p = Picker::new();
        speak(&p, None, &STREAM_SPANS);
        speak(&p, Some((1, "vesktop")), &BRIDGE_SPANS);
        speak(&p, Some((2, "Discord")), &OTHER_SPANS);

        let bridge = share_of(&p, 1, 30_000).expect("8.7 s is well past the evidence bar");
        let other = share_of(&p, 2, 30_000).expect("5.7 s is well past the evidence bar");
        assert!(
            bridge >= 0.99,
            "the bridge's own client is the same audio: {bridge}"
        );
        assert!(
            other <= 0.2,
            "an unrelated call is explained by nothing: {other}"
        );
        assert!(
            other < SHARE_BAR && bridge >= SHARE_BAR,
            "0.8 sits in the empty middle"
        );

        let v = p.verdicts(30_000 * MS, true);
        let muted: Vec<i64> = v.iter().filter(|v| v.muted).map(|v| v.session_id).collect();
        assert_eq!(muted, vec![1], "only the bridge's client is muted");
        assert!(v[1].why.contains("different call"));
    }

    #[test]
    fn the_skew_between_the_two_taps_does_not_cost_the_verdict() {
        // The mixed tap is behind the streams by up to ±SKEW_BUCKETS, which is
        // an order of magnitude more than the two clients' `node.latency` and
        // the frame's own timestamp can actually put between them.
        let spans = [(20_000u64, 2_000u64), (23_000, 2_000), (27_000, 2_000)];
        for lag in [0u64, 100, 200, SKEW_BUCKETS * BUCKET_MS] {
            let p = Picker::new();
            speak(&p, None, &spans);
            let lagged: Vec<(u64, u64)> = spans.iter().map(|(s, d)| (s + lag, *d)).collect();
            speak(&p, Some((1, "vesktop")), &lagged);
            let s = share_of(&p, 1, 32_000).expect("6 s of speech");
            assert!(
                (s - 1.0).abs() < 1e-9,
                "a {lag} ms lag should cost nothing at all, got {s}"
            );
        }
    }

    #[test]
    fn nothing_is_muted_until_the_window_has_evidence() {
        let p = Picker::new();
        // 1 s of perfectly-explained speech. The share would be 1.0, and it is
        // still not enough to take a call's audio away.
        speak(&p, None, &[(5_000, 1_000)]);
        speak(&p, Some((1, "vesktop")), &[(5_000, 1_000)]);
        let v = p.verdicts(6_500 * MS, true);
        assert_eq!(v.len(), 1);
        assert_eq!(
            v[0].share, None,
            "a share under the evidence bar is not a share of zero"
        );
        assert!(
            !v[0].muted,
            "recording a duplicate for a few seconds beats losing the other call"
        );
        assert!(v[0].why.contains("not enough evidence"));

        // Past the bar, the same timeline mutes.
        speak(&p, None, &[(6_000, 2_600)]);
        speak(&p, Some((1, "vesktop")), &[(6_000, 2_600)]);
        let v = p.verdicts(9_500 * MS, true);
        assert_eq!(v[0].active_ms, 3_600);
        assert!(v[0].muted, "3 s of explained speech is the bar");
    }

    #[test]
    fn a_client_that_appears_mid_call_is_not_muted_by_inheritance() {
        let p = Picker::new();
        let spans = [
            (30_000u64, 1_500u64),
            (34_000, 1_500),
            (38_000, 1_500),
            (42_000, 1_500),
        ];
        speak(&p, None, &spans);
        speak(&p, Some((1, "vesktop")), &spans);
        assert!(
            p.is_muted(1, 46_000 * MS, true),
            "the bridge's client is muted"
        );

        // The user launches a SECOND COPY of the same client — same source row,
        // same match key, a different PipeWire node — and joins another call.
        speak(
            &p,
            Some((2, "vesktop")),
            &[(36_100, 1_300), (40_100, 1_300), (44_100, 1_300)],
        );
        let v = p.verdicts(46_000 * MS, true);
        let new = v
            .iter()
            .find(|v| v.session_id == 2)
            .expect("the new instance");
        assert_eq!(new.active_ms, 3_900, "it has evidence of its own");
        assert!(
            !new.muted,
            "a second instance of the SAME source must earn the verdict, never inherit it"
        );
        assert!(v.iter().find(|v| v.session_id == 1).unwrap().muted);
    }

    /// Two clients that both match the streams closely are not a verdict.
    ///
    /// The share alone cannot tell a second call apart from the bridge's own
    /// when both are talking most of the time (§37), and a rule that guessed
    /// there would take a real call off the record on a coin flip. It declines
    /// instead: nothing is muted, both are written down, and the card says to
    /// settle it by hand.
    #[test]
    fn two_clients_that_look_alike_are_not_a_verdict() {
        let p = Picker::new();
        // The streams talk almost continuously; both clients are inside them.
        speak(&p, None, &[(10_000, 12_000)]);
        speak(&p, Some((1, "vesktop")), &[(10_100, 11_800)]);
        speak(&p, Some((2, "Discord")), &[(10_300, 11_000)]);

        let v = p.verdicts(23_000 * MS, true);
        assert_eq!(v.len(), 2);
        assert!(
            v.iter().all(|r| r.share.unwrap_or(0.0) >= SHARE_BAR),
            "both clear the bar: {:?}",
            v.iter().map(|r| r.share).collect::<Vec<_>>()
        );
        assert!(
            v.iter().all(|r| !r.muted),
            "a coin flip here costs somebody a whole call"
        );
        assert!(v[0].why.contains("too close to call"), "{}", v[0].why);

        // And the override is what settles it, which is why it exists.
        p.set_role("vesktop", Role::Bridge);
        assert!(p.is_muted(1, 23_000 * MS, true));
        assert!(!p.is_muted(2, 23_000 * MS, true));
    }

    #[test]
    fn nothing_is_muted_while_no_stream_is_arriving() {
        let p = Picker::new();
        let spans = [(50_000u64, 2_000u64), (53_000, 2_000)];
        speak(&p, None, &spans);
        speak(&p, Some((1, "vesktop")), &spans);
        assert!(p.is_muted(1, 56_000 * MS, true));
        assert!(
            !p.is_muted(1, 56_000 * MS, false),
            "the mute exists to stop a duplicate; with no second copy there is none"
        );
    }

    #[test]
    fn a_manual_other_is_never_muted_even_when_the_rule_is_certain() {
        let p = Picker::new();
        let spans = [(60_000u64, 2_000u64), (63_000, 2_000)];
        speak(&p, None, &spans);
        speak(&p, Some((1, "vesktop")), &spans);
        assert!(p.is_muted(1, 66_000 * MS, true), "the rule is certain");

        p.set_role("vesktop", Role::Other);
        assert!(
            !p.is_muted(1, 66_000 * MS, true),
            "the user is allowed to be right about their own machine"
        );
        let v = p.verdicts(66_000 * MS, true);
        assert_eq!(v[0].role, Role::Other);
        assert!(v[0].why.contains("not the bridge"));
        assert_eq!(v[0].share, Some(1.0), "the rule still says what it thinks");

        // And back: `auto` removes the override rather than storing a third
        // state, so the config file only ever holds decisions somebody made.
        p.set_role("vesktop", Role::Auto);
        assert!(p.roles().is_empty());
        assert!(p.is_muted(1, 66_000 * MS, true));
    }

    #[test]
    fn a_manual_bridge_is_muted_whenever_streams_are_live_with_no_evidence_at_all() {
        let p = Picker::new();
        // Not one bucket of shared speech: the instance is loud, and the
        // streams say nothing whatever about it.
        speak(&p, Some((1, "Discord")), &[(70_000, 4_000)]);
        speak(&p, None, &[(78_000, 1_000)]);
        assert!(
            !p.is_muted(1, 79_500 * MS, true),
            "the automatic rule would never mute this"
        );

        p.set_role("Discord", Role::Bridge);
        assert!(
            p.is_muted(1, 79_500 * MS, true),
            "a manual bridge does not wait for evidence"
        );
        assert!(
            !p.is_muted(1, 79_500 * MS, false),
            "and it is still muted only while something is arriving to duplicate it"
        );
        let v = p.verdicts(79_500 * MS, true);
        assert!(v[0].why.contains("has the plugin"));
    }

    #[test]
    fn the_override_is_keyed_on_the_source_so_it_survives_a_relaunch() {
        let p = Picker::new();
        p.set_role("vesktop", Role::Other);
        // A relaunch: new pid, new `object.serial`, new session row — and the
        // role the user set last week is still theirs.
        speak(&p, None, &[(90_000, 4_000)]);
        speak(&p, Some((77, "vesktop")), &[(90_000, 4_000)]);
        let v = p.verdicts(95_000 * MS, true);
        assert_eq!(v[0].role, Role::Other);
        assert!(!v[0].muted, "the role outlived the instance it was set on");
    }

    #[test]
    fn roles_round_trip_through_the_config_map_and_a_typo_is_dropped() {
        let p = Picker::new();
        p.load_roles(&BTreeMap::from([
            ("vesktop".to_string(), "bridge".to_string()),
            ("Discord".to_string(), "other".to_string()),
            ("Chromium".to_string(), "brdige".to_string()),
            ("steam".to_string(), "auto".to_string()),
        ]));
        let out = p.roles();
        assert_eq!(out.get("vesktop").map(String::as_str), Some("bridge"));
        assert_eq!(out.get("Discord").map(String::as_str), Some("other"));
        assert!(!out.contains_key("Chromium"), "a typo is not a mute");
        assert!(!out.contains_key("steam"), "auto is the absence of a role");
        assert_eq!(p.role("vesktop"), Role::Bridge);
        assert_eq!(p.role("nothing"), Role::Auto);
        assert_eq!(Role::parse("BRIDGE"), Some(Role::Bridge));
        assert_eq!(Role::parse("nonsense"), None);
    }

    #[test]
    fn a_session_that_ended_is_forgotten_and_cannot_be_inherited() {
        let p = Picker::new();
        let spans = [(100_000u64, 2_000u64), (103_000, 2_000)];
        speak(&p, None, &spans);
        speak(&p, Some((5, "vesktop")), &spans);
        assert!(p.is_muted(5, 106_000 * MS, true));
        p.forget(5);
        assert!(
            p.verdicts(106_000 * MS, true).is_empty(),
            "the row id is free to be reused and must carry nothing with it"
        );
    }

    #[test]
    fn the_window_rolls_so_an_old_verdict_cannot_outlive_its_evidence() {
        let p = Picker::new();
        let spans = [(200_000u64, 2_000u64), (203_000, 2_000)];
        speak(&p, None, &spans);
        speak(&p, Some((1, "vesktop")), &spans);
        assert!(p.is_muted(1, 206_000 * MS, true));
        // Half a minute later, with nothing new: the window is empty, the
        // evidence bar is not met, and nothing is muted.
        assert!(!p.is_muted(1, 240_000 * MS, true));
    }

    /// Where [`SHARE_BAR`] came from. Not an assertion — a measurement, run
    /// with `cargo test -p recalld --lib measure_the_share -- --ignored
    /// --nocapture`, whose output is FINDINGS §37's table.
    ///
    /// Two synthetic calls are generated from a seeded LCG at a given talk
    /// density, the bridge's client is the streams' own timeline shifted by a
    /// lag, and the second client is an independent call at the same density.
    /// The question the table answers is whether there is a gap between the
    /// two shares wide enough to put a threshold in.
    #[test]
    #[ignore = "a measurement, not a check; see FINDINGS §37"]
    fn measure_the_share() {
        /// One call as speech spans over `secs`, at `density` talk fraction.
        fn call(seed: u64, base_ms: u64, secs: u64, density: f64) -> Vec<(u64, u64)> {
            let mut s = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let mut next = || {
                s = s
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                (s >> 33) as f64 / (1u64 << 31) as f64
            };
            let mut out = Vec::new();
            let mut t = base_ms;
            let end = base_ms + secs * 1_000;
            while t < end {
                // Turns of 0.6–3.0 s, gaps sized to hit the density.
                let on = 600 + (next() * 2_400.0) as u64;
                let off = ((on as f64) * (1.0 / density - 1.0) * (0.5 + next())) as u64;
                out.push((t, on.min(end.saturating_sub(t))));
                t += on + off.max(200);
            }
            out
        }

        println!("\n density   lag   bridge   other   gap   verdict");
        for density in [0.3f64, 0.5, 0.7] {
            for lag in [0u64, 100, 200, 500] {
                let mut bridge_shares = Vec::new();
                let mut other_shares = Vec::new();
                let (mut right, mut wrong, mut declined) = (0, 0, 0);
                for seed in 0..12u64 {
                    let p = Picker::new();
                    let base = 100_000 + seed * 100_000;
                    let streams = call(seed, base, 20, density);
                    speak(&p, None, &streams);
                    let lagged: Vec<(u64, u64)> =
                        streams.iter().map(|(s, d)| (s + lag, *d)).collect();
                    speak(&p, Some((1, "vesktop")), &lagged);
                    speak(
                        &p,
                        Some((2, "Discord")),
                        &call(seed + 5_000, base, 20, density),
                    );
                    let now = (base + 20_000) * MS;
                    let v = p.verdicts(now, true);
                    for r in &v {
                        let Some(s) = r.share else { continue };
                        if r.session_id == 1 {
                            bridge_shares.push(s);
                        } else {
                            other_shares.push(s);
                        }
                    }
                    match v.iter().find(|r| r.muted).map(|r| r.session_id) {
                        Some(1) => right += 1,
                        Some(_) => wrong += 1,
                        None => declined += 1,
                    }
                }
                let lo = |v: &[f64]| v.iter().cloned().fold(f64::INFINITY, f64::min);
                let hi = |v: &[f64]| v.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
                println!(
                    "   {density:.1}   {lag:>3}   {:.2}     {:.2}    {:+.2}   {right} right, \
                     {wrong} wrong, {declined} declined",
                    lo(&bridge_shares),
                    hi(&other_shares),
                    lo(&bridge_shares) - hi(&other_shares),
                );
            }
        }
    }

    #[test]
    fn silence_is_not_activity_and_one_loud_bucket_marks_one_bucket() {
        assert!(active_buckets(0, &quiet()).is_empty());
        assert_eq!(active_buckets(0, &loud()).len(), 1);
        // A 300 ms buffer with one loud bucket in it marks exactly one, which
        // is what keeps a 500 ms per-user frame from inflating the share by
        // the length of the buffer it happened to arrive in.
        let mut buf = Vec::new();
        buf.extend(quiet());
        buf.extend(loud());
        buf.extend(quiet());
        // 1_000 ms in is bucket 10; the loud 100 ms sits in bucket 11.
        assert_eq!(active_buckets(1_000 * MS, &buf), vec![11]);
    }
}
