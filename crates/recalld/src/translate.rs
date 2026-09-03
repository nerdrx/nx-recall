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
//! ## …and re-measured in 0.11.0, against a translator (FINDINGS §23)
//!
//! The 0.948 above is a gate on one direction. `spike/nllb_bench.py` is the
//! comparison: thirteen FLEURS directions, 100 parallel sentences each, scored
//! by chrF as well as by that cosine, this prompt against
//! NLLB-200-distilled-600M ([`crate::nllb`]).
//!
//! | | mean chrF | empties | echoes | median |
//! |---|---:|---:|---:|---:|
//! | this prompt, qwen2.5-3b | 48.29 | 3 | **171** | 3.69 s |
//! | **nllb-200-distilled-600M int8** | **57.75** | 0 | **0** | 3.59 s |
//!
//! NLLB wins on 13 pairs of 13 with no regression anywhere, so `[assist]
//! translator` ships `"nllb"` and this path is the alternative. The 171 is the
//! finding worth carrying: the prompt's "if the line is already {lang}, repeat
//! it unchanged" clause is correct and a 3B cannot apply it to a language it
//! cannot read, so it handed Finnish back verbatim 34 times in 100 — every one
//! of which [`judge`] correctly dropped, leaving a third of those turns with no
//! translation at all.
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

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::bus::Bus;
use crate::config::AssistConfig;
use crate::control::Control;
use crate::lang::{self, Lang};
use crate::llm::{Llm, first_json};
use crate::nllb::Nllb;
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
    /// 0.11.0. Which backend, live for the same reason the target is.
    translator: String,
    translator_threads: i32,
    /// Where the translator's files are. Set once, at start-up, by
    /// [`set_models_root`] — it is not a setting, it is where the disk is.
    models_root: Option<PathBuf>,
}

