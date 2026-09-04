//! Sliced turns: a long turn reaches the glass in pieces, and lands as one row
//! (0.12.4).
//!
//! # The problem, and why partials are not the answer to it
//!
//! A turn is not published until it is OVER, and "over" is expensive: the VAD
//! wants `min_silence` (500 ms) of quiet before it closes a span, and the turn
//! merger then waits out `turn_merge_gap` (1.5 s) to see whether the person
//! carries on. Two of the 3.32 s median that FINDINGS §20 measured from a word
//! being spoken to being readable is the daemon deciding the sentence is done —
//! and for a turn that runs twenty seconds, the first word waits for the last.
//!
//! [`crate::partial`] answered this in 0.11.0 by re-decoding the whole open
//! turn once a second. It converges beautifully (96.4% of the last partial's
//! words survive into the final) and it is off by default, because N partials
//! over an N-second turn is N decodes of average length N/2 — **O(N²)**, which
//! measured at +553% to +702% CPU against a 20% gate.
//!
//! Slicing is the **O(N)** form of the same idea, and the difference is one
//! word: *disjoint*. Each slice is decoded once, over its own audio and nobody
//! else's. Six slices of a thirty-second turn decode thirty seconds of audio,
//! exactly as the one whole-turn decode would have. What the feature costs is a
//! fixed per-decode overhead times the extra pieces; what it buys is the first
//! piece being readable `slice_after_s` into the turn rather than at the end.
//!
//! # Never mid-word, and how that is guaranteed rather than hoped
//!
//! The one thing a slice boundary must never do is cut through a word: half a
//! word decoded alone is not a worse reading of that word, it is a different
//! word, and it would be written down. So the cut is not made at a time — it is
//! made at a **dip the VAD has already scored as not-speech**.
//!
//! `slice_after_s` is therefore a *floor*, not a period. Past the floor this
//! state machine waits for [`Segmenter::dip_len`](crate::vad::Segmenter::dip_len)
//! to reach [`DIP_SAMPLES`], and cuts at the last voiced sample before it. A
//! turn with no pause in it is never sliced; it is ended by the VAD's own 30 s
//! cap, exactly as it was before this module existed.
//!
//! # What this module is not
//!
//! It has no clock, no audio, no store and no model, and it never decides
//! anything about *text*. It answers one question — "given where the open turn
//! started and what the segmenter is seeing right now, is there a cut, and
//! where?" — so the rule can be tested without a recogniser.

/// How long a dip has to run before it is a place to cut.
///
/// A constant rather than a knob, because it is not a preference: it is the
/// shortest silence that is reliably *between* words rather than inside one.
/// 120 ms is under the VAD's own `min_silence` (500 ms, which is where it
/// decides a whole span has ENDED) and over the 32 ms frame, so a cut lands on
/// a real inter-phrase gap and a turn that pauses for breath is sliceable
/// without waiting for the pause that would have closed it anyway.
///
/// Below about three frames the dip is as likely to be a stop consonant as a
/// gap, and cutting inside "back" between the vowel and the /k/ is exactly the
/// mid-word cut this whole design exists to refuse.
pub const DIP_MS: u64 = 120;

/// [`DIP_MS`] at 16 kHz, which is the only rate this daemon runs at.
pub const DIP_SAMPLES: u64 = DIP_MS * crate::config::SAMPLE_RATE as u64 / 1000;

/// Where a turn was cut, in absolute sample indices.
///
/// Half-open `[start, end)` like [`crate::vad::SegmentSpan`], and deliberately
/// NOT that type: a span is something the VAD found, and this is something the
/// slicer decided about a turn that is still open. Conflating them is how a
/// slice would end up in the turn merger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cut {
    /// First sample of this slice. The turn's own start for slice 0, and the
    /// previous cut's `end` after that — so the slices of a turn are a
    /// partition of it with no gap and no overlap.
    pub start: u64,
    /// One past the last sample. Always a sample the VAD scored as not-speech,
    /// which is the whole guarantee this module offers.
    pub end: u64,
    /// 0 for the first slice of a turn, counting up. On the wire so a client
    /// can tell a row that has grown once from one that has grown five times.
    pub seq: u64,
}

impl Cut {
    pub fn samples(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }
}

