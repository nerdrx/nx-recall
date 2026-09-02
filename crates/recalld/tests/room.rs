//! The room microphone as a capture source, end to end through the real
//! pipeline (0.10.0).
//!
//! Live capture of a physical device is not testable here and is not attempted:
//! the tap's PipeWire half is a second caller of `capture::mic_plan`, whose
//! state table is unit-tested, and `recalld devices` enumerates without opening
//! anything. What is tested here is everything downstream of "audio arrived on
//! a session whose source is kind = room":
//!
//! * the turns are ORDINARY turns — no You pin, no `label_via: "mic"`, and the
//!   voicebank is consulted exactly as it is for an application;
//! * their provenance says where they came from (`source: "room"`);
//! * and they thread with the headset's turns, because the room and the
//!   headset are the same evening.
//!
//! No models are loaded: none of the three claims is about ASR or embeddings,
//! and the VAD is the bundled one.

use std::path::{Path, PathBuf};

use recalld::config::{Config, GraphConfig};
use recalld::ingest::{OfflinePipeline, ingest_pcm, read_wav};
use recalld::room::{ROOM_DISPLAY_NAME, ROOM_MATCH_KEY};
use recalld::store::{KIND_MIC, KIND_ROOM, Store, label_via};
use recalld::vad::SileroVad;

const SEC: i64 = 1_000_000_000;

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
}