impl Live {
    const fn new() -> Self {
        Self {
            to: String::new(),
            read: Vec::new(),
            display: String::new(),
            min_words: 3,
            translator: String::new(),
            translator_threads: 4,
            models_root: None,
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
    // 0.11.0: and the backend, which is process-wide for the same reason and
    // would otherwise carry a loaded ONNX session between tests.
    set_models_root(None);
    forget_translator();
    guard
}

/// Adopt a whole `[assist]` block — start-up, and every `assist.set`.
pub fn adopt(cfg: &AssistConfig) {
    set_target(&cfg.translate_to);
    set_read_languages(&cfg.read_languages);
    set_display(&cfg.translation_display);
    LIVE.write().unwrap_or_else(|p| p.into_inner()).min_words = cfg.translate_min_words.max(1);
    set_translator(&cfg.translator, cfg.translator_threads);
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
    llm: Option<&Llm>,
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
        let (text, lang) = {
            let guard = store.lock().unwrap_or_else(|p| p.into_inner());
            match guard.segment_row(id)? {
                Some(row) if row.translation_via.is_none() => {
                    (row.text.unwrap_or_default(), row.lang.unwrap_or_default())
                }
                // Translated, declined or deleted while it waited.
                _ => continue,
            }
        };
        if text.trim().is_empty() {
            continue;
        }

        // ---- ask (no lock) ----
        // 0.11.0: the same backend rule as `batch` — the dedicated translator
        // when it is selected and installed, else the graph model, else
        // nothing (the row stays queued for a later pass, not declined).
        let asked = if nllb_selected() {
            ask_nllb(&text, &lang, &to)
        } else if let Some(llm) = llm {
            let tuned = llm.with_threads(control.graph().llm_threads);
            ask(&tuned, &text, &to)
        } else {
            live_queue().push_front(id);
            break;
        };
        let verdict = match asked {
            Ok(v) => v,
            Err(e) => {
                warn!(segment = id, "a live translation failed: {e:#}");
                continue;
            }
        };

        // Whichever model actually wrote it (same rule as `batch`).
        let via = if nllb_selected() {
            with_nllb(|n| n.model_id().to_string()).unwrap_or_else(|| TRANSLATOR_NLLB.to_string())
        } else {
            llm.map(|l| l.model_id().to_string()).unwrap_or_default()
        };

        // ---- commit (lock held, no model) ----
        {
            let guard = store.lock().unwrap_or_else(|p| p.into_inner());
            match &verdict {
                Verdict::Translated(text) => guard.set_segment_translation(id, text, &via)?,
                other => {
                    debug!(segment = id, ?other, "no live translation for this turn");
                    guard.mark_translation_declined(id, &via)?;
                }
            }
        }
        // Here and not before the ask: the turn is only work once something
        // decided it. The `break` above puts an untouched id back on the queue
        // and is not work — claiming it was halved the worker's sleep for a
        // pass that translated nothing.
        worked = true;
        if matches!(verdict, Verdict::Translated(_)) {
            let guard = store.lock().unwrap_or_else(|p| p.into_inner());
            crate::pipeline::publish_segment(bus, &guard, id);
        }
    }
    Ok(worked)
}

// ---- end 0.11.0 ------------------------------------------------------------

// ---- 0.11.0, the second backend -------------------------------------------

/// `[assist] translator`: the 0.9.0 path — the graph model's prompt, its
/// grammar, and a 1.9 GB child per line.
pub const TRANSLATOR_QWEN: &str = "qwen";
/// `[assist] translator`: NLLB-200-distilled-600M in this process
/// ([`crate::nllb`]).
pub const TRANSLATOR_NLLB: &str = "nllb";
/// What ships. See FINDINGS §23 for the numbers that chose it.
pub const DEFAULT_TRANSLATOR: &str = TRANSLATOR_NLLB;

/// Choose a backend. An unrecognised name is [`DEFAULT_TRANSLATOR`] rather than
/// an error: a typo in a config file must not switch a feature off silently,
/// and it must not switch it to something nobody named either.
pub fn set_translator(name: &str, threads: i32) {
    let mut live = LIVE.write().unwrap_or_else(|p| p.into_inner());
    live.translator = match name.trim().to_ascii_lowercase().as_str() {
        TRANSLATOR_QWEN => TRANSLATOR_QWEN.to_string(),
        TRANSLATOR_NLLB => TRANSLATOR_NLLB.to_string(),
        _ => DEFAULT_TRANSLATOR.to_string(),
    };
    live.translator_threads = threads.max(1);
}

/// The backend a person asked for, whether or not its model is on disk.
pub fn translator() -> String {
    let name = live().translator;
    if name.is_empty() {
        DEFAULT_TRANSLATOR.to_string()
    } else {
        name
    }
}

/// Where the models are. Start-up only; see [`Live::models_root`].
pub fn set_models_root(root: Option<PathBuf>) {
    LIVE.write().unwrap_or_else(|p| p.into_inner()).models_root = root;
}

/// The translator, loaded on first use and kept.
///
/// A `Mutex<Option<..>>` rather than a `OnceLock` because loading can fail, and
/// a failure must be re-attemptable after somebody runs the fetch — but not
/// re-attempted on every turn, which is what `tried` is for.
static NLLB: std::sync::Mutex<Option<Nllb>> = std::sync::Mutex::new(None);
static NLLB_SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Run `f` against the loaded translator, or `None` when there is not one.
///
/// Absent is an ordinary state: the assets are an opt-in, non-commercially
/// licensed group and this daemon works without them.
fn with_nllb<T>(f: impl FnOnce(&mut Nllb) -> T) -> Option<T> {
    let mut guard = NLLB.lock().unwrap_or_else(|p| p.into_inner());
    if guard.is_none() {
        let live = live();
        let m = crate::models::TranslatorModel::resolve_at(live.models_root?);
        if !m.present() {
            if !NLLB_SAID.swap(true, std::sync::atomic::Ordering::Relaxed) {
                info!("{}", crate::models::TranslatorModel::how_to_get_it());
            }
            return None;
        }
        match Nllb::load(&m, live.translator_threads) {
            Ok(n) => {
                info!(model = n.model_id(), "the translator is loaded");
                *guard = Some(n);
            }
            Err(e) => {
                if !NLLB_SAID.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    warn!("the translator would not load, falling back: {e:#}");
                }
                return None;
            }
        }
    }
    guard.as_mut().map(f)
}

