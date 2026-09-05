//! The inference side: drain the capture queue, run VAD, write segments.
//!
//! Everything here runs on one deliberately deprioritised thread. Step 0's
//! field measurement put the whole analysis pipeline under 5% of one core, so
//! throughput is not the concern — scheduling is. The failure this guards
//! against is analysis work winning a timeslice from a VR frame.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use anyhow::{Context, Result};
use serde_json::json;
use tracing::{debug, error, info, warn};

use crate::analysis::{
    AnalysisStats, Analyzer, MicEnroll, PinnedLeg, analyse_mic_or_log, analyse_or_log,
    analyse_pinned_or_log,
};
use crate::bus::{Bus, Topic};
use crate::clock::{Anchor, samples_to_ns, utc_now_ns};
use crate::config::{Config, SAMPLE_RATE};
use crate::control::Control;
use crate::models::ModelSet;
use crate::queue::{AudioChunk, CaptureEvent, EventQueue};
use crate::store::{KIND_MIC, Store};
use crate::turns::TurnMerger;
use crate::vad::{FRAME_SAMPLES, SegmenterConfig, SileroVad, VadState};

/// The stronger form for BACKGROUND passes (0.11.2): nice, pin, and then the
/// idle scheduling class, which yields to anything else runnable at all —
/// including this daemon's own capture thread. The live inference thread must
/// NOT use this (an idle-class ASR under load would never transcribe); the
/// night shift, the digests, the translator, the cross-check and the truth
/// pass must, because on the first night all of them ran at once the capture
/// thread, then at the same nice as they were, missed its PipeWire deadlines
/// 658 times in one hour. Children spawned from the thread (llama-cli,
/// whisper-cli) inherit the class.
pub fn background_current_thread(nice: i32, cpus: &[usize]) {
    deprioritise_current_thread(nice, cpus);
    // SAFETY: gettid cannot fail; sched_setscheduler with SCHED_IDLE takes a
    // zeroed sched_param and needs no privilege to LOWER a thread's class.
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid) as libc::pid_t;
        let param: libc::sched_param = std::mem::zeroed();
        if libc::sched_setscheduler(tid, libc::SCHED_IDLE, &param) != 0 {
            warn!(
                "could not move a background thread to SCHED_IDLE: {}",
                std::io::Error::last_os_error()
            );
        } else {
            debug!("background thread moved to SCHED_IDLE");
        }
    }
}

/// Apply the project's "never steal a VR frame" rule to the calling thread.
///
/// `setpriority(PRIO_PROCESS, tid, ...)` is per-thread on Linux despite the
/// name: the "process" ID it takes is really a task ID, so this niceness lands
/// on the inference thread alone and leaves the PipeWire thread responsive.
pub fn deprioritise_current_thread(nice: i32, cpus: &[usize]) {
    // SAFETY: gettid takes no arguments and cannot fail.
    let tid = unsafe { libc::syscall(libc::SYS_gettid) } as libc::id_t;

    // SAFETY: PRIO_PROCESS with a live tid; failure is reported via errno and
    // is not fatal for us.
    let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS, tid, nice) };
    if rc != 0 {
        warn!(
            "could not set nice {nice} on the inference thread: {}",
            std::io::Error::last_os_error()
        );
    } else {
        debug!("inference thread niced to {nice}");
    }

    if cpus.is_empty() {
        return;
    }
    // SAFETY: the set is zeroed before use and only valid CPU indices are set.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_ZERO(&mut set);
        for &c in cpus {
            if c < libc::CPU_SETSIZE as usize {
                libc::CPU_SET(c, &mut set);
            }
        }
        if libc::sched_setaffinity(tid as libc::pid_t, size_of::<libc::cpu_set_t>(), &set) != 0 {
            warn!(
                "could not pin the inference thread to {cpus:?}: {}",
                std::io::Error::last_os_error()
            );
        } else {
            debug!("inference thread pinned to CPUs {cpus:?}");
        }
    }
}

/// Per-source analysis state. Kept separate per session because the VAD's LSTM
/// carries context: sharing it across sources would let one app's audio bias
/// another's decisions.
struct SessionPipeline {
    vad_state: VadState,
    segmenter: crate::vad::Segmenter,
    /// Kept so the segmenter can be replaced mid-session when a gap forces the
    /// buffered turn to be discarded (`discard_across_gap`).
    seg_cfg: SegmenterConfig,
    /// Joins VAD segments separated by a short silence, so what reaches storage
    /// and the analysis leg is a turn rather than a breath-sized fragment.
    turns: TurnMerger,
    /// Rolling audio buffer; `ring[0]` is absolute sample `ring_base`.
    ring: Vec<f32>,
    ring_base: u64,
    /// Total samples ever accepted for this session.
    received: u64,
    /// Clock anchor for converting sample indices to UTC.
    anchor: Anchor,
    anchor_sample: u64,
    segment_seq: u64,
    /// Whether this session belongs to the user's own microphone
    /// (`sources.kind = "mic"`). Read once from the row when the session
    /// starts: the identity route differs, and it has to be the database that
    /// says so rather than a flag the capture side hoped would survive a queue
    /// that is allowed to drop things.
    is_mic: bool,
    /// Whether this session belongs to the ROOM microphone (0.10.0,
    /// `sources.kind = "room"`). Read the same way and for a much smaller
    /// reason: a room turn takes the ORDINARY route in every respect — VAD,
    /// the overlap gate, ASR, the voicebank — so nothing branches on it. It is
    /// carried only so the daemon can count what came off that device, which
    /// is otherwise invisible precisely because it is treated like everything
    /// else.
    is_room: bool,
    /// The voice this session's turns are pinned to, when the session is one
    /// Discord user's own audio stream (0.12.1, `sources.kind =
    /// "discord-user"`). Read from the row once, like the two flags above, and
    /// for the same reason: `Store::discord_session_speaker` is the thing that
    /// knows, and the capture side is not.
    ///
    /// `Some` here is the strongest claim in this struct. It means the whole
    /// identity ladder is skipped — no comparison, no mint, no margin — because
    /// the audio arrived on a wire that carried one person's packets. It is
    /// exactly [`Self::is_mic`]'s claim about somebody who is not the user.
    discord_speaker: Option<i64>,
    // ---- 0.11.0, partial turns: begin --------------------------------------
    /// Cadence, sequence and the previous turn's identity, for the provisional
    /// captions published while a turn is still open (`crate::partial`).
    partial: crate::partial::PartialState,
    /// The session's source match key, read once and cached — the outer
    /// `Option` is "have we looked", the inner one is the answer. A partial has
    /// to name its source before any row of the session exists, so it cannot
    /// borrow the one `SegmentRow` carries.
    source_key: Option<Option<String>>,
    // ---- 0.11.0, partial turns: end ----------------------------------------
    // ---- 0.12.5, sliced turns: begin ---------------------------------------
    /// Where the open turn has already been cut, and how often
    /// (`crate::slice`).
    slicer: crate::slice::Slicer,
    /// The words of every slice of the open turn, in order, already joined.
    ///
    /// This is the half of the feature that makes it O(N) rather than O(N²):
    /// a slice's decode is kept, so when the turn ends only the REMAINDER is
    /// read and the row's text is this plus that. A partial, by contrast, threw
    /// every decode away and paid for the whole turn again a second later.
    slice_text: String,
    // ---- 0.12.5, sliced turns: end -----------------------------------------
}

impl SessionPipeline {
    fn new(
        vad_state: VadState,
        seg_cfg: SegmenterConfig,
        turns: TurnMerger,
        first_chunk_mono_ns: u64,
        is_mic: bool,
        is_room: bool,
        discord_speaker: Option<i64>,
    ) -> Self {
        Self {
            vad_state,
            segmenter: crate::vad::Segmenter::new(seg_cfg),
            seg_cfg,
            turns,
            ring: Vec::new(),
            ring_base: 0,
            received: 0,
            anchor: Anchor::at(first_chunk_mono_ns),
            anchor_sample: 0,
            segment_seq: 0,
            is_mic,
            is_room,
            discord_speaker,
            // ---- 0.11.0, partial turns ------------------------------------
            partial: crate::partial::PartialState::default(),
            source_key: None,
            // ---- end 0.11.0 -----------------------------------------------
            // ---- 0.12.5, sliced turns --------------------------------------
            slicer: crate::slice::Slicer::default(),
            slice_text: String::new(),
            // ---- end 0.12.5 ------------------------------------------------
        }
    }

    fn utc_of_sample(&self, sample: u64) -> i64 {
        let delta = sample as i64 - self.anchor_sample as i64;
        let ns = (delta as i128 * 1_000_000_000i128) / SAMPLE_RATE as i128;
        self.anchor.utc_ns + ns as i64
    }

    /// A queue overflow or an xrun leaves a real hole in the audio. Detect it by
    /// comparing the buffer's own monotonic stamp against where the sample
    /// counter thinks we are, and re-anchor so later segments are not shifted
    /// by the missing time.
    ///
    /// Returns whether a gap was found, because re-anchoring the clock is only
    /// half the answer: see [`Self::discard_across_gap`].
    fn maybe_reanchor(&mut self, chunk_mono_ns: u64) -> bool {
        let predicted = self
            .anchor
            .mono_ns
            .wrapping_add(samples_to_ns(self.received - self.anchor_sample, SAMPLE_RATE) as u64);
        let skew = chunk_mono_ns as i64 - predicted as i64;
        if skew.unsigned_abs() > 100_000_000 {
            debug!(
                skew_ms = skew / 1_000_000,
                "re-anchoring session clock after a gap"
            );
            self.anchor = Anchor {
                mono_ns: chunk_mono_ns,
                utc_ns: self.anchor.utc_of(chunk_mono_ns),
            };
            self.anchor_sample = self.received;
            return true;
        }
        false
    }

