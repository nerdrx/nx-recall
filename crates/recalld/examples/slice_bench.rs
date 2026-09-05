//! Does slicing a long turn put words on the glass sooner, and what does it
//! cost in CPU and in words? (0.12.5)
//!
//! ```text
//! NXR_MODELS=/path/to/models \
//!   cargo run --release -p recalld --example slice_bench -- corpus [long]
//! ```
//!
//! # The three numbers, and why these three
//!
//! **1. Caption latency.** Two readings, because a sliced turn changes two
//! different things. *Turn end → row published* is how long after the last word
//! the finished row lands; slicing should make it slightly BETTER, because the
//! final decode is only the remainder rather than the whole turn. *Word spoken →
//! word shown* is the number the feature exists for: for a word in the middle of
//! a long turn, when could a person read it? Both are reported as histograms,
//! because the median hides the entire point — the mean turn is 1.6 s long and
//! is never sliced, and the distribution's tail is where the complaint lives.
//!
//! **2. CPU per audio minute.** The same corpus twice through the same
//! recogniser in one process, arm A as 0.12.3 runs it and arm B with slicing on.
//! The gate is **+10%**, a fifth of what partials were allowed and did not make
//! (FINDINGS §20 measured those at +553% to +702%). Slicing should be far
//! inside it, because the arithmetic is different in kind: a turn's audio is
//! decoded exactly once whether it was cut into one piece or six, so the whole
//! delta is per-decode overhead times the extra pieces.
//!
//! **3. WER of sliced-then-joined against the whole-turn decode.** This is the
//! cost the feature actually has. A slice is decoded without the context of the
//! rest of the turn, and FINDINGS §12 measured context as worth a great deal
//! (a 1.5 s window alone at 56.7% WER against 20.4% with ±3 s around it). The
//! claim under test is that a cut made at a VAD dip, at least six seconds in, is
//! a different regime from that — and the way to find out is to take the
//! whole-turn decode as the reference and score the joined text against it.
//!
//! # Two corpora on purpose
//!
//! The CPU number is measured on a sample that reproduces the archive's real
//! turn-length distribution, because that is what the daemon would pay. The WER
//! bound is stated over turns of six seconds and up, because those are the only
//! turns slicing touches — averaging in thousands of turns the feature never
//! looks at would report the sampler rather than the cost.
//!
//! # Two things this bench got wrong first, both now designed against
//!
//! **The arms must interleave.** Written as "all of arm A, then all of arm B",
//! it measured the machine: two sequential half-hour arms straddle a thermal
//! ramp, and `getrusage` reports TIME, so identical work on a downclocked core
//! costs more CPU-seconds. The same code on the same corpus gave **−18.8%** on
//! one run and **+16.3%** on the next. The arms now alternate per clip.
//!
//! **The noise floor has to be measured, not assumed.** An unsliced turn goes
//! down a path this feature does not touch, so its words ought to be identical
//! between arms — and on the first run they were not, which would have meant
//! the recogniser's answer depended on what it decoded before it and that no
//! WER here meant anything. So the pass runs arm A **twice** and reports
//! A-against-A beside A-against-B. The floor came back at **exactly 0.00%**:
//! the decoder is deterministic, the phantom was this bench counting a turn
//! whose only slice decoded to nothing as "unsliced", and every point of the
//! 17.6% is therefore real.
//!
//! Numbers live in `spike/FINDINGS.md` §41.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};

use recalld::asr::{TimedAsr, Word, normalise_words};
use recalld::config::{Config, SAMPLE_RATE};
use recalld::models::ModelSet;
use recalld::slice::Slicer;
use recalld::vad::{FRAME_SAMPLES, Segmenter, SileroVad};

/// Silence appended to each clip so the VAD closes the turn the way it would in
/// the field, rather than on `flush()`. Without it the "turn end → published"
/// number would be missing the two seconds that are the whole reason a caption
/// arrives late.
const TAIL_SILENCE_S: f32 = 2.5;

