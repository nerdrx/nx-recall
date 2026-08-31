//! Overlapped-speech detection with pyannote segmentation-3.0.
//!
//! Used strictly as a powerset classifier, never for diarization or separation.
//! The export has three speaker slots and at most two simultaneously active, so
//! the seven classes are: 0 = silence, 1..=3 = one speaker, 4..=6 = two
//! speakers. `overlap_frac` is the share of *speech* frames in 4..=6.
//!
//! This is the one stage sherpa-onnx cannot serve: its C API exposes the full
//! diarization pipeline (segmentation + clustering) but not the raw per-frame
//! posteriors, and the posteriors are the entire product here. A single session
//! run plus an argmax needs no feature extraction, so running it on `ort`
//! directly costs nothing but this file.

use anyhow::{Context, Result, bail};
use ort::session::Session;
use ort::value::Tensor;

/// 10 s at 16 kHz: the length the export was trained on (`window_size` in its
/// metadata) and the length Step 0 measured. Longer windows beat shorter ones
/// when one talker dominates, so segments are chopped at the trained size
/// rather than finer — but a segment *shorter* than this is fed at its own
/// length, not padded up to it. See `analyse`.
pub const WINDOW_SAMPLES: usize = 160_000;

/// Shortest input worth running. The export's frame formula yields its first
/// frame at roughly 1000 samples (receptive field 991), so anything much below
/// this produces an empty or degenerate output. One second is also the identity
/// gate's own duration floor, so nothing above it is thrown away.
pub const MIN_SAMPLES: usize = 16_000;

/// Number of powerset classes in the export.
const CLASSES: usize = 7;
/// Classes at or above this index mean two speakers are simultaneously active.
const FIRST_OVERLAP_CLASS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverlapStats {
    pub speech_frames: usize,
    pub overlap_frames: usize,
}

impl OverlapStats {
    /// Fraction of speech that is two people at once. Silence contributes to
    /// neither term, and tiling repeats the same audio into both, so neither
    /// padding nor window count can move the ratio on its own.
    pub fn frac(&self) -> f32 {
        if self.speech_frames == 0 {
            return 0.0;
        }
        self.overlap_frames as f32 / self.speech_frames as f32
    }
}

pub struct OverlapDetector {
    session: Session,
}

impl OverlapDetector {
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let ort_err = |what: &'static str| move |e: ort::Error<_>| anyhow::anyhow!("{what}: {e}");
        let session = Session::builder()
            .map_err(|e| anyhow::anyhow!("creating ONNX session builder: {e}"))?
            .with_intra_threads(1)
            .map_err(ort_err("configuring intra-op threads"))?
            .with_inter_threads(1)
            .map_err(ort_err("configuring inter-op threads"))?
            .commit_from_file(path)
            .with_context(|| format!("loading the segmentation model {}", path.display()))?;
        Ok(Self { session })
    }

    /// Classify one window of real audio. The export's time axis is dynamic,
    /// so the length is whatever `analyse` hands over; only the lower bound is
    /// enforced.
    fn window(&mut self, samples: &[f32]) -> Result<OverlapStats> {
        if samples.len() < MIN_SAMPLES {
            bail!(
                "segmentation window must be at least {MIN_SAMPLES} samples, got {}",
                samples.len()
            );
        }

        let x = Tensor::from_array((vec![1i64, 1, samples.len() as i64], samples.to_vec()))?;
        let out = self.session.run(ort::inputs!["x" => x])?;
        let (shape, y) = out["y"].try_extract_tensor::<f32>()?;
        if shape.len() != 3 || shape[2] as usize != CLASSES {
            bail!(
                "unexpected segmentation output shape {shape:?}; expected [1, frames, {CLASSES}]"
            );
        }

        let frames = shape[1] as usize;
        let mut stats = OverlapStats {
            speech_frames: 0,
            overlap_frames: 0,
        };
        for f in 0..frames {
            let row = &y[f * CLASSES..(f + 1) * CLASSES];
            let class = argmax(row);
            if class != 0 {
                stats.speech_frames += 1;
            }
            if class >= FIRST_OVERLAP_CLASS {
                stats.overlap_frames += 1;
            }
        }
        Ok(stats)
    }

    /// Overlap statistics for a whole segment, however long.
    ///
    /// **Nothing is ever padded up to the window length.** The model normalises
    /// per chunk, so filling a window out with silence raises the effective
    /// gain on the speech that is actually there and lifts a −12 dB interferer
    /// into "second active speaker" — measured on `lobby_dominant_1` (1.8 s),
    /// that is the difference between 0.914 and 0.000, and real turns are
    /// mostly 2–4 s, so it would be the common path rather than an edge case.
    /// Tiling the segment to fill the window fixes that but fabricates
    /// periodic content, which costs the other half of the gate: the 10-talker
    /// equal-loudness fixtures read 1.000 and 0.841 at their true length and
    /// collapse to 0.000 and 0.063 tiled.
    ///
    /// The export's time axis is dynamic, so the fix for both is to feed it the
    /// real audio at its real length:
    ///
    /// * under `MIN_SAMPLES` — too short for the model to produce frames, so
    ///   tile up to that floor. Such turns are refused by the duration gate
    ///   regardless, making the number informational only.
    /// * up to a window — run as-is.
    /// * longer — 10 s chunks, with a short tail slid back over real audio
    ///   rather than padded. Re-measuring a few frames costs less than
    ///   distorting the tail.
    pub fn analyse(&mut self, samples: &[f32]) -> Result<OverlapStats> {
        if samples.is_empty() {
            return Ok(OverlapStats {
                speech_frames: 0,
                overlap_frames: 0,
            });
        }
        if samples.len() < MIN_SAMPLES {
            return self.window(&tile_to(samples, MIN_SAMPLES));
        }
        if samples.len() <= WINDOW_SAMPLES {
            return self.window(samples);
        }

        let mut total = OverlapStats {
            speech_frames: 0,
            overlap_frames: 0,
        };
        let mut start = 0usize;
        while start < samples.len() {
            let lo = if start + WINDOW_SAMPLES > samples.len() {
                samples.len() - WINDOW_SAMPLES
            } else {
                start
            };
            let s = self.window(&samples[lo..lo + WINDOW_SAMPLES])?;
            total.speech_frames += s.speech_frames;
            total.overlap_frames += s.overlap_frames;
            start += WINDOW_SAMPLES;
        }
        Ok(total)
    }

    pub fn overlap_frac(&mut self, samples: &[f32]) -> Result<f32> {
        Ok(self.analyse(samples)?.frac())
    }
}

