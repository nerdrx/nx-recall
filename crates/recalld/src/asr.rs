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
