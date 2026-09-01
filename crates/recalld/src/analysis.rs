//! The analysis leg: overlap gate, ASR, speaker identity.
//!
//! Order matters and is measured, not arbitrary:
//!
//! 1. **Overlap** first, because its answer decides whether the identity branch
//!    is allowed to run at all.
//! 2. **ASR on every turn regardless of overlap** — a transcript of the mix is
//!    still what the user wants to search, and Parakeet's insertion rate stays
//!    at or under 1.5% in the realistic regime.
//! 3. **Identity only on turns the gate approved**, so a blended embedding is
//!    never even computed, let alone stored.

use std::sync::atomic::Ordering;

use anyhow::Result;
use tracing::{debug, warn};

use crate::asr::{Asr, normalise_words};
use crate::config::{IdentityConfig, SAMPLE_RATE};
use crate::embed::{Embedder, Embedding};
use crate::identity::{self, Decision, Refusal};
use crate::models::ModelSet;
use crate::overlap::OverlapDetector;
use crate::store::{SegmentAnalysis, Store};

/// What inference learned about a turn, before anything is written down.
///
/// Splitting this out keeps the model work off the store mutex: the daemon
/// runs `prepare` unlocked and only takes the lock for `commit`, so a 30 s turn
/// cannot stall PipeWire's graph callbacks for the length of an ASR pass.
#[derive(Debug, Clone, PartialEq)]
pub struct Prepared {
    pub overlap_frac: f32,
    pub duration_s: f32,
    pub text: Option<String>,
    pub asr_model_id: String,
    /// The transcript's language, when the model is one that only speaks one.
    /// `None` from the multilingual export — see `Asr::lang`.
    pub lang: Option<&'static str>,
    /// `None` exactly when `refusal` is `Some`: audio the gate rejects is never
    /// embedded, so no blended vector can reach the voicebank.
    pub embedding: Option<Embedding>,
    pub refusal: Option<Refusal>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub overlap_frac: f32,
    pub text: Option<String>,
    pub decision: Decision,
    pub speaker_id: Option<i64>,
    pub match_score: Option<f32>,
    pub enrolled: bool,
}

pub struct Analyzer {
    overlap: OverlapDetector,
    asr: Asr,
    embedder: Embedder,
    cfg: IdentityConfig,
}

impl Analyzer {
    pub fn load(models: &ModelSet, cfg: &IdentityConfig) -> Result<Self> {
        let missing = models.missing();
        if !missing.is_empty() {
            let list = missing
                .iter()
                .map(|e| format!("{} ({})", e.role, e.path.display()))
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!("analysis models are incomplete: {list}");
        }
        Ok(Self {
            overlap: OverlapDetector::load(&models.segmentation)?,
            asr: Asr::load(models)?,
            embedder: Embedder::load(models)?,
            cfg: cfg.clone(),
        })
    }

    pub fn embed_model_id(&self) -> &str {
        self.embedder.model_id()
    }

    /// All the inference for one turn. Touches no database.
    pub fn prepare(&mut self, samples: &[f32]) -> Result<Prepared> {
        let duration_s = samples.len() as f32 / SAMPLE_RATE as f32;
        let overlap_frac = self.overlap.overlap_frac(samples)?;

        let raw = self.asr.transcribe(samples);
        // An empty transcript is stored as NULL rather than "": it keeps the
        // full-text index free of empty documents and makes "has a transcript"
        // a single IS NOT NULL.
        let text = (!normalise_words(&raw).is_empty()).then_some(raw);

        let refusal = identity::gate(&self.cfg, overlap_frac, duration_s);
        let embedding = match refusal {
            Some(_) => None,
            None => Some(self.embedder.embed(samples, SAMPLE_RATE)?),
        };
        Ok(Prepared {
            overlap_frac,
            duration_s,
            text,
            asr_model_id: self.asr.model_id().to_string(),
            lang: self.asr.lang(),
            embedding,
            refusal,
        })
    }

