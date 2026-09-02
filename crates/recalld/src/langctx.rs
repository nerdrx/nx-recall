//! The conversational language prior (0.7.7).
//!
//! > *"if something doesn't get recognised and everything was german before,
//! > the undecoded stuff is likely to be german too."*
//!
//! That is the whole feature, and it is a claim about **conversations**, not
//! about speakers. 0.6.1 could already correct a transcript when somebody had
//! declared which language a voice speaks — but almost nobody declares
//! anything, and the flip does not wait for them to. What is always available
//! is the thread: threading (0.7.6) already groups turns into conversations,
//! and a conversation has a language whether or not anyone said so.
//!
//! ## What the context is
//!
//! The majority language over a thread's last `[lang].context_window` **clear**
//! stamps — rows that came out `de` or `en`. A context exists once at least
//! `context_min_clear` of them are there and at least `context_min_agree` of
//! them agree. Below that bar the thread has no language and nothing here
//! happens, which is the right answer for a greeting, for a bilingual room, and
//! for the first minute of any evening.
//!
//! Stamps that came *from* a context are excluded from the evidence
//! ([`Store::thread_language_stamps`]). A context that counted its own
//! inferences would be self-reinforcing: three real German turns would inherit
//! their way to a hundred, and the tenth mistake would be indistinguishable
//! from the first fact.
//!
//! ## What it does with it
//!
//! Two things, and they are different sizes:
//!
//! 1. **Inheritance.** A turn that classifies `unclear` — words, but nothing
//!    votes either way: a name, "ok yeah", one number — takes the thread's
//!    language, stamped `lang_via = "context"`. Nothing is re-decoded and no
//!    text changes; a NULL becomes an inference that says it is one. A turn
//!    with *no words at all* stays NULL: there is nothing there to have a
//!    language.
//! 2. **Suspicion.** A turn that classifies as the *opposite* of a strong
//!    context is a suspected flip, and is handed to the arbiter
//!    (`crate::arbiter`) — including when nobody has said a word about the
//!    speaker, which is the case 0.6.1 could not touch and the case that
//!    actually happens.
//!
//! ## Priority
//!
//! An explicit single-language speaker tag **beats** the context, always. A
//! person who has said "this voice speaks English" has given the daemon better
//! evidence than a majority vote, and 0.6.1's behaviour for those speakers is
//! unchanged. The context applies to untagged voices, to bilingual ones, and to
//! turns nobody could put a voice to at all — a flip is a property of the
//! audio, not of whether the voicebank happened to recognise it.

use anyhow::Result;
use tracing::{debug, info};

use crate::config::LangConfig;
use crate::lang::{self, Lang};
use crate::store::{Store, lang_via};

/// A conversation's language, and how strongly it is held.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Context {
    pub lang: Lang,
    /// Clear stamps the answer was read from.
    pub clear: usize,
    /// Share of them that agreed, in `0.0..=1.0`.
    pub agree: f32,
}

/// The majority rule, as a pure function over a thread's recent clear stamps.
///
/// `stamps` is newest-first and already capped at `context_window`; anything
/// that is not a language is ignored rather than counted as a dissenting vote,
/// because "I could not tell" is not evidence that the room switched.
///
/// Returns `None` when the thread has no language — too few stamps, or too much
/// disagreement. There is no weaker "probably German" state on purpose: every
/// consumer of this either acts or does not, and a maybe would only move the
/// decision somewhere less visible.
pub fn context_of(stamps: &[Lang], cfg: &LangConfig) -> Option<Context> {
    if cfg.context_window == 0 {
        return None;
    }
    let de = stamps.iter().filter(|l| **l == Lang::De).count();
    let en = stamps.iter().filter(|l| **l == Lang::En).count();
    let clear = de + en;
    if clear < cfg.context_min_clear.max(1) {
        return None;
    }
    let (lang, count) = if de >= en {
        (Lang::De, de)
    } else {
        (Lang::En, en)
    };
    let agree = count as f32 / clear as f32;
    // `>=` with a tie going to German would be a coin flip dressed as a rule;
    // the agreement floor is what actually decides, and at 0.5 no floor worth
    // having is cleared.
    if agree < cfg.context_min_agree {
        return None;
    }
    Some(Context { lang, clear, agree })
}

/// The context of one thread, read from the database.
///
/// `exclude` is the segment being decided — its own stamp must not vote on
/// itself, or a flip would help confirm itself.
pub fn thread_context(
    store: &Store,
    cfg: &LangConfig,
    thread_id: i64,
    exclude: i64,
) -> Result<Option<Context>> {
    if cfg.context_window == 0 {
        return Ok(None);
    }
    let stamps = store.thread_language_stamps(thread_id, exclude, cfg.context_window)?;
    let stamps: Vec<Lang> = stamps
        .iter()
        .map(|s| match s.as_str() {
            "de" => Lang::De,
            "en" => Lang::En,
            _ => Lang::Unclear,
        })
        .collect();
    Ok(context_of(&stamps, cfg))
}

