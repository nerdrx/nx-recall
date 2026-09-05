//! The stereo azimuth probe (0.13.0).
//!
//! VRChat is stereo and spatialised, so a voice's left/right balance — and the
//! tiny inter-channel delay it arrives with — is a candidate second identity
//! signal. That feature (`spike/FINDINGS.md`, "azimuth") has stayed parked
//! since the spike for one reason: nobody ever had a stereo sample to measure
//! it against. VRChat did not run during the spike's recording window, and
//! every OTHER capture in this daemon deliberately downmixes to mono
//! (`resample::downmix`) before a single sample is measured — that is correct
//! for the VAD and the ASR and it is exactly why the azimuth question has
//! never been answerable from anything this daemon has ever written to disk.
//!
//! This module does not build azimuth identity. It answers the narrower
//! question the feature has been blocked on: once the audio is actually
//! captured in stereo, does it carry a signal worth building on at all? Once
//! a day, the first time a `VRChat.exe` source is seen
//! (`capture::VRCHAT_MATCH_KEY`), the capture side records up to
//! [`MAX_SECONDS`] of the ORIGINAL stereo stream — before the downmix — to
//! `<data-dir>/probes/vrchat-stereo-<date>.wav`, and this module measures the
//! inter-channel level difference (ILD) and inter-channel time difference
//! (ITD) of every VAD-active window in it, writing a short plain-text report
//! beside the wav.
//!
//! Bounded three ways, deliberately: 60 seconds, once a day, and only for one
//! named application. It is a diagnostic recording answering one question,
//! not a standing stereo capture of anything.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::resample::downmix;
use crate::vad::{FRAME_SAMPLES, SegmenterConfig, SileroVad};

/// Subdirectory under the data dir every probe file lives in.
pub const SUBDIR: &str = "probes";
/// The bound on one day's recording.
pub const MAX_SECONDS: f32 = 60.0;
pub const CHANNELS: u16 = 2;

/// How far either side of zero the cross-correlation search looks for the
/// inter-channel delay. 40 samples at 16 kHz is 2.5 ms either way — a person
/// would have to be standing beside the near speaker for a real ITD to exceed
/// that, so a measurement that pegs the search window is a sign of noise, not
/// of an extreme position.
const ITD_SEARCH_SAMPLES: i32 = 40;

/// Maximum interleaved-sample count [`MAX_SECONDS`] of stereo audio holds at
/// `rate`.
pub fn max_interleaved_samples(rate: u32) -> usize {
    (MAX_SECONDS * rate as f32) as usize * CHANNELS as usize
}

pub fn wav_path(data_dir: &Path, date: &str) -> PathBuf {
    data_dir
        .join(SUBDIR)
        .join(format!("vrchat-stereo-{date}.wav"))
}

pub fn report_path(data_dir: &Path, date: &str) -> PathBuf {
    data_dir
        .join(SUBDIR)
        .join(format!("vrchat-stereo-{date}.txt"))
}

/// Has today's probe already run? Bounded by the wav's own presence — no
/// database row and no separate marker file, just the file the probe would
/// otherwise overwrite.
pub fn already_captured_today(data_dir: &Path, date: &str) -> bool {
    wav_path(data_dir, date).exists()
}

/// Write interleaved stereo f32 samples as a 16-bit PCM stereo WAV.
pub fn write_stereo_wav(path: &Path, rate: u32, interleaved: &[f32]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let spec = hound::WavSpec {
        channels: CHANNELS,
        sample_rate: rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec)
        .with_context(|| format!("creating {}", path.display()))?;
    for s in interleaved {
        w.write_sample((s.clamp(-1.0, 1.0) * 32767.0) as i16)?;
    }
    w.finalize()?;
    Ok(())
}

/// One VAD-active window's spatial measurement.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowMeasurement {
    pub start_s: f32,
    pub end_s: f32,
    /// `20 * log10(rms_left / rms_right)`. Positive: louder on the left.
    pub ild_db: f32,
    /// Positive: the right channel lags the left (the source is left of
    /// centre, for an ordinary stereo image).
    pub itd_us: f32,
}

/// What one day's probe found.
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeReport {
    pub duration_s: f32,
    pub windows: Vec<WindowMeasurement>,
    /// The actual answer to "is this worth building the azimuth feature on":
    /// do the per-turn ILD/ITD measurements cluster into distinct positions,
    /// or do they smear around the centre the way a game that pans dialogue
    /// only slightly (or not at all) would produce?
    pub usable_for_azimuth: bool,
    pub summary: String,
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
}

