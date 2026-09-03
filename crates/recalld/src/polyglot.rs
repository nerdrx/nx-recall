//! The other languages: re-decoding a turn the multilingual model flipped
//! (0.11.6).
//!
//! ## The failure, and how it differs from the Japanese one
//!
//! Three rows from the user's own microphone, one evening:
//!
//! > "Mon petit chou."      1.01 s
//! > "During apartments."   1.20 s — French
//! > "Wanky Daska."         1.30 s — Japanese, "genki desu ka"
//!
//! The middle one is what this module is for, and it is **not** the failure
//! [`crate::asr_ja`] fixes. Japanese fails because Parakeet-TDT-0.6b-v3 does
//! not speak it: 25 European languages, no Japanese, and the model
//! transliterates rather than declining. French is one of the 25. v3 speaks it
//! well — measured here at 8-16% WER on FLEURS cuts — and on a turn this short
//! it does not fail to hear French, it **commits to English**. That is the flip
//! [`crate::arbiter`] was built for in 0.7.7, and the reason the German arbiter
//! did not already catch this one is that the arbiter is reached from *text*:
//! `lang::classify` has to suspect a wrong-language decode before anything is
//! re-read, and "During apartments." is two ordinary English words.
//!
//! So the fix is the arbiter's mechanism reached by the identifier's route:
//! decide from the audio ([`crate::lid`]), then re-decode under a hard language
//! constraint, then judge the result.
//!
//! ## Why the guard has to be stricter here than it is for Japanese
//!
//! [`crate::asr_ja::judge`] is safe almost for free, and it is worth being
//! precise about why, because none of it carries over. A Japanese-only decoder
//! handed German audio produces something with no kana in it; kana is a
//! *script* test, exact and uncheatable, so a false positive from the
//! identifier costs one wasted decode and never a wrong transcript.
//!
//! Whisper forced to `fr` over German audio produces **French words**. Real
//! ones, with French function words among them, which [`crate::lang::
//! guess_other`] will call French because it *is* French — it is simply French
//! nobody said. There is no script to appeal to. Every Romance and Germanic
//! language this module can route to shares an alphabet with the two it must
//! never damage.
//!
//! What stands in for the script test is **three independent narrowings**, and
//! the measurement (FINDINGS §28) is that the route is safe because of all
//! three together rather than any one of them:
//!
//! 1. **The turn is only asked about if its transcript is already unreadable.**
//!    [`crate::asr_ja::pre_route`] — shared, not re-implemented — sends a turn
//!    to the identifier only when `classify` says `Unclear`/`Empty` and
//!    `guess_other` is not confident. A German turn that decoded as German
//!    never reaches LID, so the raw LID confusion rate is not the rate that
//!    matters. Conditioning on the real gate is most of the safety argument.
//! 2. **The re-decode must not agree with the transcript it would replace.**
//!    An arbiter whose "correction" is the words already on the row has found
//!    nothing, and on a flipped turn it always disagrees.
//! 3. **The result must read as the language the identifier named, and read as
//!    it *confidently*** — the same bar `segments.lang` is written at, not the
//!    looser one that only puts a turn in the translation queue. A German
//!    fragment re-decoded as French rarely produces four French function words;
//!    a real French turn usually does.
//!
//! ## The decoder, and the one that was measured and thrown away
//!
//! **whisper-large-v3 q5_0 on the GPU**, through the night shift's compiled
//! runtime, forced to the language the identifier named. That is an expensive
//! dependency for a live path and it is not the one this module was written to
//! use — it is the one the measurement left standing.
//!
//! The obvious backend was whisper base int8: the same 29 MB export the German
//! arbiter already loads, with a different language token, which would have
//! made six languages cost zero new bytes. It was measured (FINDINGS §28) and
//! it is **actively harmful** — on every language, at every length, over the
//! turns this route would actually hand it:
//!
//! | backend            | fr 1.0 s | fr 1.5 s | es 1.5 s | it 1.5 s |
//! |--------------------|---------:|---------:|---------:|---------:|
//! | whisper-base int8  |   -54.7% |   -63.7% |  -195.0% |   -84.4% |
//! | large-v3 q5_0, GPU |   +62.4% |   +31.2% |        — |        — |
//!
//! (Relative WER change on the turns whose text was replaced; positive is
//! better. Gate: >= +30%, and at most 5% of touched turns made worse. base put
//! 10-83% of them in that category; the GPU put 0%.)
//!
//! The sign flip is not a surprise once it is written down, and it is the
//! difference between this route and the Japanese one. `asr_ja`'s arbiter
//! competes against a decoder **that cannot spell the language at all** — any
//! kana beats "Sima Sen Okenki Deska", so a middling Japanese model wins
//! easily. This one competes against Parakeet-TDT-0.6b-v3 speaking French,
//! which it does at 18-22% WER. An arbiter here has to be better than a good
//! decoder having a bad second, and whisper base is not better than v3 at
//! anything. Only large-v3 is.
//!
//! So the route is **conditional on the night shift being installed**
//! (`models fetch --night` and `models build-night`) and on the GPU being idle
//! enough — the same `[night].gpu_busy_max_pct` gate the night shift itself
//! passes before every batch, because the GPU is drawing the user's frames. A
//! machine without it keeps the transcript it has, which is exactly what it
//! does today.

use tracing::{debug, info, warn};

use crate::arbiter::strip_captions;
use crate::asr::normalise_words;
use crate::config::{AsrConfig, LangConfig, SAMPLE_RATE};
use crate::lang;
use crate::lid::Reading;
use crate::models::{ModelSet, NightModels};
use crate::store::Store;

