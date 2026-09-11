//! The control socket, end to end: a scripted client on a real Unix socket,
//! driving a real daemon over an isolated data directory.
//!
//! Nothing here touches the live daemon's data dir or socket — every rig gets
//! its own temporary directory and binds its socket inside it. The pipeline is
//! the real one (`ingest_pcm`, the same call the fixture suite uses); with
//! `NXR_MODELS` set it also loads the analysis models, so the segment events
//! carry real transcripts, and without them the VAD leg alone still produces
//! the segments and events these tests are about.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use recalld::allowlist::Allowlist;
use recalld::analysis::Analyzer;
use recalld::bus::Bus;
use recalld::config::Config;
use recalld::control::Control;
use recalld::ingest::{OfflinePipeline, ingest_pcm, read_wav};
use recalld::models::ModelSet;
use recalld::server::{self, Server};
use recalld::service::Service;
use recalld::store::Store;
use recalld::vad::SileroVad;

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
}

/// `NXR_MODELS` if it is set and absolute. A relative path would resolve
/// against whatever directory the test binary happens to run in, so it is
/// refused loudly rather than silently skipping the analysis leg.
fn models_dir() -> Option<PathBuf> {
    let raw = std::env::var("NXR_MODELS").ok()?;
    if raw.trim().is_empty() {
        return None;
    }
    let path = PathBuf::from(raw);
    assert!(
        path.is_absolute(),
        "NXR_MODELS must be an absolute path, got {}",
        path.display()
    );
    Some(path)
}

struct Daemon {
    dir: PathBuf,
    socket: PathBuf,
    store: Arc<Mutex<Store>>,
    control: Arc<Control>,
    bus: Arc<Bus>,
    server: Server,
    session: i64,
    vad: SileroVad,
    cfg: Config,
    analyzer: Option<Analyzer>,
    clock_ns: i64,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.server.shutdown();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Daemon {
    fn start(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("nx-recall-sock-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creating the throwaway data dir");

        let mut cfg = Config::default();
        let analyzer = models_dir().map(|models| {
            cfg.models.dir = Some(models.clone());
            let mut set =
                ModelSet::resolve(&cfg.models).expect("NXR_MODELS resolves to a model set");
            set.select_asr();
            let missing = set.missing();
            assert!(
                missing.is_empty(),
                "NXR_MODELS={} is missing {:?}",
                models.display(),
                missing.iter().map(|e| e.role).collect::<Vec<_>>()
            );
            Analyzer::load(&set, &cfg.identity).expect("loading the analysis models")
        });

        let store = Store::open(&dir).expect("opening the throwaway database");
        let source = store.upsert_source("fixtures", "fixtures", 0).unwrap();
        let session = store.begin_session(source, 0).unwrap();
        let store = Arc::new(Mutex::new(store));

        let control = Control::new(
            dir.clone(),
            None,
            &Allowlist::from_rules([("fixtures", true)]),
        )
        // `speakers.split` re-clusters a voicebank, so the socket needs the
        // same operating point the pipeline labelled with.
        .with_identity(cfg.identity.clone());
        let bus = Bus::new(cfg.socket.replay_events, cfg.socket.client_outbox);
        let service = Service::new(Arc::clone(&store), Arc::clone(&control), Arc::clone(&bus));
        let socket = dir.join("nx-recall.sock");
        let server = server::serve(service, &socket).expect("binding the isolated socket");

        Self {
            dir,
            socket,
            store,
            control,
            bus,
            server,
            session,
            vad: SileroVad::from_bytes(recalld::VAD_MODEL).expect("bundled VAD model"),
            cfg,
            analyzer: None.or(analyzer),
            clock_ns: 0,
        }
    }

    /// One fixture through the real pipeline, publishing on the real bus.
    fn ingest(&mut self, fixture: &str) -> Vec<i64> {
        let samples =
            read_wav(&fixtures_dir().join(fixture)).unwrap_or_else(|e| panic!("{fixture}: {e:#}"));
        self.ingest_samples(&samples)
    }

    /// The same fixture, long enough to be worth an identity.
    ///
    /// The mint bar (0.6.1) refuses to spend a permanent voice on a turn under
    /// `mint_min_duration_s`, and `clean_single_1.wav` is 1.75 s — a real turn,
    /// but a shorter one than a new identity is worth. Doubling it is the
    /// cheapest way to get a second *person* out of the fixture set.
    fn ingest_twice_over(&mut self, fixture: &str) -> Vec<i64> {
        let samples =
            read_wav(&fixtures_dir().join(fixture)).unwrap_or_else(|e| panic!("{fixture}: {e:#}"));
        let doubled: Vec<f32> = samples.iter().chain(samples.iter()).copied().collect();
        self.ingest_samples(&doubled)
    }

    fn ingest_samples(&mut self, samples: &[f32]) -> Vec<i64> {
        let t0 = self.clock_ns;
        self.clock_ns += 60_000_000_000;
        let mut pipe = OfflinePipeline {
            vad: &mut self.vad,
            cfg: &self.cfg,
            analyzer: self.analyzer.as_mut(),
            control: Some(Arc::clone(&self.control)),
            bus: Some(Arc::clone(&self.bus)),
        };
        let store = self.store.lock().unwrap();
        ingest_pcm(&store, &self.dir, self.session, samples, t0, &mut pipe)
            .unwrap_or_else(|e| panic!("ingesting {} samples: {e:#}", samples.len()))
    }

    fn segment_rows(&self) -> usize {
        self.store
            .lock()
            .unwrap()
            .transcript(None, None)
            .unwrap()
            .len()
    }

    fn wav_count(&self) -> usize {
        fn walk(dir: &Path, n: &mut usize) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, n);
                } else if p.extension().is_some_and(|x| x == "wav") {
                    *n += 1;
                }
            }
        }
        let mut n = 0;
        walk(&self.dir.join("segments"), &mut n);
        n
    }

    fn connect(&self) -> Conn {
        Conn::connect(&self.socket)
    }
}

/// A scripted NDJSON client. Deliberately hand-rolled rather than reusing the
/// daemon's own client type: the point is to exercise the wire.
///
/// One rule it has to get right, because every real client does too: replies
/// and events share the connection, so a message read while waiting for
/// something else is *kept*, never dropped.
struct Conn {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: i64,
    pending: std::collections::VecDeque<Value>,
}

impl Conn {
    fn connect(path: &Path) -> Self {
        let s = UnixStream::connect(path).expect("connecting to the daemon socket");
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        Self {
            reader: BufReader::new(s.try_clone().unwrap()),
            writer: s,
            next_id: 1,
            pending: std::collections::VecDeque::new(),
        }
    }

    fn line(&mut self, text: &str) {
        self.writer.write_all(text.as_bytes()).unwrap();
        self.writer.write_all(b"\n").unwrap();
        self.writer.flush().unwrap();
    }

    /// The next message, from what we already buffered or from the socket.
    fn read(&mut self) -> Value {
        if let Some(msg) = self.pending.pop_front() {
            return msg;
        }
        self.read_socket()
    }

