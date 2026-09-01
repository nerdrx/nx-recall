//! The microphone as a capture source, end to end through the real pipeline.
//!
//! Live capture of an actual device is not testable here and is not attempted:
//! the mic tap's PipeWire half is exercised by `capture::mic_plan`'s unit tests
//! (the state machine as a pure function) and by `recalld probe` (which resolves
//! the default source and records nothing). What *is* tested here is everything
//! downstream of "audio arrived on a session whose source is kind = mic":
//!
//! * the voicebank is never consulted, and `match_score` stays NULL;
//! * the overlap gate still runs, and its answer is still stored;
//! * enrolment is gated exactly as the matching leg's is, and the goldens it
//!   writes are capped, longest-first, and outside the retention sweep;
//! * global pause covers the mic like everything else;
//! * the protocol surface says all of that out loud.
//!
//! Every rig gets its own temporary data directory and its own socket inside
//! it. Nothing here touches the live daemon.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use recalld::allowlist::Allowlist;
use recalld::analysis::{Analyzer, golden_path};
use recalld::bus::Bus;
use recalld::capture::{MIC_DISPLAY_NAME, MIC_MATCH_KEY};
use recalld::config::RetentionConfig;
use recalld::config::{Config, MicMode};
use recalld::control::Control;
use recalld::ingest::{OfflinePipeline, ingest_pcm, read_wav};
use recalld::models::ModelSet;
use recalld::retention;
use recalld::server::{self, Server};
use recalld::service::Service;
use recalld::store::{KIND_APP, KIND_MIC, Store, YOU_AUTO_LABEL};
use recalld::vad::SileroVad;

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
}

/// `NXR_MODELS` if it is set and absolute, exactly as the socket suite reads it.
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

struct Rig {
    dir: PathBuf,
    socket: PathBuf,
    store: Arc<Mutex<Store>>,
    control: Arc<Control>,
    bus: Arc<Bus>,
    server: Server,
    /// A session on the microphone source.
    mic_session: i64,
    /// A session on an ordinary application source, for the contrast.
    app_session: i64,
    vad: SileroVad,
    cfg: Config,
    analyzer: Option<Analyzer>,
    clock_ns: i64,
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.server.shutdown();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Rig {
    fn start(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("nx-recall-mic-{}-{name}", std::process::id()));
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
        let mic_source = store
            .upsert_source_kind(MIC_MATCH_KEY, MIC_DISPLAY_NAME, KIND_MIC, 0)
            .unwrap();
        let app_source = store.upsert_source("VRChat.exe", "VRChat", 0).unwrap();
        let mic_session = store.begin_session(mic_source, 0).unwrap();
        let app_session = store.begin_session(app_source, 0).unwrap();
        let store = Arc::new(Mutex::new(store));

        let control = Control::new(
            dir.clone(),
            None,
            &Allowlist::from_rules([("VRChat.exe", true)]),
        )
        .with_identity(cfg.identity.clone())
        .with_mic(cfg.mic.clone());
        let bus = Bus::new(cfg.socket.replay_events, cfg.socket.client_outbox);
        let service = Service::new(Arc::clone(&store), Arc::clone(&control), Arc::clone(&bus));
        let socket = dir.join("s");
        let server = server::serve(service, &socket).expect("binding the isolated socket");

        Self {
            dir,
            socket,
            store,
            control,
            bus,
            server,
            mic_session,
            app_session,
            vad: SileroVad::from_bytes(recalld::VAD_MODEL).expect("bundled VAD model"),
            cfg,
            analyzer,
            clock_ns: 0,
        }
    }

    /// One fixture through the real pipeline, on the given session. The mic
    /// route is chosen by `ingest_pcm` from the session's `sources.kind`, so
    /// this is the same decision the live daemon makes.
    fn ingest(&mut self, session: i64, fixture: &str) -> Vec<i64> {
        let samples =
            read_wav(&fixtures_dir().join(fixture)).unwrap_or_else(|e| panic!("{fixture}: {e:#}"));
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
        ingest_pcm(&store, &self.dir, session, &samples, t0, &mut pipe)
            .unwrap_or_else(|e| panic!("ingesting {fixture}: {e:#}"))
    }

