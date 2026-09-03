//! `replay.get` — the thin query behind conversation replay (0.9.2).
//!
//! Replay plays a conversation's turns in order with the transcript following
//! along. To draw a scrubber over that conversation the client needs, in one
//! round trip and *before* it fetches a single byte of audio: how many turns
//! there are, when each one starts, how long it runs, who said it, and
//! **whether it can be heard at all**. The audio itself still comes from
//! `segments.audio`, one turn at a time, prefetching the next — a conversation
//! is minutes of WAV and nobody needs all of it to press play.
//!
//! ### Why this is not `thread.get`
//!
//! `thread.get` already returns the whole conversation in the ordinary segment
//! shape, and that shape already carries `has_audio`. It was the obvious thing
//! to reuse and it is the wrong thing to reuse, for one reason:
//! `segment_json`'s `has_audio` is `audio_path != ''` — *the database still
//! names a file* — and that is not the same claim as *the file is there*.
//! [`crate::retention`]'s reconcile pass counts exactly this case (its
//! `dangling_paths`) and deliberately leaves the row alone: "segment audio is
//! missing; the transcript is kept and the row left alone". So a scrubber built
//! on that flag draws a playable tick over a turn that answers `err:gone` the
//! instant it is asked for, and the playhead stutters on a turn the UI promised
//! would sound.
//!
//! This method stats the file. One `stat` per turn over a conversation that is
//! tens of turns long is nothing next to the round trip it saves, and it is the
//! difference between a scrubber that is right and one that is usually right.
//!
//! It stays a *snapshot* either way, and the client is not allowed to trust it
//! as more than one: retention can sweep between this reply and the fetch, so
//! `err:gone` from `segments.audio` remains a normal answer that the player
//! handles by skipping the turn — `has_audio` only decides what the scrubber
//! draws before anybody presses anything.

use std::path::Path;

use serde_json::{Value, json};

use crate::clock::ns_to_ms;
use crate::proto::{Error, Request};
use crate::store::Store;

/// `replay.get {thread}`, off the socket. The conversation is named `thread`
/// rather than `id` because that is what it is — the reply is not about a row.
pub fn get(store: &Store, data_dir: &Path, req: &Request) -> Result<Value, Error> {
    turns(store, data_dir, req.i64("thread")?)
}

