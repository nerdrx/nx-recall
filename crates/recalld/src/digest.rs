//! One paragraph per conversation, written overnight (PROTOCOL 0.9.0).
//!
//! Every other Tier 3 output is something the daemon was *asked* for. A
//! commitment is surfaced because somebody promised something; a topic label is
//! three words on a row that already exists. A digest is a paragraph the daemon
//! volunteered about your evening, and that changes what the failure mode is.
//!
//! **The failure is not a wrong summary. It is a summary of nothing.** A VRChat
//! evening produces dozens of "conversations" by the threading rule — eight
//! turns of "ja / ne / lol", a microphone check, four rounds of a game and the
//! callouts around them. Every one of those is two voices inside the gap, and
//! summarising them means a morning card of ten paragraphs that say nothing,
//! which is how a feature becomes something people scroll past.
//!
//! So the verdict comes first, exactly as the commitment bake-off's does and
//! for exactly the same measured reason (`crate::llm`, finding 1) — and here it
//! comes first in its own model call, with a grammar that cannot express a
//! summary at all. `spike/digest_bench` is the gate: six traps in the shapes
//! above and four real conversations, on four pinned cores at nice 19.
//! **6/6 traps refused, 4/4 conversations summarised**, 3.4 s median.
//!
//! ### Two calls, which is what the numbers forced
//!
//! One call that decided *and* wrote degraded monotonically as the prompt grew.
//! Same model, same ten cases (`spike/digest_bench`):
//!
//! | prompt | traps refused | right language | open list right |
//! |---|---:|---:|---:|
//! | short, no language steering | **6/6** | 1/4 | 1/4 |
//! | + an `open` clause and a language order | 5/6 | 2/4 | 4/4 |
//! | + a worked trap example as well | 1/6 | 3/4 | 4/4 |
//!
//! Every clause that made the summary better made the refusal worse. That is
//! the verdict-first finding with the knife turned the other way: the verdict
//! is cheap to lose, and everything competing for the model's attention is what
//! loses it.
//!
//! So the verdict gets a prompt with nothing else in it ([`VERDICT_SYSTEM`],
//! [`VERDICT_GBNF`]), and the summary — which only runs for a conversation that
//! passed — gets all the steering it wants ([`summary_system`],
//! [`SUMMARY_GBNF`]). A trap costs one short call instead of one long one, and
//! traps are most conversations, so the split is also **cheaper**: 4.1 s median
//! against 11.7 s.
//!
//! ### The language, which is in the system prompt for a reason
//!
//! A digest is written in the conversation's own language. As a line of the
//! *input* — "Language: German", or an order at the end — that was obeyed on
//! one German conversation in four; the model read eight turns of German and
//! answered in English anyway. What works is the instruction in the **system**
//! prompt, written **in the language it asks for**, with the worked example in
//! that language too — the same shape [`crate::translate`] uses, and for the
//! same reason. The daemon knows the conversation's language before it calls,
//! so there was never a reason to make the model read it out of the transcript.
//!
//! ### One digest per conversation, ever
//!
//! `digests.thread_id` is the primary key. A conversation that resumes after a
//! digest was written keeps the digest it has, which is an honest limitation:
//! the alternative is a paragraph that changes under somebody who is reading
//! it. A conversation the model refused is written down too, with an empty
//! summary — a refusal is a result, and without it the worker asks about the
//! same eight "ja"s every ten seconds until the machine is switched off.
//!
//! ### The lock discipline, which is not negotiable
//!
//! [`crate::enrich`]'s, unchanged and for the reason written there in blood on
//! 2026-09-02: gather under the store lock, ask the model without it, commit
//! under it again. **Model time and store-lock time never overlap.**

use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};
use tracing::{debug, warn};

use crate::bus::{Bus, Topic};
use crate::clock::{civil_from_days, local_offset_s, ns_to_ms};
use crate::config::AssistConfig;
use crate::control::Control;
use crate::lang::Lang;
use crate::llm::{Line, Llm, first_json};
use crate::store::{DigestRow, Store};

