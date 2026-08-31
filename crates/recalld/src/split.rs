//! Undoing a false merge: cutting one speaker back into two.
//!
//! False merges are the poison this feature exists to cure (FINDINGS §5): the
//! embedder is *captured* by one talker in an equal-loudness mix rather than
//! blended between two, so it stays confident while being wrong — and every
//! turn it mislabels lands in one voice's bank. The recovery is only possible
//! because a prototype remembers `source_segment_id` (DESIGN §5/§6), so the
//! vectors that made the mistake can be re-clustered and the segments they came
//! from can follow them out.
//!
//! Three rules shape everything here, and each is a refusal rather than a
//! guess:
//!
//! * **A split that finds one voice is refused.** If the two candidate
//!   centroids sit closer than `split_max_centroid_similarity`, k-means has
//!   done what k-means always does — returned two clusters — and they are two
//!   halves of one person. Inventing a second identity is exactly as damaging
//!   as the merge being undone.
//! * **Undecidable segments stay put.** A segment near the boundary is the case
//!   the spike proved the score cannot see. It keeps the existing speaker at
//!   the *weaker* of its two similarities, so the correction UI can see that
//!   the label is not to be trusted.
//! * **Goldens are ground truth.** Hand-enrolled audio pins its cluster to the
//!   existing speaker id. Goldens of one speaker landing on both sides is not a
//!   split, it is conflicting evidence, and it is refused.
//!
//! Pure decision logic: no I/O, no model, no database. The store hands in the
//! vectors and writes back what this returns.

use crate::config::IdentityConfig;
use crate::embed::Embedding;

/// Fixed, so the same voicebank always splits the same way. A split a user
/// re-runs after a rename must not land differently.
const SEED: u64 = 0x5EED_5D11_7A5C_0DE1;

/// Lloyd iterations per restart. Two clusters over a capped bank converge in a
/// handful; the bound only exists so an oscillation cannot spin.
const MAX_ITERATIONS: usize = 50;

/// One vector belonging to the speaker being examined, and what it is.
///
/// A prototype whose `source_segment_id` still exists carries both ids: it is
/// one piece of evidence, not two, and moving it moves the segment with it.
#[derive(Debug, Clone, PartialEq)]
pub struct Vector {
    pub prototype_id: Option<i64>,
    pub is_golden: bool,
    pub segment_id: Option<i64>,
    pub embedding: Embedding,
}

/// What the re-cluster decided. Everything not named here stays exactly as it
/// is — including the confident majority, whose scores are left alone rather
/// than rewritten with a number a different algorithm produced.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// Prototypes that move to the new speaker.
    pub moved_prototypes: Vec<i64>,
    /// Segments that move, with their similarity to the new centroid.
    pub moved_segments: Vec<(i64, f32)>,
    /// Segments in the band between the centroids: they keep the existing
    /// speaker, recorded at the lower of the two similarities.
    pub ambiguous_segments: Vec<(i64, f32)>,
    /// `cos(kept centroid, new centroid)` — the number the refusal reads.
    pub centroid_similarity: f32,
    pub kept_vectors: usize,
    pub moved_vectors: usize,
}

/// Why a split did not happen. Every one of these is a deliberate decision to
/// leave the database alone.
#[derive(Debug, Clone, PartialEq)]
pub enum Refusal {
    /// Fewer than two comparable vectors: nothing to cluster.
    NotEnoughVectors { have: usize },
    /// The two centroids are one voice.
    OneVoice { similarity: f32 },
    /// Hand-enrolled audio for this speaker landed in both clusters.
    GoldenConflict,
    /// A second cluster exists but everything in it is undecidable, so nothing
    /// would move.
    NothingToMove,
    /// Vectors from two embedding spaces reached the clusterer. Comparing them
    /// is forbidden (DESIGN §5); this is a bug, not a user error.
    Incomparable { a: String, b: String },
}

impl Refusal {
    pub fn message(&self) -> String {
        match self {
            Refusal::NotEnoughVectors { have } => format!(
                "this voice has {have} usable embedding(s); a split needs at least two \
                 to have anything to separate"
            ),
            Refusal::OneVoice { similarity } => format!(
                "the two candidate voices score {similarity:.3} against each other — \
                 that is one person, not two; refusing to split"
            ),
            Refusal::GoldenConflict => "hand-enrolled samples for this speaker fall on both \
                 sides of the split; that is conflicting evidence, not two voices"
                .to_string(),
            Refusal::NothingToMove => "the second cluster is entirely inside the undecidable \
                 band, so nothing would move"
                .to_string(),
            Refusal::Incomparable { a, b } => {
                format!("embeddings from two models reached the clusterer: {a} vs {b}")
            }
        }
    }

