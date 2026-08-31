//! Turn merging: join VAD segments separated by a short silence into one turn.
//!
//! Step 0 measured this as free: joining regions separated by <=1.5 s took mean
//! length 2.4 s -> 4.1 s, distinct voices 62 -> 36, and matched regions 67.9% ->
//! 71.2%. It runs *before* embedding, so what the rest of the daemon calls a
//! segment is really a turn.
//!
//! The state machine is pure so it can be tested without audio.

use crate::vad::SegmentSpan;

pub struct TurnMerger {
    /// Maximum silence, in samples, that a turn may span.
    gap: u64,
    /// Hard cap on a turn's voiced extent. The VAD's own cap emits back-to-back
    /// spans with zero gap, which would otherwise re-merge into one unbounded
    /// turn.
    max_voiced: u64,
    pending: Option<SegmentSpan>,
}

impl TurnMerger {
    pub fn new(gap: u64, max_voiced: u64) -> Self {
        Self {
            gap,
            max_voiced,
            pending: None,
        }
    }

    /// Sample index the audio ring must still hold, if a turn is open.
    pub fn pending_start(&self) -> Option<u64> {
        self.pending.as_ref().map(|s| s.start)
    }

    /// Offer the next VAD span. Returns a completed turn if this span could not
    /// join the one in progress.
    pub fn push(&mut self, span: SegmentSpan) -> Option<SegmentSpan> {
        let Some(pending) = self.pending.take() else {
            self.pending = Some(span);
            return None;
        };

        let silence = span.voiced_start.saturating_sub(pending.voiced_end);
        let joined_len = span.voiced_end.saturating_sub(pending.voiced_start);
        if silence <= self.gap && joined_len <= self.max_voiced {
            self.pending = Some(SegmentSpan {
                start: pending.start,
                end: span.end,
                voiced_start: pending.voiced_start,
                voiced_end: span.voiced_end,
            });
            return None;
        }

        self.pending = Some(span);
        Some(pending)
    }

    /// Emit the open turn once `cursor` has advanced past the merge window, so
    /// a turn followed by nothing but silence is not held indefinitely.
    pub fn poll(&mut self, cursor: u64) -> Option<SegmentSpan> {
        let pending = self.pending.as_ref()?;
        if cursor.saturating_sub(pending.voiced_end) > self.gap {
            return self.pending.take();
        }
        None
    }

    pub fn flush(&mut self) -> Option<SegmentSpan> {
        self.pending.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u64 = 16_000;
    const GAP: u64 = 3 * RATE / 2; // 1.5 s
    const MAX: u64 = 30 * RATE;

    fn span(voiced_start: u64, voiced_end: u64) -> SegmentSpan {
        SegmentSpan {
            start: voiced_start.saturating_sub(3_200),
            end: voiced_end + 3_200,
            voiced_start,
            voiced_end,
        }
    }

    fn drain(spans: &[SegmentSpan]) -> Vec<SegmentSpan> {
        let mut m = TurnMerger::new(GAP, MAX);
        let mut out = Vec::new();
        for s in spans {
            out.extend(m.push(*s));
        }
        out.extend(m.flush());
        out
    }

    #[test]
    fn a_lone_segment_passes_through_unchanged() {
        let out = drain(&[span(0, RATE)]);
        assert_eq!(out, vec![span(0, RATE)]);
    }

    #[test]
    fn a_short_gap_joins_two_segments_into_one_turn() {
        // 1.0 s of silence between them.
        let out = drain(&[span(0, RATE), span(2 * RATE, 3 * RATE)]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].voiced_start, 0);
        assert_eq!(out[0].voiced_end, 3 * RATE);
        // The padded extent spans both, so the audio slice is contiguous.
        assert_eq!(out[0].start, 0);
        assert_eq!(out[0].end, 3 * RATE + 3_200);
    }

    #[test]
    fn a_gap_over_the_threshold_keeps_them_separate() {
        // 2.0 s of silence.
        let out = drain(&[span(0, RATE), span(3 * RATE, 4 * RATE)]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].voiced_end, RATE);
        assert_eq!(out[1].voiced_start, 3 * RATE);
    }

    #[test]
    fn the_gap_threshold_is_inclusive() {
        let exactly = drain(&[span(0, RATE), span(RATE + GAP, RATE + GAP + RATE)]);
        assert_eq!(exactly.len(), 1);
        let one_more = drain(&[span(0, RATE), span(RATE + GAP + 1, RATE + GAP + RATE)]);
        assert_eq!(one_more.len(), 2);
    }

    #[test]
    fn a_chain_of_short_gaps_becomes_one_turn() {
        let spans: Vec<_> = (0..5)
            .map(|i| span(i * 2 * RATE, i * 2 * RATE + RATE))
            .collect();
        let out = drain(&spans);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].voiced_end, 9 * RATE);
    }

    #[test]
    fn merging_stops_at_the_turn_cap() {
        // The VAD's own 30 s cap emits adjacent spans with zero gap; without the
        // cap here they would re-merge forever.
        let spans: Vec<_> = (0..4)
            .map(|i| span(i * 20 * RATE, (i + 1) * 20 * RATE))
            .collect();
        let out = drain(&spans);
        assert!(out.len() >= 2, "expected the cap to split, got {out:?}");
        for t in &out {
            assert!(t.voiced_end - t.voiced_start <= MAX);
        }
    }

    #[test]
    fn poll_releases_a_turn_once_the_merge_window_has_passed() {
        let mut m = TurnMerger::new(GAP, MAX);
        assert!(m.push(span(0, RATE)).is_none());
        // Still inside the window: hold it, a continuation may yet arrive.
        assert!(m.poll(RATE + GAP).is_none());
        let out = m.poll(RATE + GAP + 1).expect("turn must be released");
        assert_eq!(out.voiced_end, RATE);
        assert!(m.poll(u64::MAX).is_none(), "poll must not re-emit");
    }

    #[test]
    fn pending_start_pins_the_audio_ring_while_a_turn_is_open() {
        let mut m = TurnMerger::new(GAP, MAX);
        assert_eq!(m.pending_start(), None);
        m.push(span(10 * RATE, 11 * RATE));
        assert_eq!(m.pending_start(), Some(10 * RATE - 3_200));
        m.flush();
        assert_eq!(m.pending_start(), None);
    }

    #[test]
    fn flush_is_idempotent() {
        let mut m = TurnMerger::new(GAP, MAX);
        m.push(span(0, RATE));
        assert!(m.flush().is_some());
        assert!(m.flush().is_none());
    }
}
