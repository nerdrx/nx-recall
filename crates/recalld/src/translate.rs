//! A turn you cannot read, in a language you can (PROTOCOL 0.9.0).
//!
//! Half the lobby is German and half of it is not, and the daemon already knows
//! which is which — every committed turn carries a `lang` stamp, from the model
//! or from the classifier or from the conversation around it. `[assist]
//! translate_to` says which language the person reading is expected to have,
//! and an idle pass fills in the rest.
//!
//! ## Why this is dangerous, and what is done about it
//!
//! A translation shown under a transcript line is read as **what that person
//! said**. A fluent paraphrase of something else is therefore worse than no
//! translation at all — it is a quotation nobody uttered, in a record whose
//! whole value is that it is a record. Three things bound that:
//!
//! 1. **The grammar admits one field.** `{"translation": "..."}` and nothing
//!    else: the model cannot explain, cannot answer the line, cannot add a
//!    note about what it thinks was meant.
//! 2. **An echo is dropped.** A model that hands back its input has not
//!    translated it, and a row that says "translation: <the same words>" would
//!    tell a reader the sentence was already in their language when it was not.
//! 3. **A wrong-language answer is dropped.** The same stopword classifier the
//!    rest of the daemon uses ([`crate::lang`]) reads what came back, and an
//!    answer that is not in `translate_to` is thrown away rather than shown.
//!
//! What is left is written with the model id on it (`translation_via`), so a
//! translation from one model is never mistaken for a translation from another,
//! and so the whole lot can be found and re-run when the model changes.
//!
//! ## Measured before it shipped
//!
//! `spike/translate_bench.py`: 20 FLEURS sentence ids that exist in both
//! `en_us` and `de_de` — FLEURS is parallel, so the German reference is a
//! human's and not a model's. English in, German out, scored by cosine in the
//! multilingual-e5-small space `crate::semantic` already uses.
//!
//! | | cosine |
//! |---|---:|
//! | two *different* German FLEURS sentences (the floor) | 0.792 |
//! | the English input against the German reference (passthrough) | 0.906 |
//! | **qwen2.5-3b's German against the German reference** | **0.948** |
//!
//! Gate was ≥ 0.80. Read the floor row before the headline: e5 is multilingual
//! and its space is crowded, so 0.80 is barely off the bottom and the number
//! that actually says the feature works is the 0.906 → 0.948 gap — a real
//! translation beats simply showing the untranslated line.
//!
//! ## Only turns worth the call
//!
//! Three words minimum. "ja klar" translated is "yeah sure" and nobody needed
//! it; below three words is also where the decoder is least reliable, so the
//! model would be translating a guess. Turns already in `translate_to`, and
//! turns in a language the user's own voice is declared to speak, are skipped
//! without a model call at all — see [`Store::segments_for_translation`].
//!
//! ## The lock discipline
//!
//! [`crate::enrich`]'s. Gather under the store lock, call the model without it,
//! commit under it again. **Model time and store-lock time never overlap.**

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;
use tracing::{debug, warn};

use crate::bus::Bus;
use crate::config::AssistConfig;
use crate::control::Control;
use crate::lang::{self, Lang};
use crate::llm::{Llm, first_json};
use crate::store::Store;

/// The bench's grammar, shipped in the crate. One field, no commentary — see
/// the module note for why that is the whole safety story.
pub const TRANSLATE_GBNF: &str = include_str!("../grammars/translate.gbnf");

/// Tokens one translation may take. A turn is a sentence; this is four of them,
/// which is enough for a long turn and not enough for an essay.
const TRANSLATE_TOKENS: i32 = 400;

/// Longest translation kept.
const MAX_TRANSLATION: usize = 1000;

/// How many language-less turns one batch offers the guesser (0.10.2).
///
/// A scan window, not a batch: most rows with no language stamp are mumbles
/// and names that no guess will ever name, and they stay in the window until
/// something else moves them. Two thousand is roughly a fortnight of a busy
/// lobby's unreadable turns, and the window is newest-first — the turns a
/// person is actually reading are the ones at the front of it.
const GUESS_SCAN: usize = 2000;

/// The system prompt, per target language. **Verbatim from
/// `spike/translate_bench.py`**, which is what the 0.948 was measured with,
/// except for the target language name and the example — see below.
///
/// Every clause is a refusal. "Do not answer it" is there because a model shown
/// a question translates it into an answer; "keep names, worlds and usernames
/// exactly as they are spelled" is there because a VRChat lobby is full of
/// proper nouns that look like words.
///
/// 0.10.2: the few-shot example is **in the target language**. It was German
/// hard-coded, from the bench, and a prompt that says "translate into English"
/// under two worked examples that answer in German is a prompt arguing with
/// itself — the one place a small model reliably takes the demonstration over
/// the instruction. Only English and German have written examples because those
/// are the two this project can check; every other target gets the instruction
/// alone, which is honest about what is known rather than shipping a machine
/// translation of a demonstration as if it were one.
pub fn system_for(to: &str) -> String {
    let lang = language_name(to);
    let examples = match to {
        "de" => {
            "Examples:\n\
             i will send you the link tomorrow -> {\"translation\": \"ich schicke dir \
             morgen den Link\"}\n\
             which portal was it -> {\"translation\": \"welches Portal war es\"}"
        }
        "en" => {
            "Examples:\n\
             ich schicke dir morgen den Link -> {\"translation\": \"i will send you \
             the link tomorrow\"}\n\
             welches Portal war es -> {\"translation\": \"which portal was it\"}"
        }
        _ => "",
    };
    format!(
        "You translate one line of overheard conversation into {lang}. Translate \
         only what is written. Do not answer it, do not explain it, do not add or \
         remove anything, and do not comment on it. Keep names, worlds and \
         usernames exactly as they are spelled. If the line is already {lang}, \
         repeat it unchanged. Output ONLY JSON.\n{examples}"
    )
}

/// The English name of a language tag, as the prompt says it.
///
/// Falls back to English rather than to the raw tag: a prompt that says
/// "translate into zz" is a prompt with no instruction in it, and English is
/// the target every other default in this file assumes.
pub fn language_name(tag: &str) -> &'static str {
    lang::name_of(tag).unwrap_or("English")
}

/// `translation_display`: the translation is the line, the original is subtext.
pub const DISPLAY_MAIN: &str = "main";
/// `translation_display`: 0.9.0's layout — the original leads.
pub const DISPLAY_UNDER: &str = "under";