    fn mic(&mut self, fixture: &str) -> Vec<i64> {
        self.ingest(self.mic_session, fixture)
    }

    fn app(&mut self, fixture: &str) -> Vec<i64> {
        self.ingest(self.app_session, fixture)
    }

    fn you(&self) -> Option<i64> {
        self.store.lock().unwrap().you_speaker_id().unwrap()
    }

    fn row(&self, id: i64) -> recalld::store::SegmentRow {
        self.store
            .lock()
            .unwrap()
            .segment_row(id)
            .unwrap()
            .unwrap_or_else(|| panic!("no segment {id}"))
    }

    fn goldens(&self, speaker: i64) -> Vec<recalld::store::GoldenRow> {
        self.store
            .lock()
            .unwrap()
            .golden_samples_for(speaker)
            .unwrap()
    }

    fn prototypes(&self, speaker: i64) -> i64 {
        self.store.lock().unwrap().prototype_count(speaker).unwrap()
    }

    fn connect(&self) -> Conn {
        Conn::connect(&self.socket)
    }

    /// The analysis leg is what enrolment and goldens live on; without models
    /// there is nothing to assert about them.
    fn needs_models(&self, what: &str) -> bool {
        if self.analyzer.is_none() {
            eprintln!("skipping {what}: NXR_MODELS is not set");
            return false;
        }
        true
    }
}