/// Per-session slicing state: where the open turn's un-sliced remainder begins,
/// and how many slices it has already yielded.
///
/// One instance per session for the same reason [`crate::partial::PartialState`]
/// is: two sources are two conversations, and a cut in one says nothing about
/// the other.
#[derive(Debug, Default)]
pub struct Slicer {
    /// The turn this state is about, identified by the sample its FIRST slice
    /// started at. A different value is a different turn and resets everything.
    turn_start: Option<u64>,
    /// Where the next slice begins — the turn's start, then each cut's end.
    ///
    /// Advances on EVERY cut, including one the decoder made nothing of. It has
    /// to: the floor is measured from here, and a cut that did not advance it
    /// would be offered again on the very next frame, and the one after that,
    /// for as long as the dip lasted.
    next_start: u64,
    /// Where audio nobody has successfully READ begins.
    ///
    /// Usually the same as `next_start`, and deliberately a second number for
    /// the case where they differ: a slice whose decode came back empty. That
    /// audio has been *looked at* and must not stop the cut cursor, but it has
    /// not been *read* — nothing from it is on the row — so it stays in front
    /// of the remainder and is decoded again with the words that follow it.
    ///
    /// Without the split, six seconds of speech the decoder happened to make
    /// nothing of in isolation would be silently deleted from the transcript.
    /// That is the one failure mode of this feature that loses a recording, and
    /// it is worth re-reading a rare slice to refuse it.
    read_from: u64,
    /// How many slices this turn has already produced.
    seq: u64,
}

impl Slicer {
    /// Is there a cut to make right now?
    ///
    /// * `turn_start` — where the open turn begins (`open_turn_start`).
    /// * `cursor` — the VAD's cursor. Never `received`: audio past the cursor
    ///   has not been scored, so cutting into it would put words on screen for
    ///   speech the daemon has not yet decided is speech.
    /// * `dip_len` — samples since the last voiced frame, or `None` when the
    ///   segmenter is not in speech at all.
    /// * `after_samples` — `[captions] slice_after_s` in samples. Zero is off
    ///   and is checked by the caller, but checked here too: this is the
    ///   function that decides, and a floor of zero would slice at every dip.
    ///
    /// Read-only. The caller spends a decode between asking and committing, and
    /// a query that consumed the state would lose a slice whenever that decode
    /// was skipped — the same reason [`crate::partial::PartialState::due`] does
    /// not mutate.
    pub fn due(
        &self,
        turn_start: u64,
        cursor: u64,
        dip_len: Option<u64>,
        after_samples: u64,
    ) -> Option<Cut> {
        if after_samples == 0 {
            return None;
        }
        // A turn this state has not seen yet slices from its own beginning; one
        // it has slices from where the last cut left off.
        let start = match self.turn_start {
            Some(t) if t == turn_start => self.next_start,
            _ => turn_start,
        };
        // The floor is on THIS slice, not on the turn: after a cut the clock
        // starts again, so a two-minute monologue yields a slice every
        // `slice_after_s` rather than one enormous first slice and then a
        // stream of tiny ones.
        if cursor.saturating_sub(start) < after_samples {
            return None;
        }
        // Past the floor, the only thing that makes a cut is a dip long enough
        // to be between words. `None` is the segmenter not being in speech at
        // all, which means the span is closing on its own and the ordinary turn
        // path is about to publish it — there is nothing for a slice to beat.
        let dip = dip_len?;
        if dip < DIP_SAMPLES {
            return None;
        }
        // Cut at the START of the dip: the trailing silence belongs to whatever
        // comes next, and including it would hand the decoder a slice that ends
        // in nothing.
        let end = cursor.saturating_sub(dip);
        // The dip began before this slice did — the floor was reached during a
        // silence that started earlier. There is no speech in the cut at all,
        // so there is nothing to say.
        if end <= start {
            return None;
        }
        Some(Cut {
            start,
            end,
            seq: match self.turn_start {
                Some(t) if t == turn_start => self.seq,
                _ => 0,
            },
        })
    }

    /// Record that a cut was made.
    ///
    /// `read` is whether the decoder actually made words of it. A cut is marked
    /// either way — see `next_start` — but only a cut that was READ moves the
    /// boundary of what the final decode still has to cover. An empty slice's
    /// audio goes back in front of the remainder, so it is read again with the
    /// words that follow it rather than being dropped from the row.
    pub fn mark(&mut self, turn_start: u64, cut: Cut, read: bool) {
        if self.turn_start != Some(turn_start) {
            self.turn_start = Some(turn_start);
            self.seq = 0;
            self.read_from = cut.start;
        }
        self.next_start = cut.end;
        if read {
            self.read_from = cut.end;
        }
        self.seq += 1;
    }

