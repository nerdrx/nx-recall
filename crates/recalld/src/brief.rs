//! `person.brief` — the thirty seconds before you say hello (PROTOCOL 0.8.0).
//!
//! Someone joins the world and the client can show a card: what you still owe
//! them, what they still owe you, when you last heard them, what the last few
//! conversations were about, and anything you told yourself to remember about
//! them. It is `person.get`'s question asked the other way round — that method
//! answers "who is this?", this one answers "what is outstanding?".
//!
//! Everything here is composition over queries that already exist. There is no
//! new derivation and no model: a brief is the memory graph, filtered to one
//! person and to the things that are still open. It says nothing about them
//! that is not already in the transcript, and the moment a segment is deleted
//! the brief that quoted it stops quoting it.
//!
//! ### Which direction is which
//!
//! The contract names the two lists from *your* side of the conversation:
//!
//! * `open_to_you` — what **they** said they would do. Rows whose `who` is this
//!   person. A commitment with no counterparty is included here: the extractor
//!   could not name who it was said to, and the person it was said to is
//!   overwhelmingly the person who was listening.
//! * `open_from_you` — what **you** said you would do for them. Rows whose
//!   `who` is your own pinned voice and whose `to` is this person.
//!
//! Open means `candidate` or `confirmed`. A `done` or `dismissed` row is a
//! decision somebody made and re-raising it would undo that decision.
//!
//! Asking for a brief of your own voice is legal and gives you both halves of
//! what you owe yourself; the daemon does not editorialise about it.

use anyhow::Result;
use serde_json::{Value, json};

use crate::clock::{iso8601, ns_to_ms};
use crate::store::Store;

/// How many rows each list of a brief carries. A brief is a card, not an
/// archive: past a handful nobody reads it before saying hello, and
/// `commitments.list` is the surface for the rest.
pub const LIST_LIMIT: usize = 20;
/// How many conversation labels a brief carries.
pub const TOPIC_LIMIT: usize = 8;

