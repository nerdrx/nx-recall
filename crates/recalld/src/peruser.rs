//! Per-user Discord audio (0.12.1): one source, one session, one voice, per
//! person in the call.
//!
//! ## The problem this deletes
//!
//! Every Discord turn this daemon has ever stored came off the *mixed* stream:
//! one tap on the client's output, carrying everybody at once. Working out who
//! said what is then a speaker-recognition problem, and when two people talk
//! over each other it is a source-separation problem as well. FINDINGS §33
//! measured the separation route and refused it — SepFormer costs −0.070
//! cosine in artifacts to recover +0.015 of interference, and the toll is
//! charged by the embedder, so a better separator cannot pay it.
//!
//! Its last question was whether the client could just hand over the streams
//! separately, and on Vesktop it can: the web engine gives every remote user
//! their own `MediaStream`. So the answer to overlap is not a model, it is a
//! wire. This module is that wire's end.
//!
//! ## What is different about this audio, and what follows from it
//!
//! It is **single-speaker by construction**. Not "usually", not "when the
//! detector agrees" — the packets were decoded from one person's connection.
//! Three things follow, and each is a rule rather than a heuristic:
//!
//! * **The overlap gate does not apply.** It exists because at equal loudness
//!   an embedding is captured by one of two talkers; there is no second talker
//!   here, and a positive reading is the detector being wrong about reverb.
//!   The reading is still stored — it is information — and it still gates
//!   *enrolment*, because "whose voice this is" and "is this clip worth
//!   keeping" are different questions and only the first one has been settled.
//! * **The identity ladder does not run.** There is nothing to match: the
//!   speaker is the voice linked to that Discord account, or a voice minted for
//!   it on the spot. `label_via = "discord-stream"`, `match_score` NULL — the
//!   same shape a headset turn has, and for the same reason (DESIGN §5).
//! * **The mixed tap must go quiet.** Both sources hear the same call, so
//!   leaving both on writes every sentence twice. See [`PerUser::any_live`] and
//!   the rule in `crate::pipeline`.
//!
//! ## What it does not do
//!
//! It does not name anybody. A Discord nickname is a per-guild string somebody
//! picked for a joke last Tuesday; the source row is called `Discord · <nick>`
//! and the *voice* keeps its `Speaker_NN` until a person names it, exactly as
//! PROTOCOL has said since 0.9.0.
//!
//! It does not keep goldens. The microphone leg does, because a golden is a
//! retention-exempt clip of the user's own voice kept for a future embedding
//! model. Keeping one of somebody else's, forever, outside the retention
//! sweeper, on the strength of them having been in a call — no.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tracing::{debug, info, warn};

use crate::clock::{Anchor, monotonic_ns, utc_now_ns};
use crate::config::{SAMPLE_RATE, TruthConfig};
use crate::queue::{AudioChunk, CaptureEvent, EventQueue};
use crate::resample::LinearResampler;
use crate::store::{KIND_DISCORD_USER, Store, truth_via};

/// `sources.match_key` for one account. The id and not the nickname: a
/// nickname changes per guild and per whim, and the match key is a primary
/// key in everything but name.
pub fn match_key(user_id: &str) -> String {
    format!("discord:{user_id}")
}

/// `sources.display_name`. The middle dot is the same separator the GUI's
/// source cards use elsewhere, and the prefix is what makes a list of fifteen
/// of these readable.
pub fn display_name(name: &str) -> String {
    format!("Discord · {name}")
}

/// Sample rates the ingest will take. Anything else is a refused line rather
/// than a guess: a frame at the wrong rate resamples into speech at the wrong
/// pitch, which transcribes into confident nonsense.
const MIN_RATE: u32 = 8_000;
const MAX_RATE: u32 = 96_000;

/// How far a frame's `t_ms` may sit from the daemon's own clock before it is
/// treated as a broken clock rather than a late frame. Both ends are on the
/// same machine, so a minute is already absurd; the guard exists so a client
/// with a corrupted `Date.now()` cannot stamp a turn in 1970 and strand it
/// outside every window a client will ever ask for.
const MAX_CLOCK_SKEW_NS: i64 = 60 * 1_000_000_000;

// ---------------------------------------------------------------------------
// the wire
// ---------------------------------------------------------------------------

