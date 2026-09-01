//! The tiny local model (GRAPH.md Tier 3), as a child process.
//!
//! > "Tiny by requirement, not by concession (user constraint: ~4 CPU cores,
//! > GPU only if it must). Budget: ≤ 3B parameters, Q4 GGUF, ≤ ~2 GB on disk.
//! > **Bake-off done: Qwen2.5-3B-Instruct Q4 wins** — 9/9 trap rejections
//! > (zero invented obligations), 9/9 who and what on everything it extracted,
//! > 3.3 s/case, 1.9 GB."
//!
//! ## Why a child process and not a linked library
//!
//! `spike/graph_bench/run_bench.py` measured the winner by shelling out to
//! `llama-cli`. Doing the same thing in the daemon means Tier 3 costs this
//! crate no cmake, no C++ toolchain, no new link-time anything, and no way for
//! a model to take the capture process down with it — a wedged child is killed
//! and forgotten, where a wedged thread is a lost evening of transcript. The
//! binaries are an ordinary catalogue asset (`crate::models`, `Group::Graph`)
//! and the invocation below is the harness's, argument for argument.
//!
//! ## The two findings the bench proved, kept literally
//!
//! 1. **The schema must force a boolean verdict BEFORE any extractable field
//!    exists.** Plain object-or-null grammars bias every model toward
//!    extraction — bigger models were *worse* until the verdict-first fix. The
//!    grammar in `grammars/commitment.gbnf` is the bench's file byte for byte,
//!    and [`commitment_grammar`] returns it unchanged for the two-speaker case
//!    the bench measured.
//! 2. **Few-shot examples in the prompt are load-bearing.** [`COMMITMENT_SYSTEM`]
//!    is the harness's system prompt verbatim, examples included. Editing it is
//!    editing a measured result; the test at the bottom of this file says so.
//!
//! ## Discipline
//!
//! Same as ASR's, and for the same reason (DESIGN §4's runtime rule): nice 19
//! and the `[runtime] inference_cpus` pin, applied to the child between fork
//! and exec so llama.cpp's own threads inherit both. `--temp 0`, so the same
//! window always produces the same answer. A per-call timeout, after which the
//! child is killed rather than waited on.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde_json::Value;
use tracing::{debug, warn};

use crate::config::{GraphConfig, RuntimeConfig};
use crate::models::GraphModels;

/// The bake-off's grammar, shipped in the crate and identical to
/// `spike/graph_bench/commitment.gbnf`. Verdict-first: `is_commitment` is
/// decided before any field that would tempt the model into filling it in.
pub const COMMITMENT_GBNF: &str = include_str!("../grammars/commitment.gbnf");

/// Distinct voices one window may carry. The grammar names speakers by letter,
/// so this is where the alphabet stops being useful — and it is well past what
/// the threading rule produces (`threads::RECENT_SPEAKERS` is 4). A conversation
/// with more voices than this gets a topic and no commitment pass, which is the
/// honest failure: the model would have nothing to call the seventh person.
pub const MAX_SPEAKERS: usize = 6;

/// The bake-off's system prompt, **verbatim**, few-shot examples included.
///
/// Every clause here bought a trap rejection. The negatives are the trap list
/// from `spike/graph_bench/cases.json`: suggestions, questions, refusals, hedged
/// hypotheticals, past actions, in-game banter, things about oneself only, and
/// reported promises of absent third people.
pub const COMMITMENT_SYSTEM: &str = concat!(
    "You extract commitments from chat-lobby dialogue between speakers A and B. ",
    "A commitment is one speaker in THIS dialogue promising the OTHER a concrete ",
    "future action. NOT commitments: suggestions (we should...), questions, ",
    "refusals, hedged hypotheticals (if I ever... I guess), past/already-done ",
    "actions, in-game banter (I will kill you next round), things about oneself ",
    "only (I will probably sleep), or reported promises of absent third people. ",
    "Dialogue may be German, English or mixed; keep what/due in the original ",
    "language. Output ONLY JSON.\n",
    "Examples:\n",
    "A: we should totally do a photo -> {\"is_commitment\": false}\n",
    "B: ich hab dir das gestern geschickt -> {\"is_commitment\": false}\n",
    "B: I will probably just log off soon -> {\"is_commitment\": false}\n",
    "B: ich schick dir morgen den Link -> {\"is_commitment\": true, \"who\": \"B\", ",
    "\"what\": \"den Link schicken\", \"due\": \"morgen\"}"
);