/// `segments.lang_via` and `segments.text_via` for a row this route settled.
///
/// Deliberately the **same** `lid` value [`crate::asr_ja`] writes, rather than a
/// seventh: the claim being made about the row is identical — a model listened
/// to the audio and named the language — and the thing that differs is only
/// which decoder was then asked, which `asr_model_id` already records. A
/// consumer that wants to know whether a row was settled by ear should not have
/// to learn a new string every time a language is added.
pub const LANG_VIA_LID: &str = crate::asr_ja::LANG_VIA_LID;

/// Every language tag the route knows how to be forced into.
///
/// The catalogue, not the default — `[asr].polyglot_languages` is what an
/// install actually turns on, and it ships shorter than this (see the config
/// note). One decoder serves all of them, because the language is a decoding
/// argument to `whisper-cli`, so adding a tag here costs nothing but the
/// obligation to have measured it.
///
/// Which tags are here, and why not the other twenty Whisper speaks: every one
/// is in [`crate::lang::GUESSABLE`], because guard 3 above cannot be applied to
/// a language [`lang::guess_other`] has no opinion about. A tag the judge
/// cannot read is a tag whose re-decode could never be checked, and shipping
/// that would be shipping the flip in the other direction.
pub const ROUTABLE: &[&str] = &["fr", "es", "it", "pt", "nl", "pl"];

/// The tags whose *benefit* was measured, and therefore the shipped default.
///
/// French is the only one the GPU rows could be collected for — the GPU is the
/// user's and it went back to drawing frames — so `fr` is the only language
/// with a WER gate behind it. `es` and `it` are here on the strength of the
/// half of the measurement that *was* completed for them: the same LID recall
/// (89.0% and 58.5% at 1.5 s) and the same false-positive behaviour, with the
/// identical judge deciding what is kept. `pt`, `nl` and `pl` are in
/// [`ROUTABLE`] and not here, because nothing about them was measured at all.
pub const MEASURED: &[&str] = &["fr", "es", "it"];

/// Is `tag` a language the route knows how to force a decoder to?
///
/// The same shape and the same reasoning as [`crate::arbiter::target_for`]: a
/// total mapping, because the bug that function exists to document — a
/// catch-all arm quietly re-decoding a turn in the wrong language and stamping
/// the row with the right one — is available here for five more tags than it
/// was there.
pub fn is_routable(tag: &str) -> bool {
    ROUTABLE.contains(&tag)
}

/// What this route stamps on a row it rewrote, for a given forced language.
///
/// The forced language is in the id for the reason
/// [`crate::models::WhisperExport::model_id`] spells out: it is part of the
/// decoding contract rather than an observation, and a row re-read in French
/// must never claim to have been written by the pass that re-read one in
/// Spanish — even though it was the same weights and the same binary.
pub fn model_id(night: &NightModels, want: &str) -> String {
    format!("{}-{want}", night.model_id())
}

/// Is this a language the route may re-decode into *on this machine*?
///
/// Three conditions and they are different: the feature is on, the tag is one
/// the route knows ([`is_routable`]), and the operator has it in
/// `[asr].polyglot_languages`. The config list is allowed to name a tag the
/// route does not know — it is a list of what the operator wants, not a claim
/// about the catalogue — and this is where the two meet.
pub fn routable(tag: &str, cfg: &AsrConfig) -> bool {
    cfg.polyglot && is_routable(tag) && cfg.polyglot_languages.iter().any(|l| l == tag)
}

/// Step two of the route, for a reading the identifier has already produced.
///
/// Returns the tag to re-decode into. Separate from [`crate::asr_ja::post_route`]
/// rather than folded into it because the two answer different questions about
/// the same `Reading`: that one asks "is it Japanese", this one asks "is it one
/// of the several languages we could do something about", and only this one can
/// answer with *which*.
pub fn post_route(reading: Option<&Reading>, cfg: &AsrConfig) -> Option<String> {
    let r = reading?;
    if r.confidence < cfg.lid_min_confidence || !routable(&r.lang, cfg) {
        return None;
    }
    Some(r.lang.clone())
}

/// What a forced re-decode did, or why it did not.
#[derive(Debug, Clone, PartialEq)]
pub enum Rerouted {
    /// The re-decode cleared every guard. These words replace the row's.
    Replaced { text: String, model_id: String },
    /// Under `[lang].arbiter_min_duration_s`. The model was not run: at this
    /// length the arbiter's own words were measured to be no better than the
    /// ones they would overwrite (FINDINGS §7).
    TooShort,
    /// The night shift's model or its compiled runtime is not installed, so
    /// there is no decoder good enough to arbitrate this. Said once per
    /// daemon, not once per turn.
    Unavailable,
    /// The GPU is busy drawing the user's frames. The same gate the night
    /// shift passes before every batch, for the same reason, and a refusal
    /// rather than a queue: the turn is already transcribed, just wrongly, and
    /// nothing here is worth a dropped frame.
    GpuBusy { pct: u32 },
    /// `whisper-cli` failed or timed out. The row stands.
    Failed,
    /// It ran and the answer failed a guard.
    Rejected {
        /// What the re-decode actually read as — the whole point of the guard.
        read_as: &'static str,
        words: usize,
        /// The re-decode said what the row already said. Not a correction.
        echo: bool,
    },
}