/// The three live translation settings.
///
/// A static, and it is worth saying why rather than hiding it.
/// [`crate::service::segment_json`] is a free function called from a dozen
/// places — a transcript page, a search hit, five different events — precisely
/// so that all of them describe a segment identically, and threading a config
/// value through every one of those call sites to spell one field would trade a
/// global for twelve signatures.
///
/// 0.10.2 made it a lock rather than a `OnceLock`. It was written once at
/// start-up because it was a config file entry; it is now three controls in the
/// Memory view, and a control that needs a restart is not a control. The
/// **worker** reads it here too rather than from the `AssistConfig` it was
/// handed at spawn: that copy is a snapshot from start-up, and a target changed
/// at 21:00 that the queue only honours after a restart is the same bug in a
/// quieter place.
static LIVE: std::sync::RwLock<Live> = std::sync::RwLock::new(Live::new());

#[derive(Debug, Clone)]
struct Live {
    to: String,
    read: Vec<String>,
    display: String,
    /// 0.11.0. Here rather than read from the worker's `AssistConfig` snapshot
    /// for the same reason the other three are: `pipeline::write_segment` has
    /// no `AssistConfig` in scope at all, and threading one down the capture
    /// path to spell one integer would be a worse trade than this static.
    min_words: usize,
}

impl Live {
    const fn new() -> Self {
        Self {
            to: String::new(),
            read: Vec::new(),
            display: String::new(),
            min_words: 3,
        }
    }
}

fn live() -> Live {
    LIVE.read().unwrap_or_else(|p| p.into_inner()).clone()
}

/// Point the daemon at a target language. `""` switches translation off.
pub fn set_target(tag: &str) {
    let tag = tag.trim().to_ascii_lowercase();
    let changed = {
        let mut live = LIVE.write().unwrap_or_else(|p| p.into_inner());
        let changed = live.to != tag;
        live.to = tag;
        changed
    };
    if changed {
        // 0.11.0: turns queued for the old target are not turns anybody asked
        // to see in the new one, and translating them would put a line in a
        // language nobody chose under a transcript row. They keep their NULL
        // `translation_via`, so the ordinary pass re-offers them.
        clear_live();
    }
}

/// The configured target, or `""` when translation is off.
pub fn target() -> String {
    live().to
}

/// Is there a target at all? The worker's own switch, so that turning
/// translation on does not need a restart to be noticed.
pub fn enabled() -> bool {
    !target().is_empty()
}

/// The languages the reader already has. See `[assist] read_languages`.
pub fn set_read_languages(codes: &[String]) {
    LIVE.write().unwrap_or_else(|p| p.into_inner()).read = codes
        .iter()
        .map(|c| c.trim().to_ascii_lowercase())
        .filter(|c| !c.is_empty())
        .collect();
}

/// The languages a turn may be in without being translated — `read_languages`
/// **plus the target**, always. A target you would then translate away from is
/// not a setting anybody meant.
pub fn read_languages() -> Vec<String> {
    let live = live();
    let mut out = live.read;
    let to = live.to;
    if !to.is_empty() && !out.contains(&to) {
        out.push(to);
    }
    out
}

/// Where a client puts the translation. [`DISPLAY_MAIN`] or [`DISPLAY_UNDER`];
/// anything else, including unset, reads as [`DISPLAY_MAIN`].
pub fn set_display(mode: &str) {
    LIVE.write().unwrap_or_else(|p| p.into_inner()).display = mode.trim().to_ascii_lowercase();
}

pub fn display() -> &'static str {
    if live().display == DISPLAY_UNDER {
        DISPLAY_UNDER
    } else {
        DISPLAY_MAIN
    }
}

/// Hold the live settings still for the duration of one test.
///
/// [`LIVE`] is one value for the whole process, which is right for a daemon and
/// awkward for a test binary that runs its tests in parallel threads — a test
/// that sets a target would otherwise be read by a test that asserts there is
/// none. Every test that touches these three takes this first, and the ones
/// that do not, do not care.
#[cfg(test)]
pub(crate) fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let guard = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    adopt(&AssistConfig::default());
    clear_live();
    guard
}

/// Adopt a whole `[assist]` block — start-up, and every `assist.set`.
pub fn adopt(cfg: &AssistConfig) {
    set_target(&cfg.translate_to);
    set_read_languages(&cfg.read_languages);
    set_display(&cfg.translation_display);
    LIVE.write().unwrap_or_else(|p| p.into_inner()).min_words = cfg.translate_min_words.max(1);
}

/// `[assist] translate_min_words`, live like the other three.
pub fn min_words() -> usize {
    live().min_words.max(1)
}

// ---- 0.11.0, the line that is on screen now --------------------------------
//
// Until 0.11.0 a foreign turn waited for the assistant's fair-share pass, and
// that pass yields to the enrichment queue for `assist::SHARE_EVERY_S` — five
// minutes — every time it runs. So the honest description of 0.10.2's
// translation was "some time in the next five minutes, if nothing else is
// queued". The user watched a French line sit untranslated for exactly that
// reason, which is half of why this version exists.
//
// The fix is not a shorter share. A digest is for tomorrow morning and a
// translation is for the sentence somebody is reading *now*: they are not the
// same kind of work and they should not queue behind one another. So a turn
// committed in a language the reader does not have goes onto a small queue of
// its own, the worker is woken, and that queue is drained **before** the gate's
// fair-share arithmetic is even consulted. The gates that are about the machine
// rather than about fairness — paused, and a capture backlog — still apply
// unchanged: a paused daemon writes nothing, including this.

/// How many turns may be waiting for a live translation at once.
///
/// Small on purpose. This queue is "what is on screen", and a lobby that
/// produced more than sixty-four untranslated turns while the model was busy is
/// a lobby whose oldest entry is scrollback, not a caption. Past the cap the
/// **oldest** is dropped — the newest line is the one somebody is reading — and
/// nothing is lost for good: the row keeps its NULL `translation_via`, so the
/// ordinary fair-share pass still picks it up.
pub const LIVE_CAP: usize = 64;

