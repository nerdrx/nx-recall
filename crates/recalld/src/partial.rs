//! Partial turns: provisional words on the glass while somebody is still
//! talking (0.11.0).
//!
//! # What this is, and what it deliberately is not
//!
//! Parakeet v3 is an **offline transducer**. There is no streaming API and this
//! module does not invent one: a partial is the ordinary recogniser run over
//! the WHOLE open turn so far, on the inference thread that was going to decode
//! that audio anyway. No second model, no second thread, no online zipformer —
//! sherpa has English-only ones and a second model is exactly the cost this
//! feature is not allowed to have.
//!
//! Decoding the whole turn rather than the newest chunk is the one decision
//! that makes the feature usable. FINDINGS §12 measured a 1.5 s slice at 56.7%
//! WER against 20.4% for the same slice inside a ±3 s window: short windows
//! decode badly, so a partial built from the last second alone would be noise
//! that never improves. Re-reading the whole turn every time means each partial
//! is a strictly better-informed reading than the last, and the text
//! **converges** on the final — which is also why a client may replace the
//! provisional row wholesale instead of appending to it.
//!
//! # Three rules, all of them about not costing a recording
//!
//! 1. **Cadence.** A partial is offered at most every `partial_every_ms`, and
//!    never before the turn has lasted `partial_min_ms`. Below that the words
//!    are wrong often enough to be worse than an empty bar.
//! 2. **Backlog.** If more than `partial_backlog_max_s` of audio is waiting for
//!    the inference thread, no partial is decoded at all. The queue drops the
//!    OLDEST audio when it overflows (`queue.rs`), so a partial that pushed the
//!    thread behind would be paying for a caption with a lost turn. It is the
//!    same rule and the same reason as `[graph].max_queue_seconds`.
//! 3. **Pause.** Nothing is decoded and nothing is published while capture is
//!    paused. The pipeline drops the session's whole state on the first paused
//!    buffer, which takes this state with it — but the emit path checks anyway,
//!    for the same reason `write_segment` re-checks: this is the panic path.
//!
//! # The speaker on a partial
//!
//! The identity ladder only ever labels FINISHED turns: it needs an embedding,
//! an embedding needs the overlap gate's approval, and both are decided over
//! the completed audio. A partial therefore carries **no embedding and no
//! match**. What it can carry is the cheapest true thing available — if the
//! previous turn in this session ended less than [`PROXIMITY_WINDOW_NS`] ago,
//! the same person is very probably still speaking, so the partial repeats that
//! turn's speaker and stamps `speaker_hint: "proximity"` so a client renders it
//! as the guess it is. Otherwise the speaker is `null` and so is the hint.
//!
//! Nothing here is ever written down: partials are not rows, not events in the
//! replay ring, and not part of `events.since`.

use serde_json::{Value, json};

/// How recently the previous turn must have ended for its speaker to be
/// borrowed as a partial's proximity hint.
///
/// Two seconds is `[vad].turn_merge_gap` (1.5 s) plus a little: past the merge
/// gap the daemon has already decided this is a *different* turn, and past two
/// seconds of silence in a lobby it is very often a different person.
pub const PROXIMITY_WINDOW_NS: i64 = 2_000_000_000;

/// The three numbers from `[asr]` that decide when a partial is offered.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cadence {
    /// At most one partial per this many milliseconds of wall clock.
    pub every_ms: u64,
    /// The open turn must have lasted at least this long.
    pub min_ms: u64,
    /// Seconds of audio waiting for the inference thread, above which partials
    /// stand down entirely.
    pub backlog_max_s: i64,
}

impl Cadence {
    pub fn from_config(cfg: &crate::config::AsrConfig) -> Self {
        Self {
            every_ms: cfg.partial_every_ms,
            min_ms: cfg.partial_min_ms,
            backlog_max_s: cfg.partial_backlog_max_s,
        }
    }
}

/// May this daemon consider a partial at all, right now?
///
/// The three answers that have nothing to do with which turn is open, in one
/// place so they can be checked without a model, a store or a socket. The
/// pipeline asks this first and asks it again after the decode, because the
/// decode is the one part of the path that takes real time and a pause landing
/// inside it must not be followed by words appearing on somebody's screen.
pub fn may_emit(enabled: bool, paused: bool, queued_samples: usize, rate: u32, max_s: i64) -> bool {
    enabled && !paused && !backlog_blocks(queued_samples, rate, max_s)
}