/// Level difference between the two channels of one window, in dB. `-90.0`
/// (an arbitrary, very quiet floor) when one side is silent and the other is
/// not, rather than an infinity that would poison every average it touches.
fn ild_db(left: &[f32], right: &[f32]) -> f32 {
    let l = rms(left);
    let r = rms(right);
    const FLOOR: f32 = 1e-6;
    20.0 * (l.max(FLOOR) / r.max(FLOOR)).log10()
}

/// The lag (in samples, right relative to left) that maximises normalised
/// cross-correlation over `±search`. A positive result means right lags left.
fn best_lag(left: &[f32], right: &[f32], search: i32) -> i32 {
    let n = left.len().min(right.len());
    if n == 0 {
        return 0;
    }
    let mut best_lag = 0i32;
    let mut best_score = f32::MIN;
    for lag in -search..=search {
        let mut sum = 0.0f32;
        let mut count = 0usize;
        for (i, &l) in left.iter().enumerate().take(n) {
            let j = i as i32 + lag;
            if j < 0 || j as usize >= n {
                continue;
            }
            // right shifted by `lag` compared against left: right[i + lag]
            // lines up with left[i] when right lags left by `lag` samples.
            sum += l * right[j as usize];
            count += 1;
        }
        if count == 0 {
            continue;
        }
        let score = sum / count as f32;
        if score > best_score {
            best_score = score;
            best_lag = lag;
        }
    }
    best_lag
}

fn itd_us(left: &[f32], right: &[f32], rate: u32) -> f32 {
    let lag = best_lag(left, right, ITD_SEARCH_SAMPLES);
    lag as f32 * 1_000_000.0 / rate as f32
}

/// Standard deviation of a slice, `0.0` for fewer than two values.
fn stddev(values: &[f32]) -> f32 {
    if values.len() < 2 {
        return 0.0;
    }
    let mean = values.iter().sum::<f32>() / values.len() as f32;
    let var = values.iter().map(|v| (v - mean).powi(2)).sum::<f32>() / values.len() as f32;
    var.sqrt()
}

/// Measure ILD and ITD over every VAD-active window of a stereo recording.
///
/// The VAD itself runs on the downmix — Silero was trained on mono speech,
/// and "is this window speech" is exactly the question the daemon already
/// answers this way for every other capture — but every measurement is taken
/// from the ORIGINAL, un-mixed channels, which is the entire reason this
/// module exists rather than reusing a segment that had already been
/// downmixed and written to `segments/`.
pub fn analyze(interleaved: &[f32], rate: u32) -> Result<ProbeReport> {
    let frames = interleaved.len() / CHANNELS as usize;
    let mut left = Vec::with_capacity(frames);
    let mut right = Vec::with_capacity(frames);
    // `CHANNELS` is fixed at 2 (see its doc comment); `as_chunks::<2>` needs
    // that as a literal, so it is spelled out rather than passed through.
    for chunk in interleaved.as_chunks::<2>().0 {
        left.push(chunk[0]);
        right.push(chunk[1]);
    }
    let mono = downmix(
        &interleaved[..frames * CHANNELS as usize],
        CHANNELS as usize,
    );

    let mut vad = SileroVad::from_bytes(crate::VAD_MODEL).context("loading the bundled VAD")?;
    let mut state = vad.new_state();
    let seg_cfg = SegmenterConfig::from_ms(0.5, 250, 500, 200, 30_000, rate);
    let mut segmenter = crate::vad::Segmenter::new(seg_cfg);

    let mut spans = Vec::new();
    let mut cursor = 0u64;
    while (cursor as usize + FRAME_SAMPLES) <= mono.len() {
        let frame = &mono[cursor as usize..cursor as usize + FRAME_SAMPLES];
        let prob = vad.frame(frame, &mut state)?;
        if let Some(span) = segmenter.push_frame(prob, cursor, FRAME_SAMPLES as u64) {
            spans.push(span);
        }
        cursor += FRAME_SAMPLES as u64;
    }
    if let Some(span) = segmenter.flush() {
        spans.push(span);
    }

    let mut windows = Vec::with_capacity(spans.len());
    for span in &spans {
        let start = span.start as usize;
        let end = (span.end as usize).min(frames);
        if start >= end {
            continue;
        }
        let l = &left[start..end];
        let r = &right[start..end];
        windows.push(WindowMeasurement {
            start_s: start as f32 / rate as f32,
            end_s: end as f32 / rate as f32,
            ild_db: ild_db(l, r),
            itd_us: itd_us(l, r, rate),
        });
    }

    let ilds: Vec<f32> = windows.iter().map(|w| w.ild_db).collect();
    let itds: Vec<f32> = windows.iter().map(|w| w.itd_us).collect();
    let ild_spread = stddev(&ilds);
    let itd_spread = stddev(&itds);
    // Distinct positions read as spread ACROSS windows, not loudness within
    // one — a single centred voice has near-zero ILD/ITD every window, and a
    // game that pans dialogue only slightly produces spread too small to ever
    // separate two people. 1.5 dB and 80 us are comfortably above what mic
    // self-noise and quantisation alone would produce on a silent channel
    // pair, and comfortably below a real left/right seating difference.
    let usable_for_azimuth = windows.len() >= 2 && (ild_spread > 1.5 || itd_spread > 80.0);

    let duration_s = frames as f32 / rate as f32;
    let summary = if windows.is_empty() {
        "no VAD-active window in the recording; nothing to measure".to_string()
    } else if usable_for_azimuth {
        format!(
            "{} window(s), ILD spread {ild_spread:.2} dB, ITD spread {itd_spread:.1} us — \
             turns cluster into distinct positions; the audio carries usable azimuth",
            windows.len()
        )
    } else {
        format!(
            "{} window(s), ILD spread {ild_spread:.2} dB, ITD spread {itd_spread:.1} us — \
             no separation between turns; azimuth would not distinguish anyone here",
            windows.len()
        )
    };

    Ok(ProbeReport {
        duration_s,
        windows,
        usable_for_azimuth,
        summary,
    })
}

