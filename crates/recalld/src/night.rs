//! The night shift (0.9.0): a third reading of the day's shaky rows.
//!
//! §11 of `spike/FINDINGS.md` parked the ensemble, and it was right to. Three
//! models disagree with each other on about half of real lobby sentences, and
//! nothing in that table said which of them to believe — an ensemble without a
//! judge is a coin toss with a bigger electricity bill. What §12 then found is
//! the one place where the coin is not fair: on rows the cross-check already
//! calls **shaky**, whisper-large-v3 disagrees with the live text 76% of the
//! time against 27% on solid rows. That is not a claim that large-v3 is right.
//! It is a claim that the shaky flag has found the rows where a third opinion
//! is worth asking for, which is a much smaller and much more defensible claim.
//!
//! So this worker is deliberately narrow:
//!
//! * **Only shaky rows.** A third reading of a row two decoders agree on buys
//!   nothing (§12: they disagree 27% of the time there, and the live text is
//!   usually the better of the two) and costs a GPU.
//! * **Only overnight, and only on an idle GPU.** The card belongs to whatever
//!   is drawing frames. `[night].window` is the clock and
//!   `[night].gpu_busy_max_pct` is the check before *every* batch, so a person
//!   who sits down at 04:00 gets the GPU back within one batch.
//! * **Only by a vote.** Whatever the rule, large-v3's words never replace
//!   anything on their own authority — see [`judge_vote`].
//!
//! ## The hallucination, which is the reason for every guard here
//!
//! §12 measured whisper-large-v3 answering German lobby audio in Arabic,
//! Finnish and Swedish. That is the failure mode this feature has to survive:
//! on exactly the rows the live decoder failed, the "better" model frequently
//! fails too, and it fails *fluently* — confident, well-formed text in the
//! wrong language. Replacing a bad German transcript with a good Arabic one is
//! not an improvement, it is a more convincing lie. Hence the language guard in
//! [`judge_vote`] — and hence the fact that it is **two** tests rather than
//! one. `lang::classify` answers "German or English?" and settles on German the
//! moment it sees an umlaut, which is right for the question it was built for
//! and useless against a third language: the measured Swedish hallucination
//! "Tack för att ni tittade" has an ö in it and classifies as perfectly good
//! German. So a replacement must also carry at least one German function word
//! ([`crate::lang::stopword_votes`]), which the Swedish, Finnish and Arabic
//! strings do not.
//!
//! ## The batch, and why there is one
//!
//! Loading large-v3 costs seconds; decoding a two-second clip costs a fraction
//! of one. A night shift that spawned one process per row would spend its whole
//! budget on model loads, and the measurement says so plainly (FINDINGS §13, a
//! 7900 XTX through the Vulkan build): 30 lobby-sized clips in one invocation
//! run at RTF 0.154, 167 of them at 0.143, and twenty long utterances — the
//! same model load spread over four times the audio — at **0.045**. The load is
//! the cost. On four CPU cores the same weights run at 16.9 (§11), which is why
//! this was parked as a CPU feature and is shippable as a GPU one. So `[night].batch_rows` clips are concatenated with
//! `[night].gap_s` of silence between them, decoded in one invocation with
//! timestamps, and split apart again by offset ([`pack`] / [`split_by_offsets`]).
//! The gap is one second: long enough that the decoder does not read two turns
//! as one sentence, short enough not to invite a caption hallucination about
//! the quiet.
//!
//! A batch carries **one language**, because the queue is grouped by the rows'
//! own `lang` and the language is passed to the decoder rather than detected.
//! A forced language is a guard in itself, and for the rows whose language
//! nobody knows the decoder is asked to detect one and the answer is checked.
//!
//! ## The jail, and the lock discipline
//!
//! Both are inherited, not invented. The decoder is a child process pinned and
//! niced between fork and exec exactly as [`crate::llm`] jails llama.cpp — same
//! posture, different binary. And the rule from [`crate::quality`] holds
//! without exception: **gather under the store lock, decode without it, commit
//! under it again.** Model time and store-lock time never overlap.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tracing::{debug, info, warn};

use crate::bus::Bus;
use crate::canary::{Canary, agreement};
use crate::clock::utc_now_ns;
use crate::config::{NightConfig, SAMPLE_RATE};
use crate::control::Control;
use crate::lang::{self, Lang};
use crate::models::{ConfidenceModel, NightModels};
use crate::store::{RedecodeCandidate, Store, text_via};
use crate::text_truth::{Cell, Rules, VoteRule};

// ---------------------------------------------------------------------------
// the clock
// ---------------------------------------------------------------------------

/// `HH:MM-HH:MM` in local time, wrapping over midnight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hours {
    pub from_min: u32,
    pub to_min: u32,
}

impl Hours {
    /// Parse the config string, or `None` if it is not two `HH:MM` separated by
    /// a dash. A malformed window is not silently widened to the whole day: the
    /// caller treats `None` as "the clock gate can never open", which fails
    /// closed on a typo in the one setting that keeps this feature off people's
    /// machines while they are using them.
    pub fn parse(s: &str) -> Option<Self> {
        let (a, b) = s.split_once('-')?;
        Some(Self {
            from_min: minute_of_day(a.trim())?,
            to_min: minute_of_day(b.trim())?,
        })
    }

    /// Is `minute` inside the window? A window whose end is not after its start
    /// wraps midnight, which is the normal case for these hours.
    pub fn contains(&self, minute: u32) -> bool {
        if self.from_min <= self.to_min {
            (self.from_min..self.to_min).contains(&minute)
        } else {
            minute >= self.from_min || minute < self.to_min
        }
    }
}