/// A minimal scripted NDJSON client — enough to exercise the mic methods.
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
        let mut c = Self {
            reader: BufReader::new(s.try_clone().unwrap()),
            writer: s,
            next_id: 1,
            pending: std::collections::VecDeque::new(),
        };
        c.line(r#"{"hello":{"proto":1,"client":"mic-test/1"}}"#);
        let welcome = c.read();
        assert_eq!(welcome["welcome"]["schema"], recalld::store::SCHEMA_VERSION);
        c
    }

    fn line(&mut self, text: &str) {
        self.writer.write_all(text.as_bytes()).unwrap();
        self.writer.write_all(b"\n").unwrap();
        self.writer.flush().unwrap();
    }

    fn read(&mut self) -> Value {
        if let Some(msg) = self.pending.pop_front() {
            return msg;
        }
        let mut line = String::new();
        let n = self.reader.read_line(&mut line).expect("reading a reply");
        assert!(n > 0, "the daemon closed the connection unexpectedly");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON: {line:?} ({e})"))
    }

    fn call_raw(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.line(&json!({"id": id, "method": method, "params": params}).to_string());
        let mut skipped = Vec::new();
        loop {
            let msg = self.read();
            if msg["id"] == json!(id) {
                for m in skipped.into_iter().rev() {
                    self.pending.push_front(m);
                }
                return msg;
            }
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

    fn subscribe(&mut self, topics: &[&str]) {
        self.call("subscribe", json!({"topics": topics}));
    }

    fn wait_event(&mut self, ev: &str) -> Value {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut skipped = Vec::new();
        while std::time::Instant::now() < deadline {
            let msg = self.read();
            if msg["ev"] == ev {
                for m in skipped.into_iter().rev() {
                    self.pending.push_front(m);
                }
                return msg;
            }
            skipped.push(msg);
        }
        panic!("no {ev} event arrived; saw {skipped:?}");
    }
}

// ---- 1. provenance, not a match -----------------------------------------

#[test]
fn a_mic_segment_is_labelled_without_ever_asking_the_voicebank() {
    let mut r = Rig::start("pinned");
    assert_eq!(r.you(), None, "no You until the microphone speaks");

    let ids = r.mic("clean_single_0.wav");
    assert!(!ids.is_empty(), "the fixture must produce segments");
    let you = r.you().expect("the first mic segment pins a You speaker");

    for id in &ids {
        let row = r.row(*id);
        assert_eq!(
            row.speaker_id,
            Some(you),
            "every mic turn belongs to the user by construction"
        );
        // The heart of it: a score would claim a comparison that never
        // happened. NULL is the honest answer, and the correction UI reads it.
        assert_eq!(
            row.match_score, None,
            "a mic label is provenance, not a match score"
        );
    }

    // The pinned voice reads as an unnamed voice the daemon labelled, which is
    // exactly what it is until the user calls themselves something.
    let summary = r
        .store
        .lock()
        .unwrap()
        .speaker_summary(you)
        .unwrap()
        .unwrap();
    assert_eq!(summary.auto_label, YOU_AUTO_LABEL);
    assert_eq!(summary.name(), None);

    // A second mic turn reuses the pin rather than minting.
    let before = r.store.lock().unwrap().list_speakers().unwrap().len();
    let more = r.mic("clean_single_1.wav");
    assert_eq!(r.you(), Some(you));
    assert_eq!(
        r.store.lock().unwrap().list_speakers().unwrap().len(),
        before
    );
    for id in &more {
        assert_eq!(r.row(*id).speaker_id, Some(you));
    }
}

#[test]
fn an_application_turn_still_goes_through_the_voicebank() {
    let mut r = Rig::start("contrast");
    if !r.needs_models("the app/mic contrast") {
        return;
    }
    // The same audio down both routes. The mic one is pinned; the app one is
    // matched or minted, and either way it is a *decision* with a score.
    let mic_ids = r.mic("clean_single_0.wav");
    let you = r.you().unwrap();
    let app_ids = r.app("opus24_single_1.wav");

    assert!(!app_ids.is_empty());
    let scored = app_ids
        .iter()
        .map(|id| r.row(*id))
        .filter(|row| row.speaker_id.is_some())
        .collect::<Vec<_>>();
    assert!(
        !scored.is_empty(),
        "the app fixture should have produced at least one labelled turn"
    );
    for row in &scored {
        assert_ne!(
            row.speaker_id,
            Some(you),
            "an application turn must never be labelled as the user"
        );
    }
    assert_eq!(r.row(mic_ids[0]).match_score, None);
}

#[test]
fn a_mic_segment_keeps_its_overlap_fraction_even_though_the_name_is_certain() {
    let mut r = Rig::start("overlap");
    if !r.needs_models("the mic overlap gate") {
        return;
    }
    // Speakers-bleed: the user runs loudspeakers and the mic hears the room
    // talking back. The name is still right — it is the user's microphone —
    // but the audio is not clean, and the UI has to be able to see that.
    let ids = r.mic("duo_equal_0.wav");
    assert!(!ids.is_empty());
    let you = r
        .you()
        .expect("even an overlapped mic turn pins the speaker");

    let heavy = ids
        .iter()
        .map(|id| r.row(*id))
        .filter(|row| row.overlap_frac.unwrap_or(0.0) > r.cfg.identity.max_overlap)
        .collect::<Vec<_>>();
    assert!(
        !heavy.is_empty(),
        "the equal-loudness fixture must read as overlapped: {:?}",
        ids.iter()
            .map(|id| r.row(*id).overlap_frac)
            .collect::<Vec<_>>()
    );
    for row in &heavy {
        assert_eq!(row.speaker_id, Some(you), "the label survives the overlap");
        assert_eq!(row.match_score, None);
        assert!(
            row.overlap_frac.is_some(),
            "and the frac is kept for the UI"
        );
    }
    // Refused audio is never embedded, so it can never have enrolled.
    assert_eq!(
        r.prototypes(you),
        0,
        "an overlapped mic turn must not reach the voicebank"
    );
    assert!(r.goldens(you).is_empty());
}

// ---- 2. the free enrolment ----------------------------------------------

#[test]
fn a_clean_mic_turn_enrols_and_a_short_one_does_not() {
    let mut r = Rig::start("enrol");
    if !r.needs_models("mic enrolment") {
        return;
    }
    // 1.75 s: long enough to label (min_duration_s = 1.0), too short to enrol
    // (enroll_min_duration_s = 3.0). The gates are the matching leg's, minus
    // the two that are about identifying rather than about audio quality.
    let short = r.mic("clean_single_1.wav");
    let you = r.you().unwrap();
    assert!(short.iter().all(|id| r.row(*id).speaker_id == Some(you)));
    assert_eq!(
        r.prototypes(you),
        0,
        "a two-second 'yeah' identifies nobody, mic or not"
    );
    assert!(r.goldens(you).is_empty());

    // 5.17 s, clean: this is the payoff.
    r.mic("clean_single_0.wav");
    assert!(
        r.prototypes(you) > 0,
        "a clean long mic turn is free perfect enrolment"
    );
    assert_eq!(
        r.goldens(you).len(),
        1,
        "and the first one is kept for a future model migration"
    );
}

#[test]
fn goldens_are_written_once_capped_and_kept_longest_first() {
    let mut r = Rig::start("goldens");
    if !r.needs_models("mic goldens") {
        return;
    }
    r.cfg.mic.max_goldens = 2;

    // Three qualifying turns of different lengths, shortest first.
    r.mic("opus24_single_0.wav"); // 3.40 s
    let you = r.you().unwrap();
    r.mic("clean_single_0.wav"); // 5.17 s
    let two = r.goldens(you);
    assert_eq!(two.len(), 2, "under the cap, everything qualifying is kept");
    assert!(two[0].duration_s >= two[1].duration_s, "longest first");
    for g in &two {
        assert!(
            g.audio_path.starts_with("goldens/"),
            "goldens live outside segments/: {}",
            g.audio_path
        );
        assert!(
            r.dir.join(&g.audio_path).is_file(),
            "the file is really there"
        );
    }

    let shortest = two.last().unwrap().clone();
    r.mic("opus24_single_1.wav"); // 8.68 s — beats the shortest
    let after = r.goldens(you);
    assert_eq!(after.len(), 2, "the cap holds");
    assert!(
        after.iter().all(|g| g.id != shortest.id),
        "the shortest was replaced, not appended"
    );
    assert!(
        !r.dir.join(&shortest.audio_path).exists(),
        "and its file went with the row"
    );
    assert!(after[0].duration_s > shortest.duration_s);

    // Re-analysing a turn that already has a golden does not write a second
    // copy of it: the path carries the segment id, so the write is idempotent
    // per segment however many times the analysis leg runs over it.
    let longest = r.goldens(you).first().unwrap().clone();
    let segment_id = golden_segment_id(&longest.audio_path);
    assert_eq!(
        golden_path(you, segment_id).to_string_lossy(),
        longest.audio_path
    );
    let samples = read_wav(&r.dir.join(r.row(segment_id).audio_path)).unwrap();
    {
        let store = r.store.lock().unwrap();
        r.analyzer
            .as_mut()
            .unwrap()
            .process_mic(
                &store,
                segment_id,
                &samples,
                &recalld::analysis::MicEnroll {
                    speaker_id: you,
                    data_dir: &r.dir,
                    max_goldens: r.cfg.mic.max_goldens,
                },
                0,
            )
            .unwrap();
    }
    let again = r.goldens(you);
    assert_eq!(again.len(), 2, "no duplicate for the same segment");
    assert!(
        again.iter().any(|g| g.id == longest.id),
        "and none replaced"
    );
}

/// `goldens/<speaker>/golden-<segment>.wav` -> the segment id.
fn golden_segment_id(rel: &str) -> i64 {
    rel.rsplit('/')
        .next()
        .and_then(|f| f.strip_prefix("golden-"))
        .and_then(|f| f.strip_suffix(".wav"))
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("unexpected golden path {rel:?}"))
}