/// The verdict's grammar: one boolean and nothing else, byte for byte
/// `spike/digest_bench/verdict.gbnf`. There is no extractable field anywhere in
/// it, which is verdict-first taken to its end — the model cannot be tempted by
/// a schema it never sees.
pub const VERDICT_GBNF: &str = include_str!("../grammars/verdict.gbnf");

/// The summary's grammar, `spike/digest_bench/summary.gbnf`. No verdict in it:
/// by the time this runs the question has been answered.
pub const SUMMARY_GBNF: &str = include_str!("../grammars/summary.gbnf");

/// The verdict's system prompt, **verbatim** from the bench.
///
/// Each clause bought a trap. "Agreeing to play another round is not a plan" is
/// the last one added and it is the one that took the traps from 5/6 to 6/6:
/// without it the model read `rematch? / ja los / gg wp` as two people making
/// arrangements and wrote *"A and B play a game, A suggests a rematch, which B
/// agrees to"* — a true sentence about nothing, which is the exact failure this
/// gate exists for.
pub const VERDICT_SYSTEM: &str = concat!(
    "You decide whether one conversation from a chat lobby is worth ",
    "summarising. NOT worth summarising: greetings and goodbyes, backchannel ",
    "(yeah, mhm, ja, ne, lol, ok), in-game callouts (gg, nice, rematch, oof), ",
    "microphone checks, and any exchange whose only subject is the ",
    "conversation itself. Agreeing to play another round is not a plan. Worth ",
    "summarising: anything with a subject somebody could ask about a week ",
    "later. Output ONLY JSON.\n",
    "Examples:\n",
    "A: ja -> B: ne -> A: lol -> B: ja ok -> {\"worth_summarising\": false}\n",
    "A: hey -> B: hi, na? -> A: alles gut? -> B: ja passt -> ",
    "{\"worth_summarising\": false}\n",
    "A: gg -> B: nice one -> A: oof -> B: der war knapp -> A: rematch? -> ",
    "B: ja los -> A: gg -> B: gg wp -> {\"worth_summarising\": false}\n",
    "A: hast du das Video noch? -> B: ja klar, ich schick dir morgen den Link ",
    "-> {\"worth_summarising\": true}"
);

/// The summary's system prompt, per language, **verbatim** from the bench.
///
/// The language instruction is here and not in the input, and it is written in
/// the language it asks for with the worked example in that language too. See
/// the module note: as an input line it was obeyed once in four.
pub fn summary_system(tag: &str) -> String {
    // Raw strings, because both halves are JSON with quotes all through them
    // and a prompt that is a measured artefact must be readable as itself.
    let (order, example) = if tag == "de" {
        (
            "Schreibe AUF DEUTSCH. Jedes Wort von summary und open muss \
             deutsch sein, auch wenn das Gespräch englisch war.",
            concat!(
                "A: hast du das Video noch? -> B: ja klar, ich schick dir ",
                r#"morgen den Link -> {"summary": "A fragt nach dem Video vom "#,
                r#"letzten Abend. B hat es noch und will den Link schicken.", "#,
                r#""people": ["A", "B"], "open": ["B schickt A morgen den Link"]}"#,
            ),
        )
    } else {
        (
            "Write in ENGLISH. Every word of summary and open must be \
             English, even if the conversation was not.",
            concat!(
                "A: can I get the recording? -> B: sure, I will cut it and ",
                r#"send it over -> {"summary": "A asked for the recording of "#,
                r#"the meetup. B still has it and offered to cut it down "#,
                r#"first.", "people": ["A", "B"], "open": ["B cuts the "#,
                r#"recording and sends it to A"]}"#,
            ),
        )
    };
    format!(
        "You summarise ONE conversation from a chat lobby. It has already been \
         decided that this conversation is worth summarising; your job is only \
         to write it down. {order}\n\
         `summary` is two or three sentences about what was discussed, never a \
         list, never a judgement about the speakers. `people` lists the \
         speaker letters that took part. `open` lists everything somebody said \
         they would do and has not done yet, one entry each, in the words of \
         the dialogue; an empty list when there is nothing. Output ONLY \
         JSON.\nExample:\n{example}"
    )
}

/// Tokens the verdict may take. It is one boolean.
const VERDICT_TOKENS: i32 = 24;