fn minute_of_day(hhmm: &str) -> Option<u32> {
    let (h, m) = hhmm.split_once(':')?;
    let (h, m) = (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

/// Local minute of day, from the system clock.
fn local_minute_now() -> u32 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let local = now + crate::clock::local_offset_s(now * 1_000_000_000);
    let day = local.rem_euclid(86_400);
    (day / 60) as u32
}

// ---------------------------------------------------------------------------
// the GPU
// ---------------------------------------------------------------------------

/// How busy the discrete GPU is, in percent, or `None` when nothing here can
/// say.
///
/// Read from `amdgpu`'s own counter in sysfs rather than by shelling out to
/// `rocm-smi`: the tool reads this same file, it is not installed on every
/// machine with a working ROCm/Vulkan runtime (this one has the compiler stack
/// and no `rocm-smi` at all), and a gate that depends on an optional
/// diagnostics package is a gate that quietly stops gating.
///
/// **`None` closes the gate.** A night shift that cannot tell whether the card
/// is busy does not get to assume it is free.
///
/// One number to keep in mind when reading `[night].gpu_busy_max_pct`: this
/// card reports **around 30% with nothing but a desktop on it**, well above the
/// 20% default. That is deliberate and it is not a mis-set ceiling — on a
/// machine like this the clock is what lets the night shift start and this
/// check is what stops it again the moment somebody sits down.
pub fn gpu_busy_pct() -> Option<u32> {
    gpu_busy_pct_in(Path::new("/sys/class/drm"))
}

/// The same, rooted anywhere, so the test suite can build a fake sysfs.
pub fn gpu_busy_pct_in(root: &Path) -> Option<u32> {
    let mut best: Option<u32> = None;
    let entries = std::fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let device = entry.path().join("device");
        let busy = device.join("gpu_busy_percent");
        if !busy.is_file() {
            continue;
        }
        // Integrated graphics share the counter's name and none of its
        // meaning here: a 7900 XTX has 24 GB and the CPU's display adapter has
        // a couple. The one with real VRAM is the one the decoder will run on.
        let vram = device.join("mem_info_vram_total");
        let big = std::fs::read_to_string(&vram)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .is_some_and(|b| b >= 8 << 30);
        if !big {
            continue;
        }
        if let Some(pct) = std::fs::read_to_string(&busy)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
        {
            best = Some(best.map_or(pct, |b: u32| b.max(pct)));
        }
    }
    best
}

// ---------------------------------------------------------------------------
// the gates
// ---------------------------------------------------------------------------

/// Why the night shift may not run right now, or `None` for "go ahead".
///
/// Every one of these is re-checked between batches, not once at the top of the
/// night. `idle_min` is minutes since the last capture activity, and `busy` is
/// [`gpu_busy_pct`]'s answer, both passed in so the whole gate is a pure
/// function of four numbers and can be tested without a clock or a GPU.
pub fn gate(
    control: &Control,
    cfg: &NightConfig,
    minute: u32,
    idle_min: i64,
    busy: Option<u32>,
) -> Option<String> {
    if !cfg.enabled {
        return Some("the night shift is off".to_string());
    }
    if control.is_paused() {
        return Some("capture is paused — nothing is written down, including this".to_string());
    }
    let in_window = Hours::parse(&cfg.window).is_some_and(|h| h.contains(minute));
    let idle_enough = cfg.also_when_idle_min > 0 && idle_min >= cfg.also_when_idle_min;
    if !in_window && !idle_enough {
        return Some(format!(
            "outside {} and the machine has been busy within the last {} minutes",
            cfg.window, cfg.also_when_idle_min
        ));
    }
    match busy {
        None => Some("no GPU utilisation counter to read — refusing to assume it is free".into()),
        Some(pct) if pct > cfg.gpu_busy_max_pct => Some(format!(
            "the GPU is {pct}% busy (ceiling {}%) — it belongs to whatever is drawing frames",
            cfg.gpu_busy_max_pct
        )),
        Some(_) => None,
    }
}

// ---------------------------------------------------------------------------
// the batch
// ---------------------------------------------------------------------------

/// Where one clip sits inside a packed batch, in seconds from its start.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Slot {
    pub from_s: f32,
    pub to_s: f32,
}

/// Many clips as one decodable waveform, plus where each of them landed.
#[derive(Debug, Clone, PartialEq)]
pub struct Packed {
    pub samples: Vec<f32>,
    pub slots: Vec<Slot>,
}

/// Butt clips together with `gap_s` of silence between them.
///
/// Pure, and the arithmetic is the part worth a test: an off-by-one gap here
/// puts every word in the batch into the wrong row, which is a data-corruption
/// bug wearing the costume of a transcription bug.
pub fn pack(clips: &[Vec<f32>], gap_s: f32) -> Packed {
    let gap = (gap_s.max(0.0) * SAMPLE_RATE as f32) as usize;
    let mut samples: Vec<f32> = Vec::new();
    let mut slots = Vec::with_capacity(clips.len());
    for (i, clip) in clips.iter().enumerate() {
        if i > 0 {
            samples.extend(std::iter::repeat_n(0.0, gap));
        }
        let from = samples.len() as f32 / SAMPLE_RATE as f32;
        samples.extend_from_slice(clip);
        let to = samples.len() as f32 / SAMPLE_RATE as f32;
        slots.push(Slot {
            from_s: from,
            to_s: to,
        });
    }
    Packed { samples, slots }
}

/// One line of the decoder's output: text and where it sits in the batch.
#[derive(Debug, Clone, PartialEq)]
pub struct Utterance {
    pub from_s: f32,
    pub to_s: f32,
    pub text: String,
}

/// Hand each decoded utterance back to the clip it came from.
///
/// A line belongs to the slot its **midpoint** falls in. Not its start: the
/// decoder rounds an utterance's edges outwards to the nearest half second and
/// a line that begins a hair before its clip would otherwise be given to the
/// silence in front of it (or to the previous row, which is worse). Lines whose
/// midpoint lands in a gap are dropped — that is the silence, and nobody said
/// anything in it.
pub fn split_by_offsets(lines: &[Utterance], slots: &[Slot]) -> Vec<String> {
    let mut out = vec![String::new(); slots.len()];
    for line in lines {
        let mid = (line.from_s + line.to_s) / 2.0;
        let Some(i) = slots
            .iter()
            .position(|s| mid >= s.from_s && mid < s.to_s)
            .or_else(|| {
                // A line that spans a whole clip and overshoots it on both
                // sides has its midpoint in the gap; give it to the slot it
                // overlaps most, if it overlaps one at all.
                slots
                    .iter()
                    .enumerate()
                    .map(|(i, s)| {
                        let lo = line.from_s.max(s.from_s);
                        let hi = line.to_s.min(s.to_s);
                        (i, (hi - lo).max(0.0))
                    })
                    .filter(|(_, overlap)| *overlap > 0.0)
                    .max_by(|a, b| a.1.total_cmp(&b.1))
                    .map(|(i, _)| i)
            })
        else {
            continue;
        };
        if !out[i].is_empty() {
            out[i].push(' ');
        }
        out[i].push_str(line.text.trim());
    }
    out.into_iter()
        .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect()
}

// ---------------------------------------------------------------------------
// the vote
// ---------------------------------------------------------------------------

/// What the vote decided about one row.
#[derive(Debug, Clone, PartialEq)]
pub enum Vote {
    /// Two of the three readings agreed against the stored words, and the
    /// winner cleared every guard. These words replace the row's.
    Replace { text: String },
    /// The night reading is kept beside the row rather than over it.
    Annotate { text: String },
    /// The decoder produced nothing usable. The row is stamped and left alone;
    /// an empty third reading is the decoder's failure, not a discovery of
    /// silence, exactly as an empty cross-check is (§11: 11% of real turns).
    Nothing,
}