/// One string per conversation. Deliberately not the commitment grammar with a
/// field bolted on: naming a conversation has no verdict to get wrong, so it
/// gets the smallest schema that can be right.
pub const TOPIC_GBNF: &str = concat!(
    "root ::= \"{\" ws \"\\\"topic\\\":\" ws str ws \"}\"\n",
    "str ::= \"\\\"\" chars \"\\\"\"\n",
    "chars ::= char chars | char\n",
    "char ::= [^\"\\\\\\x00-\\x1f] | \"\\\\\" ([\"\\\\/bfnrt] | \"u\" [0-9a-fA-F] [0-9a-fA-F] [0-9a-fA-F] [0-9a-fA-F])\n",
    "ws ::= [ \\t\\n]?\n"
);

/// Few-shot again, for the same reason. A topic is a noun phrase, in the
/// language the conversation was in, and emphatically not a sentence about the
/// people in it — a label that reads like a dossier entry is the failure mode
/// this whole feature has to avoid.
pub const TOPIC_SYSTEM: &str = concat!(
    "You name what a short chat-lobby conversation was ABOUT. Answer with a ",
    "noun phrase of two to five words in the language most of the dialogue is ",
    "in. Never a sentence, never a verb about the speakers, never anybody's ",
    "name, never a judgement about them. If the dialogue is small talk with no ",
    "subject, say so plainly. Output ONLY JSON.\n",
    "Examples:\n",
    "A: which portal was it -> B: the stairwell one, it opens after dark ",
    "-> {\"topic\": \"world portals\"}\n",
    "A: ich hab die Doku angefangen -> B: schick mir den Link ",
    "-> {\"topic\": \"Dokumentation\"}\n",
    "A: hey -> B: hi, how's it going -> {\"topic\": \"small talk\"}"
);

/// Tokens one commitment answer may take. The grammar bounds the *shape*; this
/// bounds a model that decides to describe a promise at length.
const COMMITMENT_TOKENS: i32 = 160;
const TOPIC_TOKENS: i32 = 40;

/// Longest topic label kept. Past this it has stopped being a label.
const MAX_TOPIC: usize = 48;

/// One turn, as the model is shown it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    /// The segment this turn is, so an extraction can point back at a row.
    pub segment_id: i64,
    /// Canonical speaker, or `None` for a turn nobody could place.
    pub speaker_id: Option<i64>,
    pub t_start_ns: i64,
    pub text: String,
}

/// What the model said about a window, before anything is resolved against the
/// database or the calendar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extraction {
    /// Index into the window's speaker roster — the letter, decoded.
    pub who: usize,
    pub what: String,
    /// The due phrase, in the language it was said in. Resolved by
    /// [`crate::timeref`] against the turn's own capture time, exactly as a
    /// Tier 2 reference is.
    pub due: Option<String>,
}

/// A resolved runner: the model, the binary, and the discipline both run under.
#[derive(Debug, Clone)]
pub struct Llm {
    cli: PathBuf,
    lib_dir: PathBuf,
    model: PathBuf,
    model_id: String,
    threads: i32,
    gpu_layers: i32,
    timeout: Duration,
    nice: i32,
    cpus: Vec<usize>,
}

impl Llm {
    /// `None` when Tier 3 is not installed, which is the ordinary state: the
    /// assets are an opt-in group and the feature ships switched off.
    pub fn resolve(
        models_root: &Path,
        graph: &GraphConfig,
        runtime: &RuntimeConfig,
    ) -> Option<Self> {
        let found = GraphModels::resolve(models_root, graph);
        if !found.present() {
            return None;
        }
        Some(Self {
            model_id: found.model_id(),
            cli: found.cli,
            lib_dir: found.lib_dir,
            model: found.model,
            threads: graph.llm_threads.max(1),
            gpu_layers: graph.gpu_layers.max(0),
            timeout: Duration::from_secs(graph.llm_timeout_s.max(5)),
            nice: runtime.inference_nice,
            cpus: runtime.inference_cpus.clone(),
        })
    }

    /// Stored on every row this model writes, so a row produced by one model is
    /// never mistaken for a row produced by another.
    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    pub fn model_path(&self) -> &Path {
        &self.model
    }