    fn read_socket(&mut self) -> Value {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line).expect("reading a reply");
        assert!(n > 0, "the daemon closed the connection unexpectedly");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON: {line:?} ({e})"))
    }

    fn keep(&mut self, skipped: Vec<Value>) {
        for msg in skipped.into_iter().rev() {
            self.pending.push_front(msg);
        }
    }

    /// Handshake, exactly as PROTOCOL.md describes it.
    fn hello(&mut self) -> Value {
        self.line(r#"{"hello":{"proto":1,"client":"integration-test/1"}}"#);
        let welcome = self.read();
        assert_eq!(welcome["welcome"]["proto"], 1);
        assert_eq!(welcome["welcome"]["schema"], recalld::store::SCHEMA_VERSION);
        welcome["welcome"].clone()
    }

    fn call_raw(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.line(&json!({"id": id, "method": method, "params": params}).to_string());
        let mut skipped = Vec::new();
        loop {
            let msg = self.read();
            if msg["id"] == json!(id) {
                self.keep(skipped);
                return msg;
            }
            // An event overtook the reply. It is still ours.
            skipped.push(msg);
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        let msg = self.call_raw(method, params);
        assert!(msg.get("err").is_none(), "{method} failed: {}", msg["err"]);
        msg["ok"].clone()
    }

    fn call_err(&mut self, method: &str, params: Value) -> Value {
        self.call_raw(method, params)["err"].clone()
    }

    fn subscribe(&mut self, topics: &[&str]) -> Value {
        self.call("subscribe", json!({"topics": topics}))
    }

    /// Whether the daemon has hung up.
    fn eof(&mut self) -> bool {
        if !self.pending.is_empty() {
            return false;
        }
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap_or(0) == 0
    }

    /// Read until an event of this type arrives, keeping everything else.
    fn wait_event(&mut self, ev: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut skipped = Vec::new();
        while Instant::now() < deadline {
            let msg = self.read();
            if msg["ev"] == ev {
                self.keep(skipped);
                return msg;
            }
            skipped.push(msg);
        }
        panic!("no {ev} event arrived; saw {skipped:?}");
    }

    /// Everything already queued for us, without blocking for more.
    fn drain(&mut self) -> Vec<Value> {
        self.writer
            .set_read_timeout(Some(Duration::from_millis(250)))
            .unwrap();
        let mut out: Vec<Value> = self.pending.drain(..).collect();
        loop {
            let mut line = String::new();
            match self.reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => out.push(serde_json::from_str(&line).unwrap()),
                Err(_) => break, // the timeout: nothing more is waiting
            }
        }
        self.writer
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        out
    }
}

// ---- 1. the handshake and the segment stream ---------------------------

#[test]
fn a_subscribed_client_receives_stored_segments_with_sequence_numbers() {
    let mut d = Daemon::start("segments");
    let mut c = d.connect();
    let welcome = c.hello();
    assert_eq!(
        welcome["seq"], 0,
        "a fresh daemon starts the stream at zero"
    );
    c.subscribe(&["segments"]);

    let ids = d.ingest("clean_single_0.wav");
    assert!(!ids.is_empty(), "the fixture must produce segments");

    let ev = c.wait_event("segment");
    assert!(
        ev["seq"].as_u64().unwrap() >= 1,
        "every event carries a seq"
    );
    assert_eq!(ev["data"]["id"], ids[0]);
    assert_eq!(ev["data"]["session"], d.session);
    if d.analyzer.is_some() {
        assert!(
            ev["data"]["text"].as_str().is_some_and(|t| !t.is_empty()),
            "with models loaded the event carries the transcript"
        );
    }

    // And the same rows are readable through the request path.
    let t = c.call("transcript", json!({}));
    assert_eq!(t["segments"].as_array().unwrap().len(), ids.len());
}

#[test]
fn a_client_only_gets_the_topics_it_asked_for() {
    let mut d = Daemon::start("topics");
    let mut c = d.connect();
    c.hello();
    let out = c.subscribe(&["relabel"]);
    assert_eq!(out["topics"], json!(["relabel"]));

    d.ingest("clean_single_0.wav");
    let speaker = d.store.lock().unwrap().mint_speaker(0).unwrap();
    c.call("speakers.name", json!({"id": speaker, "name": "Ines"}));

    let evs = c.drain();
    assert!(!evs.is_empty());
    assert!(
        evs.iter().all(|e| e["ev"] == "relabel"),
        "a relabel subscriber must not receive segment events: {evs:?}"
    );
}

// ---- 2. relabel reaches every client ------------------------------------

