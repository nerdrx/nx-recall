//! The cross-check decoder: NeMo Canary 180m flash, int8.
//!
//! Not a better transcriber and never used as one. §11 of `spike/FINDINGS.md`
//! measured it against the shipped model on the user's own audio: language-
//! perfect, fast (RTF 0.33 on one thread), and it drops 11% of turns entirely.
//! Its value is that it is a *different* decoder, so where it agrees with
//! Parakeet the words are probably right and where it disagrees they are
//! probably not — which `spike/confidence_bench.py` then measured rather than
//! assumed: at τ = 0.5 the disagreeing turns carry **4.2×** the word error of
//! the agreeing ones (76.4% against 18.2%).
//!
//! Canary is a translation model wearing an ASR hat, so it demands a source
//! language. The daemon feeds it the segment's own `lang` when there is one and
//! the thread's context when there is not; with neither, it decodes twice and
//! keeps the *higher* agreement, which the same bench costs at 4.2× → 3.0×.
//!
//! Bound through the C API directly: `sherpa-rs` 0.6.8 has no Canary module at
//! all, though `sherpa-rs-sys` carries the struct its recognizer needs.

use anyhow::{Context, Result};

use crate::asr::normalise_words;
use crate::config::SAMPLE_RATE;
use crate::models::ConfidenceModel;

/// The languages the daemon will ask Canary about. The export speaks four; the
/// two the daemon's own language machinery knows are the two it can act on.
pub const LANGS: [&str; 2] = ["de", "en"];

pub struct Canary {
    recognizer: *const sherpa_rs::sherpa_rs_sys::SherpaOnnxOfflineRecognizer,
    lang: &'static str,
    model_id: String,
}

// Same contract as the transducer binding: used behind `&mut` from the one
// thread that owns it.
unsafe impl Send for Canary {}

impl Canary {
    /// Load the recognizer for one source language. One recognizer per
    /// language, because `src_lang` is a construction parameter rather than a
    /// per-call one.
    pub fn load(model: &ConfidenceModel, lang: &'static str) -> Result<Self> {
        use sherpa_rs::sherpa_rs_sys as sys;
        use std::ffi::CString;

        let cstr = |p: &std::path::Path| -> Result<CString> {
            let s = p
                .to_str()
                .with_context(|| format!("model path {} is not valid UTF-8", p.display()))?;
            Ok(CString::new(s)?)
        };
        let encoder = cstr(&model.encoder)?;
        let decoder = cstr(&model.decoder)?;
        let tokens = cstr(&model.tokens)?;
        let src = CString::new(lang)?;
        let tgt = CString::new(lang)?;
        let decoding = CString::new("greedy_search")?;
        let provider = CString::new("cpu")?;
        let empty = CString::new("")?;

        let recognizer = unsafe {
            let mut cfg: sys::SherpaOnnxOfflineRecognizerConfig = std::mem::zeroed();
            cfg.model_config.canary.encoder = encoder.as_ptr();
            cfg.model_config.canary.decoder = decoder.as_ptr();
            cfg.model_config.canary.src_lang = src.as_ptr();
            cfg.model_config.canary.tgt_lang = tgt.as_ptr();
            // Punctuation and capitalisation on: the comparison is against a
            // transcript that has them, and the agreement score normalises both
            // away anyway — but a decoder asked for less does not decode the
            // same words.
            cfg.model_config.canary.use_pnc = 1;
            cfg.model_config.tokens = tokens.as_ptr();
            cfg.model_config.provider = provider.as_ptr();
            cfg.model_config.model_type = empty.as_ptr();
            cfg.model_config.modeling_unit = empty.as_ptr();
            cfg.model_config.bpe_vocab = empty.as_ptr();
            cfg.model_config.num_threads = model.threads.max(1);
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
            anyhow::bail!("loading the confidence decoder ({lang}) failed");
        }
        Ok(Self {
            recognizer,
            lang,
            model_id: model.model_id(lang),
        })
    }

    pub fn lang(&self) -> &'static str {
        self.lang
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

impl Drop for Canary {
    fn drop(&mut self) {
        unsafe {
            sherpa_rs::sherpa_rs_sys::SherpaOnnxDestroyOfflineRecognizer(self.recognizer);
        }
    }
}

/// How much two transcripts of the same audio say the same thing: one minus the
/// word edit distance, normalised by the longer of the two word sequences and
/// floored at zero.
///
/// Case and punctuation are folded away first ([`normalise_words`]), because
/// the two decoders disagree about both by construction and neither
/// disagreement is a transcription error. Normalising by the *longer* sequence
/// is what stops a decoder that returned two of the twenty words from scoring
/// well for having got those two right.
pub fn agreement(a: &str, b: &str) -> f32 {
    let ref_words = normalise_words(a);
    let hyp_words = normalise_words(b);
    if ref_words.is_empty() && hyp_words.is_empty() {
        return 1.0;
    }
    let denom = ref_words.len().max(hyp_words.len());
    if denom == 0 {
        return 1.0;
    }
    let distance = edit_distance(&ref_words, &hyp_words);
    (1.0 - distance as f32 / denom as f32).max(0.0)
}

/// Plain word-level Levenshtein. Two rows rather than a matrix: a turn is a
/// handful of words, and this runs once per segment in a background worker.
fn edit_distance(a: &[String], b: &[String]) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, wa) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, wb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(wa != wb);
            cur[j + 1] = sub.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// What the cross-check concluded about one turn, in the words the wire uses.
///
/// Flag only, always: the contract says the text is never replaced by the
/// cross-check, and this type carries no text for exactly that reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    Solid,
    Shaky,
}

impl Confidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::Solid => "solid",
            Confidence::Shaky => "shaky",
        }
    }

    /// The bench's rule: at or above τ the two decoders agree.
    pub fn from_agreement(agreement: f32, tau: f32) -> Self {
        if agreement >= tau {
            Confidence::Solid
        } else {
            Confidence::Shaky
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_transcripts_agree_completely() {
        assert_eq!(
            agreement("hallo wie geht es dir", "hallo wie geht es dir"),
            1.0
        );
    }

    #[test]
    fn case_and_punctuation_are_not_disagreements() {
        assert_eq!(agreement("Hallo, wie geht's?", "hallo wie geht's"), 1.0);
    }

    #[test]
    fn one_wrong_word_in_five_costs_a_fifth() {
        let a = agreement("hallo wie geht es dir", "hallo wie steht es dir");
        assert!((a - 0.8).abs() < 1e-6, "{a}");
    }

    #[test]
    fn a_decoder_that_returned_nothing_agrees_with_nothing() {
        assert_eq!(agreement("hallo wie geht es dir", ""), 0.0);
        // Both empty is agreement, and it is the caller's job not to ask.
        assert_eq!(agreement("", ""), 1.0);
    }

    #[test]
    fn a_fragment_of_the_truth_does_not_score_as_the_truth() {
        // Two right words out of twenty is 10%, not 100%: the denominator is
        // the longer sequence.
        let long: String = std::iter::repeat_n("wort", 20)
            .collect::<Vec<_>>()
            .join(" ");
        assert!(agreement(&long, "wort wort") < 0.2);
    }

    #[test]
    fn the_threshold_is_inclusive() {
        assert_eq!(Confidence::from_agreement(0.5, 0.5), Confidence::Solid);
        assert_eq!(Confidence::from_agreement(0.499, 0.5), Confidence::Shaky);
        assert_eq!(Confidence::from_agreement(0.0, 0.5), Confidence::Shaky);
    }
}
