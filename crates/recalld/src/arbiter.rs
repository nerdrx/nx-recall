//! The flip arbiters: decoders that are told which language to hear.
//!
//! A multilingual transducer does not merely fail to identify a language on a
//! short fragment — it *commits to the wrong one*. `spike/lang_flip.py` put
//! that at 12% of 1 s windows and 5% of 2 s ones on German read speech, against
//! a median real turn of 2.4 s. What comes back is not a slightly wrong
//! transcript: it is 103%-WER nonsense, English-shaped words over German audio.
//!
//! An arbiter is the second opinion, asked *only* about a fragment something
//! else already got wrong, and asked under a hard constraint:
//!
//! * **English** — the Parakeet 110m export (`FALLBACK_ASR`). Its language is a
//!   property of the weights; it cannot produce German at all. This is the
//!   0.6.1 path, unchanged except that it now also answers to a thread context
//!   rather than only to a speaker's declaration.
//! * **German** — Whisper base, forced to `language=de` (0.7.7, `ARBITER_DE`).
//!   There is no German-only transducer, and Whisper's language token is the
//!   one honest way to force the constraint.
//!
//! ### Why Whisper, when Step 0 rejected Whisper
//!
//! It did, and for good reasons that still hold: 4x Parakeet's WER, and a
//! caption-style hallucination habit — `(soft music)`, `[Applause]` — that
//! Parakeet does not have on silence, noise or music (FINDINGS §9). Neither
//! disqualifies it *here*, because the bar is not "beat Parakeet". The bar is
//! "beat a flip", and a flip is garbage. `spike/arbiter_de.py` measured the
//! arbiter against exactly that bar, on the same fragments the flip was found
//! on:
//!
//! | window | reads de | flips to en | empty | word precision |
//! |--------|---------:|------------:|------:|---------------:|
//! | 1.0 s  |          |       2-3%  |       |          28%   |
//! | 1.5 s  |          |       2-3%  |   ~0% |          54%   |
//! | 3.0 s  |          |       2-3%  |   ~0% |          66%   |
//!
//! Two decisions fall straight out of that table and are enforced in
//! [`Arbiters::arbitrate`]:
//!
//! 1. **1.5 s is the replacement floor.** At 1.0 s the arbiter's own words are
//!    in the reference 28% of the time, and replacing one wrong transcript with
//!    a differently wrong one is not a correction. Below the floor the row is
//!    *marked* and its words are kept.
//! 2. **The hallucination filter is real and is scoped here.** It is Whisper's
//!    failure mode, it is measured, and it costs one string scan — so
//!    bracketed caption text is stripped before the classifier is allowed to
//!    read the output. Without that, `(Musik)` on a non-speech fragment reads
//!    as a perfectly good German transcript.

use tracing::{info, warn};

use crate::asr::{Asr, normalise_words};
use crate::config::{LangConfig, SAMPLE_RATE};
use crate::lang::{self, Lang};
use crate::models::{ARBITER_DE, ArbiterModel, FALLBACK_ASR, ModelSet};

/// What asking an arbiter produced.
#[derive(Debug, Clone, PartialEq)]
pub enum Arbitration {
    /// The re-decode cleared every guard. These words replace the row's, and
    /// `asr_model_id` moves with them.
    Replaced { text: String, model_id: String },
    /// Too short to replace anything (`[lang].arbiter_min_duration_s`). The
    /// model was not even run: at this length its answer is not better than the
    /// one it would overwrite.
    TooShort,
    /// No decoder for that language is installed. Said once per daemon, not
    /// once per segment.
    Unavailable,
    /// The arbiter ran and its answer failed a guard — empty, too few words, or
    /// it came back reading as the language we were trying to get away from.
    /// The original words stand.
    Rejected { read_as: &'static str, words: usize },
}

/// Both constrained decoders, loaded the first time each is needed and resident
/// from then on.
///
/// `*_unavailable` is not the same as `*_asr == None`: "not tried yet" and
/// "tried, not installed" differ, and the difference is what keeps a missing
/// optional model to one warning line rather than one per turn.
pub struct Arbiters {
    models: ModelSet,
    en: Option<Asr>,
    en_unavailable: bool,
    de: Option<Whisper>,
    de_unavailable: bool,
}

impl Arbiters {
    pub fn new(models: &ModelSet) -> Self {
        Self {
            models: models.clone(),
            en: None,
            en_unavailable: false,
            de: None,
            de_unavailable: false,
        }
    }