    /// The wire code. Everything except the model leak is a decision, not a
    /// fault.
    pub fn code(&self) -> &'static str {
        match self {
            Refusal::Incomparable { .. } => "internal",
            _ => "refused",
        }
    }
}

/// Re-cluster one speaker's vectors into two, or say why not.
pub fn plan(cfg: &IdentityConfig, vectors: &[Vector]) -> Result<Plan, Refusal> {
    // One embedding space or none: a cosine between two of them is a number
    // that looks like a score and means nothing.
    if let Some(first) = vectors.first()
        && let Some(other) = vectors
            .iter()
            .find(|v| v.embedding.model_id != first.embedding.model_id)
    {
        return Err(Refusal::Incomparable {
            a: first.embedding.model_id.clone(),
            b: other.embedding.model_id.clone(),
        });
    }

    let units: Vec<Vec<f32>> = vectors
        .iter()
        .filter_map(|v| unit(&v.embedding.vector))
        .collect();
    if units.len() != vectors.len() || units.len() < 2 {
        // A zero vector has no direction to cluster on; rather than let it drag
        // a centroid to the origin, the whole split is refused and the caller
        // keeps its speaker intact.
        return Err(Refusal::NotEnoughVectors { have: units.len() });
    }
    let dim = units[0].len();
    if units.iter().any(|u| u.len() != dim) {
        return Err(Refusal::Incomparable {
            a: format!("{dim} dimensions"),
            b: "a different dimension".into(),
        });
    }

    let Some((centroids, assignment)) = two_means(&units, cfg.split_restarts.max(1)) else {
        // Every restart collapsed into one cluster: there is only one direction
        // in this bank.
        return Err(Refusal::OneVoice { similarity: 1.0 });
    };

    let centroid_similarity = dot(&centroids[0], &centroids[1]).clamp(-1.0, 1.0);
    if centroid_similarity > cfg.split_max_centroid_similarity {
        return Err(Refusal::OneVoice {
            similarity: centroid_similarity,
        });
    }

    // Goldens decide which cluster keeps the identity, and disagree at their
    // peril.
    let mut golden_side: Option<usize> = None;
    for (i, v) in vectors.iter().enumerate() {
        if !v.is_golden {
            continue;
        }
        match golden_side {
            None => golden_side = Some(assignment[i]),
            Some(side) if side != assignment[i] => return Err(Refusal::GoldenConflict),
            Some(_) => {}
        }
    }

    let keep = golden_side.unwrap_or_else(|| {
        let zeros = assignment.iter().filter(|a| **a == 0).count();
        let ones = assignment.len() - zeros;
        // The majority keeps the established id; a tie goes to whichever
        // cluster holds the first vector, so the answer never depends on
        // iteration order.
        match ones.cmp(&zeros) {
            std::cmp::Ordering::Greater => 1,
            std::cmp::Ordering::Less => 0,
            std::cmp::Ordering::Equal => assignment[0],
        }
    });
    let other = 1 - keep;

    let mut plan = Plan {
        moved_prototypes: Vec::new(),
        moved_segments: Vec::new(),
        ambiguous_segments: Vec::new(),
        centroid_similarity,
        kept_vectors: 0,
        moved_vectors: 0,
    };
    for (i, v) in vectors.iter().enumerate() {
        let sim_keep = dot(&units[i], &centroids[keep]);
        let sim_other = dot(&units[i], &centroids[other]);
        if (sim_keep - sim_other).abs() < cfg.split_ambiguous_margin {
            plan.kept_vectors += 1;
            if let Some(seg) = v.segment_id {
                plan.ambiguous_segments.push((seg, sim_keep.min(sim_other)));
            }
            continue;
        }
        if sim_other > sim_keep {
            plan.moved_vectors += 1;
            // A prototype and the segment it came from are one piece of
            // evidence and travel together.
            if let Some(p) = v.prototype_id {
                plan.moved_prototypes.push(p);
            }
            if let Some(seg) = v.segment_id {
                plan.moved_segments.push((seg, sim_other));
            }
        } else {
            plan.kept_vectors += 1;
        }
    }

    if plan.moved_prototypes.is_empty() && plan.moved_segments.is_empty() {
        return Err(Refusal::NothingToMove);
    }
    Ok(plan)
}

