//! Transcription with the NeMo Parakeet TDT transducer via sherpa-onnx.
//!
//! No hallucination filter, deliberately: Step 0 measured zero ghost words from
//! Parakeet on silence, white noise, room tone and music. The filter the design
//! brief calls mandatory is scoped to Whisper, which is not this model.

use anyhow::{Context, Result};
use sherpa_rs::transducer::{TransducerConfig, TransducerRecognizer};

use crate::config::SAMPLE_RATE;
use crate::models::ModelSet;

pub struct Asr {
    recognizer: TransducerRecognizer,
    model_id: String,
    lang: Option<&'static str>,
}

/// One decoded word and the second it starts at, relative to the start of the
/// buffer that was decoded.
///
/// The transducer emits BPE pieces, not words: a piece that opens a word
/// carries a start-of-word marker (sherpa renders it as a leading space, some
/// exports as U+2581) and the rest continue it. A word's time is its first
/// piece's timestamp, and there is no end time — a piece timestamp is the
/// frame the piece was emitted on, so "when did this word finish" is not a
/// thing the decoder knows. [`words_in_span`] is written to need only starts.
#[derive(Debug, Clone, PartialEq)]
pub struct Word {
    pub text: String,
    pub start_s: f32,
}

/// Group the decoder's pieces into words with a start time each.
pub fn words_from_tokens(tokens: &[String], stamps: &[f32]) -> Vec<Word> {
    let mut out: Vec<Word> = Vec::new();
    for (tok, &ts) in tokens.iter().zip(stamps.iter()) {
        let opens = tok.starts_with(' ') || tok.starts_with('\u{2581}');
        let piece = tok.trim_start_matches([' ', '\u{2581}']);
        if piece.is_empty() {
            continue;
        }
        match out.last_mut() {
            Some(word) if !opens => word.text.push_str(piece),
            _ => out.push(Word {
                text: piece.to_string(),
                start_s: ts,
            }),
        }
    }
    out.retain(|w| !w.text.trim().is_empty());
    out
}

/// The words of a window decode that belong to the turn inside it: the ones
/// that *start* between `from_s` and `to_s`.
///
/// Word-level rather than token-level on purpose, and measured that way
/// (`spike/context_redecode_bench.py`): a timestamp is the start of a piece, so
/// cutting on pieces leaves the last word of the span as a stump ("Kü", "sp").
/// A word belongs to the turn if it begins in the turn.
pub fn words_in_span(words: &[Word], from_s: f32, to_s: f32) -> String {
    words
        .iter()
        .filter(|w| w.start_s >= from_s && w.start_s < to_s)
        .map(|w| w.text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
}

impl Asr {
    pub fn load(models: &ModelSet) -> Result<Self> {
        let path = |p: &std::path::Path| -> Result<String> {
            Ok(p.to_str()
                .with_context(|| format!("model path {} is not valid UTF-8", p.display()))?
                .to_string())
        };
        let recognizer = TransducerRecognizer::new(TransducerConfig {
            encoder: path(&models.encoder)?,
            decoder: path(&models.decoder)?,
            joiner: path(&models.joiner)?,
            tokens: path(&models.tokens)?,
            // The NeMo transducer needs its own decoding contract; the generic
            // "transducer" type produces silent garbage against this export.
            model_type: "nemo_transducer".into(),
            decoding_method: "greedy_search".into(),
            sample_rate: SAMPLE_RATE as i32,
            feature_dim: 80,
            num_threads: models.asr_threads.max(1),
            ..Default::default()
        })
        .map_err(|e| anyhow::anyhow!("loading the ASR model: {e}"))?;
        Ok(Self {
            recognizer,
            model_id: models.asr_model_id(),
            lang: models.asr_lang(),
        })
    }

    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    /// What to stamp on this model's transcripts, `None` for a multilingual
    /// export: the transducer returns words, not a language, and guessing "en"
    /// over a German lobby writes a wrong answer into the database forever.
    pub fn lang(&self) -> Option<&'static str> {
        self.lang
    }

    /// Transcribe one segment. Returns an empty string for non-speech.
    pub fn transcribe(&mut self, samples: &[f32]) -> String {
        if samples.is_empty() {
            return String::new();
        }
        self.recognizer
            .transcribe(SAMPLE_RATE, samples)
            .trim()
            .to_string()
    }
}

/// The same transducer, decoded through the C API directly so the token
/// timestamps survive.
///
/// Why a second binding rather than one: `sherpa_rs::transducer` returns the
/// text and nothing else — it frees the result struct that carries
/// `timestamps` and `tokens` before the caller ever sees it — and the context
/// re-decode is entirely built on those timestamps. The live path keeps the
/// safe binding it has always used; this one exists for the idle worker, which
/// is the only caller, and it is deliberately configured with the same values
/// [`Asr::load`] passes so the two produce the same words.
///
/// Loaded by the worker on demand and dropped when there is no backlog: it is
/// a second copy of the encoder in memory, which is worth paying for while
/// there is work and not otherwise.
pub struct TimedAsr {
    recognizer: *const sherpa_rs::sherpa_rs_sys::SherpaOnnxOfflineRecognizer,
    model_id: String,
}