/// The three readings of one row, as the vote sees them.
#[derive(Debug, Clone)]
pub struct Readings<'a> {
    /// What the row says now.
    pub live: &'a str,
    /// The cross-check decoder's reading of the same clip, if it produced one.
    pub canary: Option<&'a str>,
    /// The night decoder's reading.
    pub night: &'a str,
    /// The language the night decoder was forced to, or the one it detected.
    pub night_lang: Option<&'a str>,
    /// The row's own stored language, if it has one.
    pub row_lang: Option<&'a str>,
}

/// The whole decision, as a pure function.
///
/// **Two of three, and the third is never allowed to win alone.** The replacing
/// branch fires only when the night reading and the cross-check reading agree
/// with *each other* at `tau` and both disagree with the stored words: two
/// independently-built decoders converging on the same sentence is the closest
/// thing this system has to a judge, and it is the only evidence that clears
/// the bar §11 set when it refused to ship an ensemble without one.
///
/// Every replacement then passes the arbiter's guards, which exist because of a
/// measured failure and not because of a worry:
///
/// 1. **Captions stripped** — Whisper invents `(soft music)` on non-speech
///    (FINDINGS §9), and `(Musik)` classifies as perfectly good German.
/// 2. **At least two words** — the same floor the cross-check and the mint bar
///    use, for the same reason.
/// 3. **It reads as the row's language** — the guard against §12's Arabic,
///    Finnish and Swedish hallucinations on German lobby audio. A third
///    language classifies as `Unclear`, which is not a match, so it cannot
///    replace anything. A row with no language of its own can never satisfy
///    this, and is therefore annotated rather than replaced.
///
/// `replace_allowed` is `[night].replace`, which ships **true** because this
/// exact rule — the majority *and* the guards — was measured against human
/// references (`spike/night_vote_bench.py`, FINDINGS §13): on 35 shaky lab
/// spans decoded by the build this daemon ships, it cuts word error from 91.1%
/// to 44.3%, 51.4% relative, and makes none of the 20 rows it touches worse.
///
/// Two looser rules scored better on that set and neither is shipped. Skipping
/// the guards is worth five points and removes the only thing standing between
/// this feature and §12's hallucinations. Replacing whenever a row is shaky,
/// with no second voter, is worth fourteen and is the "no judge" ensemble §11
/// refused on principle — on the user's own audio it is the rule that rewrote
/// two German rows in Swedish.
///
/// With `replace_allowed` false the function still runs every guard and still
/// returns the text, as an annotation. That is a supported choice rather than a
/// disabled feature: the reader is shown the second opinion and decides.
pub fn judge_vote(r: &Readings<'_>, tau: f32, replace_allowed: bool) -> Vote {
    judge_vote_ruled(r, tau, replace_allowed, VoteRule::TwoOfThree)
}

/// The same decision, under whichever rule this row's cell has learned
/// (0.12.4, `crate::text_truth`).
///
/// [`VoteRule::TwoOfThree`] is [`judge_vote`] unchanged and is what every row
/// gets until a measurement says otherwise. The other two rules exist because
/// the shipped one is a *prior*, not a measurement of this install: it was
/// fitted on 35 lab spans (FINDINGS §13), and a person who has corrected thirty
/// turns of one voice on one kind of source has better evidence than that about
/// those turns.
///
/// * [`VoteRule::NightWins`] drops the requirement for a second voter — and
///   nothing else. Every guard still runs, because the guards are about
///   §12's hallucinations and no amount of held-out WER makes a Swedish
///   sentence an acceptable replacement for a German one.
/// * [`VoteRule::KeepLive`] refuses the replacement outright; the reading is
///   still stored beside the row, because it is still a second opinion the
///   reader may want.
pub fn judge_vote_ruled(r: &Readings<'_>, tau: f32, replace_allowed: bool, rule: VoteRule) -> Vote {
    let night = crate::arbiter::strip_captions(r.night);
    if crate::asr::normalise_words(&night).is_empty() {
        return Vote::Nothing;
    }
    if !replace_allowed || rule == VoteRule::KeepLive {
        return Vote::Annotate { text: night };
    }
    if rule != VoteRule::NightWins {
        let Some(canary) = r
            .canary
            .filter(|c| !crate::asr::normalise_words(c).is_empty())
        else {
            // No cross-check reading: there is no second voter, so there is no
            // majority, so there is nothing to replace anything with.
            return Vote::Annotate { text: night };
        };
        let two_agree = agreement(&night, canary) >= tau
            && agreement(&night, r.live) < tau
            && agreement(canary, r.live) < tau;
        if !two_agree {
            return Vote::Annotate { text: night };
        }
    } else if agreement(&night, r.live) >= tau {
        // Even where the night decoder has earned the row, a reading that
        // agrees with the words already there is not a replacement.
        return Vote::Annotate { text: night };
    }
    if !guards_pass(&night, r.row_lang, r.night_lang) {
        return Vote::Annotate { text: night };
    }
    Vote::Replace { text: night }
}

/// The arbiter's guards, applied to already caption-stripped text.
fn guards_pass(night: &str, row_lang: Option<&str>, night_lang: Option<&str>) -> bool {
    if crate::asr::normalise_words(night).len() < 2 {
        return false;
    }
    // A row with no language cannot pass a language check, and a night reading
    // in a language nobody asked for is exactly the hallucination case.
    let Some(row_lang) = row_lang else {
        return false;
    };
    if night_lang.is_some_and(|l| l != row_lang) {
        return false;
    }
    if match lang::classify(night) {
        Lang::De => row_lang != "de",
        Lang::En => row_lang != "en",
        Lang::Unclear | Lang::Empty => true,
    } {
        return false;
    }
    // …and the classifier's verdict is not enough on its own here.
    //
    // `lang::classify` answers "German or English?" and settles on German the
    // moment it sees an umlaut. That is right for the question it was built
    // for and wrong as a filter against a *third* language: "Tack för att ni
    // tittade" is Swedish, has an ö, and classifies as perfectly good German —
    // and it is a real string whisper-large-v3 produced on this user's German
    // audio (FINDINGS §12). So a replacement must carry actual evidence of the
    // row's language: at least one of its stopwords, and not fewer than the
    // other language's. A German sentence with no German function word in it
    // is refused, which loses a few honest replacements and is the safe
    // direction — the row keeps words somebody said, and the night reading is
    // stored beside it either way.
    let (de, en) = lang::stopword_votes(night);
    let (mine, theirs) = if row_lang == "de" { (de, en) } else { (en, de) };
    mine >= 1 && mine >= theirs
}

// ---------------------------------------------------------------------------
// the decoder, as a child process
// ---------------------------------------------------------------------------