/// What the prior did to one row, when it did anything.
#[derive(Debug, Clone, PartialEq)]
pub enum ContextFix {
    /// An `unclear` transcript took the conversation's language. Text
    /// untouched; `lang_via = "context"`.
    Inherited { lang: &'static str },
    /// The transcript read as the opposite of the context, the arbiter agreed
    /// it was a flip, and its words won. `lang_via = "re-decode"`.
    Redecoded {
        text: String,
        lang: &'static str,
        asr_model_id: String,
    },
    /// A suspected flip that could not be settled — too short to re-decode, no
    /// arbiter installed for that language, or the arbiter's own answer failed
    /// a guard. The words are kept, `lang` goes to NULL and the row is marked
    /// `mismatch`, which is what `recalld lang repair` later walks.
    Marked { read_as: &'static str },
}

impl ContextFix {
    /// The language the row ended up claiming, for a log line.
    pub fn lang(&self) -> Option<&str> {
        match self {
            ContextFix::Inherited { lang } => Some(lang),
            ContextFix::Redecoded { lang, .. } => Some(lang),
            ContextFix::Marked { .. } => None,
        }
    }
}

/// Everything the prior needs to know about the row it is deciding, gathered in
/// one query so the decision itself is pure and the SQL is in one place.
#[derive(Debug, Clone, PartialEq)]
pub struct Subject {
    pub thread_id: Option<i64>,
    pub text: Option<String>,
    pub lang_via: Option<String>,
    /// The speaker's **sole** declared language, already reduced by
    /// `lang::sole_language`: `Some` only when a person pinned this voice to
    /// exactly one language. That, and only that, beats the context — a
    /// bilingual declaration says nothing a majority vote does not say better.
    pub declared: Option<String>,
}

/// What the prior wants done, before anything is decoded or written.
///
/// Split out from the doing so the whole priority order is one pure function
/// with a table of tests against it, rather than a shape that can only be
/// exercised by loading two ASR models.
#[derive(Debug, Clone, PartialEq)]
pub enum Intent {
    /// Nothing to do: no context, no thread, a tagged speaker (0.6.1 owns that
    /// row), a stamp that already agrees, or no words to have a language.
    Nothing,
    /// Stamp `lang` from the context without touching the text.
    Inherit(Lang),
    /// This reads as the opposite of the conversation. Re-decode toward `want`.
    Arbitrate { want: Lang, read_as: Lang },
}

/// The priority order, as a pure function.
///
/// In order, and the order is the design:
///
/// 1. **No context** — the conversation has not established a language, so
///    there is nothing to bring to bear.
/// 2. **A tagged speaker** — an explicit single-language declaration beats a
///    majority vote, and `Analyzer::correct_language` has already acted on it.
///    Deciding the row twice is how two features start fighting over one column.
/// 3. **Already settled** — a row the arbiter has rewritten, or one already
///    marked as an unsettleable mismatch, is not re-opened by a later turn.
/// 4. **No words** — `Empty` is not a language and never becomes one.
/// 5. **Unclear** — inherit.
/// 6. **Opposite** — suspect a flip, whoever is speaking.
/// 7. **Agrees** — nothing to do, which is the overwhelmingly common case.
pub fn decide(subject: &Subject, context: Option<Context>) -> Intent {
    let Some(context) = context else {
        return Intent::Nothing;
    };
    // A voice pinned to exactly one language is 0.6.1's business, whichever way
    // the vote went — including when the tag and the context agree, because the
    // row is then already correct and re-deciding it can only make it worse.
    if subject.declared.is_some() {
        return Intent::Nothing;
    }
    if matches!(
        subject.lang_via.as_deref(),
        Some(lang_via::REDECODE) | Some(lang_via::MISMATCH)
    ) {
        return Intent::Nothing;
    }
    let Some(text) = subject.text.as_deref().filter(|t| !t.trim().is_empty()) else {
        return Intent::Nothing;
    };
    match lang::classify(text) {
        // No word characters at all. There is nothing here to have a language,
        // and stamping one would be inventing a fact about silence.
        Lang::Empty => Intent::Nothing,
        Lang::Unclear => Intent::Inherit(context.lang),
        read if read == context.lang => Intent::Nothing,
        read => Intent::Arbitrate {
            want: context.lang,
            read_as: read,
        },
    }
}

/// Read the row, decide, and say what should happen. The caller runs the
/// arbiter and writes: this function opens no models and changes nothing.
pub fn intent_for(
    store: &Store,
    cfg: &LangConfig,
    segment_id: i64,
) -> Result<(Intent, Option<Context>)> {
    let Some(subject) = store.language_subject(segment_id)? else {
        return Ok((Intent::Nothing, None));
    };
    let Some(thread_id) = subject.thread_id else {
        // Threading failed or has not run. The prior is a fact about a
        // conversation and there is no conversation to read it from.
        return Ok((Intent::Nothing, None));
    };
    let context = thread_context(store, cfg, thread_id, segment_id)?;
    Ok((decide(&subject, context), context))
}

/// Write what an inheritance decided.
pub fn commit_inheritance(
    store: &Store,
    segment_id: i64,
    lang: Lang,
) -> Result<Option<ContextFix>> {
    let Some(tag) = lang.tag() else {
        return Ok(None);
    };
    store.set_segment_language_from_context(segment_id, tag)?;
    debug!(
        segment_id,
        lang = tag,
        "no language could be read from these words; took the conversation's"
    );
    Ok(Some(ContextFix::Inherited { lang: tag }))
}

/// Write what an arbitration decided, whichever way it went.
///
/// A rejected or impossible arbitration is not a no-op: the row still knows
/// something it did not before — that its language is in doubt — and saying so
/// is what makes `recalld lang repair` able to come back to it once the missing
/// arbiter is installed.
pub fn commit_arbitration(
    store: &Store,
    segment_id: i64,
    want: Lang,
    read_as: Lang,
    outcome: crate::arbiter::Arbitration,
) -> Result<Option<ContextFix>> {
    use crate::arbiter::Arbitration;
    let (Some(want_tag), Some(read_tag)) = (want.tag(), read_as.tag()) else {
        return Ok(None);
    };
    match outcome {
        Arbitration::Replaced { text, model_id } => {
            store.set_segment_text_from_redecode(segment_id, &text, want_tag, &model_id)?;
            info!(
                segment_id,
                model = %model_id,
                read_as = read_tag,
                want = want_tag,
                "the conversation said {want_tag}; re-read the turn and the arbiter agreed"
            );
            Ok(Some(ContextFix::Redecoded {
                text,
                lang: want_tag,
                asr_model_id: model_id,
            }))
        }
        other => {
            store.mark_segment_language_mismatch(segment_id)?;
            debug!(
                segment_id,
                read_as = read_tag,
                want = want_tag,
                outcome = ?other,
                "suspected flip, not settled; the words are kept and the row is marked"
            );
            Ok(Some(ContextFix::Marked { read_as: read_tag }))
        }
    }
}

// ---- the backlog ---------------------------------------------------------

/// What one repair run did. Every count is a row, and they partition `scanned`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepairReport {
    /// Rows whose stored words or language actually moved — the ones a client
    /// showing them has to be told about.
    pub changed: Vec<i64>,
    /// Rows taken off the backlog and looked at.
    pub scanned: usize,
    /// Text replaced by an arbiter that cleared every guard.
    pub repaired: usize,
    /// …of which, by direction.
    pub repaired_de: usize,
    pub repaired_en: usize,
    /// The disagreement is gone — the declaration changed, or the row was
    /// marked before its speaker was known. The classifier's reading was
    /// restored and the mark cleared.
    pub settled: usize,
    /// The arbiter ran and its answer failed a guard. Still marked.
    pub kept: usize,
    /// Under `[lang].arbiter_min_duration_s`; the model was not run.
    pub too_short: usize,
    /// No decoder for the target language is installed. Fetch it and run again.
    pub unavailable: usize,
    /// Nothing says what this row *should* be: no declared language and no
    /// thread context. Left exactly as it was.
    pub undecidable: usize,
    /// The WAV is gone from disk even though the row still names one.
    pub no_audio: usize,
}

/// Walk the mismatch backlog and try to settle it, oldest first.
///
/// This is the retroactive half of 0.7.7, and it is the reason the German
/// arbiter is worth installing on a machine that has been running since 0.6.1:
/// every flip flagged in that time is still on disk with its audio, and the
/// thing that could not be settled then can be settled now. The guards are
/// **exactly** the live ones — the same [`crate::arbiter::Arbiters`], the same
/// duration floor, the same word count, the same classifier agreement — so a
/// repaired row is indistinguishable from one the pipeline got right the first
/// time, which is the only way a backfill is allowed to work.
///
/// Bounded by `limit` and `batch`, resumable by construction: the work list is
/// a query over `lang_via = 'mismatch'`, not a cursor, so a run that is
/// interrupted loses nothing and the next one picks up where the rows still
/// are. `progress` is called once per batch.
pub fn repair(
    store: &Store,
    data_dir: &std::path::Path,
    arbiters: &mut crate::arbiter::Arbiters,
    cfg: &LangConfig,
    batch: usize,
    limit: Option<usize>,
    mut progress: impl FnMut(&RepairReport),
) -> Result<RepairReport> {
    let mut report = RepairReport::default();
    let batch = batch.clamp(1, 512);
    // Rows this run decided to leave alone. Without it a batch of undecidable
    // rows would be handed back by the very next query and the walk would spin
    // on them forever — the work list is a query, so "done with this one" has
    // to be remembered somewhere until the run ends.
    let mut passed: std::collections::HashSet<i64> = std::collections::HashSet::new();

    loop {
        let budget = match limit {
            Some(l) => l.saturating_sub(report.scanned),
            None => batch,
        };
        if budget == 0 {
            break;
        }
        let take = budget.min(batch);
        // Over-read by what has already been passed over: those rows are still
        // in the work list (they are still marked), so without this a batch
        // that is entirely undecidable would be handed back unchanged forever.
        let rows: Vec<(i64, String)> = store
            .language_mismatch_backlog(passed.len() + take)?
            .into_iter()
            .filter(|(id, _)| !passed.contains(id))
            .take(take)
            .collect();
        if rows.is_empty() {
            break;
        }
        for (id, rel) in rows {
            report.scanned += 1;
            match repair_one(store, data_dir, arbiters, cfg, id, &rel)? {
                RepairOne::Repaired(Lang::De) => {
                    report.repaired += 1;
                    report.repaired_de += 1;
                    report.changed.push(id);
                }
                RepairOne::Repaired(_) => {
                    report.repaired += 1;
                    report.repaired_en += 1;
                    report.changed.push(id);
                }
                RepairOne::Settled => {
                    report.settled += 1;
                    report.changed.push(id);
                }
                other => {
                    match other {
                        RepairOne::Kept => report.kept += 1,
                        RepairOne::TooShort => report.too_short += 1,
                        RepairOne::Unavailable => report.unavailable += 1,
                        RepairOne::Undecidable => report.undecidable += 1,
                        RepairOne::NoAudio => report.no_audio += 1,
                        _ => {}
                    }
                    // Still marked, so still in the work list. Remember it.
                    passed.insert(id);
                }
            }
        }
        progress(&report);
    }
    if report.repaired > 0 || report.settled > 0 {
        info!(
            repaired = report.repaired,
            settled = report.settled,
            scanned = report.scanned,
            "language repair pass finished"
        );
    }
    Ok(report)
}

/// What happened to one backlog row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepairOne {
    Repaired(Lang),
    Settled,
    Kept,
    TooShort,
    Unavailable,
    Undecidable,
    NoAudio,
}

