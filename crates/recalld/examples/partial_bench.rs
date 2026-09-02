//! Are partial captions worth showing, and what do they cost? (0.11.0)
//!
//! ```text
//! NXR_MODELS=/path/to/models \
//!   cargo run --release -p recalld --example partial_bench -- fixtures
//! ```
//!
//! # What it measures, and why these two numbers
//!
//! **1. Speech-to-glass latency.** For every word the daemon ends up
//! transcribing, when could a person have read it? Twice: in a partial, and in
//! the ordinary `segment`. The second number is the one that makes the feature
//! interesting and it is mostly not decode time at all — a turn is not released
//! until the VAD has heard `min_silence` (500 ms) of quiet AND the turn merger
//! has waited out `turn_merge_gap` (1.5 s) to see whether the person carries
//! on. Two seconds of the final's latency is the daemon deciding the sentence
//! is over.
//!
//! A word "appeared" in a partial at the first partial that agrees with the
//! final on the whole prefix up to and including it. That is deliberately
//! conservative — a word that flickers in, out and back counts from the reading
//! that stuck — because the number is supposed to answer "when could somebody
//! have read this", not "when did these characters first exist".
//!
//! **2. Convergence.** How much of the LAST partial survives into the final:
//! word-level agreement, aligned (so one inserted word does not fail the rest).
//! If the last thing on screen before the row settles is mostly wrong, the
//! honest thing is to not show it.
//!
//! **3. The CPU delta.** The same fixtures twice through the same recogniser —
//! once as 0.10.2 ran them, once with partials on — and the process's own CPU
//! time either way, per minute of speech. Not the 5% design budget: the live
//! pipeline's measured cost is the denominator.
//!
//! # One recogniser, one process
//!
//! [`TimedAsr`] does both jobs. It is the same model, the same export and the
//! same decoding contract as the live `Asr`, and it additionally hands back
//! token timestamps, which is where the words' start times come from. Loading
//! a second recogniser to get them would put two copies of the encoder in RAM
//! for a bench about not spending resources.
//!
//! Numbers live in `spike/FINDINGS.md` §20.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};

use recalld::asr::{TimedAsr, Word, normalise_words};
use recalld::config::{Config, SAMPLE_RATE};
use recalld::models::ModelSet;
use recalld::partial::{Cadence, PartialState};
use recalld::vad::{FRAME_SAMPLES, Segmenter, SileroVad};

/// Silence appended to each fixture so the VAD closes the turn the way it would
/// in the field. Without it every clip ends on `flush()` and the final's
/// latency would be missing the two seconds that are the whole reason partials
/// are worth having.
const TAIL_SILENCE_S: f32 = 2.5;