/// Tokens one digest may take./// Tokens one digest may take. Three sentences plus two short lists.
const DIGEST_TOKENS: i32 = 320;

/// Longest summary kept. Past this the model has stopped writing a paragraph
/// and started writing minutes.
const MAX_SUMMARY: usize = 700;
/// Entries kept in `open`, and the length of one.
const MAX_OPEN: usize = 6;
const MAX_OPEN_LEN: usize = 160;

/// What the model made of one conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Digest {
    pub summary: String,
    /// Speaker letters, decoded to roster positions — the same alphabet
    /// [`crate::llm::transcript`] hands out, so `people[i]` indexes the roster.
    pub people: Vec<usize>,
    pub open: Vec<String>,
}

/// Is this conversation worth a paragraph at all?
///
/// Its own call, with a prompt that contains nothing but this question. See the
/// module note: everything else in the prompt costs refusals, and the refusal
/// is the whole feature.
pub fn worth_summarising(llm: &Llm, lines: &[Line]) -> Result<bool> {
    let roster = crate::llm::roster(lines);
    if roster.is_empty() {
        // Nobody could be named, so there is nobody for a summary to be about.
        return Ok(false);
    }
    let out = llm.ask(
        VERDICT_SYSTEM,
        &crate::llm::transcript(lines, &roster),
        VERDICT_GBNF,
        VERDICT_TOKENS,
    )?;
    let Some(value) = first_json(&out) else {
        warn!("the model produced nothing a grammar should have allowed");
        return Ok(false);
    };
    Ok(value.get("worth_summarising").and_then(Value::as_bool) == Some(true))
}

/// Ask the model about one conversation. `Ok(None)` is the refusal, and it is
/// the answer the bench cared about most: 6/6 on its trap set.
///
/// Two calls, the second only if the first said yes. No store handle is in
/// scope, which is what enforces the lock split.
pub fn judge(llm: &Llm, lines: &[Line], lang: &str) -> Result<Option<Digest>> {
    if !worth_summarising(llm, lines)? {
        return Ok(None);
    }
    let roster = crate::llm::roster(lines);
    let out = llm.ask(
        &summary_system(lang),
        &crate::llm::transcript(lines, &roster),
        SUMMARY_GBNF,
        DIGEST_TOKENS,
    )?;
    let Some(value) = first_json(&out) else {
        warn!("the model said a conversation was worth summarising and then wrote nothing");
        return Ok(None);
    };
    let summary = value
        .get("summary")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| truncate(s, MAX_SUMMARY));
    // A verdict of `true` with nothing behind it is not a digest. The grammar
    // forces the shape; this is the honesty check the grammar cannot do.
    let Some(summary) = summary else {
        warn!("the model said a conversation was worth summarising and then said nothing");
        return Ok(None);
    };
    let people = strings(value.get("people"))
        .iter()
        .filter_map(|s| letter_index(s))
        .filter(|i| *i < roster.len())
        .collect::<Vec<_>>();
    let open = strings(value.get("open"))
        .into_iter()
        .map(|s| truncate(&s, MAX_OPEN_LEN))
        .filter(|s| !s.is_empty())
        .take(MAX_OPEN)
        .collect();
    Ok(Some(Digest {
        summary,
        people,
        open,
    }))
}

/// The language a conversation's digest is written in.
///
/// The thread's own majority, by the same rule [`crate::langctx`] uses for
/// everything else — a conversation has a language whether or not anybody
/// declared one. A thread with no majority (a genuinely bilingual room, or one
/// too short to have decided) falls back to the reader's own first declared
/// language, and then to English: a digest is for the person reading it, and if
/// the room could not agree then the reader is the tie-break.
pub fn language_of(store: &Store, cfg: &crate::config::LangConfig, thread_id: i64) -> String {
    let stamps: Vec<Lang> = store
        .thread_language_stamps(thread_id, -1, cfg.context_window.max(1))
        .unwrap_or_default()
        .iter()
        .map(|s| match s.as_str() {
            "de" => Lang::De,
            "en" => Lang::En,
            _ => Lang::Unclear,
        })
        .collect();
    if let Some(ctx) = crate::langctx::context_of(&stamps, cfg)
        && let Some(tag) = ctx.lang.tag()
    {
        return tag.to_string();
    }
    store
        .your_languages()
        .unwrap_or_default()
        .into_iter()
        .next()
        .filter(|l| l == "de" || l == "en")
        .unwrap_or_else(|| "en".to_string())
}