/// The word floor for a line the detector was **confident** about.
///
/// Three words is the ordinary floor, and it is right for the fair-share pass:
/// "ja klar" translated is "yeah sure". It is wrong for a live caption, because
/// a two-word French line is still a line the reader cannot read. Two, and only
/// when the language was named confidently — "Ja." style one-worders stay out
/// at any confidence, and an unconfident two-word guess is exactly the shape
/// that turns out to be a name.
pub const LIVE_MIN_WORDS_CONFIDENT: usize = 2;

static LIVE_QUEUE: std::sync::LazyLock<std::sync::Mutex<std::collections::VecDeque<i64>>> =
    std::sync::LazyLock::new(Default::default);
static LIVE_DROPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn live_queue() -> std::sync::MutexGuard<'static, std::collections::VecDeque<i64>> {
    LIVE_QUEUE.lock().unwrap_or_else(|p| p.into_inner())
}

/// Turns dropped from the front of the queue because it was full.
pub fn live_dropped() -> u64 {
    LIVE_DROPPED.load(std::sync::atomic::Ordering::Relaxed)
}

/// How many turns are waiting. For `status` and for the tests.
pub fn live_queued() -> usize {
    live_queue().len()
}

/// Put a turn at the back of the live queue and wake the worker.
///
/// Idempotent against itself: a segment already waiting is not queued twice,
/// which matters because `write_segment` publishes a segment more than once in
/// some paths and each publish would otherwise cost a model call.
pub fn push_live(segment_id: i64) {
    {
        let mut q = live_queue();
        if q.contains(&segment_id) {
            return;
        }
        while q.len() >= LIVE_CAP {
            q.pop_front();
            LIVE_DROPPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        q.push_back(segment_id);
    }
    crate::assist::wake();
}

/// Take the oldest waiting turn, if there is one.
pub fn take_live() -> Option<i64> {
    live_queue().pop_front()
}

/// Empty the queue. Tests, and `set_target("")` — a target that changed while
/// turns were waiting would translate them into the language nobody asked for.
pub fn clear_live() {
    live_queue().clear();
}

/// Should this committed turn be translated **now**? If so, queue it.
///
/// Called from `pipeline::write_segment` with the store lock already held, once
/// per committed segment. Everything it does is either a read or the one write
/// 0.10.2 already made from the fair-share pass — a confident guess stamped
/// onto `segments.lang` with `lang_via = "guessed"` — so a client that queries
/// the row a millisecond later sees the same language the queue decided on.
///
/// Returns the tag it queued the turn under, for the tests and the log.
pub fn queue_live(store: &Store, segment_id: i64) -> Option<&'static str> {
    if !enabled() {
        return None;
    }
    let row = match store.segment_row(segment_id) {
        Ok(Some(row)) => row,
        Ok(None) => return None,
        Err(e) => {
            warn!(
                segment_id,
                "could not read a segment back to translate it: {e:#}"
            );
            return None;
        }
    };
    if row.translation_via.is_some() {
        return None; // already looked at
    }
    let text = row.text.as_deref().unwrap_or_default();
    let n_words = crate::asr::normalise_words(text).len();
    if n_words == 0 {
        return None;
    }

    // What language it is in, and how sure. A stamped row is taken at its word
    // — the model, the classifier or the conversational prior put it there and
    // all three know more than a guess does. Only an unstamped row is guessed
    // at, which is the same asymmetry the fair-share pass uses: a mumbled
    // German line and a French line are the same NULL, and the guesser naming
    // one of them is the only thing that separates them.
    let (tag, confident) = match row.lang.as_deref() {
        Some(stamped) => {
            // A tag outside the guessable set — `de`, `en`, or something a
            // client wrote. If it is not one the reader has, the ordinary pass
            // will still take it; the live queue only carries what this daemon
            // can name for itself.
            let tag = crate::lang::GUESSABLE.iter().find(|t| **t == stamped)?;
            (*tag, true)
        }
        None => {
            let g = crate::lang::guess_other(text)?;
            if g.confident
                && let Err(e) =
                    store.set_segment_language(segment_id, g.tag, crate::store::lang_via::GUESSED)
            {
                warn!(segment_id, "could not stamp a guessed language: {e:#}");
            }
            (g.tag, g.confident)
        }
    };

    let floor = if confident {
        min_words().min(LIVE_MIN_WORDS_CONFIDENT)
    } else {
        min_words()
    };
    if n_words < floor {
        return None;
    }

    // A language the reader already has — `read_languages` plus the target,
    // plus whatever their own voice is declared to speak — is not a language to
    // translate out of, whatever the target is.
    if read_languages().iter().any(|s| s == tag) {
        return None;
    }
    match store.your_languages() {
        Ok(mine) => {
            if mine.iter().any(|s| s == tag) {
                return None;
            }
        }
        Err(e) => warn!(segment_id, "could not read your own languages: {e:#}"),
    }

    debug!(segment_id, lang = tag, "queued for a live translation");
    push_live(segment_id);
    Some(tag)
}

/// Drain the live queue. `Ok(true)` means there was work.
///
/// One model call per line, and the same lock discipline as [`batch`]: read
/// under the lock, ask without it, commit under it, publish. The gate is
/// re-checked between lines so a pause lands within one translation rather than
/// within one batch.
pub fn drain_live(
    store: &Arc<std::sync::Mutex<Store>>,
    control: &Arc<Control>,
    bus: &Bus,
    llm: &Llm,
    stop: &dyn Fn() -> bool,
) -> Result<bool> {
    let to = target();
    if to.is_empty() {
        clear_live();
        return Ok(false);
    }
    let mut worked = false;
    while let Some(id) = take_live() {
        if stop() || crate::enrich::gate(control, &control.graph()).is_some() {
            // Put it back: the daemon paused, it did not decide.
            live_queue().push_front(id);
            break;
        }
        let text = {
            let guard = store.lock().unwrap_or_else(|p| p.into_inner());
            match guard.segment_row(id)? {
                Some(row) if row.translation_via.is_none() => row.text.unwrap_or_default(),
                // Translated, declined or deleted while it waited.
                _ => continue,
            }
        };
        if text.trim().is_empty() {
            continue;
        }
        worked = true;

        // ---- ask (no lock) ----
        let tuned = llm.with_threads(control.graph().llm_threads);
        let verdict = match ask(&tuned, &text, &to) {
            Ok(v) => v,
            Err(e) => {
                warn!(segment = id, "a live translation failed: {e:#}");
                continue;
            }
        };

        // ---- commit (lock held, no model) ----
        {
            let guard = store.lock().unwrap_or_else(|p| p.into_inner());
            match &verdict {
                Verdict::Translated(text) => {
                    guard.set_segment_translation(id, text, tuned.model_id())?
                }
                other => {
                    debug!(segment = id, ?other, "no live translation for this turn");
                    guard.mark_translation_declined(id, tuned.model_id())?;
                }
            }
        }
        if matches!(verdict, Verdict::Translated(_)) {
            let guard = store.lock().unwrap_or_else(|p| p.into_inner());
            crate::pipeline::publish_segment(bus, &guard, id);
        }
    }
    Ok(worked)
}