    /// Is there a promise in this window, and whose?
    ///
    /// `Ok(None)` is the answer the bench cared about most: nine times out of
    /// nine on its trap set, the model declined. A refusal is a result, never a
    /// failure — and it is the reason this tier is allowed to overwrite what
    /// the Tier 2 rules guessed.
    pub fn commitment(&self, window: &[Line]) -> Result<Option<Extraction>> {
        let roster = roster(window);
        if roster.len() < 2 || roster.len() > MAX_SPEAKERS {
            // One voice has nobody to promise; more than the alphabet has
            // nobody the model can name.
            return Ok(None);
        }
        let out = self.run(
            COMMITMENT_SYSTEM,
            &transcript(window, &roster),
            &commitment_grammar(roster.len()),
            COMMITMENT_TOKENS,
        )?;
        let Some(value) = first_json(&out) else {
            warn!("the model produced nothing a grammar should have allowed");
            return Ok(None);
        };
        if value.get("is_commitment").and_then(Value::as_bool) != Some(true) {
            return Ok(None);
        }
        let who = value
            .get("who")
            .and_then(Value::as_str)
            .and_then(letter_index)
            .filter(|i| *i < roster.len());
        let what = value
            .get("what")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        // Both or neither: a commitment with no promiser or no substance is not
        // one, and inventing either half is exactly the failure this tier is
        // here to avoid.
        let (Some(who), Some(what)) = (who, what) else {
            warn!("the model claimed a commitment without saying who or what");
            return Ok(None);
        };
        let said = transcript(window, &roster);
        Ok(Some(Extraction {
            who,
            what: what.to_string(),
            due: value
                .get("due")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("null"))
                // …and only if somebody actually said it. Measured on the three
                // lobby-shaped windows below: asked about "sure, I will cut it
                // and send it over", the model answers `due: "soon"` — a
                // perfectly reasonable paraphrase of a promise with no date,
                // and a date nobody gave. The grammar can force the shape of an
                // answer but not its honesty, so the one field that is supposed
                // to be a QUOTE is checked against the transcript. Dropping a
                // real date we cannot find is the safe direction; inventing one
                // is not.
                .filter(|s| quoted(s, &said))
                .map(str::to_string),
        }))
    }

    /// What was this conversation about? A short noun phrase, or `None`.
    pub fn topic(&self, window: &[Line]) -> Result<Option<String>> {
        let roster = roster(window);
        if roster.is_empty() {
            return Ok(None);
        }
        let out = self.run(
            TOPIC_SYSTEM,
            &transcript(window, &roster),
            TOPIC_GBNF,
            TOPIC_TOKENS,
        )?;
        let Some(value) = first_json(&out) else {
            return Ok(None);
        };
        Ok(value
            .get("topic")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| truncate(s, MAX_TOPIC)))
    }

    /// One call. The argument list is `run_bench.py`'s, in the same order, with
    /// the affinity moved from `taskset` into the child itself.
    fn run(&self, system: &str, prompt: &str, grammar: &str, max_tokens: i32) -> Result<String> {
        let started = Instant::now();
        let mut cmd = Command::new(&self.cli);
        cmd.arg("-m")
            .arg(&self.model)
            .args(["-t", &self.threads.to_string()])
            .args(["--temp", "0"])
            .args(["-n", &max_tokens.to_string()])
            .arg("--single-turn")
            .arg("--grammar")
            .arg(grammar)
            .arg("-sys")
            .arg(system)
            .arg("-p")
            .arg(prompt)
            .arg("--no-display-prompt")
            .arg("--no-warmup")
            .args(["-ngl", &self.gpu_layers.to_string()])
            // The release binaries find each other by plain name, exactly as
            // the bench harness ran them.
            .env("LD_LIBRARY_PATH", &self.lib_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let nice = self.nice;
        let cpus = self.cpus.clone();
        // SAFETY: between fork and exec, in a single-threaded child. Both calls
        // are plain syscalls with no allocation and no locks, which is the bar
        // for this hook; a failure of either is ignored on purpose, because
        // running at the wrong priority beats not running.
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

        let mut child = cmd
            .spawn()
            .with_context(|| format!("running {}", self.cli.display()))?;
        let stdout = drain(child.stdout.take());
        let stderr = drain(child.stderr.take());

        let status = match wait_with_timeout(&mut child, self.timeout)? {
            Some(status) => status,
            None => {
                // A wedged child is holding four cores. Kill it, reap it, and
                // let the worker move on: one conversation is not worth a
                // process that will not stop.
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout.join();
                let _ = stderr.join();
                bail!(
                    "the model did not answer within {}s and was killed",
                    self.timeout.as_secs()
                );
            }
        };
        let out = stdout.join().unwrap_or_default();
        let err = stderr.join().unwrap_or_default();
        debug!(
            ms = started.elapsed().as_millis() as u64,
            bytes = out.len(),
            "model call"
        );
        if !status.success() {
            bail!(
                "{} exited {}: {}",
                self.cli.display(),
                status.code().unwrap_or(-1),
                err.lines().rev().take(3).collect::<Vec<_>>().join(" / ")
            );
        }
        Ok(out)
    }
}