/// One decoded frame from `POST /v1/discord/audio`.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    /// Wall-clock UTC milliseconds of the frame's FIRST sample — the same
    /// `Date.now()` clock the speaking edges are on, and the same clock
    /// `segments.t_start_ns` ends up on. Said out loud here for the third time
    /// because comparing it against a monotonic one would be silently wrong.
    pub t_ms: i64,
    pub user_id: String,
    pub name: String,
    pub channel_id: Option<String>,
    pub rate: u32,
    /// Per-stream, monotonically increasing. Its only job is to make a hole
    /// visible: a frame that is not its predecessor's successor means audio
    /// was lost, and audio that was lost must not be spliced over.
    pub seq: u64,
    pub samples: Vec<f32>,
    /// Which bridge sent it (0.12.3). Two plugins now POST at one daemon, and
    /// a stream is only a duplicate of *its own* client's mixed tap — so the
    /// frame has to say whose it is or the mute is a coin flip.
    pub client: crate::bridge::ClientRef,
}

/// Why a line was not taken. Every one of these is counted, not raised: the
/// plugin retries a whole batch on a non-2xx, so failing the batch over one
/// bad line would loop forever on it — the same rule the speaking endpoint has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    Shape,
    Rate,
    Pcm,
    TooLong,
}

impl Reject {
    pub fn as_str(self) -> &'static str {
        match self {
            Reject::Shape => "a field was missing or the wrong type",
            Reject::Rate => "the sample rate is not one we can resample from",
            Reject::Pcm => "the base64 PCM did not decode to whole samples",
            Reject::TooLong => "the frame is longer than [truth].audio_max_frame_ms",
        }
    }
}

/// Read one NDJSON line. `max_frame_ms` is the guard, not the expectation.
pub fn parse_frame(v: &Value, max_frame_ms: u64) -> Result<Frame, Reject> {
    let t_ms = v.get("t_ms").and_then(Value::as_i64).ok_or(Reject::Shape)?;
    let user_id = v
        .get("user_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or(Reject::Shape)?
        .to_string();
    let rate = v
        .get("rate")
        .and_then(Value::as_u64)
        .unwrap_or(SAMPLE_RATE as u64);
    let rate = u32::try_from(rate).map_err(|_| Reject::Rate)?;
    if !(MIN_RATE..=MAX_RATE).contains(&rate) {
        return Err(Reject::Rate);
    }
    let seq = v.get("seq").and_then(Value::as_u64).ok_or(Reject::Shape)?;
    let pcm = v.get("pcm").and_then(Value::as_str).ok_or(Reject::Shape)?;
    let name = v
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(&user_id)
        .to_string();
    let channel_id = v
        .get("channel_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let bytes = crate::b64::decode(pcm).ok_or(Reject::Pcm)?;
    if bytes.is_empty() || bytes.len() % 2 != 0 {
        return Err(Reject::Pcm);
    }
    let n = bytes.len() / 2;
    if (n as u64) * 1_000 / u64::from(rate) > max_frame_ms {
        return Err(Reject::TooLong);
    }
    // Little-endian PCM16, the only thing `Int16Array` produces on any machine
    // this runs on, and the only thing the plugin says it sends.
    let samples: Vec<f32> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|p| f32::from(i16::from_le_bytes(*p)) / 32_768.0)
        .collect();
    Ok(Frame {
        t_ms,
        user_id,
        name,
        channel_id,
        rate,
        seq,
        samples,
        client: crate::bridge::ClientRef::parse(v),
    })
}

// ---------------------------------------------------------------------------
// counters
// ---------------------------------------------------------------------------

#[derive(Default, Debug)]
pub struct AudioStats {
    pub frames: AtomicU64,
    pub samples: AtomicU64,
    pub rejected: AtomicU64,
    /// Frames whose sequence number was not its predecessor's successor. Each
    /// one is a hole the pipeline is told about rather than spliced over.
    pub gaps: AtomicU64,
    pub sessions_opened: AtomicU64,
    pub sessions_closed: AtomicU64,
    /// Voices minted because the account had never been linked to one.
    pub voices_minted: AtomicU64,
    /// Frames whose `t_ms` was too far from this machine's clock to believe.
    pub clock_skew: AtomicU64,
}

impl AudioStats {
    pub fn to_json(&self) -> Value {
        let g = |v: &AtomicU64| v.load(Ordering::Relaxed);
        json!({
            "frames": g(&self.frames),
            "samples": g(&self.samples),
            "rejected": g(&self.rejected),
            "gaps": g(&self.gaps),
            "sessions_opened": g(&self.sessions_opened),
            "sessions_closed": g(&self.sessions_closed),
            "voices_minted": g(&self.voices_minted),
            "clock_skew": g(&self.clock_skew),
        })
    }
}

// ---------------------------------------------------------------------------
// one live stream
// ---------------------------------------------------------------------------

/// Which stream a frame belongs to (0.12.3).
///
/// **The account, and not only the user.** Two bridges may both be in a call
/// with the same person — or, far more commonly, both report the local user of
/// the *other* client — and one stream table keyed on the user id alone would
/// braid two people's audio into one session, re-anchoring on every frame
/// because the two runs' sequence numbers interleave. `None` is an older
/// plugin, which is one bridge by construction.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct StreamKey {
    account: Option<String>,
    user: String,
}

