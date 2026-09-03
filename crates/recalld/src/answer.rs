//! One sentence, built only out of transcript rows it names (PROTOCOL 0.11.0,
//! `search.answer`).
//!
//! Search hands back twenty rows and leaves the reading to you. A question —
//! *"wie viel kostet Aspens Shader?"* — has an answer, and the answer is eleven
//! words long. Writing those eleven words is the one thing in this project
//! where a language model is obviously the right tool and just as obviously the
//! wrong one: it will write eleven words whether or not the archive contains
//! them.
//!
//! So this module is almost entirely about **not** answering.
//!
//! ## Two calls, verdict first
//!
//! The shape is [`crate::digest`]'s, and for the reason measured there: every
//! clause added to a prompt that both decides and writes makes the deciding
//! worse (6/6 traps → 5/6 → 1/6 as the writing instructions grew). So:
//!
//! 1. [`ANSWERABLE_SYSTEM`] + [`ANSWERABLE_GBNF`] — one boolean, over the
//!    question and the rows. Nothing in the grammar can hold an answer, so
//!    there is nothing in the schema to tempt the model with. The prompt's one
//!    job is the distinction the whole feature turns on: **the rows must state
//!    the answer, not merely mention what it is about.**
//! 2. Only if that said yes: [`answer_system`] + [`answer_grammar`] — the
//!    sentence, and the ids it came from. The grammar's `id` rule is built out
//!    of the ids that were actually shown, so a citation of a row that was
//!    never on the page is not a thing the decoder can emit, and `cites`
//!    requires at least one.
//!
//! ## And then Rust checks it anyway
//!
//! A grammar can force the shape of an answer, never its honesty — the lesson
//! [`crate::llm`]'s `due` field bought. Two post-checks, both hard, both
//! producing a refusal rather than a repaired answer:
//!
//! * every citation is an id that was shown (the grammar already says so; this
//!   is the second net, and it is free);
//! * the sentence shares at least [`MIN_OVERLAP`] content words with the rows
//!   it cited. A sentence that cites row 109 and has no word in common with row
//!   109 was not read off row 109, whatever the model believes. For a language
//!   that does not write spaces the unit is a character bigram and the floor is
//!   [`MIN_OVERLAP_CJK`] — see there for why the word split was no check at all.
//!
//! A failed post-check is `refused`, and the hits are still returned: the
//! honest fallback for "I cannot answer this" is the search results, which is
//! what the user would have got anyway.
//!
//! ## Nothing is written down
//!
//! `search.answer` is a read. No row, no cache, no "remembered" answer — an
//! answer is a function of the archive at the moment it was asked, and an
//! archive that stores what a model said about it is an archive that will
//! eventually be searched for the model's words.
//!
//! ## The gate
//!
//! `spike/answer_bench` — 24 cases over a seeded 200-turn de/en transcript, 12
//! answerable and 12 traps, plus six over a 44-turn Japanese one (`--lang ja`),
//! run against the real Qwen on four pinned cores at nice 19. See [`GATE`] for
//! what it measured and whether this ships.

use anyhow::Result;
use serde_json::Value;
use tracing::warn;

use crate::llm::{Llm, first_json};

/// The verdict's grammar: one boolean, and no field anywhere in it that could
/// hold an answer. [`crate::digest::VERDICT_GBNF`]'s shape, for its reason.
pub const ANSWERABLE_GBNF: &str = include_str!("../grammars/answerable.gbnf");

/// The answer's grammar, with `%IDS%` where the shown ids go. Substituted by
/// [`answer_grammar`]; kept as a file so the bench and the daemon cannot drift.
pub const ANSWER_GBNF_TEMPLATE: &str = include_str!("../grammars/answer.gbnf.tmpl");

/// Rows one question is answered from. Twelve is what the contract allows and
/// what the budget below comfortably holds; past that a "top hit" is not one.
pub const MAX_HITS: usize = 12;

/// Roughly 1 800 tokens of transcript, counted in characters because that is
/// what this side of the pipe can count. Mixed de/en at Qwen's tokeniser runs
/// near 3.4 characters a token, so the budget is deliberately conservative.
pub const HIT_BUDGET_CHARS: usize = 6_000;

/// Longest single row shown. A four-minute monologue is not a citation.
const MAX_ROW_CHARS: usize = 320;

/// Content words an answer must share with the rows it cited. Two, not one:
/// one word in common is what any two sentences about the same evening have.
pub const MIN_OVERLAP: usize = 2;

/// The same check, counted in character bigrams, for text that is not written
/// with spaces in it (0.11.x).
///
/// A Japanese sentence handed to [`overlap`]'s word split is **one token**, so
/// the grounding check had exactly two outcomes: 1 if the answer was character
/// for character the row, and 0 otherwise. That is not a weak check, it is no
/// check — every honest Japanese answer failed it and the feature could only
/// ever refuse. Bigrams are the standard substitute for a segmenter here and
/// cost nothing: 「八ユーロ」 shares 八ユ, ユー, ーロ with the row it came from.
///
/// Three rather than two, because bigrams are far commoner than content words:
/// です and ました alone hand any two Japanese sentences a bigram each.
pub const MIN_OVERLAP_CJK: usize = 3;

/// Words in an answer. The prompt asks for sixty; this is what is enforced.
const MAX_ANSWER_WORDS: usize = 60;

/// …and the same cap for a language that does not put spaces between them.
/// Sixty words of German is roughly sixty characters of Japanese worth of
/// content, which is what the Japanese prompt asks for.
const MAX_ANSWER_CHARS_CJK: usize = 60;

/// Tokens each call may take. The verdict is one boolean; the answer is two
/// sentences and a short list of small integers.
const VERDICT_TOKENS: i32 = 24;
const ANSWER_TOKENS: i32 = 200;

/// Did `spike/answer_bench` pass?
///
/// The contract says so explicitly: if the bench does not clear its gate,
/// `search.answer` ships as refuse-only rather than shipping a feature that
/// invents. This constant is what that switch is, and the bench table in
/// `spike/FINDINGS.md` §19 is what set it.
///
/// **2026-09-02: 12/12 traps refused, 10/12 answerable answered with every
/// citation correct.** The gate was ≥11/12 and ≥9/12. It ships.
///
/// **2026-09-03, Japanese (`--lang ja`, six cases, FINDINGS §19.1): 3/3 traps
/// refused, 3/3 answered with every citation correct.** The gate was 3/3 and
/// ≥2/3, and it is one switch with the rest: a Japanese question is answered
/// on the same terms as a German one, or refused with the same seven reasons.
///
/// The two it lost are both compound questions ("wer hat den Shader gebaut
/// **und** wo kann man ihn kaufen") where the two halves sit in two rows: the
/// verdict reads "one line does not state all of that" and refuses. That is the
/// error in the right direction, and the user still gets the hits.
pub const GATE: bool = true;