    /// Throw away everything buffered before a gap.
    ///
    /// Re-anchoring alone kept the audio on either side of the hole in one
    /// open turn, so a drop in the middle of somebody speaking was spliced into
    /// a single segment: words from before the gap joined to words from after
    /// it, timed from the re-anchored clock, and the embedding taken over the
    /// splice. That is wrong three ways at once — the transcript says a
    /// sentence nobody said, the clip jump-cuts, and a vector over two
    /// disjoint pieces of speech can mint a voice that does not exist
    /// (audit finding #21).
    ///
    /// So the pre-gap half is dropped, by exactly the mechanics of the pause
    /// discard: the VAD's context, the segmenter, the open turn and the ring go
    /// together, because a turn half of whose audio is gone is not a turn. The
    /// clock, the sample counter and the segment sequence stay — they are the
    /// session's, not the turn's. What comes back is a clean segment starting
    /// after the gap.
    fn discard_across_gap(&mut self, vad_state: VadState) {
        self.vad_state = vad_state;
        self.segmenter = crate::vad::Segmenter::resuming_at(self.seg_cfg, self.received);
        // Dropped on the floor, deliberately: this is the turn that would
        // otherwise have been spliced across the hole.
        let _ = self.turns.flush();
        self.ring.clear();
        self.ring_base = self.received;
        // ---- 0.11.0, partial turns ----------------------------------------
        // A client may be showing provisional words for the turn that just went
        // on the floor. There is no `segment` coming to replace them and no
        // identity to lend the next turn, so the open turn is forgotten here
        // rather than left to age out on the glass.
        self.partial.reset();
        // ---- end 0.11.0 ---------------------------------------------------
        // ---- 0.12.5, sliced turns ------------------------------------------
        // Same reasoning, one step stronger. A client is showing words for a
        // turn whose audio has a hole in it; no `segment` is coming to replace
        // them, and the slices already decoded describe speech that is being
        // thrown away. Keeping `slice_text` would splice the pre-gap words onto
        // the post-gap turn, which is exactly the splice this whole method
        // exists to prevent (audit finding #21).
        self.slicer.reset();
        self.slice_text.clear();
        // ---- end 0.12.5 ----------------------------------------------------
    }

    fn extract(&self, start: u64, end: u64) -> Vec<f32> {
        let lo = start.saturating_sub(self.ring_base) as usize;
        let hi = ((end.saturating_sub(self.ring_base)) as usize).min(self.ring.len());
        if lo >= hi {
            return Vec::new();
        }
        self.ring[lo..hi].to_vec()
    }

    fn trim(&mut self) {
        // An open turn pins the ring further back than the segmenter would:
        // its audio is still waiting for a possible continuation.
        let keep_from = self
            .segmenter
            .retain_from()
            .min(self.turns.pending_start().unwrap_or(u64::MAX));
        if keep_from > self.ring_base {
            let drop = (keep_from - self.ring_base) as usize;
            if drop >= self.ring.len() {
                self.ring.clear();
                self.ring_base = keep_from;
            } else if drop > 0 {
                self.ring.drain(..drop);
                self.ring_base = keep_from;
            }
        }
    }
}

/// Live counters, readable from other threads for logging.
#[derive(Default)]
pub struct Stats {
    pub segments_written: AtomicU64,
    pub frames_analysed: AtomicU64,
    pub sessions_opened: AtomicU64,
    /// Turns thrown away because the audio under them had a hole in it. The
    /// count matters: it is the difference between "the queue dropped buffers"
    /// (which `drops` already says) and "a turn was actually lost because of
    /// it", and it is the number that says whether the queue is big enough.
    pub gaps_discarded: AtomicU64,
    /// When the last turn was written, in UTC nanoseconds, or 0 before the
    /// first one (0.9.0). The night shift's idle rule reads it: "no capture
    /// activity for N minutes" is a statement about turns landing, and this is
    /// the one place that knows when the last one did.
    pub last_segment_ns: AtomicI64,
    /// Turns that came off the room microphone (0.10.0). They are ordinary
    /// turns everywhere else in this file, which is exactly why the count is
    /// worth keeping: without it there is no way to tell a room mic that is
    /// recording from one that is merely switched on.
    pub room_segments: AtomicU64,
    /// Flaps absorbed (0.13.0, `crate::flap`): a source's node vanished and
    /// reappeared inside the grace window, and the session that was open
    /// before it left is the same one that is open now. One count per flap,
    /// not per burst — `status` is the place a burst is a single number too,
    /// but this is meant to answer "how much of this has been happening",
    /// which a burst count alone cannot: nineteen absorbed flaps in one burst
    /// and nineteen absorbed flaps spread across a week read very differently.
    pub flaps_absorbed: AtomicU64,
}

/// Which side of the per-user de-duplication rule a session sits on (0.12.2).
///
/// Three answers and not two, because the rule needs both halves of the
/// comparison: the per-user streams are the *evidence*, the mixed instances are
/// the *candidates*, and everything else in the program is neither and must
/// never be looked at by any of this.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DiscordSide {
    /// A `discord:<user_id>` source — one person's own stream off the plugin.
    PerUser,
    /// A mixed Discord tap: one client's whole output, one instance of it.
    Mixed {
        source: String,
        instance_key: Option<String>,
    },
    /// Everything else the daemon records. VRChat, the microphone, the room.
    Neither,
}

pub struct Pipeline {
    vad: SileroVad,
    seg_cfg: SegmenterConfig,
    cfg: Config,
    /// `None` when no models are configured or present: the daemon then behaves
    /// exactly as it did in Step 1 rather than refusing to capture.
    analyzer: Option<Analyzer>,
    analysis_stats: Arc<AnalysisStats>,
    store: Arc<std::sync::Mutex<Store>>,
    data_dir: PathBuf,
    sessions: HashMap<i64, SessionPipeline>,
    stats: Arc<Stats>,
    /// Global pause. Read before every write, never messaged: the panic path
    /// must not have to reach the front of a queue to take effect.
    control: Arc<Control>,
    bus: Arc<Bus>,
    /// Whether the last chunk we saw was refused because of a pause, so the
    /// transition is logged once rather than per buffer.
    was_paused: bool,
    /// Semantic search's text embedder (0.6.5), when the optional model is
    /// installed. Shared with the socket service — one 118 MB session, not two.
    semantic: Option<Arc<crate::semantic::SemanticLeg>>,
    // ---- 0.12.1: per-user Discord audio ------------------------------------
    /// The per-user audio router, when the daemon has one. Read for exactly one
    /// question — "is any per-user stream live right now" — which is the whole
    /// input to the de-duplication rule in [`Self::on_audio`].
    peruser: Option<Arc<crate::peruser::PerUser>>,
    /// `[truth].sources`, so a session's match key can be tested against the
    /// same list everything else tests against.
    truth_cfg: crate::config::TruthConfig,
    /// Which side of the de-duplication rule a session is on. Cached per
    /// session because it is a property of the source row and cannot change
    /// under a session, and kept OUTSIDE `sessions` because the mute removes
    /// the `SessionPipeline` and would otherwise re-read the row per buffer.
    discord_side: HashMap<i64, DiscordSide>,
    /// 0.12.2: which Discord *instance* the mute is aimed at. `None` on a
    /// daemon wired without one, in which case nothing is ever muted — which
    /// is the safe half of the trade.
    bridge: Option<Arc<crate::bridge::Picker>>,
    /// Which mixed sessions are currently muted, so the transition is logged
    /// once rather than fifty times a second.
    muted_mixed: std::collections::HashSet<i64>,
    // ---- end 0.12.1 --------------------------------------------------------
    // ---- light mode (0.13.x, `crate::light`) -------------------------------
    /// The resolved model set the analyzer was loaded from, kept so the
    /// inference thread can re-point the ASR leg without re-resolving
    /// `[models].dir` off disk on every check. `None` exactly when `analyzer`
    /// is — no models, no light mode.
    models: Option<ModelSet>,
    /// The GPU's smoothed busy reading (`crate::light::GpuBusyMonitor`).
    gpu_busy: crate::light::GpuBusyMonitor,
    /// When the switch was last evaluated, so a check that would otherwise run
    /// once per audio chunk runs about once a second instead.
    light_checked_at: Option<std::time::Instant>,
    // ---- end light mode ------------------------------------------------
}

impl Pipeline {
    pub fn new(
        cfg: &Config,
        store: Arc<std::sync::Mutex<Store>>,
        data_dir: PathBuf,
        stats: Arc<Stats>,
        analysis_stats: Arc<AnalysisStats>,
        control: Arc<Control>,
        bus: Arc<Bus>,
    ) -> Result<Self> {
        let vad = SileroVad::from_bytes(crate::VAD_MODEL)?;
        let seg_cfg = crate::ingest::segmenter_config(cfg);
        // Kept alongside the analyzer so light mode can re-point the ASR leg
        // later without re-resolving `[models].dir` off disk.
        let mut resolved_models: Option<ModelSet> = None;
        let analyzer = match ModelSet::resolve(&cfg.models) {
            None => {
                info!("no [models].dir configured: capture and VAD only");
                None
            }
            Some(mut models) => {
                // Resolve the ASR leg against the disk before judging the set:
                // a machine that still only has the old English-only export
                // keeps transcribing (loudly) instead of losing analysis.
                let selection = models.select_asr();
                if let Some(line) = selection.warning(&models.root) {
                    warn!("{line}");
                }
                if models.complete() {
                    let mut analyzer = Analyzer::load(&models, &cfg.identity)?;
                    analyzer.set_lang_config(&cfg.lang);
                    // 0.11.0: the Japanese router's switch and operating
                    // point. Set here rather than in `load` because it owns
                    // two lazily loaded models and must be built once, before
                    // the inference thread starts.
                    analyzer.set_asr_config(&models, &cfg.asr, &cfg.night, &cfg.runtime);
                    // 0.11.0: which capture sources count as Discord, so the
                    // source prior's hard presence rule knows where it applies.
                    analyzer.set_truth_config(&cfg.truth);
                    if cfg.identity.source_prior {
                        info!(
                            foreign_margin = cfg.identity.foreign_source_margin,
                            after_segments = cfg.identity.foreign_after_segments,
                            presence_hard = cfg.identity.presence_hard,
                            "the source-aware identity prior is on"
                        );
                    }
                    // Which flips this machine can actually settle. Said once,
                    // because "it was only flagged" otherwise has no visible
                    // cause — the arbiters are optional and both of them are.
                    let installed = analyzer.arbiters_installed();
                    if installed.is_empty() {
                        info!(
                            "no flip arbiter is installed: a wrong-language transcript will be \
                             flagged, never re-read. `recalld models fetch --arbiter-de \
                             --fallback-asr` installs both."
                        );
                    } else {
                        info!(arbiters = installed.join(", "), "flip arbiters available");
                    }
                    // 0.11.0: whether this machine can hear Japanese at all.
                    // Said once for the same reason the arbiters are — the
                    // failure it prevents is silent, so its absence must not
                    // be.
                    if let Some(note) = analyzer.japanese_note() {
                        info!("{note}");
                    } else {
                        info!("Japanese turns will be detected and re-decoded");
                    }
                    // 0.11.6: and which OTHER languages it can re-decode into.
                    // Same reasoning again — a flip that is never corrected
                    // leaves no trace saying so.
                    if let Some(note) = analyzer.polyglot_note() {
                        info!("{note}");
                    }
                    resolved_models = Some(models);
                    Some(analyzer)
                } else {
                    warn!(
                        "analysis models incomplete under {} — see `recalld models status`; \
                         capturing without ASR or speaker identity",
                        models.root.display()
                    );
                    None
                }
            }
        };
        Ok(Self {
            vad,
            seg_cfg,
            cfg: cfg.clone(),
            analyzer,
            analysis_stats,
            store,
            data_dir,
            sessions: HashMap::new(),
            stats,
            control,
            bus,
            was_paused: false,
            semantic: None,
            peruser: None,
            truth_cfg: cfg.truth.clone(),
            discord_side: HashMap::new(),
            bridge: None,
            muted_mixed: std::collections::HashSet::new(),
            models: resolved_models,
            gpu_busy: crate::light::GpuBusyMonitor::new(),
            light_checked_at: None,
        })
    }