fn main() -> Result<()> {
    let root: PathBuf = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "fixtures".into())
        .into();
    let models_dir = std::env::var("NXR_MODELS")
        .context("NXR_MODELS must name a directory holding the model set")?;

    let mut cfg = Config::default();
    cfg.models.dir = Some(std::path::PathBuf::from(&models_dir));
    let mut models = ModelSet::resolve(&cfg.models).context("resolving the model set")?;
    models.select_asr();
    println!("model: {}\n", models.asr_model_id());

    let mut asr = TimedAsr::load(&models)?;
    let mut vad = SileroVad::from_bytes(recalld::VAD_MODEL)?;
    let seg_cfg = recalld::ingest::segmenter_config(&cfg);
    let cadence = Cadence::from_config(&cfg.asr);

    let clips = collect(&root)?;
    anyhow::ensure!(
        !clips.is_empty(),
        "no .wav fixtures under {}",
        root.display()
    );

    // ---- arm A: the pipeline as 0.10.2 runs it -----------------------------
    let mut audio_s = 0.0f64;
    let cpu_before = cpu_seconds();
    for clip in &clips {
        let samples = read_wav(clip)?;
        audio_s += samples.len() as f64 / SAMPLE_RATE as f64;
        run_clip(&mut asr, &mut vad, seg_cfg, cadence, &cfg, &samples, false);
    }
    let cpu_baseline = cpu_seconds() - cpu_before;

    // ---- arm B: the same thing with partials on ----------------------------
    let mut runs = Vec::new();
    let cpu_before = cpu_seconds();
    for clip in &clips {
        let samples = read_wav(clip)?;
        let mut r = run_clip(&mut asr, &mut vad, seg_cfg, cadence, &cfg, &samples, true);
        for t in &mut r {
            t.clip = clip
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into();
        }
        runs.extend(r);
    }
    let cpu_partials = cpu_seconds() - cpu_before;

    report(&runs, audio_s, cpu_baseline, cpu_partials);

    // ---- the knob ----------------------------------------------------------
    //
    // If the shipped cadence fails the CPU gate, the useful next question is
    // which cadence would not. Same arms, same fixtures, `partial_every_ms`
    // swept — so the answer in FINDINGS is a number somebody can put in a
    // config file rather than "turn it down a bit".
    println!("\n## the cadence knob\n");
    println!(
        "| `partial_every_ms` | partials | CPU s | vs baseline | median partial latency | convergence |"
    );
    println!("|---:|---:|---:|---:|---:|---:|");
    for every_ms in [1000u64, 1500, 2000, 3000, 5000] {
        let c = Cadence {
            every_ms,
            ..cadence
        };
        let mut swept = Vec::new();
        let before = cpu_seconds();
        for clip in &clips {
            let samples = read_wav(clip)?;
            swept.extend(run_clip(
                &mut asr, &mut vad, seg_cfg, c, &cfg, &samples, true,
            ));
        }
        let cpu = cpu_seconds() - before;
        let (n, lat, conv) = summarise(&swept);
        let delta = (cpu - cpu_baseline) / cpu_baseline.max(1e-9) * 100.0;
        println!(
            "| {every_ms} | {n} | {cpu:.2} | {delta:+.0}% | {lat:.2} s | {:.1}% |",
            conv * 100.0
        );
    }
    Ok(())
}

/// (partials decoded, median partial latency, weighted convergence).
fn summarise(turns: &[Turn]) -> (usize, f64, f64) {
    let mut lat = Vec::new();
    let mut num = 0.0f64;
    let mut den = 0.0f64;
    let mut n = 0usize;
    for t in turns {
        n += t.partials.len();
        if t.final_words.is_empty() {
            continue;
        }
        for (i, _) in t.final_words.iter().enumerate() {
            let spoken = t.start_s + t.final_starts.get(i).copied().unwrap_or(0.0) as f64;
            if let Some(shot) = t
                .partials
                .iter()
                .find(|p| common_prefix(&p.words, &t.final_words) > i)
            {
                lat.push(shot.at_s + shot.cost_s - spoken);
            }
        }
        if let Some(last) = t.partials.last() {
            num += agreement(&last.words, &t.final_words) * t.final_words.len() as f64;
            den += t.final_words.len() as f64;
        }
    }
    (n, median(&mut lat), if den > 0.0 { num / den } else { 0.0 })
}

// ---------------------------------------------------------------------------
// the simulation
// ---------------------------------------------------------------------------

/// One partial as it would have reached a client: the audio time its window
/// ended at, what the decode cost in wall clock, and the words it carried.
struct Shot {
    /// Audio time (seconds from the start of the clip) of the last sample in
    /// the decoded window.
    at_s: f64,
    /// Wall-clock seconds the decode took. Added to `at_s` this is when the
    /// words could actually have been on screen.
    cost_s: f64,
    words: Vec<String>,
}

/// One turn the VAD produced, with everything measured about it.
struct Turn {
    clip: String,
    /// Audio time the turn's padded window starts at.
    start_s: f64,
    /// Audio time the pipeline RELEASES the turn: the merger's poll fires
    /// `turn_merge_gap` after the last voiced frame.
    released_s: f64,
    final_cost_s: f64,
    final_words: Vec<String>,
    /// Word start times from the final decode, relative to `start_s`.
    final_starts: Vec<f32>,
    partials: Vec<Shot>,
}