pub fn write_report(path: &Path, date: &str, report: &ProbeReport) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut out = format!(
        "nx-recall stereo azimuth probe — {date}\n\
         recorded: {:.1} s\n\
         windows measured: {}\n\
         verdict: {}\n\n",
        report.duration_s,
        report.windows.len(),
        report.summary,
    );
    for (i, w) in report.windows.iter().enumerate() {
        out.push_str(&format!(
            "  [{i:02}] {:>6.2}s - {:<6.2}s  ILD {:>+6.2} dB  ITD {:>+7.1} us\n",
            w.start_s, w.end_s, w.ild_db, w.itd_us
        ));
    }
    std::fs::write(path, out).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f32, seconds: f32, rate: u32, amplitude: f32, phase_samples: i32) -> Vec<f32> {
        let n = (seconds * rate as f32) as i32;
        (0..n)
            .map(|i| {
                let t = (i - phase_samples) as f32 / rate as f32;
                amplitude * (2.0 * std::f32::consts::PI * freq * t).sin()
            })
            .collect()
    }

    fn interleave(left: &[f32], right: &[f32]) -> Vec<f32> {
        left.iter().zip(right).flat_map(|(&l, &r)| [l, r]).collect()
    }

    #[test]
    fn paths_are_scoped_under_the_probes_subdir_and_named_by_date() {
        let dir = Path::new("/data");
        assert_eq!(
            wav_path(dir, "2026-09-05"),
            PathBuf::from("/data/probes/vrchat-stereo-2026-09-05.wav")
        );
        assert_eq!(
            report_path(dir, "2026-09-05"),
            PathBuf::from("/data/probes/vrchat-stereo-2026-09-05.txt")
        );
    }

    #[test]
    fn a_probe_is_bounded_to_once_a_day_by_the_wav_s_own_presence() {
        let dir = std::env::temp_dir().join(format!("nx-recall-probe-once-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!already_captured_today(&dir, "2026-09-05"));
        write_stereo_wav(&wav_path(&dir, "2026-09-05"), 16_000, &[0.0; 8]).unwrap();
        assert!(already_captured_today(&dir, "2026-09-05"));
        // A different day is untouched.
        assert!(!already_captured_today(&dir, "2026-09-06"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stereo_wav_round_trips_both_channels() {
        let dir = std::env::temp_dir().join(format!("nx-recall-probe-wav-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = wav_path(&dir, "2026-09-05");
        let interleaved: Vec<f32> = (0..1000)
            .map(|i| if i % 2 == 0 { 0.5 } else { -0.25 })
            .collect();
        write_stereo_wav(&path, 16_000, &interleaved).unwrap();

        let mut r = hound::WavReader::open(&path).unwrap();
        assert_eq!(r.spec().channels, 2);
        assert_eq!(r.spec().sample_rate, 16_000);
        let samples: Vec<i16> = r.samples::<i16>().map(|s| s.unwrap()).collect();
        assert_eq!(samples.len(), interleaved.len());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn max_interleaved_samples_matches_sixty_seconds_of_stereo() {
        assert_eq!(max_interleaved_samples(16_000), 16_000 * 60 * 2);
    }

    #[test]
    fn ild_reads_zero_for_identical_channels_and_signed_for_a_louder_side() {
        let l = sine(200.0, 0.1, 16_000, 0.5, 0);
        let r = l.clone();
        assert!(ild_db(&l, &r).abs() < 0.01);

        let quiet_right = sine(200.0, 0.1, 16_000, 0.125, 0); // -12 dB relative
        let db = ild_db(&l, &quiet_right);
        assert!((db - 12.0).abs() < 0.5, "expected roughly +12 dB, got {db}");
    }

    #[test]
    fn itd_recovers_a_known_inter_channel_delay() {
        let rate = 16_000;
        let left = sine(300.0, 0.2, rate, 0.5, 0);
        // The right channel is the same tone, arriving 10 samples "later" —
        // i.e. right[i] == left[i - 10].
        let right = sine(300.0, 0.2, rate, 0.5, 10);
        let us = itd_us(&left, &right, rate);
        let expected = 10.0 * 1_000_000.0 / rate as f32;
        assert!(
            (us - expected).abs() < (1_000_000.0 / rate as f32) * 1.5,
            "expected close to {expected} us, got {us} us"
        );
    }

    /// Real speech, not a synthetic tone: Silero was trained on voice, and a
    /// pure sine wave does not reliably trip it — which would make a test
    /// that used one prove nothing about "no VAD-active window", only about a
    /// signal the segmenter was never going to open on in the first place.
    fn fixture_speech() -> Vec<f32> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("fixtures")
            .join("clean_single_0.wav");
        crate::ingest::read_wav(&path).expect("fixtures/clean_single_0.wav must be readable")
    }

    #[test]
    fn a_centred_recording_is_not_usable_for_azimuth() {
        let rate = 16_000;
        // The same real speech on both channels at the same gain — exactly
        // what a game with no spatialisation, or dialogue routed to both ears
        // equally, would produce.
        let speech = fixture_speech();
        let interleaved = interleave(&speech, &speech);
        let report = analyze(&interleaved, rate).unwrap();
        assert!(
            !report.windows.is_empty(),
            "real speech must trip the VAD at all: {report:?}"
        );
        assert!(
            !report.usable_for_azimuth,
            "a centred, unspatialised recording must not read as usable: {report:?}"
        );
    }

    #[test]
    fn two_turns_at_clearly_different_positions_are_usable_for_azimuth() {
        let rate = 16_000;
        // Turn one: hard left (quiet right channel). Turn two: hard right
        // (quiet left channel), separated by a full second of silence — well
        // past `[vad].min_silence_ms` (500 ms default), so the segmenter closes
        // the first turn before the second opens. This is the shape a
        // spatialised VRChat recording of two people on opposite sides of the
        // listener would produce.
        let speech = fixture_speech();
        let quiet: Vec<f32> = speech.iter().map(|s| s * 0.02).collect();
        let silence = vec![0.0f32; rate as usize];

        let mut left = Vec::new();
        let mut right = Vec::new();
        left.extend_from_slice(&speech);
        right.extend_from_slice(&quiet);
        left.extend_from_slice(&silence);
        right.extend_from_slice(&silence);
        left.extend_from_slice(&quiet);
        right.extend_from_slice(&speech);
        left.extend_from_slice(&silence);
        right.extend_from_slice(&silence);

        let interleaved = interleave(&left, &right);
        let report = analyze(&interleaved, rate).unwrap();
        assert!(
            report.windows.len() >= 2,
            "expected two separate VAD-active windows: {report:?}"
        );
        assert!(
            report.usable_for_azimuth,
            "two turns at opposite hard-panned positions must read as usable: {report:?}"
        );
        // And the sign actually distinguishes them: one window reads
        // positive (louder left) and another negative (louder right).
        assert!(report.windows.iter().any(|w| w.ild_db > 5.0));
        assert!(report.windows.iter().any(|w| w.ild_db < -5.0));
    }

    #[test]
    fn a_silent_recording_yields_no_windows_and_is_not_usable() {
        let rate = 16_000;
        let interleaved = vec![0.0f32; rate as usize * 2]; // 1 s, both channels silent
        let report = analyze(&interleaved, rate).unwrap();
        assert!(report.windows.is_empty());
        assert!(!report.usable_for_azimuth);
    }

    #[test]
    fn the_report_file_names_the_verdict_and_every_window() {
        let dir =
            std::env::temp_dir().join(format!("nx-recall-probe-report-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let report = ProbeReport {
            duration_s: 12.5,
            windows: vec![WindowMeasurement {
                start_s: 1.0,
                end_s: 2.0,
                ild_db: 3.0,
                itd_us: -50.0,
            }],
            usable_for_azimuth: true,
            summary: "1 window(s), usable".to_string(),
        };
        let path = report_path(&dir, "2026-09-05");
        write_report(&path, "2026-09-05", &report).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("2026-09-05"));
        assert!(text.contains("usable"));
        assert!(text.contains("ILD"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