/// `whisper-cli` from the built runtime, jailed exactly as [`crate::llm`] jails
/// `llama-cli`: pinned to the inference cores and dropped to nice 19 between
/// fork and exec, so every thread it starts inherits both. The GPU work it does
/// is not covered by either, which is what `[night].gpu_busy_max_pct` and the
/// clock are for.
pub struct Whisper {
    cli: PathBuf,
    model: PathBuf,
    lib_dir: PathBuf,
    nice: i32,
    cpus: Vec<usize>,
    timeout: Duration,
}

impl Whisper {
    pub fn new(
        models: &NightModels,
        cfg: &NightConfig,
        runtime: &crate::config::RuntimeConfig,
    ) -> Self {
        Self {
            cli: models.cli.clone(),
            model: models.model.clone(),
            lib_dir: models.lib_dir.clone(),
            nice: runtime.inference_nice,
            cpus: runtime.inference_cpus.clone(),
            timeout: Duration::from_secs(cfg.timeout_s.max(1)),
        }
    }

    /// Decode one packed batch. `lang` is the language to force, or `None` to
    /// let the decoder detect one. Returns the lines and the language the
    /// decoder reports having used.
    pub fn decode(&self, wav: &Path, lang: Option<&str>) -> Result<(Vec<Utterance>, String)> {
        let json_path = PathBuf::from(format!("{}.json", wav.display()));
        let _ = std::fs::remove_file(&json_path);
        let mut cmd = Command::new(&self.cli);
        cmd.arg("-m")
            .arg(&self.model)
            .args(["-l", lang.unwrap_or("auto")])
            .arg("-oj")
            .arg("-np")
            .arg(wav)
            .env("LD_LIBRARY_PATH", &self.lib_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let nice = self.nice;
        let cpus = self.cpus.clone();
        // SAFETY: between fork and exec in a single-threaded child; two plain
        // syscalls, no allocation, no locks — the same hook and the same bar as
        // `crate::llm`. A failure of either is ignored on purpose: running at
        // the wrong priority beats not running.
        unsafe {
            cmd.pre_exec(move || {
                libc::setpriority(libc::PRIO_PROCESS, 0, nice);
                if !cpus.is_empty() {
                    let mut set: libc::cpu_set_t = std::mem::zeroed();
                    libc::CPU_ZERO(&mut set);
                    for &c in &cpus {
                        if c < libc::CPU_SETSIZE as usize {
                            libc::CPU_SET(c, &mut set);
                        }
                    }
                    libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &set);
                }
                Ok(())
            });
        }

        let started = Instant::now();
        let mut child = cmd
            .spawn()
            .with_context(|| format!("running {}", self.cli.display()))?;
        let stdout = drain(child.stdout.take());
        let stderr = drain(child.stderr.take());
        let status = match wait_with_timeout(&mut child, self.timeout)? {
            Some(status) => status,
            None => {
                // A wedged child is holding a GPU, which is worse than holding
                // a core. Kill it, reap it, lose the batch.
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout.join();
                let _ = stderr.join();
                bail!(
                    "the night decoder did not answer within {}s and was killed",
                    self.timeout.as_secs()
                );
            }
        };
        let _ = stdout.join();
        let err = stderr.join().unwrap_or_default();
        if !status.success() {
            bail!(
                "{} exited {}: {}",
                self.cli.display(),
                status.code().unwrap_or(-1),
                err.lines().rev().take(3).collect::<Vec<_>>().join(" / ")
            );
        }
        let text = std::fs::read_to_string(&json_path)
            .with_context(|| format!("reading {}", json_path.display()))?;
        let _ = std::fs::remove_file(&json_path);
        debug!(ms = started.elapsed().as_millis() as u64, "night decode");
        Ok(parse_whisper_json(&text))
    }
}

/// Pull the lines and the language out of `whisper-cli -oj`'s file.
///
/// Its own shape, kept in one place: `transcription[].offsets.{from,to}` are
/// **milliseconds** and `result.language` is what the decoder used. Anything
/// missing yields an empty batch rather than an error — a decoder that wrote a
/// file we cannot read has failed, and losing one batch is the correct cost.
pub fn parse_whisper_json(text: &str) -> (Vec<Utterance>, String) {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return (Vec::new(), String::new());
    };
    let language = value
        .get("result")
        .and_then(|r| r.get("language"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let lines = value
        .get("transcription")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let offsets = item.get("offsets")?;
                    let from = offsets.get("from")?.as_f64()? as f32 / 1000.0;
                    let to = offsets.get("to")?.as_f64()? as f32 / 1000.0;
                    let text = item.get("text")?.as_str()?.trim().to_string();
                    (!text.is_empty()).then_some(Utterance {
                        from_s: from,
                        to_s: to,
                        text,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    (lines, language)
}

fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_string(&mut s);
        }
        s
    })
}

fn wait_with_timeout(
    child: &mut Child,
    timeout: Duration,
) -> Result<Option<std::process::ExitStatus>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

// ---------------------------------------------------------------------------
// the worker
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Phase {
    /// `[night].enabled` is false. The shipped state.
    #[default]
    Off,
    /// On, but the model or the built runtime is missing.
    Unavailable,
    /// On and installed, but a gate is closed — the clock, the pause, or the
    /// GPU.
    Blocked,
    /// On, installed, nothing in the way, and no shaky rows left.
    Idle,
    Running,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Off => "off",
            Phase::Unavailable => "unavailable",
            Phase::Blocked => "blocked",
            Phase::Idle => "idle",
            Phase::Running => "running",
        }
    }
}

#[derive(Default)]
pub struct NightStop(AtomicBool);

