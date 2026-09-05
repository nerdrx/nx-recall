//! Promise candidates, by rule (GRAPH.md Tier 2).
//!
//! > "Promise candidates: modal-pattern lattice ("I'll …", "ich schick dir …",
//! > "mach ich bis …") + a person + an optional time_ref in range →
//! > `commitments(who, to_whom, what_segment, due?, state=candidate)`. Expected
//! > recall is modest; that is fine — candidates are suggestions, never
//! > actions."
//!
//! This module is the whole Tier 2 pass: it runs on the capture path, right
//! after a turn has been threaded, and it costs a handful of regex scans. There
//! is no batch job that has to have run, for the same reason threading has
//! none — the transcript a client is reading is already annotated.
//!
//! ### Three conditions, and why each one is there
//!
//! 1. **A modal pattern.** Someone said they would do something. This is the
//!    signal; everything else is a filter on it.
//! 2. **A known voice.** A promise with no promiser is not a promise, and the
//!    graph refuses to put words in an unidentified mouth.
//! 3. **A counterparty in the conversation.** An obligation needs somebody to
//!    be owed to. Talking to yourself in an empty thread is a plan, not a
//!    promise — and this is the filter that kills most of "I'll probably log
//!    off soon" without needing to understand it.
//!
//! An optional time reference ([`crate::timeref`]) supplies the due date, and
//! its presence raises the confidence a little. Its absence does not disqualify
//! anything: "sure, I will cut it and send it over" is a promise with no date.
//!
//! ### What it gets wrong, on purpose
//!
//! The rules cannot tell a promise from in-game banter ("I will kill you next
//! round"), and they do not try. That is precisely the boundary GRAPH.md draws
//! between the tiers: a pattern match is a **suggestion, marked as a guess,
//! that nothing acts on**, and the tiny local model of Tier 3 — which the
//! bake-off measured at 9/9 trap rejections — is what upgrades a guess into a
//! claim, or [retracts it entirely](crate::store::Store::retract_rule_candidate).
//!
//! The cheap negatives below are the ones that cost nothing and are never
//! wrong: a question is not a promise, a hedge is not a promise, and something
//! already done is not owed.

use std::sync::OnceLock;

use anyhow::Result;
use regex::Regex;

use crate::store::{NewCommitment, Store, commitment_source};
use crate::timeref::{self, TimeRef};

/// Written on every row this module files.
pub const EXTRACTOR: &str = "promise-rules";
pub const VERSION: u32 = 1;

/// A rule match is a low-confidence thing by construction, and the numbers say
/// so out loud. Nothing here reaches the 0.5 a coin toss would.
const BASE_CONFIDENCE: f64 = 0.25;
const WITH_DUE: f64 = 0.08;
/// Exactly one other voice in the conversation: there is no ambiguity about who
/// is being promised.
const WITH_ONE_COUNTERPARTY: f64 = 0.07;

/// How much of the turn a commitment's `what` keeps. Tier 2 does not summarise
/// — the `what` IS what was said — but a runaway ASR line is not a promise
/// either.
const MAX_WHAT: usize = 240;

/// A pattern match, before anything is known about the conversation it sits in.
#[derive(Debug, Clone, PartialEq)]
pub struct Guess {
    /// The turn's own words. Tier 2 does not summarise (same rule as a thread
    /// preview): a shortened promise is a promise somebody has to go and check.
    pub what: String,
    /// The modal phrase that fired, kept so a surface can say *why* this is on
    /// the list at all.
    pub matched: String,
}

/// Does this line read as somebody promising something?
///
/// A pure function of the text, so the whole rule is one table test away from
/// being argued with.
pub fn guess(text: &str) -> Option<Guess> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let folded = fold(trimmed);

    // A question is a request, not an undertaking — even one full of modals
    // ("will you send me the link?").
    if trimmed.ends_with('?') {
        return None;
    }
    if vetoes().is_match(&folded) {
        return None;
    }
    let m = modals().find(&folded)?;
    Some(Guess {
        what: truncate(trimmed, MAX_WHAT),
        matched: trimmed[m.start()..m.end()].trim().to_string(),
    })
}

