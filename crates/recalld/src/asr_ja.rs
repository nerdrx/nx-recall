//! Japanese: a decoder that speaks it, and the rule for reaching for one
//! (0.11.0).
//!
//! ## The failure
//!
//! Parakeet-TDT-0.6b-v3 covers 25 European languages. Japanese is not one of
//! them, and the model does not say so — it transliterates. The user's own
//! evening, verbatim: "sumimasen, ogenki desu ka" came back as
//!
//! > Sima Sen Okenki Deska.
//!
//! Every correction this daemon had before 0.11.0 reads text.
//! [`crate::lang::classify`] sees Latin words with no German stopwords and
//! says `Unclear`. [`crate::lang::guess_other`]'s script rule needs kana and
//! never sees any. The per-speaker tags ([`crate::analysis::Analyzer::
//! correct_language`]) cannot route it either, because the user speaks
//! Japanese *themselves*, mid-evening, between German and English turns — a
//! tag is a fact about a voice and this is a fact about a turn. So the
//! decision comes from the audio ([`crate::lid`]) and the fix is a second
//! decoder.
//!
//! ## The decoder
//!
//! `sherpa-onnx-nemo-parakeet-tdt_ctc-0.6b-ja-35000-int8`: NVIDIA's Japanese
//! Parakeet, as the **CTC head** — one graph file and a token table, not the
//! encoder/decoder/joiner triple [`crate::asr::Asr`] loads — which is why it
//! comes in through the offline `nemo_ctc` config here rather than through
//! `Asr::load`. Loaded through `sherpa_rs_sys` directly for the same reason
//! [`crate::asr::TimedAsr`] is: `sherpa_rs` publishes no `nemo_ctc` wrapper.
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
//! 2. **Run it, then judge.** The replacement must be non-empty and must
//!    **read as Japanese** — which for this language is a script test and not
//!    a vote, so it is exact: [`crate::lang::guess_other`] returns `ja` on two
//!    kana. A Japanese decoder that came back with no kana in it has not
//!    decoded Japanese, and has not earned the right to overwrite anything.
//!    The arbiter's `arbiter_min_words` floor is the one guard that does *not*
//!    carry over, because a language without spaces has no word count — see
//!    [`judge`], which replaces it with something stricter.
//!
//! Guard 2 is the reason a false positive from the identifier is cheap. If LID
//! is wrong and a German turn reaches this decoder, the Japanese-only model
//! produces kana over German audio or produces nothing, and either way the
//! judge throws it away and the original transcript stands.

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use crate::asr::normalise_words;
use crate::config::{AsrConfig, LangConfig, SAMPLE_RATE};
use crate::lang;
use crate::lid::Reading;
use crate::models::{JapaneseModel, ModelSet};
use crate::store::Store;

/// The BCP-47 tag this whole module is about.
pub const JA: &str = "ja";

/// `segments.lang_via` for a row whose language was decided by listening to
/// the audio rather than by reading the words (0.11.0).
///
/// A sixth value alongside the five in [`crate::store::lang_via`], and it has
/// to be its own: `re-decode` means "the text disagreed with a declaration",
/// `classified` means "the words said so", and neither is true here. The words
/// said nothing — they *could* not, they were Latin nonsense — and what
/// settled it was a model that listened.
pub const LANG_VIA_LID: &str = "lid";

// ---------------------------------------------------------------------------
// the routing decision, as pure functions
// ---------------------------------------------------------------------------

/// What to do about a turn *before* any model has run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pre {
    /// The speaker is declared Japanese and nothing else. Decode with the
    /// Japanese model straight away; the identifier is not consulted, because
    /// a declaration outranks a reading of one turn's audio.
    Direct,
    /// The transcript is unreadable in the specific way a transliterated
    /// Japanese turn is unreadable. Worth the cost of asking the identifier.
    AskLid,
    /// Nothing to do, and — this is the point of separating the two steps —
    /// **no LID call**. A clear German transcript is a clear German
    /// transcript; paying 20 ms to be told so on every turn of every evening
    /// is the cost this split exists to avoid.
    Nothing,
}