/// The floor the headline numbers are taken at. NOT the shipped default, which
/// is `0` — see `[captions] slice_after_s` and FINDINGS §41. The knob table at
/// the end of the run is what that decision was made from.
const SLICE_AFTER_S: f32 = 6.0;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let corpus: PathBuf = args.next().unwrap_or_else(|| "fixtures".into()).into();
    let long: Option<PathBuf> = args.next().map(PathBuf::from);
    let models_dir = std::env::var("NXR_MODELS")
        .context("NXR_MODELS must name a directory holding the model set")?;

    let mut cfg = Config::default();
    cfg.models.dir = Some(PathBuf::from(&models_dir));
    let mut models = ModelSet::resolve(&cfg.models).context("resolving the model set")?;
    models.select_asr();
    println!("model: {}", models.asr_model_id());
    println!("slice_after_s: {SLICE_AFTER_S}\n");

    let mut asr = TimedAsr::load(&models)?;
    let mut vad = SileroVad::from_bytes(recalld::VAD_MODEL)?;
    let seg_cfg = recalld::ingest::segmenter_config(&cfg);

    let clips = collect(&corpus)?;
    anyhow::ensure!(
        !clips.is_empty(),
        "no .wav clips under {}",
        corpus.display()
    );

    // ---- the three arms, INTERLEAVED per clip ------------------------------
    //
    // Not "all of arm A, then all of arm B", which is how this was written
    // first and which measured the machine rather than the feature: two
    // sequential half-hour arms straddle a thermal ramp, and `getrusage`
    // reports TIME, so the same work on a downclocked core costs more
    // CPU-seconds. Run back to back, the same code and the same corpus gave
    // −18.8% on one run and +16.3% on the next.
    //
    // Interleaving at clip granularity puts all three arms under the same
    // clock, and rotating which one goes first cancels the residual
    // within-triple ordering bias (a later decode of a clip has its file in
    // page cache).
    //
    // Arm C (0.13.1, FINDINGS §48) is arm B's captions plus one more thing: at
    // turn close, the WHOLE turn is decoded again and that reading — not the
    // joined slices — becomes the row. `redecode_whole_turns` does exactly the
    // extra work `write_segment` now does and nothing else, so `cpu_redecode`
    // is the real marginal cost of the fix, not an estimate of it.
    let mut audio_s = 0.0f64;
    let mut base: Vec<Turn> = Vec::new();
    let mut sliced: Vec<Turn> = Vec::new();
    let mut redecoded: Vec<Turn> = Vec::new();
    let (mut cpu_base, mut cpu_slice, mut cpu_redecode) = (0.0f64, 0.0f64, 0.0f64);
    for (i, clip) in clips.iter().enumerate() {
        let samples = read_wav(clip)?;
        audio_s += samples.len() as f64 / SAMPLE_RATE as f64;
        let mut arm_a = |asr: &mut TimedAsr, vad: &mut SileroVad, out: &mut Vec<Turn>| {
            let t = cpu_seconds();
            out.extend(run_clip(asr, vad, seg_cfg, &cfg, &samples, 0.0, i));
            cpu_base += cpu_seconds() - t;
        };
        let mut arm_b = |asr: &mut TimedAsr, vad: &mut SileroVad, out: &mut Vec<Turn>| {
            let t = cpu_seconds();
            out.extend(run_clip(
                asr,
                vad,
                seg_cfg,
                &cfg,
                &samples,
                SLICE_AFTER_S,
                i,
            ));
            cpu_slice += cpu_seconds() - t;
        };
        let mut arm_c = |asr: &mut TimedAsr, vad: &mut SileroVad, out: &mut Vec<Turn>| {
            let t = cpu_seconds();
            let mut turns = run_clip(asr, vad, seg_cfg, &cfg, &samples, SLICE_AFTER_S, i);
            let audio_ext = with_tail_silence(&samples);
            redecode_whole_turns(asr, &audio_ext, &mut turns);
            cpu_redecode += cpu_seconds() - t;
            out.extend(turns);
        };
        // Three cyclic rotations of the three arms, chosen by clip index. Not
        // every one of the six permutations — the original two-arm version
        // did not try both orderings of a pair either — but every arm leads,
        // trails and sits in the middle across the corpus in equal measure,
        // which is what cancels a systematic first-vs-last bias.
        match i % 3 {
            0 => {
                arm_a(&mut asr, &mut vad, &mut base);
                arm_b(&mut asr, &mut vad, &mut sliced);
                arm_c(&mut asr, &mut vad, &mut redecoded);
            }
            1 => {
                arm_b(&mut asr, &mut vad, &mut sliced);
                arm_c(&mut asr, &mut vad, &mut redecoded);
                arm_a(&mut asr, &mut vad, &mut base);
            }
            _ => {
                arm_c(&mut asr, &mut vad, &mut redecoded);
                arm_a(&mut asr, &mut vad, &mut base);
                arm_b(&mut asr, &mut vad, &mut sliced);
            }
        }
    }

    report(&base, &sliced, audio_s, cpu_base, cpu_slice);
    report_redecode(&base, &redecoded, audio_s, cpu_base, cpu_redecode);

    // ---- the floor that keeps 0.13.1 under the +10% line -------------------
    //
    // `SLICE_AFTER_S` (6 s) is where §41's numbers were taken, not necessarily
    // where 0.13.1 should ship: a higher floor slices a smaller share of the
    // archive's audio, and the whole marginal cost of re-decoding is
    // proportional to that share. Same corpus, same clips, re-read once per
    // floor so each pair of arms is measured under its own thermal window —
    // §41.2's lesson applies here exactly as it did there.
    println!("\n# 0.13.1 — CPU by floor, on the representative corpus\n");
    println!("| `slice_after_s` | CPU s/min | vs off | <= +10%? |");
    println!("|---:|---:|---:|:---:|");
    for floor in [SLICE_AFTER_S, 8.0, 10.0, 12.0] {
        let (mut fa, mut fc) = (0.0f64, 0.0f64);
        for (i, clip) in clips.iter().enumerate() {
            let samples = read_wav(clip)?;
            if i % 2 == 0 {
                let t = cpu_seconds();
                let _ = run_clip(&mut asr, &mut vad, seg_cfg, &cfg, &samples, 0.0, i);
                fa += cpu_seconds() - t;
                let t = cpu_seconds();
                let mut turns = run_clip(&mut asr, &mut vad, seg_cfg, &cfg, &samples, floor, i);
                let audio_ext = with_tail_silence(&samples);
                redecode_whole_turns(&mut asr, &audio_ext, &mut turns);
                fc += cpu_seconds() - t;
            } else {
                let t = cpu_seconds();
                let mut turns = run_clip(&mut asr, &mut vad, seg_cfg, &cfg, &samples, floor, i);
                let audio_ext = with_tail_silence(&samples);
                redecode_whole_turns(&mut asr, &audio_ext, &mut turns);
                fc += cpu_seconds() - t;
                let t = cpu_seconds();
                let _ = run_clip(&mut asr, &mut vad, seg_cfg, &cfg, &samples, 0.0, i);
                fa += cpu_seconds() - t;
            }
        }
        let minutes = audio_s / 60.0;
        let delta = (fc - fa) / fa.max(1e-9);
        println!(
            "| {floor} | {:.2} | {:+.1}% | {} |",
            fc / minutes.max(1e-9),
            delta * 100.0,
            if delta <= 0.10 { "yes" } else { "no" }
        );
    }

    // ---- the WER bound, over the turns slicing actually touches ------------
    if let Some(long) = long {
        let clips = collect(&long)?;
        anyhow::ensure!(!clips.is_empty(), "no .wav clips under {}", long.display());
        println!("\n# the WER bound — turns of {SLICE_AFTER_S} s and up\n");
        // THREE passes, not two. The third is arm A again, byte-identical
        // input and identical code, and it exists because the first run of
        // this bench reported that turns nobody had sliced came back with
        // different words — which can only mean the recogniser's answer
        // depends on what it decoded before it. Whatever the cause, it puts a
        // FLOOR under any WER this bench can report: a difference smaller than
        // A-against-A is not evidence about slicing. Reported rather than
        // assumed away.
        let mut a: Vec<Turn> = Vec::new();
        let mut a2: Vec<Turn> = Vec::new();
        let mut b: Vec<Turn> = Vec::new();
        let mut c: Vec<Turn> = Vec::new();
        let mut long_s = 0.0f64;
        for (i, clip) in clips.iter().enumerate() {
            let samples = read_wav(clip)?;
            long_s += samples.len() as f64 / SAMPLE_RATE as f64;
            a.extend(run_clip(
                &mut asr, &mut vad, seg_cfg, &cfg, &samples, 0.0, i,
            ));
            b.extend(run_clip(
                &mut asr,
                &mut vad,
                seg_cfg,
                &cfg,
                &samples,
                SLICE_AFTER_S,
                i,
            ));
            a2.extend(run_clip(
                &mut asr, &mut vad, seg_cfg, &cfg, &samples, 0.0, i,
            ));
            // 0.13.1: the row is a whole-turn re-decode, not the joined
            // slices. Built from a fresh run rather than reusing `b`, so this
            // is the same code path the daemon takes on a cold turn.
            let mut c_turns = run_clip(
                &mut asr,
                &mut vad,
                seg_cfg,
                &cfg,
                &samples,
                SLICE_AFTER_S,
                i,
            );
            let audio_ext = with_tail_silence(&samples);
            redecode_whole_turns(&mut asr, &audio_ext, &mut c_turns);
            c.extend(c_turns);
        }
        println!("audio: {:.1} min, turns: {}\n", long_s / 60.0, a.len());
        println!("## the noise floor — the same arm, run twice\n");
        wer_report(&a, &a2);
        println!("\n## slicing (joined captions, the pre-0.13.1 row) — vs whole-turn decode\n");
        wer_report(&a, &b);
        println!("\n## 0.13.1 — whole-turn redecode, vs the same whole-turn decode\n");
        wer_report(&a, &c);

        // ---- the knob ------------------------------------------------------
        //
        // If the disagreement at the shipped floor is bigger than the noise
        // floor — and it is — the useful next question is which floor would be
        // smaller, because a longer slice is a slice with more context and
        // FINDINGS §12 says context is most of the battle. This is the table
        // that decides the default: a number somebody can put in a config file
        // rather than "make it a bit longer".
        println!("\n## the knob — `[captions] slice_after_s` swept\n");
        println!(
            "| `slice_after_s` | turns sliced | slices | WER vs whole-turn | words sooner (median) |"
        );
        println!("|---:|---:|---:|---:|---:|");
        for after in [4.0f32, 6.0, 8.0, 10.0, 12.0, 15.0] {
            let mut swept: Vec<Turn> = Vec::new();
            for (i, clip) in clips.iter().enumerate() {
                let samples = read_wav(clip)?;
                swept.extend(run_clip(
                    &mut asr, &mut vad, seg_cfg, &cfg, &samples, after, i,
                ));
            }
            let (turns, cuts, wer, saved) = sweep_row(&a, &swept);
            println!(
                "| {after} | {turns} | {cuts} | {:.2}% | {saved:.2} s |",
                wer * 100.0
            );
        }
    }
    Ok(())
}

