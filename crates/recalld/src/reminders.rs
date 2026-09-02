//! Reminders that fire (PROTOCOL 0.9.0).
//!
//! 0.8.0's Memory view carries a sentence near the top of the commitments card:
//!
//! > *"Nothing here reminds you, notifies you, or acts on its own."*
//!
//! That sentence is still true of commitments, and it is exactly why this
//! module is about **notes** and nothing else. A commitment is something the
//! daemon *inferred* somebody promised. A note is a sentence you said into your
//! own microphone, beginning with a wake word, on purpose. The difference is
//! consent: nagging you about a promise a regular expression thought it heard
//! is the failure mode that whole card is built to avoid, and bringing back
//! something you explicitly asked to be brought back is the thing you asked
//! for.
//!
//! So the rule is narrow and it is stated in one line: **a note fires if, and
//! only if, the words after the wake phrase contain a time reference that was
//! still in the future when you said them.**
//!
//! ## No model, and no second clock
//!
//! The due date comes from [`crate::timeref`] — the Tier 2 rule parser, in its
//! forward reading, resolved against the segment's own capture time, exactly as
//! a commitment's due date is. "erinner mich morgen um zehn an den Link" said
//! on Wednesday evening is Thursday 10:00 because that is what those words
//! meant on Wednesday evening, and nothing later re-resolves it. There is no
//! model here and no new parser; a reminder is a note plus a date the daemon
//! could already read.
//!
//! ## Which reference, when a sentence has two
//!
//! [`due_for`] takes the **earliest reference still in the future**, with one
//! exception: a clock reading that refines the day before it ("morgen" then "um
//! zehn") replaces it rather than losing to it, because those are one date said
//! twice and `timeref`'s anchor rule has already resolved the second onto the
//! first. Two genuinely independent references ("morgen den Link, nächste Woche
//! anrufen") keep the earlier: the first deadline is the one a reminder is for.
//!
//! A reference that resolves into the past gives no due date at all. That is
//! not a rejection of the sentence — "heute" said at 19:30 resolves to local
//! midnight, which has gone — it is the honest reading of a note about
//! something that was already happening.
//!
//! ## Firing once
//!
//! `notes.fired_at_ns` is the whole state machine. The scheduler asks for open,
//! dated, unfired notes with `due_ns <= now`, and the write that marks one
//! fired is `UPDATE ... WHERE fired_at_ns IS NULL`, so the row itself decides
//! whether this tick was the one that announced it. A second tick, a second
//! daemon, or a tick that overlaps its predecessor cannot double-announce.
//!
//! Two things put a note back on the list, and both are deliberate:
//!
//! * **A snooze** ([`Store::snooze_note`]) — the person said "not now".
//! * **A re-decode that changed the words** ([`Store::upsert_note`]) — the
//!   sentence is different, so the date it implied is different, and a reminder
//!   that fired for the sentence before has not fired for this one.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use serde_json::{Value, json};
use tracing::{debug, warn};

use crate::bus::{Bus, Topic};
use crate::clock::{ns_to_ms, utc_now_ns};
use crate::config::AssistConfig;
use crate::store::{NoteRow, Store};
use crate::timeref::{self, kind};

