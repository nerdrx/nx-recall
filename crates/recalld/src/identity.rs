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
    /// Nothing matched, and this turn is **too slight to be worth an
    /// identity** (0.6.1): under `mint_min_duration_s`, or fewer than
    /// `mint_min_words` words. The segment keeps its transcript and its
    /// embedding and stays speaker-NULL.
    ///
    /// The bar sits *above* the label bar deliberately. Recognising a grunt as
    /// somebody already known costs nothing and is often right; minting a new
    /// voice from one is how a voicebank fills with rows nobody can name — and
    /// a wrong new identity is permanent in a way a wrong label is not.
    TooSlight {
        best_score: Option<f32>,
        duration_s: f32,
        words: usize,
    },
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
    rank_with(probe, prototypes, crate::calib::Aggregate::Max)
}

/// [`rank`], with the rule for turning a voice's several prototypes into one
/// score looked up instead of assumed (0.11.9).
///
/// `rank` is exactly this with [`Aggregate::Max`](crate::calib::Aggregate::Max),
/// so the pre-0.11.9 behaviour is not a second code path that could drift from
/// this one.
pub fn rank_with(
    probe: &Embedding,
    prototypes: &[(i64, Embedding)],
    aggregate: crate::calib::Aggregate,
) -> Result<Vec<Candidate>> {
    // Grouped rather than folded, because a top-k mean cannot be accumulated
    // one prototype at a time — it needs the voice's whole list at once.
    let mut per_voice: Vec<(i64, Vec<f32>)> = Vec::new();
    for (speaker_id, proto) in prototypes {
        let score = probe.cosine(proto)?;
        match per_voice.iter_mut().find(|(id, _)| id == speaker_id) {
            Some((_, v)) => v.push(score),
            None => per_voice.push((*speaker_id, vec![score])),
        }
    }
    let mut best: Vec<Candidate> = per_voice
        .into_iter()
        .filter_map(|(speaker_id, mut scores)| {
            let score = aggregate.of(&mut scores);
            (!score.is_nan()).then_some(Candidate { speaker_id, score })
        })
        .collect();
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

/// Is this turn substantial enough to be worth a *new* identity?
///
/// Separate from `gate` because it is a different question with a different
/// answer: `gate` decides whether the audio can be embedded at all, this
/// decides whether an unrecognised voice earns a row in the voicebank.
pub fn mints(cfg: &IdentityConfig, duration_s: f32, words: usize) -> bool {
    duration_s >= cfg.mint_min_duration_s && words >= cfg.mint_min_words
}

/// Apply the operating point. `ranked` must be sorted descending (as `rank`
/// returns it). `words` is the transcript's word count, which only the mint
/// bar looks at — matching an existing voice never depends on what was said.
pub fn decide(
    cfg: &IdentityConfig,
    overlap_frac: f32,
    duration_s: f32,
    words: usize,
    ranked: &[Candidate],
) -> Decision {
    decide_with(
        cfg,
        &crate::calib::Thresholds::global(cfg.label_threshold, 0.0),
        overlap_frac,
        duration_s,
        words,
        ranked,
    )
}

// ---- 0.11.0: learned identity ---------------------------------------------

/// [`decide`], with the label bar looked up per voice instead of read off the
/// config (0.11.0).
///
/// Two rules keep this from being a second ladder:
///
/// * only the **label** decision consults `thresholds`. Enrolment keeps every
///   global bar it had, because a wrong prototype is permanent and nothing has
///   measured a per-voice enrol point;
/// * the lookup applies to the **top** candidate only, and a top candidate
///   that fails its own bar does not hand the turn to the runner-up. The
///   ladder's shape is unchanged; one number in it moved.
///
/// `decide` is exactly this function with an empty table, so the 0.10.2
/// behaviour is not a separate code path that could drift.
pub fn decide_with(
    cfg: &IdentityConfig,
    thresholds: &crate::calib::Thresholds,
    overlap_frac: f32,
    duration_s: f32,
    words: usize,
    ranked: &[Candidate],
) -> Decision {
    if let Some(refusal) = gate(cfg, overlap_frac, duration_s) {
        return Decision::Refused(refusal);
    }

    let mint = |best_score: Option<f32>| {
        if mints(cfg, duration_s, words) {
            Decision::Mint { best_score }
        } else {
            Decision::TooSlight {
                best_score,
                duration_s,
                words,
            }
        }
    };

    let Some(top) = ranked.first() else {
        return mint(None);
    };
    let (label_threshold, label_margin) = thresholds.for_speaker(top.speaker_id);
    let runner_up = ranked.get(1).map(|c| c.score).unwrap_or(f32::NEG_INFINITY);
    let margin = top.score - runner_up;
    if top.score < label_threshold || margin < label_margin {
        return mint(Some(top.score));
    }

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

// ---- end 0.11.0 -----------------------------------------------------------

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
        let d = decide(&cfg(), 0.6, 5.0, 5, &[c(1, 0.99)]);
        assert_eq!(d, Decision::Refused(Refusal::Overlapped));
    }

    #[test]
    fn the_overlap_gate_is_at_one_tenth_and_is_inclusive() {
        assert!(matches!(
            decide(&cfg(), 0.10, 5.0, 5, &[c(1, 0.9)]),
            Decision::Matched { .. }
        ));
        assert_eq!(
            decide(&cfg(), 0.101, 5.0, 5, &[c(1, 0.9)]),
            Decision::Refused(Refusal::Overlapped)
        );
    }

    #[test]
    fn a_short_turn_is_refused_before_any_scoring() {
        assert_eq!(
            decide(&cfg(), 0.0, 0.99, 5, &[c(1, 0.99)]),
            Decision::Refused(Refusal::TooShort)
        );
        assert!(matches!(
            decide(&cfg(), 0.0, 1.0, 5, &[c(1, 0.99)]),
            Decision::Matched { .. }
        ));
    }

    #[test]
    fn overlap_is_checked_before_duration() {
        assert_eq!(
            decide(&cfg(), 0.9, 0.1, 5, &[]),
            Decision::Refused(Refusal::Overlapped)
        );
    }

    // ---- labelling -------------------------------------------------------

    #[test]
    fn an_empty_bank_mints() {
        assert_eq!(
            decide(&cfg(), 0.0, 5.0, 5, &[]),
            Decision::Mint { best_score: None }
        );
    }

    #[test]
    fn below_the_label_threshold_mints_and_reports_what_it_saw() {
        assert_eq!(
            decide(&cfg(), 0.0, 5.0, 5, &[c(1, 0.34)]),
            Decision::Mint {
                best_score: Some(0.34)
            }
        );
    }

    #[test]
    fn the_label_threshold_is_inclusive() {
        assert!(matches!(
            decide(&cfg(), 0.0, 5.0, 5, &[c(7, 0.35)]),
            Decision::Matched { speaker_id: 7, .. }
        ));
    }

    // ---- the mint bar: every condition is separately load-bearing --------

    #[test]
    fn a_grunt_matches_an_existing_voice_but_never_mints_a_new_one() {
        // The bar is above the label bar, not instead of it: half a second of
        // "hm" that scores over `label_threshold` still gets its name.
        assert!(matches!(
            decide(&cfg(), 0.0, 1.2, 1, &[c(4, 0.80)]),
            Decision::Matched { speaker_id: 4, .. }
        ));
        // The same grunt against an empty bank mints nothing.
        assert_eq!(
            decide(&cfg(), 0.0, 1.2, 1, &[]),
            Decision::TooSlight {
                best_score: None,
                duration_s: 1.2,
                words: 1
            }
        );
    }

    #[test]
    fn each_half_of_the_mint_bar_can_fail_on_its_own() {
        // Long enough, but one word.
        assert!(matches!(
            decide(&cfg(), 0.0, 5.0, 1, &[]),
            Decision::TooSlight { words: 1, .. }
        ));
        // Wordy enough, but too short.
        assert!(matches!(
            decide(&cfg(), 0.0, 1.9, 6, &[]),
            Decision::TooSlight { duration_s, .. } if (duration_s - 1.9).abs() < 1e-6
        ));
        // Both satisfied, at exactly the bar: inclusive on each.
        assert_eq!(
            decide(&cfg(), 0.0, 2.0, 2, &[]),
            Decision::Mint { best_score: None }
        );
    }

    #[test]
    fn a_near_miss_below_the_bar_reports_what_it_saw() {
        // Under the label threshold, so nothing matched — and under the mint
        // bar, so nothing is minted either. The best score is still reported:
        // it is what a later reassign would be argued from.
        assert_eq!(
            decide(&cfg(), 0.0, 1.0, 4, &[c(1, 0.30)]),
            Decision::TooSlight {
                best_score: Some(0.30),
                duration_s: 1.0,
                words: 4
            }
        );
    }

    #[test]
    fn the_overlap_gate_still_outranks_the_mint_bar() {
        // Overlapped audio is refused outright: no embedding, no label, and the
        // question of whether it *would* have minted never arises.
        assert_eq!(
            decide(&cfg(), 0.9, 1.0, 1, &[]),
            Decision::Refused(Refusal::Overlapped)
        );
    }

    // ---- enrolment: every condition is separately load-bearing -----------

    #[test]
    fn enrolment_needs_all_four_conditions() {
        // Baseline: comfortably over every bar.
        let d = decide(&cfg(), 0.0, 5.0, 5, &[c(1, 0.80), c(2, 0.40)]);
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
            5,
            &[c(1, 0.54), c(2, 0.10)]
        )));
        // Margin too thin.
        assert!(labelled_not_enrolled(decide(
            &cfg(),
            0.0,
            5.0,
            5,
            &[c(1, 0.80), c(2, 0.75)]
        )));
        // Overlap over the (stricter) enrol ceiling but under the label gate.
        assert!(labelled_not_enrolled(decide(
            &cfg(),
            0.08,
            5.0,
            5,
            &[c(1, 0.80), c(2, 0.10)]
        )));
        // Too short to enrol, long enough to label.
        assert!(labelled_not_enrolled(decide(
            &cfg(),
            0.0,
            2.9,
            5,
            &[c(1, 0.80), c(2, 0.10)]
        )));
    }

    #[test]
    fn the_enrolment_boundaries_are_inclusive() {
        let d = decide(&cfg(), 0.05, 3.0, 5, &[c(1, 0.55), c(2, 0.49)]);
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
        let d = decide(&cfg(), 0.0, 5.0, 5, &[c(1, 0.90)]);
        assert_eq!(
            d,
            Decision::Matched {
                speaker_id: 1,
                score: 0.90,
                enroll: true
            }
        );
    }

    // ---- how a voice's prototypes become one score (0.11.9) --------------

    #[test]
    fn top_k_ranking_prefers_the_voice_that_agrees_with_itself() {
        // Speaker 1 has one prototype that happens to be very close, and two
        // that are not. Speaker 2 has three that all agree. Under max cosine
        // speaker 1 wins on its one lucky prototype; under a top-3 mean the
        // voice whose whole record supports the claim wins. On this install
        // that difference is most of the Rowan/Aspen confusion (§32).
        let probe = e(&[1.0, 0.0]);
        let bank = vec![
            (1, e(&[0.95, 0.31])),
            (1, e(&[0.20, 0.98])),
            (1, e(&[0.10, 0.99])),
            (2, e(&[0.90, 0.44])),
            (2, e(&[0.88, 0.47])),
            (2, e(&[0.89, 0.46])),
        ];
        assert_eq!(rank(&probe, &bank).unwrap()[0].speaker_id, 1);
        let r = rank_with(&probe, &bank, crate::calib::Aggregate::TopK(3)).unwrap();
        assert_eq!(r[0].speaker_id, 2, "{r:?}");
    }

    #[test]
    fn rank_is_rank_with_max_and_not_a_second_code_path() {
        let probe = e(&[1.0, 0.0]);
        let bank = vec![
            (1, e(&[0.9, 0.4])),
            (1, e(&[0.5, 0.8])),
            (2, e(&[0.7, 0.7])),
        ];
        assert_eq!(
            rank(&probe, &bank).unwrap(),
            rank_with(&probe, &bank, crate::calib::Aggregate::Max).unwrap()
        );
    }

    #[test]
    fn top_k_still_refuses_to_compare_across_models() {
        let probe = Embedding::new("a@1", vec![1.0, 0.0]);
        let bank = vec![(1, Embedding::new("b@1", vec![1.0, 0.0]))];
        assert!(rank_with(&probe, &bank, crate::calib::Aggregate::TopK(3)).is_err());
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

    // ---- 0.11.0: per-voice thresholds ------------------------------------

    #[test]
    fn a_learned_threshold_raises_the_bar_for_one_voice_only() {
        let t = crate::calib::Thresholds::global(cfg().label_threshold, 0.0).with(1, 0.55, 0.0);
        // Voice 1 at 0.40 used to be a match and is now a mint.
        assert_eq!(
            decide_with(&cfg(), &t, 0.0, 5.0, 5, &[c(1, 0.40)]),
            Decision::Mint {
                best_score: Some(0.40)
            }
        );
        // Voice 2, at the same score, is untouched.
        assert!(matches!(
            decide_with(&cfg(), &t, 0.0, 5.0, 5, &[c(2, 0.40)]),
            Decision::Matched { speaker_id: 2, .. }
        ));
    }

    #[test]
    fn a_learned_threshold_can_also_lower_the_bar() {
        let t = crate::calib::Thresholds::global(cfg().label_threshold, 0.0).with(1, 0.31, 0.0);
        assert!(matches!(
            decide_with(&cfg(), &t, 0.0, 5.0, 5, &[c(1, 0.32)]),
            Decision::Matched { speaker_id: 1, .. }
        ));
    }

    #[test]
    fn a_learned_margin_declines_rather_than_promoting_the_runner_up() {
        // The point of the shape: a top candidate that fails its own margin
        // does NOT hand the turn to whoever was second.
        let t = crate::calib::Thresholds::global(cfg().label_threshold, 0.0).with(1, 0.35, 0.10);
        assert_eq!(
            decide_with(&cfg(), &t, 0.0, 5.0, 5, &[c(1, 0.60), c(2, 0.58)]),
            Decision::Mint {
                best_score: Some(0.60)
            }
        );
    }

    #[test]
    fn an_empty_learned_table_is_exactly_the_old_ladder() {
        let t = crate::calib::Thresholds::global(cfg().label_threshold, 0.0);
        for score in [0.10, 0.34, 0.35, 0.54, 0.55, 0.99] {
            for overlap in [0.0, 0.06, 0.2] {
                for duration in [0.5, 2.5, 5.0] {
                    let ranked = [c(1, score), c(2, score - 0.03)];
                    assert_eq!(
                        decide(&cfg(), overlap, duration, 5, &ranked),
                        decide_with(&cfg(), &t, overlap, duration, 5, &ranked),
                    );
                }
            }
        }
    }

    #[test]
    fn a_learned_threshold_never_relaxes_the_enrol_bar() {
        // Voice 1's label bar is learned down to 0.31, but enrolment still
        // wants 0.55: a wrong prototype is permanent and nothing here measured
        // that decision.
        let t = crate::calib::Thresholds::global(cfg().label_threshold, 0.0).with(1, 0.31, 0.0);
        assert_eq!(
            decide_with(&cfg(), &t, 0.0, 5.0, 5, &[c(1, 0.40)]),
            Decision::Matched {
                speaker_id: 1,
                score: 0.40,
                enroll: false
            }
        );
    }

    #[test]
    fn an_all_golden_speaker_has_nothing_to_evict() {
        let incoming = e(&[1.0, 0.0]);
        let existing = vec![(10, e(&[1.0, 0.0]), true)];
        assert_eq!(prototype_to_evict(&incoming, &existing).unwrap(), None);
    }
}
