//! Offline ingestion of a PCM buffer through the same stages as live capture.
//!
//! The daemon feeds the pipeline from PipeWire; this feeds it from memory. Both
//! go through the same `Segmenter`, the same `TurnMerger` and the same
//! `Analyzer`, which is what makes the fixture suite a test of the daemon
//! rather than of a parallel implementation.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;

use crate::analysis::{Analyzer, MicEnroll};
use crate::bus::Bus;
use crate::config::{Config, SAMPLE_RATE};
use crate::control::Control;
use crate::pipeline::{publish_segment, segment_path, write_wav};
use crate::store::{KIND_MIC, Store};
use crate::turns::TurnMerger;
use crate::vad::{FRAME_SAMPLES, SegmentSpan, Segmenter, SegmenterConfig, SileroVad};

pub fn segmenter_config(cfg: &Config) -> SegmenterConfig {
    SegmenterConfig::from_ms(
        cfg.vad.threshold,
        cfg.vad.min_speech_ms,
        cfg.vad.min_silence_ms,
        cfg.vad.pad_ms,
        cfg.vad.max_segment_ms,
        SAMPLE_RATE,
    )
}

pub fn turn_merger(cfg: &Config) -> TurnMerger {
    let samples = |ms: u32| (ms as u64 * SAMPLE_RATE as u64) / 1000;
    TurnMerger::new(
        samples(cfg.vad.turn_merge_gap_ms),
        samples(cfg.vad.max_segment_ms),
    )
}

/// VAD + turn merging over a whole buffer.
pub fn segment_pcm(vad: &mut SileroVad, cfg: &Config, samples: &[f32]) -> Result<Vec<SegmentSpan>> {
    let mut state = vad.new_state();
    let mut seg = Segmenter::new(segmenter_config(cfg));
    let mut merger = turn_merger(cfg);
    let mut turns = Vec::new();

    for (i, frame) in samples.as_chunks::<FRAME_SAMPLES>().0.iter().enumerate() {
        let prob = vad.frame(frame, &mut state)?;
        let start = (i * FRAME_SAMPLES) as u64;
        if let Some(span) = seg.push_frame(prob, start, FRAME_SAMPLES as u64) {
            turns.extend(merger.push(span));
        }
        turns.extend(merger.poll(start + FRAME_SAMPLES as u64));
    }
    if let Some(span) = seg.flush() {
        turns.extend(merger.push(span));
    }
    turns.extend(merger.flush());
    Ok(turns)
}

/// The stateful half of the pipeline, borrowed for one ingest call.
pub struct OfflinePipeline<'a> {
    pub vad: &'a mut SileroVad,
    pub cfg: &'a Config,
    /// `None` runs capture and segmentation only, as Step 1 did.
    pub analyzer: Option<&'a mut Analyzer>,
    /// Global pause, when this ingest belongs to a running daemon. `None` is
    /// "not a daemon" — an offline re-analysis has nothing to pause.
    pub control: Option<Arc<Control>>,
    /// Where stored segments are announced. `None` publishes nothing.
    pub bus: Option<Arc<Bus>>,
}

impl<'a> OfflinePipeline<'a> {
    /// The plain form: segment and analyse, announce nothing, pause nothing.
    pub fn new(
        vad: &'a mut SileroVad,
        cfg: &'a Config,
        analyzer: Option<&'a mut Analyzer>,
    ) -> Self {
        Self {
            vad,
            cfg,
            analyzer,
            control: None,
            bus: None,
        }
    }

    fn paused(&self) -> bool {
        self.control.as_ref().is_some_and(|c| c.is_paused())
    }
}

/// Segment `samples`, store each turn as a row plus a WAV, and analyse it.
///
/// `t0_ns` is the UTC time of sample zero. Returns the new segment ids.
pub fn ingest_pcm(
    store: &Store,
    data_dir: &Path,
    session_id: i64,
    samples: &[f32],
    t0_ns: i64,
    pipe: &mut OfflinePipeline<'_>,
) -> Result<Vec<i64>> {
    let at = |sample: u64| t0_ns + (sample as i128 * 1_000_000_000 / SAMPLE_RATE as i128) as i64;

    // Paused means no rows and no files, whichever path the audio came in on —
    // and the microphone is not an exception to that (DESIGN §8).
    if pipe.paused() {
        return Ok(Vec::new());
    }
    // The same question the live pipeline asks, from the same column: is this
    // session the user's own microphone? If so the voicebank is not consulted
    // and the speaker is the pinned "You".
    let mic_speaker = match store.session_source_kind(session_id)? {
        Some(kind) if kind == KIND_MIC => Some(store.ensure_you_speaker(t0_ns)?),
        _ => None,
    };
    let turns = segment_pcm(pipe.vad, pipe.cfg, samples)?;
    let mut ids = Vec::new();
    for (seq, span) in turns.into_iter().enumerate() {
        let lo = span.start as usize;
        let hi = (span.end as usize).min(samples.len());
        if lo >= hi {
            continue;
        }
        let slice = &samples[lo..hi];

        let t_start_ns = at(span.start);
        let t_end_ns = at(span.end.min(samples.len() as u64));
        let rel = segment_path(session_id, seq as u64 + 1, t_start_ns);
        write_wav(&data_dir.join(&rel), slice)?;
        let id = store.insert_segment(
            session_id,
            t_start_ns,
            t_end_ns,
            &rel.to_string_lossy(),
            t_start_ns,
        )?;
        match (pipe.analyzer.as_deref_mut(), mic_speaker) {
            (Some(a), Some(speaker_id)) => {
                a.process_mic(
                    store,
                    id,
                    slice,
                    &MicEnroll {
                        speaker_id,
                        data_dir,
                        max_goldens: pipe.cfg.mic.max_goldens,
                    },
                    t_start_ns,
                )?;
            }
            (Some(a), None) => {
                a.process(store, id, slice, t_start_ns)?;
            }
            // Models off: the mic label is provenance, not inference, so it is
            // still true and still worth writing.
            (None, Some(speaker_id)) => store.set_segment_speaker(id, Some(speaker_id), None)?,
            (None, None) => {}
        }
        if let Some(bus) = pipe.bus.as_ref() {
            publish_segment(bus, store, id);
        }
        ids.push(id);
    }
    Ok(ids)
}

/// 16 kHz mono WAV, the format the fixtures and stored segments both use.
pub fn read_wav(path: &Path) -> Result<Vec<f32>> {
    let mut reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    if spec.sample_rate != SAMPLE_RATE || spec.channels != 1 {
        anyhow::bail!(
            "{} is {} Hz / {} channel(s); expected {SAMPLE_RATE} Hz mono",
            path.display(),
            spec.sample_rate,
            spec.channels
        );
    }
    Ok(reader
        .samples::<i16>()
        .map(|s| s.map(|v| v as f32 / 32768.0))
        .collect::<std::result::Result<Vec<_>, _>>()?)
}