/// Byte-length preserving fold, so a match's offsets index the original string.
/// Identical to [`crate::timeref`]'s, and for the same reason.
fn fold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        out.push(match ch {
            'A'..='Z' => ch.to_ascii_lowercase(),
            'Ä' => 'ä',
            'Ö' => 'ö',
            'Ü' => 'ü',
            other => other,
        });
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", s[..end].trim_end())
}

/// The modal lattice. German puts the verb first as often as it puts the
/// pronoun first ("mach ich", "schick ich dir das"), so both orders are here;
/// English is nearly all contractions in speech, and the ASR writes them out.
fn modals() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r"\bi'?ll\b",
            r"|\bi will\b",
            r"|\bi'?m (?:gonna|going to)\b",
            r"|\bi'?ll be\b",
            r"|\bich (?:werde|schicke?|mache?|lade?|zeige?|bringe?|gebe?|hole?|baue?|kümmere?)\b",
            r"|\b(?:schick|mach|lad|zeig|bring|geb|hol|bau|schicke|mache|lade|zeige|bringe|gebe|hole|baue) ich\b",
            r"|\bversprochen\b",
        ))
        .expect("a compile-time pattern")
    })
}

/// The refusals that are never wrong. Deliberately short: a filter that is
/// occasionally wrong would cost recall this tier has little of to spare, and
/// the point of Tier 2 is to be cheap and honest rather than clever.
fn vetoes() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            // Hedges. "I will probably just sleep after this" is the bench's
            // own trap, and this is the line that refuses it.
            r"\b(?:probably|maybe|might|i guess|i think i|perhaps|possibly)\b",
            r"|\b(?:vielleicht|wahrscheinlich|eventuell|villeicht|glaub ich|ich glaube?)\b",
            // Suggestions to the group. Nobody has undertaken anything.
            r"|\b(?:we should|you should|somebody should|let'?s)\b",
            r"|\b(?:wir sollten|du solltest|man (?:sollte|müsste|könnte))\b",
            // Hypotheticals.
            r"|\bif i (?:ever|get|can|had)\b",
            r"|\bwenn ich (?:mal|jemals|irgendwann)\b",
            // Already done: not owed.
            r"|\b(?:yesterday|already sent|already did|i sent you|i did that)\b",
            r"|\b(?:gestern|schon geschickt|hab ich schon|hatte ich schon|bereits geschickt)\b",
        ))
        .expect("a compile-time pattern")
    })
}

/// What one segment's Tier 2 pass produced.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Extracted {
    pub time_refs: usize,
    /// Whether a promise candidate was filed (or refreshed) for this segment.
    pub commitment: bool,
}