/// Is the inference thread too far behind to afford a partial?
///
/// Rounded the same way `enrich::gate` rounds it, so "5 seconds" means the same
/// thing in both places and a machine that trips one trips the other.
pub fn backlog_blocks(queued_samples: usize, rate: u32, max_s: i64) -> bool {
    let queued = (queued_samples as f64 / rate.max(1) as f64).round() as i64;
    queued > max_s
}

/// Per-session partial state: where the open turn is, how many partials it has
/// had, and who spoke last.
///
/// Pure — no clock, no audio, no store — so the cadence, the proximity rule and
/// the reset-on-pause behaviour are all testable without a model.
#[derive(Debug, Default)]
pub struct PartialState {
    /// The `t_start_ns` of the turn the last partial was about. A different
    /// value means a different turn, which restarts `seq_in_turn` at zero.
    open_t_start_ns: Option<i64>,
    seq_in_turn: u64,
    /// Wall clock of the last partial actually published, in milliseconds.
    last_emit_ms: Option<u64>,
    /// When the previous FINISHED turn in this session ended, and who the
    /// ladder said it was. `None` until one has finished.
    prev: Option<PrevTurn>,
}

#[derive(Debug, Clone, Copy)]
struct PrevTurn {
    end_ns: i64,
    speaker: Option<i64>,
}

impl PartialState {
    /// Whether a partial for this open turn is due.
    ///
    /// Read-only on purpose: the caller checks the backlog and the pause flag
    /// after this and before spending a decode, and a check that consumed the
    /// cadence would make a skipped partial silence the next one too.
    pub fn due(&self, t_start_ns: i64, elapsed_ms: u64, now_ms: u64, c: Cadence) -> bool {
        if elapsed_ms < c.min_ms {
            return false;
        }
        match (self.open_t_start_ns, self.last_emit_ms) {
            // A turn this state has not seen before: the first partial is due
            // the moment it is old enough, whatever the clock says about the
            // last one — that one belonged to a different turn.
            (Some(open), Some(last)) if open == t_start_ns => {
                now_ms.saturating_sub(last) >= c.every_ms
            }
            _ => true,
        }
    }

    /// Record that a partial went out, and hand back its `seq_in_turn`.
    pub fn mark(&mut self, t_start_ns: i64, now_ms: u64) -> u64 {
        if self.open_t_start_ns != Some(t_start_ns) {
            self.open_t_start_ns = Some(t_start_ns);
            self.seq_in_turn = 0;
        } else {
            self.seq_in_turn += 1;
        }
        self.last_emit_ms = Some(now_ms);
        self.seq_in_turn
    }

    /// The speaker a partial for a turn starting at `t_start_ns` may claim, and
    /// the hint that says how it got there.
    ///
    /// `("match", …)` is in the wire contract for a client to render and is
    /// never produced here: matching needs an embedding and an embedding needs
    /// a finished turn.
    pub fn hint(&self, t_start_ns: i64) -> (Option<i64>, Option<&'static str>) {
        match self.prev {
            Some(PrevTurn {
                end_ns,
                speaker: Some(id),
            }) if t_start_ns.saturating_sub(end_ns) < PROXIMITY_WINDOW_NS => {
                (Some(id), Some("proximity"))
            }
            _ => (None, None),
        }
    }

    /// A turn finished and was announced as an ordinary `segment`. The
    /// provisional row a client has been updating is replaced by matching
    /// `(session, t_start_ns)`; here, the open turn is closed and this turn's
    /// identity becomes the next one's proximity hint.
    pub fn turn_closed(&mut self, t_end_ns: i64, speaker: Option<i64>) {
        self.open_t_start_ns = None;
        self.seq_in_turn = 0;
        self.last_emit_ms = None;
        self.prev = Some(PrevTurn {
            end_ns: t_end_ns,
            speaker,
        });
    }

    /// Forget the open turn without claiming one finished.
    ///
    /// A pause, or an audio gap that discarded the turn in progress: in both
    /// cases the words a client is showing describe audio that is now gone, and
    /// the NEXT turn must not inherit a speaker from a turn that never landed.
    pub fn reset(&mut self) {
        self.open_t_start_ns = None;
        self.seq_in_turn = 0;
        self.last_emit_ms = None;
    }