/// The due date a note's words imply, or `None`.
///
/// `at_utc_ns` is the moment the words were spoken — the segment's start — and
/// every answer is relative to it. Pure, so the whole de/en table below is a
/// test and not a thing that needs a database.
pub fn due_for(text: &str, at_utc_ns: i64) -> Option<i64> {
    let mut best: Option<(i64, &'static str)> = None;
    for r in timeref::extract(text, at_utc_ns) {
        // A reference that had already passed when it was said is not a
        // reminder. This is the "heute" case, and it is common.
        if r.resolved_utc_ns <= at_utc_ns {
            continue;
        }
        match best {
            None => best = Some((r.resolved_utc_ns, r.kind)),
            // A clock reading on the same local day as the reference before it
            // is that reference, said again with an hour on it. `timeref`'s
            // anchor rule has already put it on the right day; all this has to
            // do is prefer the more precise of the two.
            Some((have, _))
                if r.kind == kind::CLOCK
                    && timeref::local_date(r.resolved_utc_ns) == timeref::local_date(have)
                    && r.resolved_utc_ns >= have =>
            {
                best = Some((r.resolved_utc_ns, r.kind));
            }
            // A second, independent date. The first deadline wins.
            Some(_) => {}
        }
    }
    best.map(|(ns, _)| ns)
}

/// One `reminder` event. Deliberately small: the note itself is already on the
/// wire through `notes.list` and the `note` event, and a client that gets this
/// has everything it needs to raise a notification and open the row.
pub fn reminder_json(note: &NoteRow, due_ns: i64) -> Value {
    json!({
        "note_id": note.id,
        "text": note.text,
        "due_ms": ns_to_ms(due_ns),
        // Both forms, as everywhere else — `_ns` is a string because 1.8e18
        // does not survive a JSON number in a browser.
        "due_ns": due_ns.to_string(),
        // So a click can land on the turn it was said in without a second
        // round trip.
        "segment_id": note.segment_id,
        "t_ms": ns_to_ms(note.t_start_ns),
    })
}

/// One pass. Returns the notes it announced.
///
/// Public and synchronous so the scheduler is testable by calling it rather
/// than by starting a thread and sleeping: a background loop that can only be
/// observed by waiting is a background loop nobody tests.
pub fn tick(store: &Store, bus: &Bus, now_utc_ns: i64, limit: usize) -> Result<Vec<NoteRow>> {
    let due = store.notes_due(now_utc_ns, limit.max(1))?;
    let mut fired = Vec::new();
    for note in due {
        let Some(due_ns) = note.due_ns else { continue };
        // The row decides. `mark_note_fired` only matches a note that is still
        // unfired, so two overlapping ticks announce it exactly once between
        // them rather than once each.
        if !store.mark_note_fired(note.id, now_utc_ns)? {
            continue;
        }
        bus.publish(Topic::Segments, "reminder", reminder_json(&note, due_ns));
        // …and the note itself, so a list already on screen picks up `fired`
        // without re-querying. The reminder is the alarm; this is the row.
        bus.publish(Topic::Segments, "note", crate::notes::note_json(&note));
        fired.push(note);
    }
    Ok(fired)
}

#[derive(Default)]
pub struct ReminderStop(AtomicBool);

impl ReminderStop {
    pub fn stop(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn stopped(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// The scheduler thread. One query every `reminder_tick_s`, no model, no audio.
///
/// It does **not** stand down while capture is paused, and that is the one
/// place this module differs from every other background worker in the daemon.
/// Pause means nothing new is written down; a reminder writes nothing about
/// what is being said, it delivers something the user already said. Silencing
/// it during a pause would mean the panic button also swallowed the thing you
/// asked to be told.
pub fn run(
    store: Arc<std::sync::Mutex<Store>>,
    bus: Arc<Bus>,
    cfg: AssistConfig,
    stats: Arc<crate::assist::AssistStats>,
    stop: Arc<ReminderStop>,
) {
    let tick_s = cfg.reminder_tick_s.clamp(1, 3600);
    let step = Duration::from_millis(200);
    loop {
        if stop.stopped() {
            debug!("the reminder scheduler stopped");
            return;
        }
        if cfg.reminders {
            let now = utc_now_ns();
            let guard = store.lock().unwrap_or_else(|p| p.into_inner());
            match tick(&guard, &bus, now, cfg.reminder_batch) {
                Ok(fired) if !fired.is_empty() => {
                    stats
                        .reminders_fired
                        .fetch_add(fired.len() as u64, Ordering::Relaxed);
                    debug!(n = fired.len(), "reminders announced");
                }
                Ok(_) => {}
                Err(e) => warn!("a reminder tick failed: {e:#}"),
            }
        }
        let pause = Duration::from_secs(tick_s);
        let mut slept = Duration::ZERO;
        while slept < pause {
            if stop.stopped() {
                break;
            }
            std::thread::sleep(step);
            slept += step;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{days_from_civil, local_offset_s};
    use crate::store::note_state;

    const SEC: i64 = 1_000_000_000;
    const MIN: i64 = 60 * SEC;

    /// The same arithmetic `timeref`'s tests do, so nothing here is quietly
    /// asserting the test machine's timezone.
    fn local_instant(y: i64, m: u32, d: u32, hour: i64, minute: i64) -> i64 {
        let naive = (days_from_civil(y, m, d) * 86_400 + hour * 3600 + minute * 60) * SEC;
        naive - local_offset_s(naive) * SEC
    }

    /// A Wednesday, 2026-09-02, at 19:30 local — `timeref`'s own fixture.
    fn wednesday_evening() -> i64 {
        local_instant(2026, 9, 2, 19, 30)
    }

    // ---- the table ---------------------------------------------------------

    /// Every shape a note can carry a date in, and every shape that must not
    /// produce one. The two contract sentences are the first two rows.
    #[test]
    fn the_timeref_to_due_table() {
        let at = wednesday_evening();
        /// `(year, month, day, hour, minute)` in local time — the shape a
        /// person reads a due date in, which is what makes this table
        /// checkable by eye.
        type When = (i64, u32, u32, i64, i64);
        let cases: &[(&str, Option<When>)] = &[
            // ---- German, the contract sentence -------------------------
            (
                "erinner mich morgen um zehn an den Link",
                Some((2026, 9, 3, 10, 0)),
            ),
            // ---- English, the contract sentence ------------------------
            (
                "remind me at 8 pm to send the recording",
                Some((2026, 9, 2, 20, 0)),
            ),
            // ---- relative --------------------------------------------------
            (
                "in 20 Minuten den Ofen ausmachen",
                Some((2026, 9, 2, 19, 50)),
            ),
            ("in 2 hours, check the render", Some((2026, 9, 2, 21, 30))),
            ("in 3 Tagen nachfragen", Some((2026, 9, 5, 19, 30))),
            // ---- absolute days --------------------------------------------
            ("morgen den Shader fragen", Some((2026, 9, 3, 0, 0))),
            ("übermorgen die Doku anfangen", Some((2026, 9, 4, 0, 0))),
            ("heute Abend den Link schicken", Some((2026, 9, 2, 20, 0))),
            ("morgen früh den Export starten", Some((2026, 9, 3, 8, 0))),
            ("Freitag die Doku abgeben", Some((2026, 9, 4, 0, 0))),
            ("on Friday, send Aspen the file", Some((2026, 9, 4, 0, 0))),
            ("nächste Woche das Portal bauen", Some((2026, 9, 7, 0, 0))),
            ("am Wochenende die Welt hochladen", Some((2026, 9, 5, 0, 0))),
            // ---- absolute clocks -------------------------------------------
            ("um 21:15 den Stream starten", Some((2026, 9, 2, 21, 15))),
            // 18:00 has passed at 19:30, so it is tomorrow's — `timeref`'s
            // rule, unchanged, and the right one for a reminder.
            ("um 18 Uhr anrufen", Some((2026, 9, 3, 18, 0))),
            // ---- the anchor: one date said twice ---------------------------
            ("Freitag um 18 Uhr anrufen", Some((2026, 9, 4, 18, 0))),
            ("Friday at 9, the world tour", Some((2026, 9, 4, 9, 0))),
            // ---- two independent dates: the first deadline wins ------------
            (
                "morgen den Link, nächste Woche anrufen",
                Some((2026, 9, 3, 0, 0)),
            ),
            // ---- no due ----------------------------------------------------
            // A reference that had already passed when it was said. "heute"
            // is local midnight and this note was dictated at half past seven
            // in the evening.
            ("heute noch den Shader fixen", None),
            ("den Shader von Aspen fragen", None),
            ("ask Kira about the world", None),
            ("", None),
            // A number that is not a time, and a greeting that is not a day.
            ("there were 20 people in there", None),
            ("guten Morgen allerseits", None),
        ];
        for (text, want) in cases {
            let got = due_for(text, at);
            match (got, want) {
                (Some(ns), Some((y, m, d, h, mi))) => {
                    assert_eq!(
                        (timeref::local_date(ns), timeref::local_time(ns)),
                        ((*y, *m, *d), (*h, *mi)),
                        "{text:?} resolved to the wrong instant"
                    );
                    assert!(ns > at, "{text:?} produced a due date in the past");
                }
                (None, None) => {}
                _ => panic!("{text:?}: got {got:?}, wanted {want:?}"),
            }
        }
    }

    // ---- the scheduler -----------------------------------------------------

    struct Rig {
        store: Store,
        bus: Arc<Bus>,
        rx: std::sync::mpsc::Receiver<Arc<Vec<u8>>>,
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

    /// A note dictated at `at`, due at `due`.
    fn a_note(r: &Rig, at: i64, text: &str, due: Option<i64>) -> i64 {
        let seg = r
            .store
            .insert_segment(r.session, at, at + SEC, "segments/a.wav", at)
            .unwrap();
        r.store
            .set_segment_analysis(
                seg,
                &crate::store::SegmentAnalysis {
                    text: Some(format!("Recall, merk dir: {text}")),
                    ..Default::default()
                },
            )
            .unwrap();
        r.store.upsert_note(seg, text, due, at).unwrap().unwrap().id
    }

    fn events(r: &Rig) -> Vec<Value> {
        std::iter::from_fn(|| r.rx.try_recv().ok())
            .map(|b| serde_json::from_slice(&b).unwrap())
            .collect()
    }

    fn reminders(r: &Rig) -> Vec<Value> {
        events(r)
            .into_iter()
            .filter(|e| e["ev"] == json!("reminder"))
            .collect()
    }

    #[test]
    fn a_note_fires_once_and_never_twice() {
        let r = rig();
        let at = wednesday_evening();
        let due = at + 10 * MIN;
        let id = a_note(&r, at, "den Ofen ausmachen", Some(due));

        // Before it is due: nothing, however often the scheduler looks.
        assert!(tick(&r.store, &r.bus, due - SEC, 20).unwrap().is_empty());
        assert!(tick(&r.store, &r.bus, due - SEC, 20).unwrap().is_empty());
        assert!(reminders(&r).is_empty());

        // At the moment it is due.
        let fired = tick(&r.store, &r.bus, due, 20).unwrap();
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].id, id);
        let evs = reminders(&r);
        assert_eq!(evs.len(), 1, "one reminder");
        assert_eq!(evs[0]["data"]["note_id"], json!(id));
        assert_eq!(evs[0]["data"]["text"], json!("den Ofen ausmachen"));
        assert_eq!(evs[0]["data"]["due_ms"], json!(ns_to_ms(due)));

        // And never again, however long the daemon runs.
        for later in [due, due + MIN, due + 3600 * SEC] {
            assert!(
                tick(&r.store, &r.bus, later, 20).unwrap().is_empty(),
                "a note fired twice"
            );
        }
        assert!(reminders(&r).is_empty());
        assert!(r.store.note(id).unwrap().unwrap().fired_at_ns.is_some());
    }

    #[test]
    fn a_note_with_no_date_never_fires_and_a_settled_one_stops() {
        let r = rig();
        let at = wednesday_evening();
        a_note(&r, at, "den Shader von Aspen fragen", None);
        let dated = a_note(&r, at + SEC, "morgen anrufen", Some(at + MIN));
        // Dismissed before it came round: a decision somebody made, and the
        // scheduler does not undo decisions.
        r.store
            .set_note_state(dated, note_state::DISMISSED)
            .unwrap();
        assert!(
            tick(&r.store, &r.bus, at + 3600 * SEC, 20)
                .unwrap()
                .is_empty(),
            "an undated or settled note fired"
        );
    }

    #[test]
    fn a_snooze_puts_a_fired_note_back_on_the_list() {
        let r = rig();
        let at = wednesday_evening();
        let due = at + MIN;
        let id = a_note(&r, at, "den Ofen ausmachen", Some(due));
        assert_eq!(tick(&r.store, &r.bus, due, 20).unwrap().len(), 1);
        let _ = events(&r);

        // "Not now, in ten minutes."
        let snoozed = r.store.snooze_note(id, 10, due).unwrap().unwrap();
        assert_eq!(snoozed.due_ns, Some(due + 10 * MIN));
        assert_eq!(snoozed.fired_at_ns, None, "a snooze un-fires it");
        assert_eq!(snoozed.state, note_state::OPEN);

        // Still nothing for nine minutes, then exactly one more reminder.
        assert!(
            tick(&r.store, &r.bus, due + 9 * MIN, 20)
                .unwrap()
                .is_empty()
        );
        assert_eq!(tick(&r.store, &r.bus, due + 10 * MIN, 20).unwrap().len(), 1);
        assert_eq!(tick(&r.store, &r.bus, due + 20 * MIN, 20).unwrap().len(), 0);
    }

    #[test]
    fn a_snooze_gives_a_dateless_note_a_date() {
        // The only way to ask to be reminded of something you said without a
        // time in it. Without this, "remind me about this later" would be a
        // button that does nothing on three notes out of four.
        let r = rig();
        let at = wednesday_evening();
        let id = a_note(&r, at, "den Shader von Aspen fragen", None);
        assert_eq!(r.store.note(id).unwrap().unwrap().due_ns, None);
        let row = r.store.snooze_note(id, 30, at).unwrap().unwrap();
        assert_eq!(row.due_ns, Some(at + 30 * MIN));
        assert_eq!(tick(&r.store, &r.bus, at + 30 * MIN, 20).unwrap().len(), 1);
    }

    #[test]
    fn a_backlog_comes_back_oldest_first_and_bounded() {
        let r = rig();
        let at = wednesday_evening();
        // The daemon was off overnight and five reminders came due.
        for i in 0..5 {
            a_note(
                &r,
                at + i * SEC,
                &format!("thing {i}"),
                Some(at + (i + 1) * MIN),
            );
        }
        let first = tick(&r.store, &r.bus, at + 3600 * SEC, 2).unwrap();
        assert_eq!(first.len(), 2, "the limit is a limit");
        assert_eq!(first[0].text, "thing 0", "oldest first");
        assert_eq!(first[1].text, "thing 1");
        let rest = tick(&r.store, &r.bus, at + 3600 * SEC, 20).unwrap();
        assert_eq!(rest.len(), 3);
        assert!(
            tick(&r.store, &r.bus, at + 3600 * SEC, 20)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_re_decode_that_moved_the_date_puts_the_note_back() {
        // The words are the date. A pass that re-read "morgen" as "heute" has
        // changed when this note is for, and a firing that happened for the
        // sentence before did not happen for this one.
        let r = rig();
        let at = wednesday_evening();
        let due = at + MIN;
        let id = a_note(&r, at, "morgen anrufen", Some(due));
        assert_eq!(tick(&r.store, &r.bus, due, 20).unwrap().len(), 1);
        let _ = events(&r);

        let moved = at + 5 * MIN;
        let row = r
            .store
            .upsert_note(
                r.store.note(id).unwrap().unwrap().segment_id,
                "gleich anrufen",
                Some(moved),
                at,
            )
            .unwrap()
            .expect("the words changed, so the note is re-announced");
        assert_eq!(row.due_ns, Some(moved));
        assert_eq!(row.fired_at_ns, None);
        assert_eq!(tick(&r.store, &r.bus, moved, 20).unwrap().len(), 1);
    }

    #[test]
    fn the_capture_hook_writes_the_due_date_it_read() {
        // End to end through the pipeline's one call: a dictated sentence with
        // a time in it comes out of `notes.list` with a due date on it.
        let r = rig();
        let at = wednesday_evening();
        let seg = r
            .store
            .insert_segment(r.session, at, at + SEC, "segments/a.wav", at)
            .unwrap();
        r.store
            .set_segment_analysis(
                seg,
                &crate::store::SegmentAnalysis {
                    text: Some("Recall, erinner mich morgen um zehn an den Link".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        crate::notes::maybe_capture(&r.store, &r.bus, seg, true, at);
        let notes = r.store.notes(None, 10).unwrap();
        assert_eq!(notes.len(), 1);
        let due = notes[0].due_ns.expect("a due date was read from the words");
        assert_eq!(timeref::local_date(due), (2026, 9, 3));
        assert_eq!(timeref::local_time(due), (10, 0));
    }
}
