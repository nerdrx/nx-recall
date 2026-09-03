//! Japanese, Korean and Chinese: decoders that speak them, and the rule for
//! reaching for one (0.11.0 for `ja`, 0.11.6 for `ko` and `zh`).
//!
//! ## The failure
//!
//! Parakeet-TDT-0.6b-v3 covers 25 European languages. None of these three is
//! among them, and the model does not say so — it transliterates. The user's
//! own evening, verbatim: "sumimasen, ogenki desu ka" came back as
//!
//! > Sima Sen Okenki Deska.
//!
//! Every correction this daemon had before 0.11.0 reads text.
//! [`crate::lang::classify`] sees Latin words with no German stopwords and
//! says `Unclear`. [`crate::lang::guess_other`]'s script rule needs kana,
//! hangul or Han and never sees one. The per-speaker tags
//! ([`crate::analysis::Analyzer::correct_language`]) cannot route it either,
//! because the user speaks Japanese *themselves*, mid-evening, between German
//! and English turns — a tag is a fact about a voice and this is a fact about a
//! turn. So the decision comes from the audio ([`crate::lid`]) and the fix is a
//! second decoder.
//!
//! 0.11.0 shipped that for Japanese and wrote down what it did not cover
//! (FINDINGS §24): a Korean or Chinese speaker in the same lobby got
//! *precisely* the failure the round had just fixed. 0.11.6 closes it, and the
//! shape of the close was decided by a bench rather than by symmetry.
//!
//! ## Two decoders, not one — and that is a measurement
//!
//! The obvious move was one model for all three: `sherpa-onnx-sense-voice-zh-
//! en-ja-ko-yue-2024-07-17` speaks every language in this module's name, in
//! 239 MB of int8 graph against the Japanese Parakeet's 655 MB. The rule going
//! in was "one decoder if SenseVoice is within 2 CER points of the Parakeet on
//! Japanese at 3 s". Measured (`spike/asr_cjk.py`, 200 FLEURS utterances per
//! language, 4 cores, FINDINGS §27):
//!
//! | decoder                       | lang | full  | 3.0 s | RTF (3 s) |
//! |-------------------------------|------|------:|------:|----------:|
//! | ja-parakeet-tdt_ctc-0.6b int8 | ja   |  7.5% | 11.3% |     0.026 |
//! | sense-voice-small int8        | ja   |  7.6% | 15.3% |     0.014 |
//! | sense-voice-small int8        | ko   |  9.2% |  9.6% |     0.012 |
//! | sense-voice-small int8        | zh   | 10.7% |  9.6% |     0.012 |
//!
//! On whole utterances the two are level (7.6 against 7.5). On the **3 s
//! fragment this daemon actually lives in** — median turn 2.4–2.7 s, §10 —
//! SenseVoice loses 4.0 points, twice the bar. So rule (b) fired: the Japanese
//! Parakeet keeps Japanese and SenseVoice is catalogued for Korean and Chinese,
//! where it has no competition in the zoo at all (there is no Korean Parakeet;
//! `…-0.6b-ko-3000-int8` is a 404).
//!
//! The asymmetry is worth naming rather than smoothing over: SenseVoice is a
//! model that *uses context*, and a VRChat turn does not have any. Its ko and
//! zh rows barely move between full and 3 s (9.2→9.6, 10.7→9.6) while its
//! Japanese collapses, which is the same shape whisper-large-v3 showed in §23
//! and was rejected for.
//!
//! ## The identifier, on three targets
//!
//! [`crate::lid`] was measured again on all five languages (`spike/lid_cjk.py`,
//! 200 FLEURS utterances each, FINDINGS §27). The gate that mattered in §22 was
//! false positives *into* the target, because a German turn handed to a decoder
//! that does not speak German is a new kind of wrong; adding two targets adds
//! two more ways for that to happen.
//!
//! | length | ja    | ko    | zh     | de→CJK | en→CJK |
//! |--------|------:|------:|-------:|-------:|-------:|
//! | full   | 100%  | 100%  | 100%   |   0.0% |   0.0% |
//! | 3.0 s  | 96.5% | 97.5% | 100.0% |   0.0% |   0.0% |
//!
//! **Zero of 400 German and English utterances were heard as any of the three,
//! at either length.** The confusion that was expected — ja↔zh, which share a
//! script and much of a vocabulary — did not materialise either: at 3 s, 0.5%
//! of Japanese was heard as Chinese and none of the Chinese as Japanese. What
//! little cross-talk there is runs ja↔ko (1.5% and 2.0%), and [`judge`] makes
//! it free: see below.
//!
//! ## The shape of the route, and why it is the arbiter's
//!
//! [`crate::arbiter`] already solved "decode this turn again with a different
//! model, and only keep the answer if it earns it", and every guard it
//! measured applies here for the same reasons. This module reuses that shape
//! rather than inventing a second one:
//!
//! 1. **A duration floor.** Below `[lang].arbiter_min_duration_s` the model is
//!    not run at all.
//! 2. **Run it, then judge.** The replacement must be non-empty and must read
//!    as one of the languages the decoder that produced it actually speaks —
//!    which for these three is a *script* test and not a vote, so it is exact:
//!    [`crate::lang::guess_other`] returns `ja` on two kana, `ko` on two
//!    hangul, `zh` on Han dominance. The arbiter's `arbiter_min_words` floor is
//!    the one guard that does *not* carry over, because two of these three
//!    languages are written without spaces and have no word count — see
//!    [`judge`], which replaces it with something stricter.
//!
//! Guard 2 is the reason a false positive from the identifier is cheap. If LID
//! is wrong and a German turn reaches a decoder here, the decoder produces CJK
//! script over German audio or produces nothing, and either way the judge
//! throws it away and the original transcript stands.
//!
//! ## The script decides the stamp, not the identifier
//!
//! [`judge`] takes the tag off the *text* and not off the reading that routed
//! the turn, and that is the one place this module deliberately trusts
//! something other than LID. The reason is a property of SenseVoice measured
//! in the same bench: forcing its `language` makes **no difference to what it
//! writes** — a Japanese clip decoded with `language = "ko"` still comes back
//! in kana. So on the 1.5% of Japanese turns the identifier hands to the
//! Korean arm, reading the script recovers a correct transcript and a correct
//! `ja` stamp where trusting the reading would have thrown both away. The
//! identifier still chooses the *decoder*; the writing system chooses the tag,
//! and it cannot be wrong about a sentence in a script only one of them uses.
//!
//! ## What this got wrong, and the three guards that answer it (0.12.0)
//!
//! Everything above is true and none of it was enough. Measured on the user's
//! own database a day after 0.12.0 (FINDINGS §31), this route had rewritten
//! **45 archive rows** and **two of them are right**. 37 belong to one voice:
//! the user's own microphone, declared `["de","en"]`, saying "Mm-hmm." and
//! getting `うん` back.
//!
//! Each of the three failures is a place where a rule that is correct about
//! FLEURS is wrong about a room:
//!
//! 1. [`pre_route`] short-circuited only on a **sole** declared language, so a
//!    voice that declared two fell through to the identifier on every
//!    unreadable turn. A person who names their languages has answered the
//!    question — two answers are still an answer.
//! 2. A back-channel is `Unclear` to every text rule in this daemon, so "Uh"
//!    and "Okay, yeah." reached the identifier all evening. Re-decoding a grunt
//!    has no value even when the language is right; see [`MIN_CONTENT_WORDS`].
//! 3. The script test in [`judge`] is exact and it is not *evidence*: `うん`,
//!    `没` and `フフフフフフフ` are all written in a script only these three
//!    languages use. See [`weak_output`].
//!
//! And [`crate::lid`]'s window vote became the live default on the same data:
//! one window keeps seven false positives on these rows where three keeps one.
//! That is affordable precisely because guards 1 and 2 took the turns asked
//! about at all from 245 to one.
//!
//! `crate::unroute` is what happens to the rows written before any of this.
//!
//! ## The tags SenseVoice emits
//!
//! SenseVoice does not produce a transcript, it produces a transcript wrapped
//! in metadata: language, emotion (`<|HAPPY|>`, `<|NEUTRAL|>`), audio event
//! (`<|Speech|>`, `<|BGM|>`) and whether inverse text normalisation ran
//! (`<|woitn|>`). At sherpa-onnx 0.6.8's C API these land in the *result
//! struct's* own `lang`/`emotion`/`event` fields and `text` comes back clean —
//! measured, 0.0% of 1200 decodes carried a tag in the text. [`strip_tags`]
//! runs anyway, before anything reads the string, because the cost is one
//! string scan and the cost of being wrong is a transcript beginning with a
//! Latin `<|ja|>` that [`crate::lang::classify`] would read as English — the
//! exact class of undetectable failure this whole module exists to remove.

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use crate::asr::normalise_words;
use crate::config::{AsrConfig, LangConfig, SAMPLE_RATE};
use crate::lang;
use crate::lid::Reading;
use crate::models::{CjkModel, ModelSet};
use crate::store::Store;

/// The three tags this module is about, in the order the catalogue lists them.
pub const JA: &str = "ja";
pub const KO: &str = "ko";
pub const ZH: &str = "zh";

/// Every language the router will route, as a set to test membership against.
///
/// Deliberately *not* [`crate::lang::KNOWN`]: that is which languages a voice
/// may be tagged with and it also contains `de` and `en`, which the European
/// decoder already handles and which this module must never take a turn from.
pub const CJK: &[&str] = &[JA, KO, ZH];

/// `segments.lang_via` for a row whose language was decided by listening to
/// the audio rather than by reading the words (0.11.0).
///
/// A sixth value alongside the five in [`crate::store::lang_via`], and it has
/// to be its own: `re-decode` means "the text disagreed with a declaration",
/// `classified` means "the words said so", and neither is true here. The words
/// said nothing — they *could* not, they were Latin nonsense — and what
/// settled it was a model that listened.
pub const LANG_VIA_LID: &str = "lid";