struct Stream {
    session_id: i64,
    speaker_id: Option<i64>,
    /// The bridge this stream came from, so the mute rule can tell whose
    /// client's audio it would be a duplicate of.
    kind: Option<crate::bridge::ClientKind>,
    account: Option<String>,
    name: String,
    channel_id: Option<String>,
    resampler: LinearResampler,
    /// The monotonic instant of the first sample of the current CONTIGUOUS
    /// run, and how many 16 kHz samples of that run have been pushed. Together
    /// they place every frame without re-reading a clock per frame — the same
    /// discipline `clock::Anchor` keeps for the capture path, for the same
    /// reason: a wall clock read at handling time attributes audio to whenever
    /// the HTTP thread happened to get to it.
    run_mono_ns: u64,
    run_samples: u64,
    last_seq: Option<u64>,
    last_frame_mono_ns: u64,
    frames: u64,
}

// ---------------------------------------------------------------------------
// the router
// ---------------------------------------------------------------------------

/// Everything the audio half of the ingest owns: a session per account, the
/// clock that places their frames, and the liveness the de-duplication rule
/// reads.
pub struct PerUser {
    store: Arc<std::sync::Mutex<Store>>,
    queue: Arc<EventQueue>,
    cfg: TruthConfig,
    streams: std::sync::Mutex<HashMap<StreamKey, Stream>>,
    pub stats: Arc<AudioStats>,
}

impl PerUser {
    pub fn new(
        store: Arc<std::sync::Mutex<Store>>,
        queue: Arc<EventQueue>,
        cfg: TruthConfig,
        stats: Arc<AudioStats>,
    ) -> Self {
        Self {
            store,
            queue,
            cfg,
            streams: std::sync::Mutex::new(HashMap::new()),
            stats,
        }
    }

    /// Whether the audio half is switched on at all.
    pub fn accepting(&self) -> bool {
        self.cfg.audio
    }

    /// The parser's guard, so the ingest does not need its own copy of config.
    pub fn max_frame_ms(&self) -> u64 {
        self.cfg.audio_max_frame_ms.max(100)
    }

