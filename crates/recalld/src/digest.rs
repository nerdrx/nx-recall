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
//!
//! ### Whose name is in the paragraph (0.11.6)
//!
//! The model is handed speaker LETTERS ([`crate::llm::transcript`]) and it
//! writes them back: *"A und B reden über etwas, das sie miteinander teilen
//! möchten"*. That is a paragraph about a seating chart. A digest is the one
//! output the daemon volunteers, so it has to read like the user's own
//! sentence about their own evening — **Kira und Speaker 38**, "You" for the
//! voice on the microphone.
//!
//! Two designs, and `spike/digest_bench` picked between them on the same ten
//! cases and the same model. The verdict call is identical in both — same
//! prompt, same letters, same grammar — so the six traps cannot regress:
//!
//! | | traps | everyone named | invented names |
//! |---|---:|---:|---:|
//! | (a) letters in the prompt, substituted afterwards | 6/6 | 3/4 | 0 |
//! | (b) the labels themselves in the summary prompt | 6/6 | 4/4 | 0 |
//!
//! **(b) ships.** (a)'s one loss is the reason the choice was worth measuring
//! rather than assuming: an English summary opens *"A asked for the
//! recording"*, and the rule that keeps *"A meetup at eight"* from becoming
//! *"Kira meetup at eight"* — never a sentence-initial `A` before a lowercase
//! word — cannot tell those two apart. In German there is no article to
//! collide with and (a) is clean; English is half the digests.
//!
//! (a) is not thrown away. It is [`render_letters`], and it is what renders
//! the digests already in the store: those rows were written with letters,
//! they have no [`crate::store::DigestRow::summary_raw`], and re-asking the
//! model about somebody's evening from three weeks ago to fix a pronoun is not
//! a trade worth making. They are rendered at **read time** from the roster
//! the conversation still has, and marked `rendered: "legacy"` so a client can
//! see which paragraph came from where.
//!
//! ### The row keeps both
//!
//! `summary` is the prose the user reads; `summary_raw` is what the model
//! wrote, so nothing is lost and a re-render never has to guess. `roster_json`
//! is who the letters were — **ids and the labels of the day** — so a rename
//! moves the paragraph the same way it already moves the participant chips.
//! Rendering therefore happens on the way OUT ([`digest_json`]), from the raw
//! and the roster, every time; the stored `summary` is what it rendered to on
//! the night, kept so that anything reading the database directly sees what
//! the user saw.

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
///
/// 0.11.6: the worked example is written with **names** rather than letters,
/// and the speakers in the input carry names too ([`named_transcript`]). The
/// naming clause sits in this prompt and not in the verdict's, which is why
/// the traps could not move — see the module note's table.
pub fn summary_system(tag: &str) -> String {
    // Raw strings, because both halves are JSON with quotes all through them
    // and a prompt that is a measured artefact must be readable as itself.
    //
    // "Nadia" and "Timo" are in no roster this daemon can produce and in no
    // case of the bench, on purpose: a name copied out of this example into a
    // real digest is then a countable event rather than a coincidence.
    let (order, example) = if tag == "de" {
        (
            "Schreibe AUF DEUTSCH. Jedes Wort von summary und open muss \
             deutsch sein, auch wenn das Gespräch englisch war.",
            concat!(
                "Nadia: hast du das Video noch? -> Timo: ja klar, ich schick ",
                r#"dir morgen den Link -> {"summary": "Nadia fragt nach dem "#,
                r#"Video vom letzten Abend. Timo hat es noch und will den Link "#,
                r#"schicken.", "people": ["Nadia", "Timo"], "open": ["Timo "#,
                r#"schickt Nadia morgen den Link"]}"#,
            ),
        )
    } else {
        (
            "Write in ENGLISH. Every word of summary and open must be \
             English, even if the conversation was not.",
            concat!(
                "Nadia: can I get the recording? -> Timo: sure, I will cut it ",
                r#"and send it over -> {"summary": "Nadia asked for the "#,
                r#"recording of the meetup. Timo still has it and offered to "#,
                r#"cut it down first.", "people": ["Nadia", "Timo"], "open": "#,
                r#"["Timo cuts the recording and sends it to Nadia"]}"#,
            ),
        )
    };
    format!(
        "You summarise ONE conversation from a chat lobby. It has already been \
         decided that this conversation is worth summarising; your job is only \
         to write it down. {order}\n\
         `summary` is two or three sentences about what was discussed, never a \
         list, never a judgement about the speakers. Call every speaker by the \
         exact name that stands in front of their lines, and never by any \
         other name. `people` lists the names of the speakers that took part, \
         spelled exactly as the dialogue spells them. `open` lists everything \
         somebody said they would do and has not done yet, one entry each, in \
         the words of the dialogue; an empty list when there is nothing. \
         Output ONLY JSON.\nExample:\n{example}"
    )
}