// The recognizer is used behind `&mut` from one thread at a time; sherpa's
// offline recognizer is documented as safe to move between threads, which is
// the same assumption `sherpa_rs::transducer::TransducerRecognizer` makes.
unsafe impl Send for TimedAsr {}

impl TimedAsr {
    pub fn load(models: &ModelSet) -> Result<Self> {
        use sherpa_rs::sherpa_rs_sys as sys;
        use std::ffi::CString;

        let cstr = |p: &std::path::Path| -> Result<CString> {
            let s = p
                .to_str()
                .with_context(|| format!("model path {} is not valid UTF-8", p.display()))?;
            Ok(CString::new(s)?)
        };
        let encoder = cstr(&models.encoder)?;
        let decoder = cstr(&models.decoder)?;
        let joiner = cstr(&models.joiner)?;
        let tokens = cstr(&models.tokens)?;
        let model_type = CString::new("nemo_transducer")?;
        let decoding = CString::new("greedy_search")?;
        let provider = CString::new("cpu")?;
        let empty = CString::new("")?;

        // Zeroed and then filled, rather than named field by field: the C
        // config carries a dozen model sub-configs this daemon never uses, and
        // a NULL is what "not this one" means for every one of them.
        let recognizer = unsafe {
            let mut cfg: sys::SherpaOnnxOfflineRecognizerConfig = std::mem::zeroed();
            cfg.model_config.transducer.encoder = encoder.as_ptr();
            cfg.model_config.transducer.decoder = decoder.as_ptr();
            cfg.model_config.transducer.joiner = joiner.as_ptr();
            cfg.model_config.tokens = tokens.as_ptr();
            cfg.model_config.model_type = model_type.as_ptr();
            cfg.model_config.provider = provider.as_ptr();
            cfg.model_config.modeling_unit = empty.as_ptr();
            cfg.model_config.bpe_vocab = empty.as_ptr();
            cfg.model_config.num_threads = models.asr_threads.max(1);
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
            anyhow::bail!("loading the ASR model for the re-decode worker failed");
        }
        Ok(Self {
            recognizer,
            model_id: models.asr_model_id(),
        })
    }

    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    /// Decode one buffer, keeping the words and where each of them starts.
    pub fn transcribe_timed(&mut self, samples: &[f32]) -> (String, Vec<Word>) {
        use sherpa_rs::sherpa_rs_sys as sys;

        if samples.is_empty() {
            return (String::new(), Vec::new());
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
            let count = raw.count.max(0) as usize;
            let mut tokens = Vec::with_capacity(count);
            let mut stamps = Vec::with_capacity(count);
            if !raw.tokens_arr.is_null() && !raw.timestamps.is_null() {
                for i in 0..count {
                    let tok = *raw.tokens_arr.add(i);
                    if tok.is_null() {
                        continue;
                    }
                    tokens.push(std::ffi::CStr::from_ptr(tok).to_string_lossy().into_owned());
                    stamps.push(*raw.timestamps.add(i));
                }
            }
            sys::SherpaOnnxDestroyOfflineRecognizerResult(result);
            sys::SherpaOnnxDestroyOfflineStream(stream);
            (text, words_from_tokens(&tokens, &stamps))
        }
    }
}

impl Drop for TimedAsr {
    fn drop(&mut self) {
        unsafe {
            sherpa_rs::sherpa_rs_sys::SherpaOnnxDestroyOfflineRecognizer(self.recognizer);
        }
    }
}

/// Case- and punctuation-insensitive word list, used for scoring and for the
/// "did it emit any words at all" check.
pub fn normalise_words(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric() || *c == '\'')
                .flat_map(|c| c.to_uppercase())
                .collect::<String>()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalisation_strips_case_and_punctuation() {
        assert_eq!(
            normalise_words(" But in his hands, solitude and a violin. "),
            vec![
                "BUT", "IN", "HIS", "HANDS", "SOLITUDE", "AND", "A", "VIOLIN"
            ]
        );
    }

    #[test]
    fn apostrophes_survive_because_they_are_part_of_the_word() {
        assert_eq!(normalise_words("I'm can't"), vec!["I'M", "CAN'T"]);
    }

    #[test]
    fn punctuation_only_input_yields_no_words() {
        assert!(normalise_words("  ... -- ,  ").is_empty());
        assert!(normalise_words("").is_empty());
    }
}