    /// Give the inference thread the per-user audio router (0.12.1). Set once,
    /// before the thread starts, like the semantic leg beside it.
    pub fn attach_peruser(&mut self, peruser: Arc<crate::peruser::PerUser>) {
        self.peruser = Some(peruser);
    }

    /// Give the inference thread the instance picker (0.12.2). Shared with the
    /// socket, which reads its verdicts for `truth.status`; this thread is the
    /// only writer of evidence.
    pub fn attach_bridge(&mut self, bridge: Arc<crate::bridge::Picker>) {
        self.bridge = Some(bridge);
    }

    /// Give the inference thread the semantic leg, so a turn is embedded as
    /// soon as its transcript exists. Set once, before the thread starts.
    pub fn attach_semantic(&mut self, leg: Arc<crate::semantic::SemanticLeg>) {
        self.semantic = Some(leg);
    }

    /// Drain `queue` until it closes. Intended to be the body of the inference
    /// thread; a failure on one session is logged and does not stop the rest.
    pub fn run(&mut self, queue: Arc<EventQueue>) {
        while let Some(event) = queue.pop() {
            let result = match event {
                CaptureEvent::Audio(chunk) => self.on_audio(chunk),
                CaptureEvent::SessionEnd {
                    session_id,
                    mono_ns,
                } => self.on_session_end(session_id, mono_ns),
                CaptureEvent::Gap {
                    session_id,
                    mono_ns,
                    gap_ms,
                } => self.on_gap(session_id, mono_ns, gap_ms),
            };
            if let Err(e) = result {
                error!("analysis error: {e:#}");
            }
        }
        // Queue closed: flush whatever is still open so no speech is lost.
        let ids: Vec<i64> = self.sessions.keys().copied().collect();
        for id in ids {
            if let Err(e) = self.on_session_end(id, crate::clock::monotonic_ns()) {
                error!("flushing session {id}: {e:#}");
            }
        }
    }

    // ---- light mode (0.13.x, `crate::light`) -------------------------------
    /// Re-evaluate the switch and swap the decoder if the answer changed.
    ///
    /// Called from [`Self::on_audio`], which is the inference thread's only
    /// entry point — the same reason `is_paused` is checked there rather than
    /// on a timer of its own. Throttled to about once a second: `on_audio` can
    /// run many times a second and neither a sysfs read nor a session-table
    /// lookup belongs on that path unthrottled, let alone a model reload.
    fn maybe_update_light_mode(&mut self) {
        let Some(models) = self.models.clone() else {
            // No analysis models resolved at all: there is nothing to swap.
            return;
        };
        let now = std::time::Instant::now();
        const CHECK_EVERY: std::time::Duration = std::time::Duration::from_secs(1);
        if self
            .light_checked_at
            .is_some_and(|last| now.duration_since(last) < CHECK_EVERY)
        {
            return;
        }
        self.light_checked_at = Some(now);

        let cfg = self.control.asr();
        self.gpu_busy.sample(now, crate::night::gpu_busy_pct());

        let game_active = self
            .sessions
            .keys()
            .copied()
            .collect::<Vec<_>>()
            .into_iter()
            .any(|id| {
                self.session_source_key(id)
                    .is_some_and(|key| crate::light::is_game_source(&cfg.light_mode_games, &key))
            });
        let (want_light, reason) = crate::light::decide(
            cfg.light_mode,
            game_active,
            self.gpu_busy.median(),
            cfg.light_mode_gpu_busy_pct,
        );

        let changed_report = self.control.set_light_state(crate::light::LightState {
            light: want_light,
            reason,
        });

        let Some(analyzer) = &mut self.analyzer else {
            return;
        };
        match analyzer.set_light(&models, want_light) {
            Ok(true) => {
                info!(
                    light = want_light,
                    reason = reason.as_str(),
                    model = analyzer.asr_model_id(),
                    "light mode switched the live decoder"
                );
            }
            Ok(false) => {}
            Err(e) => {
                warn!("light mode wanted to switch but could not: {e:#}");
            }
        }
        if changed_report {
            self.bus.publish(
                Topic::Status,
                "light",
                json!({ "light": want_light, "reason": reason.as_str() }),
            );
        }
    }
    // ---- end light mode ------------------------------------------------

    fn on_audio(&mut self, chunk: AudioChunk) -> Result<()> {
        // Paused: the capture stream keeps running — dropping it would cost a
        // re-negotiation and a lost session — but the audio stops here. Any
        // half-built turn is discarded with the session state, so nothing said
        // before the pause can be written by a segment that ends after it.
        if self.control.is_paused() {
            if !self.was_paused {
                self.was_paused = true;
                info!("paused: audio is being discarded, nothing is written");
            }
            self.sessions.remove(&chunk.session_id);
            return Ok(());
        }
        if self.was_paused {
            self.was_paused = false;
            info!("resumed: writing segments again");
        }

        self.maybe_update_light_mode();

        // ---- 0.12.1: the de-duplication rule ---------------------------------
        //
        // While ANY per-user Discord stream is live, the MIXED Discord tap is
        // muted for analysis. Both are carrying the same call — one off the
        // speakers, one per person straight out of the client — so leaving both
        // on would write every sentence twice, under two different speakers, in
        // the same thread.
        //
        // Muting rather than de-duplicating afterwards, because the duplicate
        // this prevents has no key to be found by: `segments` has no uniqueness
        // constraint a second reading of the same speech would violate, and
        // matching two turns by time overlap after the fact would be a guess
        // about which of two transcripts is the real one. The turn that is
        // never written needs no rule for choosing.
        //
        // It is the same discard a pause performs, for the same reason: the
        // half-built turn goes with it, so nothing said before the mute can be
        // written by a segment that ends after it. When the last stream goes
        // quiet (`[truth].audio_live_s` after its last frame) the mixed tap
        // picks the call back up on its next buffer, opening a fresh turn — so
        // a plugin that is switched off mid-call costs a few seconds of
        // transcript rather than the rest of the evening.
        //
        // 0.12.2 aims it at one INSTANCE rather than at a source key. The user
        // runs two Discord clients, only one of which carries the plugin, and
        // muting by source key took the other one's whole call off the record
        // for as long as the plugin's call lasted. `crate::bridge` decides
        // which instance the streams are explaining, and only that one goes
        // quiet.
        if self.mixed_discord_is_muted(&chunk) {
            if !self.muted_mixed.contains(&chunk.session_id) {
                self.muted_mixed.insert(chunk.session_id);
                info!(
                    session_id = chunk.session_id,
                    why = self.mute_reason(chunk.session_id).as_deref().unwrap_or("—"),
                    "per-user Discord streams are live; muting this Discord instance for analysis"
                );
            }
            self.sessions.remove(&chunk.session_id);
            return Ok(());
        }
        if self.muted_mixed.remove(&chunk.session_id) {
            info!(
                session_id = chunk.session_id,
                "this Discord instance is analysing again"
            );
        }
        // ---- end 0.12.1 ------------------------------------------------------

        let seg_cfg = self.seg_cfg;
        let new_state = self.vad.new_state();
        let merger = crate::ingest::turn_merger(&self.cfg);
        // One row read per session, not per buffer. A lookup that fails reads
        // as "an application", which is the safe answer: the worst case is a
        // mic turn going through the voicebank like any other voice, never an
        // app turn being labelled as the user.
        let (is_mic, is_room, discord_speaker) = if self.sessions.contains_key(&chunk.session_id) {
            (false, false, None) // unused; the entry already exists
        } else {
            let kind = self.session_kind(chunk.session_id);
            let discord_speaker = (kind.as_deref() == Some(crate::store::KIND_DISCORD_USER))
                .then(|| self.session_discord_speaker(chunk.session_id))
                .flatten();
            (
                kind.as_deref() == Some(KIND_MIC),
                kind.as_deref() == Some(crate::store::KIND_ROOM),
                discord_speaker,
            )
        };
        let fresh_state = self.vad.new_state();
        let entry = self.sessions.entry(chunk.session_id).or_insert_with(|| {
            SessionPipeline::new(
                new_state,
                seg_cfg,
                merger,
                chunk.capture_mono_ns,
                is_mic,
                is_room,
                discord_speaker,
            )
        });

        // A gap is not only a clock problem. Re-anchoring keeps later segments
        // in the right place; discarding what was buffered before the hole is
        // what keeps the words on either side of it out of the same segment
        // (audit finding #21).
        if entry.maybe_reanchor(chunk.capture_mono_ns) {
            entry.discard_across_gap(fresh_state);
            self.stats.gaps_discarded.fetch_add(1, Ordering::Relaxed);
            warn!(
                session_id = chunk.session_id,
                "audio gap: discarded the turn in progress rather than splicing across it"
            );
        }
        entry.ring.extend_from_slice(&chunk.samples);
        entry.received += chunk.samples.len() as u64;

        let mut emitted = Vec::new();
        while entry.segmenter.cursor() + FRAME_SAMPLES as u64 <= entry.received {
            let start = entry.segmenter.cursor();
            let lo = (start - entry.ring_base) as usize;
            let frame: [f32; FRAME_SAMPLES] = match entry.ring.get(lo..lo + FRAME_SAMPLES) {
                Some(s) => s.try_into().expect("slice of exactly FRAME_SAMPLES"),
                None => {
                    // The ring was trimmed past the cursor; skip ahead rather
                    // than reading stale audio.
                    warn!("VAD cursor fell behind the audio ring; skipping forward");
                    break;
                }
            };
            let prob = self.vad.frame(&frame, &mut entry.vad_state)?;
            self.stats.frames_analysed.fetch_add(1, Ordering::Relaxed);
            if let Some(span) = entry
                .segmenter
                .push_frame(prob, start, FRAME_SAMPLES as u64)
            {
                emitted.extend(entry.turns.push(span));
            }
            // Release a turn once the merge window has passed, so a last turn
            // followed by silence is not held until the session ends.
            emitted.extend(entry.turns.poll(start + FRAME_SAMPLES as u64));
        }

        for span in emitted {
            self.write_segment(chunk.session_id, span)?;
        }
        // ---- 0.12.5, sliced turns: begin ---------------------------------
        // Before the partial hook and after the finished turns, for the same
        // reason the partial hook sits where it does: a slice reads the audio
        // of the turn that is STILL open, which is the audio `trim` is about to
        // decide to keep. A slice that fires makes the partial that would have
        // followed it redundant — the words are already on the glass and, this
        // time, on their way to the row — so the two are never both spent on
        // the same second of speech.
        self.maybe_slice(chunk.session_id);
        // ---- 0.12.5, sliced turns: end -----------------------------------
        // ---- 0.11.0, partial turns: begin --------------------------------
        // After the finished turns and before the ring is trimmed: a partial
        // reads the audio of the turn that is STILL open, which is exactly the
        // audio `trim` is about to decide to keep. Never fatal — a provisional
        // caption is the one thing in this file that is allowed to fail
        // silently, because the recording does not depend on it.
        self.maybe_partial(chunk.session_id);
        // ---- 0.11.0, partial turns: end ----------------------------------
        if let Some(s) = self.sessions.get_mut(&chunk.session_id) {
            s.trim();
        }
        Ok(())
    }