    /// Take one frame: place it in time, resample it, and push it at the same
    /// queue every microphone and every application pushes at.
    pub fn ingest(&self, frame: Frame) -> Result<()> {
        if !self.cfg.audio {
            return Ok(());
        }
        let now = utc_now_ns();
        let key = StreamKey {
            account: frame.client.account_id.clone(),
            user: frame.user_id.clone(),
        };
        let mut streams = self.streams.lock().unwrap_or_else(|p| p.into_inner());

        // ---- the session, opened on the first frame and not before ----
        if !streams.contains_key(&key) {
            let opened = self.open(&frame, now)?;
            streams.insert(key.clone(), opened);
        }
        let stream = streams
            .get_mut(&key)
            .expect("just inserted or already present");

        // A nickname that changed mid-call renames the source row, because the
        // row is a label and the label should be current. It never renames the
        // voice.
        if stream.name != frame.name {
            stream.name = frame.name.clone();
            if let Ok(store) = self.store.lock() {
                let _ = store.upsert_source_kind(
                    &match_key(&frame.user_id),
                    &display_name(&frame.name),
                    KIND_DISCORD_USER,
                    now,
                );
                let _ = store.upsert_discord_user(&frame.user_id, &frame.name, now);
            }
        }
        stream.channel_id = frame.channel_id.clone();

        // ---- place it in time ----
        //
        // Contiguous frames are placed by SAMPLE COUNT from the run's anchor,
        // not by their own `t_ms`. The wall clock is quantised to the
        // millisecond and jitters by whole scheduler slices; deriving each
        // frame's position from it would wander by ±20 ms per frame, and
        // `SessionPipeline::maybe_reanchor` reads a 100 ms wander as a hole and
        // throws away the turn in progress. So `t_ms` anchors the run, and the
        // samples carry it from there.
        //
        // A sequence gap is the exception, and it is the reason `seq` is on the
        // wire: audio was lost, the run is over, and the next frame starts a
        // new one from its own `t_ms`. The pipeline then sees a jump, discards
        // the half-built turn rather than splicing across the hole, and
        // re-anchors — which is precisely the behaviour a hole should get.
        let contiguous = stream.last_seq.is_some_and(|last| frame.seq == last + 1);
        if !contiguous {
            if stream.last_seq.is_some() {
                self.stats.gaps.fetch_add(1, Ordering::Relaxed);
                debug!(
                    user = %frame.user_id,
                    from = stream.last_seq,
                    to = frame.seq,
                    "a per-user audio frame was lost; re-anchoring rather than splicing"
                );
            }
            stream.run_mono_ns = self.wall_to_mono(frame.t_ms);
            stream.run_samples = 0;
            stream.resampler.reset();
        }
        stream.last_seq = Some(frame.seq);

        let capture_mono_ns = stream.run_mono_ns
            + (stream.run_samples * 1_000_000_000).div_euclid(u64::from(SAMPLE_RATE));

        let samples = stream
            .resampler
            .process(&frame.samples, frame.rate, SAMPLE_RATE);
        if samples.is_empty() {
            return Ok(());
        }
        stream.run_samples += samples.len() as u64;
        stream.last_frame_mono_ns = monotonic_ns();
        stream.frames += 1;

        self.stats.frames.fetch_add(1, Ordering::Relaxed);
        self.stats
            .samples
            .fetch_add(samples.len() as u64, Ordering::Relaxed);

        let session_id = stream.session_id;
        // The lock is dropped before the push: the queue's overflow policy can
        // walk its own deque, and holding the stream table across it would put
        // the HTTP thread's contention on the inference thread's back.
        drop(streams);
        self.queue.push(CaptureEvent::Audio(AudioChunk {
            session_id,
            capture_mono_ns,
            samples,
        }));
        Ok(())
    }

    /// Turn the plugin's `Date.now()` into this machine's monotonic clock.
    ///
    /// Both ends are the same machine, so this is an offset and not a
    /// synchronisation — but the offset has to be taken *now*, because
    /// `Anchor` is only linear within one reading and a call that has been
    /// running for hours may have crossed an NTP step.
    fn wall_to_mono(&self, t_ms: i64) -> u64 {
        let now = Anchor::now();
        let want_utc = t_ms.saturating_mul(1_000_000);
        let skew = want_utc - now.utc_ns;
        if skew.abs() > MAX_CLOCK_SKEW_NS {
            self.stats.clock_skew.fetch_add(1, Ordering::Relaxed);
            warn!(
                t_ms,
                skew_s = skew / 1_000_000_000,
                "a per-user audio frame's timestamp is nowhere near this clock; \
                 placing it at now instead"
            );
            return now.mono_ns;
        }
        (now.mono_ns as i64 + skew).max(0) as u64
    }

    /// Create the source row, resolve or mint the voice, and open the session.
    fn open(&self, frame: &Frame, now: i64) -> Result<Stream> {
        let store = self
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("store mutex poisoned"))?;

        // A sighting first, exactly as the speaking endpoint does it: every
        // line teaches us the account exists and what it is calling itself.
        store
            .upsert_discord_user(&frame.user_id, &frame.name, now)
            .context("recording the Discord account")?;
        let source_id = store
            .upsert_source_kind(
                &match_key(&frame.user_id),
                &display_name(&frame.name),
                KIND_DISCORD_USER,
                now,
            )
            .context("creating the per-user source row")?;