/// Is the dedicated translator both chosen and installed?
pub fn nllb_selected() -> bool {
    translator() == TRANSLATOR_NLLB && with_nllb(|_| ()).is_some()
}

/// One line through the dedicated translator, guards and all.
pub fn ask_nllb(text: &str, from: &str, to: &str) -> Result<Verdict> {
    let answer = with_nllb(|n| n.translate(text, from, to))
        .ok_or_else(|| anyhow::anyhow!("the translator is not installed"))??;
    Ok(judge(text, Some(&answer), to))
}

/// What a reader is shown, and who wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Translation {
    /// The words.
    pub text: String,
    /// The language they are in — always the target that was asked for.
    pub lang: String,
    /// The model id, as it goes into `translation.via`. A reader never sees
    /// which backend produced a row unless they look at this.
    pub via: String,
}

/// Translate one line **now**, for the live path (0.11.0).
///
/// This is the seam the short-line detector calls: a turn has just been
/// committed, it is two words long, and somebody is watching the captions
/// window. It is deliberately the *dedicated* translator only. The Qwen path
/// costs a 1.9 GB process launch per line and measured 4.4 s a sentence on four
/// cores — that is an idle-pass budget, not a live one — so when the translator
/// is not installed this returns `None` and the ordinary
/// [`batch`] pass picks the row up later, which is exactly what 0.9.0 did.
///
/// `src` is the row's language stamp when it has one. `None` asks
/// [`crate::lang`], and a line nothing can name a language for is not
/// translated: translating a language you did not identify is how a mumble
/// becomes a quotation.
pub fn translate_line(text: &str, src: Option<&str>, target: &str) -> Option<Translation> {
    let to = target.trim().to_ascii_lowercase();
    if to.is_empty() || text.trim().is_empty() {
        return None;
    }
    let from = match src.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => s.to_ascii_lowercase(),
        None => match lang::classify(text) {
            Lang::De => "de".to_string(),
            Lang::En => "en".to_string(),
            _ => lang::guess_other(text)?.tag.to_string(),
        },
    };
    if from == to || read_languages().contains(&from) {
        return None;
    }
    if translator() != TRANSLATOR_NLLB {
        return None;
    }
    let verdict = ask_nllb(text, &from, &to).ok()?;
    let Verdict::Translated(words) = verdict else {
        debug!(?verdict, "no live translation for this line");
        return None;
    };
    let via = with_nllb(|n| n.model_id().to_string())?;
    Some(Translation {
        text: words,
        lang: to,
        via,
    })
}

/// Drop the loaded translator. Tests only: the session is a process-wide
/// resource and a test that installed one must not leak it into the next.
#[cfg(test)]
pub(crate) fn forget_translator() {
    *NLLB.lock().unwrap_or_else(|p| p.into_inner()) = None;
    NLLB_SAID.store(false, std::sync::atomic::Ordering::Relaxed);
}

// ---- end 0.11.0 -----------------------------------------------------------

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
    /// The answer is far longer than the line it claims to translate (0.11.0).
    /// A translator that repeats itself does so at length — NLLB's greedy loop
    /// can fall into a repetition on a two-word turn, and a paragraph under a
    /// two-word row would read as something that person said at length.
    TooLong,
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
    if too_long(source, answer) {
        return Verdict::TooLong;
    }
    Verdict::Translated(truncate(answer, MAX_TRANSLATION))
}

/// Three times the input's length, with a floor.
///
/// The ratio alone is unusable at the short end, which is where this feature
/// spends most of its time: "ja" into English is one word and "na dann"
/// reasonably becomes "well then, in that case" — four. The floor is what stops
/// a sensible expansion of a two-word turn from being thrown away, and the
/// ratio is what catches the failure this guard exists for, which is a decoder
/// that has started repeating itself.
fn too_long(source: &str, answer: &str) -> bool {
    let got = lang::word_count(answer);
    got > (3 * length_units(source)).max(MIN_LENGTH_ALLOWANCE)
}