    /// Which languages this machine can actually arbitrate, for `models status`
    /// and for the log line at start-up. Reads the disk; loads nothing.
    pub fn installed(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.models.has_asr_export(&FALLBACK_ASR) {
            out.push("en");
        }
        if self.models.arbiter(ARBITER_DE).present() {
            out.push("de");
        }
        out
    }

    /// Decode `samples` again under a hard `want` constraint, applying every
    /// measured guard. The caller writes the row; this decides nothing about
    /// the database and touches none of it.
    ///
    /// The order is deliberate — the cheap, certain refusals come first, so a
    /// 0.8 s fragment never costs a model pass:
    ///
    /// 1. under the duration floor → [`Arbitration::TooShort`], model not run;
    /// 2. no decoder installed → [`Arbitration::Unavailable`];
    /// 3. run it, strip caption hallucinations, then require the result to be
    ///    non-empty, at least `arbiter_min_words` long, and to *read as* `want`.
    ///    A German re-decode that comes back reading as English has told us
    ///    something — that the flip was not a flip — and it has not earned the
    ///    right to overwrite anything.
    pub fn arbitrate(&mut self, want: Lang, samples: &[f32], cfg: &LangConfig) -> Arbitration {
        let duration_s = samples.len() as f32 / SAMPLE_RATE as f32;
        if duration_s < cfg.arbiter_min_duration_s {
            return Arbitration::TooShort;
        }
        let (raw, model_id) = match want {
            Lang::En => match self.english() {
                Some(asr) => {
                    let id = asr.model_id().to_string();
                    (asr.transcribe(samples), id)
                }
                None => return Arbitration::Unavailable,
            },
            Lang::De => match self.german() {
                Some(w) => {
                    let id = w.model_id().to_string();
                    (w.transcribe(samples), id)
                }
                None => return Arbitration::Unavailable,
            },
            // Not a language, so there is nothing to force a decoder to.
            Lang::Unclear | Lang::Empty => return Arbitration::Unavailable,
        };

        match judge(&raw, want, cfg) {
            Ok(text) => Arbitration::Replaced { text, model_id },
            Err(rejected) => rejected,
        }
    }

    /// The English-only export, loaded on demand and kept.
    ///
    /// It has not been part of the default model set since 0.5.6 (the
    /// multilingual export beats it at English too), so the honest answer here
    /// is often "not installed" — and that is said once.
    pub fn english(&mut self) -> Option<&mut Asr> {
        if self.en.is_none() && !self.en_unavailable {
            if !self.models.has_asr_export(&FALLBACK_ASR) {
                self.en_unavailable = true;
                warn!(
                    "a transcript looks like a wrong-language decode, but the English-only \
                     export is not installed under {} — nothing to re-decode with. \
                     `recalld models fetch --fallback-asr` installs it ({}).",
                    self.models.root.display(),
                    FALLBACK_ASR.note
                );
            } else {
                match Asr::load(&self.models.with_asr(&FALLBACK_ASR)) {
                    Ok(asr) => {
                        info!(
                            model = asr.model_id(),
                            "loaded the English-only decoder as the English flip arbiter"
                        );
                        self.en = Some(asr);
                    }
                    Err(e) => {
                        self.en_unavailable = true;
                        warn!("could not load the English-only decoder: {e:#}");
                    }
                }
            }
        }
        self.en.as_mut()
    }