    /// The session's `sources.kind`, read once when the session starts.
    ///
    /// `None` reads as "an application", which is the safe answer: the worst
    /// case is a mic turn going through the voicebank like any other voice,
    /// never an app turn being labelled as the user.
    fn session_kind(&self, session_id: i64) -> Option<String> {
        let Ok(store) = self.store.lock() else {
            warn!(
                session_id,
                "store mutex poisoned; treating the session as an application"
            );
            return None;
        };
        match store.session_source_kind(session_id) {
            Ok(kind) => kind,
            Err(e) => {
                warn!(
                    session_id,
                    "could not read the session's source kind: {e:#}"
                );
                None
            }
        }
    }

    // ---- 0.12.1: per-user Discord audio --------------------------------------

    /// The voice a per-user Discord session is pinned to, read once when the
    /// session starts.
    ///
    /// `None` — an unlinked account, a poisoned lock, a session of some other
    /// kind — means "take the ordinary route", which is the safe answer for the
    /// same reason [`Self::session_kind`]'s is: the worst case is a Discord
    /// turn going through the voicebank exactly as it did before this feature
    /// existed, never a turn being pinned to the wrong person.
    fn session_discord_speaker(&self, session_id: i64) -> Option<i64> {
        let Ok(store) = self.store.lock() else {
            warn!(
                session_id,
                "store mutex poisoned; not pinning a per-user Discord session"
            );
            return None;
        };
        match store.discord_session_speaker(session_id) {
            Ok(id) => id,
            Err(e) => {
                warn!(session_id, "could not read the pinned Discord voice: {e:#}");
                None
            }
        }
    }

    /// Should this session's audio be discarded because per-user streams are
    /// carrying the same call *on this instance*?
    ///
    /// Two conditions, and the cheap one is first: there is no router, or
    /// nothing is arriving on it, on almost every buffer this daemon ever
    /// handles. Only past that does anything measure anything.
    fn mixed_discord_is_muted(&mut self, chunk: &AudioChunk) -> bool {
        let Some(peruser) = self.peruser.as_ref() else {
            return false;
        };
        // 0.12.3: not "is anything arriving" but "whose". With two bridges the
        // first question has an answer that is true of the machine and false of
        // the client in front of us, and acting on it is how the official
        // client's call went unrecorded in the first place.
        let live = peruser.live_kinds();
        if !live.any() {
            return false;
        }
        // Cloned out before `discord_side`, which takes `&mut self` to fill its
        // per-session cache.
        let peruser = std::sync::Arc::clone(peruser);
        let Some(bridge) = self.bridge.clone() else {
            return false;
        };
        match self.discord_side(chunk.session_id).clone() {
            // The evidence for "which instance are these streams explaining"
            // is the streams themselves, so they are observed on the way past.
            DiscordSide::PerUser => {
                // A buffer whose stream has already been swept is not evidence
                // about anything: the run it belonged to has stopped arriving,
                // and crediting it to the unscoped bucket would let it explain
                // — and so mute — every Discord client for the rest of the
                // window.
                if let Some(kind) = peruser.client_kind_for_session(chunk.session_id) {
                    bridge.observe_stream(kind, chunk.capture_mono_ns, &chunk.samples);
                }
                false
            }
            DiscordSide::Mixed {
                source,
                instance_key,
            } => {
                bridge.observe_mixed(
                    chunk.session_id,
                    &source,
                    instance_key.as_deref(),
                    chunk.capture_mono_ns,
                    &chunk.samples,
                );
                bridge.is_muted(chunk.session_id, chunk.capture_mono_ns, &live)
            }
            DiscordSide::Neither => false,
        }
    }

    /// Why the instance is muted, for the one line the transition logs.
    fn mute_reason(&self, session_id: i64) -> Option<String> {
        let bridge = self.bridge.as_ref()?;
        let live = self
            .peruser
            .as_ref()
            .map(|p| p.live_kinds())
            .unwrap_or_default();
        bridge
            .verdicts(crate::clock::monotonic_ns(), &live)
            .into_iter()
            .find(|v| v.session_id == session_id)
            .map(|v| v.why)
    }

    /// Which side of the rule this session is on, read once per session.
    fn discord_side(&mut self, session_id: i64) -> &DiscordSide {
        if !self.discord_side.contains_key(&session_id) {
            let key = self.session_source_key(session_id);
            let side = match key.as_deref() {
                Some(k) if k.starts_with("discord:") => DiscordSide::PerUser,
                Some(k) if crate::peruser::is_mixed_discord_source(&self.truth_cfg, k) => {
                    let instance_key = match self.store.lock() {
                        Ok(store) => store.session_instance_key(session_id).unwrap_or(None),
                        Err(_) => None,
                    };
                    DiscordSide::Mixed {
                        source: k.to_string(),
                        instance_key,
                    }
                }
                _ => DiscordSide::Neither,
            };
            self.discord_side.insert(session_id, side);
        }
        self.discord_side
            .get(&session_id)
            .expect("just inserted or already present")
    }

    // ---- end 0.12.1 / 0.12.2 --------------------------------------------------

