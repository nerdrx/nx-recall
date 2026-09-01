//! Conversation threads: untangling the interleaved lobby (GRAPH.md, Tier 1).
//!
//! A VRChat instance is not one conversation. Four people in a bar world are
//! two pairs talking past each other, and a transcript that renders them as one
//! chronological column is technically correct and practically unreadable. This
//! is the Tier 1 answer: turn-taking adjacency, materialised as `threads(id)`
//! plus `segments.thread_id`.
//!
//! Tier 1 means **deterministic, no models, no judgement calls**, so the rule
//! here is deliberately small enough to state in a sentence and to argue with:
//!
//! > A turn continues a conversation it is already part of. A voice nobody in
//! > the conversation has heard from joins it while the conversation is still
//! > forming, and starts its own once the conversation has found a rhythm.
//!
//! Spelled out, for a turn at `t` in session `S`:
//!
//! 1. **Open threads** are `S`'s threads whose last turn ended within
//!    `thread_gap_s` of `t`. Everything else is closed for good — that is what
//!    keeps a thread a conversation rather than a session.
//! 2. An **unlabelled** turn follows pure recency: it joins the nearest open
//!    thread. Nothing is known about who spoke, so nothing but the clock can
//!    decide, and pretending otherwise would be a judgement call.
//! 3. A **labelled** turn joins the most recently active open thread whose
//!    recent participants (the last [`RECENT_SPEAKERS`] distinct voices) include
//!    this speaker. This is the alternation rule: once A and B are talking and C
//!    and D are talking, every subsequent turn sorts itself, however tangled the
//!    two are in time.
//! 4. Otherwise, if the nearest open thread is **not established** — nobody in
//!    it has taken a second turn yet — the turn joins it. This is how a
//!    conversation grows from one voice to a group.
//! 5. Otherwise it opens a new thread.
//!
//! ### What this gets wrong, on purpose
//!
//! Two conversations that begin interleaved from their very first turns are
//! indistinguishable from one growing group at the moment they begin: A then C
//! carries no information that separates "C answered A" from "C started
//! talking to someone else". They merge, and stay merged. Once each pair has
//! taken a second turn the rule sorts every later turn correctly, which is the
//! case worth getting right — an evening is long and its first ten seconds are
//! not what anyone searches for. A group of more than [`RECENT_SPEAKERS`]
//! people will likewise shed its quietest member into a thread of their own.
//!
//! Both are honest failures of a rule that cannot see the future, and both are
//! recoverable by a Tier 2/3 pass that can. Guessing better here would mean
//! guessing, which is what the tier boundary exists to prevent.

use std::collections::BTreeSet;

/// How many distinct voices count as a thread's *recent participants*. Four,
/// because a group of four is an ordinary VRChat circle and has to survive one
/// person being quiet for three turns; beyond that the thread would admit
/// anybody who ever spoke in it, which is the same as having no rule.
pub const RECENT_SPEAKERS: usize = 4;

/// One turn as threading sees it. Deliberately not a `SegmentRow`: the rule
/// reasons about a clock and a speaker id and must stay testable without a
/// database, an audio file or a transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Turn {
    pub t_start_ns: i64,
    pub t_end_ns: i64,
    /// `None` for a turn the pipeline could not put a voice to — overlapped
    /// speech, or a stranger the voicebank did not match.
    pub speaker: Option<i64>,
}

/// A thread still close enough in time to be continued, with the state the rule
/// reads. Built from the database by `Store::open_threads`, or accumulated in
/// memory by [`Threader`] when a whole session is replayed at once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenThread {
    pub id: i64,
    /// End of its most recent turn — what "open" and "nearest" are measured on.
    pub last_ns: i64,
    /// Distinct speakers, most recent first, capped at [`RECENT_SPEAKERS`].
    pub recent: Vec<i64>,
    /// Every distinct voice heard in the thread. Only its *size* matters, but
    /// keeping the set is what makes `turns > voices` an exact statement rather
    /// than an estimate.
    pub voices: BTreeSet<i64>,
    /// Labelled turns in the thread. Unlabelled ones do not count: they say
    /// nothing about turn-taking.
    pub turns: usize,
}

impl OpenThread {
    /// A thread nobody has spoken in twice is still deciding who is in it.
    pub fn established(&self) -> bool {
        self.turns > self.voices.len()
    }