#[test]
fn a_rename_is_broadcast_to_every_connected_client_with_one_seq() {
    let mut d = Daemon::start("relabel");
    let mut a = d.connect();
    let mut b = d.connect();
    a.hello();
    b.hello();
    a.subscribe(&["relabel"]);
    b.subscribe(&["relabel"]);

    let ids = d.ingest("clean_single_0.wav");
    let speaker = {
        // One lock, held once: a guard taken in a `match` scrutinee lives for
        // the whole match, and taking a second one inside would deadlock.
        let store = d.store.lock().unwrap();
        match store.list_speakers().unwrap().first().map(|s| s.id) {
            // With models the pipeline minted a voice; without them, make one
            // so the broadcast is still exercised.
            Some(id) => id,
            None => {
                let id = store.mint_speaker(0).unwrap();
                store
                    .set_segment_speaker(ids[0], Some(id), Some(0.5))
                    .unwrap();
                id
            }
        }
    };

    let ok = a.call("speakers.name", json!({"id": speaker, "name": "Kira"}));
    let ev_a = a.wait_event("relabel");
    let ev_b = b.wait_event("relabel");
    assert_eq!(ev_a, ev_b, "both clients see the identical event");
    assert_eq!(ev_a["seq"], ok["seq"]);
    assert_eq!(ev_a["data"]["speaker"], speaker);
    assert_eq!(ev_a["data"]["name"], "Kira");

    // Retroactive: the reader that never re-queried is right, and so is the
    // one that does. The row carries the id; the name rides along for display.
    let t = b.call("transcript", json!({}));
    assert_eq!(t["segments"][0]["speaker"], speaker);
    assert_eq!(t["segments"][0]["speaker_name"], "Kira");

    // And the voice is now a named one, which is what the onboarding flow asks.
    let listed = a.call("speakers.list", json!({}));
    let row = listed["speakers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == speaker)
        .expect("the renamed voice is listed");
    assert_eq!(row["name"], "Kira");
    assert!(row["auto"].as_str().unwrap().starts_with("Speaker_"));
}

/// A highlight is set over the socket, broadcast to every client, and worn by
/// the transcript rows that were already fetched (0.12.0, schema v15).
///
/// The same shape as the rename above, because it is the same promise: a
/// property of a VOICE changes once and every view of that voice changes with
/// it. What this adds is the omit-versus-null rule, which has no analogue in
/// `speakers.name` and is the part a client can get wrong — picking a colour
/// must not silently clear the emoji somebody set a minute earlier.
#[test]
fn a_highlight_is_broadcast_and_rides_the_transcript() {
    let mut d = Daemon::start("highlight");
    let mut a = d.connect();
    let mut b = d.connect();
    a.hello();
    b.hello();
    a.subscribe(&["relabel"]);
    b.subscribe(&["relabel"]);

    let ids = d.ingest("clean_single_0.wav");
    let speaker = {
        let store = d.store.lock().unwrap();
        match store.list_speakers().unwrap().first().map(|s| s.id) {
            Some(id) => id,
            None => {
                let id = store.mint_speaker(0).unwrap();
                store
                    .set_segment_speaker(ids[0], Some(id), Some(0.5))
                    .unwrap();
                id
            }
        }
    };

    // The palette is served rather than assumed, so a client can draw swatches
    // for a colour this build knows and an older GUI does not.
    let palette = a.call("speakers.palette", json!({}));
    let entries = palette["palette"].as_array().expect("a palette");
    assert_eq!(entries.len(), 10);
    assert!(
        entries.iter().any(|e| e["hex"] == "#7700ff"),
        "the suite's own colour is not in the palette"
    );

    let ok = a.call(
        "speakers.set",
        json!({"id": speaker, "colour": "violet", "icon": "\u{1f319}"}),
    );
    assert_eq!(ok["colour"], "violet");
    assert_eq!(ok["icon"], "\u{1f319}");

    let ev_a = a.wait_event("relabel");
    let ev_b = b.wait_event("relabel");
    assert_eq!(ev_a, ev_b, "both clients see the identical event");
    assert_eq!(ev_a["seq"], ok["seq"]);
    assert_eq!(ev_a["data"]["speaker"], speaker);
    assert_eq!(ev_a["data"]["colour"], "violet");
    assert_eq!(ev_a["data"]["icon"], "\u{1f319}");
    // The `name` key is on the event too, for the reason
    // `speakers.set_languages` puts it there: a client folds ONE shape into its
    // speaker row, so an event that carried the highlight and dropped the name
    // would make it choose between applying the new fact and keeping the old
    // one. It is null here only because nobody has named this voice.
    assert!(
        ev_a["data"].get("name").is_some(),
        "the name rides along on a highlight relabel: {}",
        ev_a["data"]
    );

    // Retroactive, exactly as a rename is: the rows carry it without anybody
    // re-querying the speaker.
    let t = b.call("transcript", json!({}));
    assert_eq!(t["segments"][0]["speaker_colour"], "violet");
    assert_eq!(t["segments"][0]["speaker_icon"], "\u{1f319}");

    let row = |c: &mut Conn| -> Value {
        c.call("speakers.list", json!({}))["speakers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == speaker)
            .expect("the highlighted voice is listed")
            .clone()
    };
    let listed = row(&mut a);
    assert_eq!(listed["colour"], "violet");
    assert_eq!(listed["icon"], "\u{1f319}");

    // The rule that needs a socket to be convincing: changing ONE half leaves
    // the other alone. A picker that sent both would pass a unit test and lose
    // somebody's emoji here.
    a.call("speakers.set", json!({"id": speaker, "colour": "teal"}));
    let _ = a.wait_event("relabel");
    let listed = row(&mut a);
    assert_eq!(listed["colour"], "teal");
    assert_eq!(
        listed["icon"], "\u{1f319}",
        "the emoji was collateral damage"
    );

    // And an explicit null is a deliberate clear, of that half only.
    a.call("speakers.set", json!({"id": speaker, "icon": null}));
    let _ = a.wait_event("relabel");
    let listed = row(&mut a);
    assert_eq!(listed["colour"], "teal");
    assert_eq!(listed["icon"], Value::Null);

    // A colour no build can paint is refused rather than stored: a highlight
    // that renders as nothing for ever is worse than no highlight.
    let bad = a.call_err(
        "speakers.set",
        json!({"id": speaker, "colour": "chartreuse"}),
    );
    assert_eq!(bad["code"], "params");
    // A name is not an icon.
    let bad = a.call_err("speakers.set", json!({"id": speaker, "icon": "Kira"}));
    assert_eq!(bad["code"], "params");
}

/// The recovery path for the failure this whole design fears: two people on one
/// id (DESIGN §8, FINDINGS §5). Merge two different voices on purpose, then
/// split them and check they land apart again.
///
/// Needs the real models — a false merge cannot be forced without real
/// embeddings — so without `NXR_MODELS` it reports itself skipped and passes.
#[test]
fn a_false_merge_is_undone_by_a_split_and_every_client_hears_about_it() {
    let mut d = Daemon::start("split");
    if d.analyzer.is_none() {
        eprintln!("skipping the split round trip: set NXR_MODELS=<dir> to run it");
        return;
    }
    let mut c = d.connect();
    c.hello();
    c.subscribe(&["relabel", "segments", "ops"]);

    let ines = d.ingest("clean_single_0.wav");
    let wren = d.ingest_twice_over("clean_single_1.wav");
    let speaker_of = |c: &mut Conn| -> std::collections::HashMap<i64, i64> {
        c.call("transcript", json!({}))["segments"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| !s["speaker"].is_null())
            .map(|s| (s["id"].as_i64().unwrap(), s["speaker"].as_i64().unwrap()))
            .collect()
    };
    /// The id most of a fixture's segments were given.
    fn dominant(labels: &std::collections::HashMap<i64, i64>, ids: &[i64]) -> i64 {
        let mut counts: std::collections::HashMap<i64, usize> = std::collections::HashMap::new();
        for id in ids {
            if let Some(spk) = labels.get(id) {
                *counts.entry(*spk).or_default() += 1;
            }
        }
        let mut best: Vec<(i64, usize)> = counts.into_iter().collect();
        best.sort_by_key(|(id, n)| (std::cmp::Reverse(*n), *id));
        best.first()
            .map(|(id, _)| *id)
            .expect("the fixture was labelled")
    }

    let before = speaker_of(&mut c);
    let a = dominant(&before, &ines);
    let b = dominant(&before, &wren);
    assert_ne!(a, b, "two different voices must start on two ids");

    // The poison: one id now holds two people.
    c.call("speakers.merge", json!({"from": b, "into": a}));
    let merged = speaker_of(&mut c);
    assert_eq!(dominant(&merged, &ines), a);
    assert_eq!(
        dominant(&merged, &wren),
        a,
        "the merge really did collapse them"
    );
    c.drain();

    let out = c.call("speakers.split", json!({"id": a}));
    assert_eq!(out["kept"], a);
    let minted = out["minted"].as_i64().unwrap();
    assert_ne!(minted, a);
    assert!(out["moved_segments"].as_u64().unwrap() >= 1);
    assert!(
        out["centroid_similarity"].as_f64().unwrap() < 0.6,
        "two real speakers must clear the refusal threshold: {out}"
    );

    // Apart again, and the id with the history is the one that survived.
    let after = speaker_of(&mut c);
    let split_a = dominant(&after, &ines);
    let split_b = dominant(&after, &wren);
    assert_ne!(
        split_a, split_b,
        "the split did not separate the two voices"
    );
    assert!(
        [split_a, split_b].contains(&a) && [split_a, split_b].contains(&minted),
        "the two halves are the kept id and the minted one, got {split_a} and {split_b}"
    );
    assert_eq!(
        c.call("speakers.list", json!({}))["speakers"]
            .as_array()
            .unwrap()
            .len(),
        2,
        "two people, two voices"
    );

    // And no client has to poll to find out: both ids are announced, the new
    // one saying which voice it came out of.
    let evs = c.drain();
    let relabels: Vec<&Value> = evs.iter().filter(|e| e["ev"] == "relabel").collect();
    assert!(
        relabels.iter().any(|e| e["data"]["speaker"] == a),
        "the voice that was cut is announced: {evs:?}"
    );
    let fresh = relabels
        .iter()
        .find(|e| e["data"]["speaker"] == minted)
        .expect("the minted voice is announced");
    assert_eq!(fresh["data"]["split_from"], a);
    assert_eq!(fresh["data"]["name"], Value::Null);
    assert!(
        evs.iter()
            .any(|e| e["ev"] == "segment" && e["data"]["speaker"] == minted),
        "every moved row arrives on the same event a new row would"
    );
    let done = evs
        .iter()
        .find(|e| e["ev"] == "op.done")
        .expect("PROTOCOL calls a split an operation, so it gets a terminal event");
    assert_eq!(done["data"]["kind"], "speakers.split");
    assert_eq!(done["data"]["op"], out["op"]);

    // The audit trail knows where every moved segment came from.
    let ops = c.call("operations.list", json!({}))["operations"][0].clone();
    assert_eq!(ops["op"], "speakers.split");
    assert!(
        ops["prior_state"]["segments"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["speaker_id"] == a),
        "every moved segment was on the merged id before the split"
    );
}

#[test]
fn splitting_a_voice_that_is_one_person_is_refused_over_the_wire() {
    let mut d = Daemon::start("split-refuse");
    if d.analyzer.is_none() {
        eprintln!("skipping the split refusal: set NXR_MODELS=<dir> to run it");
        return;
    }
    let mut c = d.connect();
    c.hello();
    c.subscribe(&["relabel", "segments"]);
    // The same person twice: there is no second voice to find.
    d.ingest("clean_single_0.wav");
    d.ingest("clean_single_0.wav");
    let speakers = c.call("speakers.list", json!({}))["speakers"].clone();
    assert_eq!(speakers.as_array().unwrap().len(), 1);
    let id = speakers[0]["id"].as_i64().unwrap();
    c.drain();

    let err = c.call_err("speakers.split", json!({"id": id}));
    assert_eq!(err["code"], "refused");
    // Nothing was invented and nothing was announced.
    assert_eq!(
        c.call("speakers.list", json!({}))["speakers"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        !c.drain().iter().any(|e| e["ev"] == "relabel"),
        "a refused split must not broadcast a thing"
    );
}

// ---- 3. replay ----------------------------------------------------------

#[test]
fn events_since_replays_what_a_late_client_missed() {
    let mut d = Daemon::start("replay");
    let mut early = d.connect();
    early.hello();
    early.subscribe(&["segments"]);
    let ids = d.ingest("clean_single_0.wav");
    let first = early.wait_event("segment");
    let first_seq = first["seq"].as_u64().unwrap();

    // A second client connects after the fact and catches up from zero.
    let mut late = d.connect();
    let welcome = late.hello();
    assert!(welcome["seq"].as_u64().unwrap() >= first_seq);
    late.subscribe(&["segments"]);
    let out = late.call("events.since", json!({"seq": 0}));
    // The batch is in the reply itself, so a client applies it in seq order
    // before the live events it queued while asking.
    let batch = out["events"].as_array().unwrap();
    assert_eq!(batch.len(), ids.len());
    assert_eq!(batch[0]["seq"], first["seq"]);
    assert_eq!(batch[0]["data"]["id"], ids[0]);

    // Caught up is not an error; a sequence we never had is a resync.
    let seq = out["seq"].clone();
    let caught_up = late.call("events.since", json!({"seq": seq}));
    assert_eq!(caught_up["events"].as_array().unwrap().len(), 0);
    assert_eq!(
        late.call_err("events.since", json!({"seq": 10_000_000}))["code"],
        "resync"
    );
}

// ---- 4. a dead client cannot take the daemon with it ---------------------

#[test]
fn killing_one_client_mid_stream_leaves_the_daemon_and_its_peers_alone() {
    let mut d = Daemon::start("kill");
    let mut victim = d.connect();
    let mut survivor = d.connect();
    victim.hello();
    survivor.hello();
    victim.subscribe(&["segments"]);
    survivor.subscribe(&["segments"]);

    d.ingest("clean_single_0.wav");
    victim.wait_event("segment");
    survivor.wait_event("segment");
    assert_eq!(d.bus.client_count(), 2);

    // Hang up hard, mid-stream, without a goodbye.
    victim
        .writer
        .shutdown(std::net::Shutdown::Both)
        .expect("hanging up");
    drop(victim);

    // The pipeline keeps running and the other client keeps receiving.
    let ids = d.ingest("clean_single_1.wav");
    assert!(!ids.is_empty(), "the pipeline must be unaffected");
    let ev = survivor.wait_event("segment");
    assert_eq!(ev["data"]["id"], ids[0]);
    assert_eq!(survivor.call("status", json!({}))["clients"], 1);

    // And a fresh client can still connect.
    let mut fresh = d.connect();
    fresh.hello();
    assert_eq!(fresh.call("status", json!({}))["paused"], false);
}

// ---- 5. pause -----------------------------------------------------------

#[test]
fn pause_stops_every_write_and_resume_starts_them_again() {
    let mut d = Daemon::start("pause");
    let mut c = d.connect();
    c.hello();
    c.subscribe(&["segments", "sources", "status"]);

    let before = d.ingest("clean_single_0.wav");
    assert!(!before.is_empty());
    let rows_before = d.segment_rows();
    let files_before = d.wav_count();
    assert!(files_before > 0, "the unpaused pipeline writes audio files");
    c.drain();

    // Pause over the socket, then feed it the same fixture again.
    let out = c.call("pause", json!({}));
    assert_eq!(out["paused"], true);
    assert_eq!(c.call("status", json!({}))["paused"], true);

    let during = d.ingest("clean_single_1.wav");
    assert!(during.is_empty(), "a paused pipeline stores no segments");
    assert_eq!(d.segment_rows(), rows_before, "no new rows while paused");
    assert_eq!(
        d.wav_count(),
        files_before,
        "no new audio files while paused"
    );
    let events = c.drain();
    assert!(
        !events.iter().any(|e| e["ev"] == "segment"),
        "a paused daemon must not announce segments: {events:?}"
    );
    // The pause itself is announced, so every view can show it without waiting
    // for its next poll.
    let status = events
        .iter()
        .find(|e| e["ev"] == "status")
        .expect("a paused daemon pushes a status event");
    assert_eq!(status["data"]["paused"], true);

    // Resume, and the flow returns.
    assert_eq!(c.call("resume", json!({}))["paused"], false);
    let after = d.ingest("clean_single_1.wav");
    assert!(!after.is_empty(), "resume must restore the pipeline");
    assert!(d.segment_rows() > rows_before);
    assert!(d.wav_count() > files_before);
    assert_eq!(c.wait_event("segment")["data"]["id"], after[0]);
}

// ---- 6. concurrency and the request surface -----------------------------

#[test]
fn many_clients_are_served_at_once_and_a_bad_request_is_only_that_clients_problem() {
    let mut d = Daemon::start("concurrent");
    let mut clients: Vec<Conn> = (0..4).map(|_| d.connect()).collect();
    for c in &mut clients {
        c.hello();
        c.subscribe(&["segments"]);
    }
    assert_eq!(d.bus.client_count(), 4);

    // One client sends nonsense; the others must not notice.
    assert_eq!(
        clients[0].call_err("speakers.name", json!({"id": 999, "name": "Ghost"}))["code"],
        "not_found"
    );

    let ids = d.ingest("clean_single_0.wav");
    for c in &mut clients {
        let ev = c.wait_event("segment");
        assert_eq!(ev["data"]["id"], ids[0]);
    }
    assert_eq!(clients[0].call("status", json!({}))["clients"], 4);
}

#[test]
fn a_source_toggle_is_live_persisted_in_the_daemon_and_broadcast() {
    let d = Daemon::start("sources");
    let mut c = d.connect();
    c.hello();
    c.subscribe(&["sources"]);

    let before = d.control.rules_generation();
    let out = c.call(
        "sources.set",
        json!({"match_key": "VRChat.exe", "allowed": true}),
    );
    assert_eq!(out["allowed"], true);
    assert!(
        d.control.rules_generation() > before,
        "the capture loop is told to re-apply its rules"
    );
    assert!(d.control.allowlist().decide("VRChat.exe").captures());
    assert_eq!(c.wait_event("source")["data"]["match_key"], "VRChat.exe");

    let listed = c.call("sources.list", json!({}));
    let vrchat = listed["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["match_key"] == "VRChat.exe")
        .expect("the new rule shows up as a source");
    assert_eq!(vrchat["allowed"], true);
}

#[test]
fn a_bulk_delete_runs_as_an_operation_with_progress_and_a_terminal_event() {
    let mut d = Daemon::start("delete");
    let mut c = d.connect();
    c.hello();
    c.subscribe(&["ops", "segments"]);
    let ids = d.ingest("clean_single_0.wav");
    c.drain();

    let preview = c.call("delete.preview", json!({}));
    assert_eq!(preview["segments"].as_u64().unwrap() as usize, ids.len());
    assert!(preview["bytes"].as_u64().unwrap() > 0);

    // Deleting everything now takes an explicit confession over the wire.
    let refused = c.call_raw("delete.run", json!({}));
    assert_eq!(refused["err"]["code"], "refused");
    let run = c.call("delete.run", json!({"confirm_everything": true}));
    let op = run["op"].as_str().unwrap().to_string();
    let purge = c.wait_event("purge");
    assert_eq!(purge["data"]["ids"].as_array().unwrap().len(), ids.len());
    let done = c.wait_event("op.done");
    assert_eq!(done["data"]["op"], op.as_str());
    assert_eq!(done["data"]["kind"], "delete.run");
    assert_eq!(
        done["data"]["removed"].as_u64().unwrap() as usize,
        ids.len()
    );

    // Soft delete: gone from the read paths, audio still on disk for the undo
    // window the sweeper enforces.
    assert!(
        c.call("transcript", json!({}))["segments"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(d.wav_count() > 0);
}

/// 0.6.4, and the bug it was written for: a voice sitting at "0 segments · 0s"
/// that Delete could not touch, because delete-by-speaker only ever scoped
/// SEGMENTS and there were none left — while the voiceprint behind it stayed
/// live and went on matching. Over the real wire, both halves of DESIGN §8's
/// choice, and the refusal that guards the pinned voice.
#[test]
fn an_empty_voice_is_deletable_over_the_wire_and_the_pin_is_not() {
    let d = Daemon::start("delete-speaker");
    let mut c = d.connect();
    c.hello();
    c.subscribe(&["relabel", "segments"]);

    let (ghost, keeper, you, seg) = {
        let store = d.store.lock().unwrap();
        let ghost = store.mint_speaker(0).unwrap();
        let keeper = store.mint_speaker(0).unwrap();
        let you = store.ensure_you_speaker(0).unwrap();
        let e = recalld::embed::Embedding::new("m@1", vec![1.0, 0.0]);
        for id in [ghost, keeper, you] {
            store.add_prototype(id, &e, None, false, 20, 0).unwrap();
        }
        // The ghost's words are already gone; the keeper still has one.
        let gone = store
            .insert_segment(d.session, 0, 1_000_000_000, "", 0)
            .unwrap();
        store
            .set_segment_speaker(gone, Some(ghost), Some(0.9))
            .unwrap();
        store.soft_delete_segments(&[gone], 1).unwrap();
        let seg = store
            .insert_segment(d.session, 2_000_000_000, 6_000_000_000, "", 0)
            .unwrap();
        store
            .set_segment_speaker(seg, Some(keeper), Some(0.9))
            .unwrap();
        (ghost, keeper, you, seg)
    };
    c.drain();

    let listed = |c: &mut Conn| -> Vec<i64> {
        c.call("speakers.list", json!({}))["speakers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_i64().unwrap())
            .collect()
    };
    assert!(listed(&mut c).contains(&ghost), "the ghost is not listed");

    // Your own voice: refused, with the switch that does work named.
    let refused = c.call_err("speakers.delete", json!({"id": you}));
    assert_eq!(refused["code"], "refused");
    assert!(
        refused["msg"].as_str().unwrap().contains("microphone"),
        "{refused}"
    );

    // The ghost: nothing to purge, and it goes anyway.
    let out = c.call("speakers.delete", json!({"id": ghost}));
    assert_eq!(out["segments"], json!(0));
    assert_eq!(out["removed_speaker"], json!(true));
    let ev = c.wait_event("relabel");
    assert_eq!(ev["data"]["speaker"], json!(ghost));
    assert_eq!(ev["data"]["pruned"], json!(true));
    assert!(!listed(&mut c).contains(&ghost));

    // The other half of the choice: the words go, the voice stays.
    let out = c.call(
        "speakers.delete",
        json!({"id": keeper, "keep_voiceprint": true}),
    );
    assert_eq!(out["segments"], json!(1));
    assert_eq!(out["removed_speaker"], json!(false));
    let purge = c.wait_event("purge");
    assert_eq!(purge["data"]["ids"], json!([seg]));
    assert!(listed(&mut c).contains(&keeper), "the kept voice vanished");
    assert!(
        c.call("transcript", json!({"speaker": keeper}))["segments"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    {
        let store = d.store.lock().unwrap();
        assert_eq!(store.prototype_count(keeper).unwrap(), 1);
        assert!(
            store
                .prototypes("m@1")
                .unwrap()
                .iter()
                .any(|(sp, _)| *sp == keeper),
            "a kept voiceprint has to go on matching"
        );
        assert_eq!(store.prototype_count(ghost).unwrap(), 0);
    }
}

// ---- 7. the field conventions a JavaScript client depends on -------------

#[test]
fn timestamps_travel_as_both_a_number_and_a_string() {
    // A JSON number cannot hold UTC nanoseconds: 1.8e18 is past 2^53 and every
    // JavaScript client would silently round it. So `t_ms` is the number to
    // render and `t_ns` is the string that is actually true.
    let mut d = Daemon::start("timestamps");
    let mut c = d.connect();
    c.hello();
    c.subscribe(&["segments"]);
    // A realistic wall-clock start, not zero: the point is the magnitude.
    d.clock_ns = 1_788_201_960_000_000_000;
    let ids = d.ingest("clean_single_0.wav");

    let ev = c.wait_event("segment");
    let data = &ev["data"];
    let t_ns: i64 = data["t_ns"]
        .as_str()
        .expect("t_ns must be a string")
        .parse()
        .expect("t_ns must parse as an integer");
    assert!(t_ns >= 1_788_201_960_000_000_000);
    assert_eq!(data["t_ms"].as_i64().unwrap(), t_ns / 1_000_000);
    assert!(data["dur_ms"].as_i64().unwrap() > 0);
    assert_eq!(data["source"], "fixtures");
    assert!(data["has_audio"].as_bool().unwrap());

    // The same row, the same way, through the request path.
    let seg = &c.call("transcript", json!({}))["segments"][0];
    assert_eq!(seg["id"], ids[0]);
    assert_eq!(seg["t_ns"], data["t_ns"]);
    assert_eq!(seg["t_ms"], data["t_ms"]);

    // And a client may filter with an ISO-8601 window, which is what a browser
    // produces from a `t_ms` it already has.
    let window = c.call(
        "transcript",
        json!({"from": "2026-08-31T00:00:00.000Z", "to": "2026-09-01T00:00:00Z"}),
    );
    assert_eq!(window["segments"].as_array().unwrap().len(), ids.len());
    let empty = c.call("transcript", json!({"from": "2027-01-01T00:00:00Z"}));
    assert!(empty["segments"].as_array().unwrap().is_empty());
    // Milliseconds are accepted as a number too, and read as milliseconds.
    let by_ms = c.call("transcript", json!({"from": 1_788_201_000_000i64}));
    assert_eq!(by_ms["segments"].as_array().unwrap().len(), ids.len());
    assert_eq!(
        c.call_err("transcript", json!({"from": "last tuesday"}))["code"],
        "params"
    );
}

#[test]
fn an_unnamed_voice_is_distinguishable_from_a_named_one() {
    let d = Daemon::start("unnamed");
    let mut c = d.connect();
    c.hello();
    let (a, b) = {
        let store = d.store.lock().unwrap();
        (
            store.mint_speaker(0).unwrap(),
            store.mint_speaker(0).unwrap(),
        )
    };

    c.call("speakers.name", json!({"id": b, "name": "Ines"}));
    let rows = c.call("speakers.list", json!({}));
    let of = |id: i64| -> Value {
        rows["speakers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id)
            .cloned()
            .expect("speaker is listed")
    };
    // The onboarding question is "who are these people?", so an un-named voice
    // must say so rather than reporting its generated label as a name.
    assert_eq!(of(a)["name"], Value::Null);
    assert_eq!(of(a)["auto"], "Speaker_01");
    assert_eq!(of(b)["name"], "Ines");
    assert_eq!(of(b)["auto"], "Speaker_02");
    assert!(of(a)["first_seen"].as_str().unwrap().ends_with('Z'));
}

#[test]
fn sources_report_what_they_are_when_they_were_seen_and_whether_they_are_live() {
    let d = Daemon::start("source-rows");
    let mut c = d.connect();
    c.hello();
    let row = c.call("sources.list", json!({}))["sources"][0].clone();
    assert_eq!(row["match_key"], "fixtures");
    assert_eq!(row["binary"], "fixtures");
    assert_eq!(row["display"], "fixtures");
    assert!(row["first_seen"].as_str().unwrap().ends_with('Z'));
    assert!(row["last_seen"].as_str().unwrap().ends_with('Z'));
    // The rig opened a session for this source and never closed it, which is
    // exactly what "capturing right now" means.
    assert_eq!(row["streams"], 1);
    assert_eq!(row["allowed"], true);
    assert_eq!(c.call("status", json!({}))["sources_capturing"], 1);

    // Denying it stops counting it as capturing without touching the session
    // rows — the live rule is what the status line reports.
    c.call(
        "sources.set",
        json!({"match_key": "fixtures", "allowed": false}),
    );
    let status = c.call("status", json!({}));
    assert_eq!(status["sources_allowed"], 0);
    assert_eq!(status["sources_capturing"], 0);

    c.call(
        "sources.set",
        json!({"match_key": "fixtures", "allowed": true}),
    );
    let status = c.call("status", json!({}));
    assert_eq!(status["sources_allowed"], 1);
    assert_eq!(status["sources_capturing"], 1);
    assert!(status["uptime_s"].as_i64().is_some());
    assert!(status["segments_total"].as_i64().is_some());
    assert!(status["models"].is_array());
}

#[test]
fn a_restarted_daemon_starts_its_sequence_over_and_says_so() {
    // The client's rule (PROTOCOL "Handshake"): a welcome whose seq is lower
    // than what it has already applied means the daemon restarted, and it
    // resyncs. That only works if the counter really does start fresh and the
    // events after it really do increase from there.
    let mut d = Daemon::start("restart");
    let mut before = d.connect();
    before.hello();
    before.subscribe(&["segments"]);
    d.ingest("clean_single_0.wav");
    let high = before.wait_event("segment")["seq"].as_u64().unwrap();
    assert!(high >= 1);

    // The daemon goes away and comes back over the same database.
    let path = d.socket.clone();
    d.server.shutdown();
    assert!(before.eof(), "a stopping daemon hangs up on its clients");
    d.bus = Bus::new(64, 32);
    let service = Service::new(
        Arc::clone(&d.store),
        Arc::clone(&d.control),
        Arc::clone(&d.bus),
    );
    d.server = server::serve(service, &path).expect("re-binding after a restart");

    let mut after = d.connect();
    let welcome = after.hello();
    assert_eq!(
        welcome["seq"], 0,
        "a restarted daemon counts from zero again"
    );
    after.subscribe(&["segments"]);
    let ids = d.ingest("clean_single_1.wav");
    let ev = after.wait_event("segment");
    assert_eq!(ev["seq"], 1, "and the next event is 1, not {high} + 1");
    assert_eq!(ev["data"]["id"], ids[0]);
    // The rows survived the restart even though the sequence did not.
    let t = after.call("transcript", json!({}));
    assert!(t["segments"].as_array().unwrap().len() > ids.len());
}

// ---- 8. playing a voice back --------------------------------------------

/// The question the feature answers is "who is this?", and it cannot be
/// answered by reading. This is the whole path: pipeline writes a WAV, a
/// client asks for it by segment id, and the bytes that come back over the
/// socket are the bytes on disk.
#[test]
fn a_segments_audio_crosses_the_socket_byte_for_byte() {
    let mut d = Daemon::start("audio");
    let ids = d.ingest("clean_single_0.wav");
    let seg = *ids.first().expect("the fixture produces a segment");

    let mut c = d.connect();
    c.hello();
    let out = c.call("segments.audio", json!({"id": seg}));

    let rel = {
        let store = d.store.lock().unwrap();
        store.segment_audio(seg).unwrap().unwrap().0
    };
    let on_disk = std::fs::read(d.dir.join(&rel)).expect("the pipeline wrote a WAV");
    assert_eq!(
        out["wav_b64"].as_str().unwrap(),
        recalld::b64::encode(&on_disk),
        "the wire payload is not the file"
    );
    assert_eq!(out["id"], seg);
    assert_eq!(out["sample_rate"], 16_000, "segments are stored at 16 kHz");
    assert_eq!(out["bytes"], on_disk.len());
    assert!(
        out["duration_ms"].as_i64().unwrap() > 0,
        "a clip with no length cannot be listened to"
    );

    // And an id that never existed is an error the client can branch on.
    assert_eq!(
        c.call_err("segments.audio", json!({"id": 987654}))["code"],
        "not_found"
    );
}

/// A frame far bigger than anything a real segment produces still arrives
/// whole. This is the limit half of the decision: the daemon caps the file it
/// will encode, and both ends carry a frame budget above what that cap can
/// produce — a reply that gets truncated or hangs a client up is worse than a
/// refusal.
#[test]
fn a_multi_megabyte_reply_arrives_in_one_piece() {
    let mut d = Daemon::start("audio-big");
    let ids = d.ingest("clean_single_0.wav");
    let seg = *ids.first().unwrap();
    let rel = {
        let store = d.store.lock().unwrap();
        store.segment_audio(seg).unwrap().unwrap().0
    };
    // 2 MB of audio — about a minute of 16 kHz mono, twice the segment cap,
    // and ~2.7 MB once base64'd.
    let big: Vec<f32> = (0..1_000_000)
        .map(|i| ((i % 97) as f32 / 97.0) - 0.5)
        .collect();
    recalld::pipeline::write_wav(&d.dir.join(&rel), &big).unwrap();

    let mut c = d.connect();
    c.hello();
    let out = c.call("segments.audio", json!({"id": seg}));
    assert_eq!(out["bytes"], 2_000_044);
    assert_eq!(
        out["wav_b64"].as_str().unwrap().len(),
        recalld::b64::encoded_len(2_000_044),
        "the payload was truncated on the way through"
    );
    // The connection is still usable afterwards, which is the thing a broken
    // frame guard would take away.
    assert!(c.call("status", json!({}))["daemon"].as_str().is_some());
}

/// Retention outlives the recording on purpose: the text stays, the WAV goes.
/// A client asking for audio that has aged out must be told *that*, not
/// "no such segment" — the difference is the difference between a bug and a
/// setting the user chose.
#[test]
fn audio_that_aged_out_answers_gone_over_the_wire() {
    let mut d = Daemon::start("audio-retention");
    let ids = d.ingest("clean_single_0.wav");
    let seg = *ids.first().unwrap();
    let rel = {
        let store = d.store.lock().unwrap();
        store.segment_audio(seg).unwrap().unwrap().0
    };
    std::fs::remove_file(d.dir.join(&rel)).unwrap();

    let mut c = d.connect();
    c.hello();
    let e = c.call_err("segments.audio", json!({"id": seg}));
    assert_eq!(e["code"], "gone");
    assert!(
        e["msg"].as_str().unwrap().contains("retention"),
        "the message must name the reason: {}",
        e["msg"]
    );
    // The transcript still has the row — that is the point of the distinction.
    let t = c.call("transcript", json!({}));
    assert!(
        t["segments"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"] == seg)
    );
}

/// The naming query: given a voice, hand back the clips worth listening to,
/// each one playable through `segments.audio`.
#[test]
fn speakers_sample_offers_playable_clips_for_naming() {
    let mut d = Daemon::start("sample");
    let mut ids = d.ingest("clean_single_0.wav");
    ids.extend(d.ingest("clean_single_1.wav"));
    assert!(
        ids.len() >= 2,
        "two fixtures should give at least two clips"
    );
    let spk = {
        let store = d.store.lock().unwrap();
        let spk = store.mint_speaker(0).unwrap();
        for (i, id) in ids.iter().enumerate() {
            store
                .set_segment_speaker(*id, Some(spk), Some(0.5 + i as f32 / 100.0))
                .unwrap();
        }
        spk
    };

    let mut c = d.connect();
    c.hello();
    let out = c.call("speakers.sample", json!({"id": spk, "limit": 2}));
    let samples = out["samples"].as_array().unwrap().clone();
    assert_eq!(samples.len(), 2);
    // Long first: that is what makes a clip worth playing to identify someone.
    let durs: Vec<i64> = samples
        .iter()
        .map(|s| s["duration_ms"].as_i64().unwrap())
        .collect();
    assert!(
        durs[0] >= durs[1],
        "samples are not longest-first: {durs:?}"
    );

    // Every clip offered actually plays — the promise the file check makes.
    for s in &samples {
        let audio = c.call("segments.audio", json!({"id": s["segment_id"]}));
        assert!(audio["wav_b64"].as_str().unwrap().starts_with("UklG"));
    }
}

// ---- 9. a live daemon for external clients ------------------------------

/// Not part of the suite — an interop harness for real clients.
///
/// ```text
/// NX_RECALL_SOCK=/tmp/nxr.sock NXR_INTEROP_SECS=120 \
///   cargo test --test socket -- --ignored --nocapture interop_daemon
/// ```
///
/// It binds the real socket, feeds the golden fixtures through the real
/// pipeline on a timer, and holds the daemon open so the Electron GUI (or any
/// other client) can be pointed at it with `NX_RECALL_SOCK`. Everything still
/// lives in a throwaway directory; the user's own daemon is never touched.
#[test]
#[ignore = "interop harness: run it explicitly with --ignored"]
fn interop_daemon() {
    let mut d = Daemon::start("interop");
    if let Some(path) = std::env::var_os("NX_RECALL_SOCK") {
        let path = PathBuf::from(path);
        d.server.shutdown();
        let _ = std::fs::remove_file(&path);
        let service = Service::new(
            Arc::clone(&d.store),
            Arc::clone(&d.control),
            Arc::clone(&d.bus),
        );
        d.server = server::serve(service, &path).expect("binding the requested socket");
        d.socket = path;
    }
    let seconds: u64 = std::env::var("NXR_INTEROP_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);
    // The wiring `cmd_run` does, so `status` reports what a real daemon would.
    if let Some(models) = models_dir() {
        let mut cfg = Config::default();
        cfg.models.dir = Some(models);
        if let Some(mut set) = ModelSet::resolve(&cfg.models) {
            set.select_asr();
            if set.complete() {
                d.control
                    .set_models(vec![set.asr_model_id(), set.embed_model_id()]);
            }
        }
    }
    {
        let store = d.store.lock().unwrap();
        store
            .upsert_source("fixtures", "fixtures", recalld::clock::utc_now_ns())
            .unwrap();
    }
    // A wall-clock start, so a client's date formatting has something real.
    d.clock_ns = recalld::clock::utc_now_ns() - 3_600_000_000_000;

    println!("interop daemon on {}", d.socket.display());
    println!("data dir {}", d.dir.display());
    println!(
        "models: {}",
        if d.analyzer.is_some() {
            "loaded"
        } else {
            "none"
        }
    );

    let fixtures = [
        "clean_single_0.wav",
        "clean_single_1.wav",
        "duo_dominant_0.wav",
        "duo_equal_0.wav",
    ];
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut i = 0usize;
    while Instant::now() < deadline {
        let ids = d.ingest(fixtures[i % fixtures.len()]);
        println!("fed {} -> {ids:?}", fixtures[i % fixtures.len()]);
        i += 1;
        std::thread::sleep(Duration::from_millis(2000));
    }
    println!("interop daemon stopping");
}

#[test]
fn the_daemons_own_client_speaks_the_same_protocol() {
    // `recalld pause` goes through this path, so it is worth one test that the
    // shipped client and the shipped server agree.
    let d = Daemon::start("cli-client");
    let mut client = recalld::client::Client::connect(&d.socket).unwrap();
    assert_eq!(client.welcome["proto"], 1);
    assert_eq!(client.call("pause", json!({})).unwrap()["paused"], true);
    assert!(d.control.is_paused());
    assert_eq!(client.call("resume", json!({})).unwrap()["paused"], false);
    assert!(client.call("nope", json!({})).is_err());
}

// ---- the memory graph, Tiers 2 and 3 over the wire (0.7.0) --------------

/// A conversation with a promise in it, written the way the pipeline writes
/// one: transcript, speaker, thread, then the Tier 2 pass. No models needed —
/// Tier 2 is rules, which is the point of it.
fn seed_a_promise(d: &Daemon) -> (i64, i64, i64) {
    let store = d.store.lock().unwrap();
    let a = store.mint_speaker(0).unwrap();
    let b = store.mint_speaker(0).unwrap();
    let mut at = 0i64;
    let mut say = |who: i64, text: &str| {
        at += 5;
        let t = at * 1_000_000_000;
        let id = store
            .insert_segment(d.session, t, t + 3_000_000_000, "", t)
            .unwrap();
        store
            .set_segment_analysis(
                id,
                &recalld::store::SegmentAnalysis {
                    text: Some(text.into()),
                    ..Default::default()
                },
            )
            .unwrap();
        store.set_segment_speaker(id, Some(who), Some(0.7)).unwrap();
        recalld::threads::assign(&store, &recalld::config::GraphConfig::default(), id).unwrap();
        recalld::commitment::extract(&store, id, 1).unwrap();
        id
    };
    say(a, "hast du das Video noch?");
    let promise = say(b, "ja klar, ich schick dir morgen den Link");
    (a, b, promise)
}

#[test]
fn the_graph_answers_its_summary_and_lists_what_the_rules_found() {
    let d = Daemon::start("graph-summary");
    let (a, b, promise) = seed_a_promise(&d);
    let mut c = d.connect();
    c.hello();

    // The whole view in one round trip, like the person page.
    let summary = c.call("graph.summary", json!({}));
    assert_eq!(summary["counts"]["commitments"], json!(1));
    assert_eq!(summary["counts"]["open"], json!(1));
    assert_eq!(summary["counts"]["candidates"], json!(1));
    assert_eq!(summary["counts"]["from_rules"], json!(1));
    assert_eq!(summary["counts"]["from_llm"], json!(0));
    assert_eq!(summary["counts"]["time_refs"], json!(1), "\"morgen\"");
    // Tier 3 ships off, and the summary says so rather than implying anything.
    assert_eq!(summary["config"]["enabled"], json!(false));
    assert_eq!(summary["enrichment"]["phase"], json!("off"));

    let listed = c.call("commitments.list", json!({}));
    let rows = listed["commitments"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row["segment"], json!(promise));
    assert_eq!(row["who"]["speaker_id"], json!(b));
    assert_eq!(row["to"]["speaker_id"], json!(a));
    assert_eq!(row["state"], json!("candidate"));
    // The two fields a client must never conflate: which tier claimed this,
    // and what a person has decided about it.
    assert_eq!(row["source"], json!("rules"));
    assert!(row["confidence"].as_f64().unwrap() < 0.5, "{row}");
    // The evidence travels with the claim.
    assert_eq!(
        row["said"],
        json!("ja klar, ich schick dir morgen den Link")
    );
    assert_eq!(row["due_raw"], json!("morgen"));
    assert!(row["due_ns"].is_string(), "nanoseconds travel as strings");
    assert!(row["due_ms"].as_i64().unwrap() > 0);

    // Filtering by a state nobody is in is an empty list, not an error.
    assert!(
        c.call("commitments.list", json!({"state": "done"}))["commitments"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let err = c.call_err("commitments.list", json!({"state": "maybe"}));
    assert_eq!(err["code"], json!("params"));
}

/// Only a person moves a commitment, and every other client hears about it.
#[test]
fn a_commitment_state_change_is_broadcast_to_every_client() {
    let d = Daemon::start("graph-state");
    seed_a_promise(&d);
    let mut c = d.connect();
    c.hello();
    let mut watcher = d.connect();
    watcher.hello();
    watcher.subscribe(&["ops"]);

    let id = c.call("commitments.list", json!({}))["commitments"][0]["id"]
        .as_i64()
        .unwrap();

    let confirmed = c.call(
        "commitments.set_state",
        json!({"id": id, "state": "confirmed"}),
    );
    assert_eq!(confirmed["state"], json!("confirmed"));
    let evt = watcher.wait_event("commitment");
    assert_eq!(evt["data"]["id"], json!(id));
    assert_eq!(evt["data"]["state"], json!("confirmed"));

    // Confirmed is still open; done is not.
    assert_eq!(
        c.call("graph.summary", json!({}))["counts"]["open"],
        json!(1)
    );
    c.call("commitments.set_state", json!({"id": id, "state": "done"}));
    let after = c.call("graph.summary", json!({}))["counts"].clone();
    assert_eq!(after["open"], json!(0));
    assert_eq!(after["done"], json!(1));

    assert_eq!(
        c.call_err("commitments.set_state", json!({"id": id, "state": "nope"}))["code"],
        json!("params")
    );
    assert_eq!(
        c.call_err(
            "commitments.set_state",
            json!({"id": 99_999, "state": "done"})
        )["code"],
        json!("not_found")
    );
}

/// The Tier 3 switch, over the wire: live, refused nothing, and honest about
/// the model not being installed.
#[test]
fn the_tier_three_switch_is_live_and_says_whether_the_model_is_there() {
    let d = Daemon::start("graph-switch");
    let mut c = d.connect();
    c.hello();

    let before = c.call("graph.get", json!({}));
    assert_eq!(
        before["config"]["enabled"],
        json!(false),
        "off is the default"
    );
    assert_eq!(before["config"]["installed"], json!(false));
    // The copy in a client should not have to hard-code what it costs.
    assert!(before["config"]["download_bytes"].as_u64().unwrap() > 1_000_000_000);

    let on = c.call("graph.enrich", json!({"action": "start"}));
    assert_eq!(on["config"]["enabled"], json!(true));
    assert!(d.control.graph().enabled);
    // Nothing runs anyway: the model is not installed, and the worker says so
    // rather than pretending.
    assert_eq!(
        c.call("graph.set", json!({"llm_threads": 2}))["config"]["llm_threads"],
        json!(2)
    );
    assert_eq!(d.control.graph().llm_threads, 2);
    // 0.7.2: how much of the machine the model may use is the setting that
    // replaced standing down while a game runs, so the range travels with it —
    // a client builds its control out of what the daemon will accept.
    assert_eq!(before["config"]["llm_threads_min"], json!(1));
    assert_eq!(before["config"]["llm_threads_max"], json!(32));
    // Clamped at both ends, never refused: the reply says what is now true.
    assert_eq!(
        c.call("graph.set", json!({"llm_threads": 0}))["config"]["llm_threads"],
        json!(1)
    );
    assert_eq!(
        c.call("graph.set", json!({"llm_threads": 4096}))["config"]["llm_threads"],
        json!(32)
    );
    assert_eq!(d.control.graph().llm_threads, 32);

    let off = c.call("graph.enrich", json!({"action": "stop"}));
    assert_eq!(off["config"]["enabled"], json!(false));
    assert!(!d.control.graph().enabled);
    // Asking twice for what is already true changes nothing and is not an error.
    assert_eq!(
        c.call("graph.enrich", json!({"action": "stop"}))["changed"],
        json!(false)
    );
    assert_eq!(
        c.call_err("graph.enrich", json!({"action": "sideways"}))["code"],
        json!("params")
    );
    assert_eq!(c.call_err("graph.set", json!({}))["code"], json!("params"));
}

/// Topics are the model's work, so with Tier 3 off the list is empty rather
/// than absent — a client renders "nothing yet", not an error.
#[test]
fn topics_are_empty_until_something_has_named_a_conversation() {
    let d = Daemon::start("graph-topics");
    let (_, _, promise) = seed_a_promise(&d);
    let mut c = d.connect();
    c.hello();
    assert!(
        c.call("topics.list", json!({}))["topics"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    // Write one the way the enrichment pass would.
    let thread = {
        let store = d.store.lock().unwrap();
        let thread = store
            .segment_row(promise)
            .unwrap()
            .unwrap()
            .thread_id
            .unwrap();
        store
            .set_thread_topic(thread, Some("video link"), "qwen2.5-3b-test", 5)
            .unwrap();
        thread
    };
    let topics = c.call("topics.list", json!({}));
    let rows = topics["topics"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["topic"], json!("video link"));
    assert_eq!(rows[0]["threads"], json!(1));
    assert_eq!(rows[0]["segments"], json!(2));
    assert_eq!(rows[0]["thread_ids"], json!([thread]));
    assert!(rows[0]["last_ns"].is_string());
    assert_eq!(
        c.call("graph.summary", json!({}))["counts"]["topics"],
        json!(1)
    );
}

/// Deletion means deletion, over the wire as well as in the store: purging the
/// person takes what they promised with them.
#[test]
fn deleting_a_voice_takes_its_commitments_off_the_wire_too() {
    let d = Daemon::start("graph-delete");
    let (_, b, _) = seed_a_promise(&d);
    let mut c = d.connect();
    c.hello();
    assert_eq!(
        c.call("commitments.list", json!({}))["commitments"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    c.call(
        "speakers.delete",
        json!({"id": b, "keep_voiceprint": false}),
    );
    assert!(
        c.call("commitments.list", json!({}))["commitments"]
            .as_array()
            .unwrap()
            .is_empty(),
        "a promise outlived the person who made it"
    );
    assert_eq!(
        c.call("graph.summary", json!({}))["counts"]["commitments"],
        json!(0)
    );
}

/// Saved items use the same wire dispatch as the desktop, and saved moments
/// always resolve current source rows rather than copying deleted/corrected text.
#[test]
fn saved_searches_moments_and_context_roundtrip_over_the_socket() {
    let d = Daemon::start("saved-context-v014");
    let ids = {
        let store = d.store.lock().unwrap();
        (0..3)
            .map(|i| {
                let t = (i + 1) * 1_000_000_000;
                let id = store
                    .insert_segment(d.session, t, t + 500_000_000, "", t)
                    .unwrap();
                store
                    .correct_segment_text(id, &format!("synthetic portal turn {i}"))
                    .unwrap();
                id
            })
            .collect::<Vec<_>>()
    };
    let mut c = d.connect();
    c.hello();
    c.subscribe(&["ops"]);

    let filters = json!({"mode":"keyword","source":"fixtures","date":{"kind":"rolling","days":7}});
    let search = c.call(
        "saved.searches.save",
        json!({"name":"Portals this week","query":"portal","filters":filters}),
    )["search"]
        .clone();
    assert!(search["id"].as_i64().unwrap() > 0);
    let searches = c.call("saved.searches.list", json!({"limit":10}));
    assert_eq!(searches["total"], 1);
    assert_eq!(searches["searches"][0]["filters"], filters);
    assert_eq!(searches["searches"][0]["query"], "portal");
    let renamed = c.call(
        "saved.searches.save",
        json!({"id":search["id"],"name":"Portal notes","query":"portal","filters":filters}),
    );
    assert_eq!(renamed["search"]["id"], search["id"]);
    assert_eq!(c.call("saved.searches.list", json!({}))["total"], 1);

    let context = c.call(
        "segments.context",
        json!({"id":ids[1],"before":1,"after":1}),
    );
    assert_eq!(context["anchor"]["id"], ids[1]);
    assert_eq!(
        context["segments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_i64().unwrap())
            .collect::<Vec<_>>(),
        ids
    );
    let moment = c.call(
        "saved.moments.save",
        json!({"title":"Portal discussion","note":"Synthetic socket test","segment_ids":ids}),
    )["moment"]
        .clone();
    assert_eq!(moment["segment_ids"], json!(ids));
    assert_eq!(moment["unavailable_count"], 0);
    c.call(
        "segments.correct",
        json!({"segment_id":ids[1],"text":"corrected synthetic portal"}),
    );
    let moments = c.call("saved.moments.list", json!({}));
    assert_eq!(moments["total"], 1);
    assert_eq!(
        moments["moments"][0]["segments"][1]["text"],
        "corrected synthetic portal"
    );

    let deletion = c.call(
        "delete.run",
        json!({"session":d.session,"from":1000,"to":2000}),
    );
    let done = c.wait_event("op.done");
    assert_eq!(done["data"]["op"], deletion["op"]);
    assert_eq!(done["data"]["removed"], 1);
    let moments = c.call("saved.moments.list", json!({}));
    assert_eq!(moments["moments"][0]["id"], moment["id"]);
    assert_eq!(
        moments["moments"][0]["segment_ids"],
        json!([ids[1], ids[2]])
    );
    assert_eq!(moments["moments"][0]["unavailable_count"], 1);
    assert_eq!(
        c.call_err("segments.context", json!({"id":ids[0]}))["code"],
        "not_found"
    );
    let remaining = c.call(
        "segments.context",
        json!({"id":ids[1],"before":10,"after":10}),
    );
    assert_eq!(remaining["segments"].as_array().unwrap().len(), 2);
    assert!(
        remaining["segments"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["id"] != ids[0])
    );

    let deletion = c.call("delete.run", json!({"session":d.session}));
    assert_eq!(c.wait_event("op.done")["data"]["op"], deletion["op"]);
    assert_eq!(c.call("saved.moments.list", json!({}))["total"], 0);
    assert_eq!(
        c.call("saved.searches.delete", json!({"id":search["id"]}))["removed"],
        true
    );
    assert_eq!(c.call("saved.searches.list", json!({}))["total"], 0);
}