/// The replacement guards, as a pure function over what the decoder said.
///
/// `prior` is the text this would overwrite. Split out from the model call for
/// the reason [`crate::arbiter::judge`] and [`crate::asr_ja::judge`] both are:
/// the whole rule can be exercised without a 160 MB model, and there is one
/// copy of it.
///
/// Four tests, in the order of how cheap they are:
///
/// 1. **Caption-stripped**, the same scan the German arbiter runs, for the same
///    measured reason: this is the same Whisper export, with the same habit of
///    narrating non-speech as `(soft music)`.
/// 2. **At least `arbiter_min_words`.** Unlike the Japanese route — which had
///    to drop this bar because a language without spaces has no word count —
///    every language here is written with spaces, so the arbiter's floor
///    carries over unchanged.
/// 3. **Not an echo.** A re-decode that produced the words already on the row
///    has not corrected anything, and replacing text with itself would write an
///    `operations` row recording a change that did not happen. Compared over
///    [`normalise_words`], so a difference in punctuation or case is not a
///    correction either.
/// 4. **Reads as `want`, confidently.** [`lang::guess_other`]'s `confident`
///    flag and not merely its tag: that flag is the bar `segments.lang` is
///    written at, and this writes `segments.lang`. Guard 3 of the module note,
///    and the one standing in for Japanese's kana test.
pub fn judge(raw: &str, want: &str, prior: &str, cfg: &LangConfig) -> Result<String, Rerouted> {
    let text = strip_captions(raw);
    let words = normalise_words(&text);
    let read = lang::guess_other(&text);
    let echo = words == normalise_words(prior);
    let reads_as_want = read.is_some_and(|g| g.tag == want && g.confident);
    if words.len() < cfg.arbiter_min_words.max(1) || echo || !reads_as_want {
        return Err(Rerouted::Rejected {
            read_as: read.map_or("unclear", |g| g.tag),
            words: words.len(),
            echo,
        });
    }
    Ok(text)
}

// ---------------------------------------------------------------------------
// the router
// ---------------------------------------------------------------------------

/// The GPU decoder and the two gates in front of it.
///
/// **One decoder, not one per language**, which is the whole reason the GPU
/// backend is affordable at all: `whisper-cli` takes the language as an
/// argument, so six languages are six command lines over one 1.03 GB model
/// file that is loaded per invocation rather than held. Contrast the design
/// this replaced, where each language was its own resident 160 MB sherpa
/// session.
///
/// Holds no identifier: it reads the reading [`crate::asr_ja`] already paid
/// for.
pub struct Polyglot {
    enabled: bool,
    night: NightModels,
    /// `None` when the night shift is not installed, which is the normal state
    /// and is said once. Built eagerly because nothing about it is expensive —
    /// the model is loaded by the child process, not by this struct.
    whisper: Option<crate::night::Whisper>,
    /// The same ceiling the night shift itself checks before every batch.
    gpu_busy_max_pct: u32,
    /// Where the one-turn WAV is written and immediately deleted. Per process,
    /// like the night shift's own GPU probe.
    scratch: std::path::PathBuf,
}

impl Polyglot {
    pub fn new(
        models: &ModelSet,
        cfg: &AsrConfig,
        night_cfg: &crate::config::NightConfig,
        runtime: &crate::config::RuntimeConfig,
    ) -> Self {
        let night = NightModels::resolve_at(models.root.clone(), night_cfg);
        let whisper = night
            .present()
            .then(|| crate::night::Whisper::new(&night, night_cfg, runtime));
        Self {
            enabled: cfg.polyglot,
            whisper,
            gpu_busy_max_pct: night_cfg.gpu_busy_max_pct,
            scratch: std::env::temp_dir().join(format!("nxr-poly-{}", std::process::id())),
            night,
        }
    }

    /// Switched on, and is there a decoder good enough to arbitrate with? The
    /// cheapest possible early exit, read before every turn.
    pub fn ready(&self) -> bool {
        self.enabled && self.whisper.is_some()
    }

    /// The line the daemon logs once at start-up, `None` when there is nothing
    /// worth saying.
    pub fn startup_note(&self, cfg: &AsrConfig) -> Option<String> {
        if !self.enabled {
            return Some(
                "[asr].polyglot is off: a short French or Spanish turn the multilingual \
                 decoder flipped into English will stay that way."
                    .to_string(),
            );
        }
        if self.whisper.is_none() {
            return Some(format!(
                "a short turn in another language cannot be re-decoded: the only decoder \
                 measured to beat the multilingual model at this is whisper-large-v3 on the \
                 GPU, and {}",
                NightModels::how_to_get_it()
            ));
        }
        let shipped: Vec<&str> = cfg
            .polyglot_languages
            .iter()
            .filter(|l| is_routable(l))
            .map(|l| l.as_str())
            .collect();
        Some(format!(
            "the spoken-language router can re-decode into {}",
            shipped.join(", ")
        ))
    }