    fn on_session_end(&mut self, session_id: i64, mono_ns: u64) -> Result<()> {
        let mut final_turns = Vec::new();
        if let Some(session) = self.sessions.get_mut(&session_id) {
            if let Some(span) = session.segmenter.flush() {
                final_turns.extend(session.turns.push(span));
            }
            final_turns.extend(session.turns.flush());
        }
        for span in final_turns {
            self.write_segment(session_id, span)?;
        }
        self.sessions.remove(&session_id);
        // 0.12.1: the two caches that outlive `sessions` on purpose (the mute
        // removes the entry) must not outlive the session itself, or a reused
        // row id would inherit a stale answer.
        self.discord_side.remove(&session_id);
        self.muted_mixed.remove(&session_id);
        // 0.12.2: and the instance's evidence with them. A session row id is
        // reused, and a reused one must not inherit a verdict.
        if let Some(bridge) = self.bridge.as_ref() {
            bridge.forget(session_id);
        }

        let store = self
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("store mutex poisoned"))?;
        store.end_session(session_id, Anchor::at(mono_ns).utc_of(mono_ns))?;
        info!(session_id, "session closed");
        Ok(())
    }

    /// A flap was absorbed on the capture side (0.13.0, `crate::flap`): the
    /// source's node vanished and came back inside the grace window, and
    /// `capture.rs` kept the SAME session id rather than closing it. `mono_ns`
    /// is the reappearance's monotonic stamp — what the resumed stream's next
    /// buffer will be measured against — and `gap_ms` is how long the audio
    /// was actually missing.
    ///
    /// Below the VAD's own silence threshold (`[vad].min_silence_ms`) the gap
    /// reads exactly like an ordinary pause in speech: the session's clock is
    /// re-anchored so the sample cursor does not drift against wall time, but
    /// the turn in progress — and the VAD's LSTM state — is left alone. At or
    /// above the threshold the open turn is discarded the same way any other
    /// capture gap is (`on_audio`'s own re-anchor path, `discard_across_gap`):
    /// splicing across a silence that long would join two turns that were
    /// never one (audit finding #21, and the reason a flap must not become a
    /// second way to trigger that bug).
    fn on_gap(&mut self, session_id: i64, mono_ns: u64, gap_ms: u64) -> Result<()> {
        let Some(entry) = self.sessions.get_mut(&session_id) else {
            // Nothing has produced a buffer for this session since it opened
            // (or since its last flap), so there is no `SessionPipeline` yet
            // to re-anchor. The next `on_audio` builds one fresh, anchored on
            // that buffer's own stamp — there is nothing stale here to fix.
            return Ok(());
        };
        entry.anchor = crate::clock::Anchor {
            mono_ns,
            utc_ns: entry.anchor.utc_of(mono_ns),
        };
        entry.anchor_sample = entry.received;

        let silence_ms = self.cfg.vad.min_silence_ms as u64;
        if gap_ms >= silence_ms {
            let fresh_state = self.vad.new_state();
            entry.discard_across_gap(fresh_state);
            self.stats.gaps_discarded.fetch_add(1, Ordering::Relaxed);
            warn!(
                session_id,
                gap_ms,
                silence_ms,
                "flap gap at or above the silence threshold: discarded the turn in progress"
            );
        } else {
            debug!(
                session_id,
                gap_ms,
                silence_ms,
                "flap gap absorbed under the silence threshold: turn in progress kept"
            );
        }
        Ok(())
    }

    /// One turn, as one row or — with `[identity].split_turns` on — as one row
    /// per piece where the person talking changes (0.12.4).
    ///
    /// The split is decided here, before a file exists, because a piece is an
    /// ordinary turn in every later respect: its own clip, its own row, its own
    /// transcript, its own trip through the identity ladder. Nothing downstream
    /// of this function knows a split happened, and the wire shape does not
    /// change — a split turn is two ordinary segments.
    fn write_segment(&mut self, session_id: i64, span: crate::vad::SegmentSpan) -> Result<()> {
        // Checked again here, not only in `on_audio`: this is the one place a
        // file and a row are created, so this is where "no writes" has to be
        // true no matter which path arrived.
        if self.control.is_paused() {
            return Ok(());
        }
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return Ok(());
        };
        let samples = session.extract(span.start, span.end);
        if samples.is_empty() {
            warn!(session_id, "segment had no buffered audio; dropping");
            return Ok(());
        }

        // ---- 0.12.5, sliced turns ------------------------------------------
        // Everything of this turn that no slice has read. On an unsliced turn
        // — which is 96% of them (FINDINGS §41) — this is the whole span and
        // the three lines below cost a comparison.
        let remainder_from = session.slicer.pending_start(span.start);
        let sliced = remainder_from.is_some();
        let remainder = match remainder_from {
            Some(from) => session.extract(from, span.end),
            None => Vec::new(),
        };
        // ---- end 0.12.5 ------------------------------------------------------

        // ---- 0.12.4: where the speaker changes ----
        //
        // The whole turn, decoded once, and the words handed to each piece by
        // time — never a second decode per piece, which is what would let a
        // word straddling the cut be lost or spelled twice.
        //
        // Off, this is one piece with no words in hand and the decode happens
        // exactly where it always has: inside the analysis leg, after the row
        // exists. That is not an optimisation, it is the guarantee that an
        // install which has not asked for this feature does not get a
        // re-ordered pipeline either.
        //
        // 0.12.5: and never on a turn that was SLICED. Splitting needs a timed
        // decode of the whole turn, which is exactly the decode slicing exists
        // to avoid — doing both would spend the turn twice and throw away the
        // reading already on somebody's screen. The two switches are both
        // off by default; an install that turns on both is told at start-up
        // that its long turns reach the glass early and are not split.
        let plan: Vec<(crate::turnsplit::Piece, Option<String>)> = match self
            .analyzer
            .as_mut()
            .filter(|_| self.cfg.identity.split_turns && !self.control.is_paused() && !sliced)
        {
            Some(a) => match a.plan_split(&samples) {
                Ok(p) => p
                    .pieces
                    .into_iter()
                    .map(|(piece, text)| (piece, Some(text)))
                    .collect(),
                Err(e) => {
                    // A detector that fell over must cost a split, never a
                    // recording. The turn is written whole, as it would have
                    // been with the switch off.
                    warn!(session_id, "could not look for a speaker change: {e:#}");
                    Vec::new()
                }
            },
            None => Vec::new(),
        };
        // ---- 0.12.5: the words a sliced turn already has --------------------
        // Its audio was read piece by piece while the person was still
        // speaking; only the tail nobody reached is decoded here. Handed down
        // as the piece's `said`, so the recogniser runs over this turn's audio
        // exactly once in total — the whole claim the feature makes about its
        // cost — and `said` means the same thing it means for a split piece.
        //
        // A sliced turn always takes this path, INCLUDING when the join comes
        // back empty: every slice's audio has been read and the decoder made
        // nothing of any of it, and falling through to a fresh decode would
        // read the whole turn again to ask a question already answered.
        let joined: Option<String> = sliced.then(|| {
            let tail = self
                .analyzer
                .as_mut()
                .map(|a| a.transcribe_slice(&remainder))
                .unwrap_or_default();
            let so_far = self
                .sessions
                .get(&session_id)
                .map(|s| s.slice_text.clone())
                .unwrap_or_default();
            join_slices(&so_far, &tail).unwrap_or_default()
        });
        // ---- end 0.12.5 -------------------------------------------------------
        let plan = if plan.is_empty() {
            vec![(
                crate::turnsplit::Piece {
                    from: 0,
                    to: samples.len(),
                },
                joined,
            )]
        } else {
            plan
        };
        if plan.len() > 1 {
            info!(
                session_id,
                pieces = plan.len(),
                seconds = samples.len() as f32 / SAMPLE_RATE as f32,
                "the turn changes speaker; writing it as separate rows"
            );
        }
        let mut last = Ok(());
        for (piece, said) in plan {
            last = self.write_piece(session_id, span, &samples, piece, said);
            if last.is_err() {
                break;
            }
        }
        // ---- 0.12.5, sliced turns --------------------------------------------
        // Once per TURN and not per piece: the slicer's state is about the turn
        // that has just ended, and a split turn is several rows of one turn.
        // Any growing row a client is showing was replaced by the `segment`
        // each piece published, by the same `(session, t_start_ns)` key a
        // partial is replaced by.
        if let Some(session) = self.sessions.get_mut(&session_id) {
            session.slicer.reset();
            session.slice_text.clear();
        }
        // ---- end 0.12.5 --------------------------------------------------------
        last
    }

    /// One row: the whole turn, or one piece of a split one.
    fn write_piece(
        &mut self,
        session_id: i64,
        span: crate::vad::SegmentSpan,
        turn: &[f32],
        piece: crate::turnsplit::Piece,
        said: Option<String>,
    ) -> Result<()> {
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return Ok(());
        };
        let samples = turn[piece.from..piece.to].to_vec();
        if samples.is_empty() {
            return Ok(());
        }

        let t_start_ns = session.utc_of_sample(span.start + piece.from as u64);
        let t_end_ns = session.utc_of_sample(span.start + piece.to as u64);
        session.segment_seq += 1;
        let is_mic = session.is_mic;
        let is_room = session.is_room;
        let discord_speaker = session.discord_speaker;
        let rel = segment_path(session_id, session.segment_seq, t_start_ns);
        let abs = self.data_dir.join(&rel);

        write_wav(&abs, &samples).with_context(|| format!("writing {}", abs.display()))?;

        let rel_str = rel.to_string_lossy().to_string();
        let segment_id = {
            let store = self
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("store mutex poisoned"))?;
            store.insert_segment(session_id, t_start_ns, t_end_ns, &rel_str, utc_now_ns())?
        };
        self.stats.segments_written.fetch_add(1, Ordering::Relaxed);
        if is_room {
            self.stats.room_segments.fetch_add(1, Ordering::Relaxed);
        }
        self.stats
            .last_segment_ns
            .store(utc_now_ns(), Ordering::Relaxed);
        info!(
            session_id,
            seconds = samples.len() as f32 / SAMPLE_RATE as f32,
            path = %rel_str,
            "segment stored"
        );

        // The one voice this daemon can be certain about. Resolved per segment
        // rather than cached: `ensure_you_speaker` follows a merge, so a user
        // who merges "You" into their named voice keeps the pin pointing at the
        // row that actually holds the rows.
        let you = if is_mic {
            match self.store.lock() {
                Ok(store) => match store.ensure_you_speaker(utc_now_ns()) {
                    Ok(id) => Some(id),
                    Err(e) => {
                        // Refuse rather than fall through: a mic turn that took
                        // the matching path would let the user's own voice mint
                        // or absorb a stranger's identity.
                        error!(segment_id, "could not pin the microphone speaker: {e:#}");
                        None
                    }
                },
                Err(_) => None,
            }
        } else {
            None
        };

        // Segments other than this one that the analysis leg changed: proximity
        // inheritance names the turn *before* this one, and a view that never
        // heard about it would keep showing "unknown voice" until it re-queried.
        let mut also_changed: Vec<i64> = Vec::new();
        // Pause re-checked between the transcript write and the analysis leg
        // (audit finding #5): this body can run for seconds, and a pause that
        // lands inside it must stop the enrollment artifacts — prototypes and
        // retention-EXEMPT goldens — even though the turn itself completed
        // before the pause and its transcript may stand. The window between
        // checks shrinks from "the whole body" to "one model call".
        let paused_mid_write = self.control.is_paused();
        if let Some(analyzer) = self.analyzer.as_mut().filter(|_| !paused_mid_write) {
            also_changed = match you {
                Some(speaker_id) => analyse_mic_or_log(
                    analyzer,
                    &self.store,
                    &self.analysis_stats,
                    segment_id,
                    &samples,
                    &MicEnroll {
                        speaker_id,
                        data_dir: &self.data_dir,
                        max_goldens: self.cfg.mic.max_goldens,
                    },
                    said,
                    t_start_ns,
                ),
                None if is_mic => Vec::new(),
                // 0.12.1: one Discord user's own stream. The ladder is skipped
                // entirely — there is nothing for it to decide — but the turn is
                // still transcribed, still embedded, and still a candidate for
                // enrolment, which is the point: this is the cleanest material
                // the voicebank will ever be offered for anybody who is not the
                // user, and it is the only path that produces it.
                None if discord_speaker.is_some() => analyse_pinned_or_log(
                    analyzer,
                    &self.store,
                    &self.analysis_stats,
                    segment_id,
                    &samples,
                    &PinnedLeg {
                        speaker_id: discord_speaker.expect("matched Some just above"),
                        label_via: crate::store::label_via::DISCORD_STREAM,
                        // No goldens for anybody but the user: a golden outlives
                        // retention on the strength of being the user's own
                        // voice, and that argument does not transfer.
                        goldens: None,
                        // `[truth].enrol` and nothing else. The audio is proof
                        // of WHOSE voice it is; it is not permission to write to
                        // the voicebank, and that switch is where that
                        // permission has lived since 0.9.0.
                        enrol: self.truth_cfg.enrol,
                    },
                    said,
                    t_start_ns,
                ),
                None => analyse_or_log(
                    analyzer,
                    &self.store,
                    &self.analysis_stats,
                    segment_id,
                    &samples,
                    said,
                    t_start_ns,
                ),
            };
        } else if let Some((speaker_id, via)) = you
            .map(|s| (s, crate::store::label_via::MIC))
            .or(discord_speaker.map(|s| (s, crate::store::label_via::DISCORD_STREAM)))
        {
            // No models loaded. The label is provenance, not inference, so it
            // is still true — and stamping it here is what makes a mic capture,
            // or a per-user Discord stream, useful on a machine that has not
            // fetched the model set yet.
            let store = self
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("store mutex poisoned"))?;
            store.set_segment_speaker_via(segment_id, Some(speaker_id), None, Some(via))?;
        }
        // The text vector, after the transcript exists and before the event
        // goes out. On THIS thread on purpose: it is the deprioritised worker
        // the ASR and identity legs already run on, one forward pass is ~2 ms
        // against ASR's tens, and doing it anywhere else would either put model
        // work on the capture thread or need a second copy of the weights. A
        // failure costs a search result, never a recording.
        if let Some(leg) = self.semantic.clone() {
            let store = self
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("store mutex poisoned"))?;
            if let Err(e) = leg.embed_segment(&store, segment_id) {
                warn!(
                    segment_id,
                    "could not embed a segment for semantic search: {e:#}"
                );
            }
        }

        // Published after analysis so the event carries the transcript and the
        // speaker, not an empty shell a client would have to re-query for.
        {
            let store = self
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("store mutex poisoned"))?;
            // Which conversation this turn belongs to (GRAPH.md Tier 1).
            // Between the analysis and the broadcast on purpose: the speaker
            // is what the rule reads, and the event has to carry the thread or
            // every client would have to re-query to draw one separator. A
            // threading failure is logged and the turn keeps its transcript —
            // a derived index is never allowed to cost a recording.
            if !paused_mid_write
                && let Err(e) = crate::threads::assign(&store, &self.cfg.graph, segment_id)
            {
                warn!(segment_id, "could not thread a segment: {e:#}");
            }
            // The conversational language prior (0.7.7, `crate::langctx`).
            //
            // AFTER threading and not with the rest of the analysis leg,
            // because the thing it reads is the thread: "everything was German
            // before" is a fact about a conversation, and until `assign` has
            // run this turn is not in one. Before the broadcast, so the event
            // carries the corrected text rather than the flip.
            //
            // Best-effort, like threading and the extractors either side of it:
            // a language it could not settle never costs a recording.
            if !paused_mid_write && let Some(analyzer) = self.analyzer.as_mut() {
                match analyzer.apply_language_context(&store, segment_id, &samples) {
                    Ok(fix) => self.analysis_stats.record_context(fix.as_ref()),
                    Err(e) => warn!(segment_id, "the language prior failed: {e:#}"),
                }
            }
            // Tier 2 (GRAPH.md): the deterministic extractors, always on and
            // never optional. A handful of regex scans over one line of text,
            // so it belongs here for the same reason threading does — the
            // annotations are already there when a client reads the turn, and
            // no pass has to have run.
            //
            // After threading, because a promise needs a counterparty and the
            // thread is where the counterparty is. Failures are logged and
            // dropped: a derived annotation never costs a recording.
            if !paused_mid_write
                && let Err(e) = crate::commitment::extract(&store, segment_id, utc_now_ns())
            {
                warn!(segment_id, "tier-2 extraction failed: {e:#}");
            }
            // Notes to self (0.8.0). One call, and every guard is inside it:
            // microphone only, wake phrase at the head of the turn, and a
            // failure that is logged rather than raised — a note is an
            // annotation and an annotation never costs a recording.
            crate::notes::maybe_capture(
                &store,
                &self.bus,
                segment_id,
                is_mic && !paused_mid_write,
                utc_now_ns(),
            );
            // ---- 0.11.0: the live translation hook ----
            //
            // BEFORE the broadcast, so a language this turn was only guessed at
            // is already stamped on the row the event is read back from — a
            // client must not see the same segment twice with two different
            // language codes. One call, everything inside it: the switch, the
            // word floor, the guess, the reader's own languages. It queues an
            // id and rings a bell; the model call happens on the assistant's
            // thread, never on this one.
            // Same guard as the note hook above: a pause that landed mid-write
            // queues nothing (the drain re-checks pause too — this is the
            // consistency, not the safety).
            if !paused_mid_write {
                crate::translate::queue_live(&store, segment_id);
            }
            // ---- end hook ----
            publish_segment(&self.bus, &store, segment_id);
            for id in also_changed {
                publish_segment(&self.bus, &store, id);
            }
        }
        // ---- 0.11.0, partial turns: begin ------------------------------------
        // The turn is written and announced, so any provisional row a client is
        // showing for it is replaced by matching `(session, t_start_ns)`. Here
        // rather than before the broadcast, so the replacement can never be
        // published before the thing that replaces it.
        self.close_partial_turn(session_id, segment_id, t_end_ns);
        // ---- 0.11.0, partial turns: end --------------------------------------
        // 0.12.5: the slicer is NOT reset here. This is one PIECE, and a split
        // turn is several pieces of one turn; the state is about the turn, so
        // `write_segment` clears it once, after the last piece.
        Ok(())
    }
}