/// Best-of-N 2-means in cosine space. `None` when every restart collapsed.
fn two_means(units: &[Vec<f32>], restarts: usize) -> Option<([Vec<f32>; 2], Vec<usize>)> {
    let mut best: Option<(f32, [Vec<f32>; 2], Vec<usize>)> = None;
    for r in 0..restarts {
        // Each restart is seeded from the same fixed constant, so N restarts
        // are N *different* starts and the whole search is still reproducible.
        let mut rng = Rng::new(SEED ^ (r as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let Some((centroids, assignment)) = one_run(units, &mut rng) else {
            continue;
        };
        let score: f32 = assignment
            .iter()
            .enumerate()
            .map(|(i, c)| dot(&units[i], &centroids[*c]))
            .sum();
        // Strictly greater: the first restart to reach a score keeps it, so
        // ties resolve to the earliest seed rather than the last.
        if best.as_ref().is_none_or(|(b, _, _)| score > *b) {
            best = Some((score, centroids, assignment));
        }
    }
    best.map(|(_, c, a)| (c, a))
}

fn one_run(units: &[Vec<f32>], rng: &mut Rng) -> Option<([Vec<f32>; 2], Vec<usize>)> {
    let mut centroids = seed_centroids(units, rng)?;
    let mut assignment = assign(units, &centroids);
    for _ in 0..MAX_ITERATIONS {
        centroids = update(units, &assignment)?;
        let next = assign(units, &centroids);
        if next == assignment {
            // The assignment is the arg-max of these centroids, which is what
            // the caller relies on when it reads a golden's side.
            return Some((centroids, assignment));
        }
        assignment = next;
    }
    Some((centroids, assignment))
}

/// k-means++ seeding: the second centre is drawn in proportion to squared
/// cosine distance from the first, which is what stops a restart from picking
/// two neighbours and reporting a split that is really one cluster.
fn seed_centroids(units: &[Vec<f32>], rng: &mut Rng) -> Option<[Vec<f32>; 2]> {
    let first = rng.below(units.len());
    let weights: Vec<f32> = units
        .iter()
        .map(|u| {
            let d = (1.0 - dot(u, &units[first])).max(0.0);
            d * d
        })
        .collect();
    let total: f32 = weights.iter().sum();
    let second = if total <= f32::EPSILON {
        // Every vector sits on top of the first one. Nothing to seed a second
        // cluster with; the caller reads that as one voice.
        return None;
    } else {
        let mut target = rng.unit_f32() * total;
        let mut pick = units.len() - 1;
        for (i, w) in weights.iter().enumerate() {
            target -= w;
            if target <= 0.0 {
                pick = i;
                break;
            }
        }
        pick
    };
    if second == first {
        return None;
    }
    Some([units[first].clone(), units[second].clone()])
}

fn assign(units: &[Vec<f32>], centroids: &[Vec<f32>; 2]) -> Vec<usize> {
    units
        .iter()
        // A tie goes to cluster 0, so the outcome never depends on how the
        // comparison happens to round.
        .map(|u| usize::from(dot(u, &centroids[1]) > dot(u, &centroids[0])))
        .collect()
}

/// The mean of each cluster, re-normalised. `None` if a cluster emptied.
fn update(units: &[Vec<f32>], assignment: &[usize]) -> Option<[Vec<f32>; 2]> {
    let dim = units[0].len();
    let mut sums = [vec![0.0f32; dim], vec![0.0f32; dim]];
    let mut counts = [0usize; 2];
    for (u, c) in units.iter().zip(assignment) {
        counts[*c] += 1;
        for (s, v) in sums[*c].iter_mut().zip(u) {
            *s += v;
        }
    }
    if counts[0] == 0 || counts[1] == 0 {
        return None;
    }
    let [a, b] = sums;
    Some([unit(&a)?, unit(&b)?])
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// A unit vector, or `None` if there is no direction to speak of.
fn unit(v: &[f32]) -> Option<Vec<f32>> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if !norm.is_finite() || norm <= f32::EPSILON {
        return None;
    }
    Some(v.iter().map(|x| x / norm).collect())
}

/// xorshift64. Small, deterministic, and entirely enough to place two seeds.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // The state must never be zero, or the sequence is stuck there.
        Self(if seed == 0 { SEED } else { seed })
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n.max(1) as u64) as usize
    }

    fn unit_f32(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> IdentityConfig {
        IdentityConfig::default()
    }

    /// A vector that is both a prototype and the segment it came from — the
    /// pairing the whole feature depends on.
    fn v(proto: i64, seg: i64, values: &[f32]) -> Vector {
        Vector {
            prototype_id: Some(proto),
            is_golden: false,
            segment_id: Some(seg),
            embedding: Embedding::new("m@1", values.to_vec()),
        }
    }

    fn golden(proto: i64, values: &[f32]) -> Vector {
        Vector {
            is_golden: true,
            ..v(proto, proto + 100, values)
        }
    }

    /// Two clearly separate voices, four vectors each side.
    fn two_voices() -> Vec<Vector> {
        vec![
            v(1, 11, &[1.0, 0.05, 0.0]),
            v(2, 12, &[1.0, 0.0, 0.02]),
            v(3, 13, &[0.98, 0.1, 0.0]),
            v(4, 14, &[0.0, 1.0, 0.05]),
            v(5, 15, &[0.02, 1.0, 0.0]),
            v(6, 16, &[0.0, 0.97, 0.1]),
        ]
    }

    #[test]
    fn two_voices_separate_and_the_majority_keeps_the_id() {
        let items = two_voices();
        let plan = plan(&cfg(), &items).expect("two orthogonal voices must split");
        assert!(
            plan.centroid_similarity < 0.3,
            "centroids scored {:.3}",
            plan.centroid_similarity
        );
        // Three and three: the tie goes to the cluster holding the first
        // vector, so 1-3 keep the id and 4-6 move.
        assert_eq!(plan.moved_prototypes, vec![4, 5, 6]);
        assert_eq!(
            plan.moved_segments
                .iter()
                .map(|(id, _)| *id)
                .collect::<Vec<_>>(),
            vec![14, 15, 16]
        );
        assert!(plan.ambiguous_segments.is_empty());
        assert_eq!(plan.kept_vectors, 3);
        assert_eq!(plan.moved_vectors, 3);
        for (_, score) in &plan.moved_segments {
            assert!(
                *score > 0.9,
                "a moved segment scores against its own centroid"
            );
        }
    }

    #[test]
    fn the_larger_cluster_keeps_the_id() {
        let mut items = two_voices();
        items.truncate(5); // three on the first side, two on the second
        assert_eq!(plan(&cfg(), &items).unwrap().moved_prototypes, vec![4, 5]);

        // And the other way round, so it is the size that decides and not the
        // order.
        let mut items = two_voices();
        items.remove(0);
        assert_eq!(plan(&cfg(), &items).unwrap().moved_prototypes, vec![2, 3]);
    }

    #[test]
    fn the_same_bank_always_splits_the_same_way() {
        let items = two_voices();
        let first = plan(&cfg(), &items).unwrap();
        for _ in 0..8 {
            assert_eq!(plan(&cfg(), &items).unwrap(), first);
        }
        // And more restarts must not change the answer, only the confidence
        // that it is the best one.
        let more = IdentityConfig {
            split_restarts: 64,
            ..cfg()
        };
        assert_eq!(plan(&more, &items).unwrap(), first);
    }

    #[test]
    fn one_voice_is_refused_rather_than_cut_in_half() {
        // Six recordings of one person: k-means will happily return two
        // clusters, and both are the same voice.
        let items: Vec<Vector> = (0..6)
            .map(|i| v(i, 100 + i, &[1.0, 0.01 * i as f32, 0.0]))
            .collect();
        match plan(&cfg(), &items) {
            Err(Refusal::OneVoice { similarity }) => {
                assert!(similarity > 0.6, "scored {similarity:.3}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn the_refusal_threshold_is_the_config_value() {
        let items = two_voices();
        let strict = IdentityConfig {
            split_max_centroid_similarity: -1.0,
            ..cfg()
        };
        assert!(matches!(
            plan(&strict, &items),
            Err(Refusal::OneVoice { .. })
        ));
    }

    #[test]
    fn a_golden_pins_its_cluster_to_the_existing_speaker() {
        // The minority side is hand-enrolled, so it is the side that keeps the
        // id even though it is outnumbered.
        let mut items = two_voices();
        items[4] = golden(5, &[0.02, 1.0, 0.0]);
        let plan = plan(&cfg(), &items).unwrap();
        assert_eq!(
            plan.moved_prototypes,
            vec![1, 2, 3],
            "the golden's cluster keeps the identity"
        );
        assert!(
            !plan.moved_prototypes.contains(&5),
            "a golden never leaves its speaker"
        );
    }

    #[test]
    fn goldens_on_both_sides_are_conflicting_evidence() {
        let mut items = two_voices();
        items[0] = golden(1, &[1.0, 0.05, 0.0]);
        items[4] = golden(5, &[0.02, 1.0, 0.0]);
        assert_eq!(plan(&cfg(), &items), Err(Refusal::GoldenConflict));
    }

    #[test]
    fn a_segment_between_the_two_voices_keeps_the_old_speaker_at_a_lower_score() {
        let mut items = two_voices();
        // Halfway between the two centroids.
        items.push(v(7, 17, &[1.0, 1.0, 0.0]));
        // The default band is deliberately narrow — a segment is only
        // undecidable when the two readings are genuinely a coin toss — so this
        // one is decided by default and undecidable under a wider band.
        assert!(
            plan(&cfg(), &items).unwrap().ambiguous_segments.is_empty(),
            "the default band must not swallow a merely off-centre segment"
        );
        let banded = IdentityConfig {
            split_ambiguous_margin: 0.2,
            ..cfg()
        };
        let plan = plan(&banded, &items).unwrap();
        assert_eq!(
            plan.ambiguous_segments.len(),
            1,
            "the boundary segment is the undecidable one"
        );
        let (id, score) = plan.ambiguous_segments[0];
        assert_eq!(id, 17);
        assert!(
            !plan.moved_segments.iter().any(|(m, _)| *m == 17),
            "an undecidable segment must not be handed to the new voice"
        );
        assert!(
            !plan.moved_prototypes.contains(&7),
            "and its prototype stays with it"
        );
        // The weaker of the two readings: strictly below what a confident
        // segment on either side scores.
        assert!(score < 0.9, "ambiguous segment kept score {score:.3}");
        assert!(
            plan.moved_segments.iter().all(|(_, s)| *s > score),
            "an undecidable label must be less trusted than a decided one"
        );
    }

    #[test]
    fn a_wider_band_swallows_more_segments() {
        let items = two_voices();
        let wide = IdentityConfig {
            split_ambiguous_margin: 3.0,
            ..cfg()
        };
        // Everything is undecidable, so nothing would move and the split is
        // refused rather than minting an empty voice.
        assert_eq!(plan(&wide, &items), Err(Refusal::NothingToMove));
    }

    #[test]
    fn there_is_nothing_to_split_below_two_vectors() {
        assert_eq!(
            plan(&cfg(), &[]),
            Err(Refusal::NotEnoughVectors { have: 0 })
        );
        assert_eq!(
            plan(&cfg(), &[v(1, 11, &[1.0, 0.0])]),
            Err(Refusal::NotEnoughVectors { have: 1 })
        );
    }

    #[test]
    fn a_zero_vector_refuses_the_split_rather_than_dragging_a_centroid() {
        let items = vec![v(1, 11, &[1.0, 0.0]), v(2, 12, &[0.0, 0.0])];
        assert!(matches!(
            plan(&cfg(), &items),
            Err(Refusal::NotEnoughVectors { .. })
        ));
    }

    #[test]
    fn vectors_from_two_models_never_reach_the_clusterer() {
        let mut items = two_voices();
        items[3].embedding = Embedding::new("other@1", vec![0.0, 1.0, 0.05]);
        match plan(&cfg(), &items) {
            Err(r @ Refusal::Incomparable { .. }) => {
                assert_eq!(
                    r.code(),
                    "internal",
                    "a model leak is a fault, not a verdict"
                );
            }
            other => panic!("expected an incomparable refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_prototype_with_no_segment_still_moves() {
        // Its source segment was purged by retention; the vector is still
        // evidence about which voice it belongs to.
        let mut items = two_voices();
        items[5].segment_id = None;
        let plan = plan(&cfg(), &items).unwrap();
        assert!(plan.moved_prototypes.contains(&6));
        assert_eq!(plan.moved_segments.len(), 2);
    }

    #[test]
    fn every_refusal_says_something_a_person_can_act_on() {
        for r in [
            Refusal::NotEnoughVectors { have: 1 },
            Refusal::OneVoice { similarity: 0.9 },
            Refusal::GoldenConflict,
            Refusal::NothingToMove,
        ] {
            assert!(r.message().len() > 20, "{r:?} has no explanation");
            assert_eq!(r.code(), "refused");
        }
    }
}
