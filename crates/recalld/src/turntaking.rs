//! Turn-taking statistics (0.10.0): how a person talks, not what they said.
//!
//! Everything here is a **pure query over turns that already exist**. Nothing
//! is stored, nothing is derived by a model, and a deleted segment stops
//! counting the moment it is deleted — the same rule the person page has obeyed
//! since 0.6.2 (DESIGN §0).
//!
//! The reason this is its own module rather than five more SQL statements in
//! `store.rs` is that four of the six numbers are **definitions**, not
//! measurements, and a definition belongs somewhere it can be read and argued
//! with. They are written out in full below, and every one of them is exercised
//! against a synthetic conversation with hand-computed answers
//! (`tests::the_worked_example`), because a statistic nobody can check by hand
//! is a statistic nobody should believe.
//!
//! # The definitions, in full
//!
//! **Talk share.** Their speech nanoseconds over the total speech nanoseconds
//! of the conversations considered. Speech, not wall clock: silence belongs to
//! nobody. Turns with no speaker attached are excluded from BOTH sides, so a
//! share is a fraction of the speech somebody was identified for.
//!
//! **Mean turn length.** Their total speech divided by their turn count.
//!
//! **Longest monologue.** The longest unbroken run of their turns inside one
//! conversation, measured from the run's first turn's start to its last turn's
//! end — so the pauses *inside* a monologue count towards it, which is what
//! makes it a monologue rather than a sum of turns. A run is broken by any turn
//! of another identified voice; an unlabelled turn does not break it, because
//! "somebody said something" is not evidence that somebody else did.
//!
//! **Interruption.** Turn B interrupts turn A when, in the same conversation:
//!
//! 1. A and B have different, identified speakers;
//! 2. B starts strictly inside A — `A.t_start < B.t_start < A.t_end`; and
//! 3. B's `overlap_frac` is at or above [`INTERRUPTION_OVERLAP`], the same line
//!    at which `identity` refuses to put a name to a turn.
//!
//! B's speaker *gave* the interruption and A's *received* it.
//!
//! **This is an approximation and it is worth saying how.** `overlap_frac` is a
//! property of B's own audio — the share of B's speech frames in which the
//! segmentation model heard two people — and it does not name the second
//! voice. Condition (2) is what supplies the name, from the clock, and the
//! clock cannot tell a genuine interruption from a back-channel "mhm" or from
//! two people starting a sentence at once. Condition (3) is what keeps it from
//! counting every clock coincidence: a turn that merely *begins* while another
//! is running, with no overlapped speech in it at all, is two microphones
//! being generous about their boundaries. The count is therefore a floor on
//! rudeness and a ceiling on nothing, and it is only ever shown with its
//! definition attached.
//!
//! **Response latency.** For every turn of theirs whose immediately preceding
//! turn in the same conversation belongs to a different identified voice: the
//! gap from that turn's end to theirs. The reported figure is the **median**.
//! Two exclusions, both deliberate: a negative gap is an overlap, not a
//! response, and a gap longer than [`LATENCY_CAP_MS`] is not a response either
//! — it is a lull that happened to end with them speaking. Excluded rather than
//! clamped, because clamping would let a silent hour vote for "5 s" and drag
//! the median towards a number nobody experienced.
//!
//! **Turns per minute.** Their turn count over the summed wall-clock length of
//! the conversations considered. The denominator is the conversations' length,
//! not their own speech: "how often do they say something" is a question about
//! the room's clock.

use std::collections::{BTreeMap, HashMap};

use anyhow::Result;
use rusqlite::params;

use crate::store::Store;

/// The overlap fraction at which a turn's start counts as an interruption.
///
/// Deliberately the same 0.10 as `IdentityConfig::max_overlap`, the line above
/// which the daemon refuses to put a name to a voice. One number, one meaning:
/// "there is demonstrably somebody else in this audio".
pub const INTERRUPTION_OVERLAP: f32 = 0.1;

/// Gaps longer than this are not responses. Five seconds, which is a long
/// pause in a conversation and a short one in a lobby.
pub const LATENCY_CAP_MS: i64 = 5_000;