/// One row of the knob table: (turns sliced, slices, WER against arm A, median
/// seconds a word reaches the glass sooner on the turns that were sliced).
fn sweep_row(base: &[Turn], swept: &[Turn]) -> (usize, usize, f64, f64) {
    let (mut refw, mut err, mut turns, mut cuts) = (0usize, 0usize, 0usize, 0usize);
    let mut saved = Vec::new();
    for b in swept.iter().filter(|t| t.was_sliced()) {
        let Some(a) = base.iter().find(|a| a.key == b.key) else {
            continue;
        };
        let want = normalise_words(&a.raw);
        refw += want.len();
        err += edits(&normalise_words(&b.raw), &want);
        turns += 1;
        cuts += b.slices;
        let mut before: Vec<f64> = a
            .spoken_s
            .iter()
            .zip(&a.shown_s)
            .map(|(s, w)| w - s)
            .collect();
        let mut after: Vec<f64> = b
            .spoken_s
            .iter()
            .zip(&b.shown_s)
            .map(|(s, w)| w - s)
            .collect();
        if !before.is_empty() && !after.is_empty() {
            saved.push(median(&mut before) - median(&mut after));
        }
    }
    (
        turns,
        cuts,
        err as f64 / refw.max(1) as f64,
        median(&mut saved),
    )
}

