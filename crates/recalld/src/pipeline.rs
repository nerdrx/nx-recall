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

use crate::clock::{Anchor, samples_to_ns, utc_now_ns};
use crate::config::{Config, SAMPLE_RATE};
use crate::queue::{AudioChunk, CaptureEvent, EventQueue};
use crate::store::Store;
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
    /// Rolling audio buffer; `ring[0]` is absolute sample `ring_base`.
    ring: Vec<f32>,
    ring_base: u64,
    /// Total samples ever accepted for this session.
    received: u64,
    /// Clock anchor for converting sample indices to UTC.
    anchor: Anchor,
    anchor_sample: u64,
    segment_seq: u64,
}

impl SessionPipeline {
    fn new(vad_state: VadState, seg_cfg: SegmenterConfig, first_chunk_mono_ns: u64) -> Self {
        Self {
            vad_state,
            segmenter: crate::vad::Segmenter::new(seg_cfg),
            ring: Vec::new(),
            ring_base: 0,
            received: 0,
            anchor: Anchor::at(first_chunk_mono_ns),
            anchor_sample: 0,
            segment_seq: 0,
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
    fn maybe_reanchor(&mut self, chunk_mono_ns: u64) {
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
        }
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
        let keep_from = self.segmenter.retain_from();
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
}

pub struct Pipeline {
    vad: SileroVad,
    seg_cfg: SegmenterConfig,
    store: Arc<std::sync::Mutex<Store>>,
    data_dir: PathBuf,
    sessions: HashMap<i64, SessionPipeline>,
    stats: Arc<Stats>,
}

impl Pipeline {
    pub fn new(
        cfg: &Config,
        store: Arc<std::sync::Mutex<Store>>,
        data_dir: PathBuf,
        stats: Arc<Stats>,
    ) -> Result<Self> {
        let vad = SileroVad::from_bytes(crate::VAD_MODEL)?;
        let seg_cfg = SegmenterConfig::from_ms(
            cfg.vad.threshold,
            cfg.vad.min_speech_ms,
            cfg.vad.min_silence_ms,
            cfg.vad.pad_ms,
            cfg.vad.max_segment_ms,
            SAMPLE_RATE,
        );
        Ok(Self {
            vad,
            seg_cfg,
            store,
            data_dir,
            sessions: HashMap::new(),
            stats,
        })
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
        let seg_cfg = self.seg_cfg;
        let new_state = self.vad.new_state();
        let entry = self
            .sessions
            .entry(chunk.session_id)
            .or_insert_with(|| SessionPipeline::new(new_state, seg_cfg, chunk.capture_mono_ns));

        entry.maybe_reanchor(chunk.capture_mono_ns);
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
                emitted.push(span);
            }
        }

        for span in emitted {
            self.write_segment(chunk.session_id, span)?;
        }
        if let Some(s) = self.sessions.get_mut(&chunk.session_id) {
            s.trim();
        }
        Ok(())
    }

    fn on_session_end(&mut self, session_id: i64, mono_ns: u64) -> Result<()> {
        if let Some(session) = self.sessions.get_mut(&session_id)
            && let Some(span) = session.segmenter.flush()
        {
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
        let rel = segment_path(session_id, session.segment_seq, t_start_ns);
        let abs = self.data_dir.join(&rel);

        write_wav(&abs, &samples).with_context(|| format!("writing {}", abs.display()))?;

        let rel_str = rel.to_string_lossy().to_string();
        {
            let store = self
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("store mutex poisoned"))?;
            store.insert_segment(session_id, t_start_ns, t_end_ns, &rel_str, utc_now_ns())?;
        }
        self.stats.segments_written.fetch_add(1, Ordering::Relaxed);
        info!(
            session_id,
            seconds = samples.len() as f32 / SAMPLE_RATE as f32,
            path = %rel_str,
            "segment stored"
        );
        Ok(())
    }
}

/// `segments/<session>/seg-<seq>-<utc_ns>.wav`, relative to the data dir so the
/// whole tree can be relocated without rewriting rows.
fn segment_path(session_id: i64, seq: u64, t_start_ns: i64) -> PathBuf {
    PathBuf::from("segments")
        .join(format!("{session_id:06}"))
        .join(format!("seg-{seq:06}-{t_start_ns}.wav"))
}

/// 16 kHz mono, 16-bit PCM. Opus arrives in a later step; for Step 1 a WAV keeps
/// the files trivially readable by every downstream tool.
fn write_wav(path: &Path, samples: &[f32]) -> Result<()> {
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
            0,
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
            0,
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
            0,
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

    #[test]
    fn ring_extraction_is_clamped_to_what_is_buffered() {
        let mut s = SessionPipeline::new(
            VadState_stub(),
            SegmenterConfig::from_ms(0.5, 250, 500, 200, 30_000, SAMPLE_RATE),
            0,
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