/// The turns of one conversation, in time order, each saying whether it can be
/// heard. See the module docs for why this does not reuse `thread.get`.
///
/// Unknown id — and a conversation whose every turn has been deleted, which is
/// the same thing to a client holding a link to it — is `err:not_found`, the
/// same answer `thread.get` gives.
pub fn turns(store: &Store, data_dir: &Path, thread: i64) -> Result<Value, Error> {
    let rows = store.thread_rows(thread).map_err(Error::from)?;
    if rows.is_empty() {
        return Err(Error::not_found(format!(
            "no conversation with id {thread}, or nothing is left of it"
        )));
    }
    let turns = rows
        .iter()
        .map(|row| {
            json!({
                "id": row.id,
                // The `_ms` twin is what a scrubber does arithmetic on; the
                // nanosecond value is a string, like every one on the wire.
                "t_ms": ns_to_ms(row.t_start_ns),
                "t_ns": row.t_start_ns.to_string(),
                "dur_ms": ns_to_ms(row.t_end_ns - row.t_start_ns),
                // The speaker *id*, as everywhere else in this protocol: names
                // change and ids do not, so a relabel repaints the player bar
                // without re-querying it. `speaker_name` is the convenience,
                // resolved through merges, and `null` on an unlabelled turn.
                "speaker": row.speaker_id,
                "speaker_name": row.speaker_name,
                // The highlight (v15), resolved through merges with the name
                // beside it: the player bar paints one name at a time and it
                // must be the same colour the transcript gave it.
                "speaker_colour": row.speaker_colour,
                "speaker_icon": row.speaker_icon,
                "text": row.text,
                "has_audio": on_disk(data_dir, &row.audio_path),
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({ "thread": thread, "turns": turns }))
}

/// Is there a file behind this row *right now*? A blanked path is retention
/// having done its job; a path whose file is gone is the dangling case the
/// sweeper logs and leaves. Both answer the same question the same way.
fn on_disk(data_dir: &Path, rel: &str) -> bool {
    !rel.is_empty() && data_dir.join(rel).is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GraphConfig;
    use crate::store::SegmentAnalysis;

    const SEC: i64 = 1_000_000_000;

    struct Rig {
        store: Store,
        dir: std::path::PathBuf,
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn rig(name: &str) -> Rig {
        let dir =
            std::env::temp_dir().join(format!("nx-recall-replay-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        Rig { store, dir }
    }

    /// A conversation of `script.len()` turns, one per named speaker, five
    /// seconds apart so [`crate::threads`] keeps them in one thread. Returns
    /// the segment ids and the thread they landed in.
    fn a_conversation(r: &Rig, script: &[i64]) -> (Vec<i64>, i64) {
        let store = &r.store;
        let src = store.upsert_source("VRChat.exe", "VRChat.exe", 0).unwrap();
        let sess = store.begin_session(src, 0).unwrap();
        let cfg = GraphConfig::default();
        let mut ids = Vec::new();
        for (i, speaker) in script.iter().enumerate() {
            let at = i as i64 * 5 * SEC;
            let rel = format!("segments/turn-{i}.wav");
            std::fs::create_dir_all(r.dir.join("segments")).unwrap();
            std::fs::write(r.dir.join(&rel), b"not really a wav").unwrap();
            let seg = store
                .insert_segment(sess, at, at + 3 * SEC, &rel, 0)
                .unwrap();
            store
                .set_segment_speaker(seg, Some(*speaker), Some(0.9))
                .unwrap();
            store
                .set_segment_analysis(
                    seg,
                    &SegmentAnalysis {
                        text: Some(format!("turn {i}")),
                        ..Default::default()
                    },
                )
                .unwrap();
            crate::threads::assign(store, &cfg, seg).unwrap();
            ids.push(seg);
        }
        let thread = store
            .segment_row(ids[0])
            .unwrap()
            .unwrap()
            .thread_id
            .unwrap();
        (ids, thread)
    }

    #[test]
    fn the_turns_come_back_in_order_with_everything_a_scrubber_needs() {
        let r = rig("in-order");
        let (a, b) = (
            r.store.create_speaker("A", 0).unwrap(),
            r.store.create_speaker("B", 0).unwrap(),
        );
        let (ids, thread) = a_conversation(&r, &[a, b, a, b]);

        let v = turns(&r.store, &r.dir, thread).unwrap();
        assert_eq!(v["thread"], json!(thread));
        let turns = v["turns"].as_array().unwrap();
        assert_eq!(
            turns
                .iter()
                .map(|t| t["id"].as_i64().unwrap())
                .collect::<Vec<_>>(),
            ids
        );
        // Everything the client draws the bar from, on every turn.
        for (i, t) in turns.iter().enumerate() {
            assert_eq!(t["dur_ms"], json!(3000), "turn {i} has no duration");
            assert!(t["t_ns"].is_string(), "turn {i}'s t_ns is not a string");
            assert_eq!(t["t_ms"].as_i64().unwrap(), i as i64 * 5000);
            assert!(t["text"].is_string(), "turn {i} has no words");
            assert_eq!(t["has_audio"], json!(true), "turn {i} lost its audio");
        }
        assert_eq!(turns[0]["speaker"], json!(a));
        assert_eq!(turns[0]["speaker_name"], json!("A"));
        assert_eq!(turns[1]["speaker"], json!(b));
    }

    #[test]
    fn has_audio_is_the_file_on_disk_and_not_the_column() {
        let r = rig("on-disk");
        let a = r.store.create_speaker("A", 0).unwrap();
        let (ids, thread) = a_conversation(&r, &[a, a, a]);

        // Two ways a turn loses its audio, and the row survives both. The
        // first is retention doing its job; the second is the dangling case
        // `retention::sweep` counts and deliberately leaves alone.
        r.store.forget_audio(&[ids[0]]).unwrap();
        let rel = r.store.segment_audio(ids[1]).unwrap().unwrap().0;
        std::fs::remove_file(r.dir.join(&rel)).unwrap();

        let v = turns(&r.store, &r.dir, thread).unwrap();
        let turns = v["turns"].as_array().unwrap();
        assert_eq!(
            turns
                .iter()
                .map(|t| t["has_audio"].as_bool().unwrap())
                .collect::<Vec<_>>(),
            vec![false, false, true],
            "the blanked path and the missing file must read alike"
        );
        // …and the words are still there for all three: the transcript
        // outlives the recording, which is the whole reason replay reads
        // through a silent turn rather than skipping over it.
        assert!(turns.iter().all(|t| t["text"].is_string()));
    }

    #[test]
    fn an_unknown_conversation_is_not_found_rather_than_an_empty_one() {
        let r = rig("unknown");
        let e = turns(&r.store, &r.dir, 4042).unwrap_err();
        assert_eq!(e.code, "not_found");
    }
}