/// `lang` as a `&'static str`, so a route can carry one without allocating and
/// a stamp can never be a typo. `None` for anything this module does not route.
fn canonical(tag: &str) -> Option<&'static str> {
    CJK.iter().copied().find(|t| *t == tag)
}

// ---------------------------------------------------------------------------
// which decoder speaks which language
// ---------------------------------------------------------------------------

/// The two decoders this module can reach for.
///
/// Two rather than one because the bench said so (see the module note): the
/// Japanese Parakeet is 4.0 CER points better than SenseVoice on Japanese at
/// the length a lobby speaks in, and SenseVoice is the only thing in the zoo
/// that speaks Korean at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decoder {
    /// `sherpa-onnx-nemo-parakeet-tdt_ctc-0.6b-ja-35000-int8`, `nemo_ctc`.
    /// Japanese and nothing else — a fact about the weights.
    Japanese,
    /// `sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17`, `sense_voice`.
    /// Catalogued for Korean and Chinese; it also writes Japanese, which
    /// [`judge`] accepts and the router never asks for.
    SenseVoice,
}

impl Decoder {
    /// Which decoder a language is catalogued for.
    pub fn for_lang(lang: &str) -> Option<Self> {
        match lang {
            JA => Some(Decoder::Japanese),
            KO | ZH => Some(Decoder::SenseVoice),
            _ => None,
        }
    }

    /// Every language this decoder may legitimately have written, which is what
    /// [`judge`] accepts and nothing wider.
    ///
    /// SenseVoice's list contains `ja` even though nothing routes Japanese to
    /// it: it is the set of things the weights can produce, and accepting a
    /// correct Japanese transcript from the Korean arm is the point of reading
    /// the script rather than the reading (see the module note). The Parakeet's
    /// list is one long because the model cannot produce anything else.
    pub fn writes(self) -> &'static [&'static str] {
        match self {
            Decoder::Japanese => &[JA],
            Decoder::SenseVoice => CJK,
        }
    }
}

// ---------------------------------------------------------------------------
// the routing decision, as pure functions
// ---------------------------------------------------------------------------

/// What to do about a turn *before* any model has run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pre {
    /// The speaker is declared one of these languages and nothing else. Decode
    /// with that language's model straight away; the identifier is not
    /// consulted, because a declaration outranks a reading of one turn's audio.
    Direct(&'static str),
    /// The transcript is unreadable in the specific way a transliterated CJK
    /// turn is unreadable. Worth the cost of asking the identifier.
    AskLid,
    /// Nothing to do, and — this is the point of separating the two steps —
    /// **no LID call**. A clear German transcript is a clear German
    /// transcript; paying 20 ms to be told so on every turn of every evening
    /// is the cost this split exists to avoid.
    Nothing,
}

/// Content words a transcript must have before the identifier is asked about
/// it at all (0.12.0, FINDINGS §31).
///
/// **Two**, and the number is bounded from both ends by measurement rather than
/// chosen. From below: 22 of the 45 rows the route wrongly rewrote on this
/// install had **zero** content words — "Mm-hmm.", "Uh", "Okay, yeah." — and
/// another six had one. From above: the three rows this whole feature exists
/// for are "Sima Sen Okenki Deska." (4), "Wanky Daska." (2) and "During
/// apartments." (2), so three would have thrown away the founding cases. Two is
/// the only value that removes the back-channels and keeps them.
///
/// It is also the bar [`crate::lang::word_count`] already sets for minting a
/// voice, for the same reason written down in DESIGN §5: a grunt is not a
/// voice, and — this round's addition — a grunt is not a sentence in another
/// language either.
pub const MIN_CONTENT_WORDS: usize = 2;

/// Does *anything* in this daemon re-decode `tag` off a reading of the audio?
///
/// The union of the two audio routes, because [`pre_route`] gates both of them:
/// [`crate::asr_cjk`]'s three, and whatever `[asr].polyglot_languages` has
/// turned on for [`crate::polyglot`]. A caller that asked only about this
/// module's three would let a turn through to LID that only the other route
/// could ever act on — and, worse, would refuse one it could.
fn routed_anywhere(tag: &str, cfg: &AsrConfig) -> bool {
    CJK.contains(&tag) || crate::polyglot::routable(tag, cfg)
}

/// Step one: is this turn worth asking about, and does it even need asking?
///
/// `declared` is the speaker's language tags, `text` the transcript the live
/// decoder produced, `cfg` the routes' config — read for
/// `[asr].polyglot_languages`, which is half of what "a language we could act
/// on" means (see [`routed_anywhere`]).
///
/// The rule, in order:
///
/// 1. A speaker pinned to **exactly** one of ja/ko/zh goes straight to that
///    decoder. A bilingual voice does not: a Japanese speaker's English turn is
///    not a mistake, which is the same reasoning [`crate::lang::sole_language`]
///    already encodes.
/// 2. A speaker pinned to exactly *something else* is that other feature's
///    business ([`crate::analysis::Analyzer::correct_language`]) and is left
///    alone. Deciding a row twice is how two features start fighting over one
///    column.
/// 3. **A declared set containing nothing either route can decode is a
///    declaration** (0.12.0). This is rule 2 stated for the case it was
///    written too narrowly for, and the gap was expensive: the user's own
///    microphone voice is declared `["de", "en"]`, `sole_language` says `None`
///    of two tags, and every unreadable grunt from it fell through to `AskLid`.
///    37 of the 45 rows the route wrongly rewrote on this install are that
///    voice (FINDINGS §31). A person who names their languages has answered the
///    question this route exists to ask; two answers are still an answer.
/// 4. A transcript that already reads as **something** — German, English, or
///    any script or stopword majority [`crate::lang::guess_other`] is
///    confident about, these three included — is not the transliteration
///    failure. Nothing to do.
/// 5. **A turn that is nothing but back-channel is not worth a decoder**
///    (0.12.0). Fewer than [`MIN_CONTENT_WORDS`] words that are not in
///    [`crate::lang::FILLERS`] and the row is left alone: re-decoding a grunt
///    has no value even when the language is right, and "Mm-hmm." is
///    `Unclear` to every text rule this daemon has, so without this guard it
///    reaches the identifier on every single turn of every evening.
/// 6. What is left is `Unclear` or `Empty` with real words behind it, or **no
///    words at all**. Both are what a CJK turn looks like coming out of a
///    decoder that cannot spell it — an empty transcript deliberately still
///    asks, because a decoder that gave up entirely is exactly the turn worth
///    re-reading, and the two genuine Japanese rows on this install
///    ("すいません", "聞いてみますかねちょっと") are both of that shape.
pub fn pre_route(declared: Option<&Vec<String>>, text: Option<&str>, cfg: &AsrConfig) -> Pre {
    if let Some(sole) = lang::sole_language(declared) {
        return match canonical(sole) {
            Some(tag) => Pre::Direct(tag),
            None => Pre::Nothing,
        };
    }
    // Rule 3. `parse_languages` normalises an empty array to `None`, so a
    // `Some` here is always a real declaration.
    if declared.is_some_and(|tags| !tags.iter().any(|t| routed_anywhere(t, cfg))) {
        return Pre::Nothing;
    }
    let text = text.unwrap_or("");
    // No words at all is a fair question for the identifier — the decode that
    // produced nothing is exactly the turn worth re-reading — but an empty
    // *buffer* is not, and the caller's duration floor catches that.
    if !text.trim().is_empty() {
        match lang::classify(text) {
            lang::Lang::De | lang::Lang::En => return Pre::Nothing,
            // A confident third-language guess is a real reading of the words.
            // That includes these three: kana, hangul or Han in the transcript
            // means some decoder already got it right and there is nothing to
            // fix.
            _ if lang::guess_other(text).is_some_and(|g| g.confident) => return Pre::Nothing,
            _ => {}
        }
        // Rule 5.
        if lang::content_word_count(text) < MIN_CONTENT_WORDS {
            return Pre::Nothing;
        }
    }
    Pre::AskLid
}

/// Step two: the identifier has answered. Is that answer good enough to spend
/// a decode on, and on which decoder?
///
/// `Some(tag)` is a language this module routes and a reading that cleared the
/// operating point; `None` is everything else, which is nearly every turn.
///
/// `lid_min_confidence` is the operating point. With the shipped
/// `[asr].lid_windows = 1` the confidence is always 1.0 and this is a "did it
/// name one of the three at all" test — which is what the measurement supports,
/// because nothing in 400 German and English utterances was heard as any of
/// them at any length (`crate::lid`). The knob is what a machine that hears
/// something that corpus did not can turn.
pub fn post_route(reading: Option<&Reading>, cfg: &AsrConfig) -> Option<&'static str> {
    let r = reading?;
    if r.confidence < cfg.lid_min_confidence {
        return None;
    }
    canonical(&r.lang)
}

/// What a re-decode did, or why it did not.
#[derive(Debug, Clone, PartialEq)]
pub enum Rerouted {
    /// The decoder's words replaced the row's.
    Replaced {
        text: String,
        model_id: String,
        /// What the *text* turned out to be, which is what the row is stamped
        /// with — not necessarily what the identifier said. See the module
        /// note.
        lang: &'static str,
    },
    /// Under `[lang].arbiter_min_duration_s`. The model was not run: at this
    /// length its answer is not better than the one it would overwrite.
    TooShort,
    /// No decoder for this language is installed, or its switch is off. Said
    /// once per daemon, not per turn.
    Unavailable,
    /// It ran and the answer failed a guard — empty, in a script none of the
    /// languages this decoder speaks is written in, or (0.12.0) not enough of
    /// an answer to be evidence of anything.
    Rejected {
        words: usize,
        script: Option<String>,
        /// Which guard, in words, for the log line and for a test that wants to
        /// assert *why* rather than merely that.
        why: &'static str,
    },
}