// ---- 0.11.0, partial turns: begin -----------------------------------------
//
// Everything the feature adds to this file that is longer than a line, kept in
// its own two blocks rather than threaded through the ones above. The rest of
// pipeline.rs sees five call sites and one field.

impl SessionPipeline {
    /// Where the turn currently being spoken begins, or `None` when nobody is
    /// talking.
    ///
    /// Two sources, because a turn is open in two different senses. The
    /// SEGMENTER is mid-speech from the first voiced frame — that is the whole
    /// case captions exist for and the merger has not heard of it yet. The
    /// MERGER holds a closed span for up to `turn_merge_gap` waiting to see
    /// whether the person carries on. Whichever starts earlier is where the
    /// turn a client is being shown actually began, so that is the start a
    /// partial decodes from and the `t_start_ns` it is replaced by.
    fn open_turn_start(&self) -> Option<u64> {
        match (self.turns.pending_start(), self.segmenter.speech_open()) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
        }
    }
}

impl Pipeline {
    /// Offer a provisional caption for whatever is being said on this session
    /// right now (docs/PROTOCOL.md "0.11.0 — partial turns").
    ///
    /// Returns nothing and raises nothing on purpose. Every branch below is a
    /// reason to do less work, and the most expensive thing this function is
    /// ever allowed to cost is one ASR decode that a client may ignore.
    fn maybe_partial(&mut self, session_id: i64) {
        if self.analyzer.is_none() {
            return;
        }
        // The switch, the pause and the backlog, in the one place they are
        // tested (`partial::may_emit`). Pause is the panic path and is checked
        // here as well as in `on_audio`, for the same reason `write_segment`
        // re-checks it: this is a place words leave the daemon. The backlog
        // rule is the other half — a partial that pushed the inference thread
        // behind would be paying for a caption with dropped audio, and the
        // queue drops the OLDEST buffers, i.e. speech nobody has read yet.
        let paused = self.control.is_paused();
        if !self.may_emit_partial(paused) {
            if paused && let Some(s) = self.sessions.get_mut(&session_id) {
                s.partial.reset();
            }
            return;
        }

        let cadence = crate::partial::Cadence::from_config(&self.cfg.asr);
        let now_ms = crate::clock::monotonic_ns() / 1_000_000;
        let Some(session) = self.sessions.get(&session_id) else {
            return;
        };
        let Some(start) = session.open_turn_start() else {
            return;
        };
        // The VAD's cursor, not `received`: audio past it has not been looked
        // at, so decoding it would put words on screen for speech the daemon
        // has not yet decided is speech.
        let end = session.segmenter.cursor();
        let elapsed_ms = ((end.saturating_sub(start)) * 1000) / SAMPLE_RATE.max(1) as u64;
        let t_start_ns = session.utc_of_sample(start);
        if !session.partial.due(t_start_ns, elapsed_ms, now_ms, cadence) {
            return;
        }
        // The WHOLE open turn, every time. Not the newest chunk: FINDINGS §12
        // measured a short slice decoded alone at 56.7% WER against 20.4% for
        // the same slice with context, so a partial built from the last second
        // would be noise that never improved. Re-reading from the top is what
        // makes the text converge on the final.
        let samples = session.extract(start, end);
        if samples.is_empty() {
            return;
        }
        let (speaker, hint) = session.partial.hint(t_start_ns);
        let source = self.session_source_key(session_id);

        let Some(analyzer) = self.analyzer.as_mut() else {
            return;
        };
        let text = analyzer.transcribe_partial(&samples);
        if crate::asr::normalise_words(&text).is_empty() {
            // The decoder made nothing of it. Not an error and not an event: a
            // caption bar that flashed an empty provisional row every second of
            // a cough is worse than one that waits.
            return;
        }
        // Re-checked after the decode, which is the one place in this function
        // that takes real time: a pause that lands inside it must not be
        // followed by words appearing on a screen.
        if self.control.is_paused() {
            if let Some(s) = self.sessions.get_mut(&session_id) {
                s.partial.reset();
            }
            return;
        }
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return;
        };
        let seq_in_turn = session.partial.mark(t_start_ns, now_ms);
        self.bus.publish_ephemeral(
            Topic::Segments,
            "partial",
            crate::partial::partial_json(
                session_id,
                source.as_deref(),
                speaker,
                hint,
                t_start_ns,
                elapsed_ms,
                &text,
                seq_in_turn,
            ),
        );
    }

    /// The switch, the pause and the backlog — [`crate::partial::may_emit`]
    /// with this daemon's own numbers in it.
    fn may_emit_partial(&self, paused: bool) -> bool {
        crate::partial::may_emit(
            self.cfg.asr.partials,
            paused,
            self.control
                .queue
                .as_ref()
                .map(|q| q.queued_samples())
                .unwrap_or(0),
            SAMPLE_RATE,
            self.cfg.asr.partial_backlog_max_s,
        )
    }

    /// A turn was written and announced: close the provisional row and hand
    /// this turn's identity to the next one as its proximity hint.
    ///
    /// The speaker is read back from the ROW rather than from the analysis
    /// leg's own variables, for the same reason `publish_segment` reads it
    /// back: the row is what every client sees, and a hint that disagreed with
    /// it would make the provisional row and the final one name two people.
    fn close_partial_turn(&mut self, session_id: i64, segment_id: i64, t_end_ns: i64) {
        let speaker = match self.store.lock() {
            Ok(store) => store
                .segment_row(segment_id)
                .ok()
                .flatten()
                .and_then(|r| r.speaker_id),
            Err(_) => None,
        };
        if let Some(session) = self.sessions.get_mut(&session_id) {
            session.partial.turn_closed(t_end_ns, speaker);
        }
    }

    /// The session's source match key, read once and remembered.
    fn session_source_key(&mut self, session_id: i64) -> Option<String> {
        if let Some(session) = self.sessions.get(&session_id)
            && let Some(cached) = &session.source_key
        {
            return cached.clone();
        }
        let key = match self.store.lock() {
            Ok(store) => store.session_source_key(session_id).unwrap_or(None),
            Err(_) => None,
        };
        if let Some(session) = self.sessions.get_mut(&session_id) {
            session.source_key = Some(key.clone());
        }
        key
    }
}

// ---- 0.11.0, partial turns: end -------------------------------------------

// ---- 0.12.5, sliced turns: begin ------------------------------------------

impl Pipeline {
    /// Cut the open turn if it has run long enough and the VAD has just found a
    /// gap, decode the piece, and publish it (docs/PROTOCOL.md "0.12.5 —
    /// sliced turns").
    ///
    /// Returns nothing and raises nothing, exactly like [`Self::maybe_partial`]:
    /// every branch is a reason to do less work, and the most this is ever
    /// allowed to cost is one decode of audio that the finished row was going
    /// to have to read anyway.
    fn maybe_slice(&mut self, session_id: i64) {
        let after_s = self.cfg.captions.slice_after_s;
        if after_s <= 0.0 || self.analyzer.is_none() {
            return;
        }
        // The panic path, checked here as well as in `on_audio` and again after
        // the decode, for the reason `write_segment` re-checks it: this is a
        // place words leave the daemon.
        if self.control.is_paused() {
            if let Some(s) = self.sessions.get_mut(&session_id) {
                s.slicer.reset();
                s.slice_text.clear();
            }
            return;
        }
        // The backlog rule is `partial`'s, and it applies here for a weaker but
        // still real reason. A slice does not add decoding to the turn — the
        // audio is read once either way — but it moves that work EARLIER, and a
        // thread that is already behind should be catching up rather than
        // rendering. The queue drops the oldest audio when it overflows, so the
        // thing a badly-timed caption costs is speech nobody has read yet.
        if crate::partial::backlog_blocks(
            self.control
                .queue
                .as_ref()
                .map(|q| q.queued_samples())
                .unwrap_or(0),
            SAMPLE_RATE,
            self.cfg.asr.partial_backlog_max_s,
        ) {
            return;
        }

        let after = (after_s * SAMPLE_RATE as f32) as u64;
        let Some(session) = self.sessions.get(&session_id) else {
            return;
        };
        let Some(turn_start) = session.open_turn_start() else {
            return;
        };
        // The VAD's cursor and the VAD's dip, never `received`: audio past the
        // cursor has not been scored, so a cut into it would be a cut the model
        // never said was safe.
        let Some(cut) = session.slicer.due(
            turn_start,
            session.segmenter.cursor(),
            session.segmenter.dip_len(),
            after,
        ) else {
            return;
        };
        let samples = session.extract(cut.start, cut.end);
        if samples.is_empty() {
            return;
        }
        let t_start_ns = session.utc_of_sample(turn_start);
        let elapsed_ms = ((cut.end.saturating_sub(turn_start)) * 1000) / SAMPLE_RATE.max(1) as u64;
        let (speaker, hint) = session.partial.hint(t_start_ns);
        let source = self.session_source_key(session_id);

        let Some(analyzer) = self.analyzer.as_mut() else {
            return;
        };
        let text = analyzer.transcribe_slice(&samples);
        // Re-checked after the decode, the one part of this that takes real
        // time: a pause landing inside it must not be followed by words
        // appearing on somebody's screen.
        if self.control.is_paused() {
            if let Some(s) = self.sessions.get_mut(&session_id) {
                s.slicer.reset();
                s.slice_text.clear();
            }
            return;
        }
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return;
        };
        // The cut is marked whatever the decoder made of it — otherwise it
        // would be offered again on the very next frame for as long as the dip
        // lasted — but a slice the decoder made NOTHING of does not count as
        // read: its audio goes back in front of the remainder and is decoded
        // again with the words that follow it. Six seconds of speech the model
        // happened to make nothing of in isolation must not be silently deleted
        // from the transcript.
        let read = !crate::asr::normalise_words(&text).is_empty();
        session.slicer.mark(turn_start, cut, read);
        if !read {
            return;
        }
        session.slice_text = join_slices(&session.slice_text, &text).unwrap_or_default();
        let so_far = session.slice_text.clone();
        let seq = cut.seq;
        self.bus.publish_ephemeral(
            Topic::Segments,
            "slice",
            crate::slice::slice_json(
                session_id,
                source.as_deref(),
                speaker,
                hint,
                t_start_ns,
                elapsed_ms,
                &text,
                &so_far,
                seq,
            ),
        );
    }
}