fn repair_one(
    store: &Store,
    data_dir: &std::path::Path,
    arbiters: &mut crate::arbiter::Arbiters,
    cfg: &LangConfig,
    segment_id: i64,
    rel: &str,
) -> Result<RepairOne> {
    let Some(subject) = store.language_subject(segment_id)? else {
        return Ok(RepairOne::NoAudio);
    };
    let Some(text) = subject.text.as_deref().filter(|t| !t.trim().is_empty()) else {
        return Ok(RepairOne::Undecidable);
    };
    let read = lang::classify(text);
    let Some(read_tag) = read.tag() else {
        // A row marked as a disagreement whose words no longer read as any
        // language at all. Nothing disagrees with nothing.
        return Ok(RepairOne::Undecidable);
    };

    // What should this have been? Re-derived now rather than trusted from
    // whenever the mark was written: a declaration can have been added, changed
    // or cleared since, and the thread can have grown a context it did not have.
    // The priority is the live one — a tag beats a vote.
    let want = match subject.declared.as_deref() {
        Some("de") => Lang::De,
        Some("en") => Lang::En,
        Some(_) => return Ok(RepairOne::Undecidable),
        None => match subject.thread_id {
            Some(thread_id) => match thread_context(store, cfg, thread_id, segment_id)? {
                Some(ctx) => ctx.lang,
                None => return Ok(RepairOne::Undecidable),
            },
            None => return Ok(RepairOne::Undecidable),
        },
    };
    if want == read {
        // The mark is stale: whatever it disagreed with now agrees. Give the
        // row its classifier reading back rather than leaving a NULL that says
        // "in dispute" about a dispute that ended.
        store.set_segment_language(segment_id, read_tag, lang_via::CLASSIFIED)?;
        debug!(
            segment_id,
            lang = read_tag,
            "the disagreement this row was marked for is gone; stamp restored"
        );
        return Ok(RepairOne::Settled);
    }

    let path = data_dir.join(rel);
    let samples = match crate::ingest::read_wav(&path) {
        Ok(s) => s,
        Err(e) => {
            debug!(segment_id, path = %path.display(), "no audio to repair from: {e:#}");
            return Ok(RepairOne::NoAudio);
        }
    };
    Ok(match arbiters.arbitrate(want, &samples, cfg) {
        crate::arbiter::Arbitration::Replaced { text, model_id } => {
            let tag = want.tag().unwrap_or(read_tag);
            store.set_segment_text_from_redecode(segment_id, &text, tag, &model_id)?;
            info!(
                segment_id,
                model = %model_id,
                was = read_tag,
                now = tag,
                "repaired a flagged transcript from its audio"
            );
            RepairOne::Repaired(want)
        }
        crate::arbiter::Arbitration::TooShort => RepairOne::TooShort,
        crate::arbiter::Arbitration::Unavailable => RepairOne::Unavailable,
        crate::arbiter::Arbitration::Rejected { .. } => RepairOne::Kept,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> LangConfig {
        LangConfig::default()
    }

    fn stamps(spec: &str) -> Vec<Lang> {
        spec.chars()
            .map(|c| match c {
                'd' => Lang::De,
                'e' => Lang::En,
                '?' => Lang::Unclear,
                _ => Lang::Empty,
            })
            .collect()
    }

    // ---- the majority rule -------------------------------------------------

    #[test]
    fn a_conversation_has_no_language_until_three_turns_agree() {
        let cfg = cfg();
        assert_eq!(context_of(&stamps(""), &cfg), None);
        assert_eq!(context_of(&stamps("d"), &cfg), None);
        assert_eq!(context_of(&stamps("dd"), &cfg), None);
        let ctx = context_of(&stamps("ddd"), &cfg).expect("three agreeing turns is a language");
        assert_eq!(ctx.lang, Lang::De);
        assert_eq!(ctx.clear, 3);
        assert!((ctx.agree - 1.0).abs() < 1e-6);
    }

    #[test]
    fn three_turns_have_to_be_unanimous_and_ten_do_not() {
        let cfg = cfg();
        // 2/3 is 0.67, under the 0.7 floor: two German turns and an English one
        // is not a German conversation, it is a conversation.
        assert_eq!(context_of(&stamps("dde"), &cfg), None);
        // 3/4 clears it.
        assert_eq!(
            context_of(&stamps("ddde"), &cfg).map(|c| c.lang),
            Some(Lang::De)
        );
        // Seven of ten: the room is German with an English speaker in it.
        assert_eq!(
            context_of(&stamps("dddddddeee"), &cfg).map(|c| c.lang),
            Some(Lang::De)
        );
        // Six of ten is not enough. A genuinely bilingual room gets no context,
        // which is the whole reason the floor is not a bare majority.
        assert_eq!(context_of(&stamps("ddddddeeee"), &cfg), None);
        // An even split decides nothing whichever way the tie is broken.
        assert_eq!(context_of(&stamps("dede"), &cfg), None);
    }

    #[test]
    fn the_context_follows_a_room_that_really_does_switch() {
        let cfg = cfg();
        // German first...
        assert_eq!(
            context_of(&stamps("ddddd"), &cfg).map(|c| c.lang),
            Some(Lang::De)
        );
        // ...then, once the window has moved, English. Newest-first, so this is
        // the same conversation five turns later.
        assert_eq!(
            context_of(&stamps("eeeeedddd"), &cfg).map(|c| c.lang),
            None,
            "mid-switch there is no majority, and no majority is the honest answer"
        );
        assert_eq!(
            context_of(&stamps("eeeeeeed"), &cfg).map(|c| c.lang),
            Some(Lang::En)
        );
    }

    #[test]
    fn unreadable_turns_are_not_votes_against_anything() {
        let cfg = cfg();
        // Three German turns and a dozen shrugs is still a German conversation:
        // "I could not tell" is not evidence that the room switched.
        assert_eq!(
            context_of(&stamps("d??d?????d??"), &cfg).map(|c| c.lang),
            Some(Lang::De)
        );
        // ...but the shrugs do not get it over the line on their own.
        assert_eq!(context_of(&stamps("??d???"), &cfg), None);
    }

    #[test]
    fn a_zero_window_turns_the_whole_prior_off() {
        let cfg = LangConfig {
            context_window: 0,
            ..LangConfig::default()
        };
        assert_eq!(context_of(&stamps("dddddddddd"), &cfg), None);
    }

    // ---- the priority order ------------------------------------------------

    fn subject(text: &str) -> Subject {
        Subject {
            thread_id: Some(1),
            text: Some(text.into()),
            lang_via: Some(lang_via::CLASSIFIED.into()),
            declared: None,
        }
    }

    fn german_context() -> Option<Context> {
        context_of(&stamps("ddddd"), &cfg())
    }

    #[test]
    fn an_unclear_turn_inherits_the_conversations_language() {
        // "okay" votes for nothing. In a German thread it is German.
        assert_eq!(
            decide(&subject("okay"), german_context()),
            Intent::Inherit(Lang::De)
        );
        // With no context it stays unclear, which is what 0.6.1 did.
        assert_eq!(decide(&subject("okay"), None), Intent::Nothing);
    }

    #[test]
    fn a_turn_with_no_words_stays_null_however_strong_the_context() {
        // The scope line from the design: only *unclear* inherits. There is
        // nothing in "..." to have a language.
        assert_eq!(
            decide(&subject("... --"), german_context()),
            Intent::Nothing
        );
        assert_eq!(
            decide(&subject("2019 42"), german_context()),
            Intent::Nothing
        );
        let mut s = subject("x");
        s.text = None;
        assert_eq!(decide(&s, german_context()), Intent::Nothing);
        s.text = Some("   ".into());
        assert_eq!(decide(&s, german_context()), Intent::Nothing);
    }

    #[test]
    fn a_turn_that_agrees_with_the_room_is_left_alone() {
        assert_eq!(
            decide(
                &subject("ich glaube das ist der einzige weg"),
                german_context()
            ),
            Intent::Nothing
        );
    }

    #[test]
    fn an_english_turn_in_a_german_thread_is_a_suspected_flip() {
        // The 12%-at-1s case, and the speaker is nobody in particular — which
        // is the case 0.6.1 could not touch at all.
        assert_eq!(
            decide(
                &subject("i think that is the only way to do it"),
                german_context()
            ),
            Intent::Arbitrate {
                want: Lang::De,
                read_as: Lang::En,
            }
        );
    }

    #[test]
    fn the_other_direction_works_the_same_way() {
        let english = context_of(&stamps("eeeee"), &cfg());
        assert_eq!(
            decide(&subject("ich glaube das ist der einzige weg"), english),
            Intent::Arbitrate {
                want: Lang::En,
                read_as: Lang::De,
            }
        );
    }

    #[test]
    fn a_tagged_speaker_beats_the_context_in_both_directions() {
        // The declaration is better evidence than a vote, and 0.6.1 has already
        // acted on it. Deciding the row twice is how two features start
        // fighting over one column.
        let mut s = subject("i think that is the only way to do it");
        s.declared = Some("en".into());
        assert_eq!(decide(&s, german_context()), Intent::Nothing);
        // Even when the tag and the context agree: the row is already right.
        s.declared = Some("de".into());
        assert_eq!(decide(&s, german_context()), Intent::Nothing);
        // An untagged speaker in the same thread is decided by the context.
        s.declared = None;
        assert!(matches!(
            decide(&s, german_context()),
            Intent::Arbitrate { .. }
        ));
    }

    #[test]
    fn a_row_the_arbiter_already_settled_is_not_re_opened() {
        for via in [lang_via::REDECODE, lang_via::MISMATCH] {
            let mut s = subject("i think that is the only way to do it");
            s.lang_via = Some(via.into());
            assert_eq!(decide(&s, german_context()), Intent::Nothing, "{via}");
        }
        // ...and a `context` stamp is not a settlement, it is an inference, so
        // a later real reading may still overrule it.
        let mut s = subject("i think that is the only way to do it");
        s.lang_via = Some(lang_via::CONTEXT.into());
        assert!(matches!(
            decide(&s, german_context()),
            Intent::Arbitrate { .. }
        ));
    }

    #[test]
    fn a_turn_with_no_thread_has_no_conversation_to_read_from() {
        let mut s = subject("okay");
        s.thread_id = None;
        // `decide` is given the context, so this is really about `intent_for`
        // refusing to look one up — but the shape has to be safe either way.
        assert_eq!(decide(&s, None), Intent::Nothing);
    }

    // ---- the same rule, over a real database -----------------------------

    const SEC: i64 = 1_000_000_000;

    /// A threaded German conversation of `n` turns plus one more turn whose
    /// text is `last`. Returns the store, the speaker and the last segment's id
    /// — which is the one every test below is deciding about.
    fn a_german_thread(n: usize, last: &str) -> (Store, i64, i64) {
        let store = Store::open_in_memory().unwrap();
        let src = store.upsert_source("VRChat.exe", "VRChat.exe", 1).unwrap();
        let sess = store.begin_session(src, 0).unwrap();
        let speaker = store.create_speaker("Ines", 1).unwrap();
        let graph = crate::config::GraphConfig::default();
        let mut last_id = 0;
        for i in 0..=n {
            let at = i as i64 * 5 * SEC;
            let id = store
                .insert_segment(sess, at, at + 3 * SEC, "x.wav", 0)
                .unwrap();
            store
                .set_segment_speaker(id, Some(speaker), Some(0.8))
                .unwrap();
            crate::threads::assign(&store, &graph, id).unwrap();
            let text = if i == n {
                last
            } else {
                "das ist der einzige weg"
            };
            store
                .set_segment_analysis(
                    id,
                    &crate::store::SegmentAnalysis {
                        text: Some(text.into()),
                        lang: lang::classify(text).tag().map(str::to_string),
                        lang_via: lang::classify(text)
                            .tag()
                            .map(|_| lang_via::CLASSIFIED.to_string()),
                        asr_model_id: Some("v3@1".into()),
                        overlap_frac: Some(0.02),
                    },
                )
                .unwrap();
            last_id = id;
        }
        (store, speaker, last_id)
    }

    #[test]
    fn the_prior_reads_a_real_thread_the_way_the_pure_rule_does() {
        // Four German turns then "okay": nothing votes in "okay", so it takes
        // the conversation's language.
        let (store, _, id) = a_german_thread(4, "okay");
        let (intent, ctx) = intent_for(&store, &cfg(), id).unwrap();
        assert_eq!(ctx.map(|c| c.lang), Some(Lang::De));
        assert_eq!(intent, Intent::Inherit(Lang::De));
        commit_inheritance(&store, id, Lang::De).unwrap();
        let f = store.segment_fields(id).unwrap();
        assert_eq!(f["lang"].as_deref(), Some("de"));
        assert_eq!(f["lang_via"].as_deref(), Some(lang_via::CONTEXT));
        assert_eq!(
            f["text"].as_deref(),
            Some("okay"),
            "the words are untouched"
        );
    }

    #[test]
    fn two_turns_are_not_yet_a_conversation_with_a_language() {
        let (store, _, id) = a_german_thread(2, "okay");
        // Two prior turns, and the bar is three.
        assert_eq!(intent_for(&store, &cfg(), id).unwrap().0, Intent::Nothing);
        let f = store.segment_fields(id).unwrap();
        assert_eq!(f["lang"], None, "nothing was inherited, so nothing changed");
    }

    #[test]
    fn an_english_turn_in_a_real_german_thread_is_arbitrated() {
        let (store, _, id) = a_german_thread(4, "i think that is the only way to do it");
        assert_eq!(
            intent_for(&store, &cfg(), id).unwrap().0,
            Intent::Arbitrate {
                want: Lang::De,
                read_as: Lang::En,
            }
        );
        // With no arbiter installed the words are kept and the row is marked —
        // which is the 0.6.1 behaviour, now reached by a conversation instead
        // of by a declaration.
        commit_arbitration(
            &store,
            id,
            Lang::De,
            Lang::En,
            crate::arbiter::Arbitration::Unavailable,
        )
        .unwrap();
        let f = store.segment_fields(id).unwrap();
        assert_eq!(f["lang"], None);
        assert_eq!(f["lang_via"].as_deref(), Some(lang_via::MISMATCH));
        assert_eq!(
            f["text"].as_deref(),
            Some("i think that is the only way to do it"),
            "the words are the only record of what was said"
        );
    }

    #[test]
    fn a_declaration_still_beats_the_conversation_over_a_real_database() {
        let (store, speaker, id) = a_german_thread(4, "i think that is the only way to do it");
        store
            .set_speaker_languages(speaker, Some(&["en".to_string()]))
            .unwrap();
        assert_eq!(
            intent_for(&store, &cfg(), id).unwrap().0,
            Intent::Nothing,
            "0.6.1 owns this row"
        );
        // Two languages is not a declaration anything can act on, so the
        // conversation gets it back.
        store
            .set_speaker_languages(speaker, Some(&["de".to_string(), "en".to_string()]))
            .unwrap();
        assert!(matches!(
            intent_for(&store, &cfg(), id).unwrap().0,
            Intent::Arbitrate { .. }
        ));
    }

    // ---- the backlog -----------------------------------------------------

    #[test]
    fn a_repair_with_no_arbiter_leaves_every_row_exactly_as_it_was() {
        // The honest failure. Nothing is installed, so nothing can be re-read,
        // and a walk that "finished" having silently cleared the marks would be
        // far worse than one that reports the truth.
        let dir = std::env::temp_dir().join(format!("nxr-repair-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (store, _, id) = a_german_thread(4, "i think that is the only way to do it");
        store.mark_segment_language_mismatch(id).unwrap();

        let models = crate::models::ModelSet::resolve_at(
            dir.join("no-models"),
            &crate::config::ModelsConfig::default(),
        );
        let mut arbiters = crate::arbiter::Arbiters::new(&models);
        assert!(arbiters.installed().is_empty());
        // The audio path exists in the row but not on disk, which is the other
        // way this can fail and must also be survivable.
        let report = repair(&store, &dir, &mut arbiters, &cfg(), 8, None, |_| {}).unwrap();
        assert_eq!(report.scanned, 1);
        assert_eq!(report.repaired, 0);
        assert_eq!(report.no_audio, 1);
        assert!(report.changed.is_empty());
        assert_eq!(
            store.segment_fields(id).unwrap()["lang_via"].as_deref(),
            Some(lang_via::MISMATCH),
            "still flagged, so a later run with an arbiter can come back to it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stale_mark_is_settled_without_touching_the_audio() {
        // The row was flagged when its speaker was declared English-only. The
        // declaration is gone, the conversation is German, and the transcript
        // reads as German: there is no disagreement left, so the mark is a lie
        // and the classifier's reading is restored.
        let dir = std::env::temp_dir().join(format!("nxr-settle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (store, _, id) = a_german_thread(4, "das ist wirklich so");
        store.mark_segment_language_mismatch(id).unwrap();
        assert_eq!(store.language_mismatch_counts().unwrap(), (1, 1));

        let models = crate::models::ModelSet::resolve_at(
            dir.join("no-models"),
            &crate::config::ModelsConfig::default(),
        );
        let mut arbiters = crate::arbiter::Arbiters::new(&models);
        let report = repair(&store, &dir, &mut arbiters, &cfg(), 8, None, |_| {}).unwrap();
        assert_eq!(report.settled, 1);
        assert_eq!(report.no_audio, 0, "no audio was needed to settle this");
        assert_eq!(report.changed, vec![id]);
        let f = store.segment_fields(id).unwrap();
        assert_eq!(f["lang"].as_deref(), Some("de"));
        assert_eq!(f["lang_via"].as_deref(), Some(lang_via::CLASSIFIED));
        assert_eq!(store.language_mismatch_counts().unwrap(), (0, 0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_row_nothing_can_decide_is_passed_over_rather_than_walked_forever() {
        // No declaration and no conversation: the work list is a query, so a
        // row the walk cannot decide would come straight back on the next
        // batch. It has to be remembered as done for the length of the run.
        let dir = std::env::temp_dir().join(format!("nxr-undecidable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // One turn, so the thread never gets a context.
        let (store, _, id) = a_german_thread(0, "i think that is the only way to do it");
        store.mark_segment_language_mismatch(id).unwrap();

        let models = crate::models::ModelSet::resolve_at(
            dir.join("no-models"),
            &crate::config::ModelsConfig::default(),
        );
        let mut arbiters = crate::arbiter::Arbiters::new(&models);
        let mut batches = 0;
        let report = repair(&store, &dir, &mut arbiters, &cfg(), 1, None, |_| {
            batches += 1
        })
        .unwrap();
        assert_eq!(report.scanned, 1);
        assert_eq!(report.undecidable, 1);
        assert_eq!(batches, 1, "one pass, not an infinite one");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_limit_bounds_the_walk_and_the_rest_waits_for_the_next_run() {
        let dir = std::env::temp_dir().join(format!("nxr-bounded-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open_in_memory().unwrap();
        let src = store.upsert_source("VRChat.exe", "VRChat.exe", 1).unwrap();
        let sess = store.begin_session(src, 0).unwrap();
        let mut ids = Vec::new();
        for i in 0..5 {
            let at = i * 5 * SEC;
            let id = store
                .insert_segment(sess, at, at + 3 * SEC, "x.wav", 0)
                .unwrap();
            store.mark_segment_language_mismatch(id).unwrap();
            ids.push(id);
        }
        let models = crate::models::ModelSet::resolve_at(
            dir.join("no-models"),
            &crate::config::ModelsConfig::default(),
        );
        let mut arbiters = crate::arbiter::Arbiters::new(&models);
        let report = repair(&store, &dir, &mut arbiters, &cfg(), 2, Some(3), |_| {}).unwrap();
        assert_eq!(report.scanned, 3, "the limit is a limit, not a suggestion");
        assert_eq!(store.language_mismatch_counts().unwrap().0, 5);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