/// Everything between `<|` and `|>` — SenseVoice's language, emotion, event and
/// ITN tags — and the whitespace they leave behind.
///
/// Not anchored to the start of the string on purpose: the tags are a prefix
/// today, and a rule that assumes the position is a rule that breaks on the
/// first release that appends an event tag at the end. Costs one scan and runs
/// on both decoders' output, because a guard that only runs on the model you
/// remembered to guard is not a guard.
pub fn strip_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find("<|") {
        match rest[open..].find("|>") {
            Some(close) => {
                out.push_str(&rest[..open]);
                rest = &rest[open + close + 2..];
            }
            // An unterminated `<|` is text, not a tag.
            None => break,
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// The replacement guards, as a pure function over what the decoder said.
///
/// Split out from the model call for exactly the reason
/// [`crate::arbiter::judge`] is: the whole rule can then be exercised without a
/// 655 MB model, and there is one copy of it.
///
/// The script test is the load-bearing one and it is *cheap and exact*, which
/// is unusual — these three are the languages in this daemon whose identity is
/// settled by their writing system rather than by a stopword vote. Two kana,
/// two hangul, or a Han majority: the same bars [`crate::lang::guess_other`]
/// uses, so one borrowed word is not a language.
///
/// Returns the tag the *text* carries, which must be one of
/// [`Decoder::writes`]. That is how a Japanese turn the identifier handed to
/// the Korean arm still lands correctly stamped — and it is also why a German
/// turn that reaches either decoder is rejected: whatever mojibake comes back,
/// it is not written in kana, hangul or Han.
///
/// Deliberately **without** the arbiter's `arbiter_min_words` bar, and that is
/// not an oversight: [`crate::asr::normalise_words`] splits on whitespace and
/// Japanese and Chinese have none, so a whole sentence is one "word" and a
/// two-word floor would reject every correct answer those decoders can give.
/// The bar it replaces the word count with is stricter, not looser.
///
/// ## The script test is necessary and it is not sufficient (0.12.0)
///
/// Everything above was written when the only thing the script test had to rule
/// out was a German transcript, and against that it is exact. What it is not is
/// *evidence*, and the 45 rows this route rewrote on a real install
/// (FINDINGS §31) are what that distinction costs: `うん`, `没`, `嗯嗯`,
/// `龙龙龙龙` and `ok看嗯` are all written in a script only the target languages
/// use, so all of them won. A back-channel decoded as a back-channel in the
/// wrong language is a wrong transcript with a perfect script test behind it.
///
/// So four more tests, all on the decoder's own output and all measured on
/// those rows:
///
/// * **[`MIN_OUTPUT_CHARS`] letters, and at least [`MIN_CHARS_PER_S`] of them
///   per second of audio.** A 5.3 s turn that comes back as two characters did
///   not have five seconds of Japanese in it.
/// * **Not a repetition loop** — [`MIN_DISTINCT_SHARE`] of the letters must be
///   distinct. `フフフフフフフ` and `没没没不是说说说说…` are what these decoders
///   do with noise.
/// * **Not pure interjection** in the target script ([`INTERJECTIONS`]): the
///   same rule [`crate::lang::FILLERS`] applies to the *input*, applied to the
///   output, because a route that turns "Mm-hmm." into `うん` has translated a
///   grunt rather than recovered a sentence.
/// * **One writing system, and not mostly Latin.** `오빠どなか` is hangul and
///   kana in five characters and is not a sentence in either; `そ be丈夫` and
///   `ok看嗯` are the same failure with the Latin alphabet.
///
/// There is no decoder confidence to lean on instead: sherpa's offline result
/// struct carries `text`, `lang`, `emotion` and `event` and no score at all —
/// the same absence [`crate::lid`] works around with a window vote.
pub fn judge(
    raw: &str,
    decoder: Decoder,
    duration_s: f32,
) -> Result<(String, &'static str), Rerouted> {
    // The tags first: everything after this reads a string, and a `<|ja|>` left
    // on the front is Latin text in front of a language test.
    let untagged = strip_tags(raw);
    // The same caption strip the German arbiter runs. Cheap, and neither
    // decoder has been measured for a hallucination habit — running the filter
    // that catches one costs a string scan and assuming its absence costs a
    // fabricated transcript.
    let text = crate::arbiter::strip_captions(&untagged);
    let script = lang::guess_other(&text).map(|g| g.tag);
    // Reported, not tested against a floor — see the note above. It is in the
    // rejection so a log line can say what came back instead.
    let words = normalise_words(&text).len();
    let reject = |why: &'static str| {
        Err(Rerouted::Rejected {
            words,
            script: script.map(str::to_string),
            why,
        })
    };
    let Some(tag) = script.filter(|tag| decoder.writes().contains(tag)) else {
        return reject("not a script this decoder writes");
    };
    if text.trim().is_empty() {
        return reject("nothing came back");
    }
    if let Some(why) = weak_output(&text, duration_s) {
        return reject(why);
    }
    Ok((text, tag))
}

/// Letters a re-decode must carry before it is evidence of anything.
///
/// **Four.** Of the 45 wrongly rewritten rows, 17 are under it — every `うん`,
/// `没`, `嗯嗯`, `啊 嗯`, `哎呀` and `あっ` on the list — and the two that look
/// genuine (`すいません`, 5, and `聞いてみますかねちょっと`, 12) are both clear of
/// it. It is an absolute floor under the rate below, so that a 1.5 s turn
/// cannot buy its way past on brevity.
pub const MIN_OUTPUT_CHARS: usize = 4;

/// …and per second of the audio it claims to be a transcript of.
///
/// **1.0.** Japanese and Chinese are written at 5–8 characters a second in the
/// rows here that are right; the floor is set five times lower than that
/// because it is guarding against *silence being transcribed*, not against
/// terseness. It is what catches the long ones the absolute floor cannot:
/// `うん` over 5.26 s, `そうしました` over 6.67 s.
pub const MIN_CHARS_PER_S: f32 = 1.0;

/// Share of a re-decode's letters that must be distinct, at
/// [`MIN_OUTPUT_CHARS`] or more.
///
/// **Above one half.** A decoder given noise in a language it speaks emits the
/// same character over and over — `フフフフフフフ` (1 distinct in 7),
/// `没没没不是说说说说说说说说说说说没没没没没` (4 in 21), `あまたタ待タ待タ待タ`
/// (5 in 10) — and a real sentence essentially never does. Measured on these
/// rows the rule costs nothing: both genuine transcripts are 100% distinct.
pub const MIN_DISTINCT_SHARE: f32 = 0.5;

/// …and the share of letters that may be Latin before the answer reads as two
/// decoders arguing rather than one transcript.
///
/// **One third.** Japanese does write Latin — `PC`, `OK` — so this is a
/// dominance test and not a presence one, the same shape and the same reasoning
/// as [`crate::lang::guess_other`]'s script rule.
pub const MAX_LATIN_SHARE: f32 = 1.0 / 3.0;

/// Back-channels in the three target scripts: the output-side twin of
/// [`crate::lang::FILLERS`], and built the same way — off the transcripts the
/// route actually produced.
///
/// Matched whole, over the whole output with its punctuation and spaces
/// removed, so `うん` is refused and `うんそれ` is not.
pub const INTERJECTIONS: &[&str] = &[
    "ん",
    "うん",
    "ううん",
    "うーん",
    "うんうん",
    "ええ",
    "えー",
    "えっ",
    "あっ",
    "あー",
    "あぁ",
    "おー",
    "おお",
    "はー",
    "ふー",
    "へー",
    "嗯",
    "嗯嗯",
    "嗯嗯嗯",
    "啊",
    "啊啊",
    "哦",
    "呃",
    "哎",
    "哎呀",
    "唉",
    "诶",
    "어",
    "음",
    "아",
    "으음",
];

/// Why this re-decode is not evidence, or `None` when it is.
///
/// Pure and separate from [`judge`] for [`judge`]'s own reason: the whole rule
/// can then be run over a table of strings — which is exactly what
/// `examples/lang_route_bench.rs` does over this install's 45 rows — without a
/// decoder in the process.
pub fn weak_output(text: &str, duration_s: f32) -> Option<&'static str> {
    let letters: Vec<char> = text.chars().filter(|c| c.is_alphabetic()).collect();
    let n = letters.len();
    if n < MIN_OUTPUT_CHARS {
        return Some("too few characters to be a sentence");
    }
    if (n as f32) < MIN_CHARS_PER_S * duration_s.max(0.0) {
        return Some("too little text for the length of the audio");
    }
    let distinct = {
        let mut seen: Vec<char> = letters.clone();
        seen.sort_unstable();
        seen.dedup();
        seen.len()
    };
    if (distinct as f32) <= MIN_DISTINCT_SHARE * n as f32 {
        return Some("a repetition loop");
    }
    let bare: String = letters.iter().collect();
    if INTERJECTIONS.contains(&bare.as_str()) {
        return Some("an interjection, in the right script");
    }
    let latin = letters
        .iter()
        .filter(|c| matches!(**c as u32, 0x41..=0x5A | 0x61..=0x7A | 0xC0..=0x24F))
        .count();
    if (latin as f32) > MAX_LATIN_SHARE * n as f32 {
        return Some("mostly Latin letters");
    }
    let (kana, hangul) = letters
        .iter()
        .fold((0usize, 0usize), |(k, h), c| match *c as u32 {
            0x3040..=0x30FF | 0x31F0..=0x31FF | 0xFF66..=0xFF9D => (k + 1, h),
            0x1100..=0x11FF | 0x3130..=0x318F | 0xAC00..=0xD7A3 => (k, h + 1),
            _ => (k, h),
        });
    if kana > 0 && hangul > 0 {
        return Some("two writing systems at once");
    }
    None
}