/// Step one: is this turn worth asking about, and does it even need asking?
///
/// `declared` is the speaker's language tags, `text` the transcript the live
/// decoder produced.
///
/// The rule, in order:
///
/// 1. A speaker pinned to **exactly** Japanese goes straight to the decoder. A
///    bilingual voice does not: a Japanese speaker's English turn is not a
///    mistake, which is the same reasoning
///    [`crate::lang::sole_language`] already encodes.
/// 2. A speaker pinned to exactly *something else* is that other feature's
///    business ([`crate::analysis::Analyzer::correct_language`]) and is left
///    alone. Deciding a row twice is how two features start fighting over one
///    column.
/// 3. A transcript that already reads as **something** — German, English, or
///    any script or stopword majority [`crate::lang::guess_other`] is
///    confident about, Japanese included — is not the transliteration failure.
///    Nothing to do.
/// 4. What is left is `Unclear` or `Empty`: words nobody can read, or no words
///    at all. Both are what a Japanese turn looks like coming out of a decoder
///    that cannot spell it. Ask.
pub fn pre_route(declared: Option<&Vec<String>>, text: Option<&str>) -> Pre {
    if let Some(sole) = lang::sole_language(declared) {
        return if sole == JA {
            Pre::Direct
        } else {
            Pre::Nothing
        };
    }
    let text = text.unwrap_or("");
    // No words at all is a fair question for the identifier — the Japanese
    // decode that produced nothing is exactly the turn worth re-reading — but
    // an empty *buffer* is not, and the caller's duration floor catches that.
    if !text.trim().is_empty() {
        match lang::classify(text) {
            lang::Lang::De | lang::Lang::En => return Pre::Nothing,
            // A confident third-language guess is a real reading of the words.
            // That includes `ja` itself: kana in the transcript means some
            // decoder already got it right and there is nothing to fix.
            _ if lang::guess_other(text).is_some_and(|g| g.confident) => return Pre::Nothing,
            _ => {}
        }
    }
    Pre::AskLid
}

/// Step two: the identifier has answered. Is that answer good enough to spend
/// a decode on?
///
/// `lid_min_confidence` is the operating point. With the shipped
/// `[asr].lid_windows = 1` the confidence is always 1.0 and this is a "did it
/// say ja at all" test — which is what the measurement supports, because
/// nothing in 400 German and English utterances was heard as Japanese at any
/// length (`crate::lid`). The knob is what a machine that hears something that
/// corpus did not can turn.
pub fn post_route(reading: Option<&Reading>, cfg: &AsrConfig) -> bool {
    reading.is_some_and(|r| r.is(JA) && r.confidence >= cfg.lid_min_confidence)
}

/// What a Japanese re-decode did, or why it did not.
#[derive(Debug, Clone, PartialEq)]
pub enum Rerouted {
    /// The Japanese decoder's words replaced the row's.
    Replaced { text: String, model_id: String },
    /// Under `[lang].arbiter_min_duration_s`. The model was not run: at this
    /// length its answer is not better than the one it would overwrite.
    TooShort,
    /// No Japanese decoder is installed. Said once per daemon, not per turn.
    Unavailable,
    /// It ran and the answer failed a guard — empty, too few words, or no kana
    /// in it, which means it did not decode Japanese whatever it decoded.
    Rejected { words: usize, kana: bool },
}