        // The voice. Linked already, or minted and linked now — and `Speaker_NN`
        // rather than the nickname, because naming a voice is a decision a
        // person makes.
        let speaker_id = match store
            .discord_user(&frame.user_id)?
            .and_then(|u| u.speaker_id)
        {
            Some(id) => Some(id),
            None => {
                let id = store.mint_speaker(now)?;
                store.set_discord_link(
                    &frame.user_id,
                    Some(id),
                    Some(truth_via::DISCORD_STREAM),
                    now,
                )?;
                self.stats.voices_minted.fetch_add(1, Ordering::Relaxed);
                info!(
                    user = %frame.user_id,
                    speaker_id = id,
                    "minted a voice for a Discord account that sent its own audio"
                );
                Some(id)
            }
        };

        // The user id on the session and not in the match key: `instance_key`
        // is what the schema already means by "which instance of this source",
        // and it is what `Store::discord_session_speaker` joins on.
        let session_id = store
            .begin_session_for(source_id, now, Some(&frame.user_id))
            .context("opening a per-user Discord session")?;
        drop(store);

        self.stats.sessions_opened.fetch_add(1, Ordering::Relaxed);
        info!(
            user = %frame.user_id,
            name = %frame.name,
            session_id,
            "a Discord user's own audio stream opened"
        );
        Ok(Stream {
            session_id,
            speaker_id,
            kind: frame.client.kind,
            account: frame.client.account_id.clone(),
            name: frame.name.clone(),
            channel_id: frame.channel_id.clone(),
            resampler: LinearResampler::new(),
            run_mono_ns: self.wall_to_mono(frame.t_ms),
            run_samples: 0,
            last_seq: None,
            last_frame_mono_ns: monotonic_ns(),
            frames: 0,
        })
    }

    /// Is any stream still arriving?
    ///
    /// **This is the de-duplication rule's whole input.** While it is true the
    /// mixed Discord tap is muted for analysis, because both are carrying the
    /// same call and leaving both on would write every sentence twice. It goes
    /// false `[truth].audio_live_s` after the last frame of the last stream,
    /// and the mixed tap picks the call back up on its next buffer — so a
    /// plugin that is switched off, crashes, or is running on a client that
    /// cannot do this costs a few seconds of transcript rather than the call.
    pub fn any_live(&self) -> bool {
        self.live_kinds().any()
    }

    /// [`Self::any_live`], but saying **whose** streams (0.12.3).
    ///
    /// This is the input the two-bridge rule actually needs. "Something is
    /// arriving" was a sufficient answer while there was one plugin; with two,
    /// it is the difference between muting the client whose call is being
    /// recorded twice and muting the one whose call nobody else can hear.
    pub fn live_kinds(&self) -> crate::bridge::LiveKinds {
        let mut out = crate::bridge::LiveKinds::default();
        if !self.cfg.audio {
            return out;
        }
        let window = (self.cfg.audio_live_s.max(0.5) * 1e9) as u64;
        let now = monotonic_ns();
        let streams = self.streams.lock().unwrap_or_else(|p| p.into_inner());
        for s in streams.values() {
            if now.saturating_sub(s.last_frame_mono_ns) <= window {
                out.insert(s.kind);
            }
        }
        out
    }

    /// Which bridge a per-user session's frames came from, for the pipeline:
    /// the stream's audio is evidence about that client's mixed tap and about
    /// no other.
    ///
    /// **Two levels of `Option`, and both are load-bearing.** The outer one is
    /// "there is no such stream any more" — a buffer still in the queue when
    /// the sweep closed its session — and the inner one is "an older plugin,
    /// which named no client". They must not collapse: an unnamed bridge's
    /// audio explains every mixed instance, so filing a stale buffer under it
    /// would let a stream that has stopped arriving mute a client for the rest
    /// of the window.
    pub fn client_kind_for_session(
        &self,
        session_id: i64,
    ) -> Option<Option<crate::bridge::ClientKind>> {
        let streams = self.streams.lock().unwrap_or_else(|p| p.into_inner());
        streams
            .values()
            .find(|s| s.session_id == session_id)
            .map(|s| s.kind)
    }

    /// Close the sessions of streams that have stopped arriving.
    ///
    /// Called on a timer rather than from `ingest`, because the case that
    /// matters is precisely the one where no more frames arrive: everybody left
    /// the call, or Discord was closed mid-word. The session's last turn is
    /// flushed by the pipeline when it reads the `SessionEnd` — this only says
    /// when.
    pub fn sweep(&self) -> usize {
        let idle = self.cfg.audio_idle_s.max(1) * 1_000_000_000;
        let now = monotonic_ns();
        let mut ended = Vec::new();
        {
            let mut streams = self.streams.lock().unwrap_or_else(|p| p.into_inner());
            streams.retain(|key, s| {
                let quiet = now.saturating_sub(s.last_frame_mono_ns) > idle;
                if quiet {
                    ended.push((key.user.clone(), s.session_id, s.last_frame_mono_ns));
                }
                !quiet
            });
        }
        for (user, session_id, last) in &ended {
            info!(user = %user, session_id, "a Discord user's audio stream went quiet; closing it");
            // Ended at the last frame we actually had, not at now: the last
            // thing anybody knows is that they were being heard then. The same
            // rule `[truth].open_span_timeout_s` follows for a speaking span.
            self.queue.push(CaptureEvent::SessionEnd {
                session_id: *session_id,
                mono_ns: *last,
            });
        }
        self.stats
            .sessions_closed
            .fetch_add(ended.len() as u64, Ordering::Relaxed);
        ended.len()
    }

    /// Close everything, for shutdown. Same path as [`Self::sweep`]'s, so a
    /// daemon stopping mid-call flushes the same last turn a quiet stream does.
    pub fn close_all(&self) {
        let drained: Vec<(i64, u64)> = {
            let mut streams = self.streams.lock().unwrap_or_else(|p| p.into_inner());
            streams
                .drain()
                .map(|(_, s)| (s.session_id, s.last_frame_mono_ns))
                .collect()
        };
        for (session_id, mono_ns) in &drained {
            self.queue.push(CaptureEvent::SessionEnd {
                session_id: *session_id,
                mono_ns: *mono_ns,
            });
        }
        self.stats
            .sessions_closed
            .fetch_add(drained.len() as u64, Ordering::Relaxed);
    }

    /// What `truth.status` shows: the streams, and enough about each to tell a
    /// live one from a stalled one without reading a log.
    pub fn status(&self) -> Value {
        let now = monotonic_ns();
        let window = (self.cfg.audio_live_s.max(0.5) * 1e9) as u64;
        let streams = self.streams.lock().unwrap_or_else(|p| p.into_inner());
        let mut rows: Vec<Value> = streams
            .iter()
            .map(|(key, s)| {
                let quiet_ms = now.saturating_sub(s.last_frame_mono_ns) / 1_000_000;
                json!({
                    "user_id": key.user,
                    // 0.12.3: which bridge's ear this is. Two rows may now
                    // carry one `user_id` — the same person, heard by two
                    // clients — and without these two fields that list is
                    // unreadable.
                    "account_id": s.account,
                    "client_kind": s.kind.map(crate::bridge::ClientKind::as_str),
                    "name": s.name,
                    "channel_id": s.channel_id,
                    "session_id": s.session_id,
                    "speaker_id": s.speaker_id,
                    "frames": s.frames,
                    "quiet_ms": quiet_ms,
                    "live": now.saturating_sub(s.last_frame_mono_ns) <= window,
                })
            })
            .collect();
        // Stable order, so a status that is polled does not shuffle.
        rows.sort_by(|a, b| {
            (a["account_id"].as_str(), a["user_id"].as_str())
                .cmp(&(b["account_id"].as_str(), b["user_id"].as_str()))
        });
        let live = rows.iter().filter(|r| r["live"] == json!(true)).count();
        json!({
            "enabled": self.cfg.audio,
            "live": live,
            "streams": rows,
            "live_s": self.cfg.audio_live_s,
            "idle_s": self.cfg.audio_idle_s,
            "counters": self.stats.to_json(),
        })
    }
}