/// Read a pipe to the end on its own thread. Without this the child blocks on a
/// full pipe buffer while we sit in `try_wait`, which looks exactly like a hang
/// and is not one.
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
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ---------------------------------------------------------------------------
// the window, as the model reads it
// ---------------------------------------------------------------------------

/// Distinct speakers in a window, in the order they first spoke — which is the
/// order the letters are handed out in, so A is whoever started.
pub fn roster(window: &[Line]) -> Vec<i64> {
    let mut out: Vec<i64> = Vec::new();
    for line in window {
        if let Some(id) = line.speaker_id
            && !out.contains(&id)
        {
            out.push(id);
        }
    }
    out
}

/// `A: …` / `B: …`, exactly the shape the bench's windows had. Turns nobody
/// could place are dropped rather than given a letter: an anonymous line is not
/// a party to anything, and letting the model attribute one would be inventing
/// a promiser.
pub fn transcript(window: &[Line], roster: &[i64]) -> String {
    window
        .iter()
        .filter_map(|line| {
            let id = line.speaker_id?;
            let i = roster.iter().position(|s| *s == id)?;
            Some(format!("{}: {}", letter(i), line.text.trim()))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Does `phrase` appear in `text`, ignoring case and the three German umlauts'
/// capitals? The same fold [`crate::timeref`] uses, and for the same reason.
fn quoted(phrase: &str, text: &str) -> bool {
    let fold = |s: &str| -> String {
        s.chars()
            .map(|c| match c {
                'A'..='Z' => c.to_ascii_lowercase(),
                'Ä' => 'ä',
                'Ö' => 'ö',
                'Ü' => 'ü',
                other => other,
            })
            .collect()
    };
    fold(text).contains(&fold(phrase))
}

fn letter(i: usize) -> char {
    (b'A' + (i as u8).min(25)) as char
}

fn letter_index(s: &str) -> Option<usize> {
    let c = s.trim().chars().next()?.to_ascii_uppercase();
    c.is_ascii_uppercase().then(|| (c as u8 - b'A') as usize)
}

/// The bench's grammar for two speakers, and the same grammar with a wider
/// `who` alternation for more.
///
/// Two is returned **unchanged**, byte for byte, because two is what the
/// bake-off measured: nothing about a three-way conversation is allowed to
/// change the file the numbers in GRAPH.md were produced with.
pub fn commitment_grammar(speakers: usize) -> String {
    if speakers <= 2 {
        return COMMITMENT_GBNF.to_string();
    }
    let who = format!(
        "who ::= {}",
        (0..speakers.min(MAX_SPEAKERS))
            .map(|i| format!("\"\\\"{}\\\"\"", letter(i)))
            .collect::<Vec<_>>()
            .join(" | ")
    );
    let mut out: String = COMMITMENT_GBNF
        .lines()
        .map(|l| {
            if l.starts_with("who ::=") {
                who.as_str()
            } else {
                l
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    out.push('\n');
    out
}

/// The first JSON object in the model's output. The grammar means there should
/// be exactly one and nothing else; this is the same defensive scrape the bench
/// harness did, for the same reason — a runner that prints a banner must not
/// look like a model that failed.
pub fn first_json(text: &str) -> Option<Value> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let rx = RE.get_or_init(|| Regex::new(r"(?s)\{.*\}").expect("a compile-time pattern"));
    serde_json::from_str(rx.find(text)?.as_str()).ok()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars()
        .take(max)
        .collect::<String>()
        .trim_end()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(segment_id: i64, speaker: Option<i64>, text: &str) -> Line {
        Line {
            segment_id,
            speaker_id: speaker,
            t_start_ns: segment_id * 1_000_000_000,
            text: text.into(),
        }
    }

    /// The load-bearing finding, asserted rather than remembered: the grammar
    /// this daemon runs IS the grammar the bake-off measured.
    #[test]
    fn the_shipped_grammar_is_the_bench_file_byte_for_byte() {
        let bench = include_str!("../../../spike/graph_bench/commitment.gbnf");
        assert_eq!(
            COMMITMENT_GBNF, bench,
            "the shipped grammar drifted from the one the numbers in GRAPH.md came from"
        );
        assert_eq!(
            commitment_grammar(2),
            bench,
            "two speakers is the bench case"
        );
        assert_eq!(commitment_grammar(1), bench);
    }

    /// Verdict-first is the whole finding: `is_commitment` is decided before
    /// any field that would tempt a model into filling it in.
    #[test]
    fn the_grammar_puts_the_verdict_before_anything_extractable() {
        let root = COMMITMENT_GBNF
            .lines()
            .find(|l| l.starts_with("root ::="))
            .expect("a root rule");
        assert!(root.contains("is_commitment"), "{root}");
        assert!(
            !root.contains("who") && !root.contains("what"),
            "the root rule mentions an extractable field before the verdict: {root}"
        );
        let verdict = COMMITMENT_GBNF
            .lines()
            .find(|l| l.starts_with("verdict ::="))
            .expect("a verdict rule");
        assert!(
            verdict.starts_with("verdict ::= \"false\" | \"true\""),
            "the false branch must be reachable without any field at all: {verdict}"
        );
    }

    /// The other load-bearing finding: the few-shot examples are in the prompt.
    #[test]
    fn the_system_prompt_keeps_its_few_shot_examples_and_its_trap_list() {
        assert!(COMMITMENT_SYSTEM.contains("Examples:"));
        assert_eq!(
            COMMITMENT_SYSTEM.matches("is_commitment").count(),
            4,
            "the four worked examples are what made the small model refuse"
        );
        for trap in [
            "suggestions",
            "questions",
            "refusals",
            "hedged hypotheticals",
            "past/already-done",
            "in-game banter",
            "absent third people",
        ] {
            assert!(
                COMMITMENT_SYSTEM.contains(trap),
                "the prompt dropped {trap:?}"
            );
        }
        // Both languages, because half of what this daemon hears is German.
        assert!(COMMITMENT_SYSTEM.contains("German, English or mixed"));
    }

    #[test]
    fn a_third_speaker_widens_only_the_who_rule() {
        let three = commitment_grammar(3);
        assert!(
            three.contains(r#"who ::= "\"A\"" | "\"B\"" | "\"C\"""#),
            "{three}"
        );
        // Everything else is untouched.
        for rule in ["root ::=", "verdict ::=", "null ::=", "str ::=", "ws ::="] {
            let mine = three.lines().find(|l| l.starts_with(rule)).unwrap();
            let bench = COMMITMENT_GBNF
                .lines()
                .find(|l| l.starts_with(rule))
                .unwrap();
            assert_eq!(mine, bench, "{rule} changed");
        }
        assert_eq!(
            commitment_grammar(MAX_SPEAKERS).matches('|').count(),
            COMMITMENT_GBNF.matches('|').count() + MAX_SPEAKERS - 2
        );
    }

    #[test]
    fn a_window_reads_as_the_bench_windows_did() {
        let window = [
            line(1, Some(7), "send me the link, I want to see the shaders"),
            line(2, Some(9), "yeah I'll send it to you tonight"),
            line(3, Some(7), "no rush"),
        ];
        let roster = roster(&window);
        assert_eq!(roster, vec![7, 9], "letters go out in speaking order");
        assert_eq!(
            transcript(&window, &roster),
            "A: send me the link, I want to see the shaders\n\
             B: yeah I'll send it to you tonight\n\
             A: no rush"
        );
    }

    /// An anonymous turn is not a party to anything. Letting the model see one
    /// with a letter on it would be handing it a promiser to invent.
    #[test]
    fn an_unidentified_turn_is_not_shown_to_the_model() {
        let window = [
            line(1, Some(7), "who is doing the recording"),
            line(2, None, "somebody in the corner"),
            line(3, Some(9), "I'll do it"),
        ];
        let roster = roster(&window);
        assert_eq!(roster, vec![7, 9]);
        let text = transcript(&window, &roster);
        assert!(!text.contains("corner"), "{text}");
        assert_eq!(text.lines().count(), 2);
    }

    /// The grammar can force the shape of an answer but not its honesty, and
    /// the due field is the one that is supposed to be a quote.
    #[test]
    fn a_due_phrase_nobody_said_is_not_a_date() {
        assert!(quoted("morgen", "B: ich schick dir morgen den Link"));
        assert!(
            quoted("Morgen", "B: ich schick dir morgen den Link"),
            "case"
        );
        assert!(quoted(
            "on Friday",
            "A: I will drop them in your DMs on Friday"
        ));
        assert!(quoted("Freitag", "B: ich mach die Doku bis Freitag fertig"));
        // The real one, measured: asked about a promise with no date, the model
        // answers "soon" — reasonable, and nobody said it.
        assert!(!quoted("soon", "B: sure, I will cut it and send it over"));
        assert!(!quoted("next week", "B: I'll send it over"));
    }

    #[test]
    fn letters_decode_back_to_roster_positions() {
        assert_eq!(letter_index("A"), Some(0));
        assert_eq!(letter_index("\"B\""), None, "the JSON is already unquoted");
        assert_eq!(letter_index("b"), Some(1));
        assert_eq!(letter_index(""), None);
        assert_eq!(letter(0), 'A');
        assert_eq!(letter(5), 'F');
    }

    #[test]
    fn the_output_scrape_survives_a_runner_that_prints_around_it() {
        let v = first_json("build: 10736\n{\"is_commitment\": false}\n[end of text]")
            .expect("the object");
        assert_eq!(v["is_commitment"], serde_json::json!(false));
        assert!(first_json("no json here").is_none());
        assert!(first_json("{not json}").is_none());
    }

    #[test]
    fn a_topic_label_is_bounded_and_the_grammar_is_a_single_string() {
        assert!(TOPIC_GBNF.starts_with("root ::= \"{\" ws \"\\\"topic\\\":\""));
        assert!(TOPIC_SYSTEM.contains("Examples:"));
        assert!(TOPIC_SYSTEM.contains("never anybody's name"));
        assert_eq!(truncate("world portals", MAX_TOPIC), "world portals");
        assert_eq!(
            truncate(&"x".repeat(80), MAX_TOPIC).chars().count(),
            MAX_TOPIC
        );
    }

    /// Not installed is a normal state, not an error: the assets are an opt-in
    /// group and the feature ships off.
    #[test]
    fn an_absent_model_resolves_to_nothing_rather_than_failing() {
        let cfg = GraphConfig::default();
        let rt = RuntimeConfig::default();
        assert!(Llm::resolve(Path::new("/definitely/not/here"), &cfg, &rt).is_none());
    }

    // ---- against the real model --------------------------------------------
    //
    // Gated on NXR_GRAPH_MODELS, following the NXR_MODELS convention in
    // tests/socket.rs: absent, these skip and pass, because the model is a
    // 1.9 GB optional download and CI does not have one.

    fn staged() -> Option<Llm> {
        let raw = std::env::var("NXR_GRAPH_MODELS").ok()?;
        if raw.trim().is_empty() {
            return None;
        }
        let path = PathBuf::from(raw);
        assert!(
            path.is_absolute(),
            "NXR_GRAPH_MODELS must be an absolute path, got {}",
            path.display()
        );
        let llm = Llm::resolve(&path, &GraphConfig::default(), &RuntimeConfig::default());
        assert!(
            llm.is_some(),
            "NXR_GRAPH_MODELS={} has no qwen2.5-3b-instruct-q4_k_m.gguf and llama/llama-cli",
            path.display()
        );
        llm
    }

    #[test]
    fn the_real_model_extracts_a_promise_and_refuses_a_trap() {
        let Some(llm) = staged() else {
            eprintln!(
                "skipping the Tier-3 round trip: set NXR_GRAPH_MODELS=<models dir> to run it"
            );
            return;
        };
        assert!(
            llm.model_id().starts_with("qwen2.5-3b"),
            "{}",
            llm.model_id()
        );

        // A positive from the bench's gold set.
        let promise = [
            line(1, Some(7), "hast du das Video noch?"),
            line(2, Some(9), "ja klar, ich schick dir morgen den Link"),
        ];
        let got = llm
            .commitment(&promise)
            .expect("the runner ran")
            .expect("a promise the bench scored as found");
        assert_eq!(got.who, 1, "B made the promise");
        assert!(
            got.what.to_lowercase().contains("link"),
            "the what lost its object: {got:?}"
        );
        assert!(
            got.due
                .as_deref()
                .is_some_and(|d| d.to_lowercase().contains("morgen")),
            "the due phrase is not the one that was said: {got:?}"
        );

        // A trap: future tense about oneself, no obligation to anybody.
        let trap = [
            line(1, Some(7), "we are all logging off soon then"),
            line(2, Some(9), "I will probably just sleep after this"),
        ];
        assert_eq!(
            llm.commitment(&trap).expect("the runner ran"),
            None,
            "an invented obligation poisons the feature — this is the case the 3B won on"
        );
    }

    /// Three windows shaped like the transcripts this daemon actually holds —
    /// German, English, and the Denglisch half-and-half a VRChat lobby is full
    /// of — driven through the shipped code path rather than the bench's.
    ///
    /// The assertions are deliberately about *shape*, not about exact strings:
    /// the numbers live in GRAPH.md and were measured by the bench. What this
    /// proves is that the daemon's own invocation reproduces them.
    #[test]
    fn the_real_model_reads_three_lobby_shaped_windows() {
        let Some(llm) = staged() else {
            eprintln!(
                "skipping the three-window pass: set NXR_GRAPH_MODELS=<models dir> to run it"
            );
            return;
        };
        let cases: [(&str, Vec<Line>, bool); 3] = [
            (
                "de · a link, promised for tomorrow",
                vec![
                    line(
                        1,
                        Some(7),
                        "hast du das Video noch von dem Bar-World-Abend?",
                    ),
                    line(2, Some(9), "ja klar, ich schick dir morgen den Link"),
                    line(3, Some(7), "perfekt, danke dir"),
                ],
                true,
            ),
            (
                "en · a recording, no date said",
                vec![
                    line(4, Some(7), "can I get the recording from the meetup?"),
                    line(5, Some(9), "sure, I will cut it and send it over"),
                ],
                true,
            ),
            (
                "mixed · banter, which is the trap the rules cannot see",
                vec![
                    line(6, Some(7), "du bist so tot nächste Runde"),
                    line(7, Some(9), "I will kill you next round, watch me"),
                    line(8, Some(7), "wir werden sehen"),
                ],
                false,
            ),
        ];

        for (name, window, expected) in cases {
            let started = std::time::Instant::now();
            let got = llm.commitment(&window).expect("the runner ran");
            let topic = llm.topic(&window).expect("the runner ran");
            eprintln!(
                "{name}\n  commitment: {got:?}\n  topic: {topic:?}\n  {:.1}s",
                started.elapsed().as_secs_f64()
            );
            assert_eq!(
                got.is_some(),
                expected,
                "{name}: the model {} a commitment",
                if expected { "missed" } else { "invented" }
            );
            if let Some(g) = &got {
                assert!(g.who < roster(&window).len(), "{name}: {g:?}");
                assert!(!g.what.trim().is_empty(), "{name}: an empty what");
            }
            let topic = topic.expect("every window gets a label");
            assert!(topic.chars().count() <= MAX_TOPIC, "{name}: {topic:?}");
            assert!(!topic.ends_with('.'), "{name}: a label is not a sentence");
        }
    }

    #[test]
    fn the_real_model_names_a_conversation() {
        let Some(llm) = staged() else {
            eprintln!(
                "skipping the Tier-3 topic pass: set NXR_GRAPH_MODELS=<models dir> to run it"
            );
            return;
        };
        let window = [
            line(
                1,
                Some(7),
                "wait, which portal was it — the one behind the bar or the one in the stairwell?",
            ),
            line(
                2,
                Some(9),
                "the stairwell one, but it only opens after the lights go down",
            ),
            line(
                3,
                Some(7),
                "I got dropped into the wrong instance again, give me a second",
            ),
        ];
        let topic = llm
            .topic(&window)
            .expect("the runner ran")
            .expect("a label");
        assert!(topic.chars().count() <= MAX_TOPIC, "{topic:?}");
        assert!(
            !topic.ends_with('.'),
            "a label is not a sentence: {topic:?}"
        );
        assert!(
            topic.split_whitespace().count() <= 6,
            "that is a summary, not a label: {topic:?}"
        );
        eprintln!("topic: {topic:?}");
    }
}
