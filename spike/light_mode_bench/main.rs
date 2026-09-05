//! Light mode (0.13.x): does swapping the live decoder for Parakeet-TDT 110m
//! actually halve the CPU, and what does it cost in words — measured on the
//! user's own archive, read-only, the same discipline `livegpu_bench` uses.
//!
//! §10 measured RTF on FLEURS (110m 0.011 vs 0.6b-v3 0.026) and §40 measured
//! the live path's real cost per audio minute (29.9 CPU s, 89.5% of it the
//! transducer). Neither number is "CPU seconds per audio minute for the 110m
//! export, on real turns, interleaved against the model it would replace, with
//! a WER against the decoder it is replacing" — which is the number the gate
//! in the light-mode round is written against. This binary measures that one.
//!
//! Interleaved per clip (decode A, decode B, next clip) rather than one whole
//! sweep per model: back-to-back sweeps would let thermal throttling or a
//! background jitter source land unevenly on whichever model went second, and
//! interleaving is the standard defence against exactly that.
//!
//! Read-only: it opens the segment WAVs and the model files under
//! `[models].dir` and writes nothing anywhere, the same contract
//! `livegpu_bench` carries.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};

use recalld::asr::{Asr, normalise_words};
use recalld::config::{Config, SAMPLE_RATE};
use recalld::models::{DEFAULT_ASR, FALLBACK_ASR, ModelSet};

// ---------------------------------------------------------------------------
// clocks (verbatim from `livegpu_bench`)
// ---------------------------------------------------------------------------

/// Process CPU time in seconds: user + sys, all threads.
fn cpu_secs() -> f64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `ru` is a valid, fully-owned `rusage` and RUSAGE_SELF takes no
    // other argument.
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let s = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    s(ru.ru_utime) + s(ru.ru_stime)
}

// ---------------------------------------------------------------------------
// the clips
// ---------------------------------------------------------------------------

struct Clip {
    path: PathBuf,
    samples: Vec<f32>,
}

impl Clip {
    fn seconds(&self) -> f64 {
        self.samples.len() as f64 / SAMPLE_RATE as f64
    }
}

fn read_wav(path: &Path) -> Result<Vec<f32>> {
    let mut r = hound::WavReader::open(path)?;
    let spec = r.spec();
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => r.samples::<f32>().collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => r
            .samples::<i32>()
            .map(|s| s.map(|v| v as f32 / i16::MAX as f32))
            .collect::<Result<_, _>>()?,
    };
    if spec.channels != 1 {
        anyhow::bail!("{} is not mono", path.display());
    }
    if spec.sample_rate != SAMPLE_RATE {
        anyhow::bail!("{} is {} Hz", path.display(), spec.sample_rate);
    }
    Ok(samples)
}

/// Walk the segment tree and take clips until `target_s` of audio is in hand.
/// Verbatim from `livegpu_bench`, so the two binaries sample the archive the
/// same way and their numbers stay comparable.
fn collect(root: &Path, target_s: f64, min_s: f64) -> Result<Vec<Clip>> {
    let mut paths = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "wav") {
                paths.push(p);
            }
        }
    }
    paths.sort();
    anyhow::ensure!(!paths.is_empty(), "no .wav under {}", root.display());

    let want = (target_s / 2.7).ceil() as usize;
    let stride = (paths.len() / want.max(1)).max(1);

    let mut clips = Vec::new();
    let mut total = 0.0;
    for p in paths.iter().step_by(stride) {
        if total >= target_s {
            break;
        }
        let Ok(samples) = read_wav(p) else { continue };
        let secs = samples.len() as f64 / SAMPLE_RATE as f64;
        if secs < min_s {
            continue;
        }
        total += secs;
        clips.push(Clip {
            path: p.clone(),
            samples,
        });
    }
    anyhow::ensure!(
        total > target_s * 0.5,
        "only {total:.0} s of usable audio found under {}",
        root.display()
    );
    Ok(clips)
}

