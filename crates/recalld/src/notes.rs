//! Notes to self: the microphone as a notepad (PROTOCOL 0.8.0).
//!
//! You are in a world, your hands are on the controllers, and you want to
//! remember something. There is no keyboard. So you say it out loud with a word
//! in front of it — *"Recall, merk dir: den Shader von Aspen fragen"* — and the
//! daemon, which was already listening to your microphone and already
//! transcribing it, files that turn as a note.
//!
//! Three properties make this honest rather than creepy:
//!
//! * **Microphone only.** A wake phrase heard from an application's audio is
//!   somebody else's sentence, not your note. The hook in the pipeline is
//!   inside the `is_mic` branch and there is no other caller.
//! * **The turn stays in the transcript.** A note is an annotation referencing
//!   a segment, exactly like a commitment or a time reference — never a
//!   rewrite of one. Delete the segment and the note goes with it.
//! * **The phrase has to be first.** "I should recall that" is not a note, and
//!   neither is "recall the meeting" — the wake word alone means nothing
//!   without one of the four markers behind it.
//!
//! ### Tolerating the ASR
//!
//! The wake word is the one word in the sentence the transcript *must* get
//! right, and it is a proper noun the decoder has never been told about. On a
//! short mic turn it comes back as "Ricall", "Recoll", "Recal". So the match on
//! the wake word is an edit distance of one, not equality — the cheapest
//! possible allowance, and one that cannot reach any ordinary English or German
//! word (the nearest are "recalls" and "rectal", both two edits from at least
//! one variant a decoder actually produces). The markers are matched the same
//! way when they are long enough for it to be safe; "note" is four letters and
//! one edit from "not", so that one is exact.

use anyhow::Result;
use serde_json::{Value, json};
use tracing::warn;

use crate::bus::{Bus, Topic};
use crate::clock::ns_to_ms;
use crate::store::{NoteRow, Store};

/// The wake word, before the marker. Contract: `recall`.
const WAKE: &str = "recall";

/// What has to follow the wake word for a turn to be a note. Each is a
/// sequence of words, because the German one is two.
///
/// `merke dir` is not in the contract's list and is here anyway: it is what a
/// German speaker actually says, and the alternative is a feature that ignores
/// half the people who use it. It is an alias, not a fifth marker.
const MARKERS: &[&[&str]] = &[
    &["merk", "dir"],
    &["merke", "dir"],
    &["remember"],
    &["notiz"],
    &["note"],
];

/// Below this many characters a marker is matched exactly. "note" is one edit
/// from "not", "nope" and "node"; "remember" is one edit from nothing at all.
const FUZZY_FROM: usize = 5;

/// The wake phrase at the head of `text`, and the note it leaves behind.
///
/// `None` means this turn is not a note — which is the answer for almost every
/// turn, so it is the cheap path: one tokenisation and a look at the first
/// word.
pub fn detect(text: &str) -> Option<String> {
    let words = words(text);
    let first = words.first()?;
    if !within_one(&first.folded, WAKE) {
        return None;
    }
    let marker = MARKERS.iter().find(|m| {
        m.len() < words.len()
            && m.iter()
                .zip(&words[1..])
                .all(|(want, got)| marker_matches(&got.folded, want))
    })?;
    // Everything after the marker, with the punctuation that separated them
    // trimmed off. The original casing survives: it is the user's sentence.
    let rest = text[words[marker.len()].end..].trim_matches(|c: char| {
        c.is_whitespace() || matches!(c, ',' | ':' | '-' | '–' | '—' | '.' | ';')
    });
    // "Recall, remember." remembers nothing. A note with no text would be a
    // row in a list that says only that you once said a wake word.
    (!rest.is_empty()).then(|| rest.to_string())
}

/// The pipeline's whole share of this feature: one call, per stored turn.
///
/// `is_mic` is passed rather than derived so the guard is visible at the call
/// site, and every failure is logged and swallowed — a note is an annotation
/// and an annotation never costs a recording. The text is read back from the
/// row rather than taken as an argument, for the same reason
/// [`crate::pipeline::publish_segment`] reads it back: the row is what a client
/// will query, and the note must be made of the same words.
pub fn maybe_capture(store: &Store, bus: &Bus, segment_id: i64, is_mic: bool, at_utc_ns: i64) {
    if !is_mic {
        return;
    }
    match capture(store, segment_id, at_utc_ns) {
        Ok(Some(note)) => {
            bus.publish(Topic::Segments, "note", note_json(&note));
        }
        Ok(None) => {}
        Err(e) => warn!(segment_id, "could not file a note to self: {e:#}"),
    }
}