#[test]
fn a_golden_outlives_the_retention_window_that_takes_the_segment() {
    let mut r = Rig::start("golden-retention");
    if !r.needs_models("golden retention") {
        return;
    }
    r.mic("clean_single_0.wav");
    let you = r.you().unwrap();
    let golden = r.goldens(you);
    assert_eq!(golden.len(), 1);
    let kept = r.dir.join(&golden[0].audio_path);
    assert!(kept.is_file());

    // A sweep that ages out every segment and reconciles every loose file. The
    // goldens directory is not under segments/, so the sweeper never walks it
    // and never has to know it exists (DESIGN §6).
    let cfg = RetentionConfig {
        audio_days: 1,
        undo_window_days: 0,
        reconcile: true,
        ..Default::default()
    };
    let far_future = 400i64 * 86_400 * 1_000_000_000;
    let report = {
        let store = r.store.lock().unwrap();
        retention::sweep(&cfg, &store, &r.dir, far_future).unwrap()
    };
    assert!(
        report.aged_audio > 0,
        "the segments' audio really did age out"
    );
    assert!(
        kept.is_file(),
        "the golden survived: it is what a future model gets re-enrolled from"
    );
    assert_eq!(r.goldens(you).len(), 1, "and so did its row");
}

// ---- 3. global pause covers the mic -------------------------------------