fn run_clip(
    asr: &mut TimedAsr,
    vad: &mut SileroVad,
    seg_cfg: recalld::vad::SegmenterConfig,
    cadence: Cadence,
    cfg: &Config,
    samples: &[f32],
    partials_on: bool,
) -> Vec<Turn> {
    let mut state = vad.new_state();
    let mut segmenter = Segmenter::new(seg_cfg);
    let mut merger = recalld::ingest::turn_merger(cfg);
    let mut partial = PartialState::default();

    // The clip plus the silence that lets the VAD decide it is over.
    let mut audio = samples.to_vec();
    audio.extend(vec![0.0f32; (TAIL_SILENCE_S * SAMPLE_RATE as f32) as usize]);

    let sec = |n: u64| n as f64 / SAMPLE_RATE as f64;
    let mut out: Vec<Turn> = Vec::new();
    let mut open: Vec<Shot> = Vec::new();
    let mut cursor = 0u64;

    while cursor + FRAME_SAMPLES as u64 <= audio.len() as u64 {
        let frame: [f32; FRAME_SAMPLES] = audio[cursor as usize..cursor as usize + FRAME_SAMPLES]
            .try_into()
            .expect("exactly one frame");
        let prob = vad.frame(&frame, &mut state).unwrap_or(0.0);
        let mut closed = Vec::new();
        if let Some(span) = segmenter.push_frame(prob, cursor, FRAME_SAMPLES as u64) {
            closed.extend(merger.push(span));
        }
        cursor += FRAME_SAMPLES as u64;
        closed.extend(merger.poll(cursor));

        for span in closed {
            let slice = &audio[span.start as usize..(span.end as usize).min(audio.len())];
            let t0 = Instant::now();
            let (text, words) = asr.transcribe_timed(slice);
            let cost = t0.elapsed().as_secs_f64();
            let starts = words.iter().map(|w| w.start_s).collect::<Vec<_>>();
            out.push(Turn {
                clip: String::new(),
                start_s: sec(span.start),
                // The merger releases a turn the frame after the gap has
                // passed, which is where `cursor` is standing right now.
                released_s: sec(cursor),
                final_cost_s: cost,
                final_words: words_of(&text, &words),
                final_starts: starts,
                partials: std::mem::take(&mut open),
            });
            partial.turn_closed(0, None);
        }

        if !partials_on {
            continue;
        }
        // The daemon's own rule, with audio time standing in for the wall
        // clock: the bench is not real-time, and a machine that keeps up is
        // exactly the machine the cadence is written for. A machine that does
        // NOT keep up is the backlog rule's job and is not modelled here.
        let Some(start) = open_start(&merger, &segmenter) else {
            continue;
        };
        let elapsed_ms = ((cursor.saturating_sub(start)) * 1000) / SAMPLE_RATE as u64;
        let now_ms = (cursor * 1000) / SAMPLE_RATE as u64;
        // `t_start_ns` stands in for the turn's identity; sample index is a
        // fine surrogate here and keeps the bench off the clock.
        if !partial.due(start as i64, elapsed_ms, now_ms, cadence) {
            continue;
        }
        let slice = &audio[start as usize..cursor as usize];
        let t0 = Instant::now();
        let (text, words) = asr.transcribe_timed(slice);
        let cost = t0.elapsed().as_secs_f64();
        if normalise_words(&text).is_empty() {
            continue;
        }
        partial.mark(start as i64, now_ms);
        open.push(Shot {
            at_s: sec(cursor),
            cost_s: cost,
            words: words_of(&text, &words),
        });
    }

    // Whatever is still open when the audio runs out.
    let mut tail = Vec::new();
    if let Some(span) = segmenter.flush() {
        tail.extend(merger.push(span));
    }
    tail.extend(merger.flush());
    for span in tail {
        let slice = &audio[span.start as usize..(span.end as usize).min(audio.len())];
        let t0 = Instant::now();
        let (text, words) = asr.transcribe_timed(slice);
        let cost = t0.elapsed().as_secs_f64();
        let starts = words.iter().map(|w| w.start_s).collect::<Vec<_>>();
        out.push(Turn {
            clip: String::new(),
            start_s: sec(span.start),
            released_s: sec(cursor),
            final_cost_s: cost,
            final_words: words_of(&text, &words),
            final_starts: starts,
            partials: std::mem::take(&mut open),
        });
    }
    out
}