    /// Write what `prepare` learned onto the row and resolve identity against
    /// the voicebank. Cheap: no model runs here.
    pub fn commit(
        &self,
        store: &Store,
        segment_id: i64,
        prepared: Prepared,
        now_utc_ns: i64,
    ) -> Result<Outcome> {
        let Prepared {
            overlap_frac,
            duration_s,
            text,
            asr_model_id,
            lang,
            embedding,
            refusal,
        } = prepared;

        store.set_segment_analysis(
            segment_id,
            &SegmentAnalysis {
                // Only when the model itself constrains the answer; the
                // multilingual export leaves it NULL rather than guessing.
                lang: text.as_ref().and(lang).map(|l| l.to_string()),
                text: text.clone(),
                asr_model_id: Some(asr_model_id),
                overlap_frac: Some(overlap_frac),
            },
        )?;

        let embedding = match embedding {
            Some(e) => e,
            None => {
                let refusal = refusal.unwrap_or(Refusal::TooShort);
                debug!(
                    segment_id,
                    overlap_frac,
                    duration_s,
                    "no speaker: {}",
                    refusal.as_str()
                );
                store.set_segment_speaker(segment_id, None, None)?;
                return Ok(Outcome {
                    overlap_frac,
                    text,
                    decision: Decision::Refused(refusal),
                    speaker_id: None,
                    match_score: None,
                    enrolled: false,
                });
            }
        };

        store.store_embedding(segment_id, &embedding)?;
        let bank = store.prototypes(&embedding.model_id)?;
        let ranked = identity::rank(&embedding, &bank)?;
        let decision = identity::decide(&self.cfg, overlap_frac, duration_s, &ranked);

        let (speaker_id, match_score, enrolled) = match &decision {
            // `gate` already ran, so this arm is unreachable in practice; it
            // exists so a future gate change cannot silently label anyway.
            Decision::Refused(_) => (None, None, false),
            Decision::Matched {
                speaker_id,
                score,
                enroll,
            } => {
                if *enroll {
                    self.enroll(store, *speaker_id, &embedding, segment_id, now_utc_ns)?;
                }
                (Some(*speaker_id), Some(*score), *enroll)
            }
            Decision::Mint { best_score } => {
                let id = store.mint_speaker(now_utc_ns)?;
                // The turn seeds the new speaker. It already passed the overlap
                // and duration gates, and without a seed no voice could ever be
                // recognised a second time.
                self.enroll(store, id, &embedding, segment_id, now_utc_ns)?;
                (Some(id), *best_score, true)
            }
        };
        store.set_segment_speaker(segment_id, speaker_id, match_score)?;

        Ok(Outcome {
            overlap_frac,
            text,
            decision,
            speaker_id,
            match_score,
            enrolled,
        })
    }

    /// `prepare` then `commit`, for callers that hold the store exclusively.
    pub fn process(
        &mut self,
        store: &Store,
        segment_id: i64,
        samples: &[f32],
        now_utc_ns: i64,
    ) -> Result<Outcome> {
        let prepared = self.prepare(samples)?;
        self.commit(store, segment_id, prepared, now_utc_ns)
    }

    fn enroll(
        &self,
        store: &Store,
        speaker_id: i64,
        embedding: &Embedding,
        segment_id: i64,
        now_utc_ns: i64,
    ) -> Result<()> {
        store.add_prototype(
            speaker_id,
            embedding,
            Some(segment_id),
            false,
            self.cfg.max_prototypes,
            now_utc_ns,
        )?;
        Ok(())
    }
}

/// Counters the daemon logs on shutdown.
#[derive(Default)]
pub struct AnalysisStats {
    pub analysed: std::sync::atomic::AtomicU64,
    pub labelled: std::sync::atomic::AtomicU64,
    pub refused_overlap: std::sync::atomic::AtomicU64,
}

impl AnalysisStats {
    pub fn record(&self, outcome: &Outcome) {
        self.analysed.fetch_add(1, Ordering::Relaxed);
        match &outcome.decision {
            Decision::Refused(Refusal::Overlapped) => {
                self.refused_overlap.fetch_add(1, Ordering::Relaxed);
            }
            Decision::Matched { .. } | Decision::Mint { .. } => {
                self.labelled.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }
}

/// Inference outside the lock, then the write inside it, logging rather than
/// propagating a model failure: one bad segment must not stop capture.
pub fn analyse_or_log(
    analyzer: &mut Analyzer,
    store: &std::sync::Mutex<Store>,
    stats: &AnalysisStats,
    segment_id: i64,
    samples: &[f32],
    now_utc_ns: i64,
) {
    let prepared = match analyzer.prepare(samples) {
        Ok(p) => p,
        Err(e) => {
            warn!(segment_id, "analysis failed: {e:#}");
            return;
        }
    };
    let Ok(store) = store.lock() else {
        warn!(segment_id, "store mutex poisoned; analysis discarded");
        return;
    };
    match analyzer.commit(&store, segment_id, prepared, now_utc_ns) {
        Ok(outcome) => stats.record(&outcome),
        Err(e) => warn!(segment_id, "storing analysis failed: {e:#}"),
    }
}