// ---------------------------------------------------------------------------
// the decoders
// ---------------------------------------------------------------------------

/// An offline sherpa recognizer, loaded on demand and resident from then on.
///
/// One type for both decoders because the only thing that differs between them
/// is which sub-config of `SherpaOnnxOfflineModelConfig` is filled in — and
/// filling in the wrong one does not fail to compile and does not fail to
/// load, it produces empty strings or garbage (see `crate::asr`).
pub struct CjkAsr {
    recognizer: *const sherpa_rs::sherpa_rs_sys::SherpaOnnxOfflineRecognizer,
    model_id: String,
    decoder: Decoder,
}

// The recognizer is used behind `&mut` from one thread at a time — the same
// assumption, and the same justification, as `crate::asr::TimedAsr`.
unsafe impl Send for CjkAsr {}

impl CjkAsr {
    pub fn load(model: &CjkModel, threads: i32) -> Result<Self> {
        use sherpa_rs::sherpa_rs_sys as sys;
        use std::ffi::CString;

        let cstr = |p: &std::path::Path| -> Result<CString> {
            let s = p
                .to_str()
                .with_context(|| format!("model path {} is not valid UTF-8", p.display()))?;
            Ok(CString::new(s)?)
        };
        let graph = cstr(&model.model)?;
        let tokens = cstr(&model.tokens)?;
        let decoder = model.export.decoder;
        let model_type = CString::new(match decoder {
            Decoder::Japanese => "nemo_ctc",
            Decoder::SenseVoice => "sense_voice",
        })?;
        let decoding = CString::new("greedy_search")?;
        let provider = CString::new("cpu")?;
        let empty = CString::new("")?;

        // Zeroed then filled, exactly as `TimedAsr` does and for the same
        // reason: the C config carries a dozen model sub-configs this daemon
        // never uses, and NULL is what "not this one" means for each of them.
        let recognizer = unsafe {
            let mut cfg: sys::SherpaOnnxOfflineRecognizerConfig = std::mem::zeroed();
            match decoder {
                Decoder::Japanese => cfg.model_config.nemo_ctc.model = graph.as_ptr(),
                Decoder::SenseVoice => {
                    cfg.model_config.sense_voice.model = graph.as_ptr();
                    // Auto, and that is measured rather than lazy: forcing the
                    // language changes nothing about what SenseVoice writes —
                    // a Japanese clip decoded as `ko` still comes back in kana
                    // (`spike/asr_cjk.py`) — so a forced tag would only be a
                    // second opinion nobody reads. `judge` reads the script.
                    cfg.model_config.sense_voice.language = empty.as_ptr();
                    // Inverse text normalisation off: it rewrites numbers and
                    // punctuation into a display form, and every reader
                    // downstream of here — the classifier, the embedder, the
                    // glossary — was measured on the plain form.
                    cfg.model_config.sense_voice.use_itn = 0;
                }
            }
            cfg.model_config.tokens = tokens.as_ptr();
            cfg.model_config.model_type = model_type.as_ptr();
            cfg.model_config.provider = provider.as_ptr();
            cfg.model_config.modeling_unit = empty.as_ptr();
            cfg.model_config.bpe_vocab = empty.as_ptr();
            cfg.model_config.num_threads = threads.max(1);
            cfg.model_config.debug = 0;
            cfg.feat_config.sample_rate = SAMPLE_RATE as i32;
            cfg.feat_config.feature_dim = 80;
            cfg.decoding_method = decoding.as_ptr();
            cfg.hotwords_file = empty.as_ptr();
            cfg.rule_fsts = empty.as_ptr();
            cfg.rule_fars = empty.as_ptr();
            sys::SherpaOnnxCreateOfflineRecognizer(&cfg)
        };
        if recognizer.is_null() {
            anyhow::bail!("loading the {} decoder failed", model.export.dir);
        }
        Ok(Self {
            recognizer,
            model_id: model.model_id(),
            decoder,
        })
    }

    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    pub fn decoder(&self) -> Decoder {
        self.decoder
    }

    pub fn transcribe(&mut self, samples: &[f32]) -> String {
        use sherpa_rs::sherpa_rs_sys as sys;

        if samples.is_empty() {
            return String::new();
        }
        unsafe {
            let stream = sys::SherpaOnnxCreateOfflineStream(self.recognizer);
            sys::SherpaOnnxAcceptWaveformOffline(
                stream,
                SAMPLE_RATE as i32,
                samples.as_ptr(),
                samples.len() as i32,
            );
            sys::SherpaOnnxDecodeOfflineStream(self.recognizer, stream);
            let result = sys::SherpaOnnxGetOfflineStreamResult(stream);
            let raw = result.read();
            let text = if raw.text.is_null() {
                String::new()
            } else {
                std::ffi::CStr::from_ptr(raw.text)
                    .to_string_lossy()
                    .trim()
                    .to_string()
            };
            sys::SherpaOnnxDestroyOfflineRecognizerResult(result);
            sys::SherpaOnnxDestroyOfflineStream(stream);
            text
        }
    }
}

impl Drop for CjkAsr {
    fn drop(&mut self) {
        unsafe {
            sherpa_rs::sherpa_rs_sys::SherpaOnnxDestroyOfflineRecognizer(self.recognizer);
        }
    }
}

// ---------------------------------------------------------------------------
// the router
// ---------------------------------------------------------------------------

/// One lazily loaded optional model, and the difference between "not tried
/// yet" and "tried, not installed".
///
/// The distinction is not pedantry — it is the reason
/// [`crate::arbiter::Arbiters`] has the same pair of fields: a missing optional
/// model must cost **one** warning line for the life of the daemon rather than
/// one per turn.
struct Lazy<T> {
    got: Option<T>,
    unavailable: bool,
}

// Hand-written rather than derived: `#[derive(Default)]` on a generic struct
// asks `T: Default`, and neither a loaded recognizer nor a loaded identifier
// has a default — the whole point of this type is that it starts empty.
impl<T> Default for Lazy<T> {
    fn default() -> Self {
        Self {
            got: None,
            unavailable: false,
        }
    }
}

impl<T> Lazy<T> {
    fn get_or_load(
        &mut self,
        present: bool,
        how_to_get_it: impl FnOnce() -> String,
        load: impl FnOnce() -> Result<T>,
        loaded: impl FnOnce(&T),
    ) -> Option<&mut T> {
        if self.got.is_none() && !self.unavailable {
            if !present {
                self.unavailable = true;
                warn!("{}", how_to_get_it());
            } else {
                match load() {
                    Ok(v) => {
                        loaded(&v);
                        self.got = Some(v);
                    }
                    Err(e) => {
                        self.unavailable = true;
                        warn!("could not load an optional CJK model: {e:#}");
                    }
                }
            }
        }
        self.got.as_mut()
    }
}

/// Every optional model this route needs, each loaded the first time it is
/// wanted and resident from then on.
///
/// The two decoders are held separately and are *both* resident once used —
/// 655 MB plus 239 MB on an install that meets Japanese and Korean speakers in
/// the same evening. That is the honest price of rule (b) and it is why the
/// switches are separate: a machine that only ever hears one of the two pays
/// for one of the two.
pub struct Cjk {
    models: ModelSet,
    /// `[asr].japanese` — the Japanese arm.
    japanese: bool,
    /// `[asr].cjk` — the Korean and Chinese arms.
    cjk: bool,
    ja_asr: Lazy<CjkAsr>,
    sv_asr: Lazy<CjkAsr>,
    lid: Lazy<crate::lid::Lid>,
    windows: usize,
}

impl Cjk {
    pub fn new(models: &ModelSet, cfg: &AsrConfig) -> Self {
        Self {
            models: models.clone(),
            japanese: cfg.japanese,
            cjk: cfg.cjk,
            ja_asr: Lazy::default(),
            sv_asr: Lazy::default(),
            lid: Lazy::default(),
            windows: cfg.lid_windows.max(1),
        }
    }

    /// Is this language's decoder switched on and on disk?
    pub fn ready_for(&self, lang: &str) -> bool {
        match Decoder::for_lang(lang) {
            Some(Decoder::Japanese) => self.japanese && self.models.japanese().present(),
            Some(Decoder::SenseVoice) => self.cjk && self.models.sense_voice().present(),
            None => false,
        }
    }

    /// Is *anything* routable? The cheapest possible early exit, read at
    /// start-up for the log line and before every turn.
    ///
    /// The identifier is in the test because a decoder nothing can route to
    /// never runs — except on the declared-speaker path, which does not consult
    /// it; that path is rare enough that paying `ready()`'s early exit for it
    /// is not worth a second flag.
    pub fn ready(&self) -> bool {
        self.models.lid().present() && CJK.iter().any(|l| self.ready_for(l))
    }

    /// The line the daemon logs once at start-up, `None` when there is nothing
    /// worth saying.
    pub fn startup_note(&self) -> Option<String> {
        if !self.japanese && !self.cjk {
            return Some(
                "[asr].japanese and [asr].cjk are both off: a Japanese, Korean or Chinese \
                 turn will be transcribed by the multilingual decoder, which transliterates \
                 it into Latin letters."
                    .to_string(),
            );
        }
        if !self.models.lid().present() {
            return Some(crate::lid::how_to_get_it());
        }
        let missing: Vec<&str> = CJK.iter().copied().filter(|l| !self.ready_for(l)).collect();
        if missing.is_empty() {
            return None;
        }
        Some(CjkModel::how_to_get_it(&missing))
    }