/// Join one more slice's words onto the ones already read.
///
/// A plain space, and the reason it is a function rather than a `format!` is
/// the two empty cases: a slice the decoder made nothing of must not leave a
/// double space in the middle of a sentence, and a turn whose every slice was
/// silent must produce `None` rather than `Some("")` — an empty transcript is
/// stored as NULL, and `" "` would defeat that and put a blank document in the
/// full-text index.
fn join_slices(so_far: &str, next: &str) -> Option<String> {
    let (a, b) = (so_far.trim(), next.trim());
    let joined = match (a.is_empty(), b.is_empty()) {
        (true, true) => return None,
        (true, false) => b.to_string(),
        (false, true) => a.to_string(),
        (false, false) => format!("{a} {b}"),
    };
    Some(joined)
}

// ---- 0.12.5, sliced turns: end --------------------------------------------

/// Announce a stored segment on the event stream.
///
/// Read back from the row rather than from the pipeline's own variables: the
/// row is what every client will see when it queries, and the event must not
/// disagree with it.
pub fn publish_segment(bus: &Bus, store: &Store, segment_id: i64) {
    match store.segment_row(segment_id) {
        Ok(Some(row)) => {
            bus.publish(
                Topic::Segments,
                "segment",
                crate::service::segment_json(&row),
            );
        }
        Ok(None) => warn!(
            segment_id,
            "a segment vanished between writing and announcing it"
        ),
        Err(e) => warn!(segment_id, "could not read back a stored segment: {e:#}"),
    }
}

/// `segments/<session>/seg-<seq>-<utc_ns>.wav`, relative to the data dir so the
/// whole tree can be relocated without rewriting rows.
pub fn segment_path(session_id: i64, seq: u64, t_start_ns: i64) -> PathBuf {
    PathBuf::from("segments")
        .join(format!("{session_id:06}"))
        .join(format!("seg-{seq:06}-{t_start_ns}.wav"))
}