    /// Whether a provisional row is outstanding — i.e. a client somewhere is
    /// showing words for a turn that has not been written yet.
    pub fn is_open(&self) -> bool {
        self.open_t_start_ns.is_some()
    }
}

/// One partial on the wire (docs/PROTOCOL.md "0.11.0 — partial turns").
///
/// Shaped like a cut-down `segment` on purpose: the same `session`, `source`,
/// `speaker` and `t_start_ns` fields with the same meanings, so a client folds
/// one in with the code it already has. What is absent is everything that is
/// only true of a finished turn — no `id` (there is no row), no `dur_ms`, no
/// `lang`, no `overlap_frac`, no `match_score`, no `translation`.
#[allow(clippy::too_many_arguments)]
pub fn partial_json(
    session_id: i64,
    source: Option<&str>,
    speaker: Option<i64>,
    speaker_hint: Option<&str>,
    t_start_ns: i64,
    elapsed_ms: u64,
    text: &str,
    seq_in_turn: u64,
) -> Value {
    json!({
        "session": session_id,
        "source": source,
        "speaker": speaker,
        "speaker_hint": speaker_hint,
        // Both time forms, as everywhere else: `t_start_ms` is what a client
        // renders, `t_start_ns` is the full-fidelity value AS A STRING, and it
        // is the half of the replace key that matters.
        "t_start_ms": crate::clock::ns_to_ms(t_start_ns),
        "t_start_ns": t_start_ns.to_string(),
        "elapsed_ms": elapsed_ms,
        "text": text,
        "seq_in_turn": seq_in_turn,
        // Never true on this event. Present, and constant, so a client can
        // switch on one field across both events rather than on which topic
        // handler it happens to be in.
        "final": false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const C: Cadence = Cadence {
        every_ms: 1000,
        min_ms: 800,
        backlog_max_s: 5,
    };

    #[test]
    fn nothing_is_offered_before_the_turn_is_old_enough() {
        let s = PartialState::default();
        assert!(!s.due(100, 0, 10_000, C));
        assert!(!s.due(100, 799, 10_000, C));
        assert!(s.due(100, 800, 10_000, C), "the floor is inclusive");
    }

    #[test]
    fn the_cadence_holds_between_partials_of_one_turn() {
        let mut s = PartialState::default();
        assert!(s.due(100, 900, 10_000, C));
        assert_eq!(s.mark(100, 10_000), 0);
        // Same turn, 999 ms later: not yet.
        assert!(!s.due(100, 1_900, 10_999, C));
        assert!(s.due(100, 1_900, 11_000, C));
        assert_eq!(s.mark(100, 11_000), 1, "seq counts within the turn");
        assert_eq!(s.mark(100, 12_000), 2);
    }

    #[test]
    fn a_new_turn_is_due_immediately_and_restarts_the_sequence() {
        let mut s = PartialState::default();
        s.mark(100, 10_000);
        s.mark(100, 11_000);
        // A different turn, a millisecond later: the cadence is per turn, not
        // per session, or the first words of every turn would be held back by
        // however recently the last one ended.
        assert!(s.due(200, 900, 11_001, C));
        assert_eq!(s.mark(200, 11_001), 0);
    }

    #[test]
    fn due_does_not_consume_the_cadence() {
        // The caller checks the backlog AFTER asking whether one is due. A
        // check that mutated would make a skipped partial silence the next.
        let mut s = PartialState::default();
        s.mark(100, 10_000);
        assert!(s.due(100, 2_000, 11_000, C));
        assert!(s.due(100, 2_000, 11_000, C));
        assert!(s.due(100, 2_000, 11_000, C));
    }

    #[test]
    fn pause_stops_partials_whatever_else_is_true() {
        // The panic path. Nothing is written down while capture is paused and
        // words on a caption bar are the most visible kind of "written down"
        // this daemon does.
        assert!(may_emit(true, false, 0, 16_000, 5));
        assert!(!may_emit(true, true, 0, 16_000, 5));
        // …and it is not conditional on anything else being in a good state.
        assert!(!may_emit(true, true, 99 * 16_000, 16_000, 5));
    }

    #[test]
    fn the_switch_being_off_is_the_first_thing_asked() {
        assert!(!may_emit(false, false, 0, 16_000, 5));
    }

    #[test]
    fn a_backlog_over_the_ceiling_stands_the_feature_down_at_the_gate() {
        assert!(may_emit(true, false, 5 * 16_000, 16_000, 5));
        assert!(!may_emit(true, false, 6 * 16_000, 16_000, 5));
    }

    #[test]
    fn a_backlog_over_the_ceiling_stands_the_feature_down() {
        let rate = 16_000;
        assert!(!backlog_blocks(0, rate, 5));
        assert!(
            !backlog_blocks(5 * 16_000, rate, 5),
            "exactly at is allowed"
        );
        assert!(backlog_blocks(6 * 16_000, rate, 5));
        // Rounded, like `enrich::gate`: 5.4 s is still five seconds.
        assert!(!backlog_blocks(5 * 16_000 + 6_000, rate, 5));
        assert!(backlog_blocks(5 * 16_000 + 9_000, rate, 5));
    }

    #[test]
    fn the_speaker_is_the_previous_turns_when_it_ended_a_moment_ago() {
        let mut s = PartialState::default();
        // Nothing has finished yet: nobody to borrow from.
        assert_eq!(s.hint(1_000), (None, None));

        s.turn_closed(1_000_000_000, Some(7));
        // 1.9 s later — still the same person, said as a guess.
        assert_eq!(s.hint(2_900_000_000), (Some(7), Some("proximity")));
        // 2.0 s later — the window is half-open and this is outside it.
        assert_eq!(s.hint(3_000_000_000), (None, None));
        assert_eq!(s.hint(9_000_000_000), (None, None));
    }

    #[test]
    fn an_unlabelled_previous_turn_lends_nothing() {
        let mut s = PartialState::default();
        s.turn_closed(1_000_000_000, None);
        assert_eq!(
            s.hint(1_500_000_000),
            (None, None),
            "a null speaker is not an identity to inherit"
        );
    }

    #[test]
    fn closing_a_turn_ends_the_provisional_row() {
        let mut s = PartialState::default();
        s.mark(100, 10_000);
        assert!(s.is_open());
        s.turn_closed(200, Some(3));
        assert!(!s.is_open());
        // …and the next turn starts its own sequence at zero.
        assert_eq!(s.mark(300, 11_000), 0);
    }

    #[test]
    fn a_reset_forgets_the_open_turn_and_keeps_the_last_identity() {
        // Pause, or a gap that discarded the turn in progress. The open turn is
        // gone; who spoke BEFORE it is still a fact about this session.
        let mut s = PartialState::default();
        s.turn_closed(1_000_000_000, Some(4));
        s.mark(1_500_000_000, 10_000);
        assert!(s.is_open());
        s.reset();
        assert!(!s.is_open());
        assert_eq!(s.hint(1_600_000_000), (Some(4), Some("proximity")));
        // And the cadence starts clean, so resuming does not wait a second.
        assert!(s.due(2_000_000_000, 900, 10_001, C));
    }

    #[test]
    fn the_wire_shape_carries_the_replace_key_and_says_it_is_not_final() {
        let v = partial_json(
            3,
            Some("VRChat.exe"),
            Some(7),
            Some("proximity"),
            1_700_000_000_123_456_789,
            2_400,
            "but in his hands",
            2,
        );
        assert_eq!(v["session"], 3);
        assert_eq!(v["source"], "VRChat.exe");
        assert_eq!(v["speaker"], 7);
        assert_eq!(v["speaker_hint"], "proximity");
        // The half of the replace key that has to survive JSON: a string.
        assert_eq!(v["t_start_ns"], "1700000000123456789");
        assert_eq!(v["t_start_ms"], 1_700_000_000_123i64);
        assert_eq!(v["elapsed_ms"], 2_400);
        assert_eq!(v["seq_in_turn"], 2);
        assert_eq!(v["final"], false);
        // Nothing that is only true of a finished turn.
        assert!(v.get("id").is_none());
        assert!(v.get("dur_ms").is_none());
        assert!(v.get("lang").is_none());

        // No previous turn: both fields null rather than absent, so a client
        // reading `speaker_hint` never has to distinguish the two.
        let v = partial_json(3, None, None, None, 1, 900, "hm", 0);
        assert!(v["speaker"].is_null());
        assert!(v["speaker_hint"].is_null());
        assert!(v["source"].is_null());
    }
}