    /// The German arbiter, loaded on demand and kept (0.7.7).
    pub fn german(&mut self) -> Option<&mut Whisper> {
        if self.de.is_none() && !self.de_unavailable {
            let model = self.models.arbiter(ARBITER_DE);
            if !model.present() {
                self.de_unavailable = true;
                warn!(
                    "a transcript looks like a German turn decoded as English, but {}",
                    ArbiterModel::how_to_get_it()
                );
            } else {
                match Whisper::load(&model, self.models.asr_threads) {
                    Ok(w) => {
                        info!(
                            model = %w.model_id(),
                            "loaded the German flip arbiter ({})", ARBITER_DE.note
                        );
                        self.de = Some(w);
                    }
                    Err(e) => {
                        self.de_unavailable = true;
                        warn!("could not load the German arbiter: {e:#}");
                    }
                }
            }
        }
        self.de.as_mut()
    }
}

/// A Whisper export with its language token nailed down.
pub struct Whisper {
    recognizer: sherpa_rs::whisper::WhisperRecognizer,
    model_id: String,
}

impl Whisper {
    pub fn load(model: &ArbiterModel, threads: i32) -> anyhow::Result<Self> {
        let path = |p: &std::path::Path| -> anyhow::Result<String> {
            Ok(p.to_str()
                .ok_or_else(|| anyhow::anyhow!("model path {} is not valid UTF-8", p.display()))?
                .to_string())
        };
        let recognizer =
            sherpa_rs::whisper::WhisperRecognizer::new(sherpa_rs::whisper::WhisperConfig {
                encoder: path(&model.encoder)?,
                decoder: path(&model.decoder)?,
                tokens: path(&model.tokens)?,
                // The whole point. Whisper's language is a decoding parameter,
                // which is what makes it usable as a constraint at all.
                language: model.export.lang.to_string(),
                // Same budget as the primary ASR ([models].asr_threads): this
                // runs on the same deprioritised thread and answers about one
                // fragment at a time.
                num_threads: Some(threads.max(1)),
                ..Default::default()
            })
            .map_err(|e| anyhow::anyhow!("loading the {} arbiter: {e}", model.export.lang))?;
        Ok(Self {
            recognizer,
            model_id: model.model_id(),
        })
    }

    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    pub fn transcribe(&mut self, samples: &[f32]) -> String {
        if samples.is_empty() {
            return String::new();
        }
        self.recognizer
            .transcribe(SAMPLE_RATE, samples)
            .text
            .trim()
            .to_string()
    }
}

/// The replacement guards, as a pure function over what the decoder said.
///
/// `Ok(text)` is "these words may overwrite the row's"; `Err` is the reason
/// they may not. Split out from [`Arbiters::arbitrate`] so the whole rule can be
/// exercised without a 160 MB model, and so there is exactly one copy of it —
/// the live pipeline, the tag-based correction and `recalld lang repair` all
/// come through here, which is what makes a repaired row indistinguishable from
/// one the pipeline got right the first time.
///
/// Three tests, in order of how cheap they are:
///
/// 1. **Caption-stripped.** Whisper's measured failure mode on non-speech is
///    inventing `(soft music)` (FINDINGS §9), and `(Musik)` classifies as
///    perfectly good German. Stripped before anything else looks at it.
/// 2. **At least `arbiter_min_words`.** One word is not evidence; empty is not
///    an answer.
/// 3. **Reads as the language we were trying to get to.** An arbiter that was
///    told to hear German and came back with English has said something useful
///    — that the flip was not a flip — and has not earned the right to
///    overwrite anything.
pub fn judge(raw: &str, want: Lang, cfg: &LangConfig) -> Result<String, Arbitration> {
    let text = strip_captions(raw);
    let words = normalise_words(&text).len();
    let read = lang::classify(&text);
    if words < cfg.arbiter_min_words.max(1) || read != want {
        return Err(Arbitration::Rejected {
            read_as: read.as_str(),
            words,
        });
    }
    Ok(text)
}

/// Remove Whisper's caption-style hallucinations before anything reads the text.
///
/// Measured, and scoped to exactly this model (FINDINGS §9): on silence, room
/// tone and music Whisper invents `(soft music)`, `[Applause]`, `♪♪` and the
/// like, while Parakeet emits nothing at all. Those are not words anyone said,
/// and — this is the part that matters — `(Musik)` classifies as perfectly
/// ordinary German, so an unfiltered arbiter would "confirm" a flip on a
/// fragment containing no speech.
///
/// Bracketed runs are dropped whole; a stray unclosed bracket takes the rest of
/// the line with it, which is the safe direction. What is left is trimmed of
/// the punctuation the removal stranded.
pub fn strip_captions(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut depth = 0usize;
    for ch in text.chars() {
        match ch {
            '(' | '[' | '{' | '<' => depth += 1,
            ')' | ']' | '}' | '>' => depth = depth.saturating_sub(1),
            // The music glyph Whisper wraps instrumental passages in. It is
            // never punctuation in speech, so it is dropped rather than counted.
            '♪' | '♫' => {}
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The arbiter this build would use for `want`, named for a log line. `None`
/// when the language is not one anything here can be forced to.
pub fn arbiter_for(want: Lang) -> Option<&'static str> {
    match want {
        Lang::En => Some(FALLBACK_ASR.dir),
        Lang::De => Some(ARBITER_DE.dir),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caption_hallucinations_are_stripped_before_anything_classifies_them() {
        // The measured failure mode: Whisper narrating non-speech. Left alone,
        // "(Musik)" is a German transcript as far as the classifier is
        // concerned, and would confirm a flip that never happened.
        assert_eq!(strip_captions("(soft music)"), "");
        assert_eq!(strip_captions("[Applause]"), "");
        assert_eq!(strip_captions("♪♪♪"), "");
        assert_eq!(strip_captions("(Musik)"), "");
        assert_eq!(lang::classify(&strip_captions("(Musik)")), Lang::Empty);
        // Real words survive, and so does the punctuation between them.
        assert_eq!(
            strip_captions("Ich glaube, das ist der einzige Weg."),
            "Ich glaube, das ist der einzige Weg."
        );
        // Mixed: the caption goes, the sentence stays, and the whitespace the
        // removal stranded is collapsed.
        assert_eq!(
            strip_captions("[Musik] Ich glaube das schon."),
            "Ich glaube das schon."
        );
        // An unclosed bracket swallows the rest, which is the safe direction:
        // a truncated hallucination is still a hallucination.
        assert_eq!(strip_captions("real words (music"), "real words");
        assert_eq!(strip_captions(""), "");
    }

    #[test]
    fn nested_brackets_do_not_leak_their_insides() {
        assert_eq!(strip_captions("([Applause] and (music))"), "");
        assert_eq!(strip_captions("a ([x] y) b"), "a b");
    }

    #[test]
    fn the_guards_reject_everything_that_is_not_better_than_what_it_replaces() {
        let cfg = LangConfig::default();
        let ok = |raw: &str| judge(raw, Lang::De, &cfg);

        // A caption is not speech, and "(Musik)" is not German — but it reads
        // as German to a classifier that has not had the brackets taken off it,
        // which is the whole reason the strip runs first.
        assert!(ok("(Musik)").is_err());
        assert!(ok("[Applaus]").is_err());
        assert!(ok("♪♪").is_err());
        // Nothing at all.
        assert!(ok("").is_err());
        assert!(ok("   ...  ").is_err());
        // One word is not evidence of anything.
        assert!(ok("ja").is_err());
        assert!(matches!(
            ok("ja"),
            Err(Arbitration::Rejected { words: 1, .. })
        ));
        // The arbiter was told to hear German and heard English. That is a
        // real answer — the flip was not a flip — and it overwrites nothing.
        assert!(matches!(
            ok("i think that is the only way"),
            Err(Arbitration::Rejected { read_as: "en", .. })
        ));
        // …and the same in the other direction.
        assert!(matches!(
            judge("ich glaube das ist der einzige weg", Lang::En, &cfg),
            Err(Arbitration::Rejected { read_as: "de", .. })
        ));

        // What passes: German words, enough of them, no captions.
        assert_eq!(
            ok("Ich glaube, das ist der einzige Weg.").unwrap(),
            "Ich glaube, das ist der einzige Weg."
        );
        // A caption alongside real words loses only the caption.
        assert_eq!(
            ok("[Musik] Ich glaube das schon.").unwrap(),
            "Ich glaube das schon."
        );
    }

    #[test]
    fn a_stricter_word_bar_is_honoured_and_zero_is_treated_as_one() {
        let strict = LangConfig {
            arbiter_min_words: 6,
            ..LangConfig::default()
        };
        assert!(judge("ich glaube das schon", Lang::De, &strict).is_err());
        // A configured zero would mean "an empty answer wins", which is not a
        // thing anyone can want; the floor is one word either way.
        let none = LangConfig {
            arbiter_min_words: 0,
            ..LangConfig::default()
        };
        assert!(judge("(Musik)", Lang::De, &none).is_err());
        assert!(judge("doch", Lang::De, &none).is_ok());
    }

    #[test]
    fn each_forced_language_gets_its_own_model_id() {
        // A row rewritten in German must never claim to have been written by
        // the same decoder as one rewritten in English, even from the same
        // weights: the forced language is part of the decoding contract.
        assert_eq!(ARBITER_DE.model_id(), "sherpa-onnx-whisper-base-de@1");
        assert_eq!(arbiter_for(Lang::De), Some(ARBITER_DE.dir));
        assert_eq!(arbiter_for(Lang::En), Some(FALLBACK_ASR.dir));
        assert_eq!(arbiter_for(Lang::Unclear), None);
        assert_eq!(arbiter_for(Lang::Empty), None);
    }
}