    fn decoder_for(&mut self, lang: &str) -> Option<&mut CjkAsr> {
        let threads = self.models.asr_threads;
        match Decoder::for_lang(lang)? {
            Decoder::Japanese if self.japanese => {
                let model = self.models.japanese();
                self.ja_asr.get_or_load(
                    model.present(),
                    || CjkModel::how_to_get_it(&[JA]),
                    || CjkAsr::load(&model, threads),
                    |asr| info!(model = asr.model_id(), "loaded the Japanese decoder"),
                )
            }
            Decoder::SenseVoice if self.cjk => {
                let model = self.models.sense_voice();
                self.sv_asr.get_or_load(
                    model.present(),
                    || CjkModel::how_to_get_it(&[KO, ZH]),
                    || CjkAsr::load(&model, threads),
                    |asr| {
                        info!(
                            model = asr.model_id(),
                            "loaded the Korean and Chinese decoder"
                        )
                    },
                )
            }
            _ => None,
        }
    }

    fn identifier(&mut self) -> Option<&mut crate::lid::Lid> {
        let model = self.models.lid();
        let (threads, windows) = (self.models.asr_threads, self.windows);
        self.lid.get_or_load(
            model.present(),
            crate::lid::how_to_get_it,
            || crate::lid::Lid::load(&model, threads, windows),
            |lid| {
                info!(
                    model = lid.model_id(),
                    "loaded the spoken-language identifier"
                )
            },
        )
    }

    /// Decode `samples` with `lang`'s model and apply every guard. Decides
    /// nothing about the database and touches none of it.
    pub fn redecode(&mut self, lang: &str, samples: &[f32], cfg: &LangConfig) -> Rerouted {
        let duration_s = samples.len() as f32 / SAMPLE_RATE as f32;
        if duration_s < cfg.arbiter_min_duration_s {
            return Rerouted::TooShort;
        }
        let (raw, model_id, decoder) = match self.decoder_for(lang) {
            Some(asr) => {
                let (id, d) = (asr.model_id().to_string(), asr.decoder());
                (asr.transcribe(samples), id, d)
            }
            None => return Rerouted::Unavailable,
        };
        match judge(&raw, decoder, duration_s) {
            Ok((text, tag)) => Rerouted::Replaced {
                text,
                model_id,
                lang: tag,
            },
            Err(rejected) => rejected,
        }
    }

    /// Ask the identifier what this turn was spoken in.
    ///
    /// **Two options, and the outer one is the point.** `None` means the
    /// identifier did not run at all — not installed, or it would not load —
    /// so nothing was spent on this turn. `Some(None)` means it ran and had no
    /// opinion, which is a reading and costs what a reading costs.
    ///
    /// One `Option` cannot tell those apart, and the caller counts
    /// `lid_checked` off this answer. With a single `Option`, a `Lid::load`
    /// that failed once — a corrupt export, a machine out of memory — latched
    /// the unavailable flag and then reported *every* subsequent turn as
    /// checked while no model ever ran. `lid_checked` is documented as what
    /// this feature costs ("a rising count with `routed_ja` at zero means the
    /// daemon is paying 20 ms a turn to be told German"), so a dead identifier
    /// read as the busiest possible one.
    pub fn identify(&mut self, samples: &[f32]) -> Option<Option<Reading>> {
        Some(self.identifier()?.identify(samples))
    }

    /// Is the identifier loadable at all? Loads it on the first call, like
    /// [`Self::identify`], and is here so a test can ask the question without
    /// a 116 MB export.
    pub fn lid_ready(&mut self) -> bool {
        self.identifier().is_some()
    }
}

/// What the router did to one turn, for the caller's counters and event.
#[derive(Debug, Clone, PartialEq)]
pub struct Routed {
    /// The words the row says now.
    pub text: String,
    /// The tag the row says now, taken off the script the decoder wrote.
    pub lang: &'static str,
    pub asr_model_id: String,
    /// True when the identifier was actually run — which it is not on the
    /// declared-speaker path, and not on a clear transcript.
    pub lid_checked: bool,
}

/// The whole CJK route for one turn: decide, maybe listen, maybe decode, and
/// write.
///
/// Returns a default [`Checked`] for the overwhelmingly common case of a turn
/// that is not one of these three and was never going to be. `lid_checked` is
/// reported even when nothing was replaced, because "we asked and it said
/// German" is the number that says what this feature costs.
///
/// Writes two things and in this order:
///
/// 1. the words, through [`Store::set_segment_text_via`], which is the call the
///    context pass and the night shift already use — so the prior text lands in
///    `operations` as a `segments.redecode` row with the model and route that
///    produced it, and a person can compare or revert a CJK re-decode exactly
///    as they can any other machine edit;
/// 2. the language, via [`LANG_VIA_LID`].
///
/// A write failure is the caller's to log; nothing here is allowed to cost the
/// segment that was just analysed.
pub fn route_segment(
    cjk: &mut Cjk,
    store: &Store,
    turn: Turn<'_>,
    now_utc_ns: i64,
) -> Result<Checked> {
    let Turn {
        segment_id,
        declared,
        text,
        samples,
        lang_cfg,
        asr_cfg,
    } = turn;
    if !cjk.ready() {
        return Ok(Checked::default());
    }
    let mut checked = Checked::default();
    let (want, via) = match pre_route(declared, text, asr_cfg) {
        Pre::Nothing => return Ok(checked),
        Pre::Direct(lang) => (lang, "declared"),
        Pre::AskLid => {
            // The duration floor before the identifier, not only before the
            // decoder: LID on a 0.4 s fragment costs a model pass to produce a
            // reading nothing is allowed to act on.
            //
            // `[asr].lid_min_s` since 0.11.8, where this read
            // `[lang].arbiter_min_duration_s`. The two were the same number and
            // are not the same question — that floor is about whether a
            // replacement is better than what it overwrites, and this one is
            // about whether a model can tell what language it is hearing.
            // Fusing them meant that "Wanky Daska." (1.30 s, "genki desu ka")
            // was never even asked about. See the config note.
            if (samples.len() as f32 / SAMPLE_RATE as f32) < asr_cfg.lid_min_s {
                return Ok(checked);
            }
            // Counted only when a model actually ran. See `Cjk::identify`: an
            // identifier that never loaded costs nothing, and counting it is
            // how a dead feature came to read as an expensive one.
            let Some(reading) = cjk.identify(samples) else {
                return Ok(checked);
            };
            checked.lid_checked = true;
            // Kept whatever it said, so the caller can hand it to the other
            // half of the route (`crate::polyglot`) instead of paying for a
            // second identical model pass. A reading is a fact about the turn,
            // not about any one script.
            checked.heard = reading.clone();
            match post_route(reading.as_ref(), asr_cfg) {
                Some(lang) => (lang, "lid"),
                None => {
                    debug!(
                        segment_id,
                        heard = reading
                            .as_ref()
                            .map(|r| r.lang.as_str())
                            .unwrap_or("nothing"),
                        "not one of the languages this route decodes"
                    );
                    return Ok(checked);
                }
            }
        }
    };

    match cjk.redecode(want, samples, lang_cfg) {
        Rerouted::Replaced {
            text: new,
            model_id,
            lang,
        } => {
            store.set_segment_text_via(
                segment_id,
                &new,
                &model_id,
                crate::store::text_via::LID,
                now_utc_ns,
            )?;
            store.set_segment_language(segment_id, lang, LANG_VIA_LID)?;
            info!(
                segment_id,
                model = %model_id,
                via,
                routed_as = want,
                lang,
                "re-decoded a turn the multilingual model had transliterated"
            );
            checked.routed = Some(Routed {
                text: new,
                lang,
                asr_model_id: model_id,
                lid_checked: checked.lid_checked,
            });
        }
        other => debug!(segment_id, outcome = ?other, "the CJK re-decode did not stand"),
    }
    Ok(checked)
}

/// One turn as the router sees it.
///
/// A struct rather than six more parameters: the call already carries the
/// router, the store and a clock, and a nine-argument function is one where
/// two `Option<&str>` in a row will eventually be swapped by somebody in a
/// hurry.
#[derive(Debug, Clone, Copy)]
pub struct Turn<'a> {
    pub segment_id: i64,
    /// The speaker's declared language tags, when the turn has a speaker.
    pub declared: Option<&'a Vec<String>>,
    /// What the live decoder made of the audio.
    pub text: Option<&'a str>,
    pub samples: &'a [f32],
    pub lang_cfg: &'a LangConfig,
    pub asr_cfg: &'a AsrConfig,
}

/// What [`route_segment`] did, whether or not it changed anything.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Checked {
    /// The identifier was run on this turn.
    pub lid_checked: bool,
    /// The row's words were replaced.
    pub routed: Option<Routed>,
    /// What the identifier said, when it was asked and had an opinion.
    ///
    /// Here so the *other* half of the audio-language route
    /// ([`crate::polyglot`], which acts on fr/es/it/pt/nl/pl) can read the
    /// answer this module already paid for. LID is the one cost either feature
    /// has, and asking twice about the same turn would double it for nothing.
    ///
    /// `None` covers three situations that need no distinguishing downstream:
    /// not asked, asked and no opinion, and the router not ready. A caller
    /// that must tell them apart has `lid_checked`.
    pub heard: Option<Reading>,
}