    /// Fold a turn that has been placed into this thread.
    pub fn absorb(&mut self, turn: &Turn) {
        self.last_ns = self.last_ns.max(turn.t_end_ns);
        let Some(speaker) = turn.speaker else {
            // An unlabelled turn extends the thread in time and nothing else.
            return;
        };
        self.turns += 1;
        self.voices.insert(speaker);
        self.recent.retain(|&id| id != speaker);
        self.recent.insert(0, speaker);
        self.recent.truncate(RECENT_SPEAKERS);
    }

    /// A thread that has just been opened by its first turn.
    pub fn opened(id: i64, turn: &Turn) -> Self {
        let mut t = Self {
            id,
            last_ns: turn.t_start_ns,
            recent: Vec::new(),
            voices: BTreeSet::new(),
            turns: 0,
        };
        t.absorb(turn);
        t
    }
}

/// Where a turn belongs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Continue this thread.
    Join(i64),
    /// Nothing open fits: this turn starts a conversation.
    New,
}

/// The whole rule, as a pure function.
///
/// `threads` is every thread of the turn's session that could still be open;
/// it need not be sorted or filtered — the gap is applied here, so one caller
/// cannot apply it differently from another.
pub fn place(threads: &[OpenThread], turn: &Turn, gap_ns: i64) -> Placement {
    // Step 1: what is still open, nearest first. Overlapping turns have a gap
    // of zero rather than a negative one.
    let mut open: Vec<&OpenThread> = threads
        .iter()
        .filter(|t| (turn.t_start_ns - t.last_ns).max(0) <= gap_ns)
        .collect();
    if open.is_empty() {
        return Placement::New;
    }
    // Ties broken by id so the same history always threads the same way.
    open.sort_by(|a, b| b.last_ns.cmp(&a.last_ns).then(b.id.cmp(&a.id)));

    let Some(speaker) = turn.speaker else {
        // Step 2: nobody knows who said this, so only the clock may decide.
        return Placement::Join(open[0].id);
    };

    // Step 3: the alternation rule — a voice continues the conversation it is
    // already part of, even when another one has been talking over it since.
    if let Some(t) = open.iter().find(|t| t.recent.contains(&speaker)) {
        return Placement::Join(t.id);
    }

    // Step 4: a new voice joins a conversation that has not found its rhythm.
    if !open[0].established() {
        return Placement::Join(open[0].id);
    }

    // Step 5: an established conversation plus a stranger is two conversations.
    Placement::New
}

/// Replays a session's turns in time order, minting thread ids as it goes.
///
/// This is the one implementation of the rule *over a sequence*, shared by the
/// v6 migration's backfill and by the tests, so a database that was threaded on
/// the way in and one that was threaded on the way up cannot disagree.
#[derive(Debug)]
pub struct Threader {
    gap_ns: i64,
    open: Vec<OpenThread>,
}

impl Threader {
    pub fn new(gap_s: f32) -> Self {
        Self {
            gap_ns: (gap_s.max(0.0) as f64 * 1e9) as i64,
            open: Vec::new(),
        }
    }

    pub fn gap_ns(&self) -> i64 {
        self.gap_ns
    }

    /// Place one turn, calling `mint` when it starts a thread. Turns must be
    /// offered in ascending `t_start_ns` order.
    pub fn push<E>(
        &mut self,
        turn: &Turn,
        mint: impl FnOnce(&Turn) -> Result<i64, E>,
    ) -> Result<i64, E> {
        match place(&self.open, turn, self.gap_ns) {
            Placement::Join(id) => {
                if let Some(t) = self.open.iter_mut().find(|t| t.id == id) {
                    t.absorb(turn);
                }
                Ok(id)
            }
            Placement::New => {
                let id = mint(turn)?;
                self.open.push(OpenThread::opened(id, turn));
                Ok(id)
            }
        }
    }

    /// Every thread it has seen, in the order they were opened.
    pub fn threads(&self) -> &[OpenThread] {
        &self.open
    }
}