    /// Decode `samples` again forced to `want`, applying every guard. Decides
    /// nothing about the database and touches none of it.
    ///
    /// The order is the cheap, certain refusals first, so a turn that was never
    /// going to be re-read costs no GPU and no file:
    ///
    /// 1. under `[asr].lid_min_s` → [`Rerouted::TooShort`];
    /// 2. no night shift → [`Rerouted::Unavailable`];
    /// 3. the GPU is busy → [`Rerouted::GpuBusy`], and the row stands;
    /// 4. write one WAV, run one `whisper-cli`, delete the WAV, judge.
    pub fn redecode(
        &mut self,
        want: &str,
        samples: &[f32],
        prior: &str,
        lang_cfg: &LangConfig,
        asr_cfg: &AsrConfig,
    ) -> Rerouted {
        if (samples.len() as f32 / SAMPLE_RATE as f32) < asr_cfg.lid_min_s {
            return Rerouted::TooShort;
        }
        let Some(whisper) = self.whisper.as_ref() else {
            return Rerouted::Unavailable;
        };
        // The GPU belongs to whatever the user is doing. A turn that is
        // already transcribed — wrongly, but transcribed — is not worth a
        // dropped frame, so this refuses rather than waits.
        if let Some(pct) = crate::night::gpu_busy_pct()
            && pct > self.gpu_busy_max_pct
        {
            return Rerouted::GpuBusy { pct };
        }
        if let Err(e) = std::fs::create_dir_all(&self.scratch) {
            warn!("could not make the re-decode scratch directory: {e:#}");
            return Rerouted::Failed;
        }
        let wav = self.scratch.join("turn.wav");
        if let Err(e) = crate::pipeline::write_wav(&wav, samples) {
            warn!("could not write the turn out for re-decoding: {e:#}");
            return Rerouted::Failed;
        }
        let decoded = whisper.decode(&wav, Some(want));
        let _ = std::fs::remove_file(&wav);
        let raw = match decoded {
            // `whisper-cli` returns the turn as one or more timed lines; this
            // route wants the words and not the timings, and a 1.2 s fragment
            // is one line in practice.
            Ok((lines, _)) => lines
                .iter()
                .map(|u| u.text.trim())
                .collect::<Vec<_>>()
                .join(" "),
            Err(e) => {
                warn!("the {want} re-decode failed: {e:#}");
                return Rerouted::Failed;
            }
        };
        match judge(&raw, want, prior, lang_cfg) {
            Ok(text) => Rerouted::Replaced {
                text,
                model_id: model_id(&self.night, want),
            },
            Err(rejected) => rejected,
        }
    }
}

/// One turn as this route sees it.
///
/// `heard` is the identifier's answer, which the caller **already has**: the
/// Japanese route ran LID on this same turn a moment ago, and asking twice
/// would double the only cost either feature has. That is why this function
/// takes a `Reading` rather than a [`Polyglot`]-owned identifier.
#[derive(Debug, Clone, Copy)]
pub struct Turn<'a> {
    pub segment_id: i64,
    /// What the live decoder made of the audio, and what a re-decode would
    /// overwrite.
    pub text: Option<&'a str>,
    pub samples: &'a [f32],
    /// The identifier's reading of this turn, or `None` when it was not asked.
    pub heard: Option<&'a Reading>,
    pub lang_cfg: &'a LangConfig,
    pub asr_cfg: &'a AsrConfig,
}

/// What the route did to one turn.
#[derive(Debug, Clone, PartialEq)]
pub struct Routed {
    pub text: String,
    pub asr_model_id: String,
    /// The language the row now says, which is the one the identifier named.
    pub lang: String,
}

