//! What the live path actually costs, per model, on real turns.
//!
//! Every question in the "move live inference to the GPU" round reduces to one
//! table this repo did not have: for each model on the live path, **CPU seconds
//! per minute of audio**, measured on the user's own segments rather than on
//! read speech. §10 and §11 measured the ASR leg alone, in RTF, on lab audio.
//! That is not the same number as "what fraction of a core is this daemon
//! burning while somebody talks", and the GPU decision needs the second one.
//!
//! Read-only. It opens the segment WAVs and the model files and writes nothing
//! anywhere.
//!
//! CPU time is `getrusage(RUSAGE_SELF)` (user + sys), which counts *every*
//! thread the process has — the ORT and sherpa intra-op pools included. That is
//! the honest denominator: a stage configured with four threads that finishes
//! in a quarter of the wall time has not become cheaper.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};

use recalld::asr::Asr;
use recalld::config::{Config, SAMPLE_RATE};
use recalld::embed::Embedder;
use recalld::models::ModelSet;
use recalld::overlap::OverlapDetector;
use recalld::vad::{FRAME_SAMPLES, SileroVad};

// ---------------------------------------------------------------------------
// clocks
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

/// Peak resident set of this process, in MiB.
fn peak_rss_mib() -> f64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    ru.ru_maxrss as f64 / 1024.0
}

