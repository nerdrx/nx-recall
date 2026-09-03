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
    fn window_classes(&mut self, samples: &[f32]) -> Result<Vec<u8>> {
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
        let mut out = Vec::with_capacity(frames);
        for f in 0..frames {
            out.push(argmax(&y[f * CLASSES..(f + 1) * CLASSES]) as u8);
        }
        Ok(out)
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
        Ok(stats_of(&self.classes(samples)?))
    }

    /// [`analyse`](Self::analyse)'s frames, in order, before they are counted.
    ///
    /// Same chunking and the same refusal to pad; the chunks are concatenated
    /// rather than summed, so a caller can aggregate them however it likes.
    /// The slid tail repeats a few frames of real audio, exactly as the counts
    /// do — re-measuring beats distorting, and it is the same trade either way.
    pub fn classes(&mut self, samples: &[f32]) -> Result<Vec<u8>> {
        if samples.is_empty() {
            return Ok(Vec::new());
        }
        if samples.len() < MIN_SAMPLES {
            return self.window_classes(&tile_to(samples, MIN_SAMPLES));
        }
        if samples.len() <= WINDOW_SAMPLES {
            return self.window_classes(samples);
        }
        let mut out = Vec::new();
        let mut start = 0usize;
        while start < samples.len() {
            let lo = if start + WINDOW_SAMPLES > samples.len() {
                samples.len() - WINDOW_SAMPLES
            } else {
                start
            };
            out.extend(self.window_classes(&samples[lo..lo + WINDOW_SAMPLES])?);
            start += WINDOW_SAMPLES;
        }
        Ok(out)
    }

    pub fn overlap_frac(&mut self, samples: &[f32]) -> Result<f32> {
        Ok(self.analyse(samples)?.frac())
    }
}

/// Count a run of frames the way the gate has always counted them: the share
/// of speech frames that are two speakers at once.
pub fn stats_of(classes: &[u8]) -> OverlapStats {
    let mut s = OverlapStats {
        speech_frames: 0,
        overlap_frames: 0,
    };
    for c in classes {
        if *c != 0 {
            s.speech_frames += 1;
        }
        if *c as usize >= FIRST_OVERLAP_CLASS {
            s.overlap_frames += 1;
        }
    }
    s
}

/// The frame step of the export, in seconds: the number the `win` argument of
/// [`windowed_max_frac`] is worked out from.
///
/// Measured off the export rather than assumed — 10 s of input yields 589
/// frames, so a frame is about 17 ms. Callers that have a real run in hand
/// should divide its own frame count by its own duration instead; this is for
/// the ones that only have a target window length.
pub const FRAME_SECONDS: f32 = 10.0 / 589.0;

/// The **worst second** of a turn rather than its average (0.11.6, §26).
///
/// The mean over a turn answers "how overlapped was this turn"; identity wants
/// "was there a stretch long enough to capture the embedder". A one-second
/// interjection at the top of a six-second answer is 17% of the mean and 100%
/// of the second it happens in, and only one of those two numbers is about the
/// risk of writing down the wrong name.
///
/// `win` is the window in frames. Windows with less than half their frames in
/// speech are skipped — a window that is mostly silence has too little
/// evidence to be anybody's maximum — and when no window qualifies (a very
/// short or very quiet turn) the whole-run mean is returned, so this can only
/// differ from `stats_of(...).frac()` where there was something to see.
pub fn windowed_max_frac(classes: &[u8], win: usize) -> f32 {
    let all = stats_of(classes).frac();
    if win == 0 || classes.len() <= win {
        return all;
    }
    let mut speech = 0usize;
    let mut over = 0usize;
    let mut best: Option<f32> = None;
    for (i, c) in classes.iter().enumerate() {
        if *c != 0 {
            speech += 1;
        }
        if *c as usize >= FIRST_OVERLAP_CLASS {
            over += 1;
        }
        if i >= win {
            let gone = classes[i - win];
            if gone != 0 {
                speech -= 1;
            }
            if gone as usize >= FIRST_OVERLAP_CLASS {
                over -= 1;
            }
        }
        if i + 1 >= win && speech * 2 >= win {
            let f = over as f32 / speech as f32;
            if best.is_none_or(|b| f > b) {
                best = Some(f);
            }
        }
    }
    best.unwrap_or(all)
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
    fn counting_frames_matches_the_gates_definition() {
        // 0 silence, 1..=3 one speaker, 4..=6 two.
        let s = stats_of(&[0, 1, 2, 4, 5, 0, 6, 3]);
        assert_eq!(s.speech_frames, 6);
        assert_eq!(s.overlap_frames, 3);
        assert!((s.frac() - 0.5).abs() < 1e-6);
        assert_eq!(stats_of(&[]).speech_frames, 0);
    }

    #[test]
    fn the_windowed_maximum_finds_a_burst_the_mean_dilutes() {
        // Four overlapped frames in twenty: a mean of 0.2, but the window
        // they land in is entirely overlapped.
        let mut c = vec![1u8; 20];
        c[8..12].fill(5);
        assert!((stats_of(&c).frac() - 0.2).abs() < 1e-6);
        assert!((windowed_max_frac(&c, 4) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn a_run_no_longer_than_the_window_is_just_the_mean() {
        let c = [1u8, 4, 1, 4];
        assert_eq!(windowed_max_frac(&c, 4), stats_of(&c).frac());
        assert_eq!(windowed_max_frac(&c, 99), stats_of(&c).frac());
        assert_eq!(windowed_max_frac(&c, 0), stats_of(&c).frac());
    }

    #[test]
    fn clean_speech_stays_clean_under_either_aggregation() {
        let c = vec![2u8; 100];
        assert_eq!(stats_of(&c).frac(), 0.0);
        assert_eq!(windowed_max_frac(&c, 60), 0.0);
    }

    #[test]
    fn a_window_that_is_mostly_silence_is_not_allowed_to_be_the_maximum() {
        // One overlapped frame surrounded by silence would read 1.0 if the
        // evidence bar were not there; the real answer is the run's mean.
        let mut c = vec![0u8; 40];
        c[20] = 4;
        let f = windowed_max_frac(&c, 10);
        assert!((f - stats_of(&c).frac()).abs() < 1e-6, "{f}");
        assert!(
            (f - 1.0).abs() < 1e-6,
            "one speech frame, and it is overlap"
        );
    }

    #[test]
    fn the_sliding_window_agrees_with_a_naive_scan() {
        let c: Vec<u8> = (0..97u32).map(|i| ((i * 7 + i / 3) % 7) as u8).collect();
        for win in [3usize, 10, 59] {
            let mut want: Option<f32> = None;
            for w in c.windows(win) {
                let s = stats_of(w);
                if s.speech_frames * 2 >= win {
                    let f = s.frac();
                    if want.is_none_or(|b| f > b) {
                        want = Some(f);
                    }
                }
            }
            let want = want.unwrap_or_else(|| stats_of(&c).frac());
            assert!(
                (windowed_max_frac(&c, win) - want).abs() < 1e-6,
                "win {win}: {} vs {want}",
                windowed_max_frac(&c, win)
            );
        }
    }

    #[test]
    fn a_second_is_about_sixty_frames() {
        let win = (1.0 / FRAME_SECONDS).round() as usize;
        assert!((55..=62).contains(&win), "{win}");
    }

    #[test]
    fn the_minimum_clears_the_models_receptive_field() {
        // The export needs ~991 samples before it emits a single frame.
        const { assert!(MIN_SAMPLES > 1_000) };
        const { assert!(MIN_SAMPLES < WINDOW_SAMPLES) };
    }
}