/// How many conversations `person.stats` breaks down.
pub const BY_CONVERSATION: usize = 10;

/// One turn, as the statistics see it. Deliberately not a `SegmentRow`: these
/// rules reason about a clock, a voice and one float, and must stay testable
/// without a database or an audio file.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Turn {
    pub thread_id: i64,
    pub t_start_ns: i64,
    pub t_end_ns: i64,
    /// `None` for a turn no voice could be put to.
    pub speaker: Option<i64>,
    /// `None` on a turn the overlap detector never ran on — treated as 0.0,
    /// which is the reading that refuses to call anything an interruption.
    pub overlap_frac: Option<f32>,
}

impl Turn {
    fn speech_ns(&self) -> i64 {
        (self.t_end_ns - self.t_start_ns).max(0)
    }
    fn overlapped(&self) -> bool {
        self.overlap_frac.unwrap_or(0.0) >= INTERRUPTION_OVERLAP
    }
}

/// What one voice's turn-taking looks like.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Stats {
    pub turns: i64,
    pub speech_ns: i64,
    /// Speech of every identified voice in the same conversations.
    pub total_speech_ns: i64,
    /// `speech_ns / total_speech_ns`, or 0.0 when nothing was said.
    pub share: f64,
    pub mean_turn_ns: i64,
    pub longest_monologue_ns: i64,
    pub interruptions_given: i64,
    pub interruptions_received: i64,
    /// Median response gap, or `None` when they never answered anybody inside
    /// the cap — which is a real state and not a zero.
    pub median_latency_ms: Option<i64>,
    /// Summed wall-clock length of the conversations considered.
    pub span_ns: i64,
    pub turns_per_minute: f64,
}

/// Every voice's share of one conversation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Share {
    pub speaker_id: i64,
    pub turns: i64,
    pub speech_ns: i64,
    pub share: f64,
}

/// One row of `person.stats.by_conversation`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ByConversation {
    pub thread_id: i64,
    pub turns: i64,
    pub share: f64,
    pub last_ns: i64,
}

// ---------------------------------------------------------------------------
// the rules
// ---------------------------------------------------------------------------

/// `turns` must be in `(thread, time)` order; [`person_turns`] and
/// [`thread_turns`] both return it that way.
pub fn stats_for(turns: &[Turn], speaker: i64) -> Stats {
    let mut out = Stats::default();
    let mut latencies: Vec<i64> = Vec::new();

    for thread in by_thread(turns) {
        // The conversation's own clock: its earliest start to its latest end.
        // `last()` would be wrong — the slice is in START order, and an
        // interrupting turn can begin before, and end after, the turn that
        // follows it in the list.
        if let Some(first) = thread.first() {
            let end = thread
                .iter()
                .map(|t| t.t_end_ns)
                .max()
                .unwrap_or(first.t_end_ns);
            out.span_ns += (end - first.t_start_ns).max(0);
        }
        let mut run_start: Option<i64> = None;
        let mut run_end: i64 = 0;

        for (i, t) in thread.iter().enumerate() {
            let Some(who) = t.speaker else {
                // Unlabelled: it counts towards nobody's share and breaks
                // nobody's monologue.
                continue;
            };
            out.total_speech_ns += t.speech_ns();
            if who == speaker {
                out.turns += 1;
                out.speech_ns += t.speech_ns();
                match run_start {
                    Some(_) => run_end = run_end.max(t.t_end_ns),
                    None => {
                        run_start = Some(t.t_start_ns);
                        run_end = t.t_end_ns;
                    }
                }
                out.longest_monologue_ns = out
                    .longest_monologue_ns
                    .max((run_end - run_start.unwrap_or(run_end)).max(0));
            } else if run_start.is_some() {
                run_start = None;
            }

            // Interruptions. Only the turn's own start matters, and only
            // against turns that were still running when it began.
            if t.overlapped() {
                for a in thread[..i].iter() {
                    let Some(other) = a.speaker else { continue };
                    if other == who {
                        continue;
                    }
                    if a.t_start_ns < t.t_start_ns && t.t_start_ns < a.t_end_ns {
                        if who == speaker {
                            out.interruptions_given += 1;
                        }
                        if other == speaker {
                            out.interruptions_received += 1;
                        }
                    }
                }
            }

            // Latency, against the immediately preceding IDENTIFIED turn.
            if who == speaker
                && let Some(prev) = thread[..i].iter().rev().find(|p| p.speaker.is_some())
                && prev.speaker != Some(speaker)
            {
                let gap_ms = (t.t_start_ns - prev.t_end_ns) / 1_000_000;
                if (0..=LATENCY_CAP_MS).contains(&gap_ms) {
                    latencies.push(gap_ms);
                }
            }
        }
    }

    if out.total_speech_ns > 0 {
        out.share = out.speech_ns as f64 / out.total_speech_ns as f64;
    }
    if out.turns > 0 {
        out.mean_turn_ns = out.speech_ns / out.turns;
    }
    out.median_latency_ms = median(&mut latencies);
    if out.span_ns > 0 {
        out.turns_per_minute = out.turns as f64 / (out.span_ns as f64 / 60e9);
    }
    out
}

