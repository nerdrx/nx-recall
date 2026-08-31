//! Speaker embeddings.
//!
//! The extractor is sherpa-onnx's, which owns the kaldi 80-bin fbank front end
//! and the model's own `global-mean` normalisation. Reimplementing that here
//! would be the single easiest way to produce vectors that look plausible and
//! compare wrongly.

use anyhow::{Context, Result, bail};
use sherpa_rs::speaker_id::{EmbeddingExtractor, ExtractorConfig};

use crate::models::ModelSet;

/// A vector plus the identity of the space it lives in.
///
/// The pairing is not decoration: cosine between vectors from different
/// extractors (or different feature contracts) is a meaningless number that
/// still looks like a score, so the two travel together and `cosine` refuses
/// to mix them.
#[derive(Debug, Clone, PartialEq)]
pub struct Embedding {
    pub model_id: String,
    pub vector: Vec<f32>,
}

impl Embedding {
    pub fn new(model_id: impl Into<String>, vector: Vec<f32>) -> Self {
        Self {
            model_id: model_id.into(),
            vector,
        }
    }

    pub fn dim(&self) -> usize {
        self.vector.len()
    }

    /// Cosine similarity, or an error if the two vectors are not comparable.
    pub fn cosine(&self, other: &Self) -> Result<f32> {
        if self.model_id != other.model_id {
            bail!(
                "refusing to compare embeddings across models: {} vs {}",
                self.model_id,
                other.model_id
            );
        }
        if self.vector.len() != other.vector.len() {
            bail!(
                "embedding dimension mismatch within model {}: {} vs {}",
                self.model_id,
                self.vector.len(),
                other.vector.len()
            );
        }
        let mut dot = 0.0f32;
        let mut na = 0.0f32;
        let mut nb = 0.0f32;
        for (a, b) in self.vector.iter().zip(&other.vector) {
            dot += a * b;
            na += a * a;
            nb += b * b;
        }
        let denom = na.sqrt() * nb.sqrt();
        if denom <= f32::EPSILON {
            return Ok(0.0);
        }
        Ok(dot / denom)
    }

    /// Little-endian f32s. Byte order is fixed rather than native so a database
    /// stays readable if the tree is ever moved to another machine.
    pub fn to_blob(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.vector.len() * 4);
        for v in &self.vector {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out
    }

    pub fn from_blob(model_id: impl Into<String>, blob: &[u8]) -> Result<Self> {
        if !blob.len().is_multiple_of(4) {
            bail!("embedding blob of {} bytes is not whole f32s", blob.len());
        }
        let vector = blob
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes(*c))
            .collect();
        Ok(Self {
            model_id: model_id.into(),
            vector,
        })
    }
}

pub struct Embedder {
    extractor: EmbeddingExtractor,
    model_id: String,
}

impl Embedder {
    pub fn load(models: &ModelSet) -> Result<Self> {
        let path = models
            .embedding
            .to_str()
            .context("embedding model path is not valid UTF-8")?
            .to_string();
        let extractor = EmbeddingExtractor::new(ExtractorConfig {
            model: path,
            num_threads: Some(1),
            ..Default::default()
        })
        .map_err(|e| anyhow::anyhow!("loading the speaker embedding model: {e}"))?;
        Ok(Self {
            extractor,
            model_id: models.embed_model_id(),
        })
    }

    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    pub fn embed(&mut self, samples: &[f32], sample_rate: u32) -> Result<Embedding> {
        let vector = self
            .extractor
            .compute_speaker_embedding(samples.to_vec(), sample_rate)
            .map_err(|e| anyhow::anyhow!("computing a speaker embedding: {e}"))?;
        Ok(Embedding::new(self.model_id.clone(), vector))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(id: &str, v: &[f32]) -> Embedding {
        Embedding::new(id, v.to_vec())
    }

    #[test]
    fn identical_vectors_score_one() {
        let a = e("m@1", &[1.0, 2.0, 3.0]);
        assert!((a.cosine(&a).unwrap() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn orthogonal_vectors_score_zero() {
        let a = e("m@1", &[1.0, 0.0]);
        let b = e("m@1", &[0.0, 1.0]);
        assert!(a.cosine(&b).unwrap().abs() < 1e-6);
    }

    #[test]
    fn magnitude_does_not_matter() {
        let a = e("m@1", &[1.0, 2.0]);
        let b = e("m@1", &[10.0, 20.0]);
        assert!((a.cosine(&b).unwrap() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn comparing_across_model_ids_is_an_error() {
        let a = e("eres2net_en@1", &[1.0, 0.0]);
        let b = e("titanet_small@1", &[1.0, 0.0]);
        let err = a.cosine(&b).unwrap_err().to_string();
        assert!(err.contains("across models"), "{err}");
    }

    #[test]
    fn a_contract_version_bump_makes_old_vectors_incomparable() {
        // Same file, new preprocessing: the id changes, so nothing silently
        // compares against the old bank.
        let old = e("eres2net_en@1", &[1.0, 0.0]);
        let new = e("eres2net_en@2", &[1.0, 0.0]);
        assert!(old.cosine(&new).is_err());
    }

    #[test]
    fn dimension_mismatch_within_a_model_is_an_error() {
        let a = e("m@1", &[1.0, 0.0]);
        let b = e("m@1", &[1.0, 0.0, 0.0]);
        assert!(a.cosine(&b).is_err());
    }

    #[test]
    fn a_zero_vector_scores_zero_rather_than_nan() {
        let a = e("m@1", &[0.0, 0.0]);
        let b = e("m@1", &[1.0, 1.0]);
        assert_eq!(a.cosine(&b).unwrap(), 0.0);
    }

    #[test]
    fn blobs_round_trip() {
        let a = e("m@1", &[1.5, -2.25, 0.0, 1e-7]);
        let back = Embedding::from_blob("m@1", &a.to_blob()).unwrap();
        assert_eq!(a, back);
    }

    #[test]
    fn a_truncated_blob_is_rejected() {
        assert!(Embedding::from_blob("m@1", &[0u8, 1, 2]).is_err());
    }
}