/// The whole reply, or `None` when there is no such voice.
pub fn brief(store: &Store, id: i64) -> Result<Option<Value>> {
    let Some(speaker) = store.speaker_summary(id)? else {
        return Ok(None);
    };
    let you = store.you_speaker_id()?;
    let totals = store.person_totals(id)?;

    let open = store.open_commitments_for(id, LIST_LIMIT * 4)?;
    let mut to_you = Vec::new();
    let mut from_you = Vec::new();
    for c in &open {
        if c.who_speaker_id == Some(id) {
            to_you.push(crate::service::commitment_json(c));
        } else if Some(id) == c.to_speaker_id && c.who_speaker_id == you {
            from_you.push(crate::service::commitment_json(c));
        }
    }
    to_you.truncate(LIST_LIMIT);
    from_you.truncate(LIST_LIMIT);

    let topics = store.person_recent_topics(id, TOPIC_LIMIT)?;
    // Notes are matched on the NAME, so an unnamed voice has none — there is
    // nothing a note could have called it that a person would have said.
    let notes = match speaker.name() {
        Some(name) => store.notes_mentioning(name, LIST_LIMIT)?,
        None => Vec::new(),
    };

    Ok(Some(json!({
        // The same speaker object `person.get` returns, so a client renders a
        // brief with the code it already has.
        "speaker": {
            "id": speaker.id,
            "you": Some(speaker.id) == you,
            "name": speaker.name(),
            "auto": speaker.auto_label,
            "languages": speaker.languages,
            "first_seen": iso8601(speaker.created_at),
        },
        "last_heard_ms": totals.last_ns.map(ns_to_ms),
        "last_heard_ns": totals.last_ns.map(|v| v.to_string()),
        "open_to_you": to_you,
        "open_from_you": from_you,
        "recent_topics": topics
            .iter()
            .map(|t| json!({
                "topic": t.topic,
                "thread_id": t.thread_id,
                "last_ms": ns_to_ms(t.last_ns),
                "last_ns": t.last_ns.to_string(),
            }))
            .collect::<Vec<_>>(),
        "notes_mentioning": notes
            .iter()
            .map(crate::notes::note_json)
            .collect::<Vec<_>>(),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{NewCommitment, SegmentAnalysis, commitment_source, commitment_state};

    struct Seed {
        store: Store,
        session: i64,
    }

    fn seed() -> Seed {
        let store = Store::open_in_memory().unwrap();
        let src = store.upsert_source("VRChat.exe", "VRChat.exe", 0).unwrap();
        let session = store.begin_session(src, 0).unwrap();
        Seed { store, session }
    }

    fn a_turn(s: &Seed, t: i64, speaker: Option<i64>, text: &str) -> i64 {
        let id = s
            .store
            .insert_segment(s.session, t, t + 1_000_000_000, "segments/a.wav", 0)
            .unwrap();
        s.store
            .set_segment_analysis(
                id,
                &SegmentAnalysis {
                    text: Some(text.into()),
                    ..Default::default()
                },
            )
            .unwrap();
        if let Some(spk) = speaker {
            s.store
                .set_segment_speaker(id, Some(spk), Some(0.9))
                .unwrap();
        }
        id
    }

    fn a_promise(s: &Seed, segment: i64, who: Option<i64>, to: Option<i64>, what: &str) -> i64 {
        s.store
            .upsert_commitment(
                &NewCommitment {
                    segment_id: segment,
                    thread_id: None,
                    who_speaker_id: who,
                    to_speaker_id: to,
                    what: what.into(),
                    due_utc_ns: None,
                    due_raw: None,
                    due_kind: None,
                    source: commitment_source::RULES,
                    model_id: None,
                    confidence: 0.5,
                },
                0,
            )
            .unwrap()
            .id()
    }

    /// You, and the person the brief is about.
    fn two(s: &Seed) -> (i64, i64) {
        let you = s.store.ensure_you_speaker(0).unwrap();
        let them = s.store.create_speaker("Aspen", 0).unwrap();
        s.store.rename_speaker(them, "Aspen", 1).unwrap();
        (you, them)
    }

    #[test]
    fn a_voice_that_is_not_there_has_no_brief() {
        let s = seed();
        assert!(brief(&s.store, 9999).unwrap().is_none());
    }

    #[test]
    fn both_directions_are_reported_and_only_the_open_ones() {
        let s = seed();
        let (you, them) = two(&s);

        let theirs = a_turn(&s, 1_000_000_000, Some(them), "i will send you the shader");
        a_promise(&s, theirs, Some(them), Some(you), "send the shader");

        let mine = a_turn(&s, 2_000_000_000, Some(you), "i will build the world");
        a_promise(&s, mine, Some(you), Some(them), "build the world");

        // One of each that is finished, and must not come back.
        let done = a_turn(&s, 3_000_000_000, Some(them), "i will fix the fountain");
        let done_id = a_promise(&s, done, Some(them), Some(you), "fix the fountain");
        s.store
            .set_commitment_state(done_id, commitment_state::DONE, 0)
            .unwrap();

        let b = brief(&s.store, them).unwrap().unwrap();
        let to_you = b["open_to_you"].as_array().unwrap();
        let from_you = b["open_from_you"].as_array().unwrap();
        assert_eq!(to_you.len(), 1, "what they owe you");
        assert_eq!(to_you[0]["what"], json!("send the shader"));
        assert_eq!(from_you.len(), 1, "what you owe them");
        assert_eq!(from_you[0]["what"], json!("build the world"));
        assert_eq!(b["speaker"]["name"], json!("Aspen"));
        assert_eq!(b["speaker"]["you"], json!(false));
        assert_eq!(b["last_heard_ms"], json!(ns_to_ms(4_000_000_000)));
    }

    #[test]
    fn a_promise_with_no_counterparty_counts_as_owed_to_you() {
        let s = seed();
        let (_, them) = two(&s);
        let seg = a_turn(&s, 1_000_000_000, Some(them), "i will look it up");
        a_promise(&s, seg, Some(them), None, "look it up");
        let b = brief(&s.store, them).unwrap().unwrap();
        assert_eq!(b["open_to_you"].as_array().unwrap().len(), 1);
        assert!(b["open_from_you"].as_array().unwrap().is_empty());
    }

    #[test]
    fn recent_topics_are_the_labels_of_their_conversations_each_once() {
        let s = seed();
        let (_, them) = two(&s);
        let mut threads = Vec::new();
        for (i, topic) in ["shaders", "shaders", "world building"].iter().enumerate() {
            // An hour apart, so each is its own conversation rather than one
            // conversation relabelled three times.
            let seg = a_turn(&s, (i as i64 + 1) * 3_600_000_000_000, Some(them), "words");
            crate::threads::assign(&s.store, &crate::config::GraphConfig::default(), seg).unwrap();
            let t = s
                .store
                .segment_row(seg)
                .unwrap()
                .unwrap()
                .thread_id
                .unwrap();
            s.store.set_thread_topic(t, Some(topic), "m@1", 0).unwrap();
            threads.push(t);
        }
        let b = brief(&s.store, them).unwrap().unwrap();
        let labels: Vec<&str> = b["recent_topics"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["topic"].as_str().unwrap())
            .collect();
        assert_eq!(
            labels,
            vec!["world building", "shaders"],
            "newest first, once each"
        );
    }

    #[test]
    fn notes_that_name_them_come_with_the_brief() {
        let s = seed();
        let (you, them) = two(&s);
        let seg = a_turn(
            &s,
            5_000_000_000,
            Some(you),
            "Recall, merk dir: Aspen den Shader fragen",
        );
        let note = crate::notes::detect("Recall, merk dir: Aspen den Shader fragen").unwrap();
        s.store.upsert_note(seg, &note, 0).unwrap();
        // And one that names somebody else.
        let other = a_turn(
            &s,
            6_000_000_000,
            Some(you),
            "Recall, note ask Kira about the world",
        );
        s.store
            .upsert_note(
                other,
                &crate::notes::detect("Recall, note ask Kira about the world").unwrap(),
                0,
            )
            .unwrap();

        let b = brief(&s.store, them).unwrap().unwrap();
        let notes = b["notes_mentioning"].as_array().unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0]["text"], json!("Aspen den Shader fragen"));

        // An unnamed voice has no name to be mentioned by, and gets none.
        let anon = s.store.mint_speaker(0).unwrap();
        let b = brief(&s.store, anon).unwrap().unwrap();
        assert!(b["notes_mentioning"].as_array().unwrap().is_empty());
        assert_eq!(b["speaker"]["name"], Value::Null);
    }
}