/// The replacement guards, as a pure function over what the decoder said.
///
/// Split out from the model call for exactly the reason
/// [`crate::arbiter::judge`] is: the whole rule can then be exercised without
/// a 655 MB model, and there is one copy of it.
///
/// The kana test is the load-bearing one and it is *cheap and exact*, which is
/// unusual — Japanese is the one language in this daemon whose identity is
/// settled by its writing system rather than by a stopword vote. Two kana
/// characters, the same bar [`crate::lang::guess_other`] uses, so one borrowed
/// word is not a language.
///
/// Deliberately **without** the arbiter's `arbiter_min_words` bar, and that is
/// not an oversight: [`crate::asr::normalise_words`] splits on whitespace and
/// Japanese has none, so a whole Japanese sentence is one "word" and a
/// two-word floor would reject every correct answer this decoder can give. The
/// bar it replaces the word count with is stricter, not looser — the text must
/// contain kana, which no wrong-language decode of German or English audio
/// can accidentally satisfy.
pub fn judge(raw: &str) -> Result<String, Rerouted> {
    // The same caption strip the German arbiter runs. Cheap, and this decoder
    // has not been measured for a hallucination habit — running the filter
    // that catches one costs a string scan and assuming its absence costs a
    // fabricated transcript.
    let text = crate::arbiter::strip_captions(raw);
    let kana = lang::guess_other(&text).is_some_and(|g| g.tag == JA);
    // Reported, not tested against a floor — see the note above. It is in the
    // rejection so a log line can say what came back instead of Japanese.
    let words = normalise_words(&text).len();
    if !kana || text.trim().is_empty() {
        return Err(Rerouted::Rejected { words, kana });
    }
    Ok(text)
}

// ---------------------------------------------------------------------------
// the decoder
// ---------------------------------------------------------------------------

/// The Japanese CTC decoder, loaded on demand and resident from then on.
pub struct JaAsr {
    recognizer: *const sherpa_rs::sherpa_rs_sys::SherpaOnnxOfflineRecognizer,
    model_id: String,
}

// The recognizer is used behind `&mut` from one thread at a time — the same
// assumption, and the same justification, as `crate::asr::TimedAsr`.
unsafe impl Send for JaAsr {}

impl JaAsr {
    pub fn load(model: &JapaneseModel, threads: i32) -> Result<Self> {
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
        let model_type = CString::new("nemo_ctc")?;
        let decoding = CString::new("greedy_search")?;
        let provider = CString::new("cpu")?;
        let empty = CString::new("")?;

        // Zeroed then filled, exactly as `TimedAsr` does and for the same
        // reason: the C config carries a dozen model sub-configs this daemon
        // never uses, and NULL is what "not this one" means for each of them.
        let recognizer = unsafe {
            let mut cfg: sys::SherpaOnnxOfflineRecognizerConfig = std::mem::zeroed();
            cfg.model_config.nemo_ctc.model = graph.as_ptr();
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
            anyhow::bail!("loading the Japanese decoder failed");
        }
        Ok(Self {
            recognizer,
            model_id: model.model_id(),
        })
    }