/// Whether a source is the **mixed** Discord tap — the one that has to go
/// quiet while per-user streams are arriving.
///
/// Matched the way `[truth].sources` is matched everywhere else: lower-case
/// substrings against the match key. A per-user row is excluded explicitly
/// even though its key starts with `discord:` and would otherwise match its
/// own mute rule, which would be a very quiet bug.
pub fn is_mixed_discord_source(cfg: &TruthConfig, match_key: &str) -> bool {
    if match_key.starts_with("discord:") {
        return false;
    }
    let key = match_key.to_ascii_lowercase();
    cfg.sources
        .iter()
        .any(|s| !s.trim().is_empty() && key.contains(&s.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> TruthConfig {
        TruthConfig {
            audio: true,
            ..Default::default()
        }
    }

    fn line(pcm: &[i16], seq: u64, rate: u32) -> Value {
        let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
        json!({
            "t_ms": 1_788_201_960_000i64,
            "user_id": "42",
            "name": "Aspen",
            "channel_id": "c1",
            "rate": rate,
            "seq": seq,
            "pcm": crate::b64::encode(&bytes),
        })
    }

    #[test]
    fn a_frame_decodes_to_the_samples_that_were_sent() {
        let pcm: Vec<i16> = vec![0, 32_767, -32_768, 1_000];
        let f = parse_frame(&line(&pcm, 7, 16_000), 5_000).expect("a well-formed frame");
        assert_eq!(f.user_id, "42");
        assert_eq!(f.name, "Aspen");
        assert_eq!(f.channel_id.as_deref(), Some("c1"));
        assert_eq!(f.seq, 7);
        assert_eq!(f.rate, 16_000);
        assert_eq!(f.samples.len(), 4);
        assert_eq!(f.samples[0], 0.0);
        assert!((f.samples[1] - 0.999_97).abs() < 1e-4);
        assert_eq!(f.samples[2], -1.0);
    }

    #[test]
    fn a_missing_field_is_a_refused_line_and_not_a_default() {
        // Every one of these could be "defaulted" into something plausible, and
        // every one of those defaults would be a fabricated recording.
        for key in ["t_ms", "user_id", "seq", "pcm"] {
            let mut v = line(&[1, 2], 0, 16_000);
            v.as_object_mut().unwrap().remove(key);
            assert_eq!(
                parse_frame(&v, 5_000).unwrap_err(),
                Reject::Shape,
                "{key} must be required"
            );
        }
        // A nickname is the one thing that may be missing: the id is the
        // identity and the name is a label.
        let mut v = line(&[1, 2], 0, 16_000);
        v.as_object_mut().unwrap().remove("name");
        assert_eq!(parse_frame(&v, 5_000).unwrap().name, "42");
    }

    #[test]
    fn an_impossible_rate_or_a_ragged_frame_is_refused() {
        assert_eq!(
            parse_frame(&line(&[1], 0, 1_000), 5_000).unwrap_err(),
            Reject::Rate
        );
        assert_eq!(
            parse_frame(&line(&[1], 0, 192_000), 5_000).unwrap_err(),
            Reject::Rate
        );
        // An odd number of bytes is half a sample, which is a corrupted frame
        // and not a shorter one.
        let mut v = line(&[1, 2], 0, 16_000);
        v["pcm"] = json!(crate::b64::encode(&[1u8, 2, 3]));
        assert_eq!(parse_frame(&v, 5_000).unwrap_err(), Reject::Pcm);
        v["pcm"] = json!("not base64!");
        assert_eq!(parse_frame(&v, 5_000).unwrap_err(), Reject::Pcm);
    }

    #[test]
    fn a_frame_longer_than_the_guard_is_refused_rather_than_queued() {
        // 2 s at 16 kHz against a 1 s guard.
        let pcm = vec![0i16; 32_000];
        assert_eq!(
            parse_frame(&line(&pcm, 0, 16_000), 1_000).unwrap_err(),
            Reject::TooLong
        );
        assert!(parse_frame(&line(&pcm, 0, 16_000), 5_000).is_ok());
    }

    #[test]
    fn the_mixed_tap_is_recognised_and_a_per_user_row_is_never_mistaken_for_it() {
        let c = cfg();
        assert!(is_mixed_discord_source(&c, "vesktop"));
        assert!(is_mixed_discord_source(&c, "Discord"));
        assert!(!is_mixed_discord_source(&c, "VRChat.exe"));
        assert!(!is_mixed_discord_source(&c, "mic"));
        assert!(!is_mixed_discord_source(&c, "room"));
        // The one that would be a very quiet bug: a per-user row muting itself.
        assert!(!is_mixed_discord_source(&c, "discord:12345"));
    }

    #[test]
    fn the_match_key_carries_the_id_and_the_display_name_carries_the_nickname() {
        assert_eq!(match_key("12345"), "discord:12345");
        assert_eq!(display_name("Aspen"), "Discord · Aspen");
        // A renamed account keeps its row: the key is the id.
        assert_eq!(match_key("12345"), match_key("12345"));
    }
}