// ---------------------------------------------------------------------------
// the simulation
// ---------------------------------------------------------------------------

/// One slice as it would have reached a client.
struct Slice {
    /// Audio time the daemon NOTICED the cut — the VAD cursor when
    /// `Slicer::due` fired. The decode starts here.
    detected_s: f64,
    /// Wall-clock seconds the slice's decode took.
    cost_s: f64,
    /// Audio time of the first sample in the slice, so a word's absolute spoken
    /// time is this plus its timestamp within the slice.
    start_s: f64,
    words: Vec<String>,
    /// Word start times, relative to `start_s`.
    starts: Vec<f32>,
    /// This slice's reading as one string — see `Turn::raw`.
    raw: String,
}

/// One turn the VAD produced, with everything measured about it.
struct Turn {
    /// Which clip and which turn within it. THIS is what the two arms are
    /// paired on: audio time is measured from the start of each clip, so it
    /// collides across a corpus of hundreds of them, and pairing on it silently
    /// scored every turn against a stranger.
    key: (usize, usize),
    /// Whether the remainder decode read the WHOLE turn despite the turn having
    /// been sliced — i.e. the slices were paid for and then thrown away. Always
    /// false if the feature is working; asserted rather than assumed, because
    /// the failure is invisible in the text and shows up only as CPU.
    redecoded_whole: bool,
    /// What the last decode cost. In arm A this is the whole turn; in arm B,
    /// with slices, it is only the remainder.
    final_cost_s: f64,
    /// The words on the finished row: the whole-turn decode in arm A, the
    /// slices joined in arm B.
    words: Vec<String>,
    /// The same reading as one string, before any tokenisation.
    ///
    /// WER is scored on THIS, re-split, and not on `words`. `words` comes from
    /// the decoder's own token boundaries, and a transducer emits subword
    /// pieces — so a whole-turn decode and a slice decode can hand back the
    /// same sentence cut into different pieces and score as a dozen errors
    /// without disagreeing about a single word.
    raw: String,
    /// Absolute spoken time of each word in `words`.
    spoken_s: Vec<f64>,
    /// Absolute time each word could first have been READ. For a word in a
    /// slice that is the slice's publish; for one in the remainder it is the
    /// row settling.
    shown_s: Vec<f64>,
    slices: usize,
    /// The turn's own span, in absolute audio seconds — where `write_segment`
    /// would extract `samples` from. Kept so a later pass can re-decode the
    /// WHOLE turn (0.13.1, FINDINGS §48) without re-running the VAD and the
    /// merger: the span is a fact about the turn, not about which arm read it.
    span_start_s: f64,
    span_end_s: f64,
}

impl Turn {
    fn was_sliced(&self) -> bool {
        self.slices > 0
    }
}