/// Run Tier 2 over one stored turn.
///
/// Called from the pipeline once the turn has its transcript, its speaker and
/// its thread — which is also the order those three become true, so nothing
/// here has to wait for anything.
///
/// A failure is the caller's to log and ignore: a derived annotation is never
/// allowed to cost a recording.
pub fn extract(store: &Store, segment_id: i64, at_utc_ns: i64) -> Result<Extracted> {
    let Some(row) = store.segment_row(segment_id)? else {
        return Ok(Extracted::default());
    };
    let Some(text) = row.text.as_deref().map(str::trim).filter(|t| !t.is_empty()) else {
        return Ok(Extracted::default());
    };

    // Resolved against the turn's own capture time — GRAPH.md's "that is the
    // whole trick". Never against now: by the time anything reads this, the
    // Friday the speaker meant has moved.
    let refs = timeref::extract(text, row.t_start_ns);
    let n = store.replace_time_refs(segment_id, &refs, at_utc_ns)?;

    let Some(guess) = guess(text) else {
        return Ok(Extracted {
            time_refs: n,
            commitment: false,
        });
    };
    // Condition 2: a promise needs a promiser.
    let Some(who) = row.speaker_id else {
        return Ok(Extracted {
            time_refs: n,
            commitment: false,
        });
    };
    // Condition 3: and somebody to be owed.
    let Some(thread_id) = row.thread_id else {
        return Ok(Extracted {
            time_refs: n,
            commitment: false,
        });
    };
    let others: Vec<i64> = store
        .thread_participants(thread_id)?
        .into_iter()
        .filter(|p| *p != who)
        .collect();
    if others.is_empty() {
        return Ok(Extracted {
            time_refs: n,
            commitment: false,
        });
    }

    let due = pick_due(&refs, row.t_start_ns);
    let mut confidence = BASE_CONFIDENCE;
    if due.is_some() {
        confidence += WITH_DUE;
    }
    // Named to exactly one person, or to the room. Both are candidates; only
    // the first is unambiguous enough to be worth more confidence.
    let to = if others.len() == 1 {
        confidence += WITH_ONE_COUNTERPARTY;
        Some(others[0])
    } else {
        None
    };

    store.upsert_commitment(
        &NewCommitment {
            segment_id,
            thread_id: Some(thread_id),
            who_speaker_id: Some(who),
            to_speaker_id: to,
            what: guess.what,
            due_utc_ns: due.map(|d| d.resolved_utc_ns),
            due_raw: due.map(|d| d.raw.clone()),
            due_kind: due.map(|d| d.kind.to_string()),
            source: commitment_source::RULES,
            model_id: Some(format!("{EXTRACTOR}@{VERSION}")),
            confidence,
        },
        at_utc_ns,
    )?;
    Ok(Extracted {
        time_refs: n,
        commitment: true,
    })
}

/// Which of a line's time references is the due date.
///
/// The **most specific one that is not in the past**: a clock reading beats the
/// day it hangs off ("Freitag um 18 Uhr" is a time, not a date), and a
/// reference that already resolved backwards is somebody talking about last
/// week rather than promising anything for it.
fn pick_due(refs: &[TimeRef], said_at_ns: i64) -> Option<&TimeRef> {
    let future: Vec<&TimeRef> = refs
        .iter()
        .filter(|r| r.resolved_utc_ns >= said_at_ns)
        .collect();
    // Later in the sentence wins the tie, which is where the anchoring rule in
    // `timeref` puts the more specific reading.
    future
        .iter()
        .copied()
        .rev()
        .max_by_key(|r| specificity(r.kind))
}