#[test]
fn global_pause_stops_the_microphone_like_everything_else() {
    let mut r = Rig::start("pause");
    r.control.pause();

    assert!(r.mic("clean_single_0.wav").is_empty());
    assert!(r.app("clean_single_0.wav").is_empty());
    assert_eq!(
        r.store
            .lock()
            .unwrap()
            .transcript(None, None)
            .unwrap()
            .len(),
        0,
        "no rows while paused"
    );
    assert_eq!(r.you(), None, "and no You minted from audio never written");
    assert!(
        !r.dir.join("goldens").exists(),
        "nothing kept from a paused microphone either"
    );

    r.control.resume();
    let ids = r.mic("clean_single_0.wav");
    assert!(!ids.is_empty(), "resuming writes again");
    assert_eq!(r.row(ids[0]).speaker_id, r.you());
}

// ---- 4. the protocol surface --------------------------------------------

#[test]
fn the_microphone_switch_is_its_own_method_and_not_an_allowlist_rule() {
    let r = Rig::start("proto-switch");
    let mut c = r.connect();

    let off = c.call("mic.get", json!({}));
    assert_eq!(off["enabled"], json!(false), "off by default — the feature");
    assert_eq!(off["mode"], json!("follow"), "and following, not always");
    assert_eq!(off["state"], json!("off"));
    assert_eq!(off["active"], json!(false));
    assert_eq!(off["you_speaker"], Value::Null);

    // The allowlist is emphatically not the way in: an app rule is consent
    // about one program's output, and this device hears the room.
    let refused = c.call_err("sources.set", json!({"match_key": "mic", "allowed": true}));
    assert_eq!(refused["code"], "refused");
    assert!(
        refused["msg"].as_str().unwrap().contains("mic.set"),
        "the refusal has to name the method that works: {}",
        refused["msg"]
    );
    assert_eq!(
        c.call("mic.get", json!({}))["enabled"],
        json!(false),
        "and it really did not take effect"
    );

    // The real way in.
    let on = c.call("mic.set", json!({"enabled": true}));
    assert_eq!(on["enabled"], json!(true));
    assert_eq!(
        on["state"], "following:idle",
        "enabled, waiting for an allowed application — not recording"
    );
    assert_eq!(c.call("mic.get", json!({}))["state"], "following:idle");

    let always = c.call("mic.set", json!({"mode": "always"}));
    assert_eq!(always["mode"], json!("always"));
    assert_eq!(
        always["enabled"],
        json!(true),
        "mode alone does not switch it"
    );
    assert_eq!(
        always["state"], "always:idle",
        "on, but no device opened yet"
    );

    let off = c.call("mic.set", json!({"enabled": false}));
    assert_eq!(off["state"], json!("off"));
    assert_eq!(off["mode"], json!("always"), "the mode is remembered");

    // Bad input is refused rather than defaulted: "sometimes" is not a privacy
    // model, and silently choosing one for the user would be worse than an error.
    assert_eq!(
        c.call_err("mic.set", json!({"mode": "sometimes"}))["code"],
        "params"
    );
    assert_eq!(c.call_err("mic.set", json!({}))["code"], "params");
    assert_eq!(c.call("mic.get", json!({}))["mode"], json!("always"));
}

#[test]
fn the_switch_is_broadcast_and_reaches_status_as_a_state_string() {
    let r = Rig::start("proto-events");
    let mut c = r.connect();
    c.subscribe(&["status"]);

    assert_eq!(c.call("status", json!({}))["mic_state"], json!("off"));

    c.call("mic.set", json!({"enabled": true, "mode": "always"}));
    let ev = c.wait_event("mic");
    assert_eq!(ev["data"]["enabled"], json!(true));
    assert_eq!(ev["data"]["mode"], json!("always"));
    assert_eq!(ev["data"]["state"], json!("always:idle"));
    assert!(
        ev["seq"].as_u64().unwrap() >= 1,
        "every event carries a seq"
    );

    // The same answer through the polled method, in both shapes.
    let st = c.call("status", json!({}));
    assert_eq!(st["mic_state"], json!("always:idle"));
    assert_eq!(st["mic"]["mode"], json!("always"));
    assert_eq!(st["mic"]["enabled"], json!(true));

    // The capture thread is the only thing that can say "recording right now".
    r.control.set_mic_active(true);
    assert_eq!(
        c.call("status", json!({}))["mic_state"],
        json!("always:active")
    );
}