// ---- end 0.11.0 ------------------------------------------------------------

/// What the pass concluded about one turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// These words go on the row.
    Translated(String),
    /// The model handed back its input. Not a translation.
    Echoed,
    /// The answer is not in `translate_to` by the stopword classifier.
    WrongLanguage,
    /// The model produced nothing the grammar should have allowed, or nothing
    /// at all.
    Empty,
}

/// Judge one answer. Pure, so all three guards are testable without a model —
/// which is the point, because the guards are the feature.
pub fn judge(source: &str, answer: Option<&str>, to: &str) -> Verdict {
    let Some(answer) = answer.map(str::trim).filter(|s| !s.is_empty()) else {
        return Verdict::Empty;
    };
    // An echo. Compared on normalised words rather than bytes, so a model that
    // gave the input back with different punctuation is still an echo.
    if crate::asr::normalise_words(answer) == crate::asr::normalise_words(source) {
        return Verdict::Echoed;
    }
    // …and the classifier's reading. `Unclear` and `Empty` are NOT rejections:
    // a three-word answer often votes for nothing at all, and throwing those
    // away would drop the short turns this feature is most useful on. Only a
    // clear reading of the *wrong* language is a rejection.
    let want = match to {
        "de" => Some(Lang::De),
        "en" => Some(Lang::En),
        // The two-way classifier has no opinion about any other target, and an
        // absent opinion is not evidence. 0.10.2 gives those targets a check of
        // their own below rather than leaving them unguarded.
        _ => None,
    };
    match want {
        Some(want) => {
            let read = lang::classify(answer);
            if matches!(read, Lang::De | Lang::En) && read != want {
                return Verdict::WrongLanguage;
            }
        }
        None => {
            // A third-language target (0.10.2). The guesser is allowed to
            // reject only what it is *confident* about, and only when it names
            // a language that is not the one asked for: three Spanish
            // stopwords in a Portuguese answer must not throw the answer away,
            // because those two are exactly the pair the guesser is worst at.
            match lang::guess_other(answer) {
                Some(g) if g.confident && g.tag == to => {}
                Some(g) if g.confident => return Verdict::WrongLanguage,
                _ => {
                    // The guesser could not name it. The commonest failure it
                    // cannot see is the one it does not speak: asked for
                    // Japanese, the model answers in English. `classify` is
                    // NOT used for this — it settles on German the moment it
                    // sees an umlaut, and Swedish, Turkish and Finnish are
                    // full of them. The raw stopword evidence, at the same
                    // floor the guesser uses, is the honest test.
                    let (de, en) = lang::stopword_votes(answer);
                    if de.max(en) >= 3 {
                        return Verdict::WrongLanguage;
                    }
                }
            }
        }
    }
    Verdict::Translated(truncate(answer, MAX_TRANSLATION))
}

/// Ask the model for one line. No store handle in scope, which is what enforces
/// the lock split.
pub fn ask(llm: &Llm, text: &str, to: &str) -> Result<Verdict> {
    let out = llm.ask(&system_for(to), text, TRANSLATE_GBNF, TRANSLATE_TOKENS)?;
    let answer = first_json(&out)
        .as_ref()
        .and_then(|v| v.get("translation"))
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(judge(text, answer.as_deref(), to))
}

/// The wire shape, from one function so `segments.*` and the captions window
/// cannot drift. `None` when the row has no translation — which is most rows,
/// and every row on a machine where `translate_to` is empty.
pub fn translation_json(translation: Option<&str>, via: Option<&str>, to: &str) -> Option<Value> {
    // Translation is off. Rows translated while it was on stay in the database
    // — throwing them away would make switching a feature off destructive —
    // but they do not go on the wire: a reading labelled with a language
    // nobody is translating into any more is one a client cannot honestly
    // present, and `lang: ""` would be worse than nothing.
    if to.is_empty() {
        return None;
    }
    let text = translation?;
    Some(serde_json::json!({
        "lang": to,
        "text": text,
        // Which model wrote it. Always present when `text` is.
        "via": via.unwrap_or_default(),
    }))
}

