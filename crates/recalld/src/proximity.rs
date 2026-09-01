//! Proximity inheritance: giving a grunt the name of the turn it sits inside.
//!
//! The mint bar (`identity::mints`) stops a half-second "hm" from becoming a
//! permanent identity, which is right — but it leaves those rows nameless, and
//! most of them are not mysteries. A one-word interjection wedged between two
//! turns of the same confident voice, seconds either side, is that voice: the
//! embedder could not say so from a fragment that short, and the *clock* can.
//!
//! DESIGN §5 calls this a nicety rather than a mechanism ("1 s suffices for
//! labeling; proximity-inheritance for sub-second utterances is a nicety"), and
//! it is treated as exactly that:
//!
//! * It never mints, never enrols, and never writes a prototype. Nothing it
//!   decides can reach the voicebank, so a wrong inheritance costs one row's
//!   label and nothing permanent.
//! * `match_score` stays NULL and `label_via` says `proximity`, so a client can
//!   — and does — render it as uncertain. It is a guess with a good reason, not
//!   a measurement.
//! * **It never inherits from an inherited row.** One guess may not become the
//!   evidence for the next, or a single mislabelled turn would propagate down a
//!   whole conversation.
//!
//! The rule itself: at least one confident neighbour within `proximity_gap_s`,
//! and every confident neighbour inside that window naming the *same* voice.
//! One side is enough — a turn can be the first or last thing in a session —
//! but two that disagree are a handover, which is precisely when a grunt
//! belongs to nobody in particular.

use anyhow::Result;
use tracing::info;

use crate::config::IdentityConfig;
use crate::store::{NeighbourSegment, Store, label_via};

/// Is this segment a candidate at all: unlabelled, and short enough that the
/// mint bar is why it is unlabelled?
pub fn is_candidate(cfg: &IdentityConfig, seg: &NeighbourSegment) -> bool {
    seg.speaker_id.is_none() && seg.duration_s() < cfg.mint_min_duration_s
}

/// Can this neighbour's label be inherited?
///
/// Confident means measured: a match at or above the label threshold, or a
/// label that never needed a measurement (the microphone's pin, a person's own
/// decision). An inherited label is explicitly not confident — that is the
/// no-chaining rule, and it lives here so it cannot be forgotten at a call
/// site.
pub fn is_confident(cfg: &IdentityConfig, seg: &NeighbourSegment) -> bool {
    if seg.speaker_id.is_none() {
        return false;
    }
    match seg.label_via.as_deref() {
        Some(label_via::PROXIMITY) => false,
        Some(label_via::MIC) | Some(label_via::MANUAL) => true,
        // `match` and pre-v5 rows with no provenance: trust the score.
        _ => seg
            .match_score
            .is_none_or(|score| score >= cfg.label_threshold),
    }
}

/// The gap between two segments in seconds, zero when they touch or overlap.
fn gap_s(earlier: &NeighbourSegment, later: &NeighbourSegment) -> f32 {
    ((later.t_start_ns - earlier.t_end_ns).max(0)) as f32 / 1e9
}

/// Whose label `target` should inherit, if anyone's.
///
/// `before` and `after` are the nearest live segments either side of `target`
/// in the same session; either may be absent (the edge of a session), and
/// either may be unlabelled or too far away, in which case it simply does not
/// vote.
pub fn inherit(
    cfg: &IdentityConfig,
    target: &NeighbourSegment,
    before: Option<&NeighbourSegment>,
    after: Option<&NeighbourSegment>,
) -> Option<i64> {
    if !is_candidate(cfg, target) {
        return None;
    }
    let mut vote: Option<i64> = None;
    let mut voters = [
        before.map(|n| (n, gap_s(n, target))),
        after.map(|n| (n, gap_s(target, n))),
    ]
    .into_iter()
    .flatten()
    .filter(|(n, gap)| *gap <= cfg.proximity_gap_s && is_confident(cfg, n))
    .peekable();

    voters.peek()?;
    for (neighbour, _) in voters {
        let id = neighbour.speaker_id?;
        match vote {
            None => vote = Some(id),
            // Two confident neighbours naming different people: this is a
            // handover, and the turn between them belongs to neither.
            Some(seen) if seen != id => return None,
            Some(_) => {}
        }
    }
    vote
}