/// The local calendar day an instant falls on, `YYYY-MM-DD`.
///
/// Local, not UTC: "yesterday" is a thing that happens in a timezone, and a
/// conversation at half past midnight belongs to the night it was part of as
/// far as the calendar is concerned — which is the day the clock said, and this
/// is what the clock said.
pub fn local_day(utc_ns: i64) -> String {
    let s = utc_ns.div_euclid(1_000_000_000) + local_offset_s(utc_ns);
    let (y, m, d) = civil_from_days(s.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

// ---------------------------------------------------------------------------
// the wire
// ---------------------------------------------------------------------------

/// One digest as `digest.list` returns it and as a `digest` event carries it —
/// from one function, so the two cannot drift.
pub fn digest_json(store: &Store, row: &DigestRow) -> Value {
    let summary = store.thread_summary(row.thread_id).ok().flatten();
    let participants: Vec<Value> = store
        .thread_participants(row.thread_id)
        .unwrap_or_default()
        .into_iter()
        .map(|id| {
            json!({
                "speaker_id": id,
                // The name as the voicebank spells it now, so a rename moves
                // every digest that quoted them without a re-derivation.
                "label": store.speaker_name(id).ok().flatten(),
            })
        })
        .collect();
    json!({
        "thread_id": row.thread_id,
        "day": row.day,
        "lang": row.lang,
        "summary": row.summary,
        "open": serde_json::from_str::<Value>(&row.open_json).unwrap_or_else(|_| json!([])),
        "participants": participants,
        // Both forms, as everywhere else.
        "started_ms": summary.as_ref().map(|s| ns_to_ms(s.started_ns)),
        "started_ns": summary.as_ref().map(|s| s.started_ns.to_string()),
        "ended_ms": summary.as_ref().map(|s| ns_to_ms(s.ended_ns)),
        "ended_ns": summary.as_ref().map(|s| s.ended_ns.to_string()),
        "turns": summary.as_ref().map(|s| s.segments),
        "model_id": row.model_id,
        "created_ms": ns_to_ms(row.created_ns),
    })
}

// ---------------------------------------------------------------------------
// the pass
// ---------------------------------------------------------------------------

/// One batch of conversations. `Ok(true)` means there was work.
///
/// Gather, judge, commit — the three phases, with the store lock held for the
/// first and third and dropped for the second.
pub fn batch(
    store: &Arc<std::sync::Mutex<Store>>,
    control: &Arc<Control>,
    bus: &Bus,
    llm: &Llm,
    cfg: &AssistConfig,
    stop: &dyn Fn() -> bool,
) -> Result<bool> {
    let now = crate::clock::utc_now_ns();
    let settled = now - cfg.digest_settle_min.max(0) * 60 * 1_000_000_000;
    let candidates = {
        let guard = store.lock().unwrap_or_else(|p| p.into_inner());
        guard.threads_for_digest(settled, cfg.digest_min_turns.max(2), cfg.batch.max(1))?
    };
    if candidates.is_empty() {
        return Ok(false);
    }

    for candidate in candidates {
        if stop() || crate::enrich::gate(control, &control.graph()).is_some() {
            break;
        }
        // ---- gather (lock held, no model) ----
        let (lines, lang) = {
            let guard = store.lock().unwrap_or_else(|p| p.into_inner());
            let lines: Vec<Line> = guard
                .thread_lines(candidate.thread_id)?
                .into_iter()
                .take(cfg.digest_max_turns.max(4))
                .map(|l| Line {
                    segment_id: l.segment_id,
                    speaker_id: l.speaker_id,
                    t_start_ns: l.t_start_ns,
                    text: l.text,
                })
                .collect();
            let lang = language_of(&guard, &control.lang, candidate.thread_id);
            (lines, lang)
        };
        if lines.is_empty() {
            continue;
        }
        let at = crate::clock::utc_now_ns();

        // ---- judge (no lock) ----
        let tuned = llm.with_threads(control.graph().llm_threads);
        let verdict = match judge(&tuned, &lines, &lang) {
            Ok(v) => v,
            Err(e) => {
                // The conversation is left unmarked, so a later pass retries
                // it. A model that timed out has not decided anything.
                warn!(thread = candidate.thread_id, "a digest failed: {e:#}");
                continue;
            }
        };

        // ---- commit (lock held, no model) ----
        let published = {
            let guard = store.lock().unwrap_or_else(|p| p.into_inner());
            match verdict {
                None => {
                    guard.mark_thread_not_worth_summarising(
                        candidate.thread_id,
                        tuned.model_id(),
                        at,
                    )?;
                    debug!(thread = candidate.thread_id, "not worth summarising");
                    None
                }
                Some(d) => {
                    let roster = crate::llm::roster(&lines);
                    let people: Vec<i64> = d
                        .people
                        .iter()
                        .filter_map(|i| roster.get(*i).copied())
                        .collect();
                    let row = DigestRow {
                        thread_id: candidate.thread_id,
                        day: local_day(candidate.started_ns),
                        lang: lang.clone(),
                        summary: d.summary,
                        people_json: serde_json::to_string(&people)?,
                        open_json: serde_json::to_string(&d.open)?,
                        model_id: tuned.model_id().to_string(),
                        created_ns: at,
                    };
                    guard.upsert_digest(&row)?;
                    Some(digest_json(&guard, &row))
                }
            }
        };
        if let Some(payload) = published {
            bus.publish(Topic::Segments, "digest", payload);
        }
    }
    Ok(true)
}

// ---------------------------------------------------------------------------

fn strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn letter_index(s: &str) -> Option<usize> {
    let c = s.trim().chars().next()?.to_ascii_uppercase();
    c.is_ascii_uppercase().then(|| (c as u8 - b'A') as usize)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.trim().to_string();
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
    use crate::config::GraphConfig;
    use crate::store::SegmentAnalysis;

    const SEC: i64 = 1_000_000_000;

    /// The prompts are measured artefacts, so the bench and the daemon must be
    /// running the same bytes — not "the same text, retyped".
    ///
    /// The Rust constants are the source of truth and the bench reads the files
    /// this writes. Run with `NXR_WRITE_PROMPTS=1` after editing a prompt to
    /// re-export them; without it this fails, which is the point — a prompt
    /// edited here and not re-exported means the next bench run measures
    /// something that is not shipped.
    #[test]
    fn the_bench_runs_the_prompts_this_daemon_ships() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../spike/digest_bench");
        let files = [
            ("verdict.system.txt", VERDICT_SYSTEM.to_string()),
            ("summary.de.txt", summary_system("de")),
            ("summary.en.txt", summary_system("en")),
        ];
        let writing = std::env::var("NXR_WRITE_PROMPTS").is_ok_and(|v| !v.is_empty());
        for (name, want) in files {
            let path = dir.join(name);
            if writing {
                std::fs::write(&path, &want).expect("exporting a prompt");
                continue;
            }
            let have = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            assert_eq!(
                have, want,
                "spike/digest_bench/{name} is not the prompt this daemon runs; \
                 re-export with NXR_WRITE_PROMPTS=1 cargo test the_bench_runs"
            );
        }
    }

    /// The load-bearing finding, asserted rather than remembered: the grammars
    /// and the prompts this daemon runs ARE the ones the bench measured.
    #[test]
    fn the_shipped_grammars_are_the_bench_files() {
        assert_eq!(
            VERDICT_GBNF,
            include_str!("../../../spike/digest_bench/verdict.gbnf"),
            "the verdict grammar drifted from the one the trap numbers came from"
        );
        assert_eq!(
            SUMMARY_GBNF,
            include_str!("../../../spike/digest_bench/summary.gbnf"),
            "the summary grammar drifted from the bench's"
        );
    }

    /// Verdict-first, taken to its end: the verdict's grammar has no
    /// extractable field ANYWHERE in it, so there is nothing in the schema for
    /// a model to be tempted by.
    #[test]
    fn the_verdict_grammar_admits_a_boolean_and_nothing_else() {
        let root = VERDICT_GBNF
            .lines()
            .find(|l| l.starts_with("root ::="))
            .expect("a root rule");
        assert!(root.contains("worth_summarising"), "{root}");
        for field in ["summary", "people", "open", "str"] {
            assert!(
                !VERDICT_GBNF.contains(field),
                "the verdict grammar can express {field:?}: {VERDICT_GBNF}"
            );
        }
        // Two rules: the object, and whitespace. Nothing else exists.
        assert_eq!(VERDICT_GBNF.matches("::=").count(), 2);
        // …and the summary grammar, which runs afterwards, has no verdict in
        // it at all: by then the question has been answered.
        assert!(!SUMMARY_GBNF.contains("worth_summarising"));
    }

    #[test]
    fn the_verdict_prompt_keeps_the_clauses_that_bought_a_trap() {
        for trap in [
            "greetings and goodbyes",
            "backchannel",
            "in-game callouts",
            "microphone checks",
            "subject is the conversation itself",
            // The last clause added, and the one that took the traps from 5/6
            // to 6/6. See the constant's own note.
            "Agreeing to play another round is not a plan",
        ] {
            assert!(VERDICT_SYSTEM.contains(trap), "the prompt dropped {trap:?}");
        }
        assert_eq!(
            VERDICT_SYSTEM.matches("worth_summarising").count(),
            4,
            "the four worked examples are what made the small model refuse"
        );
        // Nothing about summaries, languages or lists: every one of those cost
        // refusals when it was in here (see the module note's table).
        for leak in ["summary", "people", "open", "Deutsch", "language"] {
            assert!(
                !VERDICT_SYSTEM.contains(leak),
                "{leak:?} leaked into the verdict prompt"
            );
        }
    }

    #[test]
    fn the_summary_prompt_asks_in_the_language_it_wants_back() {
        let de = summary_system("de");
        assert!(de.contains("Schreibe AUF DEUTSCH"), "{de}");
        assert!(
            de.contains("A fragt nach dem Video"),
            "the example is German"
        );
        assert!(!de.contains("Write in ENGLISH"));
        let en = summary_system("en");
        assert!(en.contains("Write in ENGLISH"), "{en}");
        assert!(en.contains("A asked for the recording"));
        assert_eq!(summary_system("fr"), en, "English is the fallback");
        // It never re-litigates the verdict: that call has already happened.
        assert!(de.contains("already been decided"));
        assert!(!de.contains("worth_summarising"));
    }

    #[test]
    fn a_local_day_is_the_day_the_clock_said() {
        // The same instant `timeref`'s tests use, and the day a person would
        // call it.
        let at = (crate::clock::days_from_civil(2026, 9, 2) * 86_400 + 19 * 3600) * SEC;
        let utc = at - local_offset_s(at) * SEC;
        assert_eq!(local_day(utc), "2026-09-02");
    }

    // ---- the queue ---------------------------------------------------------

    struct Rig {
        store: Store,
        session: i64,
        a: i64,
        b: i64,
    }

    fn rig() -> Rig {
        let store = Store::open_in_memory().unwrap();
        let src = store.upsert_source("VRChat.exe", "VRChat", 0).unwrap();
        let session = store.begin_session(src, 0).unwrap();
        let a = store.create_speaker("Aspen", 0).unwrap();
        let b = store.create_speaker("Kira", 0).unwrap();
        store.rename_speaker(a, "Aspen", 0).unwrap();
        store.rename_speaker(b, "Kira", 0).unwrap();
        Rig {
            store,
            session,
            a,
            b,
        }
    }

    /// `n` turns, five seconds apart, alternating between two voices.
    fn a_conversation(r: &Rig, from_s: i64, n: usize, text: &str) -> i64 {
        let cfg = GraphConfig::default();
        let mut thread = 0;
        for i in 0..n {
            let t = (from_s + i as i64 * 5) * SEC;
            let id = r
                .store
                .insert_segment(r.session, t, t + 3 * SEC, "x.wav", t)
                .unwrap();
            r.store
                .set_segment_analysis(
                    id,
                    &SegmentAnalysis {
                        text: Some(format!("{text} {i}")),
                        lang: Some("de".into()),
                        lang_via: Some(crate::store::lang_via::CLASSIFIED.into()),
                        ..Default::default()
                    },
                )
                .unwrap();
            let who = if i % 2 == 0 { r.a } else { r.b };
            r.store
                .set_segment_speaker(id, Some(who), Some(0.9))
                .unwrap();
            thread = crate::threads::assign(&r.store, &cfg, id).unwrap().unwrap();
        }
        thread
    }

    #[test]
    fn the_queue_is_settled_conversations_long_enough_to_be_worth_reading() {
        let r = rig();
        // Eight turns, an hour ago: eligible.
        let old = a_conversation(&r, 0, 8, "das ist der einzige weg");
        // Eight turns, two minutes ago: not settled yet.
        let fresh = a_conversation(&r, 100_000, 8, "das ist der einzige weg");
        // Four turns, an hour ago: too short.
        let short = a_conversation(&r, 500, 4, "das ist der einzige weg");

        let now = 100_100 * SEC;
        let settled = now - 30 * 60 * SEC;
        let got: Vec<i64> = r
            .store
            .threads_for_digest(settled, 8, 50)
            .unwrap()
            .into_iter()
            .map(|c| c.thread_id)
            .collect();
        assert_eq!(got, vec![old], "only the settled, long-enough one");
        assert!(!got.contains(&fresh));
        assert!(!got.contains(&short));
    }

    #[test]
    fn a_conversation_is_read_once_whichever_way_it_went() {
        let r = rig();
        let thread = a_conversation(&r, 0, 8, "das ist der einzige weg");
        let now = 100_000 * SEC;
        let settled = now - 30 * 60 * SEC;
        assert_eq!(r.store.threads_for_digest(settled, 8, 50).unwrap().len(), 1);

        // The model refused. That is a result, and it has to be written down or
        // the worker asks about the same eight "ja"s for ever.
        r.store
            .mark_thread_not_worth_summarising(thread, "m@1", 1)
            .unwrap();
        assert!(
            r.store
                .threads_for_digest(settled, 8, 50)
                .unwrap()
                .is_empty(),
            "a refusal left the conversation on the queue"
        );
        // …and a refusal is invisible to a client.
        assert!(r.store.digest_rows(None, 50).unwrap().is_empty());
        assert_eq!(r.store.digest_counts(settled, 8).unwrap(), (0, 1, 0));

        // A real digest replaces it and is visible.
        r.store
            .upsert_digest(&DigestRow {
                thread_id: thread,
                day: "2026-09-02".into(),
                lang: "de".into(),
                summary: "Es ging um den Shader.".into(),
                people_json: format!("[{},{}]", r.a, r.b),
                open_json: "[\"B schickt den Link\"]".into(),
                model_id: "m@1".into(),
                created_ns: 2,
            })
            .unwrap();
        let rows = r.store.digest_rows(None, 50).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].summary, "Es ging um den Shader.");
        assert_eq!(r.store.digest_counts(settled, 8).unwrap(), (1, 0, 0));
        assert!(
            r.store
                .threads_for_digest(settled, 8, 50)
                .unwrap()
                .is_empty()
        );

        // The wire shape, including the participants a client draws chips from.
        let v = digest_json(&r.store, &rows[0]);
        assert_eq!(v["thread_id"], json!(thread));
        assert_eq!(v["day"], json!("2026-09-02"));
        assert_eq!(v["open"], json!(["B schickt den Link"]));
        let people = v["participants"].as_array().unwrap();
        assert_eq!(people.len(), 2);
        let names: Vec<&str> = people
            .iter()
            .map(|p| p["label"].as_str().unwrap())
            .collect();
        assert!(
            names.contains(&"Aspen") && names.contains(&"Kira"),
            "{names:?}"
        );
        assert!(v["started_ms"].is_number() && v["ended_ms"].is_number());
    }

    #[test]
    fn a_day_filter_selects_one_evening() {
        let r = rig();
        let t1 = a_conversation(&r, 0, 8, "eins");
        let t2 = a_conversation(&r, 100_000, 8, "zwei");
        for (thread, day) in [(t1, "2026-09-01"), (t2, "2026-09-02")] {
            r.store
                .upsert_digest(&DigestRow {
                    thread_id: thread,
                    day: day.into(),
                    lang: "de".into(),
                    summary: format!("about {day}"),
                    people_json: "[]".into(),
                    open_json: "[]".into(),
                    model_id: "m@1".into(),
                    created_ns: 1,
                })
                .unwrap();
        }
        let one = r.store.digest_rows(Some("2026-09-01"), 50).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].thread_id, t1);
        assert_eq!(r.store.digest_rows(None, 50).unwrap().len(), 2);
        assert!(
            r.store
                .digest_rows(Some("1999-01-01"), 50)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_conversation_takes_its_own_language_and_falls_back_to_the_reader() {
        let r = rig();
        let cfg = crate::config::LangConfig::default();
        let german = a_conversation(&r, 0, 8, "das ist der einzige weg");
        assert_eq!(language_of(&r.store, &cfg, german), "de");

        // A thread with no stamps at all has no language. With no declaration
        // on the reader's own voice either, English is the fallback.
        let bare = r.store.create_thread(r.session, 0, 1).unwrap();
        assert_eq!(language_of(&r.store, &cfg, bare), "en");
        // …and with one, the reader's.
        let you = r.store.ensure_you_speaker(0).unwrap();
        r.store
            .set_speaker_languages(you, Some(&["de".to_string()]))
            .unwrap();
        assert_eq!(language_of(&r.store, &cfg, bare), "de");
    }

    // ---- against the real model --------------------------------------------

    fn staged() -> Option<Llm> {
        let raw = std::env::var("NXR_GRAPH_MODELS").ok()?;
        if raw.trim().is_empty() {
            return None;
        }
        Llm::resolve(
            std::path::Path::new(&raw),
            &GraphConfig::default(),
            &crate::config::RuntimeConfig::default(),
        )
    }

    fn line(i: i64, who: i64, text: &str) -> Line {
        Line {
            segment_id: i,
            speaker_id: Some(who),
            t_start_ns: i * SEC,
            text: text.into(),
        }
    }

    /// One trap and one positive, through the daemon's own call path rather
    /// than the bench's. The numbers live in `spike/digest_bench`; what this
    /// proves is that the shipped invocation reproduces them.
    #[test]
    fn the_real_model_refuses_a_trap_and_summarises_a_conversation() {
        let Some(llm) = staged() else {
            eprintln!("skipping the digest round trip: set NXR_GRAPH_MODELS=<models dir>");
            return;
        };
        let trap: Vec<Line> = ["ja", "ne", "lol", "ja ok", "hm", "lol ja", "ne echt", "ja"]
            .iter()
            .enumerate()
            .map(|(i, t)| line(i as i64, if i % 2 == 0 { 7 } else { 9 }, t))
            .collect();
        assert_eq!(
            judge(&llm, &trap, "de").expect("the runner ran"),
            None,
            "the model wrote a paragraph about eight turns of nothing"
        );

        let real = vec![
            line(0, 7, "hast du den Shader von dem Avatar noch?"),
            line(1, 9, "ja klar, der ist von Aspen, ich hab die Datei hier"),
            line(2, 7, "kannst du mir den schicken?"),
            line(3, 9, "ich schick dir morgen den Link"),
            line(4, 7, "passt, kein Stress"),
            line(5, 9, "der frisst aber Performance"),
            line(6, 7, "ich bau eh nur für PC"),
            line(7, 9, "dann geht das klar"),
        ];
        let d = judge(&llm, &real, "de")
            .expect("the runner ran")
            .expect("a conversation with a subject");
        eprintln!("digest: {d:?}");
        assert!(!d.summary.trim().is_empty());
        assert!(d.summary.chars().count() <= MAX_SUMMARY);
        assert!(d.people.iter().all(|i| *i < 2), "{:?}", d.people);
        // Written in the conversation's language, which is the thing that took
        // two attempts to get right.
        let de = ["der", "die", "das", "und", "ist", "nicht", "schick", "ein"]
            .iter()
            .filter(|w| d.summary.to_lowercase().contains(*w))
            .count();
        assert!(de >= 2, "the summary is not German: {:?}", d.summary);
    }
}
