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

/// 10 s at 16 kHz. Baked into the export (`window_size` in its metadata) and
/// the length Step 0 measured; longer windows beat shorter ones when one talker
/// dominates, so we window at the trained size rather than chopping finer.
pub const WINDOW_SAMPLES: usize = 160_000;

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
    /// neither term, so zero-padding a short window cannot dilute the answer.
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

    /// Classify one 10 s window. `samples` is zero-padded to the trained length.
    fn window(&mut self, samples: &[f32]) -> Result<OverlapStats> {
        let mut win = samples.to_vec();
        win.truncate(WINDOW_SAMPLES);
        win.resize(WINDOW_SAMPLES, 0.0);

        let x = Tensor::from_array((vec![1i64, 1, WINDOW_SAMPLES as i64], win))?;
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
    pub fn analyse(&mut self, samples: &[f32]) -> Result<OverlapStats> {
        if samples.is_empty() {
            return Ok(OverlapStats {
                speech_frames: 0,
                overlap_frames: 0,
            });
        }
        let mut total = OverlapStats {
            speech_frames: 0,
            overlap_frames: 0,
        };
        for chunk in samples.chunks(WINDOW_SAMPLES) {
            let s = self.window(chunk)?;
            total.speech_frames += s.speech_frames;
            total.overlap_frames += s.overlap_frames;
        }
        Ok(total)
    }

    pub fn overlap_frac(&mut self, samples: &[f32]) -> Result<f32> {
        Ok(self.analyse(samples)?.frac())
    }
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
        // A short utterance padded out to 10 s must not read as "mostly clean"
        // just because the padding is silent.
        let s = OverlapStats {
            speech_frames: 10,
            overlap_frames: 9,
        };
        assert!((s.frac() - 0.9).abs() < 1e-6);
    }
}