/// The fallible half, so the pipeline's call site has no `?` in it.
fn capture(store: &Store, segment_id: i64, at_utc_ns: i64) -> Result<Option<NoteRow>> {
    let Some(row) = store.segment_row(segment_id)? else {
        return Ok(None);
    };
    let Some(text) = row.text.as_deref() else {
        return Ok(None);
    };
    let Some(note) = detect(text) else {
        return Ok(None);
    };
    // A re-decode or a correction can bring the same turn back through here.
    // One note per turn: the second pass updates the words rather than filing
    // a duplicate the user has to dismiss twice.
    store.upsert_note(segment_id, &note, at_utc_ns)
}

/// One note on the wire — `notes.list`, and the body of a `note` event, from
/// one function so the two cannot drift.
pub fn note_json(note: &NoteRow) -> Value {
    json!({
        "id": note.id,
        "segment_id": note.segment_id,
        "text": note.text,
        // *When it was said*, not when the row was written: a note is a moment
        // in the transcript, and the transcript is where a client will want to
        // put it. Both forms, as everywhere else (`t_ns` is a string).
        "t_ms": ns_to_ms(note.t_start_ns),
        "t_ns": note.t_start_ns.to_string(),
        "state": note.state,
        "created_ms": ns_to_ms(note.created_utc_ns),
    })
}

// ---------------------------------------------------------------------------
// matching
// ---------------------------------------------------------------------------

struct Word {
    end: usize,
    folded: String,
}

/// Words with their end offsets in the original string, so the note text can
/// be cut out of it with its casing and its punctuation intact.
fn words(text: &str) -> Vec<Word> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, ch) in text.char_indices() {
        match (
            ch.is_alphanumeric() || ch == '\'' || ch == '\u{2019}',
            start,
        ) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                out.push(word(text, s, i));
                start = None;
            }
            _ => {}
        }
        // Only the head of the sentence is ever inspected: the wake word plus
        // the longest marker is three words.
        if out.len() > 3 {
            return out;
        }
    }
    if let Some(s) = start {
        out.push(word(text, s, text.len()));
    }
    out
}

fn word(text: &str, start: usize, end: usize) -> Word {
    Word {
        end,
        folded: crate::ask::fold_word(&text[start..end]),
    }
}

fn marker_matches(got: &str, want: &str) -> bool {
    got == want || (want.len() >= FUZZY_FROM && within_one(got, want))
}