/// 16 kHz mono, 16-bit PCM. Opus arrives in a later step; for Step 1 a WAV keeps
/// the files trivially readable by every downstream tool.
pub fn write_wav(path: &Path, samples: &[f32]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec)?;
    for s in samples {
        w.write_sample((s.clamp(-1.0, 1.0) * 32767.0) as i16)?;
    }
    w.finalize()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_paths_are_relative_and_sortable() {
        let p = segment_path(7, 3, 1_700_000_000_000_000_000);
        assert!(p.is_relative());
        assert_eq!(
            p,
            PathBuf::from("segments/000007/seg-000003-1700000000000000000.wav")
        );
        // Sequence numbers are zero-padded so lexical order is chronological.
        let a = segment_path(1, 2, 0);
        let b = segment_path(1, 10, 0);
        assert!(a.to_string_lossy() < b.to_string_lossy());
    }

    #[test]
    fn wav_round_trips_through_hound() {
        let dir = std::env::temp_dir().join(format!("nx-recall-wav-{}", std::process::id()));
        let path = dir.join("a").join("b.wav");
        let samples: Vec<f32> = (0..1600)
            .map(|i| ((i as f32) / 1600.0) * 2.0 - 1.0)
            .collect();
        write_wav(&path, &samples).unwrap();

        let mut r = hound::WavReader::open(&path).unwrap();
        assert_eq!(r.spec().sample_rate, SAMPLE_RATE);
        assert_eq!(r.spec().channels, 1);
        let back: Vec<f32> = r
            .samples::<i16>()
            .map(|s| s.unwrap() as f32 / 32767.0)
            .collect();
        assert_eq!(back.len(), samples.len());
        for (a, b) in samples.iter().zip(&back) {
            assert!((a - b).abs() < 1e-3);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sample_index_maps_to_utc_through_the_anchor() {
        let mut s = SessionPipeline::new(
            VadState_stub(),
            SegmenterConfig::from_ms(0.5, 250, 500, 200, 30_000, SAMPLE_RATE),
            TurnMerger::new(24_000, 480_000),
            0,
            false,
            false,
            None,
        );
        s.anchor = Anchor {
            mono_ns: 0,
            utc_ns: 1_700_000_000_000_000_000,
        };
        s.anchor_sample = 0;
        assert_eq!(s.utc_of_sample(0), 1_700_000_000_000_000_000);
        assert_eq!(s.utc_of_sample(16_000), 1_700_000_001_000_000_000);
        assert_eq!(s.utc_of_sample(8_000), 1_700_000_000_500_000_000);
    }

    // The VAD state is opaque; tests that only exercise clock arithmetic borrow
    // a real one from the bundled model.
    #[allow(non_snake_case)]
    fn VadState_stub() -> VadState {
        SileroVad::from_bytes(crate::VAD_MODEL).unwrap().new_state()
    }

    #[test]
    fn re_anchoring_absorbs_a_gap_instead_of_shifting_later_segments() {
        let mut s = SessionPipeline::new(
            VadState_stub(),
            SegmenterConfig::from_ms(0.5, 250, 500, 200, 30_000, SAMPLE_RATE),
            TurnMerger::new(24_000, 480_000),
            0,
            false,
            false,
            None,
        );
        s.anchor = Anchor {
            mono_ns: 0,
            utc_ns: 1_000_000_000,
        };
        s.anchor_sample = 0;
        s.received = 16_000; // 1 s of audio consumed

        // A buffer that arrives 5 s later than the sample count predicts means
        // 4 s of audio never reached us.
        s.maybe_reanchor(5_000_000_000);
        assert_eq!(s.anchor_sample, 16_000);
        // Sample 16_000 now reads as 5 s of wall time after the session start.
        assert_eq!(s.utc_of_sample(16_000), 1_000_000_000 + 5_000_000_000);
    }

    #[test]
    fn small_jitter_does_not_re_anchor() {
        let mut s = SessionPipeline::new(
            VadState_stub(),
            SegmenterConfig::from_ms(0.5, 250, 500, 200, 30_000, SAMPLE_RATE),
            TurnMerger::new(24_000, 480_000),
            0,
            false,
            false,
            None,
        );
        s.anchor = Anchor {
            mono_ns: 0,
            utc_ns: 1_000_000_000,
        };
        s.received = 16_000;
        s.maybe_reanchor(1_010_000_000); // 10 ms early/late
        assert_eq!(s.anchor_sample, 0);
        assert_eq!(s.anchor.mono_ns, 0);
    }

    /// Audit finding #21. A queue drop mid-turn used to be *only* a clock
    /// problem: the anchor moved, and the audio on either side of the hole
    /// stayed in one open turn. What came out was a single segment splicing
    /// pre-gap words onto post-gap words at a shifted timestamp — wrong
    /// transcript, jump-cut clip, and an embedding taken across two disjoint
    /// pieces of speech, which is how a voice that never existed gets minted.
    #[test]
    fn a_gap_discards_the_turn_in_progress_instead_of_splicing_across_it() {
        let cfg = SegmenterConfig::from_ms(0.5, 250, 500, 200, 30_000, SAMPLE_RATE);
        let mut s = SessionPipeline::new(
            VadState_stub(),
            cfg,
            TurnMerger::new(24_000, 480_000),
            0,
            false,
            false,
            None,
        );
        s.anchor = Anchor {
            mono_ns: 0,
            utc_ns: 1_000_000_000,
        };
        s.received = 16_000;
        s.ring = vec![0.25; 16_000];
        s.ring_base = 0;
        // Somebody is mid-sentence: a turn is open and pinning the ring back to
        // where they started speaking.
        let _ = s.turns.push(crate::vad::SegmentSpan {
            start: 1_000,
            end: 15_000,
            voiced_start: 1_200,
            voiced_end: 14_800,
        });
        assert_eq!(s.turns.pending_start(), Some(1_000));

        // Four seconds of audio never arrived.
        assert!(
            s.maybe_reanchor(5_000_000_000),
            "a gap this size is a gap, and the caller has to be told"
        );
        s.discard_across_gap(VadState_stub());

        assert_eq!(
            s.turns.pending_start(),
            None,
            "the half-turn from before the hole is gone, not waiting to be joined"
        );
        assert!(s.ring.is_empty(), "and so is the audio under it");
        assert_eq!(
            s.ring_base, s.received,
            "the ring restarts where the session's sample counter actually is"
        );
        assert_eq!(
            s.segmenter.cursor(),
            s.received,
            "the segmenter agrees, so no frame is read from before the gap"
        );
        assert!(
            s.extract(1_000, 15_000).is_empty(),
            "nothing from before the gap can reach a segment any more"
        );
        // The clock still absorbed the gap: later turns are not shifted.
        assert_eq!(s.anchor_sample, 16_000);
        assert_eq!(s.utc_of_sample(16_000), 6_000_000_000);

        // Ordinary jitter is not a gap and takes nothing away.
        let mut fine = SessionPipeline::new(
            VadState_stub(),
            cfg,
            TurnMerger::new(24_000, 480_000),
            0,
            false,
            false,
            None,
        );
        fine.received = 16_000;
        fine.ring = vec![0.25; 16_000];
        let _ = fine.turns.push(crate::vad::SegmentSpan {
            start: 1_000,
            end: 15_000,
            voiced_start: 1_200,
            voiced_end: 14_800,
        });
        assert!(!fine.maybe_reanchor(1_010_000_000));
        assert_eq!(fine.turns.pending_start(), Some(1_000));
        assert_eq!(fine.ring.len(), 16_000);
    }

    // ---- 0.11.0, partial turns: begin --------------------------------------

    #[test]
    fn an_open_turn_is_visible_from_the_first_voiced_frame() {
        // The whole case captions exist for. The turn merger hears nothing
        // until the segmenter has CLOSED a span, which needs 500 ms of silence
        // — so mid-sentence, `pending_start` is None and the segmenter is the
        // only thing that knows somebody is talking.
        let cfg = SegmenterConfig::from_ms(0.5, 250, 500, 200, 30_000, SAMPLE_RATE);
        let mut s = SessionPipeline::new(
            VadState_stub(),
            cfg,
            TurnMerger::new(24_000, 480_000),
            0,
            false,
            false,
            None,
        );
        assert_eq!(s.open_turn_start(), None, "nobody is talking");

        // Speech opens at sample 16_000; the pad reaches back 200 ms.
        for i in 0..40u64 {
            s.segmenter
                .push_frame(0.9, 16_000 + i * FRAME_SAMPLES as u64, FRAME_SAMPLES as u64);
        }
        assert_eq!(
            s.open_turn_start(),
            Some(16_000 - 3_200),
            "a turn is open the moment the VAD says so, not when it ends"
        );

        // Now with a turn ALSO pending in the merger — the previous span,
        // waiting to see whether this is a continuation. The earlier of the two
        // is where the turn a client is being shown actually began.
        let _ = s.turns.push(crate::vad::SegmentSpan {
            start: 4_000,
            end: 12_000,
            voiced_start: 4_200,
            voiced_end: 11_800,
        });
        assert_eq!(s.open_turn_start(), Some(4_000));
    }

    #[test]
    fn a_gap_takes_the_provisional_row_with_the_turn_it_described() {
        // There is no `segment` coming for a discarded turn, so nothing would
        // ever replace the words on screen — and the NEXT turn must not inherit
        // an identity from a turn that never landed.
        let cfg = SegmenterConfig::from_ms(0.5, 250, 500, 200, 30_000, SAMPLE_RATE);
        let mut s = SessionPipeline::new(
            VadState_stub(),
            cfg,
            TurnMerger::new(24_000, 480_000),
            0,
            false,
            false,
            None,
        );
        s.received = 16_000;
        s.ring = vec![0.25; 16_000];
        s.partial.turn_closed(500_000_000, Some(7));
        s.partial.mark(1_000_000_000, 10_000);
        assert!(s.partial.is_open());

        s.discard_across_gap(VadState_stub());
        assert!(!s.partial.is_open(), "the open turn is forgotten");
        // Who spoke BEFORE the discarded turn is still a fact about the session,
        // so the proximity hint survives — it is the turn that vanished, not the
        // history.
        assert_eq!(
            s.partial.hint(1_100_000_000),
            (Some(7), Some("proximity")),
            "the gap discarded a turn, not the session's memory"
        );
    }

    // ---- 0.11.0, partial turns: end ----------------------------------------

    #[test]
    fn ring_extraction_is_clamped_to_what_is_buffered() {
        let mut s = SessionPipeline::new(
            VadState_stub(),
            SegmenterConfig::from_ms(0.5, 250, 500, 200, 30_000, SAMPLE_RATE),
            TurnMerger::new(24_000, 480_000),
            0,
            false,
            false,
            None,
        );
        s.ring = (0..100).map(|i| i as f32).collect();
        s.ring_base = 1000;
        assert_eq!(s.extract(1000, 1005), vec![0.0, 1.0, 2.0, 3.0, 4.0]);
        // Past the end: clamped, not out of bounds.
        assert_eq!(s.extract(1095, 2000).len(), 5);
        // Before the base: the trimmed prefix is simply unavailable.
        assert!(s.extract(0, 500).is_empty());
    }

    // ---- 0.12.5, sliced turns: begin ---------------------------------------

    #[test]
    fn slices_join_with_one_space() {
        assert_eq!(
            join_slices(
                "so the way the portal network works",
                "is that every instance"
            ),
            Some("so the way the portal network works is that every instance".into())
        );
    }

    #[test]
    fn a_silent_slice_does_not_leave_a_hole_in_the_sentence() {
        // The decoder makes nothing of a slice that was all breath. Joining it
        // anyway would put a double space in the middle of a transcript, and
        // the archive is a record somebody reads.
        assert_eq!(
            join_slices("the door behind the bar", ""),
            Some("the door behind the bar".into())
        );
        assert_eq!(
            join_slices("", "the door behind the bar"),
            Some("the door behind the bar".into())
        );
        assert_eq!(join_slices("  ", "  the door  "), Some("the door".into()));
    }

    #[test]
    fn a_turn_whose_every_slice_was_silent_has_no_words_rather_than_a_space() {
        // `Some("")` would be written into `segments.text` and put a blank
        // document in the full-text index; an empty transcript is stored as
        // NULL, and "has a transcript" stays a single IS NOT NULL.
        assert_eq!(join_slices("", ""), None);
        assert_eq!(join_slices("  ", "\n\t "), None);
    }

    #[test]
    fn the_open_turn_is_forgotten_when_a_gap_throws_its_audio_away() {
        // A turn half of whose audio is gone is not a turn (audit finding #21),
        // and the words already read from the near side of the hole must not be
        // spliced onto the far side of it.
        let mut s = SessionPipeline::new(
            VadState_stub(),
            SegmenterConfig::from_ms(0.5, 250, 500, 200, 30_000, SAMPLE_RATE),
            TurnMerger::new(24_000, 480_000),
            0,
            false,
            false,
            None,
        );
        let cut = crate::slice::Cut {
            start: 0,
            end: 96_000,
            seq: 0,
        };
        s.slicer.mark(0, cut, true);
        s.slice_text = "words from before the hole".into();
        assert!(s.slicer.is_open());

        s.discard_across_gap(VadState_stub());
        assert!(
            !s.slicer.is_open(),
            "the open row survived a discarded turn"
        );
        assert!(
            s.slice_text.is_empty(),
            "pre-gap words survived to be spliced onto the next turn"
        );
        assert_eq!(s.slicer.pending_start(0), None);
    }

    // ---- 0.12.5, sliced turns: end -----------------------------------------

    // ---- 0.13.0, flap tolerance's other half: `Pipeline::on_gap` ----------

    /// A whole `Pipeline`, wired exactly like the daemon's but with no models
    /// configured — `on_gap` never touches the analysis leg, so there is
    /// nothing here that needs one.
    fn test_pipeline(name: &str) -> (Pipeline, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("nx-recall-flap-gap-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(std::sync::Mutex::new(Store::open(&dir).unwrap()));
        let cfg = Config::default();
        let control = Control::new(dir.clone(), None, &cfg.allowlist());
        let bus = Bus::new(16, 16);
        let pipeline = Pipeline::new(
            &cfg,
            store,
            dir.clone(),
            Arc::new(Stats::default()),
            Arc::new(AnalysisStats::default()),
            control,
            bus,
        )
        .expect("a Pipeline with no models configured must still build");
        (pipeline, dir)
    }

    fn open_session(pipeline: &mut Pipeline, session_id: i64) {
        pipeline.sessions.insert(
            session_id,
            SessionPipeline::new(
                VadState_stub(),
                SegmenterConfig::from_ms(0.5, 250, 500, 200, 30_000, SAMPLE_RATE),
                TurnMerger::new(24_000, 480_000),
                0,
                false,
                false,
                None,
            ),
        );
    }

    #[test]
    fn a_flap_gap_under_the_silence_threshold_re_anchors_but_keeps_the_turn() {
        let (mut pipeline, dir) = test_pipeline("under");
        let session_id = 1;
        open_session(&mut pipeline, session_id);
        let s = pipeline.sessions.get_mut(&session_id).unwrap();
        s.anchor = Anchor {
            mono_ns: 0,
            utc_ns: 1_000_000_000,
        };
        s.anchor_sample = 0;
        s.received = 16_000; // 1 s already consumed
        s.ring = vec![0.25; 1_600]; // some buffered audio, standing in for "a turn in progress"
        s.ring_base = 0;

        // 500 ms is the default [vad].min_silence_ms; 200 ms must not discard.
        assert_eq!(pipeline.cfg.vad.min_silence_ms, 500);
        pipeline.on_gap(session_id, 5_200_000_000, 200).unwrap();

        let s = pipeline.sessions.get(&session_id).unwrap();
        assert_eq!(
            s.ring.len(),
            1_600,
            "a gap under the silence threshold must not throw the open turn away"
        );
        assert_eq!(
            s.anchor_sample, 16_000,
            "the clock must still re-anchor so the sample cursor does not drift"
        );
        assert_eq!(s.utc_of_sample(16_000), 1_000_000_000 + 5_200_000_000);
        assert_eq!(
            pipeline.stats.gaps_discarded.load(Ordering::Relaxed),
            0,
            "nothing was discarded"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_flap_gap_at_or_above_the_silence_threshold_discards_the_open_turn() {
        let (mut pipeline, dir) = test_pipeline("over");
        let session_id = 2;
        open_session(&mut pipeline, session_id);
        let s = pipeline.sessions.get_mut(&session_id).unwrap();
        s.anchor = Anchor {
            mono_ns: 0,
            utc_ns: 1_000_000_000,
        };
        s.anchor_sample = 0;
        s.received = 16_000;
        s.ring = vec![0.25; 1_600];
        s.ring_base = 0;

        // Exactly at the threshold counts as "at or above": the caller could
        // otherwise flap forever one millisecond under a real silence and
        // never once trip the discard.
        pipeline.on_gap(session_id, 5_500_000_000, 500).unwrap();

        let s = pipeline.sessions.get(&session_id).unwrap();
        assert!(
            s.ring.is_empty(),
            "a gap at the silence threshold must discard the turn in progress, same as any other gap"
        );
        assert_eq!(pipeline.stats.gaps_discarded.load(Ordering::Relaxed), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_gap_for_a_session_with_no_open_pipeline_yet_is_a_no_op() {
        // The resumed stream has not produced a buffer yet, so there is no
        // `SessionPipeline` to re-anchor. The next `on_audio` starts one fresh.
        let (mut pipeline, dir) = test_pipeline("no-session");
        assert!(pipeline.on_gap(999, 1_000, 50).is_ok());
        assert!(!pipeline.sessions.contains_key(&999));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- end 0.13.0 ---------------------------------------------------------
}
