//! The inference side: drain the capture queue, run VAD, write segments.
//!
//! Everything here runs on one deliberately deprioritised thread. Step 0's
//! field measurement put the whole analysis pipeline under 5% of one core, so
//! throughput is not the concern — scheduling is. The failure this guards
//! against is analysis work winning a timeslice from a VR frame.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use tracing::{debug, error, info, warn};

use crate::analysis::{AnalysisStats, Analyzer, MicEnroll, analyse_mic_or_log, analyse_or_log};
use crate::bus::{Bus, Topic};
use crate::clock::{Anchor, samples_to_ns, utc_now_ns};
use crate::config::{Config, SAMPLE_RATE};
use crate::control::Control;
use crate::models::ModelSet;
use crate::queue::{AudioChunk, CaptureEvent, EventQueue};
use crate::store::{KIND_MIC, Store};
use crate::turns::TurnMerger;
use crate::vad::{FRAME_SAMPLES, SegmenterConfig, SileroVad, VadState};

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
}

impl SessionPipeline {
    fn new(
        vad_state: VadState,
        seg_cfg: SegmenterConfig,
        turns: TurnMerger,
        first_chunk_mono_ns: u64,
        is_mic: bool,
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
        })
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

        let seg_cfg = self.seg_cfg;
        let new_state = self.vad.new_state();
        let merger = crate::ingest::turn_merger(&self.cfg);
        // One row read per session, not per buffer. A lookup that fails reads
        // as "an application", which is the safe answer: the worst case is a
        // mic turn going through the voicebank like any other voice, never an
        // app turn being labelled as the user.
        let is_mic = if self.sessions.contains_key(&chunk.session_id) {
            false // unused; the entry already exists
        } else {
            self.session_is_mic(chunk.session_id)
        };
        let fresh_state = self.vad.new_state();
        let entry = self.sessions.entry(chunk.session_id).or_insert_with(|| {
            SessionPipeline::new(new_state, seg_cfg, merger, chunk.capture_mono_ns, is_mic)
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
        if let Some(s) = self.sessions.get_mut(&chunk.session_id) {
            s.trim();
        }
        Ok(())
    }

    fn session_is_mic(&self, session_id: i64) -> bool {
        let Ok(store) = self.store.lock() else {
            warn!(
                session_id,
                "store mutex poisoned; treating the session as an application"
            );
            return false;
        };
        match store.session_source_kind(session_id) {
            Ok(kind) => kind.as_deref() == Some(KIND_MIC),
            Err(e) => {
                warn!(
                    session_id,
                    "could not read the session's source kind: {e:#}"
                );
                false
            }
        }
    }

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

        let store = self
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("store mutex poisoned"))?;
        store.end_session(session_id, Anchor::at(mono_ns).utc_of(mono_ns))?;
        info!(session_id, "session closed");
        Ok(())
    }

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

        let t_start_ns = session.utc_of_sample(span.start);
        let t_end_ns = session.utc_of_sample(span.end);
        session.segment_seq += 1;
        let is_mic = session.is_mic;
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
                    t_start_ns,
                ),
                None if is_mic => Vec::new(),
                None => analyse_or_log(
                    analyzer,
                    &self.store,
                    &self.analysis_stats,
                    segment_id,
                    &samples,
                    t_start_ns,
                ),
            };
        } else if let Some(speaker_id) = you {
            // No models loaded. The label is provenance, not inference, so it
            // is still true — and stamping it here is what makes a mic capture
            // useful on a machine that has not fetched the model set yet.
            let store = self
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("store mutex poisoned"))?;
            store.set_segment_speaker_via(
                segment_id,
                Some(speaker_id),
                None,
                Some(crate::store::label_via::MIC),
            )?;
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
            publish_segment(&self.bus, &store, segment_id);
            for id in also_changed {
                publish_segment(&self.bus, &store, id);
            }
        }
        Ok(())
    }
}

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

    #[test]
    fn ring_extraction_is_clamped_to_what_is_buffered() {
        let mut s = SessionPipeline::new(
            VadState_stub(),
            SegmenterConfig::from_ms(0.5, 250, 500, 200, 30_000, SAMPLE_RATE),
            TurnMerger::new(24_000, 480_000),
            0,
            false,
        );
        s.ring = (0..100).map(|i| i as f32).collect();
        s.ring_base = 1000;
        assert_eq!(s.extract(1000, 1005), vec![0.0, 1.0, 2.0, 3.0, 4.0]);
        // Past the end: clamped, not out of bounds.
        assert_eq!(s.extract(1095, 2000).len(), 5);
        // Before the base: the trimmed prefix is simply unavailable.
        assert!(s.extract(0, 500).is_empty());
    }
}