fn specificity(kind: &str) -> u8 {
    match kind {
        timeref::kind::CLOCK | timeref::kind::IN => 3,
        timeref::kind::DAY | timeref::kind::WEEKDAY => 2,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{SegmentAnalysis, commitment_state};

    fn fires(text: &str) -> bool {
        guess(text).is_some()
    }

    // ---- the pure rule -----------------------------------------------------
    //
    // The positives and the traps are lifted from the Tier 3 bake-off's gold
    // set (spike/graph_bench/cases.json) on purpose: the two tiers are answering
    // the same question, and the honest way to state what rules cost is to run
    // them over the cases a model was measured on.

    #[test]
    fn the_english_modals_fire() {
        for text in [
            "yeah I'll send it to you tonight",
            "I will drop them in your DMs on Friday",
            "sure, I will cut it and send it over",
            "next time she is online I will introduce you two",
            "I'm gonna fix the shader tomorrow",
        ] {
            assert!(fires(text), "{text:?} did not fire");
        }
    }

    #[test]
    fn the_german_modals_fire_in_both_word_orders() {
        for text in [
            "ja klar, ich schick dir morgen den Link",
            "ich mach die Doku bis Freitag fertig, versprochen",
            "wenn du Sonntag da bist zeig ich sie dir",
            "ich lad das nachher hoch",
            "mach ich, kein Problem",
            "ich werde das nochmal anschauen",
        ] {
            assert!(fires(text), "{text:?} did not fire");
        }
    }

    #[test]
    fn a_mixed_sentence_fires_on_whichever_half_carries_the_promise() {
        let g = guess("ich hab das mal gequickfixt, I will push the update tomorrow")
            .expect("the English half is a promise");
        assert_eq!(g.matched, "I will");
    }

    #[test]
    fn a_question_is_never_a_promise() {
        assert!(!fires("will you send me the link?"));
        assert!(!fires("schickst du mir das morgen?"));
        // …not even one that contains a first-person modal.
        assert!(!fires("should I send it to you tonight?"));
    }

    /// The bench's traps, as far as rules can reach them. Each of these is a
    /// line a naive modal match would file, and each refusal here costs nothing.
    #[test]
    fn the_cheap_traps_are_refused() {
        for text in [
            "I will probably just sleep after this",
            "I'll maybe do it later",
            "we should totally do a photo",
            "wir sollten das mal aufnehmen",
            "if I ever get round to it I'll send it",
            "wenn ich mal Zeit hab mach ich das",
            "ich hab dir das gestern geschickt",
            "I sent you the link yesterday",
        ] {
            assert!(!fires(text), "{text:?} should have been refused");
        }
    }

    /// The one the rules cannot see, stated in a test so nobody is surprised by
    /// it later. Tier 3 is the answer; the bake-off rejected 9/9 of these.
    #[test]
    fn in_game_banter_is_a_false_positive_the_rules_cannot_avoid() {
        assert!(
            fires("I will kill you next round"),
            "if this ever stops firing the rules got cleverer than they are documented to be"
        );
    }

    #[test]
    fn a_line_with_no_modal_at_all_is_nothing() {
        for text in [
            "wait, which portal was it",
            "the stairwell one, but it only opens after the lights go down",
            "",
            "   ",
        ] {
            assert!(!fires(text), "{text:?}");
        }
    }

    #[test]
    fn what_is_the_whole_line_and_is_bounded() {
        let g = guess("I'll send it tonight").expect("fires");
        assert_eq!(g.what, "I'll send it tonight", "tier 2 does not summarise");

        let long = format!("I'll {}", "send the very long thing ".repeat(40));
        let g = guess(&long).expect("fires");
        assert!(g.what.len() <= MAX_WHAT + 4, "{}", g.what.len());
        assert!(g.what.ends_with('…'));
    }

    // ---- due-date selection ------------------------------------------------

    #[test]
    fn the_most_specific_future_reference_wins() {
        let at = 1_788_000_000_000_000_000;
        let refs = timeref::extract("Freitag um 18 Uhr", at);
        assert_eq!(refs.len(), 2);
        let due = pick_due(&refs, at).expect("a due date");
        assert_eq!(due.kind, timeref::kind::CLOCK);
        assert_eq!(due.resolved_utc_ns, refs[1].resolved_utc_ns);
    }

    #[test]
    fn a_reference_that_resolved_into_the_past_is_not_a_due_date() {
        let at = 1_788_000_000_000_000_000;
        let past = [TimeRef {
            raw: "heute".into(),
            resolved_utc_ns: at - 3_600_000_000_000,
            kind: timeref::kind::DAY,
        }];
        assert!(pick_due(&past, at).is_none());
        assert!(pick_due(&[], at).is_none());
    }

    // ---- the pass, against a real database ---------------------------------

    struct Rig {
        store: Store,
        session: i64,
    }

    fn rig() -> Rig {
        let store = Store::open_in_memory().expect("an in-memory store");
        let src = store.upsert_source("VRChat.exe", "VRChat", 0).expect("src");
        let session = store.begin_session(src, 0).expect("session");
        Rig { store, session }
    }

    /// A turn, transcribed and labelled and threaded, exactly as the pipeline
    /// would have left it by the time Tier 2 runs.
    fn turn(rig: &Rig, at_s: i64, speaker: Option<i64>, text: &str) -> i64 {
        let t = at_s * 1_000_000_000;
        let id = rig
            .store
            .insert_segment(rig.session, t, t + 3_000_000_000, "", t)
            .expect("segment");
        rig.store
            .set_segment_analysis(
                id,
                &SegmentAnalysis {
                    text: Some(text.into()),
                    ..Default::default()
                },
            )
            .expect("analysis");
        if let Some(sp) = speaker {
            rig.store
                .set_segment_speaker(id, Some(sp), Some(0.7))
                .expect("speaker");
        }
        crate::threads::assign(&rig.store, &crate::config::GraphConfig::default(), id)
            .expect("threading");
        id
    }

    #[test]
    fn a_promise_between_two_voices_becomes_a_candidate() {
        let r = rig();
        let a = r.store.mint_speaker(0).expect("a");
        let b = r.store.mint_speaker(0).expect("b");
        // Two turns, so the thread has two voices in it.
        turn(
            &r,
            0,
            Some(a),
            "send me the link, I want to see the shaders",
        );
        let promise = turn(&r, 5, Some(b), "yeah I'll send it to you tonight");

        let out = extract(&r.store, promise, 99).expect("extract");
        assert!(out.commitment);
        assert_eq!(out.time_refs, 1, "tonight");

        let rows = r.store.commitments(None, 50).expect("list");
        assert_eq!(rows.len(), 1);
        let c = &rows[0];
        assert_eq!(c.segment_id, promise);
        assert_eq!(c.who_speaker_id, Some(b));
        assert_eq!(c.to_speaker_id, Some(a), "exactly one counterparty");
        assert_eq!(c.what, "yeah I'll send it to you tonight");
        assert_eq!(c.state, commitment_state::CANDIDATE);
        assert_eq!(c.source, commitment_source::RULES);
        assert_eq!(c.due_raw.as_deref(), Some("tonight"));
        assert!(c.due_utc_ns.is_some());
        // A rule match never sounds sure of itself.
        assert!(c.confidence.unwrap() < 0.5, "{:?}", c.confidence);
        assert_eq!(c.said.as_deref(), Some("yeah I'll send it to you tonight"));
    }

    #[test]
    fn a_promise_with_nobody_to_owe_it_to_is_not_a_commitment() {
        let r = rig();
        let a = r.store.mint_speaker(0).expect("a");
        // One voice, talking to nobody: a plan, not a promise.
        let alone = turn(&r, 0, Some(a), "ich mach das morgen fertig");
        let out = extract(&r.store, alone, 99).expect("extract");
        assert!(!out.commitment);
        assert_eq!(out.time_refs, 1, "the date is still worth recording");
        assert!(r.store.commitments(None, 50).expect("list").is_empty());
    }

    #[test]
    fn a_promise_from_an_unidentified_voice_is_not_a_commitment() {
        let r = rig();
        let a = r.store.mint_speaker(0).expect("a");
        turn(&r, 0, Some(a), "who is going to do it");
        let anon = turn(&r, 5, None, "I'll do it tomorrow");
        assert!(!extract(&r.store, anon, 99).expect("extract").commitment);
    }

    #[test]
    fn re_running_the_pass_neither_duplicates_nor_multiplies() {
        let r = rig();
        let a = r.store.mint_speaker(0).expect("a");
        let b = r.store.mint_speaker(0).expect("b");
        turn(&r, 0, Some(a), "can I get the recording?");
        let promise = turn(&r, 5, Some(b), "sure, I will cut it and send it over");

        for _ in 0..3 {
            extract(&r.store, promise, 99).expect("extract");
        }
        assert_eq!(r.store.commitments(None, 50).expect("list").len(), 1);
        assert_eq!(r.store.time_refs_for(promise).expect("refs").len(), 0);
    }

    #[test]
    fn a_persons_decision_survives_the_pass_running_again() {
        let r = rig();
        let a = r.store.mint_speaker(0).expect("a");
        let b = r.store.mint_speaker(0).expect("b");
        turn(&r, 0, Some(a), "the link?");
        let promise = turn(&r, 5, Some(b), "ich schick dir morgen den Link");
        extract(&r.store, promise, 99).expect("extract");

        let id = r.store.commitments(None, 50).expect("list")[0].id;
        r.store
            .set_commitment_state(id, commitment_state::DONE, 100)
            .expect("state");

        extract(&r.store, promise, 101).expect("extract again");
        let rows = r.store.commitments(None, 50).expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].state,
            commitment_state::DONE,
            "a background pass re-opened a decision a person made"
        );
    }

    /// Deletion means deletion, for what was inferred as much as for what was
    /// said (GRAPH.md's charter guard).
    #[test]
    fn deleting_the_segment_takes_the_commitment_and_the_dates_with_it() {
        let r = rig();
        let a = r.store.mint_speaker(0).expect("a");
        let b = r.store.mint_speaker(0).expect("b");
        turn(&r, 0, Some(a), "the recording?");
        let promise = turn(&r, 5, Some(b), "I will send it tomorrow");
        extract(&r.store, promise, 99).expect("extract");
        assert_eq!(r.store.commitments(None, 50).expect("list").len(), 1);
        assert_eq!(r.store.time_refs_for(promise).expect("refs").len(), 1);

        // Hidden first: a soft delete takes it out of every read path…
        r.store.soft_delete_segments(&[promise], 100).expect("soft");
        assert!(r.store.commitments(None, 50).expect("list").is_empty());
        assert_eq!(
            r.store.time_refs_for(promise).expect("refs").len(),
            1,
            "…but the rows are still there, because an undo has to bring them back"
        );

        // …and a purge takes the rows themselves.
        r.store.purge_segments(&[promise]).expect("purge");
        assert!(r.store.time_refs_for(promise).expect("refs").is_empty());
        assert_eq!(r.store.graph_counts(3).expect("counts").commitments, 0);
    }

    #[test]
    fn deleting_a_person_takes_what_they_were_owed_as_well_as_what_they_promised() {
        let r = rig();
        let a = r.store.mint_speaker(0).expect("a");
        let b = r.store.mint_speaker(0).expect("b");
        turn(&r, 0, Some(a), "the link, when you get a moment");
        let mine = turn(&r, 5, Some(b), "ich schick dir morgen den Link");
        extract(&r.store, mine, 99).expect("extract");

        let c = &r.store.commitments(None, 50).expect("list")[0];
        assert_eq!(c.who_speaker_id, Some(b));
        assert_eq!(c.to_speaker_id, Some(a));

        // Delete the person it was owed TO. The promise was made by somebody
        // else and its segment survives — and it still has to go.
        let report = r.store.delete_speaker(a, false, 200).expect("delete");
        assert_eq!(report.commitments, 1);
        assert!(r.store.commitments(None, 50).expect("list").is_empty());
    }

    #[test]
    fn clearing_the_derived_rows_leaves_the_transcript_alone() {
        let r = rig();
        let a = r.store.mint_speaker(0).expect("a");
        let b = r.store.mint_speaker(0).expect("b");
        turn(&r, 0, Some(a), "the link?");
        let promise = turn(&r, 5, Some(b), "I'll send it tomorrow");
        extract(&r.store, promise, 99).expect("extract");

        let (commitments, time_refs, _) = r.store.clear_derived().expect("clear");
        assert_eq!(commitments, 1);
        assert_eq!(time_refs, 1);
        assert_eq!(
            r.store.transcript(None, None).expect("transcript").len(),
            2,
            "the graph is an index, never the source of truth"
        );
    }
}