/// Every identified voice's share of one conversation, most talkative first.
pub fn shares(turns: &[Turn]) -> Vec<Share> {
    let mut acc: BTreeMap<i64, (i64, i64)> = BTreeMap::new();
    let mut total = 0i64;
    for t in turns {
        let Some(who) = t.speaker else { continue };
        let slot = acc.entry(who).or_insert((0, 0));
        slot.0 += 1;
        slot.1 += t.speech_ns();
        total += t.speech_ns();
    }
    let mut out: Vec<Share> = acc
        .into_iter()
        .map(|(speaker_id, (turns, speech_ns))| Share {
            speaker_id,
            turns,
            speech_ns,
            share: if total > 0 {
                speech_ns as f64 / total as f64
            } else {
                0.0
            },
        })
        .collect();
    out.sort_by(|a, b| {
        b.speech_ns
            .cmp(&a.speech_ns)
            .then(a.speaker_id.cmp(&b.speaker_id))
    });
    out
}

/// A person's share of each conversation they took part in, newest first.
pub fn by_conversation(turns: &[Turn], speaker: i64, limit: usize) -> Vec<ByConversation> {
    let mut rows: Vec<ByConversation> = Vec::new();
    for thread in by_thread(turns) {
        let mine: i64 = thread
            .iter()
            .filter(|t| t.speaker == Some(speaker))
            .map(Turn::speech_ns)
            .sum();
        let turns_mine = thread.iter().filter(|t| t.speaker == Some(speaker)).count() as i64;
        if turns_mine == 0 {
            continue;
        }
        let total: i64 = thread
            .iter()
            .filter(|t| t.speaker.is_some())
            .map(Turn::speech_ns)
            .sum();
        rows.push(ByConversation {
            thread_id: thread[0].thread_id,
            turns: turns_mine,
            share: if total > 0 {
                mine as f64 / total as f64
            } else {
                0.0
            },
            last_ns: thread.iter().map(|t| t.t_end_ns).max().unwrap_or(0),
        });
    }
    rows.sort_by(|a, b| {
        b.last_ns
            .cmp(&a.last_ns)
            .then(b.thread_id.cmp(&a.thread_id))
    });
    rows.truncate(limit);
    rows
}

/// Split a `(thread, time)`-ordered slice into per-conversation runs.
fn by_thread(turns: &[Turn]) -> Vec<&[Turn]> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for i in 1..=turns.len() {
        if i == turns.len() || turns[i].thread_id != turns[start].thread_id {
            out.push(&turns[start..i]);
            start = i;
        }
    }
    out
}

fn median(xs: &mut [i64]) -> Option<i64> {
    if xs.is_empty() {
        return None;
    }
    xs.sort_unstable();
    let n = xs.len();
    Some(if n % 2 == 1 {
        xs[n / 2]
    } else {
        // The lower-and-upper mean, rounded towards zero. A median of two
        // gaps is the only place this matters and halving them is what a
        // person means by it.
        (xs[n / 2 - 1] + xs[n / 2]) / 2
    })
}