impl NightStop {
    pub fn stop(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn stopped(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// The counters `status.asr.night` carries.
#[derive(Debug, Default)]
pub struct NightStats {
    pub replaced: AtomicU64,
    pub annotated: AtomicU64,
    pub skipped_busy: AtomicU64,
    pub last_run_ms: AtomicU64,
    phase: AtomicU64,
}

impl NightStats {
    pub fn set_phase(&self, phase: Phase) {
        self.phase.store(phase as u64, Ordering::Relaxed);
    }

    pub fn phase(&self) -> Phase {
        match self.phase.load(Ordering::Relaxed) {
            1 => Phase::Unavailable,
            2 => Phase::Blocked,
            3 => Phase::Idle,
            4 => Phase::Running,
            _ => Phase::Off,
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "phase": self.phase().as_str(),
            "replaced": self.replaced.load(Ordering::Relaxed),
            "annotated": self.annotated.load(Ordering::Relaxed),
            "skipped_busy": self.skipped_busy.load(Ordering::Relaxed),
            "last_run_ms": self.last_run_ms.load(Ordering::Relaxed),
        })
    }
}

/// One batch: gather, decode, commit. Public so a test can run exactly one pass
/// against a real binary instead of starting the thread and sleeping — the same
/// reason [`crate::quality::redecode_batch`] is public.
///
/// Returns how many rows were stamped, so the caller can stop when the queue
/// dries up and when the night's budget runs out.
#[allow(clippy::too_many_arguments)]
pub fn night_batch(
    store: &Arc<std::sync::Mutex<Store>>,
    bus: &Bus,
    whisper: &Whisper,
    canaries: &mut [Canary],
    cfg: &NightConfig,
    data_dir: &Path,
    scratch: &Path,
    model_id: &str,
    stats: &NightStats,
) -> Result<usize> {
    // ---- gather (lock held, no model) ----
    //
    // The learned rules are read here with everything else, and once per batch
    // rather than once per row: `recalld accuracy learn --apply` is a deliberate
    // act at a keyboard, and a rule that lands mid-batch can wait for the next
    // one.
    let (candidates, langs, rules): (Vec<RedecodeCandidate>, Vec<Option<String>>, Rules) = {
        let guard = store.lock().unwrap_or_else(|p| p.into_inner());
        let rows = guard.segments_for_night(cfg.max_rows_per_night)?;
        let mut langs = Vec::with_capacity(rows.len());
        for row in &rows {
            langs.push(guard.segment_lang_hint(row.id)?.0);
        }
        (rows, langs, Rules::load(&guard))
    };
    if candidates.is_empty() {
        return Ok(0);
    }
    // One language per batch: the decoder is told which one rather than left to
    // guess, and a forced language is the cheapest guard there is.
    let want = langs.first().cloned().flatten();
    let mut rows: Vec<(RedecodeCandidate, Option<String>)> = candidates
        .into_iter()
        .zip(langs)
        .filter(|(_, lang)| *lang == want)
        .collect();
    rows.truncate(cfg.batch_rows.max(1));

    // ---- decode (no lock) ----
    let mut clips = Vec::with_capacity(rows.len());
    let mut kept: Vec<(RedecodeCandidate, Option<String>)> = Vec::with_capacity(rows.len());
    for (row, lang) in rows {
        match crate::ingest::read_wav(&data_dir.join(&row.audio_path)) {
            Ok(samples) if !samples.is_empty() => {
                clips.push(samples);
                kept.push((row, lang));
            }
            // Retention has taken the audio, or it never landed. Stamp the row
            // so the queue moves on; there is nothing to re-read.
            _ => {
                let at = utc_now_ns();
                let guard = store.lock().unwrap_or_else(|p| p.into_inner());
                guard.set_segment_night(row.id, None, at)?;
            }
        }
    }
    if kept.is_empty() {
        return Ok(0);
    }
    let packed = pack(&clips, cfg.gap_s);
    let wav = scratch.join(format!("night-{}.wav", std::process::id()));
    crate::pipeline::write_wav(&wav, &packed.samples)?;
    let started = Instant::now();
    let decoded = whisper.decode(&wav, want.as_deref());
    let _ = std::fs::remove_file(&wav);
    let (lines, used_lang) = decoded?;
    let texts = split_by_offsets(&lines, &packed.slots);
    // The cross-check reads the same clips again, because the daemon stores its
    // verdict and not its words: a two-of-three vote needs the second voter's
    // actual sentence, and re-decoding 180m over eight short clips is cents on
    // the dollar of what the batch already spent.
    let canary_texts: Vec<Option<String>> = clips
        .iter()
        .zip(&kept)
        .map(|(samples, (_, lang))| {
            let want = lang.as_deref().or(Some(used_lang.as_str()));
            canaries
                .iter_mut()
                .find(|c| Some(c.lang()) == want)
                .map(|c| c.transcribe(samples))
        })
        .collect();
    stats
        .last_run_ms
        .store(started.elapsed().as_millis() as u64, Ordering::Relaxed);

    // ---- commit (lock held, no model) ----
    let at = utc_now_ns();
    let mut stamped = 0usize;
    for (i, (row, lang)) in kept.iter().enumerate() {
        let night = texts.get(i).map(String::as_str).unwrap_or("");
        // Which rule this row falls under (0.12.4). A row whose facets cannot
        // be read — purged between the gather and the commit — gets the
        // shipped rule, which is the conservative one.
        let rule = {
            let guard = store.lock().unwrap_or_else(|p| p.into_inner());
            guard
                .segment_facets(row.id)?
                .map(|f| rules.vote_for(&Cell::of(&f)))
                .unwrap_or(VoteRule::TwoOfThree)
        };
        let vote = judge_vote_ruled(
            &Readings {
                live: row.text.as_deref().unwrap_or(""),
                canary: canary_texts.get(i).and_then(Option::as_deref),
                night,
                night_lang: (!used_lang.is_empty()).then_some(used_lang.as_str()),
                row_lang: lang.as_deref(),
            },
            crate::config::AsrConfig::default().confidence_tau,
            cfg.replace,
            rule,
        );
        let changed = {
            let guard = store.lock().unwrap_or_else(|p| p.into_inner());
            match &vote {
                Vote::Replace { text } => {
                    guard.set_segment_night(row.id, Some(text), at)?;
                    guard.set_segment_text_via(row.id, text, model_id, text_via::NIGHT, at)?;
                    stats.replaced.fetch_add(1, Ordering::Relaxed);
                    true
                }
                Vote::Annotate { text } => {
                    guard.set_segment_night(row.id, Some(text), at)?;
                    stats.annotated.fetch_add(1, Ordering::Relaxed);
                    true
                }
                Vote::Nothing => {
                    guard.set_segment_night(row.id, None, at)?;
                    false
                }
            }
        };
        stamped += 1;
        if changed {
            let guard = store.lock().unwrap_or_else(|p| p.into_inner());
            crate::pipeline::publish_segment(bus, &guard, row.id);
        }
    }
    Ok(stamped)
}

/// The background thread. Started whether or not the feature is on, like the
/// quality worker's: the switch is live and something has to be watching it.
#[allow(clippy::too_many_arguments)]
pub fn run(
    store: Arc<std::sync::Mutex<Store>>,
    control: Arc<Control>,
    bus: Arc<Bus>,
    models_root: Option<PathBuf>,
    data_dir: PathBuf,
    runtime: crate::config::RuntimeConfig,
    stats: Arc<NightStats>,
    stop: Arc<NightStop>,
) {
    crate::pipeline::background_current_thread(runtime.inference_nice, &runtime.inference_cpus);
    let scratch = data_dir.join("tmp");
    let _ = std::fs::create_dir_all(&scratch);
    let mut said_unavailable = false;
    let mut said_blocked = String::new();

    loop {
        if stop.stopped() {
            debug!("the night shift stopped");
            return;
        }
        let cfg = control.night();
        let done = one_pass(
            &store,
            &control,
            &bus,
            &cfg,
            &models_root,
            &data_dir,
            &scratch,
            &runtime,
            &stats,
            &stop,
            &mut said_unavailable,
            &mut said_blocked,
        );
        match done {
            Ok(_) => {}
            Err(e) => warn!("a night batch failed: {e:#}"),
        }
        // Nothing here is urgent. A minute between looks is invisible to a
        // sleeping machine and cheap on a busy one.
        let step = Duration::from_millis(250);
        let mut slept = Duration::ZERO;
        while slept < Duration::from_secs(60) {
            if stop.stopped() {
                break;
            }
            std::thread::sleep(step);
            slept += step;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn one_pass(
    store: &Arc<std::sync::Mutex<Store>>,
    control: &Arc<Control>,
    bus: &Bus,
    cfg: &NightConfig,
    models_root: &Option<PathBuf>,
    data_dir: &Path,
    scratch: &Path,
    runtime: &crate::config::RuntimeConfig,
    stats: &NightStats,
    stop: &NightStop,
    said_unavailable: &mut bool,
    said_blocked: &mut String,
) -> Result<()> {
    if !cfg.enabled {
        stats.set_phase(Phase::Off);
        return Ok(());
    }
    let Some(root) = models_root.clone() else {
        stats.set_phase(Phase::Unavailable);
        return Ok(());
    };
    let models = NightModels::resolve_at(root.clone(), cfg);
    if !models.present() {
        stats.set_phase(Phase::Unavailable);
        if !*said_unavailable {
            info!("{}", NightModels::how_to_get_it());
            *said_unavailable = true;
        }
        return Ok(());
    }
    let confidence = ConfidenceModel::resolve_at(root, 1);
    if !confidence.present() {
        // Without the cross-check there are no shaky rows to read and no second
        // voter to read them with. That is `unavailable`, not `idle`.
        stats.set_phase(Phase::Unavailable);
        if !*said_unavailable {
            info!("{}", ConfidenceModel::how_to_get_it());
            *said_unavailable = true;
        }
        return Ok(());
    }

    let idle_min = control.idle_minutes();
    let busy = gpu_busy_pct();
    if let Some(reason) = gate(control, cfg, local_minute_now(), idle_min, busy) {
        stats.set_phase(Phase::Blocked);
        if busy.is_some_and(|b| b > cfg.gpu_busy_max_pct) {
            stats.skipped_busy.fetch_add(1, Ordering::Relaxed);
        }
        if *said_blocked != reason {
            debug!("the night shift is standing down: {reason}");
            *said_blocked = reason;
        }
        return Ok(());
    }
    said_blocked.clear();

    let whisper = Whisper::new(&models, cfg, runtime);
    let mut canaries: Vec<Canary> = Vec::new();
    for lang in crate::canary::LANGS {
        match Canary::load(&confidence, lang) {
            Ok(c) => canaries.push(c),
            Err(e) => warn!("the night shift could not load the {lang} cross-check: {e:#}"),
        }
    }
    if canaries.is_empty() {
        stats.set_phase(Phase::Unavailable);
        return Ok(());
    }

    let model_id = models.model_id();
    let mut done = 0usize;
    stats.set_phase(Phase::Running);
    while done < cfg.max_rows_per_night {
        if stop.stopped() {
            break;
        }
        // Between batches, not once a night: the whole point of a ceiling on
        // GPU use is that somebody can take their machine back at 04:00.
        if let Some(reason) = gate(
            control,
            cfg,
            local_minute_now(),
            control.idle_minutes(),
            gpu_busy_pct(),
        ) {
            debug!("the night shift is standing down mid-run: {reason}");
            stats.set_phase(Phase::Blocked);
            return Ok(());
        }
        let n = night_batch(
            store,
            bus,
            &whisper,
            &mut canaries,
            cfg,
            data_dir,
            scratch,
            &model_id,
            stats,
        )?;
        if n == 0 {
            stats.set_phase(Phase::Idle);
            return Ok(());
        }
        done += n;
    }
    stats.set_phase(Phase::Idle);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn readings<'a>(
        live: &'a str,
        canary: Option<&'a str>,
        night: &'a str,
        night_lang: Option<&'a str>,
        row_lang: Option<&'a str>,
    ) -> Readings<'a> {
        Readings {
            live,
            canary,
            night,
            night_lang,
            row_lang,
        }
    }

    // ---- the clock ----

    #[test]
    fn a_window_that_wraps_midnight_contains_the_small_hours() {
        let h = Hours::parse("23:30-07:00").unwrap();
        assert!(h.contains(23 * 60 + 45));
        assert!(h.contains(3 * 60));
        assert!(!h.contains(12 * 60));
        assert!(!h.contains(7 * 60), "the end is exclusive");
    }

    #[test]
    fn the_shipped_window_is_the_small_hours_and_nothing_else() {
        let h = Hours::parse("03:00-07:00").unwrap();
        assert!(h.contains(3 * 60));
        assert!(h.contains(6 * 60 + 59));
        assert!(!h.contains(2 * 60 + 59));
        assert!(!h.contains(20 * 60));
    }

    #[test]
    fn a_malformed_window_never_opens() {
        for bad in ["", "03:00", "25:00-07:00", "3-7", "03:60-07:00"] {
            assert!(Hours::parse(bad).is_none(), "{bad} parsed");
        }
    }

    // ---- the gates ----

    fn control() -> Arc<Control> {
        Control::new(
            PathBuf::from("/nonexistent"),
            None,
            &crate::allowlist::Allowlist::from_rules([("VRChat.exe", true)]),
        )
    }

    fn on() -> NightConfig {
        NightConfig {
            enabled: true,
            ..NightConfig::default()
        }
    }

    #[test]
    fn the_gate_is_shut_while_the_feature_is_off() {
        let cfg = NightConfig::default();
        assert!(
            gate(&control(), &cfg, 4 * 60, 999, Some(0))
                .unwrap()
                .contains("off")
        );
    }

    #[test]
    fn the_gate_opens_inside_the_window_on_an_idle_gpu() {
        assert!(gate(&control(), &on(), 4 * 60, 0, Some(3)).is_none());
    }

    #[test]
    fn a_busy_gpu_shuts_the_gate_even_at_four_in_the_morning() {
        let reason = gate(&control(), &on(), 4 * 60, 0, Some(55)).unwrap();
        assert!(reason.contains("55%"), "{reason}");
    }

    #[test]
    fn an_unreadable_gpu_counter_shuts_the_gate() {
        // Fails closed on purpose: not knowing whether the card is busy is not
        // the same as knowing it is free.
        assert!(gate(&control(), &on(), 4 * 60, 0, None).is_some());
    }

    #[test]
    fn outside_the_window_a_long_idle_opens_the_gate_and_a_short_one_does_not() {
        assert!(gate(&control(), &on(), 14 * 60, 40, Some(2)).is_none());
        let reason = gate(&control(), &on(), 14 * 60, 5, Some(2)).unwrap();
        assert!(reason.contains("busy within"), "{reason}");
    }

    #[test]
    fn a_pause_shuts_the_gate_at_any_hour() {
        let c = control();
        c.pause();
        assert!(
            gate(&c, &on(), 4 * 60, 999, Some(0))
                .unwrap()
                .contains("paused")
        );
    }

    #[test]
    fn the_idle_path_can_be_switched_off_leaving_only_the_clock() {
        let cfg = NightConfig {
            also_when_idle_min: 0,
            ..on()
        };
        assert!(gate(&control(), &cfg, 14 * 60, 10_000, Some(0)).is_some());
        assert!(gate(&control(), &cfg, 4 * 60, 0, Some(0)).is_none());
    }

    #[test]
    fn the_gpu_counter_ignores_a_card_with_no_real_vram() {
        let dir = std::env::temp_dir().join(format!("nxr-night-gpu-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (card, vram, busy) in [("card0", 2u64 << 30, "97"), ("card1", 24u64 << 30, "4")] {
            let device = dir.join(card).join("device");
            std::fs::create_dir_all(&device).unwrap();
            std::fs::write(device.join("mem_info_vram_total"), vram.to_string()).unwrap();
            std::fs::write(device.join("gpu_busy_percent"), busy).unwrap();
        }
        assert_eq!(
            gpu_busy_pct_in(&dir),
            Some(4),
            "the iGPU must not gate the dGPU"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- the batch ----

    fn clip(seconds: f32, value: f32) -> Vec<f32> {
        vec![value; (seconds * SAMPLE_RATE as f32) as usize]
    }

    #[test]
    fn packing_leaves_exactly_one_gap_between_clips_and_none_at_the_ends() {
        let packed = pack(&[clip(1.0, 1.0), clip(2.0, 2.0), clip(0.5, 3.0)], 1.0);
        assert_eq!(
            packed.slots[0],
            Slot {
                from_s: 0.0,
                to_s: 1.0
            }
        );
        assert_eq!(
            packed.slots[1],
            Slot {
                from_s: 2.0,
                to_s: 4.0
            }
        );
        assert_eq!(
            packed.slots[2],
            Slot {
                from_s: 5.0,
                to_s: 5.5
            }
        );
        assert_eq!(packed.samples.len(), (5.5 * SAMPLE_RATE as f32) as usize);
        // The gap really is silence, and the clips really are where the slots
        // say they are.
        assert_eq!(packed.samples[(1.5 * SAMPLE_RATE as f32) as usize], 0.0);
        assert_eq!(packed.samples[(3.0 * SAMPLE_RATE as f32) as usize], 2.0);
    }

    #[test]
    fn the_offsets_round_trip_a_line_back_to_the_clip_it_came_from() {
        let packed = pack(&[clip(2.0, 1.0), clip(2.0, 2.0), clip(2.0, 3.0)], 1.0);
        // Slots: 0-2, 3-5, 6-8.
        let lines = [
            Utterance {
                from_s: 0.0,
                to_s: 2.0,
                text: "erste".into(),
            },
            Utterance {
                from_s: 3.0,
                to_s: 5.0,
                text: "zweite".into(),
            },
            Utterance {
                from_s: 6.0,
                to_s: 8.0,
                text: "dritte".into(),
            },
        ];
        assert_eq!(
            split_by_offsets(&lines, &packed.slots),
            vec!["erste", "zweite", "dritte"]
        );
    }

    #[test]
    fn a_line_that_overshoots_its_clip_still_lands_in_it() {
        // The decoder rounds edges outwards to the half second, so a two-second
        // clip routinely comes back as a line from just before it to just after.
        let slots = [
            Slot {
                from_s: 0.0,
                to_s: 2.0,
            },
            Slot {
                from_s: 3.0,
                to_s: 5.0,
            },
        ];
        let lines = [Utterance {
            from_s: 2.9,
            to_s: 5.4,
            text: "hallo du".into(),
        }];
        assert_eq!(split_by_offsets(&lines, &slots), vec!["", "hallo du"]);
    }

    #[test]
    fn a_line_wholly_inside_the_silence_is_dropped() {
        // Whisper's caption habit lives here: `(Musik)` over a one-second gap
        // belongs to nobody's turn.
        let slots = [
            Slot {
                from_s: 0.0,
                to_s: 2.0,
            },
            Slot {
                from_s: 3.0,
                to_s: 5.0,
            },
        ];
        let lines = [Utterance {
            from_s: 2.1,
            to_s: 2.8,
            text: "(Musik)".into(),
        }];
        assert_eq!(split_by_offsets(&lines, &slots), vec!["", ""]);
    }

    #[test]
    fn two_lines_in_one_clip_are_joined_in_order() {
        let slots = [Slot {
            from_s: 0.0,
            to_s: 4.0,
        }];
        let lines = [
            Utterance {
                from_s: 0.0,
                to_s: 2.0,
                text: "das war".into(),
            },
            Utterance {
                from_s: 2.0,
                to_s: 4.0,
                text: "eigentlich gut".into(),
            },
        ];
        assert_eq!(
            split_by_offsets(&lines, &slots),
            vec!["das war eigentlich gut"]
        );
    }

    // ---- the vote ----

    #[test]
    fn two_readings_that_agree_against_the_row_replace_it() {
        let r = readings(
            "komm ich sag the country",
            Some("ich sage dir keins"),
            "Ich sage dir keins.",
            Some("de"),
            Some("de"),
        );
        assert_eq!(
            judge_vote(&r, 0.5, true),
            Vote::Replace {
                text: "Ich sage dir keins.".into()
            }
        );
    }

    #[test]
    fn the_night_reading_alone_never_replaces_anything() {
        // Canary agrees with the live text: there is no majority against it,
        // and large-v3's dissent is one vote.
        let r = readings(
            "das war ganz gut",
            Some("das war ganz gut"),
            "Das war ganz schlecht.",
            Some("de"),
            Some("de"),
        );
        assert!(matches!(judge_vote(&r, 0.5, true), Vote::Annotate { .. }));
    }

    #[test]
    fn with_no_cross_check_reading_there_is_no_majority() {
        // Canary is empty on 11% of real turns (§11). An empty second voter is
        // not a vote for the third one.
        let r = readings(
            "hallo zusammen",
            Some(""),
            "Hallo zusammen alle.",
            Some("de"),
            Some("de"),
        );
        assert!(matches!(judge_vote(&r, 0.5, true), Vote::Annotate { .. }));
        let r = readings(
            "hallo zusammen",
            None,
            "Hallo zusammen alle.",
            Some("de"),
            Some("de"),
        );
        assert!(matches!(judge_vote(&r, 0.5, true), Vote::Annotate { .. }));
    }

    #[test]
    fn an_arabic_hallucination_on_a_german_row_never_replaces_it() {
        // FINDINGS §12, measured on the user's own audio: on the rows the live
        // decoder failed, large-v3 answered in Arabic, Finnish and Swedish.
        // Even with the cross-check nodding along, a reading that does not
        // classify as the row's language cannot take the row.
        for hallucination in [
            "شكرا لمشاهدتكم",
            "Kiitos kun katsoitte",
            "Tack för att ni tittade",
        ] {
            let r = readings(
                "ähm ja also",
                Some(hallucination),
                hallucination,
                Some("de"),
                Some("de"),
            );
            match judge_vote(&r, 0.5, true) {
                Vote::Annotate { text } => assert_eq!(text, hallucination),
                other => panic!("{hallucination} was allowed to win: {other:?}"),
            }
        }
    }

    #[test]
    fn a_reading_in_the_wrong_language_is_refused_even_when_it_is_a_real_language() {
        // The decoder was pointed at a German row and came back with English.
        // That is information — and not permission to overwrite German.
        let r = readings(
            "ich hab das gestern gemacht",
            Some("I did that yesterday"),
            "I did that yesterday.",
            Some("en"),
            Some("de"),
        );
        assert!(matches!(judge_vote(&r, 0.5, true), Vote::Annotate { .. }));
    }

    #[test]
    fn a_row_with_no_language_of_its_own_is_annotated_and_never_replaced() {
        let r = readings(
            "mhm ok",
            Some("Mhm okay dann"),
            "Mhm okay dann.",
            None,
            None,
        );
        assert!(matches!(judge_vote(&r, 0.5, true), Vote::Annotate { .. }));
    }

    #[test]
    fn a_one_word_reading_cannot_replace_a_row() {
        // The mint bar's floor and the cross-check's, for the same reason.
        let r = readings(
            "was hast du gesagt",
            Some("Ja"),
            "Ja.",
            Some("de"),
            Some("de"),
        );
        assert!(matches!(judge_vote(&r, 0.5, true), Vote::Annotate { .. }));
    }

    #[test]
    fn captions_are_stripped_before_anything_judges_the_text() {
        let r = readings(
            "hmm",
            Some("(sanfte Musik)"),
            "(sanfte Musik)",
            Some("de"),
            Some("de"),
        );
        // Nothing survives the strip, so there is no reading at all.
        assert_eq!(judge_vote(&r, 0.5, true), Vote::Nothing);
    }

    #[test]
    fn an_empty_night_reading_is_a_failure_of_the_decoder_not_a_silence() {
        let r = readings(
            "etwas gesagt",
            Some("etwas anderes gesagt"),
            "  ",
            Some("de"),
            Some("de"),
        );
        assert_eq!(judge_vote(&r, 0.5, true), Vote::Nothing);
    }

    // ---- the learned vote rules (0.12.4) ----

    #[test]
    fn a_cell_that_earned_it_lets_the_night_reading_win_without_a_second_voter() {
        // The same readings the two-of-three rule annotates — canary agrees
        // with the LIVE text, so there is no majority — under a cell whose
        // corrections say the night decoder is the one to believe.
        let r = readings(
            "das war ganz gut",
            Some("das war ganz gut"),
            "Das war völlig daneben leider.",
            Some("de"),
            Some("de"),
        );
        assert!(
            matches!(
                judge_vote_ruled(&r, 0.5, true, VoteRule::TwoOfThree),
                Vote::Annotate { .. }
            ),
            "the shipped rule still needs the second voter"
        );
        assert_eq!(
            judge_vote_ruled(&r, 0.5, true, VoteRule::NightWins),
            Vote::Replace {
                text: "Das war völlig daneben leider.".into()
            }
        );
    }

    #[test]
    fn night_wins_does_not_switch_off_a_single_guard() {
        // FINDINGS §12's hallucinations, under the most permissive rule the
        // learner can reach. A cell cannot buy its way past the language guard.
        for hallucination in [
            "شكرا لمشاهدتكم",
            "Kiitos kun katsoitte",
            "Tack för att ni tittade",
        ] {
            let r = readings("ähm ja also", None, hallucination, Some("de"), Some("de"));
            assert!(
                matches!(
                    judge_vote_ruled(&r, 0.5, true, VoteRule::NightWins),
                    Vote::Annotate { .. }
                ),
                "{hallucination} won under night_wins"
            );
        }
        // …and the one-word floor, and the row with no language of its own.
        let r = readings("was hast du gesagt", None, "Ja.", Some("de"), Some("de"));
        assert!(matches!(
            judge_vote_ruled(&r, 0.5, true, VoteRule::NightWins),
            Vote::Annotate { .. }
        ));
        let r = readings("mhm ok", None, "Mhm okay dann.", None, None);
        assert!(matches!(
            judge_vote_ruled(&r, 0.5, true, VoteRule::NightWins),
            Vote::Annotate { .. }
        ));
    }

    #[test]
    fn night_wins_still_does_not_replace_words_it_agrees_with() {
        let r = readings(
            "ich sage dir keins",
            None,
            "Ich sage dir keins.",
            Some("de"),
            Some("de"),
        );
        assert!(matches!(
            judge_vote_ruled(&r, 0.5, true, VoteRule::NightWins),
            Vote::Annotate { .. }
        ));
    }

    #[test]
    fn keep_live_refuses_a_replacement_the_shipped_rule_would_have_allowed() {
        let r = readings(
            "komm ich sag the country",
            Some("ich sage dir keins"),
            "Ich sage dir keins.",
            Some("de"),
            Some("de"),
        );
        assert_eq!(
            judge_vote_ruled(&r, 0.5, true, VoteRule::TwoOfThree),
            Vote::Replace {
                text: "Ich sage dir keins.".into()
            }
        );
        assert_eq!(
            judge_vote_ruled(&r, 0.5, true, VoteRule::KeepLive),
            Vote::Annotate {
                text: "Ich sage dir keins.".into()
            },
            "the reading is still kept beside the row"
        );
    }

    #[test]
    fn an_empty_reading_is_nothing_under_every_rule() {
        for rule in [
            VoteRule::TwoOfThree,
            VoteRule::NightWins,
            VoteRule::KeepLive,
        ] {
            let r = readings(
                "etwas gesagt",
                Some("etwas anderes"),
                "  ",
                Some("de"),
                Some("de"),
            );
            assert_eq!(judge_vote_ruled(&r, 0.5, true, rule), Vote::Nothing);
        }
    }

    #[test]
    fn with_replacement_switched_off_every_reading_is_only_an_annotation() {
        let r = readings(
            "komm ich sag the country",
            Some("ich sage dir keins"),
            "Ich sage dir keins.",
            Some("de"),
            Some("de"),
        );
        assert_eq!(
            judge_vote(&r, 0.5, false),
            Vote::Annotate {
                text: "Ich sage dir keins.".into()
            }
        );
    }
}
