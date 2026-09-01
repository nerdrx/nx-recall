//! Voicebank matching: who, if anyone, is this turn.
//!
//! Pure decision logic, no I/O and no model, so the operating point can be
//! tested exhaustively. The two rules that matter and why:
//!
//! * **The overlap gate is not advisory.** Step 0 showed that at equal
//!   loudness the embedder is *captured* by one talker rather than blended
//!   between two, so it stays confident while being wrong about half the time.
//!   Neither a higher threshold nor a top1/top2 margin sees it. Only the
//!   independent segmentation model does, so a turn above `max_overlap` gets no
//!   speaker at all — not a low-confidence one.
//! * **Labelling and enrolling are different decisions.** A wrong label costs a
//!   moment of confusion; a wrong prototype permanently corrupts the bank.

use anyhow::Result;

use crate::config::IdentityConfig;
use crate::embed::Embedding;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Candidate {
    pub speaker_id: i64,
    pub score: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Two people are talking at once; no embedding of this is trustworthy.
    Overlapped,
    /// Too little audio to embed reliably.
    TooShort,
}

impl Refusal {
    pub fn as_str(&self) -> &'static str {
        match self {
            Refusal::Overlapped => "overlapped",
            Refusal::TooShort => "too short",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// No speaker, and no embedding stored either.
    Refused(Refusal),
    /// Matched an existing speaker. `enroll` is the separate, stricter verdict
    /// on whether this turn is good enough to become a prototype.
    Matched {
        speaker_id: i64,
        score: f32,
        enroll: bool,
    },
    /// Nothing in the bank is close enough; mint a new speaker. The turn seeds
    /// the new speaker's first prototype — it already passed the overlap and
    /// duration gates, and without a seed the bank could never grow.
    Mint { best_score: Option<f32> },
    /// The turn came off the user's own microphone, so the speaker is known
    /// before any model runs. This is **provenance, not a match**: the
    /// voicebank is never consulted and `match_score` stays NULL, because a
    /// score would claim a comparison that never happened.
    Pinned { speaker_id: i64 },
}

/// Rank the bank: one score per speaker, the best of that speaker's prototypes,
/// highest first.
///
/// Prototypes must already be filtered to the probe's `embed_model_id`; the
/// cosine call enforces it anyway and turns a leak into an error.
pub fn rank(probe: &Embedding, prototypes: &[(i64, Embedding)]) -> Result<Vec<Candidate>> {
    let mut best: Vec<Candidate> = Vec::new();
    for (speaker_id, proto) in prototypes {
        let score = probe.cosine(proto)?;
        match best.iter_mut().find(|c| c.speaker_id == *speaker_id) {
            Some(c) => c.score = c.score.max(score),
            None => best.push(Candidate {
                speaker_id: *speaker_id,
                score,
            }),
        }
    }
    best.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.speaker_id.cmp(&b.speaker_id))
    });
    Ok(best)
}

/// Is this turn worth embedding at all? Checked before the embedder runs, so
/// audio the gate would reject never costs an inference and never lands in the
/// `embeddings` table.
pub fn gate(cfg: &IdentityConfig, overlap_frac: f32, duration_s: f32) -> Option<Refusal> {
    if overlap_frac > cfg.max_overlap {
        return Some(Refusal::Overlapped);
    }
    if duration_s < cfg.min_duration_s {
        return Some(Refusal::TooShort);
    }
    None
}

/// Apply the operating point. `ranked` must be sorted descending (as `rank`
/// returns it).
pub fn decide(
    cfg: &IdentityConfig,
    overlap_frac: f32,
    duration_s: f32,
    ranked: &[Candidate],
) -> Decision {
    if let Some(refusal) = gate(cfg, overlap_frac, duration_s) {
        return Decision::Refused(refusal);
    }

    let Some(top) = ranked.first() else {
        return Decision::Mint { best_score: None };
    };
    if top.score < cfg.label_threshold {
        return Decision::Mint {
            best_score: Some(top.score),
        };
    }

    let runner_up = ranked.get(1).map(|c| c.score).unwrap_or(f32::NEG_INFINITY);
    let margin = top.score - runner_up;
    let enroll = top.score >= cfg.enroll_threshold
        && margin >= cfg.enroll_margin
        && overlap_frac <= cfg.enroll_max_overlap
        && duration_s >= cfg.enroll_min_duration_s;

    Decision::Matched {
        speaker_id: top.speaker_id,
        score: top.score,
        enroll,
    }
}