    /// How many slices the open turn has produced, or 0 when it has none.
    ///
    /// The caller reads this to know whether a finished turn has a row waiting
    /// to be closed or is an ordinary unsliced turn to be written from scratch.
    pub fn slices(&self, turn_start: u64) -> u64 {
        match self.turn_start {
            Some(t) if t == turn_start => self.seq,
            _ => 0,
        }
    }

    /// Where the un-published remainder of the open turn begins, if it has been
    /// sliced at all.
    ///
    /// Everything before it is already on the growing row; everything after it
    /// is what the final decode still has to read. Not `next_start`: a slice
    /// the decoder made nothing of advanced the cut cursor but put no words
    /// anywhere, so its audio belongs to the remainder.
    pub fn pending_start(&self, turn_start: u64) -> Option<u64> {
        (self.slices(turn_start) > 0).then_some(self.read_from)
    }

    /// Forget the open turn.
    ///
    /// A pause, a gap that discarded the audio, or the turn simply finishing.
    /// Called on all three for the same reason [`crate::partial::PartialState`]
    /// resets on all three: the next turn must not inherit a cut position from
    /// a turn that is no longer being spoken.
    pub fn reset(&mut self) {
        self.turn_start = None;
        self.next_start = 0;
        self.read_from = 0;
        self.seq = 0;
    }

    /// Whether a row is open — i.e. some client is showing a partly-written
    /// turn that has not been closed yet.
    pub fn is_open(&self) -> bool {
        self.turn_start.is_some() && self.seq > 0
    }
}