/// The whole route for one turn: read the identifier's answer, maybe decode,
/// and write.
///
/// Returns `Ok(None)` for the overwhelmingly common case of a turn the
/// identifier did not name a routable language for.
///
/// Writes the same two things the Japanese route writes, in the same order and
/// through the same calls — [`Store::set_segment_text_via`] first, so the prior
/// text lands in `operations` as a `segments.redecode` row a person can compare
/// or revert, then the language. A row corrected here is indistinguishable in
/// shape from one the Japanese route corrected, which is the point of sharing
/// [`LANG_VIA_LID`].
///
/// The live translation queue picks the row up from there: `segments.lang` now
/// says `fr`, and [`crate::translate`] does the rest without knowing this
/// module exists.
pub fn route_segment(
    poly: &mut Polyglot,
    store: &Store,
    turn: Turn<'_>,
    now_utc_ns: i64,
) -> anyhow::Result<Option<Routed>> {
    let Turn {
        segment_id,
        text,
        samples,
        heard,
        lang_cfg,
        asr_cfg,
    } = turn;
    if !poly.ready() {
        return Ok(None);
    }
    let Some(want) = post_route(heard, asr_cfg) else {
        return Ok(None);
    };
    let prior = text.unwrap_or("");
    match poly.redecode(&want, samples, prior, lang_cfg, asr_cfg) {
        Rerouted::Replaced {
            text: new,
            model_id,
        } => {
            store.set_segment_text_via(
                segment_id,
                &new,
                &model_id,
                crate::store::text_via::LID,
                now_utc_ns,
            )?;
            store.set_segment_language(segment_id, &want, LANG_VIA_LID)?;
            info!(
                segment_id,
                model = %model_id,
                lang = %want,
                "re-decoded a turn the multilingual model had flipped into another language"
            );
            Ok(Some(Routed {
                text: new,
                asr_model_id: model_id,
                lang: want,
            }))
        }
        other => {
            debug!(segment_id, heard = %want, outcome = ?other, "the re-decode did not stand");
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AsrConfig;

    fn reading(lang: &str, confidence: f32) -> Reading {
        Reading {
            lang: lang.into(),
            confidence,
        }
    }

    #[test]
    fn every_routable_language_has_a_judge_that_can_read_it() {
        // The invariant that keeps guard 3 applicable: a language the route can
        // decode into must be one `guess_other` has an opinion about, or the
        // re-decode could never be checked and the route would be the flip in
        // the other direction.
        for tag in ROUTABLE {
            assert!(
                lang::GUESSABLE.contains(tag),
                "{tag} is routable and unjudgeable"
            );
        }
        // The shipped default is the MEASURED subset, not the whole catalogue.
        // Those are different lists on purpose (FINDINGS §28) and the day they
        // stop being different should be the day somebody measured the rest.
        let cfg = AsrConfig::default();
        assert_eq!(cfg.polyglot_languages, MEASURED);
        for tag in MEASURED {
            assert!(is_routable(tag), "{tag} is defaulted on and unroutable");
        }
        assert!(MEASURED.len() < ROUTABLE.len());
    }

    #[test]
    fn each_forced_language_gets_its_own_model_id() {
        // One decoder, one model file, six decoding contracts. A row re-read in
        // French must never claim to have been written by the pass that re-read
        // one in Spanish, even though it was the same weights and the same
        // binary — the same rule `WhisperExport::model_id` follows.
        let night = NightModels::resolve_at(
            std::path::PathBuf::from("/nonexistent/nx-recall-audit"),
            &crate::config::NightConfig::default(),
        );
        assert_ne!(model_id(&night, "fr"), model_id(&night, "es"));
        assert!(model_id(&night, "fr").ends_with("-fr"));
        // …and it is recognisably the night shift's model, because it is.
        assert!(model_id(&night, "fr").starts_with(&night.model_id()));
        assert_ne!(model_id(&night, "fr"), night.model_id());
    }

    #[test]
    fn the_allowlist_is_the_config_and_the_catalogue_together() {
        let cfg = AsrConfig::default();
        assert!(routable("fr", &cfg));
        assert!(routable("es", &cfg));
        // In the catalogue, not in the shipped default: nothing about Dutch was
        // measured, so an operator has to ask for it by name.
        assert!(is_routable("nl"));
        assert!(!routable("nl", &cfg));
        assert!(routable(
            "nl",
            &AsrConfig {
                polyglot_languages: vec!["nl".into()],
                ..AsrConfig::default()
            }
        ));
        // German and English are never routable here. They have their own
        // arbiters (`crate::arbiter`), reached from text, and routing them by
        // ear as well is how two features start fighting over one column.
        assert!(!routable("de", &cfg));
        assert!(!routable("en", &cfg));
        // Japanese is the other route's, and Korean and Chinese are nobody's
        // yet: no export, so no amount of configuration makes them routable.
        for tag in ["ja", "ko", "zh", "ru", ""] {
            assert!(!routable(tag, &cfg), "{tag}");
            assert!(!is_routable(tag), "{tag}");
        }
        // An operator may shorten the list…
        let only_fr = AsrConfig {
            polyglot_languages: vec!["fr".into()],
            ..AsrConfig::default()
        };
        assert!(routable("fr", &only_fr));
        assert!(!routable("es", &only_fr));
        // …may not lengthen it past the catalogue…
        let wishful = AsrConfig {
            polyglot_languages: vec!["fr".into(), "ko".into()],
            ..AsrConfig::default()
        };
        assert!(!routable("ko", &wishful));
        // …and the master switch beats both.
        let off = AsrConfig {
            polyglot: false,
            ..AsrConfig::default()
        };
        assert!(!routable("fr", &off));
    }

    #[test]
    fn the_identifier_has_to_name_a_language_this_route_can_act_on() {
        let cfg = AsrConfig::default();
        assert_eq!(
            post_route(Some(&reading("fr", 1.0)), &cfg),
            Some("fr".into())
        );
        assert_eq!(
            post_route(Some(&reading("it", 1.0)), &cfg),
            Some("it".into())
        );
        // The overwhelmingly common answers, and they stop here.
        assert_eq!(post_route(Some(&reading("de", 1.0)), &cfg), None);
        assert_eq!(post_route(Some(&reading("en", 1.0)), &cfg), None);
        // Japanese belongs to `asr_ja`, which runs FIRST and has already dealt
        // with it. Answering `ja` here as well would decode the turn twice.
        assert_eq!(post_route(Some(&reading("ja", 1.0)), &cfg), None);
        // No opinion is not an opinion, and neither is not having asked.
        assert_eq!(post_route(None, &cfg), None);
        // Under the operating point, once a machine has raised it.
        let strict = AsrConfig {
            lid_min_confidence: 0.9,
            ..AsrConfig::default()
        };
        assert_eq!(post_route(Some(&reading("fr", 0.66)), &strict), None);
        assert_eq!(
            post_route(Some(&reading("fr", 1.0)), &strict),
            Some("fr".into())
        );
    }

    #[test]
    fn a_re_decode_that_does_not_read_as_the_target_is_thrown_away() {
        let cfg = LangConfig::default();
        let prior = "During apartments.";

        // THE case, and the whole feature: the flipped English words go, the
        // French ones stay.
        let french = "Tu arrêtes de le faire, ce n'est pas pour nous, mais pour elle";
        assert_eq!(judge(french, "fr", prior, &cfg).unwrap(), french);

        // The dangerous case, and the one Japanese never had: the identifier
        // was wrong, the audio was German, and Whisper forced to French
        // produced… English-looking mush. It reads as nothing, so it stands
        // for nothing.
        assert!(matches!(
            judge("during apartments", "fr", prior, &cfg),
            Err(Rerouted::Rejected {
                read_as: "unclear",
                ..
            })
        ));
        // …and the same when it comes back reading as a DIFFERENT language
        // from the one the identifier named. Italian and Spanish are the pair
        // the identifier confuses most (FINDINGS §28), so this is not
        // hypothetical.
        let spanish = "el que los las del y en un una por para con su como más pero";
        assert!(matches!(
            judge(spanish, "it", prior, &cfg),
            Err(Rerouted::Rejected { read_as: "es", .. })
        ));

        // Nothing at all, and one word, and a caption — the same three the
        // German arbiter refuses, refused here for the same reasons.
        assert!(judge("", "fr", prior, &cfg).is_err());
        assert!(judge("oui", "fr", prior, &cfg).is_err());
        assert!(judge("(musique douce)", "fr", prior, &cfg).is_err());
    }

    #[test]
    fn a_re_decode_that_agrees_with_the_row_has_corrected_nothing() {
        let cfg = LangConfig::default();
        // Guard 3. An "arbiter" whose answer is the words already on the row
        // has found nothing, and letting it through would write an operations
        // row recording a change that did not happen.
        let same = "le la les des une est ne pas que qui pour dans";
        assert!(matches!(
            judge(same, "fr", same, &cfg),
            Err(Rerouted::Rejected { echo: true, .. })
        ));
        // Punctuation and case are not a correction either.
        assert!(matches!(
            judge(
                "Le la les des une est ne pas que qui, pour dans!",
                "fr",
                same,
                &cfg
            ),
            Err(Rerouted::Rejected { echo: true, .. })
        ));
        // But the same words against a different row are a correction.
        assert!(judge(same, "fr", "during apartments", &cfg).is_ok());
    }

    #[test]
    fn a_hedged_reading_of_the_words_is_not_enough_to_stamp_the_row() {
        let cfg = LangConfig::default();
        // `guess_other` answers with a tag AND a confidence, and the two are
        // used differently everywhere else in this daemon: any guess is enough
        // to try a translation, only a confident one is written into
        // `segments.lang`. This writes `segments.lang`.
        let three_votes = "le la les chou";
        let g = lang::guess_other(three_votes);
        if let Some(g) = g.filter(|g| g.tag == "fr" && !g.confident) {
            assert!(!g.confident);
            assert!(matches!(
                judge(three_votes, "fr", "mon petit chou", &cfg),
                Err(Rerouted::Rejected { .. })
            ));
        }
    }

    #[test]
    fn a_row_this_route_settles_looks_exactly_like_one_the_japanese_route_did() {
        // The same `lang_via`, on purpose: the claim is identical — a model
        // listened — and only the decoder differs, which `asr_model_id`
        // already records.
        assert_eq!(LANG_VIA_LID, "lid");
        assert_eq!(LANG_VIA_LID, crate::asr_ja::LANG_VIA_LID);
        assert_eq!(crate::store::text_via::LID, "lid");
    }

    /// A router pointed at a directory with nothing in it: switched on, and
    /// with no decoder behind the switch.
    fn a_router_with_no_night_shift(cfg: &AsrConfig) -> Polyglot {
        let models = ModelSet::resolve_at(
            std::path::PathBuf::from("/nonexistent/nx-recall-audit"),
            &crate::config::ModelsConfig::default(),
        );
        Polyglot::new(
            &models,
            cfg,
            &crate::config::NightConfig::default(),
            &crate::config::RuntimeConfig::default(),
        )
    }

    #[test]
    fn without_the_night_shift_the_route_is_not_ready_and_says_why() {
        // The normal state on almost every machine: `--night` is a gigabyte
        // AND a local compile, and this feature is the second thing to want
        // it. Not ready, and one line at start-up rather than one per turn.
        let on = AsrConfig::default();
        let mut poly = a_router_with_no_night_shift(&on);
        assert!(!poly.ready());
        let note = poly.startup_note(&on).expect("a line");
        assert!(note.contains("night"), "{note}");

        // And nothing is spent on a turn either — not a WAV, not a GPU check.
        assert_eq!(
            poly.redecode(
                "fr",
                &vec![0.0f32; 32_000],
                "during apartments",
                &LangConfig::default(),
                &on
            ),
            Rerouted::Unavailable
        );
        // The duration floor is checked FIRST, so a fragment under it does not
        // even reach the "is anything installed" question.
        assert_eq!(
            poly.redecode("fr", &vec![0.0f32; 8_000], "x", &LangConfig::default(), &on),
            Rerouted::TooShort
        );

        // Switched off is a different line, and it is the one that names the
        // failure rather than the missing download.
        let off = AsrConfig {
            polyglot: false,
            ..AsrConfig::default()
        };
        let note = a_router_with_no_night_shift(&off)
            .startup_note(&off)
            .expect("a line");
        assert!(note.contains("off"), "{note}");
    }

    #[test]
    fn the_shipped_operating_point_is_the_measured_one() {
        let cfg = AsrConfig::default();
        // The floor that makes the three live rows reachable at all: 1.01,
        // 1.20 and 1.30 seconds (FINDINGS §28).
        assert_eq!(cfg.lid_min_s, 1.0);
        for live_row_s in [1.01f32, 1.20, 1.30] {
            assert!(live_row_s >= cfg.lid_min_s, "{live_row_s}");
        }
        // It is DELIBERATELY below the arbiter's replacement floor. Those were
        // the same number until 0.11.6 and they answer different questions —
        // that one is "are these better words", this one is "what language is
        // this" — and fusing them is why none of the three was ever asked
        // about.
        assert!(cfg.lid_min_s < LangConfig::default().arbiter_min_duration_s);
        assert!(cfg.polyglot);
        assert_eq!(cfg.polyglot_languages, ["fr", "es", "it"]);
    }

    /// What the route does, end to end, without a model in the process.
    ///
    /// The three stages are three functions in two modules and each is tested
    /// alone above; this is the table that says what they do *together*, which
    /// is the thing a reader actually wants to know and the thing that breaks
    /// when one of them is changed in isolation.
    #[test]
    fn the_routing_table() {
        let asr = AsrConfig::default();
        let lang_cfg = LangConfig::default();

        /// Every stage: does the turn reach the identifier, what does the
        /// identifier's answer make of it, and does the re-decode stand?
        fn walk(
            v3: &str,
            heard: &str,
            redecode: &str,
            asr: &AsrConfig,
            lang_cfg: &LangConfig,
        ) -> &'static str {
            if crate::asr_ja::pre_route(None, Some(v3)) == crate::asr_ja::Pre::Nothing {
                return "never asked";
            }
            let Some(want) = post_route(
                Some(&Reading {
                    lang: heard.into(),
                    confidence: 1.0,
                }),
                asr,
            ) else {
                return "not routable";
            };
            match judge(redecode, &want, v3, lang_cfg) {
                Ok(_) => "replaced",
                Err(_) => "kept original",
            }
        }

        // THE case. "During apartments." is two ordinary English words over
        // French audio: `classify` cannot see it, `guess_other` cannot see it,
        // and only the identifier can. The re-decode comes back French and
        // wins.
        assert_eq!(
            walk(
                "During apartments.",
                "fr",
                "Tu arrêtes de le faire, ce n'est pas pour nous mais pour elle",
                &asr,
                &lang_cfg
            ),
            "replaced"
        );

        // A clear German transcript never reaches the identifier at all, and
        // that is a cost decision as much as a correctness one — it is the
        // overwhelmingly common turn on this user's machine.
        assert_eq!(
            walk(
                "ich glaube das ist der einzige weg",
                "de",
                "",
                &asr,
                &lang_cfg
            ),
            "never asked"
        );
        // …and neither does a clear English one, or one already readable as
        // French: some decoder got those right.
        assert_eq!(
            walk("i think that is the only way", "en", "", &asr, &lang_cfg),
            "never asked"
        );
        assert_eq!(
            walk(
                "le la les des une est ne pas que qui pour dans",
                "fr",
                "anything",
                &asr,
                &lang_cfg
            ),
            "never asked"
        );

        // Unreadable, but the identifier says a language nothing here can act
        // on. German and English have their own arbiters, reached from text;
        // Japanese has its own route, which ran first.
        for heard in ["de", "en", "ja", "ko", "ru"] {
            assert_eq!(
                walk("mumble mumble", heard, "irgendwas", &asr, &lang_cfg),
                "not routable",
                "{heard}"
            );
        }

        // The dangerous one, and the guard the whole module turns on: the
        // identifier said French, the audio was not, and Whisper forced to
        // French produced something that does not read as French. The original
        // words stand — which is the same state the row was already in, so a
        // false positive from the identifier costs one decode and nothing else.
        assert_eq!(
            walk(
                "During apartments.",
                "fr",
                "during apartments there",
                &asr,
                &lang_cfg
            ),
            "kept original"
        );
        // Including when what came back is perfectly good French — belonging
        // to the row it was supposed to correct. An echo is not a correction.
        let french = "le la les des une est ne pas que qui pour dans";
        assert_eq!(
            walk("mumble mumble", "fr", french, &asr, &lang_cfg),
            "replaced"
        );
        assert_eq!(walk(french, "fr", french, &asr, &lang_cfg), "never asked");
    }

    /// The measurement bridge (FINDINGS §28).
    ///
    /// `spike/asr_poly.py` produces the decodes; the two guards that decide
    /// whether any of them is *kept* live here and in [`crate::lang`], so the
    /// numbers that matter cannot be computed in Python without a second copy
    /// of the rules that would immediately drift from this one.
    ///
    /// ```text
    /// NXR_POLY_SPIKE=/run/media/.../nx-scratch/asr_poly.json \
    ///   cargo test -p recalld --lib -- --ignored --nocapture the_spike
    /// ```
    #[test]
    #[ignore = "needs spike/asr_poly.py's output"]
    fn the_spike_through_the_shipped_guards() {
        use std::collections::BTreeMap;

        let path = std::env::var("NXR_POLY_SPIKE").expect("set NXR_POLY_SPIKE");
        let raw = std::fs::read_to_string(&path).expect("reading the spike file");
        let doc: serde_json::Value = serde_json::from_str(&raw).expect("parsing it");
        let lang_cfg = LangConfig::default();
        let asr_cfg = AsrConfig::default();

        // ---- who reaches the identifier at all --------------------------
        // `pre_route` is the first narrowing and the biggest one. Counted per
        // (truth, length) so the negatives' denominator is honest.
        let mut reach: BTreeMap<(String, String), (usize, usize)> = BTreeMap::new();
        let mut named: BTreeMap<(String, String), usize> = BTreeMap::new();
        for s in doc["seen"].as_array().unwrap() {
            let truth = s["truth"].as_str().unwrap().to_string();
            let length = s["length"].to_string();
            let v3 = s["v3"].as_str().unwrap_or("");
            let heard = s["heard"].as_str().unwrap_or("");
            let e = reach.entry((truth.clone(), length.clone())).or_default();
            e.1 += 1;
            let asked = crate::asr_ja::pre_route(None, Some(v3)) != crate::asr_ja::Pre::Nothing;
            if asked {
                e.0 += 1;
                if routable(heard, &asr_cfg) {
                    *named.entry((truth, length)).or_default() += 1;
                }
            }
        }

        println!("\n## Who reaches the route (pre_route, then a routable reading)\n");
        println!("| truth | length | n | reaches LID | LID names a routed language |");
        println!("|-------|--------|--:|------------:|----------------------------:|");
        for ((truth, length), (asked, n)) in &reach {
            let hit = named
                .get(&(truth.clone(), length.clone()))
                .copied()
                .unwrap_or(0);
            println!(
                "| {truth} | {length} | {n} | {:.1}% | {:.1}% |",
                100.0 * *asked as f64 / *n as f64,
                100.0 * hit as f64 / *n as f64,
            );
        }

        // ---- and of those, who survives the judge -----------------------
        /// Word-level edit distance against the best-matching SUBSTRING of the
        /// reference, normalised by the hypothesis. The Rust half of
        /// `spike/asr_poly.py`'s `infix_wer`, and here rather than there
        /// because only this side knows which turns were kept.
        fn infix(reference: &[String], hyp: &[String]) -> (usize, usize) {
            if hyp.is_empty() {
                return (0, 0);
            }
            let m = reference.len();
            let mut prev = vec![0usize; m + 1];
            for (i, h) in hyp.iter().enumerate() {
                let mut cur = vec![0usize; m + 1];
                cur[0] = i + 1;
                for j in 1..=m {
                    cur[j] = (prev[j] + 1)
                        .min(cur[j - 1] + 1)
                        .min(prev[j - 1] + usize::from(h != &reference[j - 1]));
                }
                prev = cur;
            }
            (prev.into_iter().min().unwrap_or(0), hyp.len())
        }

        let mut kept: BTreeMap<(String, String, String), (usize, usize)> = BTreeMap::new();
        // Over the turns actually REPLACED: the words before, the words after,
        // and how many rows the route made worse. Nothing else can move — a
        // rejected re-decode leaves the row exactly as it was.
        let mut wer: BTreeMap<(String, String, String), [usize; 5]> = BTreeMap::new();
        for r in doc["rows"].as_array().unwrap() {
            let truth = r["truth"].as_str().unwrap().to_string();
            let length = r["length"].to_string();
            let backend = r["backend"].as_str().unwrap().to_string();
            let forced = r["forced"].as_str().unwrap();
            let heard = r["heard"].as_str().unwrap_or("");
            let v3 = r["v3"].as_str().unwrap_or("");
            let hyp = r["hyp"].as_str().unwrap_or("");
            // The live route, in full: unreadable transcript, a routable
            // reading, and the reading is what we forced.
            if crate::asr_ja::pre_route(None, Some(v3)) == crate::asr_ja::Pre::Nothing {
                continue;
            }
            if heard != forced || !routable(heard, &asr_cfg) {
                continue;
            }
            let key = (truth, length, backend);
            let e = kept.entry(key.clone()).or_default();
            e.1 += 1;
            let Ok(new) = judge(hyp, forced, v3, &lang_cfg) else {
                continue;
            };
            e.0 += 1;
            let reference = normalise_words(r["ref"].as_str().unwrap_or(""))
                .into_iter()
                .map(|w| w.to_lowercase())
                .collect::<Vec<_>>();
            let low = |s: &str| {
                normalise_words(s)
                    .into_iter()
                    .map(|w| w.to_lowercase())
                    .collect::<Vec<_>>()
            };
            let (be, bn) = infix(&reference, &low(v3));
            let (ae, an) = infix(&reference, &low(&new));
            let slot = wer.entry(key).or_insert([0; 5]);
            slot[0] += be;
            slot[1] += bn;
            slot[2] += ae;
            slot[3] += an;
            // Per turn, as a rate, because the two hypotheses have different
            // lengths and a raw error count would call a longer correct
            // transcript worse than a shorter wrong one.
            if bn > 0 && an > 0 && (ae as f64 / an as f64) > (be as f64 / bn as f64) {
                slot[4] += 1;
            }
        }

        println!("\n## Of the turns the route touches, how many replacements are KEPT\n");
        println!("| truth | length | backend | touched | kept | rate |");
        println!("|-------|--------|---------|--------:|-----:|-----:|");
        for ((truth, length, backend), (ok, n)) in &kept {
            println!(
                "| {truth} | {length} | {backend} | {n} | {ok} | {:.1}% |",
                100.0 * *ok as f64 / *n as f64
            );
        }
        println!(
            "\nA `de` or `en` row in that table is a FALSE POSITIVE that survived every guard, \
             and is the number the route lives or dies by."
        );

        println!("\n## On the turns the route actually REPLACES: was it worth it\n");
        println!("| truth | length | backend | replaced | WER before | WER after | rel. | worse |");
        println!("|-------|--------|---------|---------:|-----------:|----------:|-----:|------:|");
        for ((truth, length, backend), s) in &wer {
            let n = kept
                .get(&(truth.clone(), length.clone(), backend.clone()))
                .map_or(0, |k| k.0);
            let before = s[0] as f64 / s[1].max(1) as f64;
            let after = s[2] as f64 / s[3].max(1) as f64;
            println!(
                "| {truth} | {length} | {backend} | {n} | {:.1}% | {:.1}% | {:+.1}% | {:.1}% |",
                100.0 * before,
                100.0 * after,
                100.0 * (before - after) / before,
                100.0 * s[4] as f64 / n.max(1) as f64,
            );
        }
        println!(
            "\nGate: >= 30% relative WER improvement on the touched turns, <= 5% of them \
             made worse."
        );
    }
}