/// How long a line is, in units comparable across scripts.
///
/// Words, except that a Japanese sentence has no spaces in it and
/// [`lang::word_count`] therefore calls a whole paragraph of it *one word*. A
/// guard built on that alone would throw away every honest translation of a
/// Japanese turn — which is the one language in this daemon's list where that
/// mistake is guaranteed rather than possible. Six characters to the unit is
/// roughly what a Japanese sentence's English translation runs at, and for a
/// spaced language the word count is the larger of the two and wins.
fn length_units(s: &str) -> usize {
    lang::word_count(s).max(s.chars().count() / 6).max(1)
}

/// Words an answer may always have, however short the line was.
const MIN_LENGTH_ALLOWANCE: usize = 12;

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
/// The `translation_via` of a row declined because no backend can read its
/// source language. Its own value so `accuracy`/status can count it apart from
/// "the model declined", and so a later backend that CAN read the language
/// has a row to find.
pub const DECLINED_UNSUPPORTED: &str = "unsupported-language";

/// Can the NLLB backend translate *from* this tag? A row with no tag, or a
/// tag outside NLLB's list, is not a question the model can be asked.
fn nllb_can_read(lang: &str) -> bool {
    !lang.trim().is_empty() && crate::nllb::code_for(lang.trim()).is_some()
}

/// Put the guesser's tag on a candidate the column had none for.
///
/// Before 0.11.9 only a *confident* guess reached the row, and the candidate
/// went to the model with `lang == ""`; NLLB refused the empty tag, the error
/// arm left the row unmarked "so a later pass retries", and "Mon petit chou."
/// was retried every five minutes for an evening. The database is still only
/// told about confident guesses (that is a claim on the record); the model is
/// told the best tag there is, which is what the guess was for.
fn adopt_guess(
    mut c: crate::store::TranslateCandidate,
    g: &lang::OtherLang,
) -> crate::store::TranslateCandidate {
    if c.lang.trim().is_empty() {
        c.lang = g.tag.to_string();
    }
    c
}