// ---------------------------------------------------------------------------
// the store side
// ---------------------------------------------------------------------------

/// Every turn of every conversation this voice took part in.
///
/// The whole conversation, not only their turns: a share is a fraction of
/// somebody else's speech too, and an interruption needs the turn it landed in.
pub fn person_turns(store: &Store, speaker_id: i64, from_ns: Option<i64>) -> Result<Vec<Turn>> {
    let mut stmt = store.conn().prepare(
        "SELECT g.thread_id, g.t_start_ns, g.t_end_ns, sp.canonical_id, g.overlap_frac
         FROM segments g
         LEFT JOIN speaker_resolved sp ON sp.id = g.speaker_id
         WHERE g.deleted_at IS NULL AND g.thread_id IS NOT NULL
           AND (?2 IS NULL OR g.t_start_ns >= ?2)
           AND g.thread_id IN (
                 SELECT DISTINCT g2.thread_id FROM segments g2
                 JOIN speaker_resolved sp2 ON sp2.id = g2.speaker_id
                 WHERE g2.deleted_at IS NULL AND g2.thread_id IS NOT NULL
                   AND sp2.canonical_id = ?1
                   AND (?2 IS NULL OR g2.t_start_ns >= ?2))
         ORDER BY g.thread_id ASC, g.t_start_ns ASC, g.id ASC",
    )?;
    Ok(stmt
        .query_map(params![speaker_id, from_ns], turn_from)?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// One conversation's turns, in order.
pub fn thread_turns(store: &Store, thread_id: i64) -> Result<Vec<Turn>> {
    let mut stmt = store.conn().prepare(
        "SELECT g.thread_id, g.t_start_ns, g.t_end_ns, sp.canonical_id, g.overlap_frac
         FROM segments g
         LEFT JOIN speaker_resolved sp ON sp.id = g.speaker_id
         WHERE g.deleted_at IS NULL AND g.thread_id = ?1
         ORDER BY g.t_start_ns ASC, g.id ASC",
    )?;
    Ok(stmt
        .query_map(params![thread_id], turn_from)?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

fn turn_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<Turn> {
    Ok(Turn {
        thread_id: r.get(0)?,
        t_start_ns: r.get(1)?,
        t_end_ns: r.get(2)?,
        speaker: r.get(3)?,
        overlap_frac: r.get(4)?,
    })
}

/// Display labels for a set of voices, for a client that must render a share
/// bar without a second round trip.
pub fn labels(store: &Store, ids: &[i64]) -> Result<HashMap<i64, (Option<String>, String)>> {
    let mut out = HashMap::new();
    for id in ids {
        if let Some(s) = store.speaker_summary(*id)? {
            out.insert(*id, (s.name().map(str::to_string), s.auto_label.clone()));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: i64 = 1_000_000;

    /// `(thread, start_ms, end_ms, speaker, overlap)`
    fn t(thread: i64, a: i64, b: i64, who: Option<i64>, ov: f32) -> Turn {
        Turn {
            thread_id: thread,
            t_start_ns: a * MS,
            t_end_ns: b * MS,
            speaker: who,
            overlap_frac: Some(ov),
        }
    }

    /// The fixture, and the whole point of this module having tests.
    ///
    /// One conversation, two voices, sixty seconds of clock. Every number below
    /// is computed by hand in the comment beside it, and the assertions are
    /// EXACT — an approximate assertion on a synthetic fixture would only be
    /// testing that the code runs.
    ///
    /// ```text
    ///  t (s)  0      2      4      6      8     10 ...          30    32
    ///  A      [--2s--]             [--2s--]      [------ 12 s ------]
    ///  B             [--2s--]  ^B starts at 5.0, inside A's 4–6 turn
    /// ```
    fn conversation() -> Vec<Turn> {
        vec![
            // A: 0.0–2.0 (2 s)
            t(1, 0, 2_000, Some(1), 0.0),
            // B: 2.5–4.0 (1.5 s). Gap after A: 500 ms.
            t(1, 2_500, 4_000, Some(2), 0.0),
            // A: 4.5–6.5 (2 s). Gap after B: 500 ms.
            t(1, 4_500, 6_500, Some(1), 0.0),
            // B: 5.5–7.5 (2 s), overlap 0.4 — starts INSIDE A's turn, so this
            // is B interrupting A. Negative gap, so it is not a response.
            t(1, 5_500, 7_500, Some(2), 0.4),
            // A: 10.0–22.0 (12 s), then A again 22.5–30.5 (8 s): one unbroken
            // run, 10.0 → 30.5 = 20.5 s of monologue.
            t(1, 10_000, 22_000, Some(1), 0.0),
            t(1, 22_500, 30_500, Some(1), 0.0),
            // B: 32.5–34.5 (2 s). Gap after A: 2000 ms.
            t(1, 32_500, 34_500, Some(2), 0.0),
            // An unlabelled turn: counts towards nobody and breaks nothing.
            t(1, 35_000, 36_000, None, 0.0),
            // A: 50.0–51.0 (1 s). The previous IDENTIFIED turn is B's, ending
            // at 34.5 — a gap of 15.5 s, far over the cap, so not a response.
            t(1, 50_000, 51_000, Some(1), 0.0),
        ]
    }

    #[test]
    fn the_worked_example() {
        let turns = conversation();

        // ---- A -----------------------------------------------------------
        // Speech: 2 + 2 + 12 + 8 + 1 = 25 s over 5 turns.
        // B's speech: 1.5 + 2 + 2 = 5.5 s. Identified total = 30.5 s.
        let a = stats_for(&turns, 1);
        assert_eq!(a.turns, 5);
        assert_eq!(a.speech_ns, 25_000 * MS);
        assert_eq!(a.total_speech_ns, 30_500 * MS);
        assert_eq!(a.share, 25_000.0 / 30_500.0);
        // 25 s / 5 turns, in whole nanoseconds.
        assert_eq!(a.mean_turn_ns, 25_000 * MS / 5);
        // 10.0 → 30.5, pauses inside it included.
        assert_eq!(a.longest_monologue_ns, 20_500 * MS);
        assert_eq!(a.interruptions_given, 0);
        assert_eq!(a.interruptions_received, 1);
        // A's responses to B: 4.5 − 4.0 = 500 ms, 10.0 − 7.5 = 2500 ms, and
        // 50.0 − 34.5 = 15 500 ms (dropped, over the cap). Median of
        // {500, 2500} is 1500.
        assert_eq!(a.median_latency_ms, Some(1500));
        // The conversation runs 0.0 → 51.0.
        assert_eq!(a.span_ns, 51_000 * MS);
        assert_eq!(a.turns_per_minute, 5.0 / (51.0 / 60.0));

        // ---- B -----------------------------------------------------------
        let b = stats_for(&turns, 2);
        assert_eq!(b.turns, 3);
        assert_eq!(b.speech_ns, 5_500 * MS);
        assert_eq!(b.total_speech_ns, 30_500 * MS);
        assert_eq!(b.share, 5_500.0 / 30_500.0);
        assert_eq!(b.mean_turn_ns, 5_500 * MS / 3);
        // B never takes two turns in a row: the longest run is one turn, and
        // the longest of those is 2 s.
        assert_eq!(b.longest_monologue_ns, 2_000 * MS);
        assert_eq!(b.interruptions_given, 1);
        assert_eq!(b.interruptions_received, 0);
        // B's responses to A: 2.5 − 2.0 = 500 ms; 5.5 − 6.5 = −1000 ms
        // (an overlap, not a response); 32.5 − 30.5 = 2000 ms. Median of
        // {500, 2000} is 1250.
        assert_eq!(b.median_latency_ms, Some(1250));
        assert_eq!(b.span_ns, 51_000 * MS);

        // The two shares add to one, which is what makes them shares.
        assert!((a.share + b.share - 1.0).abs() < 1e-12);
    }

    #[test]
    fn an_overlap_below_the_refuse_line_is_not_an_interruption() {
        // Identical timing, one number changed: B still starts inside A's
        // turn, but its audio holds no second voice. Two microphones being
        // generous about a boundary is not an interruption.
        let mut turns = conversation();
        turns[3].overlap_frac = Some(INTERRUPTION_OVERLAP - 0.01);
        assert_eq!(stats_for(&turns, 2).interruptions_given, 0);
        assert_eq!(stats_for(&turns, 1).interruptions_received, 0);

        // Exactly at the line counts: the line is the same one identity
        // refuses at, and "at or above" is what that means there too.
        turns[3].overlap_frac = Some(INTERRUPTION_OVERLAP);
        assert_eq!(stats_for(&turns, 2).interruptions_given, 1);

        // A turn the detector never ran on reads as no overlap, never as some.
        turns[3].overlap_frac = None;
        assert_eq!(stats_for(&turns, 2).interruptions_given, 0);
    }

    #[test]
    fn shares_of_a_conversation_add_up() {
        let s = shares(&conversation());
        assert_eq!(s.len(), 2, "the unlabelled turn is nobody's share");
        assert_eq!(s[0].speaker_id, 1);
        assert_eq!(s[0].turns, 5);
        assert_eq!(s[0].speech_ns, 25_000 * MS);
        assert_eq!(s[1].speaker_id, 2);
        assert!((s.iter().map(|x| x.share).sum::<f64>() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn a_monologue_does_not_run_across_two_conversations() {
        // The same voice, back to back, in two threads. That is two turns in
        // two rooms, not one long speech.
        let turns = vec![
            t(1, 0, 10_000, Some(1), 0.0),
            t(2, 10_000, 20_000, Some(1), 0.0),
        ];
        let s = stats_for(&turns, 1);
        assert_eq!(s.longest_monologue_ns, 10_000 * MS);
        // And the span is both conversations' clocks added together.
        assert_eq!(s.span_ns, 20_000 * MS);
    }

    #[test]
    fn silence_and_strangers_belong_to_nobody() {
        let turns = vec![
            t(1, 0, 1_000, Some(1), 0.0),
            t(1, 5_000, 6_000, None, 0.9),
            t(1, 9_000, 10_000, Some(1), 0.0),
        ];
        let s = stats_for(&turns, 1);
        assert_eq!(s.share, 1.0, "the only identified voice has all of it");
        assert_eq!(s.total_speech_ns, 2_000 * MS);
        // An unlabelled turn between two of theirs does not break the run.
        assert_eq!(s.longest_monologue_ns, 10_000 * MS);
        assert_eq!(s.median_latency_ms, None, "they answered nobody");
        // An overlapped turn with no voice on it interrupts nobody.
        assert_eq!(s.interruptions_received, 0);
    }

    #[test]
    fn a_voice_that_never_spoke_is_zero_rather_than_a_division_by_zero() {
        let s = stats_for(&[], 1);
        assert_eq!(s, Stats::default());
        assert_eq!(s.share, 0.0);
        assert_eq!(s.turns_per_minute, 0.0);
        assert!(shares(&[]).is_empty());
        assert!(by_conversation(&[], 1, 10).is_empty());
    }

    #[test]
    fn by_conversation_is_newest_first_and_only_theirs() {
        let turns = vec![
            t(1, 0, 1_000, Some(1), 0.0),
            t(1, 1_000, 3_000, Some(2), 0.0),
            t(2, 10_000, 11_000, Some(2), 0.0),
            t(3, 20_000, 24_000, Some(1), 0.0),
        ];
        let rows = by_conversation(&turns, 1, 10);
        assert_eq!(rows.len(), 2, "thread 2 has none of their turns");
        assert_eq!(rows[0].thread_id, 3);
        assert_eq!(rows[0].share, 1.0);
        assert_eq!(rows[1].thread_id, 1);
        assert_eq!(rows[1].share, 1_000.0 / 3_000.0);
        assert_eq!(by_conversation(&turns, 1, 1).len(), 1);
    }
}