/// Thread one stored segment, now that it exists.
///
/// Called once per committed turn, from the pipeline — threading is
/// incremental, never a batch pass, so that the transcript a client is reading
/// is already threaded rather than threaded later by something that has to run.
/// Returns the thread the turn landed in.
///
/// A turn is threaded once, from what was known when it was stored. A later
/// relabel (proximity inheritance naming the turn before, a merge, a manual
/// reassignment) does not re-thread it: the conversation it was part of is a
/// fact about the clock and the room, and rewriting history every time a name
/// changes would make the same transcript thread differently depending on when
/// you looked at it.
pub fn assign(
    store: &crate::store::Store,
    cfg: &crate::config::GraphConfig,
    segment_id: i64,
) -> anyhow::Result<Option<i64>> {
    let Some((session_id, turn)) = store.segment_turn(segment_id)? else {
        return Ok(None);
    };
    let gap_ns = (cfg.thread_gap_s.max(0.0) as f64 * 1e9) as i64;
    let open = store.open_threads(session_id, turn.t_start_ns, gap_ns)?;
    let thread_id = match place(&open, &turn, gap_ns) {
        Placement::Join(id) => id,
        Placement::New => store.create_thread(session_id, turn.t_start_ns, turn.t_end_ns)?,
    };
    store.set_segment_thread(segment_id, thread_id, turn.t_end_ns)?;
    Ok(Some(thread_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: i64 = 1_000_000_000;
    const GAP_S: f32 = 20.0;

    /// A turn by `speaker` starting at `at` seconds, three seconds long.
    fn turn(at: i64, speaker: Option<i64>) -> Turn {
        Turn {
            t_start_ns: at * S,
            t_end_ns: at * S + 3 * S,
            speaker,
        }
    }

    /// Replay a script of `(second, speaker)` and report which thread each turn
    /// landed in, as small integers in the order the threads were opened.
    fn thread_of(script: &[(i64, Option<i64>)]) -> Vec<i64> {
        let mut th = Threader::new(GAP_S);
        let mut next = 0i64;
        script
            .iter()
            .map(|(at, sp)| {
                th.push(&turn(*at, *sp), |_| -> Result<i64, ()> {
                    next += 1;
                    Ok(next)
                })
                .unwrap()
            })
            .collect()
    }

    /// `(second, speaker)` for a back-and-forth, one turn every five seconds.
    fn dialogue(from: i64, pairs: &[i64]) -> Vec<(i64, Option<i64>)> {
        pairs
            .iter()
            .enumerate()
            .map(|(i, sp)| (from + i as i64 * 5, Some(*sp)))
            .collect()
    }

    // ---- the table -------------------------------------------------------

    #[test]
    fn one_voice_answering_another_is_one_thread() {
        // The lone Q&A from the brief: two people, one conversation.
        assert_eq!(thread_of(&dialogue(0, &[1, 2, 1, 2])), vec![1, 1, 1, 1]);
    }

    #[test]
    fn two_pairs_talking_past_each_other_are_two_threads() {
        // The case the feature exists for. A and B find their rhythm; C is a
        // stranger to that rhythm and starts their own, which D joins.
        let script = dialogue(0, &[1, 2, 1, 2, 3, 4, 3, 4]);
        assert_eq!(thread_of(&script), vec![1, 1, 1, 1, 2, 2, 2, 2]);
    }

    #[test]
    fn once_two_threads_exist_their_turns_interleave_correctly() {
        // A,B established, then C,D beside them, then every turn alternates
        // between the two conversations. This is what "untangling the lobby"
        // means, and it is the case the feature is for.
        let mut script = dialogue(0, &[1, 2, 1, 2]);
        script.push((18, Some(3)));
        script.push((22, Some(4)));
        for (i, sp) in [1, 3, 2, 4, 1, 3, 2, 4].iter().enumerate() {
            script.push((26 + i as i64 * 3, Some(*sp)));
        }
        let got = thread_of(&script);
        assert_eq!(&got[..6], &[1, 1, 1, 1, 2, 2]);
        assert_eq!(&got[6..], &[1, 2, 1, 2, 1, 2, 1, 2]);
    }

    #[test]
    fn a_group_that_takes_turns_in_order_stays_one_conversation() {
        // Three, and four, people going round the circle: everyone is new until
        // somebody speaks twice, which is exactly how a group forms.
        assert_eq!(
            thread_of(&dialogue(0, &[1, 2, 3, 1, 2, 3])),
            vec![1, 1, 1, 1, 1, 1]
        );
        assert_eq!(
            thread_of(&dialogue(0, &[1, 2, 3, 4, 1, 2, 3, 4])),
            vec![1, 1, 1, 1, 1, 1, 1, 1]
        );
    }

    #[test]
    fn a_long_silence_ends_a_thread_even_for_the_same_voices() {
        // 25 s of nothing is past the 20 s gap: the same two people coming back
        // are having a second conversation, not continuing the first.
        let mut script = dialogue(0, &[1, 2, 1, 2]);
        script.extend(dialogue(40, &[1, 2]));
        assert_eq!(thread_of(&script), vec![1, 1, 1, 1, 2, 2]);
    }

    #[test]
    fn a_turn_inside_the_gap_continues_the_thread() {
        // The boundary itself: the last turn ends at 18 s, this one starts at
        // 38 s — exactly the gap, and therefore still the same conversation.
        let script = vec![
            (0, Some(1)),
            (5, Some(2)),
            (10, Some(1)),
            (15, Some(2)),
            (38, Some(1)),
        ];
        assert_eq!(thread_of(&script), vec![1, 1, 1, 1, 1]);
    }

    #[test]
    fn an_unlabelled_turn_follows_the_clock_and_nothing_else() {
        // Two conversations running; a turn nobody could identify lands in the
        // one that spoke most recently, because that is all that is known.
        let mut script = dialogue(0, &[1, 2, 1, 2, 3, 4, 3, 4]);
        script.push((36, None)); // nearest to C/D's thread
        script.push((38, Some(1))); // …and A is still A
        let got = thread_of(&script);
        assert_eq!(got[8], 2, "an unlabelled turn joins the nearest thread");
        assert_eq!(got[9], 1, "a labelled one still follows its own voice");
    }

    #[test]
    fn an_unlabelled_turn_never_establishes_a_thread() {
        // Nothing about an anonymous turn says who is in the conversation, so
        // it must not close the door on the next voice.
        let script = vec![(0, Some(1)), (5, None), (10, None), (15, Some(2))];
        assert_eq!(thread_of(&script), vec![1, 1, 1, 1]);
    }

    #[test]
    fn the_first_turn_of_a_session_opens_a_thread() {
        assert_eq!(thread_of(&[(0, Some(1))]), vec![1]);
        assert_eq!(thread_of(&[(0, None)]), vec![1]);
    }

    #[test]
    fn a_stranger_after_an_established_pair_starts_their_own() {
        let script = dialogue(0, &[1, 2, 1, 2, 9]);
        assert_eq!(thread_of(&script), vec![1, 1, 1, 1, 2]);
    }

    #[test]
    fn a_voice_returning_after_four_others_have_spoken_is_not_forgotten() {
        // RECENT_SPEAKERS is 4, so a fifth distinct voice pushes the first out
        // of `recent`. The turn after that lands by the not-established rule
        // rather than by alternation — it is still one conversation.
        let script = dialogue(0, &[1, 2, 3, 4, 5, 1]);
        assert_eq!(thread_of(&script), vec![1, 1, 1, 1, 1, 1]);
    }

    // ---- the pure function's own edges -----------------------------------

    #[test]
    fn nothing_open_means_a_new_thread() {
        assert_eq!(place(&[], &turn(0, Some(1)), 20 * S), Placement::New);
        let stale = OpenThread::opened(7, &turn(0, Some(1)));
        // 100 s later, far past the gap.
        assert_eq!(place(&[stale], &turn(100, Some(1)), 20 * S), Placement::New);
    }

    #[test]
    fn overlapping_turns_have_no_negative_gap() {
        let mut t = OpenThread::opened(7, &turn(0, Some(1)));
        t.last_ns = 10 * S; // ends after the next turn starts
        assert_eq!(place(&[t], &turn(5, Some(2)), 20 * S), Placement::Join(7));
    }

    #[test]
    fn the_nearest_thread_wins_when_neither_knows_the_voice() {
        let older = OpenThread::opened(1, &turn(0, Some(1)));
        let mut newer = OpenThread::opened(2, &turn(5, Some(2)));
        // Establish both so the not-established rule cannot fire.
        newer.absorb(&turn(8, Some(2)));
        let mut older = older;
        older.absorb(&turn(2, Some(1)));
        assert_eq!(
            place(&[older, newer], &turn(12, Some(9)), 20 * S),
            Placement::New
        );
    }

    #[test]
    fn absorbing_moves_a_voice_to_the_front_of_recent() {
        let mut t = OpenThread::opened(1, &turn(0, Some(1)));
        t.absorb(&turn(5, Some(2)));
        t.absorb(&turn(10, Some(1)));
        assert_eq!(t.recent, vec![1, 2]);
        assert_eq!(t.voices.len(), 2);
        assert_eq!(t.turns, 3);
        assert!(t.established());
    }

    #[test]
    fn recent_is_capped_and_ordered_by_recency() {
        let mut t = OpenThread::opened(1, &turn(0, Some(1)));
        for (i, sp) in [2, 3, 4, 5].iter().enumerate() {
            t.absorb(&turn(5 + i as i64 * 5, Some(*sp)));
        }
        assert_eq!(t.recent, vec![5, 4, 3, 2]);
        assert_eq!(t.voices.len(), 5);
    }
}