/// [`recalld::pipeline`]'s `open_turn_start`, which is private to it.
fn open_start(merger: &recalld::turns::TurnMerger, segmenter: &Segmenter) -> Option<u64> {
    match (merger.pending_start(), segmenter.speech_open()) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) | (None, Some(a)) => Some(a),
        (None, None) => None,
    }
}

/// The decode's words, normalised, preferring the tokeniser's own grouping and
/// falling back to whitespace when a decode came back with no timestamps.
fn words_of(text: &str, words: &[Word]) -> Vec<String> {
    if words.is_empty() {
        return normalise_words(text);
    }
    words
        .iter()
        .flat_map(|w| normalise_words(&w.text))
        .collect()
}

// ---------------------------------------------------------------------------
// the report
// ---------------------------------------------------------------------------

fn report(turns: &[Turn], audio_s: f64, cpu_baseline: f64, cpu_partials: f64) {
    let mut partial_lat: Vec<f64> = Vec::new();
    let mut final_lat: Vec<f64> = Vec::new();
    let mut agreements: Vec<(String, f64, usize)> = Vec::new();
    let mut with_partials = 0usize;
    let mut partial_count = 0usize;

    for t in turns {
        if t.final_words.is_empty() {
            continue;
        }
        partial_count += t.partials.len();
        if !t.partials.is_empty() {
            with_partials += 1;
        }
        // When each word of the final could have been read, in the final.
        for (i, w) in t.final_words.iter().enumerate() {
            let _ = w;
            let spoken = t.start_s + t.final_starts.get(i).copied().unwrap_or(0.0) as f64;
            final_lat.push(t.released_s + t.final_cost_s - spoken);
            // …and in a partial, if one ever agreed with the final this far.
            if let Some(shot) = t
                .partials
                .iter()
                .find(|p| common_prefix(&p.words, &t.final_words) > i)
            {
                partial_lat.push(shot.at_s + shot.cost_s - spoken);
            }
        }
        if let Some(last) = t.partials.last() {
            agreements.push((
                t.clip.clone(),
                agreement(&last.words, &t.final_words),
                t.final_words.len(),
            ));
        }
    }

    println!(
        "turns: {} ({} with at least one partial)",
        turns.len(),
        with_partials
    );
    println!("partials decoded: {partial_count}");
    println!("audio: {audio_s:.1} s\n");

    println!("## speech-to-glass latency (seconds from the word being spoken)\n");
    println!("| reading | n | median | p90 |");
    println!("|---|---:|---:|---:|");
    row("partial", &mut partial_lat);
    row("final segment", &mut final_lat);
    let saved = median(&mut final_lat.clone()) - median(&mut partial_lat.clone());
    println!("\nmedian saved by a partial: **{saved:.2} s**\n");

    println!("## convergence — last partial against the final\n");
    let mean = if agreements.is_empty() {
        0.0
    } else {
        // Weighted by turn length: a one-word turn agreeing perfectly is not
        // worth as much evidence as a twenty-word one.
        let num: f64 = agreements.iter().map(|(_, a, n)| a * *n as f64).sum();
        let den: f64 = agreements.iter().map(|(_, _, n)| *n as f64).sum();
        num / den
    };
    let unweighted = if agreements.is_empty() {
        0.0
    } else {
        agreements.iter().map(|(_, a, _)| a).sum::<f64>() / agreements.len() as f64
    };
    println!("turns scored: {}", agreements.len());
    println!(
        "word agreement, weighted by turn length: **{:.1}%**",
        mean * 100.0
    );
    println!("word agreement, per turn: {:.1}%\n", unweighted * 100.0);

    println!("## CPU\n");
    let minutes = audio_s / 60.0;
    let base_per_min = cpu_baseline / minutes.max(1e-9);
    let part_per_min = cpu_partials / minutes.max(1e-9);
    let delta = if cpu_baseline > 0.0 {
        (cpu_partials - cpu_baseline) / cpu_baseline
    } else {
        f64::NAN
    };
    println!("| arm | process CPU s | CPU s per minute of speech |");
    println!("|---|---:|---:|");
    println!("| 0.10.2 (VAD + one decode per turn) | {cpu_baseline:.2} | {base_per_min:.2} |");
    println!("| with partials | {cpu_partials:.2} | {part_per_min:.2} |");
    println!(
        "\npartials add **{:.1}%** CPU over the live pipeline's own cost\n",
        delta * 100.0
    );

    println!("## gate\n");
    let cpu_ok = delta <= 0.20;
    let conv_ok = mean >= 0.70;
    println!("- CPU ≤ 20%: {} ({:.1}%)", verdict(cpu_ok), delta * 100.0);
    println!(
        "- agreement ≥ 70%: {} ({:.1}%)",
        verdict(conv_ok),
        mean * 100.0
    );
    println!(
        "\n**{}** — partials ship {} by default",
        if cpu_ok && conv_ok { "PASS" } else { "FAIL" },
        if cpu_ok && conv_ok { "ON" } else { "OFF" }
    );
}