/// Was this row's language settled by the CJK router?
///
/// Read by [`crate::analysis::Analyzer::apply_language_context`] to keep the
/// conversational prior off it. Without this guard the prior would undo the
/// fix: [`crate::langctx::decide`] reads `lang_via` for the values it treats
/// as settled, `lid` is not one of them, and `lang::classify` on kana returns
/// `Unclear` — so a correctly re-decoded Japanese turn would inherit "de" from
/// the German conversation around it.
pub fn settled_by_lid(store: &Store, segment_id: i64) -> Result<bool> {
    Ok(store
        .language_subject(segment_id)?
        .and_then(|s| s.lang_via)
        .is_some_and(|via| via == LANG_VIA_LID))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn reading(lang: &str, confidence: f32) -> Reading {
        Reading {
            lang: lang.into(),
            confidence,
        }
    }

    /// A router pointed at a directory with nothing in it: both switches on,
    /// and nothing behind either of them.
    fn a_router_with_no_models() -> Cjk {
        let models = ModelSet::resolve_at(
            std::path::PathBuf::from("/nonexistent/nx-recall-audit"),
            &crate::config::ModelsConfig::default(),
        );
        Cjk::new(
            &models,
            &crate::config::AsrConfig {
                japanese: true,
                cjk: true,
                ..Default::default()
            },
        )
    }

    #[test]
    fn an_identifier_that_never_ran_is_not_a_turn_that_was_checked() {
        let mut j = a_router_with_no_models();
        // Nothing on disk, so the load fails once and latches. The OUTER
        // option is what says so.
        assert!(!j.lid_ready());
        assert_eq!(j.identify(&vec![0.0f32; 16_000]), None);
        // And it stays `None` on every turn after, without a second warning —
        // which is exactly the state in which the old single-`Option`
        // signature made `route_segment` write `lid_checked = true` for ever.
        assert_eq!(j.identify(&vec![0.0f32; 16_000]), None);

        // The distinction the outer option buys: `Some(None)` is a reading
        // that cost a model pass and found nothing, and only that is counted.
        assert_eq!(post_route(None, &crate::config::AsrConfig::default()), None);

        // A turn nothing was spent on carries no count.
        assert!(!Checked::default().lid_checked);
    }

    #[test]
    fn a_router_that_is_not_ready_does_not_route_and_does_not_count() {
        // `ready()` is the cheap early exit, and it is false in every state
        // where `lid_checked` would otherwise be a lie: switched off, or
        // nothing installed.
        let mut off = a_router_with_no_models();
        assert!(!off.ready(), "no models means not ready");
        assert!(!off.lid_ready());
        assert!(CJK.iter().all(|l| !off.ready_for(l)));

        let models = ModelSet::resolve_at(
            std::path::PathBuf::from("/nonexistent/nx-recall-audit"),
            &crate::config::ModelsConfig::default(),
        );
        let switched_off = Cjk::new(
            &models,
            &crate::config::AsrConfig {
                japanese: false,
                cjk: false,
                ..Default::default()
            },
        );
        assert!(!switched_off.ready());
        assert!(
            switched_off
                .startup_note()
                .is_some_and(|s| s.contains("off")),
            "and it says so once, at start-up"
        );
    }

    #[test]
    fn the_two_switches_are_independent() {
        // Rule (b) of FINDINGS §27 shipped two decoders, two downloads and two
        // failure modes, so it ships two switches: a machine that only ever
        // hears Korean does not load 655 MB of Japanese Parakeet, and turning
        // Japanese off does not silently take Korean with it.
        let models = ModelSet::resolve_at(
            std::path::PathBuf::from("/nonexistent/nx-recall-audit"),
            &crate::config::ModelsConfig::default(),
        );
        let ko_only = Cjk::new(
            &models,
            &crate::config::AsrConfig {
                japanese: false,
                cjk: true,
                ..Default::default()
            },
        );
        // Not *ready* here only because nothing is on disk; the switch is what
        // is under test and it no longer answers for the other language.
        assert!(!ko_only.ready_for(JA));
        assert!(
            ko_only
                .startup_note()
                .is_some_and(|s| !s.contains("both off"))
        );
    }

    #[test]
    fn a_speaker_declared_one_of_the_three_goes_straight_to_its_decoder() {
        let cfg = AsrConfig::default();
        // No LID call: a declaration outranks a reading of one turn's audio,
        // and asking would cost a model pass to be told what we were told.
        assert_eq!(
            pre_route(Some(&tags(&["ja"])), Some("Sima Sen Okenki Deska."), &cfg),
            Pre::Direct(JA)
        );
        assert_eq!(
            pre_route(Some(&tags(&["ko"])), Some("mumble"), &cfg),
            Pre::Direct(KO)
        );
        assert_eq!(
            pre_route(Some(&tags(&["zh"])), Some("mumble"), &cfg),
            Pre::Direct(ZH)
        );
        // Even when the transliteration happens to read as English: the tag is
        // the whole point of the direct path.
        assert_eq!(
            pre_route(Some(&tags(&["ja"])), Some("i think that is so"), &cfg),
            Pre::Direct(JA)
        );
    }

    #[test]
    fn a_bilingual_speaker_is_not_pinned_to_anything() {
        let cfg = AsrConfig::default();
        // A Japanese speaker's English turn is not a mistake, so two tags mean
        // the same as none: fall through to the audio.
        assert_eq!(
            pre_route(Some(&tags(&["en", "ja"])), Some("mumble mumble"), &cfg),
            Pre::AskLid
        );
        assert_eq!(
            pre_route(Some(&tags(&["ko", "zh"])), Some("mumble mumble"), &cfg),
            Pre::AskLid
        );
    }

    #[test]
    fn a_speaker_declared_something_else_is_the_other_features_business() {
        let cfg = AsrConfig::default();
        // `correct_language` owns this row. Deciding it twice is how two
        // features start fighting over one column.
        assert_eq!(
            pre_route(Some(&tags(&["de"])), Some("Sima Sen Okenki Deska."), &cfg),
            Pre::Nothing
        );
        assert_eq!(
            pre_route(Some(&tags(&["en"])), Some("mumble"), &cfg),
            Pre::Nothing
        );
    }

    #[test]
    fn the_transliteration_failure_is_what_reaches_the_identifier() {
        let cfg = AsrConfig::default();
        // THE case. Latin letters, English-shaped, no German stopwords, no
        // kana for the script rule — `Unclear`, and invisible to every text
        // mechanism this daemon had before 0.11.0.
        assert_eq!(
            lang::classify("Sima Sen Okenki Deska."),
            lang::Lang::Unclear
        );
        assert_eq!(lang::guess_other("Sima Sen Okenki Deska."), None);
        assert_eq!(
            pre_route(None, Some("Sima Sen Okenki Deska."), &cfg),
            Pre::AskLid
        );
        // No words at all is the same failure with the volume turned down.
        assert_eq!(pre_route(None, None, &cfg), Pre::AskLid);
        assert_eq!(pre_route(None, Some("   "), &cfg), Pre::AskLid);
    }

    #[test]
    fn clear_german_text_never_costs_a_lid_call() {
        let cfg = AsrConfig::default();
        // The cost this whole two-step split exists to avoid: 20 ms per turn
        // on an evening of German that was never going to be Japanese.
        assert_eq!(
            pre_route(None, Some("ich glaube das ist der einzige weg"), &cfg),
            Pre::Nothing
        );
        assert_eq!(
            pre_route(None, Some("i think that is the only way"), &cfg),
            Pre::Nothing
        );
    }

    #[test]
    fn a_transcript_that_already_reads_as_something_is_left_alone() {
        let cfg = AsrConfig::default();
        // Kana, hangul or Han in the transcript means some decoder already got
        // it right.
        assert_eq!(
            pre_route(None, Some("すみません、お元気ですか"), &cfg),
            Pre::Nothing
        );
        assert_eq!(
            pre_route(None, Some("안녕하세요 반갑습니다"), &cfg),
            Pre::Nothing
        );
        assert_eq!(
            pre_route(None, Some("这是区分某些动词的方法"), &cfg),
            Pre::Nothing
        );
        // And a confident third-language guess is a real reading of the words.
        assert_eq!(
            pre_route(
                None,
                Some("le la les des une est ne pas que qui pour dans"),
                &cfg
            ),
            Pre::Nothing
        );
    }

    #[test]
    fn the_identifier_has_to_name_one_of_the_three_and_mean_it() {
        let cfg = AsrConfig::default();
        assert_eq!(post_route(Some(&reading("ja", 1.0)), &cfg), Some(JA));
        assert_eq!(post_route(Some(&reading("ko", 1.0)), &cfg), Some(KO));
        assert_eq!(post_route(Some(&reading("zh", 1.0)), &cfg), Some(ZH));
        // German. The overwhelmingly common answer, and it stops here.
        assert_eq!(post_route(Some(&reading("de", 1.0)), &cfg), None);
        assert_eq!(post_route(Some(&reading("en", 1.0)), &cfg), None);
        // Cantonese: SenseVoice speaks it, the catalogue does not claim it and
        // `lang::KNOWN` does not carry it, so nothing routes it.
        assert_eq!(post_route(Some(&reading("yue", 1.0)), &cfg), None);
        // No opinion is not an opinion.
        assert_eq!(post_route(None, &cfg), None);
        // Under the operating point, once a machine has raised it.
        let strict = AsrConfig {
            lid_min_confidence: 0.9,
            ..AsrConfig::default()
        };
        assert_eq!(post_route(Some(&reading("ko", 0.66)), &strict), None);
        assert_eq!(post_route(Some(&reading("ko", 1.0)), &strict), Some(KO));
    }

    #[test]
    fn each_language_reaches_the_decoder_it_is_catalogued_for() {
        assert_eq!(Decoder::for_lang(JA), Some(Decoder::Japanese));
        assert_eq!(Decoder::for_lang(KO), Some(Decoder::SenseVoice));
        assert_eq!(Decoder::for_lang(ZH), Some(Decoder::SenseVoice));
        // Nothing else has a decoder, including the two the European model
        // already speaks and the one SenseVoice can write but nothing claims.
        for tag in ["de", "en", "yue", "fr", ""] {
            assert_eq!(Decoder::for_lang(tag), None, "{tag}");
        }
        // The Parakeet's weights can produce Japanese and nothing else.
        assert_eq!(Decoder::Japanese.writes(), &[JA]);
        assert_eq!(Decoder::SenseVoice.writes(), CJK);
    }

    #[test]
    fn sense_voices_tags_come_off_before_anything_reads_the_string() {
        // The measured shape: language, emotion, event, ITN, then the words.
        assert_eq!(
            strip_tags("<|ja|><|NEUTRAL|><|Speech|><|woitn|>すみません"),
            "すみません"
        );
        // Emotion and event tags are the two the upstream README warns about,
        // and they are not always neutral.
        assert_eq!(strip_tags("<|zh|><|HAPPY|><|BGM|>开饭时间"), "开饭时间");
        // Not anchored to the front: a release that appends one must not leave
        // Latin angle brackets in a Korean sentence.
        assert_eq!(strip_tags("안녕하세요<|Applause|>"), "안녕하세요");
        // An unterminated `<|` is text, not a tag, and is left alone rather
        // than eating the rest of the transcript.
        assert_eq!(strip_tags("a <| b"), "a <| b");
        // Nothing to strip is the common case at sherpa 0.6.8, where the C
        // result carries the tags in its own fields.
        assert_eq!(strip_tags("元気ですか"), "元気ですか");
        // And a stripped string that is nothing but tags is empty, which the
        // judge rejects rather than storing.
        assert!(judge("<|en|><|NEUTRAL|><|Speech|>", Decoder::SenseVoice, 3.0).is_err());
    }

    #[test]
    fn the_guards_reject_a_re_decode_that_is_not_in_the_right_script() {
        // The load-bearing guard, and why a false positive from the identifier
        // is cheap: whatever either decoder makes of German audio, it is not
        // kana, hangul or Han, so it overwrites nothing.
        assert!(matches!(
            judge("ich glaube das schon", Decoder::Japanese, 3.0),
            Err(Rerouted::Rejected { .. })
        ));
        assert!(judge("ich glaube das schon", Decoder::SenseVoice, 3.0).is_err());
        assert!(judge("", Decoder::Japanese, 3.0).is_err());
        assert!(judge("   ...  ", Decoder::SenseVoice, 3.0).is_err());
        // Kanji with no kana is not proof of Japanese — it is the script
        // Chinese writes in too, which is exactly why `guess_other` checks
        // kana by presence rather than Han by dominance. The Parakeet, which
        // speaks only Japanese, must not have that accepted as Japanese.
        assert!(judge("技術決定論", Decoder::Japanese, 3.0).is_err());
        // A caption is stripped before anything judges it, the same as the
        // German arbiter — and what is left is nothing.
        assert!(judge("(music)", Decoder::Japanese, 3.0).is_err());

        // What passes, and with which tag.
        assert_eq!(
            judge("すみません、お元気ですか", Decoder::Japanese, 3.0).unwrap(),
            ("すみません、お元気ですか".to_string(), JA)
        );
        assert_eq!(
            judge("[音楽] 元気ですか", Decoder::Japanese, 3.0).unwrap(),
            ("元気ですか".to_string(), JA)
        );
        assert_eq!(
            judge("안녕하세요 반갑습니다", Decoder::SenseVoice, 3.0)
                .unwrap()
                .1,
            KO
        );
        assert_eq!(
            judge(
                "这是区分某些动词和宾语的一个重要方法",
                Decoder::SenseVoice,
                3.0
            )
            .unwrap()
            .1,
            ZH
        );
    }

    #[test]
    fn the_script_decides_the_stamp_and_that_is_what_saves_a_misrouted_turn() {
        // 1.5% of Japanese is heard as Korean at 3 s (FINDINGS §27), so those
        // turns reach SenseVoice through the Korean arm. Forcing SenseVoice's
        // language changes nothing about what it writes — measured — so it
        // writes kana, and reading the script recovers both the transcript and
        // the right tag where trusting the reading would have thrown away both.
        assert_eq!(
            judge("すみません、お元気ですか", Decoder::SenseVoice, 3.0)
                .unwrap()
                .1,
            JA,
            "routed as ko, written in kana, stamped ja"
        );
        // The guard is not loosened by that: a language this decoder does not
        // write is still rejected however confidently the script reads.
        assert!(matches!(
            judge("Здравствуйте как дела сегодня", Decoder::SenseVoice, 3.0),
            Err(Rerouted::Rejected {
                script: Some(_),
                ..
            })
        ));
    }

    #[test]
    fn a_re_decoded_row_is_stamped_by_the_thing_that_actually_decided_it() {
        // Not `re-decode` (nothing disagreed with a declaration) and not
        // `classified` (the words said nothing). A model listened.
        assert_eq!(LANG_VIA_LID, "lid");
        assert_ne!(LANG_VIA_LID, crate::store::lang_via::REDECODE);
        assert_ne!(LANG_VIA_LID, crate::store::lang_via::CLASSIFIED);
        // And every tag this route can stamp is a tag a voice may be declared
        // with, so the speaker picker and the router agree about the world.
        for tag in CJK {
            assert!(lang::KNOWN.contains(tag), "{tag} is routable but not KNOWN");
        }
    }

    // ---- 0.12.0: the lobby is not FLEURS (FINDINGS §31) -------------------

    #[test]
    fn a_voice_that_declared_two_languages_has_still_declared_them() {
        // THE failure of this round, from the live database: speaker 26 is the
        // user's own microphone, `languages = ["de","en"]`, and **37 of the 45
        // rows this route wrongly rewrote are that voice**. `sole_language`
        // answers `None` to two tags, so every unreadable grunt from the person
        // whose languages we know fell through to the identifier.
        let cfg = AsrConfig::default();
        assert_eq!(
            pre_route(Some(&tags(&["de", "en"])), Some("Mm-hmm."), &cfg),
            Pre::Nothing
        );
        assert_eq!(
            pre_route(
                Some(&tags(&["de", "en"])),
                Some("Yeah, Gott was zu trinken."),
                &cfg
            ),
            Pre::Nothing,
            "the row that became よしじゃあ。"
        );
        // …including with no words at all, which is the shape three of the
        // wrong rewrites had.
        assert_eq!(
            pre_route(Some(&tags(&["de", "en"])), None, &cfg),
            Pre::Nothing
        );
        // A declared set that DOES contain something a route can decode still
        // falls through: a Japanese speaker's English turn is not a mistake and
        // that reasoning is untouched.
        assert_eq!(
            pre_route(Some(&tags(&["en", "ja"])), Some("Sima Sen Deska"), &cfg),
            Pre::AskLid
        );
        // And so does an undeclared voice — most of a lobby.
        assert_eq!(
            pre_route(None, Some("Sima Sen Okenki Deska."), &cfg),
            Pre::AskLid
        );
        // The set is the union of BOTH routes, so a French-only allowlist and a
        // French-only declaration still meet. `de`+`fr` is not a set a client
        // can currently send (`lang::KNOWN` is narrower), which is exactly why
        // the rule is written against the config rather than against `KNOWN` —
        // the day `fr` becomes declarable it must not silently start refusing.
        let de_fr = tags(&["de", "fr"]);
        assert_eq!(
            pre_route(Some(&de_fr), Some("mumble mumble"), &cfg),
            Pre::AskLid
        );
        let no_french = AsrConfig {
            polyglot_languages: vec![],
            ..AsrConfig::default()
        };
        assert_eq!(
            pre_route(Some(&de_fr), Some("mumble mumble"), &no_french),
            Pre::Nothing
        );
    }

    #[test]
    fn a_back_channel_is_never_worth_a_second_decoder() {
        // The 22 rows with no content word at all, verbatim from the live
        // database, and what the route made of each.
        let cfg = AsrConfig::default();
        for grunt in [
            "Mm-hmm.",     // → うん, four separate times
            "Mm.",         // → うん
            "Uh",          // → いただきります
            "Uh.",         // → 啊 嗯
            "Oh",          // → あっ
            "Ah.",         // → なるほどあっ
            "Yeah.",       // → 没
            "Okay, yeah.", // → ok看嗯
            "Right.",      // → 可以嗯
            "Mm, mm-hmm.", // → 嗯嗯嗯来
            "Uh yeah.",    // → 嗯嗯
            "Um",          // → ガンとあたって
        ] {
            assert_eq!(
                pre_route(None, Some(grunt), &cfg),
                Pre::Nothing,
                "{grunt:?} reached the identifier"
            );
            assert_eq!(lang::content_word_count(grunt), 0, "{grunt:?}");
        }
        // One content word is not two: "Uh special.", "Katastro.", "H",
        // "Mm s.", "Uh Alter.", "Oh my ooh ooh oh okay." are the other six.
        for thin in ["Uh special.", "Katastro.", "Oh my ooh ooh oh okay."] {
            assert_eq!(pre_route(None, Some(thin), &cfg), Pre::Nothing, "{thin:?}");
        }

        // …and the bar is where it is because of what is on the other side of
        // it. All three rows this feature was built for clear it, and two of
        // them clear it exactly.
        for founding in [
            "Sima Sen Okenki Deska.",
            "Wanky Daska.",
            "During apartments.",
        ] {
            assert!(lang::content_word_count(founding) >= MIN_CONTENT_WORDS);
            assert_eq!(
                pre_route(None, Some(founding), &cfg),
                Pre::AskLid,
                "{founding:?}"
            );
        }
        assert_eq!(lang::content_word_count("Wanky Daska."), 2);
        assert_eq!(
            MIN_CONTENT_WORDS, 2,
            "three would have lost the founding rows"
        );

        // A turn with NO words is still asked about: a decoder that gave up
        // entirely is the one worth re-reading, and both of this install's
        // genuine Japanese rows are that shape.
        assert_eq!(pre_route(None, None, &cfg), Pre::AskLid);
        assert_eq!(pre_route(None, Some("  "), &cfg), Pre::AskLid);
    }

    #[test]
    fn the_script_test_is_not_evidence_and_these_rows_are_why() {
        // Every one of these is a real re-decode from the live database, in a
        // script only the target languages use, which is all the 0.11.6 judge
        // ever asked for. `(text, seconds)`.
        let junk: &[(&str, f32)] = &[
            ("うん", 1.78),  // ← "Mm-hmm."
            ("うん", 5.26),  // ← "Mm-hmm.", over five seconds of audio
            ("没", 2.38),    // ← "Yeah."
            ("あっ", 2.26),  // ← "Oh"
            ("嗯嗯", 1.68),  // ← "Uh yeah."
            ("哎呀", 1.78),  // ← "Uh Alter."
            ("啊 嗯", 1.81), // ← "Uh."
            ("フフフフフフフ", 1.74),
            ("没没没不是说说说说说说说说说说说没没没没没", 4.94),
            ("あまたタ待タ待タ待タ。", 1.84),
            ("ok看嗯", 3.89), // ← "Okay, yeah."
            ("そ be丈夫", 1.74),
            ("오빠どなか", 3.41), // ← "Hopp! Oh danke!", hangul AND kana
            ("そうしました", 6.67),
            ("可以嗯", 2.32), // ← "Right."
        ];
        for (text, duration_s) in junk {
            assert!(
                weak_output(text, *duration_s).is_some(),
                "{text:?} at {duration_s} s passed as evidence"
            );
            // …and the whole judge refuses it, not merely the helper.
            assert!(
                judge(text, Decoder::SenseVoice, *duration_s).is_err(),
                "{text:?}"
            );
        }

        // The two rows on this install that look genuine survive all of it,
        // and so does an ordinary sentence.
        for (text, duration_s) in [
            ("すいません", 3.79f32),
            ("聞いてみますかねちょっと", 1.55),
            ("すみません、お元気ですか", 3.0),
        ] {
            assert_eq!(weak_output(text, duration_s), None, "{text:?}");
            assert!(
                judge(text, Decoder::Japanese, duration_s).is_ok(),
                "{text:?}"
            );
        }

        // And the rejection says which guard, so a log line is worth reading.
        assert!(matches!(
            judge("うん", Decoder::Japanese, 5.26),
            Err(Rerouted::Rejected {
                why: "too few characters to be a sentence",
                ..
            })
        ));
        assert!(matches!(
            judge("フフフフフフフ", Decoder::Japanese, 1.74),
            Err(Rerouted::Rejected {
                why: "a repetition loop",
                ..
            })
        ));
        assert!(matches!(
            judge("そうしました", Decoder::Japanese, 6.67),
            Err(Rerouted::Rejected {
                why: "too little text for the length of the audio",
                ..
            })
        ));
        assert!(matches!(
            judge("오빠どなか", Decoder::SenseVoice, 3.41),
            Err(Rerouted::Rejected {
                why: "two writing systems at once",
                ..
            })
        ));
        // A German re-decode is still refused by the script test first, which
        // is the guard that has not changed.
        assert!(matches!(
            judge("ich glaube das schon", Decoder::Japanese, 3.0),
            Err(Rerouted::Rejected {
                why: "not a script this decoder writes",
                ..
            })
        ));
    }

    /// Every model, on real audio, through the real FFI.
    ///
    /// `#[ignore]` because it needs a populated models root and a WAV, neither
    /// of which lives in this repository. It is here rather than only in the
    /// Python spike because the part of this module most likely to be silently
    /// wrong is the hand-filled model config in [`CjkAsr::load`] — a mistake
    /// there does not fail to compile and does not fail to load, it produces
    /// empty strings or garbage, exactly as the multilingual export does under
    /// the generic `transducer` type (see `crate::asr`).
    ///
    /// ```text
    /// NXR_MODELS=<dir> NXR_CJK_WAV=<16k mono wav> NXR_CJK_LANG=ja \
    ///   cargo test -p recalld --lib -- --ignored the_models_actually
    /// ```
    #[test]
    #[ignore = "needs a populated models root and a CJK WAV"]
    fn the_models_actually_load_and_hear_the_language() {
        let Ok(root) = std::env::var("NXR_MODELS") else {
            panic!("set NXR_MODELS");
        };
        let wav = std::env::var("NXR_CJK_WAV").expect("set NXR_CJK_WAV");
        let want = std::env::var("NXR_CJK_LANG").unwrap_or_else(|_| JA.to_string());
        let tag = canonical(&want).expect("NXR_CJK_LANG must be ja, ko or zh");
        let mut reader = hound::WavReader::open(&wav).expect("opening the WAV");
        assert_eq!(
            reader.spec().sample_rate,
            SAMPLE_RATE,
            "the WAV must be 16 kHz"
        );
        let spec = reader.spec();
        // FLEURS ships 16-bit PCM and a hand-exported clip may well be float,
        // so take either rather than making the operator convert one.
        let samples: Vec<f32> = match spec.sample_format {
            hound::SampleFormat::Float => reader
                .samples::<f32>()
                .map(|s| s.expect("a sample"))
                .collect(),
            hound::SampleFormat::Int => {
                let scale = 1.0 / (1i64 << (spec.bits_per_sample - 1)) as f32;
                reader
                    .samples::<i32>()
                    .map(|s| s.expect("a sample") as f32 * scale)
                    .collect()
            }
        };

        let models = crate::models::ModelSet::resolve_at(
            std::path::PathBuf::from(&root),
            &crate::config::ModelsConfig::default(),
        );
        assert!(
            models.lid().present(),
            "the identifier is not installed at {root}"
        );

        // The identifier hears the language.
        let mut lid = crate::lid::Lid::load(&models.lid(), 4, 1).expect("loading the identifier");
        let reading = lid.identify(&samples).expect("a reading");
        assert_eq!(reading.lang, tag, "heard {} instead", reading.lang);

        // The decoder produces that language's script, and the guard accepts
        // it. This is the assertion the FFI config has to earn: a wrong
        // `model_type` or a missing NULL comes back empty or as Latin mojibake,
        // and `judge` rejects both.
        let decoder = Decoder::for_lang(tag).expect("a decoder");
        let model = match decoder {
            Decoder::Japanese => models.japanese(),
            Decoder::SenseVoice => models.sense_voice(),
        };
        assert!(model.present(), "{} is not installed", model.export.dir);
        let mut asr = CjkAsr::load(&model, 4).expect("loading the decoder");
        let text = asr.transcribe(&samples);
        assert!(!text.is_empty(), "the decoder returned nothing");
        let (kept, heard) = judge(&text, decoder, samples.len() as f32 / SAMPLE_RATE as f32)
            .unwrap_or_else(|e| panic!("judged {text:?} as {e:?}"));
        assert_eq!(heard, tag);
        println!("decoded: {kept}");
    }

    #[test]
    fn the_catalogue_entries_name_the_files_the_decoders_open() {
        use crate::models::{
            JAPANESE_ASR, JAPANESE_DIR, LID_WHISPER, SENSE_VOICE_ASR, SENSE_VOICE_DIR,
            expected_bytes,
        };
        // The sizes the fetch verifies against, transcribed from the extracted
        // files (spike, 2026-09-03) rather than from the release page.
        assert_eq!(
            expected_bytes(&format!("{JAPANESE_DIR}/{}", JAPANESE_ASR.model)),
            Some(655_542_604)
        );
        assert_eq!(
            expected_bytes(&format!("{JAPANESE_DIR}/{}", JAPANESE_ASR.tokens)),
            Some(28_557)
        );
        assert_eq!(
            expected_bytes(&format!("{SENSE_VOICE_DIR}/{}", SENSE_VOICE_ASR.model)),
            Some(239_233_841)
        );
        assert_eq!(
            expected_bytes(&format!("{SENSE_VOICE_DIR}/{}", SENSE_VOICE_ASR.tokens)),
            Some(315_894)
        );
        assert_eq!(
            expected_bytes(&format!("{}/{}", LID_WHISPER.dir, LID_WHISPER.encoder)),
            Some(12_937_772)
        );
        assert_eq!(
            expected_bytes(&format!("{}/{}", LID_WHISPER.dir, LID_WHISPER.decoder)),
            Some(89_855_401)
        );
        // `--japanese` is still the decoder and the identifier, unchanged, so
        // an install that only ever meets Japanese pays exactly what it paid.
        assert_eq!(
            crate::models::japanese_download_bytes(),
            489_389_564 + 116_204_861
        );
        // `--cjk` is that plus SenseVoice: Japanese is one of the three, so it
        // is a superset rather than a separate download.
        assert_eq!(
            crate::models::cjk_download_bytes(),
            489_389_564 + 116_204_861 + 1_047_870_769
        );
        // The decoding contract is part of the id, the same rule the arbiter
        // follows.
        assert_eq!(
            JAPANESE_ASR.model_id(),
            "sherpa-onnx-nemo-parakeet-tdt_ctc-0.6b-ja-35000-int8@1"
        );
        assert_eq!(
            SENSE_VOICE_ASR.model_id(),
            "sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17@1"
        );
        // And each export knows which sherpa model config loads it, which is
        // the one mistake in this file that neither compiles nor loads wrong.
        assert_eq!(JAPANESE_ASR.decoder, Decoder::Japanese);
        assert_eq!(SENSE_VOICE_ASR.decoder, Decoder::SenseVoice);
    }
}