/// Repeat `samples` cyclically until it is at least `target` long. Used only to
/// lift sub-second audio to the model's minimum; never to fill a window.
/// `samples` must be non-empty.
fn tile_to(samples: &[f32], target: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(target);
    while out.len() < target {
        let take = (target - out.len()).min(samples.len());
        out.extend_from_slice(&samples[..take]);
    }
    out
}

fn argmax(row: &[f32]) -> usize {
    let mut best = 0usize;
    for (i, v) in row.iter().enumerate() {
        if *v > row[best] {
            best = i;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argmax_picks_the_largest_and_ties_go_to_the_first() {
        assert_eq!(argmax(&[0.1, 0.9, 0.3]), 1);
        assert_eq!(argmax(&[0.5, 0.5]), 0);
        assert_eq!(argmax(&[-3.0, -1.0, -2.0]), 1);
    }

    #[test]
    fn silence_only_yields_zero_rather_than_a_division_by_zero() {
        let s = OverlapStats {
            speech_frames: 0,
            overlap_frames: 0,
        };
        assert_eq!(s.frac(), 0.0);
    }

    #[test]
    fn the_fraction_is_of_speech_not_of_the_window() {
        // A short utterance filled out to 10 s must not read as "mostly clean"
        // just because most of the window is not the utterance.
        let s = OverlapStats {
            speech_frames: 10,
            overlap_frames: 9,
        };
        assert!((s.frac() - 0.9).abs() < 1e-6);
    }

    #[test]
    fn tiling_repeats_the_audio_and_never_introduces_silence() {
        let short = [1.0f32, 2.0, 3.0];
        let win = tile_to(&short, MIN_SAMPLES);
        assert_eq!(win.len(), MIN_SAMPLES);
        for (i, v) in win.iter().enumerate() {
            assert_eq!(*v, short[i % 3]);
        }
        // Silence is exactly what shifts the model's per-chunk normalisation.
        assert!(win.iter().all(|v| *v != 0.0));
    }

    #[test]
    fn tiling_leaves_long_enough_audio_alone() {
        let already = vec![0.25f32; MIN_SAMPLES];
        assert_eq!(tile_to(&already, MIN_SAMPLES), already);
    }

    #[test]
    fn tiling_a_single_sample_terminates() {
        assert_eq!(tile_to(&[0.5], MIN_SAMPLES).len(), MIN_SAMPLES);
    }

    #[test]
    fn the_minimum_clears_the_models_receptive_field() {
        // The export needs ~991 samples before it emits a single frame.
        const { assert!(MIN_SAMPLES > 1_000) };
        const { assert!(MIN_SAMPLES < WINDOW_SAMPLES) };
    }
}