/// The reason a question was not answered. These strings are the contract's —
/// a client renders them.
pub mod refusal {
    /// There was nothing to read. Decided before any model call.
    pub const NO_HITS: &str = "there is nothing in the archive about that";
    /// The verdict call said the rows do not state the answer.
    pub const NOT_STATED: &str = "the transcript does not say";
    /// A post-check failed. Deliberately not "the model was wrong": what the
    /// user needs to know is that the sentence was not read off the rows.
    pub const UNGROUNDED: &str = "the model's answer did not come from the cited turns";
    /// The model is not installed.
    pub const NO_MODEL: &str = "answers need the local model — `recalld models fetch --graph`";
    /// It is installed and `[graph] enabled` is off. A switch that a person
    /// turned off is a switch that stays off, including for a question.
    pub const SWITCHED_OFF: &str = "the local model is switched off";
    /// [`GATE`] is false.
    pub const OFF: &str = "answers are off until the bench passes";
    /// The model call itself did not complete — a timeout and a kill, a
    /// non-zero exit, a child that would not spawn.
    ///
    /// Its own reason, and that is the whole point. This used to be reported
    /// as [`UNGROUNDED`], which says *"the model's answer did not come from
    /// the cited turns"* about an answer that was never produced; the GUI has
    /// no branch for `UNGROUNDED` either, so it fell through to its default
    /// and told the person **"The transcript does not say."** That is a
    /// positive claim about the contents of their own archive, made by a
    /// daemon that had just learned nothing whatsoever about it. A failure
    /// says it failed.
    pub const FAILED: &str = "the model did not answer";

    /// Every reason a client can be handed, in one list.
    ///
    /// Here so the set cannot drift from what the protocol documents and what
    /// a client branches on — the previous drift is why `OFF` was undocumented
    /// and unhandled. `every_refusal_reason_a_client_can_receive_is_written
    /// _down` holds this against `docs/PROTOCOL.md`.
    pub const ALL: &[&str] = &[
        NO_HITS,
        NOT_STATED,
        UNGROUNDED,
        NO_MODEL,
        SWITCHED_OFF,
        OFF,
        FAILED,
    ];
}

/// One row, as the model is shown it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The segment id. It is what the model cites and what a client scrolls to,
    /// so it is the real id and not a position in a list — a citation that
    /// means "the third one" is a citation nothing can follow.
    pub id: i64,
    /// `HH:MM`, local. A question is often about *when*, and a row with no
    /// clock on it cannot answer one.
    pub clock: String,
    /// The speaker's name as the voicebank spells it now, or a placeholder. A
    /// question is often about *who*, for the same reason.
    pub who: String,
    pub text: String,
}

/// What came back, before any of it is believed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub text: String,
    pub citations: Vec<i64>,
}

/// The whole of one `search.answer`, minus the search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Answered(Answer),
    /// One of [`refusal`]'s strings.
    Refused(&'static str),
}

// ---------------------------------------------------------------------------
// the prompts, which are the load-bearing part
// ---------------------------------------------------------------------------

/// The verdict's system prompt.
///
/// Nothing in it about writing, about language, about citations or about
/// length — every one of those is a clause competing for the model's attention,
/// and `spike/digest_bench` measured what that competition costs the refusal.
/// What is in it is the one distinction this feature lives or dies on, said
/// four different ways, plus four worked examples: two refusals of rows that
/// *mention the topic*, one refusal of an intention read as a fact, and one
/// acceptance.
pub const ANSWERABLE_SYSTEM: &str = concat!(
    "You decide ONE thing: do these numbered transcript lines CONTAIN the ",
    "answer to the question? Answerable is true only when you can point at one ",
    "line and read the answer off it. Answerable is FALSE when: the lines are ",
    "about the subject but never state the answer; the answer would have to be ",
    "worked out, inferred, or brought in from outside the lines; somebody says ",
    "they will do a thing and no line says it happened; the question names a ",
    "person who does not speak in the lines; or the lines say that a thing ",
    "exists without saying which one, how much, or when. \"The lines do not ",
    "say\" is a correct and welcome answer. If in doubt, false. Lines may be ",
    "German, English or mixed. Output ONLY JSON.\n",
    "Examples:\n",
    "Q: wie viel kostet der Shader?\n",
    "[12] 20:14 Kira: der Shader ist echt gut geworden\n",
    "[13] 20:15 Aspen: hat drei Wochenenden gedauert\n",
    "-> {\"answerable\": false}\n",
    "Q: hat Kira den Shader gekauft?\n",
    "[40] 20:20 Aspen: acht Euro, mit allen Updates dabei\n",
    "[41] 20:21 Kira: ich kauf ihn heute Abend\n",
    "-> {\"answerable\": false}\n",
    "Q: what did Tamsin say about the shader?\n",
    "[55] 19:02 Aspen: I built the shader myself\n",
    "[56] 19:03 Kira: it is all over my timeline\n",
    "-> {\"answerable\": false}\n",
    "Q: which headphones did Milo buy?\n",
    "[70] 23:50 Milo: I spent the money on the headphones\n",
    "[71] 23:51 Nova: right, the headphones\n",
    "-> {\"answerable\": false}\n",
    "Q: what time is the meetup?\n",
    "[58] 19:38 Wren: eight in the evening, so we catch the Americans too\n",
    "[59] 19:39 Nova: eight is good for me\n",
    "-> {\"answerable\": true}"
);