// ---------------------------------------------------------------------------
// word error rate
// ---------------------------------------------------------------------------

/// Word-level edit distance, normalised by the reference length. `ref_words`
/// is what the 0.6b-v3 export said — the live product's OWN answer, not a
/// human transcript, which is the honest reference for "what does swapping to
/// the smaller export change" rather than a claim about either model's
/// absolute accuracy (§10's FLEURS numbers are that claim, and are quoted in
/// the summary rather than re-derived here).
fn wer(reference: &str, hypothesis: &str) -> Option<f64> {
    let r = normalise_words(reference);
    let h = normalise_words(hypothesis);
    if r.is_empty() {
        return None;
    }
    let (rn, hn) = (r.len(), h.len());
    let mut dp = vec![vec![0usize; hn + 1]; rn + 1];
    for (i, row) in dp.iter_mut().enumerate() {
        row[0] = i;
    }
    for j in 0..=hn {
        dp[0][j] = j;
    }
    for i in 1..=rn {
        for j in 1..=hn {
            dp[i][j] = if r[i - 1] == h[j - 1] {
                dp[i - 1][j - 1]
            } else {
                1 + dp[i - 1][j - 1].min(dp[i - 1][j]).min(dp[i][j - 1])
            };
        }
    }
    Some(dp[rn][hn] as f64 / rn as f64)
}

// ---------------------------------------------------------------------------
// duration buckets
// ---------------------------------------------------------------------------

/// FINDINGS' own fragment lengths (1.5 s, 2.5 s / the measured median 2.7 s,
/// 8 s) rather than round numbers, so a bucket boundary lines up with a claim
/// this codebase already makes about where accuracy changes.
const BUCKETS: &[(f64, &str)] = &[(1.5, "< 1.5s"), (2.7, "1.5-2.7s"), (8.0, "2.7-8s"), (f64::INFINITY, "8s+")];

fn bucket_of(secs: f64) -> &'static str {
    for (ceil, name) in BUCKETS {
        if secs < *ceil {
            return name;
        }
    }
    BUCKETS.last().unwrap().1
}

#[derive(Default)]
struct BucketStats {
    clips: usize,
    audio_s: f64,
    cpu_default: f64,
    cpu_light: f64,
    wer_sum: f64,
    wer_n: usize,
}

// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    // Two roots, not one: the machine rules for this round put model
    // downloads under the agent's scratch directory and forbid touching
    // `~/.local/share/nx-recall/models`, while the measurement itself is
    // explicitly against the REAL archive's segments — so the models root and
    // the segments root are different arguments, unlike `livegpu_bench`,
    // which reads both from one data directory it is safe to assume is the
    // user's real install.
    let mut args = std::env::args().skip(1);
    let models_dir = args
        .next()
        .map(PathBuf::from)
        .context("usage: light_mode_bench <models-dir> <segments-dir> [audio-seconds]")?;
    let segments_dir = args
        .next()
        .map(PathBuf::from)
        .context("usage: light_mode_bench <models-dir> <segments-dir> [audio-seconds]")?;
    let target_s: f64 = args.next().map_or(Ok(1800.0), |s| s.parse())?;

    let cfg = Config::default();
    let root = ModelSet::resolve_at(models_dir, &cfg.models);
    anyhow::ensure!(
        root.has_asr_export(&DEFAULT_ASR),
        "the default export is not installed under {}",
        root.root.display()
    );
    anyhow::ensure!(
        root.has_asr_export(&FALLBACK_ASR),
        "the light export (110m) is not installed under {} — \
         `recalld models fetch --fallback-asr` (also `--light`) installs it",
        root.root.display()
    );

    println!("models:     {}", root.root.display());
    println!("segments:   {}", segments_dir.display());
    println!("asr threads:{}", root.asr_threads);

    let clips = collect(&segments_dir, target_s, 0.3)?;
    let audio_s: f64 = clips.iter().map(Clip::seconds).sum();
    println!(
        "clips:      {} totalling {:.1} s ({:.1} min)\n",
        clips.len(),
        audio_s,
        audio_s / 60.0
    );

    let default_models = root.with_asr(&DEFAULT_ASR);
    let light_models = root.with_asr(&FALLBACK_ASR);

    let t_load = Instant::now();
    let mut default_asr = Asr::load(&default_models)?;
    let mut light_asr = Asr::load(&light_models)?;
    println!(
        "loaded {} and {} in {:.2}s\n",
        default_asr.model_id(),
        light_asr.model_id(),
        t_load.elapsed().as_secs_f64()
    );

    use std::collections::BTreeMap;
    let mut buckets: BTreeMap<&'static str, BucketStats> = BTreeMap::new();
    let mut total_cpu_default = 0.0;
    let mut total_cpu_light = 0.0;
    let mut wer_sum = 0.0;
    let mut wer_n = 0usize;

    for c in &clips {
        // Interleaved: default, then light, every clip — never a whole sweep
        // of one model followed by a whole sweep of the other.
        let cpu0 = cpu_secs();
        let default_text = default_asr.transcribe(&c.samples);
        let cpu_default = cpu_secs() - cpu0;

        let cpu0 = cpu_secs();
        let light_text = light_asr.transcribe(&c.samples);
        let cpu_light = cpu_secs() - cpu0;

        let secs = c.seconds();
        let bucket = buckets.entry(bucket_of(secs)).or_default();
        bucket.clips += 1;
        bucket.audio_s += secs;
        bucket.cpu_default += cpu_default;
        bucket.cpu_light += cpu_light;
        total_cpu_default += cpu_default;
        total_cpu_light += cpu_light;

        if let Some(w) = wer(&default_text, &light_text) {
            bucket.wer_sum += w;
            bucket.wer_n += 1;
            wer_sum += w;
            wer_n += 1;
        }
    }

    // Order the report the way the fragment lengths are declared, not the
    // order clips happened to arrive in.
    println!(
        "| duration | clips | audio min | CPU s/min (0.6b-v3) | CPU s/min (110m) | Δ CPU | WER (110m vs 0.6b-v3) |"
    );
    println!("|---|---:|---:|---:|---:|---:|---:|");
    for (_, name) in BUCKETS {
        let Some(b) = buckets.get(name) else { continue };
        if b.clips == 0 {
            continue;
        }
        let per_min_default = b.cpu_default / (b.audio_s / 60.0);
        let per_min_light = b.cpu_light / (b.audio_s / 60.0);
        let delta = (per_min_light - per_min_default) / per_min_default * 100.0;
        let wer_pct = if b.wer_n > 0 {
            format!("{:.1}%", b.wer_sum / b.wer_n as f64 * 100.0)
        } else {
            "-".into()
        };
        println!(
            "| {name} | {} | {:.1} | {:.2} | {:.2} | {:+.1}% | {wer_pct} |",
            b.clips,
            b.audio_s / 60.0,
            per_min_default,
            per_min_light,
            delta,
        );
    }

    let per_min_default = total_cpu_default / (audio_s / 60.0);
    let per_min_light = total_cpu_light / (audio_s / 60.0);
    let delta = (per_min_light - per_min_default) / per_min_default * 100.0;
    println!(
        "\noverall: {:.2} CPU s/audio-min (0.6b-v3) -> {:.2} CPU s/audio-min (110m), {:+.1}%",
        per_min_default, per_min_light, delta
    );
    if wer_n > 0 {
        println!(
            "overall WER, 110m vs 0.6b-v3 as reference: {:.1}% over {wer_n} clips with words in the reference",
            wer_sum / wer_n as f64 * 100.0
        );
    }
    println!(
        "\ngate: CPU >= -50% in light mode. {}",
        if delta <= -50.0 { "PASS" } else { "FAIL" }
    );
    Ok(())
}