struct Rig {
    dir: PathBuf,
    store: Store,
    vad: SileroVad,
    cfg: Config,
    room_session: i64,
    mic_session: i64,
    app_session: i64,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Rig {
    fn start(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("nx-recall-room-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creating the throwaway data dir");
        let store = Store::open(&dir).expect("opening the throwaway database");

        let room = store
            .upsert_source_kind(ROOM_MATCH_KEY, ROOM_DISPLAY_NAME, KIND_ROOM, 0)
            .unwrap();
        let mic = store
            .upsert_source_kind("mic", "Microphone", KIND_MIC, 0)
            .unwrap();
        let app = store.upsert_source("VRChat.exe", "VRChat", 0).unwrap();

        Self {
            room_session: store.begin_session(room, 0).unwrap(),
            mic_session: store.begin_session(mic, 0).unwrap(),
            app_session: store.begin_session(app, 0).unwrap(),
            store,
            dir,
            vad: SileroVad::from_bytes(recalld::VAD_MODEL).expect("bundled VAD model"),
            cfg: Config::default(),
        }
    }

    /// One fixture through the real ingest path, at a chosen wall-clock time.
    /// The route (mic pin vs ordinary) is chosen from the session's
    /// `sources.kind`, which is the same decision the live daemon makes.
    fn ingest(&mut self, session: i64, fixture: &str, t0_ns: i64) -> Vec<i64> {
        let samples =
            read_wav(&fixtures_dir().join(fixture)).unwrap_or_else(|e| panic!("{fixture}: {e:#}"));
        let mut pipe = OfflinePipeline::new(&mut self.vad, &self.cfg, None);
        ingest_pcm(&self.store, &self.dir, session, &samples, t0_ns, &mut pipe)
            .unwrap_or_else(|e| panic!("ingesting {fixture}: {e:#}"))
    }

    fn thread_of(&self, id: i64) -> Option<i64> {
        recalld::threads::assign(&self.store, &GraphConfig::default(), id).unwrap()
    }
}

#[test]
fn a_room_turn_is_an_ordinary_turn_that_says_where_it_came_from() {
    let mut rig = Rig::start("provenance");
    let ids = rig.ingest(rig.room_session, "clean_single_0.wav", 1_800_000_000 * SEC);
    assert!(!ids.is_empty(), "the fixture produced no turns");

    for id in &ids {
        let row = rig.store.segment_row(*id).unwrap().unwrap();
        // Provenance: the source's match key, which is what every read path
        // already puts on the wire as `source`.
        assert_eq!(
            row.source, ROOM_MATCH_KEY,
            "a room turn must say it came off the room microphone"
        );
        // And NOT the microphone's route: no pin, no mic label.
        assert_eq!(
            row.label_via.as_deref(),
            None,
            "a room turn was labelled by provenance; it must go through the voicebank"
        );
        assert_ne!(row.label_via.as_deref(), Some(label_via::MIC));
    }
    // The You speaker is not minted by a room turn. That is the whole
    // difference between the two microphones.
    assert_eq!(
        rig.store.you_speaker_id().unwrap(),
        None,
        "the room microphone minted a \"You\" — it hears everybody except you"
    );
}

#[test]
fn a_room_turn_joins_the_conversation_the_headset_is_in() {
    // The room and the headset are one physical evening: somebody on the sofa
    // answering somebody in the instance is in that conversation, and which
    // device carried the sound is a fact about cabling.
    let mut rig = Rig::start("threading");
    let t0 = 1_800_000_000 * SEC;

    let app = rig.ingest(rig.app_session, "clean_single_0.wav", t0);
    let mic = rig.ingest(rig.mic_session, "clean_single_1.wav", t0 + 6 * SEC);
    let room = rig.ingest(rig.room_session, "clean_single_0.wav", t0 + 12 * SEC);
    assert!(!app.is_empty() && !mic.is_empty() && !room.is_empty());

    // Distinct voices, minted and set directly: the rule reasons about speaker
    // ids and a model is not needed to give it three.
    let voices: Vec<i64> = (0..4).map(|i| rig.store.mint_speaker(i).unwrap()).collect();
    for (ids, speaker) in [(&app, voices[0]), (&mic, voices[1]), (&room, voices[2])] {
        for id in ids {
            rig.store
                .set_segment_speaker_via(*id, Some(speaker), None, None)
                .unwrap();
        }
    }

    let app_thread = rig.thread_of(app[0]).expect("the app turn opened a thread");
    let mic_thread = rig.thread_of(mic[0]).expect("the mic turn was threaded");
    let room_thread = rig.thread_of(room[0]).expect("the room turn was threaded");

    assert_eq!(
        mic_thread, app_thread,
        "the microphone stopped bridging — that regression made every thread a monologue"
    );
    assert_eq!(
        room_thread, app_thread,
        "the room microphone opened a thread of its own instead of joining the live conversation"
    );

    // The contrast, so the test is about bridging rather than about a rule
    // that puts everything in one thread: a SECOND application at the same
    // moment stays separate.
    let other = rig
        .store
        .upsert_source("Discord", "Discord", 0)
        .and_then(|s| rig.store.begin_session(s, 0))
        .unwrap();
    let discord = rig.ingest(other, "clean_single_1.wav", t0 + 14 * SEC);
    for id in &discord {
        rig.store
            .set_segment_speaker_via(*id, Some(voices[3]), None, None)
            .unwrap();
    }
    assert_ne!(
        rig.thread_of(discord[0]).unwrap(),
        app_thread,
        "two applications talking at once are two conversations"
    );
}

#[test]
fn the_room_source_row_is_its_own_kind() {
    let rig = Rig::start("kind");
    let rows = rig.store.list_sources().unwrap();
    let room = rows
        .iter()
        .find(|r| r.match_key == ROOM_MATCH_KEY)
        .expect("the room source row exists whether or not the switch is on");
    assert_eq!(room.kind, KIND_ROOM);
    assert!(
        !room.allowed,
        "the room microphone must be off until somebody turns it on"
    );
    assert_eq!(
        rig.store.session_source_kind(rig.room_session).unwrap(),
        Some(KIND_ROOM.to_string())
    );
    // And it bridges, while an application does not.
    assert!(recalld::store::kind_bridges_threads(KIND_ROOM));
    assert!(recalld::store::kind_bridges_threads(KIND_MIC));
    assert!(!recalld::store::kind_bridges_threads(
        recalld::store::KIND_APP
    ));
}