/// One slice on the wire (docs/PROTOCOL.md "0.12.4 — sliced turns").
///
/// Shaped like a [`crate::partial`] on purpose, down to the field names, so a
/// client that already renders a provisional row folds this in with the code it
/// has. Two fields are new and they carry the one difference that matters:
///
/// * `text` is **this slice only** — the newest words and nothing else.
/// * `text_so_far` is every slice of this turn joined, which is what a client
///   actually draws.
///
/// Both, rather than either, because the two events differ in exactly this
/// respect and a client must not have to guess which it is holding. A partial
/// *replaces* the provisional row (each one is a better reading of the same
/// audio); a slice *extends* it (each one is a reading of new audio). A client
/// that renders `text_so_far` is correct without knowing that; one that wants
/// to animate the newest words has `text` without having to diff for them.
///
/// Like a partial, nothing here is written down: a slice is not a row, not an
/// entry in the replay ring, and not part of `events.since`. The row arrives
/// when the turn ends, once, carrying these words joined to the remainder.
#[allow(clippy::too_many_arguments)]
pub fn slice_json(
    session_id: i64,
    source: Option<&str>,
    speaker: Option<i64>,
    speaker_hint: Option<&str>,
    t_start_ns: i64,
    elapsed_ms: u64,
    text: &str,
    text_so_far: &str,
    seq: u64,
) -> serde_json::Value {
    serde_json::json!({
        "session": session_id,
        "source": source,
        "speaker": speaker,
        "speaker_hint": speaker_hint,
        // Both time forms, as everywhere else, and `t_start_ns` is a STRING —
        // it is the half of the replace key that has to survive JSON.
        "t_start_ms": crate::clock::ns_to_ms(t_start_ns),
        "t_start_ns": t_start_ns.to_string(),
        "elapsed_ms": elapsed_ms,
        "text": text,
        "text_so_far": text_so_far,
        "seq": seq,
        // Never true on this event. Present and constant so a client can switch
        // on one field across `partial`, `slice` and `segment` rather than on
        // which topic handler it happens to be in.
        "final": false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u64 = crate::config::SAMPLE_RATE as u64;
    /// Six seconds, the shipped floor.
    const AFTER: u64 = 6 * RATE;

    #[test]
    fn nothing_is_cut_before_the_floor() {
        let s = Slicer::default();
        // Deep in a dip, but the turn is only five seconds old.
        assert_eq!(s.due(0, 5 * RATE, Some(RATE), AFTER), None);
        // …and at exactly the floor, with a dip, it cuts. The floor is
        // inclusive, like every other threshold in this daemon.
        assert!(s.due(0, 6 * RATE, Some(DIP_SAMPLES), AFTER).is_some());
    }

    #[test]
    fn past_the_floor_it_waits_for_a_dip_rather_than_cutting_on_time() {
        // This is the whole safety property: `slice_after_s` is a floor, and a
        // person who does not pause is not cut.
        let s = Slicer::default();
        for secs in [6, 10, 20, 29] {
            assert_eq!(
                s.due(0, secs * RATE, Some(0), AFTER),
                None,
                "cut mid-word at {secs} s with no dip"
            );
        }
        // A dip a frame short of the threshold is still not a place to cut.
        assert_eq!(s.due(0, 20 * RATE, Some(DIP_SAMPLES - 1), AFTER), None);
        assert!(s.due(0, 20 * RATE, Some(DIP_SAMPLES), AFTER).is_some());
    }

    #[test]
    fn the_cut_lands_at_the_start_of_the_dip_and_not_at_the_cursor() {
        // The trailing silence belongs to the next slice; a slice that ended in
        // 400 ms of nothing would hand the decoder a worse window for no gain.
        let s = Slicer::default();
        let dip = 4 * DIP_SAMPLES;
        let cut = s.due(0, 10 * RATE, Some(dip), AFTER).unwrap();
        assert_eq!(cut.start, 0);
        assert_eq!(cut.end, 10 * RATE - dip);
        assert_eq!(cut.seq, 0);
    }

    #[test]
    fn a_turn_start_that_is_not_zero_is_where_the_first_slice_begins() {
        let s = Slicer::default();
        let start = 100 * RATE;
        assert_eq!(s.due(start, start + 5 * RATE, Some(RATE), AFTER), None);
        let cut = s
            .due(start, start + 7 * RATE, Some(DIP_SAMPLES), AFTER)
            .unwrap();
        assert_eq!(cut.start, start);
    }

    #[test]
    fn the_slices_of_a_turn_partition_it_with_no_gap_and_no_overlap() {
        // The property the join depends on: concatenating the slices' audio
        // must reproduce the turn, or the row's WAV and its text would describe
        // different speech.
        let mut s = Slicer::default();
        let mut cuts = Vec::new();
        let mut cursor = 0u64;
        for _ in 0..4 {
            cursor += 7 * RATE;
            let cut = s.due(0, cursor, Some(DIP_SAMPLES), AFTER).expect("a cut");
            s.mark(0, cut, true);
            cuts.push(cut);
        }
        assert_eq!(cuts[0].start, 0);
        for w in cuts.windows(2) {
            assert_eq!(w[0].end, w[1].start, "slices must not gap or overlap");
        }
        assert_eq!(
            cuts.iter().map(|c| c.seq).collect::<Vec<_>>(),
            vec![0, 1, 2, 3],
            "the sequence counts within the turn"
        );
    }

    #[test]
    fn the_floor_applies_to_each_slice_and_not_to_the_turn() {
        // Otherwise a long monologue would yield one six-second slice and then
        // a cut at every dip for the rest of the evening.
        let mut s = Slicer::default();
        let first = s.due(0, 6 * RATE, Some(DIP_SAMPLES), AFTER).unwrap();
        s.mark(0, first, true);
        // One second past the first cut: nowhere near the floor again.
        let cursor = first.end + RATE;
        assert_eq!(s.due(0, cursor, Some(DIP_SAMPLES), AFTER), None);
        // Six seconds past it: due.
        let cursor = first.end + 6 * RATE + DIP_SAMPLES;
        let second = s.due(0, cursor, Some(DIP_SAMPLES), AFTER).unwrap();
        assert_eq!(second.start, first.end);
    }

    #[test]
    fn a_different_turn_starts_its_own_sequence_from_its_own_beginning() {
        let mut s = Slicer::default();
        let cut = s.due(0, 8 * RATE, Some(DIP_SAMPLES), AFTER).unwrap();
        s.mark(0, cut, true);
        // A new turn, starting well after the old one's last cut.
        let start = 50 * RATE;
        let next = s
            .due(start, start + 7 * RATE, Some(DIP_SAMPLES), AFTER)
            .unwrap();
        assert_eq!(next.start, start, "a new turn does not resume the old cut");
        assert_eq!(next.seq, 0);
    }

    #[test]
    fn a_floor_of_zero_never_cuts_however_long_the_dip() {
        // `slice_after_s = 0` is the off switch and it is checked here as well
        // as at the call site, because this is the function that decides.
        let s = Slicer::default();
        assert_eq!(s.due(0, 60 * RATE, Some(10 * RATE), 0), None);
    }

    #[test]
    fn silence_alone_is_not_a_slice() {
        // The floor was reached during a dip that began before the slice did:
        // there is no speech between `start` and the cut, so there is nothing
        // to publish and cutting would mint an empty row.
        let mut s = Slicer::default();
        let first = s.due(0, 6 * RATE, Some(DIP_SAMPLES), AFTER).unwrap();
        s.mark(0, first, true);
        // Cursor is 7 s past the cut and ALL of it has been dip.
        let cursor = first.end + 7 * RATE;
        assert_eq!(s.due(0, cursor, Some(7 * RATE), AFTER), None);
    }

    #[test]
    fn not_being_in_speech_is_never_a_cut() {
        // `None` means the segmenter has closed the span: the ordinary turn
        // path is about to publish it and there is nothing for a slice to beat.
        let s = Slicer::default();
        assert_eq!(s.due(0, 30 * RATE, None, AFTER), None);
    }

    #[test]
    fn a_slice_the_decoder_made_nothing_of_is_not_deleted_from_the_turn() {
        // The one failure mode of this feature that loses a recording. The cut
        // still advances — otherwise it would be re-offered every frame for as
        // long as the dip lasted — but the audio stays in front of the
        // remainder, so the final decode reads it again with the words after it
        // rather than dropping six seconds of speech on the floor.
        let mut s = Slicer::default();
        let first = s.due(0, 6 * RATE, Some(DIP_SAMPLES), AFTER).unwrap();
        s.mark(0, first, false);
        assert_eq!(
            s.pending_start(0),
            Some(first.start),
            "an unread slice's audio was dropped from the remainder"
        );
        // …and the cut cursor DID move, so the next slice is a new one.
        let second = s
            .due(
                0,
                first.end + 6 * RATE + DIP_SAMPLES,
                Some(DIP_SAMPLES),
                AFTER,
            )
            .unwrap();
        assert_eq!(second.start, first.end, "the cut cursor did not advance");

        // A slice that WAS read moves the boundary, and the unread audio before
        // it is covered by that same reading.
        s.mark(0, second, true);
        assert_eq!(s.pending_start(0), Some(second.end));
    }

    #[test]
    fn a_run_of_empty_slices_keeps_every_one_of_them_for_the_remainder() {
        let mut s = Slicer::default();
        let mut cursor = 0u64;
        let mut first_start = None;
        for _ in 0..3 {
            cursor += 7 * RATE;
            let cut = s.due(0, cursor, Some(DIP_SAMPLES), AFTER).unwrap();
            first_start.get_or_insert(cut.start);
            s.mark(0, cut, false);
        }
        assert_eq!(
            s.pending_start(0),
            first_start,
            "the remainder must cover every slice nobody could read"
        );
    }

    #[test]
    fn a_reset_forgets_the_open_row() {
        let mut s = Slicer::default();
        assert!(!s.is_open());
        let cut = s.due(0, 8 * RATE, Some(DIP_SAMPLES), AFTER).unwrap();
        s.mark(0, cut, true);
        assert!(s.is_open());
        assert_eq!(s.slices(0), 1);
        assert_eq!(s.pending_start(0), Some(cut.end));

        s.reset();
        assert!(!s.is_open());
        assert_eq!(s.slices(0), 0);
        assert_eq!(s.pending_start(0), None, "nothing is pinned any more");
        // …and the next turn slices from its own start.
        let next = s.due(0, 8 * RATE, Some(DIP_SAMPLES), AFTER).unwrap();
        assert_eq!(next.start, 0);
        assert_eq!(next.seq, 0);
    }

    #[test]
    fn slices_and_pending_start_are_about_the_turn_they_are_asked_about() {
        // A stale answer here would make an unsliced turn adopt the previous
        // turn's row, which is the worst bug this module could have.
        let mut s = Slicer::default();
        let cut = s.due(0, 8 * RATE, Some(DIP_SAMPLES), AFTER).unwrap();
        s.mark(0, cut, true);
        assert_eq!(s.slices(0), 1);
        assert_eq!(s.slices(99 * RATE), 0);
        assert_eq!(s.pending_start(99 * RATE), None);
    }

    #[test]
    fn the_dip_threshold_is_over_a_frame_and_under_the_vads_own_silence() {
        // The two bounds that make 120 ms the right constant, asserted so a
        // later tuning of either one cannot silently invalidate it.
        assert!(DIP_SAMPLES > crate::vad::FRAME_SAMPLES as u64);
        let min_silence = crate::config::VadConfig::default().min_silence_ms as u64;
        assert!(
            DIP_MS < min_silence,
            "a dip must be shorter than a span close"
        );
    }
}