/// Apply the rule now that `just_written` exists.
///
/// The trigger is deliberately the *arrival of the next turn*, not the arrival
/// of the grunt itself: a segment's right-hand neighbour does not exist yet
/// when it is analysed, and a rule that only ever looked left would inherit
/// from a handover half the time. So each stored turn asks one question about
/// the one before it, which is O(1) per segment and needs no backlog pass.
///
/// Returns the segment that was relabelled, so the caller can announce it.
pub fn apply(store: &Store, cfg: &IdentityConfig, just_written: i64) -> Result<Option<i64>> {
    let Some(after) = store.neighbour_segment(just_written)? else {
        return Ok(None);
    };
    let Some(target) = store.segments_before(just_written, 1)?.into_iter().next() else {
        return Ok(None);
    };
    if !is_candidate(cfg, &target) {
        return Ok(None);
    }
    let before = store.segments_before(target.id, 1)?.into_iter().next();
    let Some(speaker_id) = inherit(cfg, &target, before.as_ref(), Some(&after)) else {
        return Ok(None);
    };
    // No score: nothing was compared. The provenance is what a client reads to
    // know this label is a good reason rather than a measurement.
    store.set_segment_speaker_via(
        target.id,
        Some(speaker_id),
        None,
        Some(label_via::PROXIMITY),
    )?;
    info!(
        segment_id = target.id,
        speaker = speaker_id,
        duration_s = target.duration_s(),
        "inherited a speaker from the turns around it"
    );
    Ok(Some(target.id))
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: i64 = 1_000_000_000;

    fn cfg() -> IdentityConfig {
        IdentityConfig::default()
    }

    /// A segment from `start` to `end` seconds, labelled or not.
    fn seg(id: i64, start: f32, end: f32, speaker: Option<i64>) -> NeighbourSegment {
        NeighbourSegment {
            id,
            t_start_ns: (start * S as f32) as i64,
            t_end_ns: (end * S as f32) as i64,
            speaker_id: speaker,
            match_score: speaker.map(|_| 0.8),
            label_via: speaker.map(|_| "match".to_string()),
        }
    }

    fn inherited(mut s: NeighbourSegment) -> NeighbourSegment {
        s.match_score = None;
        s.label_via = Some(label_via::PROXIMITY.to_string());
        s
    }

    #[test]
    fn a_grunt_between_two_turns_of_one_voice_inherits_it() {
        let before = seg(1, 0.0, 4.0, Some(7));
        let target = seg(2, 5.0, 5.6, None);
        let after = seg(3, 6.5, 10.0, Some(7));
        assert_eq!(
            inherit(&cfg(), &target, Some(&before), Some(&after)),
            Some(7)
        );
    }

    #[test]
    fn one_side_is_enough_because_a_session_has_edges() {
        let target = seg(2, 5.0, 5.6, None);
        let after = seg(3, 6.0, 10.0, Some(7));
        assert_eq!(inherit(&cfg(), &target, None, Some(&after)), Some(7));
        let before = seg(1, 0.0, 4.0, Some(9));
        assert_eq!(inherit(&cfg(), &target, Some(&before), None), Some(9));
    }

    #[test]
    fn two_confident_neighbours_that_disagree_are_a_handover() {
        let before = seg(1, 0.0, 4.0, Some(7));
        let target = seg(2, 5.0, 5.6, None);
        let after = seg(3, 6.0, 10.0, Some(8));
        assert_eq!(inherit(&cfg(), &target, Some(&before), Some(&after)), None);
    }

    #[test]
    fn a_neighbour_past_the_gap_does_not_vote() {
        let target = seg(2, 10.0, 10.6, None);
        // 3 s of silence before it, past the 2.5 s window.
        let before = seg(1, 0.0, 7.0, Some(7));
        assert_eq!(inherit(&cfg(), &target, Some(&before), None), None);
        // …and a disagreeing neighbour that is too far away does not veto
        // either: it is not in the conversation any more.
        let after = seg(3, 12.0, 15.0, Some(9));
        assert_eq!(
            inherit(&cfg(), &target, Some(&before), Some(&after)),
            Some(9)
        );
    }

    #[test]
    fn nothing_is_inherited_from_an_inherited_row() {
        let before = inherited(seg(1, 0.0, 4.0, Some(7)));
        let target = seg(2, 5.0, 5.6, None);
        assert_eq!(inherit(&cfg(), &target, Some(&before), None), None);
        // One guess may not become the evidence for the next — even when the
        // other side agrees, the inherited row simply does not vote.
        let after = seg(3, 6.0, 10.0, Some(7));
        assert_eq!(
            inherit(&cfg(), &target, Some(&before), Some(&after)),
            Some(7)
        );
    }

    #[test]
    fn a_weakly_matched_neighbour_is_not_confident() {
        let mut before = seg(1, 0.0, 4.0, Some(7));
        before.match_score = Some(cfg().label_threshold - 0.01);
        let target = seg(2, 5.0, 5.6, None);
        assert_eq!(inherit(&cfg(), &target, Some(&before), None), None);
    }

    #[test]
    fn the_microphones_pin_is_confident_without_a_score() {
        let mut before = seg(1, 0.0, 4.0, Some(3));
        before.match_score = None;
        before.label_via = Some(label_via::MIC.to_string());
        let target = seg(2, 5.0, 5.6, None);
        assert_eq!(inherit(&cfg(), &target, Some(&before), None), Some(3));
    }

    #[test]
    fn a_long_or_already_labelled_turn_is_not_a_candidate() {
        let before = seg(1, 0.0, 4.0, Some(7));
        // Long enough that the mint bar would have let it mint: it is a turn,
        // not a grunt, and being unlabelled means the voicebank had its say.
        let long = seg(2, 5.0, 8.0, None);
        assert_eq!(inherit(&cfg(), &long, Some(&before), None), None);
        // Already labelled: nothing to inherit.
        let labelled = seg(3, 5.0, 5.6, Some(2));
        assert_eq!(inherit(&cfg(), &labelled, Some(&before), None), None);
    }

    // ---- against a real database ----------------------------------------

    /// `apply` on a store, which is where the rule's *timing* lives: the
    /// question is asked when the turn AFTER the grunt arrives, because that is
    /// the first moment both sides exist.
    #[test]
    fn the_rule_runs_when_the_following_turn_arrives() {
        let s = Store::open_in_memory().unwrap();
        let src = s.upsert_source("VRChat.exe", "VRChat.exe", 0).unwrap();
        let sess = s.begin_session(src, 0).unwrap();
        let spk = s.create_speaker("Kira", 0).unwrap();
        let sec = 1_000_000_000i64;
        let add = |from: i64, to: i64| s.insert_segment(sess, from, to, "x.wav", 0).unwrap();

        let first = add(0, 4 * sec);
        s.set_segment_speaker(first, Some(spk), Some(0.8)).unwrap();
        let grunt = add(5 * sec, 5 * sec + 600_000_000);

        // Nothing yet: the grunt is the newest row, so there is no "after".
        assert_eq!(apply(&s, &cfg(), grunt).unwrap(), None);
        assert_eq!(s.segment_fields(grunt).unwrap()["speaker_id"], None);

        // The next turn arrives and settles it.
        let third = add(7 * sec, 11 * sec);
        s.set_segment_speaker(third, Some(spk), Some(0.9)).unwrap();
        assert_eq!(apply(&s, &cfg(), third).unwrap(), Some(grunt));

        let f = s.segment_fields(grunt).unwrap();
        assert_eq!(f["speaker_id"].as_deref(), Some(spk.to_string().as_str()));
        assert_eq!(
            f["match_score"], None,
            "nothing was compared, so there is no score to report"
        );
        assert_eq!(f["label_via"].as_deref(), Some(label_via::PROXIMITY));

        // Running it again changes nothing: the grunt is labelled now, so it is
        // no longer a candidate.
        assert_eq!(apply(&s, &cfg(), third).unwrap(), None);
    }

    #[test]
    fn an_inherited_label_never_seeds_another_one() {
        let s = Store::open_in_memory().unwrap();
        let src = s.upsert_source("VRChat.exe", "VRChat.exe", 0).unwrap();
        let sess = s.begin_session(src, 0).unwrap();
        let spk = s.create_speaker("Kira", 0).unwrap();
        let sec = 1_000_000_000i64;
        let add = |from: i64, to: i64| s.insert_segment(sess, from, to, "x.wav", 0).unwrap();

        // A confident turn, then two grunts back to back.
        let anchor = add(0, 4 * sec);
        s.set_segment_speaker(anchor, Some(spk), Some(0.8)).unwrap();
        let grunt_a = add(4 * sec + 500_000_000, 5 * sec);
        let grunt_b = add(5 * sec + 200_000_000, 5 * sec + 700_000_000);

        // The first grunt inherits from the confident turn beside it.
        assert_eq!(apply(&s, &cfg(), grunt_b).unwrap(), Some(grunt_a));
        assert_eq!(
            s.segment_fields(grunt_a).unwrap()["label_via"].as_deref(),
            Some(label_via::PROXIMITY)
        );

        // The second one's only near neighbour is that inherited row, and the
        // next real turn is too far away to vote. It stays nameless rather than
        // taking a guess built on a guess.
        let far = add(9 * sec, 12 * sec);
        s.set_segment_speaker(far, Some(spk), Some(0.9)).unwrap();
        assert_eq!(apply(&s, &cfg(), far).unwrap(), None);
        assert_eq!(s.segment_fields(grunt_b).unwrap()["speaker_id"], None);
    }

    #[test]
    fn overlapping_neighbours_have_no_negative_gap() {
        let mut before = seg(1, 0.0, 6.0, Some(7));
        before.t_end_ns = 6 * S; // ends after the target starts
        let target = seg(2, 5.0, 5.6, None);
        assert_eq!(inherit(&cfg(), &target, Some(&before), None), Some(7));
    }
}