/// The summary call's input: `Kira: …` / `Speaker 38: …`, the same shape
/// [`crate::llm::transcript`] produces but with the roster's own labels in
/// front of the lines instead of letters.
///
/// Its own function rather than a flag on `transcript`, because the verdict
/// call still gets letters and must keep getting exactly the bytes it was
/// measured on. Turns nobody could place are dropped for the same reason as
/// there: an anonymous line is not a party to anything.
pub fn named_transcript(window: &[Line], roster: &[i64], labels: &[String]) -> String {
    window
        .iter()
        .filter_map(|line| {
            let id = line.speaker_id?;
            let i = roster.iter().position(|s| *s == id)?;
            Some(format!("{}: {}", labels.get(i)?, line.text.trim()))
        })
        .collect::<Vec<_>>()
        .join("\n")
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
    /// What the model wrote, with the roster's own labels in it. The rendering
    /// on the wire is a re-render of this against today's names.
    pub summary: String,
    /// Roster positions, decoded from the names the model listed by exact
    /// match — so `people[i]` indexes the roster, as it always did.
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
pub fn judge(llm: &Llm, lines: &[Line], lang: &str, labels: &[String]) -> Result<Option<Digest>> {
    if !worth_summarising(llm, lines)? {
        return Ok(None);
    }
    let roster = crate::llm::roster(lines);
    // The verdict above ran on letters, byte for byte what it was measured on.
    // The summary runs on names — and only the summary, which is why the traps
    // are the same six refusals they were.
    let out = llm.ask(
        &summary_system(lang),
        &named_transcript(lines, &roster, labels),
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
    // Decoded by **exact label match**, and anything else is dropped. A name
    // the model made up is not a person in this room, and the one thing worse
    // than a digest that says "A" is a digest that credits the wrong voice.
    let people = strings(value.get("people"))
        .iter()
        .filter_map(|s| labels.iter().position(|l| l == s.trim()))
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

// ---------------------------------------------------------------------------
// names
// ---------------------------------------------------------------------------

/// What this daemon calls one voice, in the words the user sees everywhere
/// else: the name they gave it, else the auto label, else — for a voice whose
/// row has gone — the id.
///
/// The auto label is stored as `Speaker_38` because it is an identifier; in a
/// sentence it is a name, and a name does not have an underscore in it. Only
/// the generated shape is rewritten, so a person who calls themselves
/// `moon_child` keeps their underscore.
pub fn label(store: &Store, speaker_id: i64) -> String {
    store
        .speaker_name(speaker_id)
        .ok()
        .flatten()
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .map(|n| match n.strip_prefix("Speaker_") {
            Some(rest) if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) => {
                format!("Speaker {rest}")
            }
            _ => n,
        })
        .unwrap_or_else(|| format!("Speaker {speaker_id}"))
}

/// One entry of `roster_json`: the voice a letter stood for, and what that
/// voice was called on the night. The id is what a re-render follows.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RosterEntry {
    pub id: i64,
    pub label: String,
}

/// The roster of one conversation as it is now, in letter order — `A` first.
///
/// Recomputed from the turns rather than remembered, which is what makes a row
/// written before 0.11.6 renderable at all: [`crate::llm::roster`] is
/// first-appearance order over the same query, so it hands out the same letters
/// it handed out then. A window that was truncated at `digest_max_turns` can
/// gain a trailing letter here that the model was never given, which is
/// harmless — that letter is in no summary.
pub fn thread_roster(store: &Store, thread_id: i64) -> Vec<RosterEntry> {
    let lines: Vec<Line> = store
        .thread_lines(thread_id)
        .unwrap_or_default()
        .into_iter()
        .map(|l| Line {
            segment_id: l.segment_id,
            speaker_id: l.speaker_id,
            t_start_ns: l.t_start_ns,
            text: l.text,
        })
        .collect();
    crate::llm::roster(&lines)
        .into_iter()
        .map(|id| RosterEntry {
            id,
            label: label(store, id),
        })
        .collect()
}

/// Design (a): a speaker letter in the model's prose becomes a person's name.
///
/// This is what renders the digests written before 0.11.6, and the whole of
/// its design is the three conditions it refuses on:
///
/// * **only letters that were assigned.** `roster.len()` is the alphabet; `C`
///   in a two-voice conversation is a letter the model invented and is left
///   exactly where it is.
/// * **only standalone tokens.** A letter with a word character on either side
///   is part of a word — `AB`, `A4`, `Grad_A` — not a person.
/// * **never an English article.** `A` is a word in English, and *"A meetup at
///   eight"* must not become *"Kira meetup at eight"*. Sentence-initial `A`
///   followed by a lowercase word is an article and is left alone. This is
///   conservative on purpose and it costs: *"A asked for the recording"* is
///   the same shape and is also left alone, which is exactly why design (a)
///   lost the bench and is not what writes new digests. German has no such
///   collision, so German text is rendered whole.
pub fn render_letters(text: &str, lang: &str, roster: &[RosterEntry]) -> String {
    if roster.is_empty() {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let wordish = |c: Option<&char>| c.is_some_and(|c| c.is_alphanumeric() || *c == '_');
    let mut out = String::with_capacity(text.len());
    for (i, c) in chars.iter().enumerate() {
        let idx = (*c as u32).checked_sub('A' as u32).map(|d| d as usize);
        let entry = idx
            .filter(|_| c.is_ascii_uppercase())
            .and_then(|d| roster.get(d));
        let standalone =
            !wordish(i.checked_sub(1).and_then(|p| chars.get(p))) && !wordish(chars.get(i + 1));
        match entry {
            Some(e) if standalone && !(lang == "en" && *c == 'A' && english_article(&chars, i)) => {
                out.push_str(&e.label)
            }
            _ => out.push(*c),
        }
    }
    out
}

/// Is the `A` at `i` an English article — sentence-initial, and followed by a
/// lowercase word?
fn english_article(chars: &[char], i: usize) -> bool {
    let mut back = i;
    while back > 0 && matches!(chars[back - 1], ' ' | '\t') {
        back -= 1;
    }
    let initial = back == 0 || matches!(chars[back - 1], '.' | '!' | '?' | '\n');
    let mut fwd = i + 1;
    while fwd < chars.len() && matches!(chars[fwd], ' ' | '\t' | '\n') {
        fwd += 1;
    }
    let lower = chars
        .get(fwd)
        .is_some_and(|c| c.is_alphabetic() && c.is_lowercase());
    initial && lower
}

/// Design (b): the labels the model was given become the labels the voices
/// have now, so a rename moves a paragraph that was written weeks ago.
///
/// Exact standalone-token match against the roster's stored labels, longest
/// first so `Speaker 3` cannot eat the front of `Speaker 38`. A label that has
/// not changed is replaced with itself, which is why this is unconditional.
///
/// Two voices the user has given the *same* name are indistinguishable here
/// and the first one in the roster wins. That is the honest outcome: the
/// paragraph was written about two people called the same thing, and nothing
/// in the text says which sentence belongs to which.
pub fn render_labels(text: &str, roster: &[RosterEntry], now: &[String]) -> String {
    let mut pairs: Vec<(&str, &str)> = roster
        .iter()
        .zip(now)
        .filter(|(e, _)| !e.label.trim().is_empty())
        .map(|(e, n)| (e.label.as_str(), n.as_str()))
        .collect();
    pairs.sort_by_key(|(old, _)| std::cmp::Reverse(old.chars().count()));
    let chars: Vec<char> = text.chars().collect();
    let wordish = |c: Option<&char>| c.is_some_and(|c| c.is_alphanumeric() || *c == '_');
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    'outer: while i < chars.len() {
        if !wordish(i.checked_sub(1).and_then(|p| chars.get(p))) {
            for (old, new) in &pairs {
                let want: Vec<char> = old.chars().collect();
                if chars[i..].starts_with(&want[..]) && !wordish(chars.get(i + want.len())) {
                    out.push_str(new);
                    i += want.len();
                    continue 'outer;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
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
    // 0.11.6: the paragraph is rendered on the way OUT, from what the model
    // wrote and from who the voices are *now* — so a rename moves the prose
    // the same way it already moves the participant chips below it.
    //
    // A row from before 0.11.6 has neither a raw nor a roster: its stored
    // summary IS the raw, still full of letters, and the roster is recomputed
    // from the conversation's own turns (`thread_roster`, which hands out the
    // same letters it handed out then). No model call, ever, for an old row.
    let legacy = row.summary_raw.is_none();
    let written: Vec<RosterEntry> = row
        .roster_json
        .as_deref()
        .and_then(|s| serde_json::from_str::<Vec<RosterEntry>>(s).ok())
        .unwrap_or_else(|| thread_roster(store, row.thread_id));
    let now: Vec<String> = written.iter().map(|e| label(store, e.id)).collect();
    let mode = if legacy {
        "legacy"
    } else {
        row.rendered.as_deref().unwrap_or("names")
    };
    let current: Vec<RosterEntry> = written
        .iter()
        .zip(&now)
        .map(|(e, n)| RosterEntry {
            id: e.id,
            label: n.clone(),
        })
        .collect();
    let render = |text: &str| match mode {
        "names" => render_labels(text, &written, &now),
        // "letters" and "legacy" are the same substitution; they differ only
        // in whether the raw was kept on the row or is the summary itself.
        _ => render_letters(text, &row.lang, &current),
    };
    let raw_summary = row
        .summary_raw
        .clone()
        .unwrap_or_else(|| row.summary.clone());
    let raw_open: Vec<String> = serde_json::from_str(&row.open_json).unwrap_or_else(|_| Vec::new());
    // 0.10.0: who did the talking. Attached to the participant rather than
    // offered as a second list, because a share is a property OF a person in
    // a conversation and a client that has to join two arrays to draw one bar
    // will eventually join them wrong.
    let shares: std::collections::HashMap<i64, crate::turntaking::Share> =
        crate::turntaking::thread_turns(store, row.thread_id)
            .map(|turns| {
                crate::turntaking::shares(&turns)
                    .into_iter()
                    .map(|s| (s.speaker_id, s))
                    .collect()
            })
            .unwrap_or_default();
    let participants: Vec<Value> = store
        .thread_participants(row.thread_id)
        .unwrap_or_default()
        .into_iter()
        .map(|id| {
            let style = store
                .speaker_style(id)
                .ok()
                .flatten()
                .unwrap_or((None, None));
            json!({
                "speaker_id": id,
                // The name as the voicebank spells it now, so a rename moves
                // every digest that quoted them without a re-derivation —
                // and through the same [`label`] the paragraph above the
                // chips went through, because a chip that says `Speaker_07`
                // beside a sentence about "Speaker 07" reads as two people.
                "label": label(store, id),
                // The highlight (v15). The chips under a digest are the same
                // people the paragraph names, and a person who is picked out
                // everywhere else in the app must be picked out here too —
                // otherwise the highlight is a per-page decoration rather than
                // a property of the person.
                "colour": style.0,
                "icon": style.1,
                "share": shares.get(&id).map(|s| s.share),
                "turns": shares.get(&id).map(|s| s.turns),
            })
        })
        .collect();
    json!({
        "thread_id": row.thread_id,
        "day": row.day,
        "lang": row.lang,
        // The prose, with people's names in it. `*_raw` is what the model
        // wrote — kept on the wire as well as on the row, so a client that
        // wants to show the letters can and nothing is lost.
        "summary": render(&raw_summary),
        "summary_raw": raw_summary,
        "open": raw_open.iter().map(|o| render(o)).collect::<Vec<_>>(),
        "open_raw": raw_open,
        "rendered": mode,
        "participants": participants,
        // Both forms, as everywhere else.
        "started_ms": summary.as_ref().map(|s| ns_to_ms(s.started_ns)),
        "started_ns": summary.as_ref().map(|s| s.started_ns.to_string()),
        "ended_ms": summary.as_ref().map(|s| ns_to_ms(s.ended_ns)),
        "ended_ns": summary.as_ref().map(|s| s.ended_ns.to_string()),
        "turns": summary.as_ref().map(|s| s.segments),
        // 0.10.0: where it happened, for the same reason `thread.get` carries
        // it — a paragraph about an evening reads differently once you know
        // which room it was in.
        "world": store.thread_world(row.thread_id).ok().flatten(),
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
        let (lines, lang, written) = {
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
            // 0.11.6: what these voices are called, read under the same lock
            // as the turns. The model is given these names and writes them
            // back, so they are gathered here rather than after the call.
            let written: Vec<RosterEntry> = crate::llm::roster(&lines)
                .into_iter()
                .map(|id| RosterEntry {
                    id,
                    label: label(&guard, id),
                })
                .collect();
            (lines, lang, written)
        };
        if lines.is_empty() {
            continue;
        }
        let labels: Vec<String> = written.iter().map(|e| e.label.clone()).collect();
        let at = crate::clock::utc_now_ns();

        // ---- judge (no lock) ----
        let tuned = llm.with_threads(control.graph().llm_threads);
        let verdict = match judge(&tuned, &lines, &lang, &labels) {
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
                        // What the user would have read tonight. The wire
                        // renders again from the raw every time it is asked,
                        // so this is a record and not the source of truth.
                        summary: render_labels(&d.summary, &written, &labels),
                        people_json: serde_json::to_string(&people)?,
                        open_json: serde_json::to_string(&d.open)?,
                        summary_raw: Some(d.summary),
                        roster_json: Some(serde_json::to_string(&written)?),
                        rendered: Some("names".into()),
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
            de.contains("Nadia fragt nach dem Video"),
            "the example is German"
        );
        assert!(!de.contains("Write in ENGLISH"));
        let en = summary_system("en");
        assert!(en.contains("Write in ENGLISH"), "{en}");
        assert!(en.contains("Nadia asked for the recording"));
        assert_eq!(summary_system("fr"), en, "English is the fallback");
        // It never re-litigates the verdict: that call has already happened.
        assert!(de.contains("already been decided"));
        assert!(!de.contains("worth_summarising"));
    }

    /// 0.11.6: the summary prompt asks for names and shows names, and the
    /// example's names are ones no roster can produce — so a name copied out
    /// of the prompt is countable rather than deniable.
    #[test]
    fn the_summary_prompt_asks_for_names_and_the_verdict_never_hears_about_them() {
        for tag in ["de", "en"] {
            let s = summary_system(tag);
            assert!(
                s.contains(
                    "Call every speaker by the exact name that stands in front of their lines"
                ),
                "{s}"
            );
            assert!(s.contains("Nadia") && s.contains("Timo"), "{s}");
            // The letters are gone from the example: the input has names in
            // it now, and an example in a different shape from the input is
            // what §19 measured a loss on.
            for letters in ["A:", "B:", "\"A\"", "\"B\""] {
                assert!(!s.contains(letters), "{letters:?} is still in {tag}: {s}");
            }
        }
        // …and none of it reached the verdict, which is why the six traps are
        // the same six refusals. See the module note's table.
        for leak in ["Nadia", "Timo", "name"] {
            assert!(
                !VERDICT_SYSTEM.contains(leak),
                "{leak:?} leaked into the verdict prompt"
            );
        }
    }

    /// The input the summary call actually gets: the roster's own labels in
    /// front of the lines, and a turn nobody could place dropped rather than
    /// given a name.
    #[test]
    fn the_summary_reads_names_where_the_verdict_reads_letters() {
        let lines = vec![
            line(0, 7, "hast du den Shader noch?"),
            line(1, 9, "ja, ich schick dir den Link"),
            Line {
                segment_id: 2,
                speaker_id: None,
                t_start_ns: 2 * SEC,
                text: "irgendwer im Hintergrund".into(),
            },
        ];
        let roster = crate::llm::roster(&lines);
        let labels = ["Kira".to_string(), "Speaker 38".to_string()];
        let named = named_transcript(&lines, &roster, &labels);
        assert_eq!(
            named,
            "Kira: hast du den Shader noch?\nSpeaker 38: ja, ich schick dir den Link"
        );
        // The verdict's input is untouched, byte for byte what it was measured
        // on — which is the whole reason the traps could not move.
        assert!(crate::llm::transcript(&lines, &roster).starts_with("A: hast du"));
    }

    // ---- rendering ---------------------------------------------------------

    fn roster_of(labels: &[&str]) -> Vec<RosterEntry> {
        labels
            .iter()
            .enumerate()
            .map(|(i, l)| RosterEntry {
                id: i as i64 + 1,
                label: (*l).to_string(),
            })
            .collect()
    }

    /// Design (a)'s rule, which is what renders every digest written before
    /// 0.11.6. Each case here is one of the three conditions it refuses on.
    #[test]
    fn a_letter_becomes_a_name_only_where_it_is_certainly_a_speaker() {
        let two = roster_of(&["Kira", "Speaker 38"]);
        // The ordinary case, in German, where there is no article to collide
        // with and the substitution is total.
        assert_eq!(
            render_letters(
                "A und B reden über den Shader. B schickt A den Link.",
                "de",
                &two
            ),
            "Kira und Speaker 38 reden über den Shader. Speaker 38 schickt Kira den Link."
        );
        // Only letters that were assigned: C is a letter the model invented
        // in a two-voice conversation and is left exactly where it is.
        assert_eq!(render_letters("A und C", "de", &two), "Kira und C");
        // Only standalone tokens.
        assert_eq!(
            render_letters("AB, A4 und Grad_A bleiben", "de", &two),
            "AB, A4 und Grad_A bleiben"
        );
        // Never an English article — and the cost of that, in the same
        // string: the meetup survives, and so does the "A asked" the bench
        // counted as design (a)'s one loss.
        assert_eq!(
            render_letters("A meetup at eight. A asked B for it.", "en", &two),
            "A meetup at eight. A asked Speaker 38 for it."
        );
        // The same sentence in German renders whole: there is no German
        // article that is a bare "A".
        assert_eq!(
            render_letters("A fragt B danach.", "de", &two),
            "Kira fragt Speaker 38 danach."
        );
        // Mid-sentence in English is not an article shape, so it renders.
        assert_eq!(
            render_letters("The link B sent A works.", "en", &two),
            "The link Speaker 38 sent Kira works."
        );
        // Nobody in the room, nothing to do.
        assert_eq!(render_letters("A und B", "de", &[]), "A und B");
    }

    /// Design (b)'s: the names the model was given become the names the voices
    /// have now, so a rename moves a paragraph written weeks ago.
    #[test]
    fn a_rename_moves_the_paragraph_it_was_written_into() {
        let written = roster_of(&["Speaker 3", "Speaker 38"]);
        let now = ["Kira".to_string(), "Speaker 38".to_string()];
        // Longest first, so "Speaker 3" cannot eat the front of "Speaker 38".
        assert_eq!(
            render_labels(
                "Speaker 38 schickt Speaker 3 den Link, Speaker 3 baut es nach.",
                &written,
                &now
            ),
            "Speaker 38 schickt Kira den Link, Kira baut es nach."
        );
        // A name inside a word is not that person.
        assert_eq!(
            render_labels("Speaker 3000 ist niemand", &written, &now),
            "Speaker 3000 ist niemand"
        );
    }

    /// A name is what the user sees everywhere else, and a generated label is
    /// an identifier until it has to stand in a sentence.
    #[test]
    fn a_voice_is_called_what_the_user_calls_it() {
        let store = Store::open_in_memory().unwrap();
        let named = store.create_speaker("Aspen", 0).unwrap();
        store.rename_speaker(named, "Aspen", 0).unwrap();
        assert_eq!(label(&store, named), "Aspen");
        // The auto label loses its underscore, because a sentence is not a
        // symbol table.
        let auto = store.create_speaker("Speaker_38", 0).unwrap();
        assert_eq!(label(&store, auto), "Speaker 38");
        // …but only the generated shape does.
        let odd = store.create_speaker("moon_child", 0).unwrap();
        assert_eq!(label(&store, odd), "moon_child");
        // The microphone's voice is "You" already, and stays it.
        let you = store.ensure_you_speaker(0).unwrap();
        assert_eq!(label(&store, you), "You");
    }

    /// Requirement 4, and the reason there is no `digest rerender`: a row
    /// written before 0.11.6 has no raw to re-render FROM. It is rendered on
    /// the way out from the roster the conversation still has, marked
    /// `legacy`, and no model is asked anything about it.
    #[test]
    fn a_digest_written_before_this_change_is_rendered_at_read_time() {
        let r = rig();
        let thread = a_conversation(&r, 0, 8, "das ist der einzige weg");
        r.store
            .upsert_digest(&DigestRow {
                thread_id: thread,
                day: "2026-09-02".into(),
                lang: "de".into(),
                summary: "A und B reden über den Shader. B schickt A den Link.".into(),
                people_json: format!("[{},{}]", r.a, r.b),
                open_json: "[\"B schickt A morgen den Link\"]".into(),
                // What 0.11.5 wrote: none of the three.
                summary_raw: None,
                roster_json: None,
                rendered: None,
                model_id: "m@1".into(),
                created_ns: 2,
            })
            .unwrap();
        let rows = r.store.digest_rows(None, 50).unwrap();
        let v = digest_json(&r.store, &rows[0]);
        assert_eq!(v["rendered"], json!("legacy"));
        assert_eq!(
            v["summary"],
            json!("Aspen und Kira reden über den Shader. Kira schickt Aspen den Link.")
        );
        assert_eq!(v["open"], json!(["Kira schickt Aspen morgen den Link"]));
        // Nothing is lost: the letters are still on the wire.
        assert_eq!(
            v["summary_raw"],
            json!("A und B reden über den Shader. B schickt A den Link.")
        );
        assert_eq!(v["open_raw"], json!(["B schickt A morgen den Link"]));
        // …and a rename moves it, because the roster is recomputed from the
        // conversation every time it is read.
        r.store.rename_speaker(r.b, "Wren", 1).unwrap();
        let v = digest_json(&r.store, &r.store.digest_rows(None, 50).unwrap()[0]);
        assert_eq!(
            v["summary"],
            json!("Aspen und Wren reden über den Shader. Wren schickt Aspen den Link.")
        );
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
                open_json: "[\"Kira schickt den Link\"]".into(),
                summary_raw: Some("Es ging um den Shader.".into()),
                roster_json: Some(format!(
                    "[{{\"id\":{},\"label\":\"Aspen\"}},{{\"id\":{},\"label\":\"Kira\"}}]",
                    r.a, r.b
                )),
                rendered: Some("names".into()),
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
        assert_eq!(v["open"], json!(["Kira schickt den Link"]));
        assert_eq!(v["rendered"], json!("names"));
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
                    summary_raw: None,
                    roster_json: None,
                    rendered: None,
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
        let labels = ["Kira".to_string(), "Speaker 38".to_string()];
        assert_eq!(
            judge(&llm, &trap, "de", &labels).expect("the runner ran"),
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
        let d = judge(&llm, &real, "de", &labels)
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
        // 0.11.6, and the whole point of it: the paragraph says who, by the
        // name this daemon would put on a chip. No letters left standing.
        assert!(
            labels.iter().any(|l| d.summary.contains(l.as_str())),
            "nobody is named in {:?}",
            d.summary
        );
        for stray in [" A ", " B ", "A ist", "B ist"] {
            assert!(
                !d.summary.contains(stray),
                "a letter survived into {:?}",
                d.summary
            );
        }
    }
}