/// The answer's system prompt, per language.
///
/// Written **in** the language it asks for, with the worked example in that
/// language too — [`crate::digest`]'s finding, which is [`crate::translate`]'s
/// finding: as a line of the *input*, "answer in German" was obeyed once in
/// four. It never re-litigates the verdict; that call has already happened.
pub fn answer_system(tag: &str) -> String {
    if tag == "ja" {
        // Written in Japanese for the reason the German one is written in
        // German, and it matters more here: as a line of the input, "答えは
        // 日本語で" competes with a system prompt in English that the model has
        // just read, and the model writes in the language its instructions are
        // in. The example is Japanese too, including the answer inside the
        // JSON — an example whose `answer` field is English is an instruction
        // to write English, whatever the prose above it says.
        //
        // The length is asked for in characters, not words: a Japanese
        // sentence has no spaces to count.
        return concat!(
            "あなたは、示された番号つきの行だけを使って質問に一つ答えます。",
            "答えが行の中にあることはすでに判断済みです。あなたの仕事は、",
            "それを書き写すことだけです。日本語で、一文か二文、60文字以内で",
            "答えてください。行に書かれていないことは一切書かないでください",
            "——外部の知識も、推測も、「行」や「記録」への言及も禁止です。",
            "`citations` は、その文の出どころとなった行の番号で、最低一つ、",
            "そして示された番号だけです。JSONだけを出力してください。\n",
            "例:\n",
            "質問: シェーダーはいくらですか\n",
            "[107] 19:08 エンバー: 昨日Gumroadにアップロードした\n",
            "[109] 19:10 エンバー: 八ユーロで、アップデートも全部込み\n",
            "-> {\"answer\": \"シェーダーは八ユーロで、アップデートも全部",
            "含まれていて、Gumroadにあります。\", \"citations\": [109, 107]}"
        )
        .to_string();
    }
    if tag == "de" {
        concat!(
            "Du beantwortest EINE Frage und benutzt dafür ausschließlich die ",
            "nummerierten Zeilen, die dir gezeigt werden. Es ist bereits ",
            "entschieden, dass die Antwort in den Zeilen steht; du musst sie ",
            "nur aufschreiben. Antworte AUF DEUTSCH, in einem oder zwei ",
            "Sätzen, höchstens 60 Wörter. Schreibe nichts, was nicht in den ",
            "Zeilen steht — kein Weltwissen, keine Vermutung, keine Rede von ",
            "\"den Zeilen\" oder \"dem Transkript\". `citations` sind die ",
            "Nummern der Zeilen, aus denen der Satz kommt, mindestens eine, ",
            "und nur Nummern, die du gesehen hast. Gib NUR JSON aus.\n",
            "Beispiel:\n",
            "Frage: wie viel kostet der Shader?\n",
            "[107] 19:08 Aspen: ich hab ihn gestern auf Gumroad hochgeladen\n",
            "[109] 19:10 Aspen: acht Euro, mit allen Updates dabei\n",
            "-> {\"answer\": \"Der Shader kostet acht Euro, inklusive aller ",
            "Updates, und liegt auf Gumroad.\", \"citations\": [109, 107]}"
        )
        .to_string()
    } else {
        concat!(
            "You answer ONE question using ONLY the numbered lines you are ",
            "shown. It has already been decided that the answer is in the ",
            "lines; your job is only to write it down. Write in ENGLISH, in ",
            "one or two sentences, at most 60 words. Write nothing that is not ",
            "in the lines — no outside knowledge, no guessing, and never ",
            "mention \"the lines\" or \"the transcript\". `citations` are the ",
            "numbers of the lines the sentence came from, at least one, and ",
            "only numbers you were shown. Output ONLY JSON.\n",
            "Example:\n",
            "Question: what time is the meetup and where?\n",
            "[204] 19:38 Wren: eight in the evening, so we catch the Americans ",
            "too\n",
            "[206] 19:40 Wren: I put it in The Great Pug, the usual instance\n",
            "-> {\"answer\": \"The meetup is at eight in the evening in The ",
            "Great Pug.\", \"citations\": [204, 206]}"
        )
        .to_string()
    }
}

/// The answer grammar for one particular set of shown ids.
///
/// The `id` rule is the ids themselves, alternated. A citation of a row that
/// was not on the page is therefore not something the decoder is able to
/// produce — which is a stronger statement than any instruction in a prompt,
/// and the reason this grammar is built per call instead of being a file.
pub fn answer_grammar(ids: &[i64]) -> String {
    let alternation = ids
        .iter()
        .map(|id| format!("\"{id}\""))
        .collect::<Vec<_>>()
        .join(" | ");
    ANSWER_GBNF_TEMPLATE.replace("%IDS%", &alternation)
}

// ---------------------------------------------------------------------------
// the rows, as the model reads them
// ---------------------------------------------------------------------------

/// `[id] HH:MM Name: text`, one per line, oldest first and clipped to the
/// budget.
///
/// Oldest first because a conversation read backwards is a conversation whose
/// pronouns point at nothing. Clipped from the END, so the highest-ranked hits
/// — which arrive first — are the ones that survive a long day's rows.
pub fn rows_text(rows: &[Row]) -> String {
    let mut out = String::new();
    for row in rows {
        let text = clip(row.text.trim(), MAX_ROW_CHARS);
        let line = format!("[{}] {} {}: {}\n", row.id, row.clock, row.who, text);
        if !out.is_empty() && out.len() + line.len() > HIT_BUDGET_CHARS {
            break;
        }
        out.push_str(&line);
    }
    out
}