fn verdict(ok: bool) -> &'static str {
    if ok { "PASS" } else { "FAIL" }
}

fn row(name: &str, v: &mut [f64]) {
    if v.is_empty() {
        println!("| {name} | 0 | — | — |");
        return;
    }
    let n = v.len();
    let med = median(v);
    let p90 = percentile(v, 0.90);
    println!("| {name} | {n} | {med:.2} s | {p90:.2} s |");
}

fn median(v: &mut [f64]) -> f64 {
    percentile(v, 0.5)
}

fn percentile(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let i = ((v.len() - 1) as f64 * p).round() as usize;
    v[i]
}

/// How many leading words two readings agree on.
fn common_prefix(a: &[String], b: &[String]) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

/// Aligned word agreement: matches / length of the reference, by the usual edit
/// distance DP. One inserted word must not fail everything after it.
fn agreement(got: &[String], want: &[String]) -> f64 {
    if want.is_empty() {
        return if got.is_empty() { 1.0 } else { 0.0 };
    }
    let (n, m) = (got.len(), want.len());
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut cur = vec![0usize; m + 1];
    for i in 1..=n {
        cur[0] = i;
        for j in 1..=m {
            let sub = prev[j - 1] + usize::from(got[i - 1] != want[j - 1]);
            cur[j] = sub.min(prev[j] + 1).min(cur[j - 1] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    let dist = prev[m];
    1.0 - (dist as f64 / m as f64).min(1.0)
}

// ---------------------------------------------------------------------------
// odds and ends
// ---------------------------------------------------------------------------

fn collect(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    walk(root, &mut out)?;
    // Multi-talker fixtures are not what this measures: a partial over babble
    // is a caption of babble either way, and the overlap gate refuses those
    // turns before identity ever sees them.
    out.retain(|p| {
        let n = p
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        !n.starts_with("duo_")
            && !n.starts_with("trio_")
            && !n.starts_with("lobby_")
            && n != "silence.wav"
            && n != "noise.wav"
    });
    out.sort();
    Ok(out)
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let p = entry?.path();
        if p.is_dir() {
            walk(&p, out)?;
        } else if p.extension().is_some_and(|e| e == "wav") {
            out.push(p);
        }
    }
    Ok(())
}

fn read_wav(path: &Path) -> Result<Vec<f32>> {
    let mut r = hound::WavReader::open(path)?;
    anyhow::ensure!(
        r.spec().sample_rate == SAMPLE_RATE && r.spec().channels == 1,
        "{} is not 16 kHz mono",
        path.display()
    );
    Ok(r.samples::<i16>()
        .map(|s| s.unwrap_or(0) as f32 / 32768.0)
        .collect())
}

/// User + system CPU for the whole process, including the recogniser's own
/// worker threads. That is the number the gate is about: what this daemon
/// takes off the machine, not what one thread spent.
fn cpu_seconds() -> f64 {
    // SAFETY: a zeroed rusage filled by the kernel; RUSAGE_SELF is always valid.
    unsafe {
        let mut ru: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut ru) != 0 {
            return f64::NAN;
        }
        let s = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
        s(ru.ru_utime) + s(ru.ru_stime)
    }
}