/// Replay one clip through the real VAD, the real merger and the real slicer.
///
/// `slice_after_s` of zero is arm A exactly: [`Slicer::due`] refuses a floor of
/// zero, so not one branch below behaves differently from 0.12.3.
fn run_clip(
    asr: &mut TimedAsr,
    vad: &mut SileroVad,
    seg_cfg: recalld::vad::SegmenterConfig,
    cfg: &Config,
    samples: &[f32],
    slice_after_s: f32,
    clip: usize,
) -> Vec<Turn> {
    let mut state = vad.new_state();
    let mut segmenter = Segmenter::new(seg_cfg);
    let mut merger = recalld::ingest::turn_merger(cfg);
    let mut slicer = Slicer::default();
    let after = (slice_after_s * SAMPLE_RATE as f32) as u64;

    let audio = with_tail_silence(samples);

    let sec = |n: u64| n as f64 / SAMPLE_RATE as f64;
    let mut out: Vec<Turn> = Vec::new();
    let mut open: Vec<Slice> = Vec::new();
    // Cuts MADE, not slices that produced words. A turn whose only slice the
    // decoder made nothing of was still sliced — its remainder is not the whole
    // turn — and counting it as unsliced is what made the first run of this
    // bench report a phantom control failure.
    let mut cuts = 0usize;
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
            // The remainder: everything of this turn that no slice has read.
            // With no slices that is the whole turn, which is arm A.
            let from = slicer.pending_start(span.start).unwrap_or(span.start);
            let lo = (from as usize).min(audio.len());
            let hi = (span.end as usize).min(audio.len());
            let (text, words) = if lo < hi {
                let t0 = Instant::now();
                let r = asr.transcribe_timed(&audio[lo..hi]);
                (r, t0.elapsed().as_secs_f64())
            } else {
                ((String::new(), Vec::new()), 0.0)
            };
            let ((text, timed), cost) = (text, words);
            let published_s = sec(cursor) + cost;

            let mut turn = Turn {
                key: (clip, out.len()),
                redecoded_whole: !open.is_empty() && from == span.start,
                final_cost_s: cost,
                words: Vec::new(),
                raw: String::new(),
                spoken_s: Vec::new(),
                shown_s: Vec::new(),
                slices: cuts,
                span_start_s: sec(span.start),
                span_end_s: sec(span.end),
            };
            // The row, in order: every slice that was published while the
            // person was still talking, then the remainder.
            for s in open.drain(..) {
                push_raw(&mut turn.raw, &s.raw);
                for (i, w) in s.words.iter().enumerate() {
                    turn.words.push(w.clone());
                    turn.spoken_s
                        .push(s.start_s + s.starts.get(i).copied().unwrap_or(0.0) as f64);
                    turn.shown_s.push(s.detected_s + s.cost_s);
                }
            }
            push_raw(&mut turn.raw, &text);
            for (i, w) in words_of(&text, &timed).iter().enumerate() {
                turn.words.push(w.clone());
                turn.spoken_s
                    .push(sec(from) + timed.get(i).map(|t| t.start_s).unwrap_or(0.0) as f64);
                turn.shown_s.push(published_s);
            }
            out.push(turn);
            slicer.reset();
            cuts = 0;
        }

        if after == 0 {
            continue;
        }
        // The daemon's own rule, verbatim, with audio time standing in for the
        // wall clock — a machine that keeps up is the machine this is written
        // for, and one that does not is the backlog rule's job.
        let Some(turn_start) = open_start(&merger, &segmenter) else {
            continue;
        };
        let Some(cut) = slicer.due(turn_start, cursor, segmenter.dip_len(), after) else {
            continue;
        };
        let lo = (cut.start as usize).min(audio.len());
        let hi = (cut.end as usize).min(audio.len());
        if lo >= hi {
            continue;
        }
        let t0 = Instant::now();
        let (text, timed) = asr.transcribe_timed(&audio[lo..hi]);
        let cost = t0.elapsed().as_secs_f64();
        // The daemon's rule exactly: the cut is marked either way so it is not
        // re-offered every frame, but a slice the decoder made nothing of does
        // not count as READ, and its audio goes back in front of the remainder.
        let read = !normalise_words(&text).is_empty();
        cuts += 1;
        slicer.mark(turn_start, cut, read);
        if !read {
            continue;
        }
        open.push(Slice {
            detected_s: sec(cursor),
            cost_s: cost,
            start_s: sec(cut.start),
            words: words_of(&text, &timed),
            starts: timed.iter().map(|w| w.start_s).collect(),
            raw: text.clone(),
        });
    }

    // Whatever is still open when the audio runs out.
    let mut tail = Vec::new();
    if let Some(span) = segmenter.flush() {
        tail.extend(merger.push(span));
    }
    tail.extend(merger.flush());
    for span in tail {
        let from = slicer.pending_start(span.start).unwrap_or(span.start);
        let lo = (from as usize).min(audio.len());
        let hi = (span.end as usize).min(audio.len());
        let t0 = Instant::now();
        let (text, timed) = if lo < hi {
            asr.transcribe_timed(&audio[lo..hi])
        } else {
            (String::new(), Vec::new())
        };
        let cost = t0.elapsed().as_secs_f64();
        let published_s = sec(cursor) + cost;
        let mut turn = Turn {
            key: (clip, out.len()),
            redecoded_whole: !open.is_empty() && from == span.start,
            final_cost_s: cost,
            words: Vec::new(),
            raw: String::new(),
            spoken_s: Vec::new(),
            shown_s: Vec::new(),
            slices: cuts,
            span_start_s: sec(span.start),
            span_end_s: sec(span.end),
        };
        for s in open.drain(..) {
            push_raw(&mut turn.raw, &s.raw);
            for (i, w) in s.words.iter().enumerate() {
                turn.words.push(w.clone());
                turn.spoken_s
                    .push(s.start_s + s.starts.get(i).copied().unwrap_or(0.0) as f64);
                turn.shown_s.push(s.detected_s + s.cost_s);
            }
        }
        push_raw(&mut turn.raw, &text);
        for (i, w) in words_of(&text, &timed).iter().enumerate() {
            turn.words.push(w.clone());
            turn.spoken_s
                .push(sec(from) + timed.get(i).map(|t| t.start_s).unwrap_or(0.0) as f64);
            turn.shown_s.push(published_s);
        }
        out.push(turn);
        slicer.reset();
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

fn words_of(text: &str, words: &[Word]) -> Vec<String> {
    if words.is_empty() {
        return normalise_words(text);
    }
    words
        .iter()
        .flat_map(|w| normalise_words(&w.text))
        .collect()
}

/// Append the tail silence every clip gets before it is fed to the VAD — the
/// only reason the daemon's own segmenter closes the span at all rather than
/// on `flush()`. One place, so arm C's re-decode indexes the exact same
/// buffer `run_clip` scored spans against.
fn with_tail_silence(samples: &[f32]) -> Vec<f32> {
    let mut audio = samples.to_vec();
    audio.extend(vec![0.0f32; (TAIL_SILENCE_S * SAMPLE_RATE as f32) as usize]);
    audio
}

/// The one new cost of 0.13.1 (FINDINGS §48): for every turn that was
/// SLICED, decode its whole span once more and let that reading replace the
/// joined slices as the row's text.
///
/// This is not a simulation of the fix, it IS the fix — the same
/// `TimedAsr::transcribe_timed` call over the same audio range
/// `write_segment` now hands `Analyzer::transcribe_slice`, on the turn's own
/// span rather than the remainder. Returns the CPU time it cost, so the
/// caller can fold it into the sliced arm's total and report the marginal
/// price rather than an estimate of it.
fn redecode_whole_turns(asr: &mut TimedAsr, audio: &[f32], turns: &mut [Turn]) -> f64 {
    let mut cost = 0.0f64;
    for t in turns.iter_mut().filter(|t| t.was_sliced()) {
        let lo = ((t.span_start_s * SAMPLE_RATE as f64) as usize).min(audio.len());
        let hi = ((t.span_end_s * SAMPLE_RATE as f64) as usize).min(audio.len());
        if lo >= hi {
            continue;
        }
        let t0 = Instant::now();
        let (text, _timed) = asr.transcribe_timed(&audio[lo..hi]);
        cost += t0.elapsed().as_secs_f64();
        // This IS the row now — not the joined slices. `words`/`spoken_s`/
        // `shown_s` are left untouched: the captions a person actually saw
        // arrived exactly when arm B says they did, and this pass changes
        // only what gets written down, not when the glass lit up.
        t.raw = text;
    }
    cost
}

// ---------------------------------------------------------------------------
// the report
// ---------------------------------------------------------------------------

/// The 0.13.1 headline: the same CPU-per-minute framing as [`report`], but for
/// the arm whose row is a whole-turn re-decode rather than joined slices. The
/// latency numbers are deliberately NOT repeated here — arm C's `words`/
/// `spoken_s`/`shown_s` are byte-for-byte arm B's, because the re-decode
/// changes what is written down, never when a caption reached the glass
/// (FINDINGS §48).
fn report_redecode(
    base: &[Turn],
    redecoded: &[Turn],
    audio_s: f64,
    cpu_base: f64,
    cpu_redecode: f64,
) {
    let n_sliced = redecoded.iter().filter(|t| t.was_sliced()).count();
    println!("\n# 0.13.1 — re-decoding the whole turn at close\n");
    println!(
        "turns: {} ({n_sliced} sliced and re-decoded whole)",
        redecoded.len()
    );
    println!("audio: {:.1} min\n", audio_s / 60.0);

    println!("## CPU\n");
    let minutes = audio_s / 60.0;
    let delta = (cpu_redecode - cpu_base) / cpu_base.max(1e-9);
    println!("| arm | process CPU s | CPU s per minute of speech |");
    println!("|---|---:|---:|");
    println!(
        "| 0.12.3 (one decode per turn) | {cpu_base:.2} | {:.2} |",
        cpu_base / minutes.max(1e-9)
    );
    println!(
        "| sliced + whole-turn redecode | {cpu_redecode:.2} | {:.2} |",
        cpu_redecode / minutes.max(1e-9)
    );
    println!(
        "\nre-decoding adds **{:+.1}%** CPU over 0.12.3\n",
        delta * 100.0
    );
    println!("## gate\n");
    let ok15 = delta <= 0.15;
    let ok10 = delta <= 0.10;
    println!(
        "- CPU <= +15%: {} ({:+.1}%)",
        if ok15 { "PASS" } else { "FAIL" },
        delta * 100.0
    );
    println!(
        "- CPU <= +10% (would need no config change to be the default everywhere): {} ({:+.1}%)",
        if ok10 {
            "yes"
        } else {
            "no, needs a higher floor"
        },
        delta * 100.0
    );

    // The WER proof: the row is now a fresh `transcribe_timed` over the
    // turn's own span, the same call the baseline arm makes over the same
    // audio — so there is no comparison for the two readings to disagree on.
    // Paired by `key`, exactly like `wer_report`.
    let mut pairs: Vec<(&Turn, &Turn)> = Vec::new();
    for r in redecoded.iter().filter(|t| t.was_sliced()) {
        if let Some(b) = base.iter().find(|b| b.key == r.key) {
            pairs.push((b, r));
        }
    }
    if !pairs.is_empty() {
        let (mut refw, mut err) = (0usize, 0usize);
        for (b, r) in &pairs {
            let (want, got) = (normalise_words(&b.raw), normalise_words(&r.raw));
            refw += want.len();
            err += edits(&got, &want);
        }
        println!(
            "\n## word disagreement, redecoded row vs whole-turn baseline\n\nturns: {}, reference words: {refw}, WER: **{:.2}%**\n",
            pairs.len(),
            100.0 * err as f64 / refw.max(1) as f64
        );
    }
}

fn report(base: &[Turn], sliced: &[Turn], audio_s: f64, cpu_base: f64, cpu_slice: f64) {
    let n_sliced = sliced.iter().filter(|t| t.was_sliced()).count();
    let cuts: usize = sliced.iter().map(|t| t.slices).sum();
    let wasted = sliced.iter().filter(|t| t.redecoded_whole).count();
    println!("turns: {} ({n_sliced} sliced, {cuts} slices)", sliced.len());
    if wasted > 0 {
        println!(
            "**{wasted} sliced turns re-decoded the WHOLE turn at the end** — the \
             slices were paid for and thrown away. This is a bug, and every CPU \
             number below is measuring it rather than the feature."
        );
    }
    println!("audio: {:.1} min\n", audio_s / 60.0);

    println!("## turn end → row published\n");
    println!("| arm | n | median | p90 | p99 |");
    println!("|---|---:|---:|---:|---:|");
    let mut a: Vec<f64> = base.iter().map(|t| t.final_cost_s).collect();
    let mut b: Vec<f64> = sliced.iter().map(|t| t.final_cost_s).collect();
    row("0.12.3", &mut a);
    row("sliced", &mut b);

    println!("\n## word spoken → word shown, over turns that were sliced\n");
    let mut base_lat = Vec::new();
    let mut slice_lat = Vec::new();
    for t in sliced.iter().filter(|t| t.was_sliced()) {
        for (spoken, shown) in t.spoken_s.iter().zip(&t.shown_s) {
            slice_lat.push(shown - spoken);
        }
    }
    // The same words in arm A, where every one of them waits for the row.
    let long: Vec<&Turn> = base
        .iter()
        .filter(|t| sliced.iter().any(|s| s.was_sliced() && s.key == t.key))
        .collect();
    for t in &long {
        for (spoken, shown) in t.spoken_s.iter().zip(&t.shown_s) {
            base_lat.push(shown - spoken);
        }
    }
    println!("| arm | n words | median | p90 | p99 |");
    println!("|---|---:|---:|---:|---:|");
    row("0.12.3", &mut base_lat);
    row("sliced", &mut slice_lat);
    if !base_lat.is_empty() && !slice_lat.is_empty() {
        let saved = median(&mut base_lat.clone()) - median(&mut slice_lat.clone());
        println!("\nmedian saved on a sliced turn: **{saved:.2} s**");
    }

    println!("\n### histogram — word spoken → word shown, sliced turns\n");
    histogram("0.12.3", &base_lat);
    histogram("sliced", &slice_lat);

    println!("\n## CPU\n");
    let minutes = audio_s / 60.0;
    let delta = (cpu_slice - cpu_base) / cpu_base.max(1e-9);
    println!("| arm | process CPU s | CPU s per minute of speech |");
    println!("|---|---:|---:|");
    println!(
        "| 0.12.3 (one decode per turn) | {cpu_base:.2} | {:.2} |",
        cpu_base / minutes.max(1e-9)
    );
    println!(
        "| sliced | {cpu_slice:.2} | {:.2} |",
        cpu_slice / minutes.max(1e-9)
    );
    println!("\nslicing adds **{:+.1}%** CPU\n", delta * 100.0);
    println!("## gate\n");
    let ok = delta <= 0.10;
    println!(
        "- CPU ≤ +10%: {} ({:+.1}%)",
        if ok { "PASS" } else { "FAIL" },
        delta * 100.0
    );
}

/// The words the two arms produced for the same turn, scored against each
/// other. Arm A is the reference: it is the reading this daemon has always
/// written down.
fn wer_report(base: &[Turn], sliced: &[Turn]) {
    let mut pairs: Vec<(&Turn, &Turn)> = Vec::new();
    for b in sliced {
        if let Some(a) = base.iter().find(|a| a.key == b.key) {
            pairs.push((a, b));
        }
    }
    let (mut ref_words, mut errors) = (0usize, 0usize);
    let (mut sl_ref, mut sl_err, mut sl_turns) = (0usize, 0usize, 0usize);
    let mut unsliced_bad = 0usize;
    let mut per_turn: Vec<(bool, f64)> = Vec::new();
    let dump: usize = std::env::var("NXR_DUMP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut shown = 0usize;
    for (a, b) in &pairs {
        // Scored on the re-normalised TEXT and not on `words`: `words` carries
        // the decoder's own token boundaries, and a transducer emits SUBWORD
        // pieces — so two readings of the same sentence, cut into different
        // pieces, would score as a dozen errors without disagreeing about a
        // single word.
        let (want, got) = (normalise_words(&a.raw), normalise_words(&b.raw));
        let d = edits(&got, &want);
        if d > 0 && shown < dump {
            shown += 1;
            println!(
                "\n--- clip {:?} slices={} edits={d}/{}\n  whole : {}\n  sliced: {}",
                b.key,
                b.slices,
                want.len(),
                a.raw.trim(),
                b.raw.trim()
            );
        }
        ref_words += want.len();
        errors += d;
        if b.was_sliced() {
            sl_ref += want.len();
            sl_err += d;
            sl_turns += 1;
        } else if d > 0 {
            unsliced_bad += 1;
        }
        // The bootstrap runs over whichever population this call is ABOUT: the
        // sliced turns when there are any, and every turn when there are none
        // (the noise-floor pass, where arm B is arm A again and nothing is
        // sliced by construction).
        if !want.is_empty() {
            per_turn.push((b.was_sliced(), d as f64 / want.len() as f64));
        }
    }
    println!("| population | turns | reference words | WER |");
    println!("|---|---:|---:|---:|");
    println!(
        "| all paired turns | {} | {ref_words} | {:.2}% |",
        pairs.len(),
        100.0 * errors as f64 / ref_words.max(1) as f64
    );
    if sl_turns > 0 {
        println!(
            "| turns that were sliced | {sl_turns} | {sl_ref} | **{:.2}%** |",
            100.0 * sl_err as f64 / sl_ref.max(1) as f64
        );
    }
    // The control. A turn nobody sliced went down a code path this feature does
    // not touch, so its words must be identical — and if they are not, the
    // bench is measuring its own harness and every number above is void.
    let unsliced = pairs.len() - sl_turns;
    if unsliced > 0 {
        println!(
            "\nof the {unsliced} turns that were NOT sliced, **{unsliced_bad} came \
             back with different words**"
        );
    }
    // A bootstrap over turns, because the useful bound is "how far could this
    // number move if the evening had gone slightly differently", and the turns
    // are the independent unit — not the words, which come in correlated runs.
    let scored: Vec<f64> = if sl_turns > 0 {
        per_turn
            .iter()
            .filter(|(s, _)| *s)
            .map(|(_, w)| *w)
            .collect()
    } else {
        per_turn.iter().map(|(_, w)| *w).collect()
    };
    let per_turn = scored;
    if per_turn.len() > 1 {
        let (lo, hi) = bootstrap(&per_turn);
        println!(
            "\nper-turn WER, 95% bootstrap interval over {} turns: \
             **{:.2}% – {:.2}%** (mean {:.2}%)",
            per_turn.len(),
            lo * 100.0,
            hi * 100.0,
            100.0 * per_turn.iter().sum::<f64>() / per_turn.len() as f64
        );
    }
}

/// Append one more reading to a turn's text, one space between.
///
/// The daemon's own `pipeline::join_slices`, in the two lines of it this bench
/// needs: an empty piece must not leave a double space in the middle of a
/// sentence, because the word splitter would not care but a human reading the
/// dump would.
fn push_raw(into: &mut String, next: &str) {
    let next = next.trim();
    if next.is_empty() {
        return;
    }
    if !into.is_empty() {
        into.push(' ');
    }
    into.push_str(next);
}

/// Percentile bootstrap of the mean, with a fixed seed so the interval is the
/// same one every rerun reports.
fn bootstrap(xs: &[f64]) -> (f64, f64) {
    let mut rng: u64 = 0x5EED_5D11_7A5C_0DE1;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let mut means: Vec<f64> = (0..2000)
        .map(|_| {
            let s: f64 = (0..xs.len())
                .map(|_| xs[(next() % xs.len() as u64) as usize])
                .sum();
            s / xs.len() as f64
        })
        .collect();
    means.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    (percentile(&mut means, 0.025), percentile(&mut means, 0.975))
}

fn histogram(name: &str, v: &[f64]) {
    if v.is_empty() {
        println!("{name}: (nothing)");
        return;
    }
    const EDGES: [f64; 8] = [1.0, 2.0, 3.0, 5.0, 8.0, 12.0, 20.0, f64::INFINITY];
    let mut counts = [0usize; 8];
    for x in v {
        let i = EDGES.iter().position(|e| x < e).unwrap_or(EDGES.len() - 1);
        counts[i] += 1;
    }
    println!("{name} (n = {}):", v.len());
    let mut lo = 0.0;
    for (i, e) in EDGES.iter().enumerate() {
        let pct = 100.0 * counts[i] as f64 / v.len() as f64;
        let bar = "#".repeat((pct / 2.0).round() as usize);
        let label = if e.is_finite() {
            format!("{lo:>5.0}–{e:<4.0}")
        } else {
            format!("{lo:>5.0}+     ")
        };
        println!("  {label} s | {:>5} {:>5.1}% {bar}", counts[i], pct);
        lo = *e;
    }
}

fn row(name: &str, v: &mut [f64]) {
    if v.is_empty() {
        println!("| {name} | 0 | — | — | — |");
        return;
    }
    println!(
        "| {name} | {} | {:.2} s | {:.2} s | {:.2} s |",
        v.len(),
        median(v),
        percentile(v, 0.90),
        percentile(v, 0.99)
    );
}

fn median(v: &mut [f64]) -> f64 {
    percentile(v, 0.5)
}

fn percentile(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    v[((v.len() - 1) as f64 * p).round() as usize]
}

/// Levenshtein distance in words — substitutions, insertions and deletions,
/// which is exactly the numerator of WER.
fn edits(got: &[String], want: &[String]) -> usize {
    let (n, m) = (got.len(), want.len());
    if m == 0 {
        return n;
    }
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
    prev[m]
}

// ---------------------------------------------------------------------------
// odds and ends
// ---------------------------------------------------------------------------

fn collect(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    walk(root, &mut out)?;
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
/// worker threads: what this daemon takes off the machine, not what one thread
/// spent.
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