/// At most one insertion, deletion or substitution apart.
fn within_one(a: &str, b: &str) -> bool {
    crate::ask::within_one(a, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped detection table. Every positive is a note and every
    /// negative is an ordinary turn that must stay one.
    #[test]
    fn the_detection_table() {
        let cases: &[(&str, Option<&str>)] = &[
            // --- the four contract phrases ------------------------------
            (
                "Recall, merk dir: den Shader von Aspen fragen",
                Some("den Shader von Aspen fragen"),
            ),
            (
                "Recall, remember to export the avatar",
                Some("to export the avatar"),
            ),
            (
                "Recall, notiz: Weltbau am Freitag",
                Some("Weltbau am Freitag"),
            ),
            (
                "Recall, note the fountain is broken",
                Some("the fountain is broken"),
            ),
            // --- casing and punctuation ---------------------------------
            ("recall remember the shader", Some("the shader")),
            ("RECALL, REMEMBER THE SHADER", Some("THE SHADER")),
            ("Recall — merke dir den Termin", Some("den Termin")),
            ("  Recall, notiz - Portal", Some("Portal")),
            // --- what the decoder actually hands us ---------------------
            ("Ricall, merk dir den Termin", Some("den Termin")),
            ("Recoll, remember the shader", Some("the shader")),
            ("Recal, note the fountain", Some("the fountain")),
            ("Recalls, remember the shader", Some("the shader")),
            ("Recall, rememberr the shader", Some("the shader")),
            // --- negatives ----------------------------------------------
            ("I should recall that meeting", None),
            ("recall the meeting", None),
            ("do you recall, remember that night", None),
            ("Recall", None),
            ("Recall, remember", None),
            ("Recall, remember.", None),
            ("Rectal, remember the shader", None),
            // "note" is four letters: one edit is too generous for it.
            ("Recall, not the fountain", None),
            ("", None),
        ];
        for (text, want) in cases {
            assert_eq!(detect(text).as_deref(), *want, "detecting in {text:?}");
        }
    }

    // ---- the pipeline's one call ---------------------------------------

    struct Rig {
        store: Store,
        bus: std::sync::Arc<Bus>,
        rx: std::sync::mpsc::Receiver<std::sync::Arc<Vec<u8>>>,
        session: i64,
    }

    fn rig() -> Rig {
        let store = Store::open_in_memory().unwrap();
        let src = store.upsert_source("mic", "Microphone", 0).unwrap();
        let session = store.begin_session(src, 0).unwrap();
        let bus = Bus::new(64, 32);
        let (client, rx) = bus.attach(None);
        client.subscribe(&Topic::ALL);
        Rig {
            store,
            bus,
            rx,
            session,
        }
    }

    fn a_turn(r: &Rig, text: &str) -> i64 {
        let id = r
            .store
            .insert_segment(r.session, 1_000, 2_000, "segments/a.wav", 0)
            .unwrap();
        r.store
            .set_segment_analysis(
                id,
                &crate::store::SegmentAnalysis {
                    text: Some(text.into()),
                    ..Default::default()
                },
            )
            .unwrap();
        id
    }

    fn events(r: &Rig) -> Vec<Value> {
        std::iter::from_fn(|| r.rx.try_recv().ok())
            .map(|b| serde_json::from_slice(&b).unwrap())
            .collect()
    }

    #[test]
    fn a_wake_phrase_from_an_application_is_somebody_elses_sentence() {
        let r = rig();
        let seg = a_turn(&r, "Recall, remember the shader");
        maybe_capture(&r.store, &r.bus, seg, false, 0);
        assert!(r.store.notes(None, 10).unwrap().is_empty());
        assert!(events(&r).is_empty());
        // The same turn, heard on the microphone, IS a note.
        maybe_capture(&r.store, &r.bus, seg, true, 0);
        assert_eq!(r.store.notes(None, 10).unwrap().len(), 1);
        assert_eq!(events(&r).len(), 1);
    }

    #[test]
    fn the_same_turn_coming_back_does_not_file_a_second_note() {
        let r = rig();
        let seg = a_turn(&r, "Recall, remember the shader");
        maybe_capture(&r.store, &r.bus, seg, true, 0);
        let id = r.store.notes(None, 10).unwrap()[0].id;
        // A person dismisses it, and then a re-decode brings the same words
        // back through the pipeline. The dismissal has to stand.
        r.store
            .set_note_state(id, crate::store::note_state::DISMISSED)
            .unwrap();
        let _ = events(&r);
        maybe_capture(&r.store, &r.bus, seg, true, 0);
        let notes = r.store.notes(None, 10).unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].state, "dismissed");
        assert!(events(&r).is_empty(), "and nothing is re-announced");
    }

    #[test]
    fn a_re_decode_that_changed_the_words_updates_the_note_it_already_filed() {
        let r = rig();
        let seg = a_turn(&r, "Recall, remember the shader");
        maybe_capture(&r.store, &r.bus, seg, true, 0);
        let _ = events(&r);
        r.store
            .correct_segment_text(seg, "Recall, remember the shader compile flags")
            .unwrap();
        maybe_capture(&r.store, &r.bus, seg, true, 0);
        let notes = r.store.notes(None, 10).unwrap();
        assert_eq!(notes.len(), 1, "one turn is one note");
        assert_eq!(notes[0].text, "the shader compile flags");
        assert_eq!(events(&r).len(), 1, "and the change is broadcast");
    }

    #[test]
    fn an_ordinary_mic_turn_files_nothing() {
        let r = rig();
        let seg = a_turn(&r, "i should recall that meeting");
        maybe_capture(&r.store, &r.bus, seg, true, 0);
        assert!(r.store.notes(None, 10).unwrap().is_empty());
        assert!(events(&r).is_empty());
    }

    #[test]
    fn the_wake_phrase_is_stripped_and_nothing_else_is() {
        // The note keeps its own punctuation — only the separator goes.
        assert_eq!(
            detect("Recall, merk dir: fragen, ob das Portal noch steht?").as_deref(),
            Some("fragen, ob das Portal noch steht?")
        );
    }
}