/// Which prototype to evict to make room for `incoming`, once a speaker is at
/// its cap.
///
/// The nearest existing prototype goes: it is the one `incoming` is most
/// redundant with, so dropping it spends the budget on coverage of the
/// speaker's range rather than on twenty recordings of the same sentence.
/// Golden (hand-enrolled) prototypes are never candidates.
pub fn prototype_to_evict(
    incoming: &Embedding,
    existing: &[(i64, Embedding, bool)],
) -> Result<Option<i64>> {
    let mut worst: Option<(i64, f32)> = None;
    for (id, proto, is_golden) in existing {
        if *is_golden {
            continue;
        }
        let score = incoming.cosine(proto)?;
        if worst.is_none_or(|(_, s)| score > s) {
            worst = Some((*id, score));
        }
    }
    Ok(worst.map(|(id, _)| id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> IdentityConfig {
        IdentityConfig::default()
    }

    fn c(id: i64, score: f32) -> Candidate {
        Candidate {
            speaker_id: id,
            score,
        }
    }

    fn e(v: &[f32]) -> Embedding {
        Embedding::new("m@1", v.to_vec())
    }

    // ---- the overlap gate ------------------------------------------------

    #[test]
    fn an_overlapped_turn_is_refused_however_confident_the_match() {
        let d = decide(&cfg(), 0.6, 5.0, &[c(1, 0.99)]);
        assert_eq!(d, Decision::Refused(Refusal::Overlapped));
    }

    #[test]
    fn the_overlap_gate_is_at_one_tenth_and_is_inclusive() {
        assert!(matches!(
            decide(&cfg(), 0.10, 5.0, &[c(1, 0.9)]),
            Decision::Matched { .. }
        ));
        assert_eq!(
            decide(&cfg(), 0.101, 5.0, &[c(1, 0.9)]),
            Decision::Refused(Refusal::Overlapped)
        );
    }

    #[test]
    fn a_short_turn_is_refused_before_any_scoring() {
        assert_eq!(
            decide(&cfg(), 0.0, 0.99, &[c(1, 0.99)]),
            Decision::Refused(Refusal::TooShort)
        );
        assert!(matches!(
            decide(&cfg(), 0.0, 1.0, &[c(1, 0.99)]),
            Decision::Matched { .. }
        ));
    }

    #[test]
    fn overlap_is_checked_before_duration() {
        assert_eq!(
            decide(&cfg(), 0.9, 0.1, &[]),
            Decision::Refused(Refusal::Overlapped)
        );
    }

    // ---- labelling -------------------------------------------------------

    #[test]
    fn an_empty_bank_mints() {
        assert_eq!(
            decide(&cfg(), 0.0, 5.0, &[]),
            Decision::Mint { best_score: None }
        );
    }

    #[test]
    fn below_the_label_threshold_mints_and_reports_what_it_saw() {
        assert_eq!(
            decide(&cfg(), 0.0, 5.0, &[c(1, 0.34)]),
            Decision::Mint {
                best_score: Some(0.34)
            }
        );
    }

    #[test]
    fn the_label_threshold_is_inclusive() {
        assert!(matches!(
            decide(&cfg(), 0.0, 5.0, &[c(7, 0.35)]),
            Decision::Matched { speaker_id: 7, .. }
        ));
    }

    // ---- enrolment: every condition is separately load-bearing -----------

    #[test]
    fn enrolment_needs_all_four_conditions() {
        // Baseline: comfortably over every bar.
        let d = decide(&cfg(), 0.0, 5.0, &[c(1, 0.80), c(2, 0.40)]);
        assert_eq!(
            d,
            Decision::Matched {
                speaker_id: 1,
                score: 0.80,
                enroll: true
            }
        );

        let labelled_not_enrolled = |d: Decision| match d {
            Decision::Matched { enroll, .. } => !enroll,
            _ => false,
        };
        // Score under the enrol threshold but over the label threshold.
        assert!(labelled_not_enrolled(decide(
            &cfg(),
            0.0,
            5.0,
            &[c(1, 0.54), c(2, 0.10)]
        )));
        // Margin too thin.
        assert!(labelled_not_enrolled(decide(
            &cfg(),
            0.0,
            5.0,
            &[c(1, 0.80), c(2, 0.75)]
        )));
        // Overlap over the (stricter) enrol ceiling but under the label gate.
        assert!(labelled_not_enrolled(decide(
            &cfg(),
            0.08,
            5.0,
            &[c(1, 0.80), c(2, 0.10)]
        )));
        // Too short to enrol, long enough to label.
        assert!(labelled_not_enrolled(decide(
            &cfg(),
            0.0,
            2.9,
            &[c(1, 0.80), c(2, 0.10)]
        )));
    }

    #[test]
    fn the_enrolment_boundaries_are_inclusive() {
        let d = decide(&cfg(), 0.05, 3.0, &[c(1, 0.55), c(2, 0.49)]);
        assert_eq!(
            d,
            Decision::Matched {
                speaker_id: 1,
                score: 0.55,
                enroll: true
            }
        );
    }

    #[test]
    fn a_sole_candidate_has_no_runner_up_to_lose_the_margin_to() {
        let d = decide(&cfg(), 0.0, 5.0, &[c(1, 0.90)]);
        assert_eq!(
            d,
            Decision::Matched {
                speaker_id: 1,
                score: 0.90,
                enroll: true
            }
        );
    }

    // ---- ranking ---------------------------------------------------------

    #[test]
    fn a_speaker_scores_the_best_of_its_prototypes() {
        let probe = e(&[1.0, 0.0]);
        let bank = vec![
            (1, e(&[0.0, 1.0])),  // 0.0
            (1, e(&[1.0, 0.05])), // ~1.0
            (2, e(&[1.0, 1.0])),  // ~0.707
        ];
        let ranked = rank(&probe, &bank).unwrap();
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].speaker_id, 1);
        assert!(ranked[0].score > 0.99);
        assert_eq!(ranked[1].speaker_id, 2);
    }

    #[test]
    fn ranking_a_foreign_vector_is_an_error_not_a_score() {
        let probe = Embedding::new("other@1", vec![1.0, 0.0]);
        assert!(rank(&probe, &[(1, e(&[1.0, 0.0]))]).is_err());
    }

    #[test]
    fn ranking_an_empty_bank_is_empty_not_an_error() {
        assert!(rank(&e(&[1.0]), &[]).unwrap().is_empty());
    }

    // ---- prototype eviction ---------------------------------------------

    #[test]
    fn the_most_redundant_prototype_is_evicted() {
        let incoming = e(&[1.0, 0.0]);
        let existing = vec![
            (10, e(&[0.0, 1.0]), false),
            (11, e(&[1.0, 0.02]), false), // nearest to the incoming vector
            (12, e(&[1.0, 1.0]), false),
        ];
        assert_eq!(prototype_to_evict(&incoming, &existing).unwrap(), Some(11));
    }

    #[test]
    fn golden_prototypes_are_never_evicted() {
        let incoming = e(&[1.0, 0.0]);
        let existing = vec![
            (10, e(&[1.0, 0.0]), true), // identical, but hand-enrolled
            (11, e(&[0.0, 1.0]), false),
        ];
        assert_eq!(prototype_to_evict(&incoming, &existing).unwrap(), Some(11));
    }

    #[test]
    fn an_all_golden_speaker_has_nothing_to_evict() {
        let incoming = e(&[1.0, 0.0]);
        let existing = vec![(10, e(&[1.0, 0.0]), true)];
        assert_eq!(prototype_to_evict(&incoming, &existing).unwrap(), None);
    }
}