pub fn batch(
    store: &Arc<std::sync::Mutex<Store>>,
    control: &Arc<Control>,
    bus: &Bus,
    llm: Option<&Llm>,
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
    candidates.extend(guessed.into_iter().map(|(c, g)| adopt_guess(c, &g)));
    if candidates.is_empty() {
        return Ok(false);
    }

    // 0.11.0. One backend for the whole batch, decided before the loop so a
    // person turning the setting mid-batch cannot produce a run of rows with
    // two different `via`s and no way to tell which is which.
    let nllb = nllb_selected();
    // What was actually decided, not what was considered. The caller uses this
    // to halve its sleep — "a backlog should drain at the pace of the model,
    // not at the pace of the sleep" — so a pass that broke out of the loop
    // before it touched anything and still answered `Ok(true)` had the worker
    // re-scanning the database twice as often for ever, on the strength of
    // work it did not do.
    let mut worked = false;
    for c in candidates {
        if stop() || crate::enrich::gate(control, &control.graph()).is_some() {
            break;
        }
        // A source language NLLB has no code for is not a model that timed
        // out: it will fail the same way every pass, and until 0.11.9 two
        // French rows did exactly that every five minutes for an evening.
        // Declined, with a `via` that says why, so the queue stays finite.
        if nllb && !nllb_can_read(&c.lang) {
            let guard = store.lock().unwrap_or_else(|p| p.into_inner());
            debug!(segment = c.id, lang = %c.lang, "no translator for this language");
            guard.mark_translation_declined(c.id, DECLINED_UNSUPPORTED)?;
            worked = true;
            continue;
        }
        // ---- ask (no lock) ----
        let asked = if nllb {
            // The row's own language stamp. Every candidate has one by the time
            // it reaches here — the queue only returns stamped rows, and
            // `adopt_guess` put the guesser's tag on the ones it named — so the
            // translator is never asked to translate *from* a language nobody
            // identified.
            ask_nllb(&c.text, &c.lang, &to)
        } else if let Some(llm) = llm {
            let tuned = llm.with_threads(control.graph().llm_threads);
            ask(&tuned, &c.text, &to)
        } else {
            // Neither backend: nothing to ask, nothing declined.
            break;
        };
        let verdict = match asked {
            Ok(v) => v,
            Err(e) => {
                // Unmarked, so a later pass retries: a model that timed out has
                // not decided this turn is untranslatable.
                warn!(segment = c.id, "a translation failed: {e:#}");
                continue;
            }
        };

        // Whichever model actually wrote it. The reader never sees which
        // backend produced a row unless they look at this.
        let via = if nllb {
            with_nllb(|n| n.model_id().to_string()).unwrap_or_else(|| TRANSLATOR_NLLB.to_string())
        } else {
            // Only reachable with a graph model in hand (see the `break` above).
            llm.map(|l| l.model_id().to_string()).unwrap_or_default()
        };

        // ---- commit (lock held, no model) ----
        {
            let guard = store.lock().unwrap_or_else(|p| p.into_inner());
            match &verdict {
                Verdict::Translated(text) => {
                    guard.set_segment_translation(c.id, text, &via)?;
                }
                other => {
                    // Marked, not left: `translation_via` set with a NULL
                    // `translation` is "looked at and declined", which is what
                    // keeps the queue finite.
                    debug!(segment = c.id, ?other, "no translation for this turn");
                    guard.mark_translation_declined(c.id, &via)?;
                }
            }
        }
        // A row was decided, either way. This is the only place it is set.
        worked = true;
        if matches!(verdict, Verdict::Translated(_)) {
            let guard = store.lock().unwrap_or_else(|p| p.into_inner());
            crate::pipeline::publish_segment(bus, &guard, c.id);
        }
    }
    Ok(worked)
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

    /// `spike/nllb_bench.py`'s Qwen leg runs the prompt this daemon ships.
    ///
    /// Same discipline as `spike/digest_bench`'s: the bench does not restate
    /// the prompt, it reads it out of a file this test writes, and this test
    /// fails if the file and the shipped string have drifted. A comparison
    /// against NLLB is only worth anything if the thing it is compared against
    /// is the thing that is running.
    #[test]
    fn the_bench_runs_the_prompt_this_daemon_ships() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../spike/nllb_bench");
        let writing = std::env::var("NXR_WRITE_PROMPTS").is_ok_and(|v| !v.is_empty());
        if writing {
            std::fs::create_dir_all(&dir).expect("the bench directory");
        }
        for to in ["en", "de"] {
            let path = dir.join(format!("system.{to}.txt"));
            let want = system_for(to);
            if writing {
                std::fs::write(&path, &want).expect("exporting a prompt");
                continue;
            }
            let have = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            assert_eq!(
                have, want,
                "spike/nllb_bench/system.{to}.txt is not the prompt this daemon runs; \
                 re-export with NXR_WRITE_PROMPTS=1 cargo test the_bench_runs_the_prompt"
            );
        }
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

    /// 0.11.0. A greedy decoder that starts repeating itself does so at length,
    /// and a paragraph under a two-word row reads as something that person said
    /// at length. Three times the input, with a floor so the short end — which
    /// is where this feature lives — is not punished for expanding.
    #[test]
    fn an_answer_far_longer_than_the_line_is_not_a_translation_of_it() {
        // The failure: NLLB looping on a fragment.
        assert_eq!(
            judge(
                "na dann",
                Some(
                    "well then, well then, well then, well then, well then, well then, \
                     well then, well then"
                ),
                "en"
            ),
            Verdict::TooLong
        );
        // …and the thing that must NOT be caught by it: a two-word turn whose
        // honest translation is four or five words.
        let v = judge("na dann", Some("well, in that case then"), "en");
        assert!(matches!(v, Verdict::Translated(_)), "{v:?}");
        // German compounds go the other way — one word in, several out — and a
        // ratio with no floor would have thrown this away.
        let v = judge(
            "Geschwindigkeitsbegrenzung",
            Some("the speed limit on this road"),
            "en",
        );
        assert!(matches!(v, Verdict::Translated(_)), "{v:?}");
        // A long line may have a long translation; the guard is a ratio.
        let v = judge(
            "i think the portal behind the bar is the one that only opens after the \
             lights go down in the evening",
            Some(
                "ich glaube das Portal hinter der Bar ist das eine das erst aufgeht \
                 wenn abends die Lichter ausgehen",
            ),
            "de",
        );
        assert!(matches!(v, Verdict::Translated(_)), "{v:?}");
        // And the trap this guard would otherwise walk straight into: Japanese
        // has no spaces, so a whole sentence of it is ONE word by the word
        // count, and every honest English translation of one would have been
        // thrown away.
        let ja = "ライオンの群れはオオカミやイヌの群れと似た行動をとり、驚くほど\
                  ライオンに似た動物で、獲物に対しても同じように致命的です";
        let v = judge(
            ja,
            Some(
                "Lion prides behave much like wolf or dog packs, animals \
                 surprisingly similar to lions in behaviour and just as deadly \
                 to their prey",
            ),
            "en",
        );
        assert!(matches!(v, Verdict::Translated(_)), "{v:?}");
        assert!(
            lang::word_count(ja) <= 1 && length_units(ja) >= 9,
            "a Japanese sentence is one WORD and must not be one length unit: {} / {}",
            lang::word_count(ja),
            length_units(ja)
        );
        assert_eq!(length_units("na dann"), 2);
        assert_eq!(length_units(""), 1, "never zero, so the ratio is defined");
    }

    // ---- 0.11.0, which backend --------------------------------------------

    #[test]
    fn the_backend_is_a_live_setting_and_a_typo_is_the_default() {
        let _live = test_guard();
        assert_eq!(translator(), DEFAULT_TRANSLATOR, "the shipped value");
        set_translator("qwen", 4);
        assert_eq!(translator(), TRANSLATOR_QWEN);
        set_translator("NLLB", 4);
        assert_eq!(translator(), TRANSLATOR_NLLB, "case-folded on the way in");
        // A typo must not switch the feature off, and must not switch it to
        // something nobody named.
        set_translator("nllb2", 4);
        assert_eq!(translator(), DEFAULT_TRANSLATOR);
        set_translator("", 4);
        assert_eq!(translator(), DEFAULT_TRANSLATOR);
        // …and the whole `[assist]` block carries it.
        adopt(&AssistConfig {
            translator: TRANSLATOR_QWEN.into(),
            ..Default::default()
        });
        assert_eq!(translator(), TRANSLATOR_QWEN);
    }

    /// Chosen and *installed* are two different things. A machine that has not
    /// fetched the 911 MB export keeps translating through the other backend
    /// rather than stopping.
    #[test]
    fn a_backend_whose_model_is_absent_is_not_selected() {
        let _live = test_guard();
        set_translator(TRANSLATOR_NLLB, 4);
        assert_eq!(translator(), TRANSLATOR_NLLB, "it is what was asked for");
        set_models_root(Some("/definitely/not/here".into()));
        assert!(!nllb_selected(), "an absent model must not be selected");
        // …and there is no model root at all on a fresh install.
        set_models_root(None);
        assert!(!nllb_selected());
        let err = ask_nllb("ich schicke dir morgen den Link", "de", "en").unwrap_err();
        assert!(err.to_string().contains("not installed"), "{err}");
    }

    #[test]
    fn the_live_path_declines_what_it_should_not_be_asked() {
        let _live = test_guard();
        set_target("en");
        set_translator(TRANSLATOR_NLLB, 4);
        // Translation is off.
        set_target("");
        assert_eq!(
            translate_line("hallo zusammen wie geht es euch", None, ""),
            None
        );
        set_target("en");
        // Nothing to translate.
        assert_eq!(translate_line("   ", Some("de"), "en"), None);
        // Already in the target.
        assert_eq!(
            translate_line("i think that is the only way", Some("en"), "en"),
            None
        );
        // A language the reader already has.
        set_read_languages(&["de".to_string(), "en".to_string()]);
        assert_eq!(
            translate_line("das ist der einzige weg das zu machen", Some("de"), "en"),
            None
        );
        // A line nothing can name a language for is not translated: translating
        // a language you did not identify is how a mumble becomes a quotation.
        set_read_languages(&["en".to_string()]);
        assert_eq!(translate_line("mmm hmm", None, "en"), None);
        // And the Qwen backend has no live path at all — a 1.9 GB process per
        // line is an idle-pass budget, not a live one.
        set_translator(TRANSLATOR_QWEN, 4);
        assert_eq!(
            translate_line("je ne sais pas ce que c'est", Some("fr"), "en"),
            None
        );
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

    #[test]
    fn words_that_were_replaced_take_their_translation_with_them() {
        let _live = test_guard();
        set_read_languages(&["en".to_string()]);
        let (store, ids) = store_with(&[("Sima Sen Okenki Deska.", Some("ja"))]);
        let id = ids[0];
        store
            .set_segment_translation(id, "Sima Sen, how are you.", "q@1")
            .unwrap();
        assert!(
            store
                .segments_for_translation("en", &read_languages(), 3, 50)
                .unwrap()
                .is_empty(),
            "a translated row is out of the queue, which is the whole problem"
        );

        // Now the Japanese router re-decodes it — the same call the context
        // pass and the night shift use.
        store
            .set_segment_text_via(
                id,
                "しませんお元気ですか。",
                "parakeet-ja@1",
                crate::store::text_via::ARBITER,
                42,
            )
            .unwrap();

        let row = store.segment_row(id).unwrap().unwrap();
        assert_eq!(row.text.as_deref(), Some("しませんお元気ですか。"));
        // The translation of the words nobody said is gone, and gone with its
        // `via` — a `via` naming a model that translated a different sentence
        // is provenance for the wrong thing.
        assert_eq!(
            row.translation, None,
            "the row served a translation of the text it no longer has"
        );
        assert_eq!(row.translation_via, None);
        // …so the ordinary pass sees it again, against the words it now says.
        // (`translation_via IS NULL` is the queue predicate, which is why a
        // stale translation was permanent rather than merely wrong.) The floor
        // is one word here because Japanese has no spaces — the same reason
        // `judge`'s length guard counts characters as well as words.
        assert_eq!(
            store
                .segments_for_translation("en", &read_languages(), 1, 50)
                .unwrap()
                .len(),
            1
        );
        // And the prior text is still in the audit trail, as it always was.
        assert_eq!(
            store.operations_of("segments.redecode", 10).unwrap().len(),
            1
        );
    }

    #[test]
    fn a_pass_with_no_backend_at_all_does_not_report_that_it_worked() {
        let _live = test_guard();
        set_target("en");
        set_read_languages(&["en".to_string()]);
        // NLLB is selected but there is no models root, so it is not
        // *installed* — and no graph model is handed in either.
        set_translator(TRANSLATOR_NLLB, 4);
        assert!(!nllb_selected(), "nothing is on disk");

        let (store, ids) = store_with(&[("das ist der einzige weg", Some("de"))]);
        // The row really is a candidate: this is a pass with work in front of
        // it and nothing to do the work with.
        assert_eq!(
            store
                .segments_for_translation("en", &read_languages(), 3, 50)
                .unwrap()
                .len(),
            1
        );

        let bus = Bus::new(64, 32);
        let control = Control::new(
            std::path::PathBuf::from("/nonexistent"),
            None,
            &crate::allowlist::Allowlist::from_rules([("VRChat.exe", true)]),
        );
        let store = Arc::new(std::sync::Mutex::new(store));
        let did = batch(
            &store,
            &control,
            &bus,
            None,
            &AssistConfig::default(),
            &|| false,
        )
        .unwrap();

        // `Ok(true)` means "there was work". Nothing was asked, nothing was
        // written, nothing was even declined — and the caller uses this answer
        // to halve its sleep, so saying yes here is a worker that re-scans the
        // database twice as fast for ever on the strength of a pass that did
        // nothing.
        assert!(!did, "a pass that decided nothing has not worked");
        let row = store.lock().unwrap().segment_row(ids[0]).unwrap().unwrap();
        assert_eq!(row.translation, None);
        assert_eq!(row.translation_via, None, "not even declined");
    }

    // ---- the unconfident guess (0.11.9) --------------------------------------

    #[test]
    fn an_unconfident_guess_still_tells_the_model_the_language() {
        // The two rows from the log: short French the guesser names without
        // confidence. The column stays NULL (no claim on the record); the
        // candidate must not go to the model with an empty tag.
        for line in ["Mon petit chou.", "Je ne sais."] {
            let g = lang::guess_other(line).expect("the guesser names French");
            assert_eq!(g.tag, "fr", "{line}");
            let c = adopt_guess(
                crate::store::TranslateCandidate {
                    id: 1,
                    text: line.into(),
                    lang: String::new(),
                },
                &g,
            );
            assert_eq!(c.lang, "fr", "{line}");
            assert!(nllb_can_read(&c.lang));
        }
        // A stamped row keeps its own stamp, whatever the guesser thinks.
        let g = lang::guess_other("Mon petit chou.").unwrap();
        let c = adopt_guess(
            crate::store::TranslateCandidate {
                id: 2,
                text: "x".into(),
                lang: "it".into(),
            },
            &g,
        );
        assert_eq!(c.lang, "it");
    }

    #[test]
    fn a_language_the_model_cannot_read_is_declined_not_retried() {
        assert!(!nllb_can_read(""));
        assert!(!nllb_can_read("   "));
        assert!(!nllb_can_read("xx"));
        assert!(nllb_can_read("de"));
        assert!(nllb_can_read("ja"));
        assert_eq!(DECLINED_UNSUPPORTED, "unsupported-language");
    }

    // ---- against the real model --------------------------------------------

    /// 0.11.0, the second backend end to end: the settings, the loader, the
    /// guards and the wire shape, on one German line and one Japanese one.
    ///
    /// Gated on `NXR_TRANSLATOR_MODELS`, like every other real-model test here:
    /// absent, it skips and passes, because the export is a ~911 MB optional
    /// download and CI does not have one.
    #[test]
    fn the_real_translator_reads_a_german_and_a_japanese_line_into_english() {
        let _live = test_guard();
        let Some(root) = std::env::var("NXR_TRANSLATOR_MODELS")
            .ok()
            .filter(|s| !s.trim().is_empty())
        else {
            eprintln!("skipping the translator round trip: set NXR_TRANSLATOR_MODELS=<models dir>");
            return;
        };
        set_target("en");
        set_read_languages(&["en".to_string()]);
        set_translator(TRANSLATOR_NLLB, 4);
        set_models_root(Some(root.into()));
        assert!(
            nllb_selected(),
            "NXR_TRANSLATOR_MODELS has no {} — `recalld models fetch --translator`",
            crate::models::TRANSLATOR_DIR
        );

        let de = translate_line("ich schicke dir morgen den Link", Some("de"), "en")
            .expect("the German line survived the guards");
        eprintln!("de -> en: {de:?}");
        assert_eq!(de.lang, "en");
        assert!(de.text.to_lowercase().contains("link"), "{de:?}");
        assert!(
            de.via.starts_with("nllb-200-distilled-600m"),
            "the row must say which model wrote it: {de:?}"
        );
        assert_eq!(lang::classify(&de.text), Lang::En, "{de:?}");

        // Japanese. The row has no language stamp, so the guesser names it by
        // script — which is also the case that proves the source-language token
        // is set by hand rather than by the tokenizer's baked-in `eng_Latn`.
        let ja = translate_line("明日リンクを送ります", None, "en")
            .expect("the Japanese line survived the guards");
        eprintln!("ja -> en: {ja:?}");
        assert_eq!(ja.lang, "en");
        assert!(ja.text.is_ascii(), "that is not English: {ja:?}");
        assert_eq!(ja.via, de.via, "one backend, one id");

        // …and the wire shape a client actually renders.
        assert_eq!(
            translation_json(Some(&de.text), Some(&de.via), "en"),
            Some(serde_json::json!({ "lang": "en", "text": de.text, "via": de.via })),
        );
    }

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
            drain_live(&store, &control, &bus, Some(&llm), &|| false).unwrap();
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