/// The ids `rows_text` actually put on the page. The grammar is built from
/// this and not from the input, so a row dropped by the budget is a row the
/// model cannot cite.
/// The trailing space matters (it is what stops `[12]` matching inside
/// `[123]`), and so does the line anchor. `contains` over the whole page also
/// searches the rows' **transcript text**, and a turn can perfectly well
/// contain the characters `[5000] ` — somebody read a log line out, or the
/// decoder heard bracketed numbering. That would admit id 5000 to the grammar
/// and to the allowlist for a row the budget had dropped and the model never
/// saw: a citation the check then happily resolves against a turn that was not
/// on the page. A page is a set of lines, so the question is asked of a line.
pub fn shown_ids(rows: &[Row]) -> Vec<i64> {
    let text = rows_text(rows);
    rows.iter()
        .map(|r| r.id)
        .filter(|id| {
            let head = format!("[{id}] ");
            text.lines().any(|line| line.starts_with(&head))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// the two calls
// ---------------------------------------------------------------------------

/// Do these rows state the answer?
///
/// Its own call, with a prompt that contains nothing else. `Ok(false)` is the
/// answer this feature is mostly for.
pub fn answerable(llm: &Llm, question: &str, rows_text: &str) -> Result<bool> {
    if rows_text.trim().is_empty() {
        return Ok(false);
    }
    let out = llm.ask(
        ANSWERABLE_SYSTEM,
        &verdict_prompt(question, rows_text),
        ANSWERABLE_GBNF,
        VERDICT_TOKENS,
    )?;
    let Some(value) = first_json(&out) else {
        warn!("the model produced nothing the answerable grammar should have allowed");
        return Ok(false);
    };
    Ok(value.get("answerable").and_then(Value::as_bool) == Some(true))
}

/// What the verdict call reads.
///
/// The lines first, the question second, and the **checklist last** — which is
/// the single change that took the traps from 9/12 to the number in FINDINGS
/// §19, and it is a finding worth writing down: with the same rules in the
/// system prompt alone, this model read four rows about a subject and called
/// the subject an answer. The same four clauses immediately before generation,
/// after it has read the rows and knows what is in them, are obeyed. A small
/// model's attention is a recency effect, and the system prompt is the least
/// recent thing in the window.
///
/// It duplicates the system prompt on purpose. The alternative — deleting them
/// from the system prompt and keeping them only here — was measured too, and
/// is worse: the system prompt is what stops it answering in prose.
pub fn verdict_prompt(question: &str, rows_text: &str) -> String {
    format!(
        "Lines:\n{rows_text}\nQuestion: {}\n\n{VERDICT_CHECK}",
        question.trim()
    )
}

/// The checklist itself, a constant so the bench can run the same bytes.
pub const VERDICT_CHECK: &str = concat!(
    "Does ONE of the lines above state the answer to that question? Answer ",
    "false if the lines are only ABOUT the subject; if the answer would have ",
    "to be worked out or brought in from outside; if somebody only says they ",
    "WILL do a thing; if the question names a person who does not speak ",
    "above; or if the lines say a thing exists without saying which one, how ",
    "much, or when."
);

/// What the answer call reads.
pub fn answer_prompt(question: &str, rows_text: &str, tag: &str) -> String {
    let label = match tag {
        "de" => "Frage",
        "ja" => "質問",
        _ => "Question",
    };
    format!("{label}: {}\n\n{rows_text}", question.trim())
}

/// Ask the archive one question. Two calls, the second only if the first said
/// yes, then the post-checks — and every path that is not a good answer is a
/// refusal.
///
/// No store handle is in scope, which is what enforces the lock split
/// [`crate::enrich`] wrote in blood: model time and store-lock time never
/// overlap.
pub fn judge(llm: &Llm, question: &str, rows: &[Row], tag: &str) -> Result<Outcome> {
    if rows.is_empty() {
        return Ok(Outcome::Refused(refusal::NO_HITS));
    }
    let page = rows_text(rows);
    let ids = shown_ids(rows);
    if ids.is_empty() {
        return Ok(Outcome::Refused(refusal::NO_HITS));
    }
    if !answerable(llm, question, &page)? {
        return Ok(Outcome::Refused(refusal::NOT_STATED));
    }
    let out = llm.ask(
        &answer_system(tag),
        &answer_prompt(question, &page, tag),
        &answer_grammar(&ids),
        ANSWER_TOKENS,
    )?;
    let Some(value) = first_json(&out) else {
        warn!("the model said a question was answerable and then wrote nothing");
        return Ok(Outcome::Refused(refusal::UNGROUNDED));
    };
    Ok(check(&value, rows, &ids))
}

/// The post-checks, split out because they are the part worth testing without
/// a 1.9 GB download.
pub fn check(value: &Value, rows: &[Row], shown: &[i64]) -> Outcome {
    let text = value
        .get("answer")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(clip_answer);
    let Some(text) = text else {
        return Outcome::Refused(refusal::UNGROUNDED);
    };

    // Citations: at least one, every one shown, in the order given and without
    // repeats. The grammar says all of this; a check that costs nothing is a
    // check worth having twice.
    let mut citations: Vec<i64> = Vec::new();
    for c in value
        .get("citations")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let Some(id) = c
            .as_i64()
            .or_else(|| c.as_str().and_then(|s| s.parse().ok()))
        else {
            return Outcome::Refused(refusal::UNGROUNDED);
        };
        if !shown.contains(&id) {
            return Outcome::Refused(refusal::UNGROUNDED);
        }
        if !citations.contains(&id) {
            citations.push(id);
        }
    }
    if citations.is_empty() {
        return Outcome::Refused(refusal::UNGROUNDED);
    }

    // Grounding: the sentence has to share content words with the rows it says
    // it came from. Not with the whole page — with the CITED rows, because a
    // citation that points at the wrong row is the failure this is here to
    // catch, and a page-wide check would wave it through.
    let cited: String = rows
        .iter()
        .filter(|r| citations.contains(&r.id))
        .map(|r| r.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    if overlap(&text, &cited) < min_overlap(&text) {
        return Outcome::Refused(refusal::UNGROUNDED);
    }
    Outcome::Answered(Answer { text, citations })
}

/// How many distinct content words two texts share.
///
/// Content, not words: the scaffolding a question and a sentence are built out
/// of ([`crate::ask`]'s list, which is exactly the de/en function words) is
/// shared by every pair of sentences in both languages, so counting it would
/// make this check pass for anything.
///
/// …unless the answer is written without spaces, in which case the unit is a
/// **character bigram** and not a word: see [`MIN_OVERLAP_CJK`] for why a word
/// split is not a check at all on a Japanese sentence. The unit is chosen off
/// the ANSWER, because the answer is what is being held to the rows, and an
/// English sentence about Japanese rows sharing nothing with them is exactly
/// the failure this is here to catch.
pub fn overlap(answer: &str, cited: &str) -> usize {
    if crate::ask::is_japanese_text(answer) {
        let theirs = bigrams(cited);
        let mut seen: Vec<String> = Vec::new();
        for b in bigrams(answer) {
            if theirs.contains(&b) && !seen.contains(&b) {
                seen.push(b);
            }
        }
        return seen.len();
    }
    let theirs = content_words(cited);
    let mut seen: Vec<String> = Vec::new();
    for w in content_words(answer) {
        if theirs.contains(&w) && !seen.contains(&w) {
            seen.push(w);
        }
    }
    seen.len()
}

/// How much overlap this particular answer has to show. The unit differs, so
/// the threshold has to as well — three bigrams, two content words.
pub fn min_overlap(answer: &str) -> usize {
    if crate::ask::is_japanese_text(answer) {
        MIN_OVERLAP_CJK
    } else {
        MIN_OVERLAP
    }
}

/// Every adjacent pair of characters, over each run of letters and digits.
///
/// Normalised first — the compatibility folds that actually occur in this
/// pipeline, which are the fullwidth ASCII forms an IME emits (`Ｇｕｍｒｏａｄ`)
/// and halfwidth katakana (`ｼｪｰﾀﾞｰ`), both of which a decoder and a person can
/// produce for the same word. Full NFKC would be a dependency and a table for
/// two rules; these two are the ones that decide a match here.
///
/// Punctuation is a boundary rather than a character, so 「八ユーロ、全部込み」
/// does not manufacture the bigram 「ロ全」 out of a comma.
fn bigrams(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for run in normalise_cjk(text).split(|c: char| !(c.is_alphanumeric())) {
        let chars: Vec<char> = run.chars().collect();
        for pair in chars.windows(2) {
            let b: String = pair.iter().collect();
            if !out.contains(&b) {
                out.push(b);
            }
        }
        // A one-character run is still evidence — 「八」 on its own is the
        // answer to "how much" — but only as itself, so it is kept whole.
        if chars.len() == 1 {
            let b: String = chars.iter().collect();
            if !out.contains(&b) {
                out.push(b);
            }
        }
    }
    out
}

/// The part of NFKC this pipeline meets: fullwidth ASCII down to ASCII,
/// halfwidth katakana up to fullwidth, and case folded.
fn normalise_cjk(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        let c = match ch as u32 {
            // Fullwidth ! .. ~ -> ASCII.
            n @ 0xFF01..=0xFF5E => char::from_u32(n - 0xFEE0).unwrap_or(ch),
            0x3000 => ' ',
            // Halfwidth katakana -> fullwidth. The mapping is not arithmetic
            // (the halfwidth block is a different order and splits the voiced
            // marks off), so the table is the block, written out.
            0xFF66..=0xFF9D => HALFWIDTH_KATAKANA
                .chars()
                .nth(ch as usize - 0xFF66)
                .unwrap_or(ch),
            _ => ch,
        };
        out.extend(c.to_lowercase());
    }
    out
}

/// U+FF66..U+FF9D in order, as their fullwidth katakana equivalents. The
/// voiced forms `ｶﾞ` are two characters halfwidth and one fullwidth; they are
/// left as the bare kana plus a mark, which is a boundary either way and does
/// not change whether two spellings of a word share bigrams.
const HALFWIDTH_KATAKANA: &str = "ヲァィゥェォャュョッーアイウエオカキクケコサシスセソタチツテトナニヌネノハヒフヘホマミムメモヤユヨラリルレロワン゙゚";

/// Lower-cased, umlauts folded, punctuation gone, function words dropped.
fn content_words(text: &str) -> Vec<String> {
    crate::asr::normalise_words(text)
        .iter()
        .map(|w| crate::ask::fold_word(w))
        .filter(|w| w.chars().count() >= 2 && !crate::ask::is_scaffolding_word(w))
        .collect()
}

/// The length cap, in whichever unit this answer is measured in.
///
/// Japanese is counted in characters because it has no spaces to count: sixty
/// "words" of it is the whole reply, however long, and the cap would never
/// fire. Sixty characters is what the Japanese prompt asks for.
fn clip_answer(s: &str) -> String {
    if crate::ask::is_japanese_text(s) {
        return clip_chars(s, MAX_ANSWER_CHARS_CJK);
    }
    clip_words(s, MAX_ANSWER_WORDS)
}

/// The first `max` characters, cut at a sentence end where there is one in the
/// second half of the budget — `clip_words`' rule, with 。 as the stop.
fn clip_chars(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    // By character, not by byte: `。` is three bytes and a byte-indexed slice
    // one past its start is a panic, not a sentence.
    let stop = head
        .char_indices()
        .rfind(|(_, c)| matches!(c, '。' | '？' | '！' | '.' | '?' | '!'));
    match stop {
        Some((at, c)) if at + c.len_utf8() >= head.len() / 2 => {
            head[..at + c.len_utf8()].to_string()
        }
        _ => head,
    }
}

/// The first `max` words, cut at a sentence end where there is one inside the
/// budget — a sentence stopped mid-clause reads like a truncated file, and the
/// product promise here is one or two sentences.
fn clip_words(s: &str, max: usize) -> String {
    let words: Vec<&str> = s.split_whitespace().collect();
    if words.len() <= max {
        return s.trim().to_string();
    }
    let head = words[..max].join(" ");
    match head.rfind(['.', '!', '?']) {
        Some(at) if at + 1 >= head.len() / 2 => head[..=at].to_string(),
        _ => head,
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// Which language to answer in: the question's, by the same reading
/// [`crate::ask`] gives it.
///
/// Deliberately not [`crate::lang::classify`]: that wants three stopwords and
/// a question has six words, so it answers `Unclear` for most of what arrives
/// here. An umlaut or an ß settles it; otherwise the question words themselves
/// vote, which is what a question is mostly made of; English is the tie-break,
/// because it is the language the model writes in when nobody tells it not to.
pub fn question_lang(question: &str) -> &'static str {
    /// Words that only a German question is built out of.
    const DE: &[&str] = &[
        "was", "wer", "wen", "wem", "wessen", "wann", "wo", "wie", "warum", "wieso", "weshalb",
        "welche", "welcher", "welches", "welchen", "worueber", "worum", "wovon", "hat", "hatte",
        "habe", "haben", "hast", "ist", "sind", "war", "waren", "wird", "wurde", "wurden", "sagt",
        "sagte", "gesagt", "der", "die", "das", "den", "dem", "des", "ein", "eine", "einen", "und",
        "ueber", "von", "vom", "zum", "zur", "im", "am", "beim", "fuer", "nicht", "viel", "lange",
        "heisst", "heiss", "gekauft", "gekostet", "kostet",
    ];
    /// …and an English one.
    const EN: &[&str] = &[
        "what", "who", "whom", "whose", "when", "where", "why", "how", "which", "did", "does",
        "do", "is", "are", "were", "has", "have", "had", "say", "said", "the", "and", "about",
        "of", "to", "in", "on", "at", "for", "with", "from", "that", "many", "much", "long",
        "there", "was",
    ];

    // The script first, as `lang::guess_other` does it and for its reason: a
    // writing system is not a vote, it is an answer, and it is available on a
    // six-word question where a stopword count is not.
    if crate::ask::is_japanese_text(question) {
        return "ja";
    }
    if question
        .chars()
        .any(|c| matches!(c, 'ä' | 'ö' | 'ü' | 'Ä' | 'Ö' | 'Ü' | 'ß'))
    {
        return "de";
    }
    let words: Vec<String> = crate::asr::normalise_words(question)
        .iter()
        .map(|w| crate::ask::fold_word(w))
        .collect();
    let de = words.iter().filter(|w| DE.contains(&w.as_str())).count();
    // "was" is a German interrogative and an English past tense, so it votes
    // for both lists and therefore for neither — which is the honest reading of
    // a word that really is in both languages.
    let en = words.iter().filter(|w| EN.contains(&w.as_str())).count();
    if de > en {
        return "de";
    }
    if en > de {
        return "en";
    }
    match crate::lang::classify(question) {
        crate::lang::Lang::De => "de",
        _ => "en",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(id: i64, clock: &str, who: &str, text: &str) -> Row {
        Row {
            id,
            clock: clock.into(),
            who: who.into(),
            text: text.into(),
        }
    }

    fn page() -> Vec<Row> {
        vec![
            row(
                107,
                "19:08",
                "Aspen",
                "ich hab ihn gestern auf Gumroad hochgeladen",
            ),
            row(109, "19:10", "Aspen", "acht Euro, mit allen Updates dabei"),
            row(112, "19:13", "Kira", "ich kauf ihn heute Abend"),
        ]
    }

    // ---- the grammars ------------------------------------------------------

    /// Verdict-first, taken to its end, exactly as `digest` takes it: the
    /// verdict's grammar has no field anywhere in it that could hold an answer,
    /// so there is nothing in the schema for a model to be tempted by.
    #[test]
    fn the_verdict_grammar_admits_a_boolean_and_nothing_else() {
        let root = ANSWERABLE_GBNF
            .lines()
            .find(|l| l.starts_with("root ::="))
            .expect("a root rule");
        assert!(root.contains("answerable"), "{root}");
        for field in ["answer\\\"", "citations", "str", "chars"] {
            assert!(
                !ANSWERABLE_GBNF.contains(field),
                "the verdict grammar can express {field:?}"
            );
        }
        // Two rules: the object, and whitespace. Nothing else exists.
        assert_eq!(ANSWERABLE_GBNF.matches("::=").count(), 2);
    }

    /// The whole citation guarantee, in one rule: an id that was not shown is
    /// not a token sequence the decoder is able to emit.
    #[test]
    fn the_answer_grammar_is_built_out_of_the_ids_that_were_shown() {
        let g = answer_grammar(&[107, 109, 112]);
        assert!(g.contains(r#"id ::= "107" | "109" | "112""#), "{g}");
        assert!(!g.contains("%IDS%"));
        // At least one citation: the list rule has no empty branch.
        let cites = g.lines().find(|l| l.starts_with("cites ::=")).unwrap();
        assert!(cites.contains("id (ws \",\" ws id)*"), "{cites}");
        assert!(
            !cites.contains(r#""[" ws "]""#),
            "an empty citation list: {cites}"
        );
        // And no verdict in it — by the time this runs, that has been decided.
        assert!(!g.contains("answerable"));
    }

    // ---- the prompts -------------------------------------------------------

    #[test]
    fn the_verdict_prompt_keeps_the_distinction_the_feature_lives_on() {
        for clause in [
            "point at one line and read the answer off it",
            "about the subject but never state the answer",
            "brought in from outside the lines",
            "will do a thing and no line says it happened",
            "person who does not speak in the lines",
            "without saying which one, how much, or when",
            "If in doubt, false",
        ] {
            assert!(
                ANSWERABLE_SYSTEM.contains(clause),
                "the prompt dropped {clause:?}"
            );
        }
        // Four refusals to one acceptance, which is roughly the ratio a real
        // archive has — and the ratio that took the traps from 8/12 to the
        // number in FINDINGS §19.
        assert_eq!(
            ANSWERABLE_SYSTEM.matches("\"answerable\": false").count(),
            4
        );
        assert_eq!(ANSWERABLE_SYSTEM.matches("\"answerable\": true").count(), 1);
        // The examples are shaped like the input: a question, then a PAGE of
        // `[id] HH:MM Name: text` lines. The first version's examples were one
        // line each and the model generalised from the wrong shape.
        assert_eq!(
            ANSWERABLE_SYSTEM.matches("] 2").count() + ANSWERABLE_SYSTEM.matches("] 1").count(),
            10
        );
        // Nothing about writing, length, language or citations: every one of
        // those is a clause competing with the refusal (see `digest`).
        for leak in ["60 words", "sentence", "citations", "ENGLISH", "DEUTSCH"] {
            assert!(
                !ANSWERABLE_SYSTEM.contains(leak),
                "{leak:?} leaked into the verdict prompt"
            );
        }
    }

    #[test]
    fn the_answer_prompt_asks_in_the_language_it_wants_back() {
        let de = answer_system("de");
        assert!(de.contains("Antworte AUF DEUTSCH"), "{de}");
        assert!(
            de.contains("Der Shader kostet acht Euro"),
            "the example is German"
        );
        assert!(!de.contains("Write in ENGLISH"));
        let en = answer_system("en");
        assert!(en.contains("Write in ENGLISH"), "{en}");
        assert!(en.contains("The meetup is at eight"));
        assert_eq!(answer_system("fr"), en, "English is the fallback");
        // Neither re-litigates the verdict.
        assert!(de.contains("bereits entschieden") && en.contains("already been decided"));
        assert!(!de.contains("answerable") && !en.contains("answerable"));
        // Both forbid the two things a grounded answer must never do.
        assert!(de.contains("kein Weltwissen") && en.contains("no outside knowledge"));
    }

    #[test]
    fn every_refusal_reason_a_client_can_receive_is_written_down() {
        // The protocol is the contract a client branches on. A reason that is
        // in the code and not in the document is a reason every client handles
        // by accident — `answers are off until the bench passes` was exactly
        // that, and the GUI rendered it as "The transcript does not say."
        // Prose is wrapped and its backticks are escaped, so both sides are
        // flattened to words before they are compared.
        let flat = |s: &str| {
            s.replace('\\', "")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        };
        let doc = flat(include_str!("../../../docs/PROTOCOL.md"));
        for reason in refusal::ALL {
            assert!(
                doc.contains(&flat(reason)),
                "PROTOCOL.md does not document {reason:?}"
            );
        }
        // …and they are seven distinct strings, so a client that branches on
        // one is never silently handling another.
        let mut seen: Vec<&&str> = refusal::ALL.iter().collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), refusal::ALL.len());

        // The one that matters most: a model that never answered does not get
        // to say anything about the archive.
        assert_ne!(refusal::FAILED, refusal::NOT_STATED);
        assert_ne!(refusal::FAILED, refusal::UNGROUNDED);
    }

    // ---- the page ----------------------------------------------------------

    #[test]
    fn a_row_reads_as_id_clock_name_and_words() {
        let text = rows_text(&page());
        assert_eq!(
            text,
            "[107] 19:08 Aspen: ich hab ihn gestern auf Gumroad hochgeladen\n\
             [109] 19:10 Aspen: acht Euro, mit allen Updates dabei\n\
             [112] 19:13 Kira: ich kauf ihn heute Abend\n"
        );
        assert_eq!(shown_ids(&page()), vec![107, 109, 112]);
    }

    /// A row the budget dropped is a row the model cannot cite — because it is
    /// not in the grammar either.
    #[test]
    fn the_budget_clips_from_the_end_and_the_grammar_follows() {
        let long = "x".repeat(MAX_ROW_CHARS * 2);
        let rows: Vec<Row> = (0..40)
            .map(|i| row(1000 + i, "20:00", "Aspen", &long))
            .collect();
        let text = rows_text(&rows);
        assert!(
            text.len() <= HIT_BUDGET_CHARS + MAX_ROW_CHARS + 64,
            "{}",
            text.len()
        );
        let shown = shown_ids(&rows);
        assert!(shown.len() < rows.len(), "nothing was clipped");
        assert_eq!(shown[0], 1000, "the best hit survived");
        let g = answer_grammar(&shown);
        assert!(!g.contains(&format!("\"{}\"", rows.last().unwrap().id)));
        // …and a row longer than the cap is shown short, with a mark saying so.
        assert!(text.lines().next().unwrap().ends_with('…'));
    }

    /// A row that was dropped by the budget must not be readmitted by
    /// something a *surviving* row happened to say.
    #[test]
    fn a_dropped_row_is_not_let_back_in_by_words_somebody_spoke() {
        let long = "x".repeat(MAX_ROW_CHARS * 2);
        let mut rows: Vec<Row> = (0..40)
            .map(|i| row(1000 + i, "20:00", "Aspen", &long))
            .collect();
        // The last row is far past the budget and will be dropped.
        let dropped = rows.last().unwrap().id;
        // The first row — which survives — quotes it. A person reading a log
        // line out loud, or a decoder hearing bracketed numbering; either way
        // it is transcript text, not a page header.
        rows[0].text = format!("schau mal, da stand [{dropped}] und dann nichts mehr");

        let text = rows_text(&rows);
        let shown = shown_ids(&rows);
        assert!(
            text.contains(&format!("[{dropped}] ")),
            "the page does contain those characters — inside a row's words"
        );
        assert!(
            !text
                .lines()
                .any(|l| l.starts_with(&format!("[{dropped}] "))),
            "…but no line of the page IS that row"
        );

        // So it is not a shown id, it is not in the grammar, and the model
        // cannot cite a turn it was never given.
        assert!(
            !shown.contains(&dropped),
            "a row the budget dropped was readmitted by another row's words"
        );
        assert!(!answer_grammar(&shown).contains(&format!("\"{dropped}\"")));

        // The rows that really are on the page are still all there.
        for line in text.lines() {
            let id: i64 = line
                .trim_start_matches('[')
                .split(']')
                .next()
                .unwrap()
                .parse()
                .unwrap();
            assert!(shown.contains(&id), "{id} is on the page and not shown");
        }
        assert_eq!(shown.len(), text.lines().count());
    }

    // ---- the post-checks ---------------------------------------------------

    fn checked(answer: &str, cites: Value) -> Outcome {
        check(
            &json!({"answer": answer, "citations": cites}),
            &page(),
            &[107, 109, 112],
        )
    }

    #[test]
    fn a_grounded_answer_survives_both_checks() {
        let out = checked("Der Shader kostet acht Euro.", json!([109]));
        assert_eq!(
            out,
            Outcome::Answered(Answer {
                text: "Der Shader kostet acht Euro.".into(),
                citations: vec![109],
            })
        );
    }

    #[test]
    fn a_citation_of_a_row_nobody_saw_is_a_refusal() {
        assert_eq!(
            checked("Der Shader kostet acht Euro.", json!([999])),
            Outcome::Refused(refusal::UNGROUNDED)
        );
        assert_eq!(
            checked("Der Shader kostet acht Euro.", json!([])),
            Outcome::Refused(refusal::UNGROUNDED)
        );
    }

    /// The check the grammar cannot do. Row 112 is about buying it tonight and
    /// says nothing about a price; a sentence about the price that points there
    /// did not come from there.
    #[test]
    fn an_answer_that_did_not_come_from_the_row_it_cites_is_a_refusal() {
        assert_eq!(
            checked("Der Shader kostet acht Euro.", json!([112])),
            Outcome::Refused(refusal::UNGROUNDED)
        );
        // …and the same sentence pointed at the row it really came from passes,
        // which is what makes the check about the citation and not about the
        // sentence.
        assert!(matches!(
            checked("Der Shader kostet acht Euro.", json!([109])),
            Outcome::Answered(_)
        ));
    }

    /// Function words are shared by every pair of German sentences ever
    /// written, so they do not count towards grounding.
    #[test]
    fn scaffolding_is_not_evidence() {
        // "der", "die", "und", "ist", "das" and nothing else in common.
        assert_eq!(
            checked("Und das ist der Grund, die Sache ist so.", json!([109])),
            Outcome::Refused(refusal::UNGROUNDED)
        );
        assert_eq!(overlap("und das ist der", "und das ist der"), 0);
        assert_eq!(
            overlap("acht Euro Updates", "acht Euro, mit allen Updates"),
            3
        );
        // One word in common is what any two sentences about one evening have.
        assert_eq!(MIN_OVERLAP, 2);
        assert_eq!(overlap("Gumroad kostet", "auf Gumroad hochgeladen"), 1);
    }

    #[test]
    fn an_empty_answer_is_not_an_answer() {
        assert_eq!(
            checked("   ", json!([109])),
            Outcome::Refused(refusal::UNGROUNDED)
        );
        assert_eq!(
            check(&json!({"citations": [109]}), &page(), &[107, 109, 112]),
            Outcome::Refused(refusal::UNGROUNDED)
        );
    }

    #[test]
    fn a_repeated_citation_is_listed_once_in_the_order_it_was_given() {
        let Outcome::Answered(a) = checked("Acht Euro, auf Gumroad.", json!([109, 107, 109]))
        else {
            panic!("that was grounded");
        };
        assert_eq!(a.citations, vec![109, 107]);
    }

    #[test]
    fn an_answer_that_became_minutes_is_cut_to_a_sentence() {
        let long = format!(
            "Der Shader kostet acht Euro. {}",
            "und dann noch etwas dazu ".repeat(20)
        );
        let Outcome::Answered(a) = checked(&long, json!([109])) else {
            panic!("grounded, merely long");
        };
        assert!(a.text.split_whitespace().count() <= MAX_ANSWER_WORDS);
        assert!(a.text.starts_with("Der Shader kostet acht Euro."));
    }

    // ---- the language ------------------------------------------------------

    #[test]
    fn the_answer_is_written_in_the_language_the_question_was_asked_in() {
        for q in [
            "wie viel kostet Aspens Shader?",
            "was hat Kira gestern gesagt",
            "wohin fährt Milo im August",
            "welche Grafikkarte hat Milo gekauft",
            "wer hat den Shader gebaut",
        ] {
            assert_eq!(question_lang(q), "de", "{q}");
        }
        for q in [
            "what time is the meetup?",
            "who built the shader",
            "where is Wren's new job",
            "how many people are coming",
            "what was wrong with the microphone",
        ] {
            assert_eq!(question_lang(q), "en", "{q}");
        }
        // Nothing to go on is English: it is what the model writes when nobody
        // tells it otherwise.
        assert_eq!(question_lang(""), "en");
        assert_eq!(question_lang("shader"), "en");
    }

    // ---- 0.11.x: Japanese ---------------------------------------------------

    fn ja_page() -> Vec<Row> {
        vec![
            row(
                301,
                "19:08",
                "エンバー",
                "昨日Gumroadにアップロードしたんだ",
            ),
            row(
                303,
                "19:10",
                "エンバー",
                "八ユーロで、アップデートも全部込みだよ",
            ),
            row(305, "19:13", "キラ", "今晩買うつもり"),
        ]
    }

    #[test]
    fn a_japanese_question_is_answered_in_japanese() {
        for q in [
            "シェーダーはいくらですか",
            "誰が作ったの？",
            "何時？",
            "エンバーはいつアップロードした",
        ] {
            assert_eq!(question_lang(q), "ja", "{q}");
        }
        // …and nothing else changed: the de/en vote still runs on de/en.
        assert_eq!(question_lang("wie viel kostet der Shader?"), "de");
        assert_eq!(question_lang("what time is the meetup?"), "en");
    }

    #[test]
    fn the_japanese_prompt_is_written_in_japanese() {
        let ja = answer_system("ja");
        assert!(ja.contains("日本語で"), "{ja}");
        assert!(ja.contains("60文字以内"), "the cap is in characters");
        // The example — including the sentence inside the JSON — is Japanese.
        // An `answer` field in English is an instruction to write English.
        assert!(ja.contains("シェーダーは八ユーロ"), "{ja}");
        assert!(!ja.contains("Write in ENGLISH") && !ja.contains("Antworte AUF DEUTSCH"));
        // It does not re-litigate the verdict, and it forbids the world.
        assert!(ja.contains("すでに判断済み") && ja.contains("外部の知識"));
        // The user turn asks in the same language.
        assert!(answer_prompt("いくら", "[1] x", "ja").starts_with("質問: いくら"));
    }

    /// The check that had no teeth. A Japanese sentence split on whitespace is
    /// one token, so the word overlap of an honest answer with the row it was
    /// read off was 1 — under [`MIN_OVERLAP`], and therefore a refusal every
    /// time.
    #[test]
    fn japanese_grounding_is_counted_in_bigrams_because_words_are_not_marked() {
        let answer = "シェーダーは八ユーロです。";
        let cited = "八ユーロで、アップデートも全部込みだよ";
        assert!(
            overlap(answer, cited) >= MIN_OVERLAP_CJK,
            "{}",
            overlap(answer, cited)
        );
        assert_eq!(min_overlap(answer), MIN_OVERLAP_CJK);
        // A sentence about a price, pointed at the row about buying it tonight.
        assert!(overlap(answer, "今晩買うつもり") < MIN_OVERLAP_CJK);
        // Latin text is untouched — same unit, same threshold, same numbers.
        assert_eq!(min_overlap("Der Shader kostet acht Euro."), MIN_OVERLAP);
        assert_eq!(
            overlap("acht Euro Updates", "acht Euro, mit allen Updates"),
            3
        );
        // Punctuation is a boundary, not a character: a comma does not
        // manufacture a bigram across it.
        assert!(!bigrams("八ユーロ、全部").contains(&"ロ全".to_string()));
        // The two compatibility folds this pipeline actually meets.
        assert!(overlap("ｼｪｰﾀﾞｰは八ユーロです", cited) >= MIN_OVERLAP_CJK);
        assert_eq!(normalise_cjk("Ｇｕｍｒｏａｄ"), "gumroad");
    }

    #[test]
    fn a_grounded_japanese_answer_survives_both_checks_and_a_wrong_citation_does_not() {
        let ok = check(
            &json!({"answer": "シェーダーは八ユーロです。", "citations": [303]}),
            &ja_page(),
            &[301, 303, 305],
        );
        assert_eq!(
            ok,
            Outcome::Answered(Answer {
                text: "シェーダーは八ユーロです。".into(),
                citations: vec![303],
            })
        );
        assert_eq!(
            check(
                &json!({"answer": "シェーダーは八ユーロです。", "citations": [305]}),
                &ja_page(),
                &[301, 303, 305],
            ),
            Outcome::Refused(refusal::UNGROUNDED)
        );
    }

    /// Sixty *words* of Japanese is the whole reply however long, so the cap is
    /// counted in the unit the language has.
    #[test]
    fn a_japanese_answer_is_capped_in_characters() {
        let long = format!(
            "シェーダーは八ユーロです。{}",
            "とても長い話です。".repeat(20)
        );
        let Outcome::Answered(a) = check(
            &json!({"answer": long, "citations": [303]}),
            &ja_page(),
            &[301, 303, 305],
        ) else {
            panic!("grounded, merely long");
        };
        assert!(a.text.chars().count() <= MAX_ANSWER_CHARS_CJK, "{}", a.text);
        assert!(a.text.starts_with("シェーダーは八ユーロです。"));
        // …and it stops on a sentence end rather than mid-clause.
        assert!(a.text.ends_with('。'), "{}", a.text);
    }

    // ---- the bench's copies ------------------------------------------------

    /// The prompts are measured artefacts, so the bench and the daemon must run
    /// the same bytes — not "the same text, retyped". `digest`'s discipline,
    /// verbatim: re-export with `NXR_WRITE_PROMPTS=1` after editing a prompt,
    /// and until you do, this fails.
    #[test]
    fn the_bench_runs_the_prompts_this_daemon_ships() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../spike/answer_bench");
        let files = [
            ("answerable.system.txt", ANSWERABLE_SYSTEM.to_string()),
            ("answerable.check.txt", VERDICT_CHECK.to_string()),
            ("answer.de.txt", answer_system("de")),
            ("answer.en.txt", answer_system("en")),
            ("answer.ja.txt", answer_system("ja")),
            ("answerable.gbnf", ANSWERABLE_GBNF.to_string()),
            ("answer.gbnf.tmpl", ANSWER_GBNF_TEMPLATE.to_string()),
            // Not a prompt, but exported for the same reason: the bench scores
            // grounding, grounding is "content words", and content words are
            // whatever is not on this list.
            (
                "scaffolding.txt",
                format!("{}\n", crate::ask::scaffolding_words().join("\n")),
            ),
        ];
        let writing = std::env::var("NXR_WRITE_PROMPTS").is_ok_and(|v| !v.is_empty());
        for (name, want) in files {
            let path = dir.join(name);
            if writing {
                std::fs::create_dir_all(&dir).expect("the bench directory");
                std::fs::write(&path, &want).expect("exporting a prompt");
                continue;
            }
            let have = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            assert_eq!(
                have, want,
                "spike/answer_bench/{name} is not what this daemon runs; \
                 re-export with NXR_WRITE_PROMPTS=1 cargo test the_bench_runs"
            );
        }
    }

    // ---- against the real model --------------------------------------------

    fn staged() -> Option<Llm> {
        let raw = std::env::var("NXR_GRAPH_MODELS").ok()?;
        if raw.trim().is_empty() {
            return None;
        }
        Llm::resolve(
            std::path::Path::new(&raw),
            &crate::config::GraphConfig::default(),
            &crate::config::RuntimeConfig::default(),
        )
    }

    /// One answerable question and one trap, through the daemon's own call path
    /// rather than the bench's. The numbers live in `spike/answer_bench`; what
    /// this proves is that the shipped invocation reproduces them.
    #[test]
    fn the_real_model_answers_one_question_and_refuses_one_trap() {
        let Some(llm) = staged() else {
            eprintln!("skipping the answer round trip: set NXR_GRAPH_MODELS=<models dir>");
            return;
        };
        let out =
            judge(&llm, "wie viel kostet der Shader?", &page(), "de").expect("the runner ran");
        let Outcome::Answered(a) = out else {
            panic!("the model refused a question the rows answer: {out:?}");
        };
        eprintln!("answer: {a:?}");
        assert!(a.citations.contains(&109), "{a:?}");
        assert!(a.text.to_lowercase().contains("acht"), "{a:?}");

        // The trap: the rows mention the shader all over and never say who
        // built it.
        assert_eq!(
            judge(&llm, "wer hat den Shader gebaut?", &page(), "de").expect("the runner ran"),
            Outcome::Refused(refusal::NOT_STATED),
        );
    }
}