/// One batch. `Ok(true)` means there was work.
pub fn batch(
    store: &Arc<std::sync::Mutex<Store>>,
    control: &Arc<Control>,
    bus: &Bus,
    llm: &Llm,
    cfg: &AssistConfig,
    stop: &dyn Fn() -> bool,
) -> Result<bool> {
    // The live value, not the snapshot the worker was spawned with: see `LIVE`.
    let to = target();
    if to.is_empty() {
        return Ok(false);
    }
    let min_words = cfg.translate_min_words.max(1);
    let limit = cfg.batch.max(1);
    let (mut candidates, unstamped) = {
        let guard = store.lock().unwrap_or_else(|p| p.into_inner());
        // What the reader already has: the configured `read_languages` (which
        // always contains the target), plus whatever their own voice is
        // declared to speak. A turn in any of these is not a turn to translate.
        let mut skip = read_languages();
        for mine in guard.your_languages()? {
            if !skip.contains(&mine) {
                skip.push(mine);
            }
        }
        let stamped = guard.segments_for_translation(&to, &skip, min_words, limit)?;
        // Only worth the scan when there is room left in the batch.
        let unstamped = if stamped.len() < limit {
            guard.segments_without_language(min_words, GUESS_SCAN)?
        } else {
            Vec::new()
        };
        (stamped, unstamped)
    };

    // ---- the third language (no lock) --------------------------------------
    //
    // A turn nothing could read a language out of is a candidate ONLY when the
    // guesser can name one. That asymmetry is the whole safety argument: a
    // mumbled German line and a French line are the same NULL in the column,
    // and the difference between them is the only thing that stops "translate
    // everything I cannot read" from meaning "translate everything".
    let skip = read_languages();
    let mut guessed: Vec<(crate::store::TranslateCandidate, lang::OtherLang)> = Vec::new();
    for c in unstamped {
        if candidates.len() + guessed.len() >= limit {
            break;
        }
        let Some(g) = lang::guess_other(&c.text) else {
            continue;
        };
        if skip.iter().any(|s| s == g.tag) {
            continue;
        }
        guessed.push((c, g));
    }
    if !guessed.is_empty() {
        let guard = store.lock().unwrap_or_else(|p| p.into_inner());
        for (c, g) in &guessed {
            // A confident guess is written onto the row, so the transcript can
            // say what language the line is in and so the next pass finds it
            // through the ordinary query. An unconfident one is enough to ask
            // the model and not enough to claim anything in the database.
            if g.confident {
                guard.set_segment_language(c.id, g.tag, crate::store::lang_via::GUESSED)?;
            }
        }
    }
    candidates.extend(guessed.into_iter().map(|(c, _)| c));
    if candidates.is_empty() {
        return Ok(false);
    }

    for c in candidates {
        if stop() || crate::enrich::gate(control, &control.graph()).is_some() {
            break;
        }
        // ---- ask (no lock) ----
        let tuned = llm.with_threads(control.graph().llm_threads);
        let verdict = match ask(&tuned, &c.text, &to) {
            Ok(v) => v,
            Err(e) => {
                // Unmarked, so a later pass retries: a model that timed out has
                // not decided this turn is untranslatable.
                warn!(segment = c.id, "a translation failed: {e:#}");
                continue;
            }
        };

        // ---- commit (lock held, no model) ----
        {
            let guard = store.lock().unwrap_or_else(|p| p.into_inner());
            match &verdict {
                Verdict::Translated(text) => {
                    guard.set_segment_translation(c.id, text, tuned.model_id())?;
                }
                other => {
                    // Marked, not left: `translation_via` set with a NULL
                    // `translation` is "looked at and declined", which is what
                    // keeps the queue finite.
                    debug!(segment = c.id, ?other, "no translation for this turn");
                    guard.mark_translation_declined(c.id, tuned.model_id())?;
                }
            }
        }
        if matches!(verdict, Verdict::Translated(_)) {
            let guard = store.lock().unwrap_or_else(|p| p.into_inner());
            crate::pipeline::publish_segment(bus, &guard, c.id);
        }
    }
    Ok(true)
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
    use crate::store::SegmentAnalysis;

    #[test]
    fn the_shipped_grammar_is_the_bench_file_and_admits_one_field() {
        // The bench decodes with the file next to it; this asserts the shipped
        // one is the same grammar, which is what makes 0.948 a number about
        // this code rather than about a script.
        let bench = include_str!("../../../spike/translate.gbnf");
        assert_eq!(
            TRANSLATE_GBNF, bench,
            "the shipped grammar drifted from the one the cosine came from"
        );
        let root = TRANSLATE_GBNF
            .lines()
            .find(|l| l.starts_with("root ::="))
            .expect("a root rule");
        assert!(root.contains("translation"), "{root}");
        // One field. There is no alternation in the root at all, so there is
        // nowhere for a commentary field to appear.
        assert!(
            !root.contains('|'),
            "the root admits more than one shape: {root}"
        );
        assert_eq!(TRANSLATE_GBNF.matches("::=").count(), 5);
    }

    #[test]
    fn the_prompt_refuses_every_way_a_model_can_help() {
        let sys = system_for("de");
        for clause in [
            "into German",
            "Do not answer it",
            "do not explain it",
            "do not add or remove anything",
            "Keep names, worlds and usernames exactly as they are spelled",
            "Output ONLY JSON",
        ] {
            assert!(sys.contains(clause), "the prompt dropped {clause:?}");
        }
        assert!(system_for("en").contains("into English"));
        assert!(system_for("zz").contains("into English"), "the fallback");
        assert_eq!(language_name("de"), "German");
        assert_eq!(language_name("ja"), "Japanese");
    }

    #[test]
    fn the_example_answers_in_the_language_the_prompt_asked_for() {
        // 0.10.2. The German example under an English instruction was the one
        // place a 3b model reliably takes the demonstration over the sentence
        // above it — and English is the user's target.
        let en = system_for("en");
        assert!(en.contains("i will send you the link tomorrow\"}"), "{en}");
        assert!(
            !en.contains("ich schicke dir morgen den Link\"}"),
            "the English prompt still demonstrates German: {en}"
        );
        let de = system_for("de");
        assert!(de.contains("ich schicke dir morgen den Link\"}"), "{de}");
        // A target with no checked example gets the instruction and no
        // demonstration at all, rather than a machine-translated one.
        let ja = system_for("ja");
        assert!(
            ja.contains("into Japanese") && !ja.contains("Examples:"),
            "{ja}"
        );
    }

    #[test]
    fn the_three_settings_are_live_and_the_target_is_always_read() {
        let _live = test_guard();
        assert_eq!(target(), "", "the shipped value is off");
        assert!(!enabled());
        assert_eq!(read_languages(), vec!["de".to_string(), "en".to_string()]);
        assert_eq!(display(), DISPLAY_MAIN);

        set_target("EN");
        assert_eq!(target(), "en", "case-folded on the way in");
        assert!(enabled());
        set_read_languages(&["de".to_string()]);
        // …and the target comes back with it, because a target you do not read
        // is a setting that translates a line into a language you cannot read.
        assert_eq!(read_languages(), vec!["de".to_string(), "en".to_string()]);
        set_read_languages(&[]);
        assert_eq!(read_languages(), vec!["en".to_string()]);

        set_display("under");
        assert_eq!(display(), DISPLAY_UNDER);
        set_display("sideways");
        assert_eq!(display(), DISPLAY_MAIN, "an unknown mode is the default");
        set_target("");
        assert!(!enabled(), "off is a setting, not an absence");
    }

    // ---- the three guards --------------------------------------------------

    #[test]
    fn a_model_that_hands_back_its_input_has_not_translated_it() {
        assert_eq!(
            judge("which portal was it", Some("which portal was it"), "de"),
            Verdict::Echoed
        );
        // Punctuation and casing are not a translation either.
        assert_eq!(
            judge("which portal was it", Some("Which portal was it?"), "de"),
            Verdict::Echoed
        );
    }

    #[test]
    fn an_answer_in_the_wrong_language_is_dropped_rather_than_shown() {
        // The real failure: asked for German, the model paraphrases in English.
        assert_eq!(
            judge(
                "which portal was it, the one behind the bar",
                Some("they are asking about the portal behind the bar"),
                "de"
            ),
            Verdict::WrongLanguage
        );
        // The other direction works the same way.
        assert_eq!(
            judge(
                "welches portal war das denn",
                Some("ich glaube das war das im treppenhaus"),
                "en"
            ),
            Verdict::WrongLanguage
        );
    }

    #[test]
    fn a_third_language_target_is_guarded_too_and_not_by_the_umlaut_rule() {
        // 0.10.2. Asked for Japanese, the model answered in English — the
        // commonest failure, and one the two-way classifier used to be given
        // no chance to catch because the target was not one of its two.
        assert_eq!(
            judge(
                "welches portal war das denn",
                Some("i think it was the one behind the bar"),
                "ja"
            ),
            Verdict::WrongLanguage
        );
        // A real answer in the target survives.
        let v = judge("which portal was it", Some("どのポータルでしたか"), "ja");
        assert!(matches!(v, Verdict::Translated(_)), "{v:?}");
        // And the trap: `classify` calls anything with an umlaut German, so a
        // correct Swedish answer must NOT be judged by it.
        let v = judge(
            "i will send you the link tomorrow",
            Some("jag skickar dig länken imorgon"),
            "sv",
        );
        assert!(matches!(v, Verdict::Translated(_)), "{v:?}");
        // A confident guess of the wrong third language is still a rejection.
        assert_eq!(
            judge(
                "which portal was it, the one behind the bar",
                Some("je ne sais pas ce que c'est mais il est dans la boîte"),
                "es"
            ),
            Verdict::WrongLanguage
        );
    }

    #[test]
    fn a_short_answer_the_classifier_cannot_read_is_still_kept() {
        // The classifier votes for nothing on three words very often, and
        // treating "I could not tell" as "wrong language" would throw away the
        // short turns this feature is most useful on. Same rule as `langctx`'s.
        let v = judge("okay cool nice", Some("okay super gut"), "de");
        assert!(matches!(v, Verdict::Translated(_)), "{v:?}");
    }

    #[test]
    fn nothing_at_all_is_not_a_translation() {
        assert_eq!(judge("hello there", None, "de"), Verdict::Empty);
        assert_eq!(judge("hello there", Some("   "), "de"), Verdict::Empty);
    }

    #[test]
    fn a_real_translation_survives_every_guard() {
        let v = judge(
            "i will send you the link tomorrow",
            Some("ich schicke dir morgen den Link"),
            "de",
        );
        assert_eq!(
            v,
            Verdict::Translated("ich schicke dir morgen den Link".into())
        );
    }

    #[test]
    fn the_wire_shape_is_the_one_the_captions_window_renders() {
        assert_eq!(
            translation_json(Some("ich schicke dir morgen den Link"), Some("q@1"), "de"),
            Some(serde_json::json!({
                "lang": "de",
                "text": "ich schicke dir morgen den Link",
                "via": "q@1",
            }))
        );
        assert_eq!(translation_json(None, Some("q@1"), "de"), None);
        // Translation switched off: the row keeps its words, the wire says
        // nothing. `lang: ""` would be a reading in no language at all.
        assert_eq!(translation_json(Some("egal"), Some("q@1"), ""), None);
    }

    // ---- the queue ---------------------------------------------------------

    fn store_with(rows: &[(&str, Option<&str>)]) -> (Store, Vec<i64>) {
        let store = Store::open_in_memory().unwrap();
        let src = store.upsert_source("VRChat.exe", "VRChat", 0).unwrap();
        let sess = store.begin_session(src, 0).unwrap();
        let mut ids = Vec::new();
        for (i, (text, lang)) in rows.iter().enumerate() {
            let t = i as i64 * 1_000_000_000;
            let id = store
                .insert_segment(sess, t, t + 1_000_000_000, "x.wav", t)
                .unwrap();
            store
                .set_segment_analysis(
                    id,
                    &SegmentAnalysis {
                        text: Some((*text).into()),
                        lang: lang.map(str::to_string),
                        lang_via: lang.map(|_| crate::store::lang_via::CLASSIFIED.to_string()),
                        ..Default::default()
                    },
                )
                .unwrap();
            ids.push(id);
        }
        (store, ids)
    }

    #[test]
    fn the_queue_skips_what_a_model_call_could_not_improve() {
        let (store, ids) = store_with(&[
            // Already in the target language: nothing to do, and no model call.
            ("das ist der einzige weg", Some("de")),
            // The one that needs translating.
            ("i think that is the only way to do it", Some("en")),
            // No language stamp at all: nothing says it needs translating.
            ("mmm", None),
            // Under three words.
            ("no way", Some("en")),
        ]);
        let want: Vec<i64> = store
            .segments_for_translation("de", &[], 3, 50)
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(want, vec![ids[1]], "only the long English turn");

        // A reader who has declared they speak English gets none of it.
        assert!(
            store
                .segments_for_translation("de", &["en".to_string()], 3, 50)
                .unwrap()
                .is_empty(),
            "a turn in a language you already read is not a turn to translate"
        );
    }

    #[test]
    fn a_turn_in_no_language_the_classifier_knows_is_only_a_candidate_when_it_can_be_named() {
        // The 0.10.2 rule, and the whole reason `lang::guess_other` exists. All
        // four of these are `lang IS NULL` in the database — the classifier
        // reads nothing out of any of them — and they must not be treated the
        // same way.
        let (store, ids) = store_with(&[
            (
                "je ne sais pas ce que c'est mais il est dans la boîte",
                None,
            ),
            ("это единственный способ сделать это", None),
            // A German mumble. Nothing can read a language out of it either,
            // and it must NOT go to a translator.
            ("mhm ne warte kurz", None),
            // Words in nobody's stopword list at all.
            ("okay cool nice one", None),
        ]);
        let rows = store.segments_without_language(3, 50).unwrap();
        assert_eq!(rows.len(), 4, "all four have no language stamp");
        let named: Vec<(i64, &str)> = rows
            .iter()
            .filter_map(|c| lang::guess_other(&c.text).map(|g| (c.id, g.tag)))
            .collect();
        // Newest first, like every other queue here.
        assert_eq!(named, vec![(ids[1], "ru"), (ids[0], "fr")]);

        // A confident guess is written onto the row, which is what puts it in
        // the ordinary queue from then on — and what lets the transcript say
        // which language the line is in.
        store
            .set_segment_language(ids[1], "ru", crate::store::lang_via::GUESSED)
            .unwrap();
        let queue: Vec<i64> = store
            .segments_for_translation("en", &["de".to_string(), "en".to_string()], 3, 50)
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(queue, vec![ids[1]]);
        // …and it is gone from the guesser's scan, so it is never asked twice.
        assert_eq!(store.segments_without_language(3, 50).unwrap().len(), 3);
    }

    #[test]
    fn a_language_you_read_is_never_translated_whatever_the_target_is() {
        let _live = test_guard();
        // The user's setting: read German and English, translate into English.
        set_target("en");
        set_read_languages(&["de".to_string(), "en".to_string()]);
        assert_eq!(read_languages(), vec!["de".to_string(), "en".to_string()]);
        let (store, ids) = store_with(&[
            ("das ist der einzige weg das zu machen", Some("de")),
            ("i think that is the only way to do it", Some("en")),
        ]);
        assert!(
            store
                .segments_for_translation("en", &read_languages(), 3, 50)
                .unwrap()
                .is_empty(),
            "both turns are in a language the reader has"
        );
        // Drop German from the list and the German turn is a candidate — the
        // one setting doing the one thing it says.
        set_read_languages(&["en".to_string()]);
        let queue: Vec<i64> = store
            .segments_for_translation("en", &read_languages(), 3, 50)
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(queue, vec![ids[0]]);
    }

    #[test]
    fn a_turn_the_pass_declined_leaves_the_queue_and_stays_off_the_wire() {
        let (store, ids) = store_with(&[("i think that is the only way", Some("en"))]);
        let id = ids[0];
        assert_eq!(
            store
                .segments_for_translation("de", &[], 3, 50)
                .unwrap()
                .len(),
            1
        );

        store.mark_translation_declined(id, "q@1").unwrap();
        assert!(
            store
                .segments_for_translation("de", &[], 3, 50)
                .unwrap()
                .is_empty(),
            "a declined turn came back round, so the worker would ask for ever"
        );
        let row = store.segment_row(id).unwrap().unwrap();
        assert_eq!(row.translation, None, "declined is not a translation");
        assert_eq!(row.translation_via.as_deref(), Some("q@1"));
        assert_eq!(store.translation_counts().unwrap(), (0, 1));

        // And a real one.
        store
            .set_segment_translation(id, "ich glaube das ist der einzige weg", "q@1")
            .unwrap();
        let row = store.segment_row(id).unwrap().unwrap();
        assert_eq!(
            row.translation.as_deref(),
            Some("ich glaube das ist der einzige weg")
        );
        assert_eq!(store.translation_counts().unwrap(), (1, 0));

        // Words that changed invalidate it, and put the row back in the queue.
        store.clear_segment_translation(id).unwrap();
        assert_eq!(
            store
                .segments_for_translation("de", &[], 3, 50)
                .unwrap()
                .len(),
            1
        );
    }

    // ---- 0.11.0, the live queue --------------------------------------------

    #[test]
    fn a_turn_the_reader_cannot_read_is_queued_the_moment_it_is_committed() {
        let _live = test_guard();
        set_target("en");
        set_read_languages(&["de".to_string(), "en".to_string()]);
        let (store, ids) = store_with(&[
            // The line the user watched sit there. No stamp, no French function
            // word, three words — 0.10.2 could not even name it.
            ("Tu arrêtes appartement.", None),
            // German and English: never queued, whatever else is true.
            ("das ist der einzige weg das zu machen", Some("de")),
            ("i think that is the only way to do it", Some("en")),
            // A mumble nothing can name.
            ("mhm ne warte kurz", None),
        ]);
        assert_eq!(queue_live(&store, ids[0]), Some("fr"));
        assert_eq!(queue_live(&store, ids[1]), None);
        assert_eq!(queue_live(&store, ids[2]), None);
        assert_eq!(queue_live(&store, ids[3]), None);
        assert_eq!(live_queued(), 1);
        assert_eq!(take_live(), Some(ids[0]));

        // …and the guess was written onto the row, so the transcript event the
        // hook is about to publish already says `fr`.
        let row = store.segment_row(ids[0]).unwrap().unwrap();
        assert_eq!(row.lang.as_deref(), Some("fr"));
        assert_eq!(
            row.lang_via.as_deref(),
            Some(crate::store::lang_via::GUESSED)
        );
    }

    #[test]
    fn a_two_word_line_is_live_only_when_the_language_was_named_confidently() {
        let _live = test_guard();
        set_target("en");
        // Two words, and the detector is sure: a Polish character no other
        // language in the set writes. The reader cannot read it, so it goes —
        // the fair-share pass's three-word floor is about not spending a model
        // call on "ja klar", not about hiding short lines from the reader.
        let (store, ids) = store_with(&[
            ("Dziękuję bardzo", None),
            // Two words and nothing names them at all.
            ("okay cool", None),
        ]);
        assert_eq!(queue_live(&store, ids[0]), Some("pl"));
        assert_eq!(queue_live(&store, ids[1]), None);
        assert_eq!(live_queued(), 1);
        clear_live();

        // One word stays out at any confidence: `LIVE_MIN_WORDS_CONFIDENT` is
        // two, not one, and "Ja." is not a caption.
        let (store, ids) = store_with(&[("Dziękuję", None)]);
        assert_eq!(queue_live(&store, ids[0]), None);
        assert_eq!(live_queued(), 0);
    }

    #[test]
    fn the_queue_is_bounded_and_drops_the_oldest_line_not_the_newest() {
        let _live = test_guard();
        clear_live();
        let before = live_dropped();
        for id in 1..=(LIVE_CAP as i64 + 5) {
            push_live(id);
        }
        assert_eq!(live_queued(), LIVE_CAP, "the cap holds");
        assert_eq!(live_dropped() - before, 5, "and the drops are counted");
        // The oldest five went, so the front is now the sixth.
        assert_eq!(take_live(), Some(6));
        // A turn already waiting is not queued twice — `write_segment` can
        // publish one segment more than once, and each would be a model call.
        clear_live();
        push_live(7);
        push_live(7);
        assert_eq!(live_queued(), 1);
    }

    #[test]
    fn turns_queued_for_one_target_are_not_translated_into_another() {
        let _live = test_guard();
        set_target("en");
        push_live(1);
        push_live(2);
        assert_eq!(live_queued(), 2);
        set_target("de");
        assert_eq!(live_queued(), 0, "a changed target empties the queue");
        // Setting the same target again is not a change and does not clear.
        push_live(3);
        set_target("de");
        assert_eq!(live_queued(), 1);
        // Switching translation off clears it too.
        set_target("");
        assert_eq!(live_queued(), 0);
    }

    #[test]
    fn a_turn_already_looked_at_is_never_queued_again() {
        let _live = test_guard();
        set_target("en");
        let (store, ids) = store_with(&[("Tu arrêtes appartement.", None)]);
        assert_eq!(queue_live(&store, ids[0]), Some("fr"));
        clear_live();
        store.mark_translation_declined(ids[0], "q@1").unwrap();
        assert_eq!(
            queue_live(&store, ids[0]),
            None,
            "declined is a decision, not a gap"
        );
        assert_eq!(live_queued(), 0);
    }

    #[test]
    fn translation_switched_off_queues_nothing_at_all() {
        let _live = test_guard();
        assert!(!enabled(), "the shipped value");
        let (store, ids) = store_with(&[("Tu arrêtes appartement.", None)]);
        assert_eq!(queue_live(&store, ids[0]), None);
        // …and the row is left completely alone: no language stamped by a
        // feature that is switched off.
        assert_eq!(store.segment_row(ids[0]).unwrap().unwrap().lang, None);
    }

    // ---- against the real model --------------------------------------------

    #[test]
    fn a_live_translation_reaches_the_transcript_in_seconds() {
        // The measurement behind FINDINGS §21 and PROTOCOL 0.11.0: the wall
        // clock from the hook `pipeline::write_segment` calls to the `segment`
        // event that carries the translation. Everything between those two
        // points is this feature; the model call inside it is the cost.
        let Some(root) = std::env::var("NXR_GRAPH_MODELS")
            .ok()
            .filter(|s| !s.is_empty())
        else {
            eprintln!("skipping the live-translation latency: set NXR_GRAPH_MODELS=<models dir>");
            return;
        };
        let Some(llm) = Llm::resolve(
            std::path::Path::new(&root),
            &crate::config::GraphConfig::default(),
            &crate::config::RuntimeConfig::default(),
        ) else {
            panic!("NXR_GRAPH_MODELS has no qwen gguf and llama/llama-cli");
        };

        let _live = test_guard();
        set_target("en");
        set_read_languages(&["de".to_string(), "en".to_string()]);
        let lines: &[(&str, Option<&str>)] = &[
            ("Tu arrêtes appartement.", None),
            (
                "je ne sais pas ce que c'est mais il est dans la boîte",
                None,
            ),
            ("Dziękuję bardzo za wszystko", None),
            ("mesto ma mnoho obyvatel", None),
            ("staden har många invånare", None),
        ];
        let (store, ids) = store_with(lines);
        let control = Control::new(
            std::path::PathBuf::from("/nonexistent"),
            None,
            &crate::allowlist::Allowlist::from_rules([("VRChat.exe", true)]),
        );
        control.set_graph_enabled(true);
        let bus = Bus::new(64, 64);
        let (client, rx) = bus.attach(None);
        client.subscribe(&crate::bus::Topic::ALL);
        let store = Arc::new(std::sync::Mutex::new(store));

        let mut waits = Vec::new();
        for id in &ids {
            let queued = {
                let guard = store.lock().unwrap();
                queue_live(&guard, *id)
            };
            let Some(tag) = queued else {
                panic!("segment {id} was not recognised as foreign");
            };
            let started = std::time::Instant::now();
            drain_live(&store, &control, &bus, &llm, &|| false).unwrap();
            // The event, not the row: the number that matters is when a client
            // could have drawn the caption.
            let mut seen = false;
            while let Ok(bytes) = rx.try_recv() {
                let v: Value = serde_json::from_slice(&bytes).unwrap();
                if v["data"]["id"] == serde_json::json!(id)
                    && v["data"]["translation"]["text"].is_string()
                {
                    seen = true;
                }
            }
            let took = started.elapsed();
            eprintln!(
                "  {tag} -> en in {:.2}s{}",
                took.as_secs_f64(),
                if seen { "" } else { " (declined)" }
            );
            if seen {
                waits.push(took);
            }
        }
        assert!(
            !waits.is_empty(),
            "the model translated none of five foreign lines"
        );
        waits.sort();
        let median = waits[waits.len() / 2];
        eprintln!(
            "live translation latency: median {:.2}s over {} lines",
            median.as_secs_f64(),
            waits.len()
        );
    }

    #[test]
    fn the_real_model_translates_a_line_and_does_not_answer_it() {
        let Some(root) = std::env::var("NXR_GRAPH_MODELS")
            .ok()
            .filter(|s| !s.is_empty())
        else {
            eprintln!("skipping the translation round trip: set NXR_GRAPH_MODELS=<models dir>");
            return;
        };
        let Some(llm) = Llm::resolve(
            std::path::Path::new(&root),
            &crate::config::GraphConfig::default(),
            &crate::config::RuntimeConfig::default(),
        ) else {
            panic!("NXR_GRAPH_MODELS has no qwen gguf and llama/llama-cli");
        };

        // A statement.
        let v = ask(&llm, "i will send you the link tomorrow", "de").unwrap();
        let Verdict::Translated(text) = &v else {
            panic!("the model refused a plain sentence: {v:?}");
        };
        eprintln!("translation: {text:?}");
        assert!(
            text.to_lowercase().contains("link"),
            "the object is gone: {text:?}"
        );
        assert_eq!(lang::classify(text), Lang::De, "{text:?}");

        // A QUESTION, which is the case the prompt's "do not answer it" clause
        // exists for: a translated question is still a question.
        let v = ask(&llm, "which portal was it, the one behind the bar?", "de").unwrap();
        let Verdict::Translated(text) = &v else {
            panic!("the model refused a question: {v:?}");
        };
        eprintln!("question: {text:?}");
        assert!(
            text.to_lowercase().contains("portal"),
            "the subject is gone: {text:?}"
        );
    }
}
