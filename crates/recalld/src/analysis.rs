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
//!
//! The microphone leg (`commit_mic`) short-circuits step 3's *question* without
//! skipping its *guard*: the speaker is known from where the audio came, so the
//! voicebank is never consulted, but the overlap detector still runs and still
//! decides whether anything may be enrolled.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

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
    /// A golden sample was written for this turn (mic enrolment only).
    pub golden: bool,
}

/// Everything the microphone leg needs that the matching leg does not: who the
/// audio belongs to by construction, and where the kept clips go.
#[derive(Debug, Clone, Copy)]
pub struct MicEnroll<'a> {
    /// The pinned "You" speaker (`Store::ensure_you_speaker`).
    pub speaker_id: i64,
    pub data_dir: &'a Path,
    pub max_goldens: usize,
}

/// `goldens/<speaker>/golden-<segment>.wav`, relative to the data dir.
///
/// Deliberately **not** under `segments/`: the retention sweeper walks that
/// tree for orphans and unlinks what the `segments` table has aged out, and a
/// golden must outlive both (DESIGN §5/§6 — a golden is what a future embedding
/// model gets re-enrolled from). Living in its own directory is what makes it
/// retention-exempt, with no special case in the sweeper at all.
pub fn golden_path(speaker_id: i64, segment_id: i64) -> PathBuf {
    PathBuf::from("goldens")
        .join(format!("{speaker_id:06}"))
        .join(format!("golden-{segment_id:06}.wav"))
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
                    golden: false,
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
            // `decide` never returns this: pinning is what `commit_mic` does
            // instead of asking. The arm exists so the match stays total.
            Decision::Pinned { speaker_id } => (Some(*speaker_id), None, false),
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
            golden: false,
        })
    }

    /// The microphone leg: write what `prepare` learned, then label the turn
    /// with the pinned "You" speaker **without consulting the voicebank**.
    ///
    /// The design note this implements (DESIGN §5) is that a mic tap is a free
    /// perfect label, and the corollary is that it must not be laundered into
    /// looking like a match: `match_score` stays NULL, because there was no
    /// comparison to score. `overlap_frac` is still stored — when the user runs
    /// loudspeakers the mic hears the room talking back, and the correction UI
    /// has to be able to see that even though the name is certain.
    ///
    /// Enrolment keeps every gate the matching leg has, minus the two that are
    /// about *identifying* (threshold and margin): overlap ≤ `enroll_max_overlap`
    /// and duration ≥ `enroll_min_duration_s`. That is the payoff — prototypes
    /// for the one voice the daemon can be certain about, plus up to
    /// `max_goldens` kept clips for a future model migration.
    pub fn commit_mic(
        &self,
        store: &Store,
        segment_id: i64,
        prepared: Prepared,
        mic: &MicEnroll<'_>,
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
                lang: text.as_ref().and(lang).map(|l| l.to_string()),
                text: text.clone(),
                asr_model_id: Some(asr_model_id),
                overlap_frac: Some(overlap_frac),
            },
        )?;
        // Provenance, before anything else can fail: the label does not depend
        // on the embedder having produced a vector.
        store.set_segment_speaker(segment_id, Some(mic.speaker_id), None)?;

        let mut enrolled = false;
        let mut golden = false;
        if let Some(embedding) = embedding {
            store.store_embedding(segment_id, &embedding)?;
            if overlap_frac <= self.cfg.enroll_max_overlap
                && duration_s >= self.cfg.enroll_min_duration_s
            {
                store.add_prototype(
                    mic.speaker_id,
                    &embedding,
                    Some(segment_id),
                    false,
                    self.cfg.max_prototypes,
                    now_utc_ns,
                )?;
                enrolled = true;
                golden = keep_golden(store, mic, segment_id, duration_s)?;
            }
        } else {
            debug!(
                segment_id,
                overlap_frac,
                duration_s,
                "microphone: labelled but not enrolled ({})",
                refusal.unwrap_or(Refusal::TooShort).as_str()
            );
        }

        Ok(Outcome {
            overlap_frac,
            text,
            decision: Decision::Pinned {
                speaker_id: mic.speaker_id,
            },
            speaker_id: Some(mic.speaker_id),
            match_score: None,
            enrolled,
            golden,
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

    /// `prepare` then `commit_mic`, for callers that hold the store exclusively.
    pub fn process_mic(
        &mut self,
        store: &Store,
        segment_id: i64,
        samples: &[f32],
        mic: &MicEnroll<'_>,
        now_utc_ns: i64,
    ) -> Result<Outcome> {
        let prepared = self.prepare(samples)?;
        self.commit_mic(store, segment_id, prepared, mic, now_utc_ns)
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

/// Keep this turn's audio as a golden sample, if it earns a slot.
///
/// "Up to N, longest": under the cap anything qualifying is kept; at the cap a
/// longer clip replaces the shortest one, because a golden exists to re-enrol a
/// future model and three seconds of speech does that better than one. Written
/// at most once per segment — the path carries the segment id, so a re-analysis
/// of the same turn finds its own file already there and does nothing.
fn keep_golden(
    store: &Store,
    mic: &MicEnroll<'_>,
    segment_id: i64,
    duration_s: f32,
) -> Result<bool> {
    if mic.max_goldens == 0 {
        return Ok(false);
    }
    let rel = golden_path(mic.speaker_id, segment_id);
    let rel_str = rel.to_string_lossy().to_string();

    let existing = store.golden_samples_for(mic.speaker_id)?;
    if existing.iter().any(|g| g.audio_path == rel_str) {
        return Ok(false);
    }
    let mut evict = None;
    if existing.len() >= mic.max_goldens {
        // `golden_samples_for` is longest-first, so the last row is the one to
        // beat. Not beating it is the common case and costs nothing.
        let Some(shortest) = existing.last() else {
            return Ok(false);
        };
        if shortest.duration_s >= duration_s {
            return Ok(false);
        }
        evict = Some(shortest.clone());
    }

    // Copy rather than move: the segment's own WAV still belongs to the
    // transcript and to `segments.audio`, and retention still owns its life.
    let Some((source_rel, _, _)) = store.segment_audio(segment_id)? else {
        return Ok(false);
    };
    if source_rel.is_empty() {
        return Ok(false);
    }
    let dst = mic.data_dir.join(&rel);
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::copy(mic.data_dir.join(&source_rel), &dst)
        .with_context(|| format!("copying {source_rel} to {}", dst.display()))?;
    store.add_golden_sample(mic.speaker_id, &rel_str, duration_s)?;

    if let Some(old) = evict {
        // Row first, then the file: a file with no row is residue the
        // reconciliation sweep understands; a row with no file is a lie.
        if let Some(path) = store.delete_golden_sample(old.id)?
            && !path.is_empty()
        {
            let _ = std::fs::remove_file(mic.data_dir.join(path));
        }
    }
    info!(
        segment_id,
        speaker = mic.speaker_id,
        duration_s,
        path = %rel_str,
        "microphone: kept a golden sample"
    );
    Ok(true)
}

/// Counters the daemon logs on shutdown.
#[derive(Default)]
pub struct AnalysisStats {
    pub analysed: std::sync::atomic::AtomicU64,
    pub labelled: std::sync::atomic::AtomicU64,
    pub refused_overlap: std::sync::atomic::AtomicU64,
    /// Turns labelled from the microphone's provenance rather than a match.
    pub mic_segments: std::sync::atomic::AtomicU64,
    pub mic_enrolled: std::sync::atomic::AtomicU64,
    pub mic_goldens: std::sync::atomic::AtomicU64,
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
            Decision::Pinned { .. } => {
                self.labelled.fetch_add(1, Ordering::Relaxed);
                self.mic_segments.fetch_add(1, Ordering::Relaxed);
                if outcome.enrolled {
                    self.mic_enrolled.fetch_add(1, Ordering::Relaxed);
                }
                if outcome.golden {
                    self.mic_goldens.fetch_add(1, Ordering::Relaxed);
                }
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

/// `analyse_or_log` for a turn that came off the user's own microphone.
///
/// Same shape, same lock discipline; the only difference is which `commit` runs
/// — and that difference is the whole point, because a mic turn must never fall
/// through to the voicebank.
pub fn analyse_mic_or_log(
    analyzer: &mut Analyzer,
    store: &std::sync::Mutex<Store>,
    stats: &AnalysisStats,
    segment_id: i64,
    samples: &[f32],
    mic: &MicEnroll<'_>,
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
    match analyzer.commit_mic(&store, segment_id, prepared, mic, now_utc_ns) {
        Ok(outcome) => stats.record(&outcome),
        Err(e) => warn!(segment_id, "storing microphone analysis failed: {e:#}"),
    }
}