#[test]
fn sources_and_speakers_say_which_row_is_the_microphone_and_which_voice_is_you() {
    let mut r = Rig::start("proto-rows");
    let mut c = r.connect();

    let rows = c.call("sources.list", json!({}));
    let by_key: std::collections::HashMap<&str, &Value> = rows["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["match_key"].as_str().unwrap(), s))
        .collect();
    assert_eq!(by_key["VRChat.exe"]["kind"], json!(KIND_APP));
    assert_eq!(by_key[MIC_MATCH_KEY]["kind"], json!(KIND_MIC));
    assert_eq!(by_key[MIC_MATCH_KEY]["display"], json!(MIC_DISPLAY_NAME));
    // The mic row's "allowed" tracks its own switch, never the allowlist.
    assert_eq!(by_key[MIC_MATCH_KEY]["allowed"], json!(false));
    c.call("mic.set", json!({"enabled": true}));
    let rows = c.call("sources.list", json!({}));
    let mic = rows["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["match_key"] == MIC_MATCH_KEY)
        .unwrap();
    assert_eq!(mic["allowed"], json!(true));

    // No mic audio yet, so nobody is You.
    for sp in c.call("speakers.list", json!({}))["speakers"]
        .as_array()
        .unwrap()
    {
        assert_eq!(sp["you"], json!(false));
    }

    r.mic("clean_single_0.wav");
    let you = r.you().unwrap();
    let listed = c.call("speakers.list", json!({}));
    let flagged: Vec<i64> = listed["speakers"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["you"] == json!(true))
        .map(|s| s["id"].as_i64().unwrap())
        .collect();
    assert_eq!(flagged, vec![you], "exactly one voice is the user");
    assert_eq!(c.call("mic.get", json!({}))["you_speaker"], json!(you));
}

#[test]
fn merging_you_into_a_named_voice_moves_the_pin_over_the_socket_too() {
    let mut r = Rig::start("proto-merge");
    let mut c = r.connect();
    r.mic("clean_single_0.wav");
    let you = r.you().unwrap();

    let kira = r.store.lock().unwrap().create_speaker("Kira", 0).unwrap();
    c.call("speakers.merge", json!({"from": you, "into": kira}));

    // The tombstone stays a tombstone: the next mic segment lands on Kira, and
    // no second "You" is minted behind the user's back.
    assert_eq!(r.you(), Some(kira));
    assert_eq!(c.call("mic.get", json!({}))["you_speaker"], json!(kira));
    let ids = r.mic("clean_single_1.wav");
    assert!(ids.iter().all(|id| r.row(*id).speaker_id == Some(kira)));
    assert_eq!(
        r.store.lock().unwrap().list_speakers().unwrap().len(),
        1,
        "one person, one voice"
    );

    let listed = c.call("speakers.list", json!({}));
    let flagged: Vec<i64> = listed["speakers"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["you"] == json!(true))
        .map(|s| s["id"].as_i64().unwrap())
        .collect();
    assert_eq!(flagged, vec![kira]);
}

#[test]
fn the_switch_survives_a_daemon_restart_through_the_config() {
    // `mic.set` persists into config.toml; a restart reads it back and the
    // capture thread starts from it. Exercised here at the Config level,
    // because that is the whole mechanism.
    let dir = std::env::temp_dir().join(format!("nx-recall-mic-persist-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("config.toml");

    let mut cfg = Config::load(&path).unwrap();
    assert!(
        !cfg.mic.enabled,
        "a machine with no config captures nothing"
    );
    cfg.mic.enabled = true;
    cfg.mic.mode = MicMode::Always;
    cfg.save(&path).unwrap();

    let back = Config::load(&path).unwrap();
    let control =
        Control::new(dir.clone(), Some(path.clone()), &back.allowlist()).with_mic(back.mic.clone());
    assert_eq!(control.mic_state(), "always:idle");
    control.set_mic_active(true);
    assert_eq!(control.mic_state(), "always:active");
    let _ = std::fs::remove_dir_all(&dir);
}