/// amdgpu's own busy counter for the card with real VRAM, the same one
/// `crate::night::gpu_busy_pct` reads.
fn gpu_busy() -> Option<u32> {
    recalld::night::gpu_busy_pct()
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
///
/// Deterministic order (sorted paths) with a stride, rather than "the first N
/// files": the first N are one evening of one session, and the point of using
/// the real archive is to get the real spread of durations and rooms.
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

    // Stride so the sample spans the whole archive rather than its first hour.
    // 21k segments at a ~2.7 s median is roughly 16 hours; a 30-minute sample
    // is about 3% of it.
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
        // The overlap detector refuses anything under its window, and a turn
        // that short does not reach the identity branch either.
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
// one stage's row
// ---------------------------------------------------------------------------

struct Row {
    stage: &'static str,
    device: &'static str,
    /// Wall seconds for the whole sweep, model load excluded.
    wall_s: f64,
    /// CPU seconds for the whole sweep, model load excluded.
    cpu_s: f64,
    /// Model load, wall seconds. Reported separately because it is paid once
    /// per daemon start on the live path and once per batch on the night path.
    load_s: f64,
    audio_s: f64,
    /// Per-clip wall time, sorted, for the latency percentiles.
    per_clip_ms: Vec<f64>,
    gpu_busy_during: Vec<u32>,
}

impl Row {
    fn print(&self) {
        let mut p = self.per_clip_ms.clone();
        p.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let pick = |q: f64| -> f64 {
            if p.is_empty() {
                return 0.0;
            }
            p[((p.len() as f64 - 1.0) * q).round() as usize]
        };
        let per_min = self.cpu_s / (self.audio_s / 60.0);
        let busy = if self.gpu_busy_during.is_empty() {
            "-".to_string()
        } else {
            let lo = self.gpu_busy_during.iter().min().unwrap();
            let hi = self.gpu_busy_during.iter().max().unwrap();
            format!("{lo}-{hi}")
        };
        println!(
            "| {} | {} | {:.2} | {:.2} | {:.3} | {:.1} | {:.1} | {:.1} | {} |",
            self.stage,
            self.device,
            per_min,
            self.cpu_s / self.wall_s.max(1e-9),
            self.cpu_s / self.audio_s,
            pick(0.5),
            pick(0.95),
            self.load_s * 1000.0,
            busy,
        );
    }
}

fn header() {
    println!(
        "\n| stage | device | CPU s / audio min | cores used | RTF (CPU) | p50 ms | p95 ms | load ms | gpu% |"
    );
    println!("|---|---|---:|---:|---:|---:|---:|---:|---:|");
}

/// Time one stage over every clip.
fn sweep<F>(stage: &'static str, device: &'static str, clips: &[Clip], load_s: f64, mut f: F) -> Row
where
    F: FnMut(&Clip) -> Result<()>,
{
    let mut per_clip_ms = Vec::with_capacity(clips.len());
    let mut gpu = Vec::new();
    let cpu0 = cpu_secs();
    let t0 = Instant::now();
    for (i, c) in clips.iter().enumerate() {
        let s = Instant::now();
        if let Err(e) = f(c) {
            eprintln!("  {stage}: {} failed: {e:#}", c.path.display());
            continue;
        }
        per_clip_ms.push(s.elapsed().as_secs_f64() * 1000.0);
        // Sampled rather than read every clip: the sysfs read is a syscall and
        // this loop is the thing being timed.
        if i % 25 == 0 && let Some(b) = gpu_busy() {
            gpu.push(b);
        }
    }
    Row {
        stage,
        device,
        wall_s: t0.elapsed().as_secs_f64(),
        cpu_s: cpu_secs() - cpu0,
        load_s,
        audio_s: clips.iter().map(Clip::seconds).sum(),
        per_clip_ms,
        gpu_busy_during: gpu,
    }
}

/// Run `f`, returning its value and how long the call took.
fn timed<T>(f: impl FnOnce() -> Result<T>) -> Result<(T, f64)> {
    let t = Instant::now();
    let v = f()?;
    Ok((v, t.elapsed().as_secs_f64()))
}

// ---------------------------------------------------------------------------
// candidate (b): whisper.cpp on Vulkan, as a LIVE decoder
// ---------------------------------------------------------------------------

/// VRAM in use on the card with real VRAM, in MiB.
fn vram_used_mib() -> Option<u64> {
    let entries = std::fs::read_dir("/sys/class/drm").ok()?;
    for e in entries.flatten() {
        let dev = e.path().join("device");
        let total = std::fs::read_to_string(dev.join("mem_info_vram_total"))
            .ok()?
            .trim()
            .parse::<u64>()
            .ok()?;
        if total < (8 << 30) {
            continue;
        }
        if let Ok(used) = std::fs::read_to_string(dev.join("mem_info_vram_used"))
            && let Ok(b) = used.trim().parse::<u64>()
        {
            return Some(b / (1024 * 1024));
        }
    }
    None
}

/// One `whisper-cli` invocation per turn, which is what a live GPU decoder
/// built on the night shift's runtime would actually do.
///
/// The night shift never pays this shape: it concatenates `[night].batch_rows`
/// clips and decodes them in **one** invocation, precisely because §13 measured
/// that the model load, not the audio, is the cost. A live decoder cannot batch
/// — the turn has to come back before the next one starts — so this leg is the
/// honest version of candidate (b) and the load is part of every row.
fn whisper_sweep(models_root: &Path, clips: &[Clip]) -> Result<()> {
    use std::process::{Command, Stdio};

    let cli = models_root.join("whisper/whisper-cli");
    let model = models_root.join("ggml-large-v3-q5_0.bin");
    anyhow::ensure!(cli.is_file(), "{} is not built", cli.display());
    anyhow::ensure!(model.is_file(), "{} is missing", model.display());

    // Bounded: this leg spawns a process per clip and each one reloads a
    // gigabyte of weights. 150 turns is enough for a p95.
    let clips: Vec<&Clip> = clips.iter().take(150).collect();
    let audio_s: f64 = clips.iter().map(|c| c.seconds()).sum();

    let vram_idle = vram_used_mib();
    println!("whisper-cli: {}", cli.display());
    println!("vram before: {vram_idle:?} MiB\n");

    // The control. Same binary, same clips, same GPU — but ONE invocation, so
    // the model is loaded once instead of 150 times. This is the night shift's
    // shape, and the gap between the two rows is the whole answer to "why is
    // the GPU decoder slower than the CPU one".
    if std::env::var("LIVEGPU_WHISPER_BATCH").is_ok() {
        let t = Instant::now();
        let mut cmd = Command::new(&cli);
        cmd.arg("-m").arg(&model);
        for c in &clips {
            cmd.arg("-f").arg(&c.path);
        }
        let st = cmd
            .args(["-l", "auto", "-t", "4", "-np", "-nt"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        let wall = t.elapsed().as_secs_f64();
        let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: owned `rusage`, RUSAGE_CHILDREN takes no other argument.
        unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut ru) };
        let s = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
        let cpu = s(ru.ru_utime) + s(ru.ru_stime);
        println!(
            "batched ({st}): {} clips, {audio_s:.1} s audio, {wall:.1} s wall, \
             {cpu:.1} CPU s = {:.2} CPU s per audio minute, RTF {:.3}",
            clips.len(),
            cpu / (audio_s / 60.0),
            wall / audio_s,
        );
        return Ok(());
    }

    let mut per_clip_ms = Vec::new();
    let mut busy = Vec::new();
    let mut vram_peak = 0u64;
    let cpu0 = cpu_secs();
    let t0 = Instant::now();
    for (i, c) in clips.iter().enumerate() {
        let s = Instant::now();
        let out = Command::new(&cli)
            .arg("-m")
            .arg(&model)
            .arg("-f")
            .arg(&c.path)
            .args(["-l", "auto", "-t", "4", "-np", "-nt"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        match out {
            Ok(st) if st.success() => per_clip_ms.push(s.elapsed().as_secs_f64() * 1000.0),
            Ok(st) => eprintln!("  {} exited {st}", c.path.display()),
            Err(e) => eprintln!("  {} failed: {e:#}", c.path.display()),
        }
        if i % 5 == 0 {
            if let Some(b) = gpu_busy() {
                busy.push(b);
            }
            if let Some(v) = vram_used_mib() {
                vram_peak = vram_peak.max(v);
            }
        }
    }
    let wall = t0.elapsed().as_secs_f64();

    // Children, not this process: the decode happened in `whisper-cli`.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `ru` is a valid owned `rusage`; RUSAGE_CHILDREN takes no other
    // argument and reports every reaped child.
    unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut ru) };
    let s = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    let child_cpu = s(ru.ru_utime) + s(ru.ru_stime);
    let _ = cpu0;

    per_clip_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pick = |q: f64| per_clip_ms[((per_clip_ms.len() as f64 - 1.0) * q).round() as usize];

    header();
    println!(
        "| ASR (whisper-large-v3-q5_0) | vulkan | {:.2} | {:.2} | {:.3} | {:.1} | {:.1} | (per call) | {} |",
        child_cpu / (audio_s / 60.0),
        child_cpu / wall,
        child_cpu / audio_s,
        pick(0.5),
        pick(0.95),
        busy
            .iter()
            .min()
            .map(|lo| format!("{lo}-{}", busy.iter().max().unwrap()))
            .unwrap_or_else(|| "-".into()),
    );
    println!(
        "\n{} turns, {:.1} s of audio, {:.1} s wall",
        per_clip_ms.len(),
        audio_s,
        wall
    );
    println!("vram idle {vram_idle:?} MiB, peak during {vram_peak} MiB");
    Ok(())
}

// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let data_dir = args
        .next()
        .map(PathBuf::from)
        .context("usage: livegpu_bench <data-dir> [audio-seconds]")?;
    let target_s: f64 = args.next().map_or(Ok(1800.0), |s| s.parse())?;

    // `whisper` runs candidate (b) instead of the CPU sweep: the same clips,
    // one `whisper-cli` invocation each, on the Vulkan build `models
    // build-night` compiled.
    let whisper_leg = std::env::var("LIVEGPU_WHISPER").is_ok();

    let cfg = Config::default();
    let models = ModelSet::resolve_at(data_dir.join("models"), &cfg.models);

    println!("data dir:   {}", data_dir.display());
    println!("models:     {}", models.root.display());
    println!("asr threads:{}", models.asr_threads);
    println!("gpu busy before: {:?}", gpu_busy());

    let clips = collect(&data_dir.join("segments"), target_s, 1.0)?;
    let audio_s: f64 = clips.iter().map(Clip::seconds).sum();
    let mut durs: Vec<f64> = clips.iter().map(Clip::seconds).collect();
    durs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "clips:      {} totalling {:.1} s ({:.1} min), median {:.2} s\n",
        clips.len(),
        audio_s,
        audio_s / 60.0,
        durs[durs.len() / 2],
    );

    if whisper_leg {
        return whisper_sweep(&models.root, &clips);
    }

    header();
    let mut rows = Vec::new();

    // --- Silero VAD. Not per turn: per 32 ms frame, over the whole stream.
    // The live daemon runs this on every frame of every session it is
    // subscribed to, speech or not, which makes it the only stage whose cost
    // is set by wall-clock time rather than by how much anybody said.
    {
        let (mut vad, load) = timed(|| SileroVad::from_bytes(recalld::VAD_MODEL))?;
        let mut state = vad.new_state();
        rows.push(sweep("VAD (silero)", "cpu", &clips, load, |c| {
            for f in c.samples.chunks_exact(FRAME_SAMPLES) {
                vad.frame(f, &mut state)?;
            }
            Ok(())
        }));
        rows.last().unwrap().print();
    }

    // --- pyannote segmentation-3.0, the overlap gate. First model on the
    // per-turn path.
    {
        let (mut ovl, load) = timed(|| OverlapDetector::load(&models.segmentation))?;
        rows.push(sweep("overlap (pyannote)", "cpu", &clips, load, |c| {
            ovl.overlap_frac(&c.samples)?;
            Ok(())
        }));
        rows.last().unwrap().print();
    }

    // --- Parakeet TDT 0.6b v3. The heavy one, and the whole reason this
    // round exists.
    {
        let (mut asr, load) = timed(|| Asr::load(&models))?;
        rows.push(sweep("ASR (parakeet-tdt-0.6b-v3)", "cpu", &clips, load, |c| {
            asr.transcribe(&c.samples);
            Ok(())
        }));
        rows.last().unwrap().print();
    }

    // --- ERes2Net speaker embedding.
    {
        let (mut emb, load) = timed(|| Embedder::load(&models))?;
        rows.push(sweep("embed (eres2net)", "cpu", &clips, load, |c| {
            emb.embed(&c.samples, SAMPLE_RATE)?;
            Ok(())
        }));
        rows.last().unwrap().print();
    }

    let total_cpu: f64 = rows.iter().map(|r| r.cpu_s).sum();
    let per_min = total_cpu / (audio_s / 60.0);
    println!(
        "\nwhole live path: {:.2} CPU s per audio minute = {:.1}% of one core \
         while somebody is talking",
        per_min,
        per_min / 60.0 * 100.0
    );
    println!("peak RSS: {:.0} MiB", peak_rss_mib());
    println!("gpu busy after: {:?}", gpu_busy());
    Ok(())
}