    pub fn model_id(&self) -> &str {
        &self.model_id
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

impl Drop for JaAsr {
    fn drop(&mut self) {
        unsafe {
            sherpa_rs::sherpa_rs_sys::SherpaOnnxDestroyOfflineRecognizer(self.recognizer);
        }
    }
}

// ---------------------------------------------------------------------------
// the router
// ---------------------------------------------------------------------------

/// Both optional models, each loaded the first time it is needed and resident
/// from then on.
///
/// `*_unavailable` is not the same as `*.is_none()`, for the reason
/// [`crate::arbiter::Arbiters`] spells out: "not tried yet" and "tried, not
/// installed" differ, and the difference is what keeps a missing optional
/// model to one warning line rather than one per turn.
pub struct Japanese {
    models: ModelSet,
    enabled: bool,
    asr: Option<JaAsr>,
    asr_unavailable: bool,
    lid: Option<crate::lid::Lid>,
    lid_unavailable: bool,
    windows: usize,
}

impl Japanese {
    pub fn new(models: &ModelSet, cfg: &AsrConfig) -> Self {
        Self {
            models: models.clone(),
            enabled: cfg.japanese,
            asr: None,
            asr_unavailable: false,
            lid: None,
            lid_unavailable: false,
            windows: cfg.lid_windows.max(1),
        }
    }

    /// Is the pair on disk and switched on? Read at start-up for the log line,
    /// and before every turn as the cheapest possible early exit.
    pub fn ready(&self) -> bool {
        self.enabled && self.models.japanese().present() && self.models.lid().present()
    }

    /// The line the daemon logs once at start-up, `None` when there is nothing
    /// worth saying.
    pub fn startup_note(&self) -> Option<String> {
        if !self.enabled {
            return Some(
                "[asr].japanese is off: a Japanese turn will be transcribed by the \
                 multilingual decoder, which transliterates it into Latin letters."
                    .to_string(),
            );
        }
        if !self.models.japanese().present() || !self.models.lid().present() {
            return Some(JapaneseModel::how_to_get_it());
        }
        None
    }

    fn decoder(&mut self) -> Option<&mut JaAsr> {
        if self.asr.is_none() && !self.asr_unavailable {
            let model = self.models.japanese();
            if !model.present() {
                self.asr_unavailable = true;
                warn!("{}", JapaneseModel::how_to_get_it());
            } else {
                match JaAsr::load(&model, self.models.asr_threads) {
                    Ok(asr) => {
                        info!(
                            model = asr.model_id(),
                            "loaded the Japanese decoder ({})", model.export.note
                        );
                        self.asr = Some(asr);
                    }
                    Err(e) => {
                        self.asr_unavailable = true;
                        warn!("could not load the Japanese decoder: {e:#}");
                    }
                }
            }
        }
        self.asr.as_mut()
    }

    fn identifier(&mut self) -> Option<&mut crate::lid::Lid> {
        if self.lid.is_none() && !self.lid_unavailable {
            let model = self.models.lid();
            if !model.present() {
                self.lid_unavailable = true;
                warn!("{}", crate::lid::how_to_get_it());
            } else {
                match crate::lid::Lid::load(&model, self.models.asr_threads, self.windows) {
                    Ok(lid) => {
                        info!(
                            model = lid.model_id(),
                            "loaded the spoken-language identifier ({})", model.export.note
                        );
                        self.lid = Some(lid);
                    }
                    Err(e) => {
                        self.lid_unavailable = true;
                        warn!("could not load the language identifier: {e:#}");
                    }
                }
            }
        }
        self.lid.as_mut()
    }

    /// Decode `samples` with the Japanese model and apply every guard. Decides
    /// nothing about the database and touches none of it.
    pub fn redecode(&mut self, samples: &[f32], cfg: &LangConfig) -> Rerouted {
        let duration_s = samples.len() as f32 / SAMPLE_RATE as f32;
        if duration_s < cfg.arbiter_min_duration_s {
            return Rerouted::TooShort;
        }
        let (raw, model_id) = match self.decoder() {
            Some(asr) => {
                let id = asr.model_id().to_string();
                (asr.transcribe(samples), id)
            }
            None => return Rerouted::Unavailable,
        };
        match judge(&raw) {
            Ok(text) => Rerouted::Replaced { text, model_id },
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
    /// `lid_unavailable` and then reported *every* subsequent turn as checked
    /// while no model ever ran. `lid_checked` is documented as what this
    /// feature costs ("a rising count with `routed_ja` at zero means the
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
    pub asr_model_id: String,
    /// True when the identifier was actually run — which it is not on the
    /// declared-speaker path, and not on a clear transcript.
    pub lid_checked: bool,
}

/// The whole Japanese route for one turn: decide, maybe listen, maybe decode,
/// and write.
///
/// Returns `Ok(None)` for the overwhelmingly common case of a turn that is not
/// Japanese and was never going to be. `lid_checked` is reported through
/// [`Checked`] even when nothing was replaced, because "we asked and it said
/// German" is the number that says what this feature costs.
///
/// Writes two things and in this order:
///
/// 1. the words, through [`Store::set_segment_text_via`], which is the call the
///    context pass and the night shift already use — so the prior text lands in
///    `operations` as a `segments.redecode` row with the model and route that
///    produced it, and a person can compare or revert a Japanese re-decode
///    exactly as they can any other machine edit;
/// 2. the language, `ja` via [`LANG_VIA_LID`].
///
/// A write failure is the caller's to log; nothing here is allowed to cost the
/// segment that was just analysed.
pub fn route_segment(
    japanese: &mut Japanese,
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
    if !japanese.ready() {
        return Ok(Checked::default());
    }
    let pre = pre_route(declared, text);
    if pre == Pre::Nothing {
        return Ok(Checked::default());
    }
    let mut checked = Checked::default();
    if pre == Pre::AskLid {
        // The duration floor before the identifier, not only before the
        // decoder: LID on a 0.4 s fragment costs a model pass to produce a
        // reading nothing is allowed to act on.
        if (samples.len() as f32 / SAMPLE_RATE as f32) < lang_cfg.arbiter_min_duration_s {
            return Ok(checked);
        }
        // Counted only when a model actually ran. See `Japanese::identify`:
        // an identifier that never loaded costs nothing, and counting it is
        // how a dead feature came to read as an expensive one.
        let Some(reading) = japanese.identify(samples) else {
            return Ok(checked);
        };
        checked.lid_checked = true;
        if !post_route(reading.as_ref(), asr_cfg) {
            debug!(
                segment_id,
                heard = reading
                    .as_ref()
                    .map(|r| r.lang.as_str())
                    .unwrap_or("nothing"),
                "not Japanese"
            );
            return Ok(checked);
        }
    }

    match japanese.redecode(samples, lang_cfg) {
        Rerouted::Replaced {
            text: new,
            model_id,
        } => {
            store.set_segment_text_via(
                segment_id,
                &new,
                &model_id,
                crate::store::text_via::ARBITER,
                now_utc_ns,
            )?;
            store.set_segment_language(segment_id, JA, LANG_VIA_LID)?;
            info!(
                segment_id,
                model = %model_id,
                via = if pre == Pre::Direct { "declared" } else { "lid" },
                "re-decoded a Japanese turn the multilingual model had transliterated"
            );
            checked.routed = Some(Routed {
                text: new,
                asr_model_id: model_id,
                lid_checked: checked.lid_checked,
            });
        }
        other => debug!(segment_id, outcome = ?other, "the Japanese re-decode did not stand"),
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
}

/// Was this row's language settled by the Japanese router?
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

    /// A router pointed at a directory with nothing in it: switched on, and
    /// with no identifier behind the switch.
    fn a_router_with_no_models() -> Japanese {
        let models = ModelSet::resolve_at(
            std::path::PathBuf::from("/nonexistent/nx-recall-audit"),
            &crate::config::ModelsConfig::default(),
        );
        Japanese::new(
            &models,
            &crate::config::AsrConfig {
                japanese: true,
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
        // (`post_route` reads the inner one, and is unchanged by any of this.)
        assert!(!post_route(None, &crate::config::AsrConfig::default()));

        // A turn nothing was spent on carries no count.
        assert!(!Checked::default().lid_checked);
    }

    #[test]
    fn a_router_that_is_not_ready_does_not_route_and_does_not_count() {
        // `ready()` is the cheap early exit, and it is false in every state
        // where `lid_checked` would otherwise be a lie: switched off, or the
        // pair not installed.
        let mut off = a_router_with_no_models();
        assert!(!off.ready(), "no models means not ready");
        assert!(!off.lid_ready());

        let models = ModelSet::resolve_at(
            std::path::PathBuf::from("/nonexistent/nx-recall-audit"),
            &crate::config::ModelsConfig::default(),
        );
        let switched_off = Japanese::new(
            &models,
            &crate::config::AsrConfig {
                japanese: false,
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
    fn a_speaker_declared_japanese_goes_straight_to_the_decoder() {
        // No LID call: a declaration outranks a reading of one turn's audio,
        // and asking would cost a model pass to be told what we were told.
        assert_eq!(
            pre_route(Some(&tags(&["ja"])), Some("Sima Sen Okenki Deska.")),
            Pre::Direct
        );
        // Even when the transliteration happens to read as English: the tag is
        // the whole point of the direct path.
        assert_eq!(
            pre_route(Some(&tags(&["ja"])), Some("i think that is so")),
            Pre::Direct
        );
    }

    #[test]
    fn a_bilingual_speaker_is_not_pinned_to_anything() {
        // A Japanese speaker's English turn is not a mistake, so two tags mean
        // the same as none: fall through to the audio.
        assert_eq!(
            pre_route(Some(&tags(&["en", "ja"])), Some("mumble")),
            Pre::AskLid
        );
    }

    #[test]
    fn a_speaker_declared_something_else_is_the_other_features_business() {
        // `correct_language` owns this row. Deciding it twice is how two
        // features start fighting over one column.
        assert_eq!(
            pre_route(Some(&tags(&["de"])), Some("Sima Sen Okenki Deska.")),
            Pre::Nothing
        );
        assert_eq!(
            pre_route(Some(&tags(&["en"])), Some("mumble")),
            Pre::Nothing
        );
    }

    #[test]
    fn the_transliteration_failure_is_what_reaches_the_identifier() {
        // THE case. Latin letters, English-shaped, no German stopwords, no
        // kana for the script rule — `Unclear`, and invisible to every text
        // mechanism this daemon had before 0.11.0.
        assert_eq!(
            lang::classify("Sima Sen Okenki Deska."),
            lang::Lang::Unclear
        );
        assert_eq!(lang::guess_other("Sima Sen Okenki Deska."), None);
        assert_eq!(pre_route(None, Some("Sima Sen Okenki Deska.")), Pre::AskLid);
        // No words at all is the same failure with the volume turned down.
        assert_eq!(pre_route(None, None), Pre::AskLid);
        assert_eq!(pre_route(None, Some("   ")), Pre::AskLid);
    }

    #[test]
    fn clear_german_text_never_costs_a_lid_call() {
        // The cost this whole two-step split exists to avoid: 20 ms per turn
        // on an evening of German that was never going to be Japanese.
        assert_eq!(
            pre_route(None, Some("ich glaube das ist der einzige weg")),
            Pre::Nothing
        );
        assert_eq!(
            pre_route(None, Some("i think that is the only way")),
            Pre::Nothing
        );
    }

    #[test]
    fn a_transcript_that_already_reads_as_something_is_left_alone() {
        // Kana in the transcript means some decoder already got it right.
        assert_eq!(
            pre_route(None, Some("すみません、お元気ですか")),
            Pre::Nothing
        );
        // And a confident third-language guess is a real reading of the words.
        assert_eq!(
            pre_route(None, Some("le la les des une est ne pas que qui pour dans")),
            Pre::Nothing
        );
    }

    #[test]
    fn the_identifier_has_to_say_japanese_and_mean_it() {
        let cfg = AsrConfig::default();
        assert!(post_route(Some(&reading("ja", 1.0)), &cfg));
        // German. The overwhelmingly common answer, and it stops here.
        assert!(!post_route(Some(&reading("de", 1.0)), &cfg));
        assert!(!post_route(Some(&reading("en", 1.0)), &cfg));
        // No opinion is not an opinion.
        assert!(!post_route(None, &cfg));
        // Under the operating point, once a machine has raised it.
        let strict = AsrConfig {
            lid_min_confidence: 0.9,
            ..AsrConfig::default()
        };
        assert!(!post_route(Some(&reading("ja", 0.66)), &strict));
        assert!(post_route(Some(&reading("ja", 1.0)), &strict));
    }

    #[test]
    fn the_guards_reject_a_re_decode_that_is_not_japanese() {
        // The load-bearing guard, and why a false positive from the identifier
        // is cheap: whatever a Japanese-only decoder makes of German audio, it
        // is not kana, so it overwrites nothing.
        assert!(matches!(
            judge("ich glaube das schon"),
            Err(Rerouted::Rejected { kana: false, .. })
        ));
        assert!(judge("").is_err());
        assert!(judge("   ...  ").is_err());
        // Kanji with no kana is not proof of Japanese — it is the script
        // Chinese writes in too, which is exactly why `guess_other` checks
        // kana by presence rather than Han by dominance.
        assert!(judge("技術決定論").is_err());
        // A caption is stripped before anything judges it, the same as the
        // German arbiter — and what is left is nothing.
        assert!(judge("(music)").is_err());

        // What passes.
        assert_eq!(
            judge("すみません、お元気ですか").unwrap(),
            "すみません、お元気ですか"
        );
        assert_eq!(judge("[音楽] 元気ですか").unwrap(), "元気ですか");
    }

    #[test]
    fn a_japanese_row_is_stamped_by_the_thing_that_actually_decided_it() {
        // Not `re-decode` (nothing disagreed with a declaration) and not
        // `classified` (the words said nothing). A model listened.
        assert_eq!(LANG_VIA_LID, "lid");
        assert_ne!(LANG_VIA_LID, crate::store::lang_via::REDECODE);
        assert_ne!(LANG_VIA_LID, crate::store::lang_via::CLASSIFIED);
    }

    /// Both models, on real audio, through the real FFI.
    ///
    /// `#[ignore]` because it needs a 768 MB models root and a WAV, neither of
    /// which lives in this repository. It is here rather than only in the
    /// Python spike because the part of this module most likely to be silently
    /// wrong is the hand-filled `nemo_ctc` config in [`JaAsr::load`] — a
    /// mistake there does not fail to compile and does not fail to load, it
    /// produces empty strings or garbage, exactly as the multilingual export
    /// does under the generic `transducer` type (see `crate::asr`).
    ///
    /// ```text
    /// NXR_MODELS=<dir> NXR_JA_WAV=<16k mono wav> \
    ///   cargo test -p recalld --lib -- --ignored the_models_actually
    /// ```
    #[test]
    #[ignore = "needs a populated models root and a Japanese WAV"]
    fn the_models_actually_load_and_hear_japanese() {
        let Ok(root) = std::env::var("NXR_MODELS") else {
            panic!("set NXR_MODELS");
        };
        let wav = std::env::var("NXR_JA_WAV").expect("set NXR_JA_WAV");
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
            models.japanese().present(),
            "the Japanese decoder is not installed at {root}"
        );
        assert!(
            models.lid().present(),
            "the identifier is not installed at {root}"
        );

        // The identifier hears Japanese.
        let mut lid = crate::lid::Lid::load(&models.lid(), 4, 1).expect("loading the identifier");
        let reading = lid.identify(&samples).expect("a reading");
        assert_eq!(reading.lang, JA, "heard {} instead", reading.lang);

        // The decoder produces Japanese, and the guard accepts it. This is the
        // assertion the FFI config has to earn: a wrong `model_type` or a
        // missing NULL comes back empty or as Latin mojibake, and `judge`
        // rejects both.
        let mut asr = JaAsr::load(&models.japanese(), 4).expect("loading the decoder");
        let text = asr.transcribe(&samples);
        assert!(!text.is_empty(), "the decoder returned nothing");
        let kept = judge(&text).unwrap_or_else(|e| panic!("judged {text:?} as {e:?}"));
        assert!(lang::guess_other(&kept).is_some_and(|g| g.tag == JA));
        println!("decoded: {kept}");
    }

    #[test]
    fn the_catalogue_entry_names_the_files_the_decoder_opens() {
        use crate::models::{JAPANESE_ASR, JAPANESE_DIR, LID_WHISPER, expected_bytes};
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
            expected_bytes(&format!("{}/{}", LID_WHISPER.dir, LID_WHISPER.encoder)),
            Some(12_937_772)
        );
        assert_eq!(
            expected_bytes(&format!("{}/{}", LID_WHISPER.dir, LID_WHISPER.decoder)),
            Some(89_855_401)
        );
        // The two assets are one group, so one flag installs the pair.
        assert_eq!(
            crate::models::japanese_download_bytes(),
            489_389_564 + 116_204_861
        );
        // The forced language is part of the decoding contract and therefore
        // part of the id, the same rule the arbiter follows.
        assert_eq!(
            JAPANESE_ASR.model_id(),
            "sherpa-onnx-nemo-parakeet-tdt_ctc-0.6b-ja-35000-int8@1"
        );
    }
}
