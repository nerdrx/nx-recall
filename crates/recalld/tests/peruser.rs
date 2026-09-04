//! Per-user Discord audio, end to end without Discord (0.12.1).
//!
//! Four claims are tested, and they are the four the feature is:
//!
//! 1. **The endpoint.** Auth, the body limit, the switch, and a sequence gap
//!    being visible rather than silently spliced over.
//! 2. **A frame becomes a turn.** A WAV fixture, cut into 500 ms frames,
//!    base64'd, POSTed over the real loopback ingest, and coming out of the
//!    real pipeline as a segment on a `discord-user` source.
//! 3. **Whose turn it is.** Pinned to the voice linked to that account —
//!    minted and linked on the spot if there was none — with
//!    `label_via = "discord-stream"` and no `match_score`, because nothing was
//!    compared.
//! 4. **The mixed tap goes quiet.** While a per-user stream is live the mixed
//!    Discord source's audio is discarded, so the same speech is not written
//!    twice; when the streams stop it comes back.
//!
//! No models are loaded for most of it: the fixture is real speech and the VAD
//! is the bundled one, so "a segment appeared, on the right source, pinned to
//! the right voice" is decidable without ASR or an embedder. The one test that
//! needs the embedder says so and skips itself.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use recalld::b64;
use recalld::clock::utc_now_ns;
use recalld::config::{Config, SAMPLE_RATE};
use recalld::ingest::read_wav;
use recalld::peruser::{AudioStats, PerUser, display_name, is_mixed_discord_source, match_key};
use recalld::pipeline::Pipeline;
use recalld::queue::EventQueue;
use recalld::store::{KIND_DISCORD_USER, Store, label_via, truth_via};
use recalld::truth::TruthStats;
use recalld::truthnet;

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
}

// ---------------------------------------------------------------------------
// the rig
// ---------------------------------------------------------------------------

/// A daemon's audio path, minus the daemon: a store, the one queue every
/// source pushes at, the real `Pipeline` draining it on its own thread, and the
/// real HTTP ingest in front of it.
struct Rig {
    dir: PathBuf,
    store: Arc<std::sync::Mutex<Store>>,
    queue: Arc<EventQueue>,
    peruser: Arc<PerUser>,
    stats: Arc<AudioStats>,
    ingest: Option<truthnet::Ingest>,
    inference: Option<std::thread::JoinHandle<()>>,
    port: u16,
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.finish();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Rig {
    fn start(name: &str, audio: bool) -> Self {
        let dir =
            std::env::temp_dir().join(format!("nx-recall-peruser-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creating the throwaway data dir");

        let mut cfg = Config::default();
        cfg.truth.enabled = true;
        cfg.truth.audio = audio;
        // Short, because the tests are: the janitor is called by hand.
        cfg.truth.audio_live_s = 2.0;
        cfg.truth.audio_idle_s = 1;

        let store = Arc::new(std::sync::Mutex::new(
            Store::open(&dir).expect("opening the throwaway database"),
        ));
        let queue = EventQueue::for_seconds(30.0, SAMPLE_RATE);
        let stats = Arc::new(AudioStats::default());
        let peruser = Arc::new(PerUser::new(
            Arc::clone(&store),
            Arc::clone(&queue),
            cfg.truth.clone(),
            Arc::clone(&stats),
        ));

        let control = recalld::control::Control::new(
            dir.clone(),
            None,
            &recalld::allowlist::Allowlist::default(),
        );
        let bus = recalld::bus::Bus::new(64, 32);
        let mut pipeline = Pipeline::new(
            &cfg,
            Arc::clone(&store),
            dir.clone(),
            Arc::new(recalld::pipeline::Stats::default()),
            Arc::new(recalld::analysis::AnalysisStats::default()),
            Arc::clone(&control),
            Arc::clone(&bus),
        )
        .expect("building the pipeline");
        pipeline.attach_peruser(Arc::clone(&peruser));

        let q = Arc::clone(&queue);
        let inference = std::thread::Builder::new()
            .name("test-vad".into())
            .spawn(move || pipeline.run(q))
            .expect("spawning the inference thread");

        let ingest = truthnet::serve(
            Arc::clone(&store),
            Arc::new(TruthStats::default()),
            TOKEN.to_string(),
            0,
            Some(Arc::clone(&peruser)),
        )
        .expect("binding the ingest on an ephemeral port");
        let port = ingest.addr().port();

        Self {
            dir,
            store,
            queue,
            peruser,
            stats,
            ingest: Some(ingest),
            inference: Some(inference),
            port,
        }
    }

    fn store(&self) -> std::sync::MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Close every stream, drain the queue and stop the pipeline. Idempotent,
    /// because `Drop` calls it too.
    fn finish(&mut self) {
        if let Some(i) = self.ingest.take() {
            i.shutdown();
        }
        self.peruser.close_all();
        self.queue.close();
        if let Some(h) = self.inference.take() {
            let _ = h.join();
        }
    }

    /// One POST, returning the status and the body.
    fn post(&self, path: &str, body: &str, auth: Option<&str>) -> (u16, String) {
        let mut stream =
            TcpStream::connect(("127.0.0.1", self.port)).expect("connecting to the ingest");
        let auth = match auth {
            Some(t) => format!("Authorization: Bearer {t}\r\n"),
            None => String::new(),
        };
        let req = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}Content-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(req.as_bytes()).unwrap();
        stream.flush().unwrap();

        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let status = line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        loop {
            let mut h = String::new();
            if reader.read_line(&mut h).unwrap() == 0 || h.trim().is_empty() {
                break;
            }
        }
        let mut out = String::new();
        let _ = reader.read_to_string(&mut out);
        (status, out)
    }

    /// Wait until the pipeline has caught up with what has been pushed.
    fn drain(&self) {
        for _ in 0..600 {
            if self.queue.queued_samples() == 0 {
                // The consumer may still be mid-turn; a beat is enough, and the
                // loop below re-checks anyway.
                std::thread::sleep(std::time::Duration::from_millis(20));
                if self.queue.queued_samples() == 0 {
                    return;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("the pipeline never drained the queue");
    }

    /// Every live turn on one source. The database is fresh per test, so
    /// walking the ids is exact and needs no query the daemon does not have.
    fn segments_of(&self, match_key: &str) -> Vec<recalld::store::SegmentRow> {
        let store = self.store();
        let total = store.segments_total().unwrap();
        (1..=total)
            .filter_map(|id| store.segment_row(id).ok().flatten())
            .filter(|r| r.source == match_key)
            .collect()
    }
}

// ---------------------------------------------------------------------------
// framing a fixture
// ---------------------------------------------------------------------------

/// One NDJSON line: `frame` as little-endian PCM16, base64.
fn line(user: &str, name: &str, t_ms: i64, seq: u64, frame: &[f32]) -> String {
    let bytes: Vec<u8> = frame
        .iter()
        .flat_map(|s| ((s.clamp(-1.0, 1.0) * 32_767.0) as i16).to_le_bytes())
        .collect();
    serde_json::json!({
        "t_ms": t_ms,
        "user_id": user,
        "name": name,
        "channel_id": "chan-1",
        "rate": SAMPLE_RATE,
        "seq": seq,
        "pcm": b64::encode(&bytes),
    })
    .to_string()
}

/// A fixture cut into 500 ms frames, as a batch of lines, starting at `t0_ms`.
fn frames(user: &str, name: &str, t0_ms: i64, samples: &[f32]) -> (Vec<String>, usize) {
    let per = (SAMPLE_RATE / 2) as usize; // 500 ms
    let mut out = Vec::new();
    for (seq, chunk) in samples.chunks(per).enumerate() {
        let t = t0_ms + (seq * 500) as i64;
        out.push(line(user, name, t, seq as u64, chunk));
    }
    let n = out.len();
    (out, n)
}

fn fixture(name: &str) -> Vec<f32> {
    read_wav(&fixtures_dir().join(name)).unwrap_or_else(|e| panic!("{name}: {e:#}"))
}

// ---------------------------------------------------------------------------
// 1. the endpoint
// ---------------------------------------------------------------------------

#[test]
fn the_audio_route_needs_the_token_and_never_reads_a_body_without_one() {
    let mut rig = Rig::start("auth", true);
    let body = line("u1", "Aspen", 1_000, 0, &[0.0; 8_000]) + "\n";

    let (status, _) = rig.post("/v1/discord/audio", &body, None);
    assert_eq!(status, 401, "no Authorization header at all");
    let (status, _) = rig.post("/v1/discord/audio", &body, Some("wrong"));
    assert_eq!(status, 401, "the wrong token");

    // Nothing was taken, so no session and no source row exist.
    assert_eq!(rig.stats.frames.load(Ordering::Relaxed), 0);
    assert!(rig.store().discord_user("u1").unwrap().is_none());

    let (status, _) = rig.post("/v1/discord/audio", &body, Some(TOKEN));
    assert_eq!(status, 204, "an accepted batch says nothing back");
    assert_eq!(rig.stats.frames.load(Ordering::Relaxed), 1);
    rig.finish();
}

#[test]
fn a_body_over_the_limit_is_refused_rather_than_truncated() {
    let mut rig = Rig::start("toobig", true);
    // A megabyte and change: half a batch of NDJSON is not a smaller batch.
    let big = "x".repeat(truthnet::MAX_BODY + 1);
    let (status, _) = rig.post("/v1/discord/audio", &big, Some(TOKEN));
    assert_eq!(status, 413);
    assert_eq!(
        rig.stats.frames.load(Ordering::Relaxed),
        0,
        "nothing from an oversized body may reach the pipeline"
    );
    rig.finish();
}

#[test]
fn audio_arriving_while_the_switch_is_off_is_counted_and_refused() {
    // The daemon's own switch, separate from the plugin's: turning the ingest
    // on to measure speaker accuracy must not silently start recording audio
    // that arrived over a socket.
    let mut rig = Rig::start("switchoff", false);
    let body = line("u1", "Aspen", 1_000, 0, &[0.1; 8_000]) + "\n";
    let (status, _) = rig.post("/v1/discord/audio", &body, Some(TOKEN));
    assert_eq!(
        status, 204,
        "a fire-and-forget client cannot act on an error"
    );
    assert_eq!(rig.stats.frames.load(Ordering::Relaxed), 0);
    assert!(
        rig.store().discord_user("u1").unwrap().is_none(),
        "a refused frame must not even record that the account exists"
    );
    rig.finish();
}

#[test]
fn a_sequence_gap_is_seen_and_counted() {
    let mut rig = Rig::start("gap", true);
    let quiet = vec![0.0f32; 8_000];
    for (seq, body) in [(0u64, &quiet), (1, &quiet), (5, &quiet)] {
        let (status, _) = rig.post(
            "/v1/discord/audio",
            &(line("u1", "Aspen", 1_000 + seq as i64 * 500, seq, body) + "\n"),
            Some(TOKEN),
        );
        assert_eq!(status, 204);
    }
    assert_eq!(rig.stats.frames.load(Ordering::Relaxed), 3);
    assert_eq!(
        rig.stats.gaps.load(Ordering::Relaxed),
        1,
        "seq 1 -> 5 is one hole; the first frame of a stream is not one"
    );
    rig.finish();
}

#[test]
fn one_malformed_line_does_not_poison_the_batch() {
    let mut rig = Rig::start("malformed", true);
    let good = line("u1", "Aspen", 1_000, 0, &[0.0; 8_000]);
    let body = format!("{good}\nnot json at all\n{{\"t_ms\":1}}\n");
    let (status, _) = rig.post("/v1/discord/audio", &body, Some(TOKEN));
    assert_eq!(status, 204);
    assert_eq!(
        rig.stats.frames.load(Ordering::Relaxed),
        1,
        "the good line still landed"
    );
    assert_eq!(rig.stats.rejected.load(Ordering::Relaxed), 2);
    rig.finish();
}

// ---------------------------------------------------------------------------
// 2 & 3. a frame becomes somebody's turn
// ---------------------------------------------------------------------------

#[test]
fn a_posted_wav_becomes_a_turn_on_a_discord_user_source_pinned_to_a_minted_voice() {
    let mut rig = Rig::start("e2e", true);
    let samples = fixture("clean_single_0.wav");
    let t0 = recalld::clock::ns_to_ms(utc_now_ns());
    let (lines, n) = frames("777", "Aspen", t0, &samples);
    assert!(n > 1, "the fixture must be longer than one frame");

    let (status, _) = rig.post("/v1/discord/audio", &(lines.join("\n") + "\n"), Some(TOKEN));
    assert_eq!(status, 204);
    assert_eq!(rig.stats.frames.load(Ordering::Relaxed), n as u64);
    assert_eq!(rig.stats.gaps.load(Ordering::Relaxed), 0);

    // A voice was minted for the account and linked on the spot.
    let user = rig
        .store()
        .discord_user("777")
        .unwrap()
        .expect("the account");
    assert_eq!(user.name, "Aspen");
    let speaker = user.speaker_id.expect("a voice was minted and linked");
    assert_eq!(
        user.via.as_deref(),
        Some(truth_via::DISCORD_STREAM),
        "the link says how it was made"
    );
    assert_eq!(rig.stats.voices_minted.load(Ordering::Relaxed), 1);
    // And it is NOT called Aspen. A nickname is not a name for a voice.
    let name = rig.store().speaker_name(speaker).unwrap();
    assert!(
        name.as_deref().is_none_or(|n| n.starts_with("Speaker_")),
        "a Discord nickname must never be applied to a voice, got {name:?}"
    );

    // The source row.
    let sources = rig.store().list_sources().unwrap();
    let row = sources
        .iter()
        .find(|s| s.match_key == match_key("777"))
        .expect("a per-user source row");
    assert_eq!(row.kind, KIND_DISCORD_USER);
    assert_eq!(row.display_name, display_name("Aspen"));

    // The turn. Closing the stream flushes the last one.
    rig.peruser.close_all();
    rig.drain();
    let rows = rig.segments_of(&match_key("777"));
    assert!(!rows.is_empty(), "the fixture produced no turns");
    for r in &rows {
        assert_eq!(r.source, match_key("777"));
        assert_eq!(
            r.speaker_id,
            Some(speaker),
            "every turn on this stream is that account's voice"
        );
        assert_eq!(
            r.label_via.as_deref(),
            Some(label_via::DISCORD_STREAM),
            "provenance, and it says which kind"
        );
        assert_eq!(
            r.match_score, None,
            "nothing was compared, so there is no score to report"
        );
    }

    // The wall clock the frames carried is the wall clock the turns are on.
    let first = rows.first().unwrap();
    let t0_ns = t0 * 1_000_000;
    let drift_ms = (first.t_start_ns - t0_ns).abs() / 1_000_000;
    assert!(
        drift_ms < 1_500,
        "a turn must land where its frames said it was, not where the HTTP \
         thread got to it: {drift_ms} ms from the first frame's t_ms"
    );
    rig.finish();
}

#[test]
fn an_account_already_linked_by_hand_keeps_its_voice() {
    let mut rig = Rig::start("prelinked", true);
    let mine = {
        let store = rig.store();
        let id = store.create_speaker("Aspen", 0).unwrap();
        store.upsert_discord_user("777", "Aspen", 0).unwrap();
        store
            .set_discord_link("777", Some(id), Some(truth_via::MANUAL), 0)
            .unwrap();
        id
    };

    let samples = fixture("clean_single_0.wav");
    let t0 = recalld::clock::ns_to_ms(utc_now_ns());
    let (lines, _) = frames("777", "Aspen", t0, &samples);
    rig.post("/v1/discord/audio", &(lines.join("\n") + "\n"), Some(TOKEN));
    rig.peruser.close_all();
    rig.drain();

    assert_eq!(
        rig.stats.voices_minted.load(Ordering::Relaxed),
        0,
        "a linked account must not get a second voice minted for it"
    );
    let user = rig.store().discord_user("777").unwrap().unwrap();
    assert_eq!(user.speaker_id, Some(mine));
    assert_eq!(
        user.via.as_deref(),
        Some(truth_via::MANUAL),
        "a hand link is never overwritten"
    );
    for r in rig.segments_of(&match_key("777")) {
        assert_eq!(r.speaker_id, Some(mine));
    }
    rig.finish();
}

// ---------------------------------------------------------------------------
// 4. the de-duplication rule
// ---------------------------------------------------------------------------

#[test]
fn the_mixed_discord_tap_is_muted_while_a_per_user_stream_is_live() {
    let mut rig = Rig::start("dedup", true);
    let samples = fixture("clean_single_0.wav");

    // The mixed tap: an ordinary application source, exactly as the daemon
    // creates it for vesktop today.
    let mixed = rig.store().upsert_source("vesktop", "Vesktop", 0).unwrap();

    // Before any per-user audio: the mixed tap writes turns, as it always has.
    push_session(&rig, mixed, &samples);
    rig.drain();
    let before = rig.segments_of("vesktop").len();
    assert!(before > 0, "the mixed tap must work when nothing else does");

    // Now a per-user stream arrives.
    let t0 = recalld::clock::ns_to_ms(utc_now_ns());
    let (lines, _) = frames("777", "Aspen", t0, &samples);
    rig.post("/v1/discord/audio", &(lines.join("\n") + "\n"), Some(TOKEN));
    assert!(rig.peruser.any_live(), "a frame just arrived");

    // …and the same speech through the mixed tap is discarded rather than
    // written a second time. Drained BEFORE the streams are closed, because
    // the mute is read when a buffer is handled: closing first would leave the
    // pipeline handling the mixed audio in a world where nothing is live any
    // more, which is the fallback case and not this one.
    push_session(&rig, mixed, &samples);
    rig.drain();
    let during = rig.segments_of("vesktop").len();
    assert_eq!(
        during,
        before,
        "the mixed tap wrote {} extra turn(s) for speech the per-user stream \
         already carried",
        during - before
    );
    // The per-user stream, meanwhile, did write them.
    rig.peruser.close_all();
    rig.drain();
    assert!(
        !rig.segments_of(&match_key("777")).is_empty(),
        "the per-user source must be the one that carries the call"
    );
    rig.finish();
}

#[test]
fn the_mixed_tap_comes_back_when_the_streams_stop() {
    let mut rig = Rig::start("fallback", true);
    let samples = fixture("clean_single_0.wav");
    let mixed = rig.store().upsert_source("vesktop", "Vesktop", 0).unwrap();

    let t0 = recalld::clock::ns_to_ms(utc_now_ns());
    let (lines, _) = frames("777", "Aspen", t0, &samples);
    rig.post("/v1/discord/audio", &(lines.join("\n") + "\n"), Some(TOKEN));
    assert!(rig.peruser.any_live());

    // `audio_live_s` is 2 s in the rig. Past it, nothing is live any more, and
    // the mixed tap is the only thing carrying the call again — which is the
    // whole fallback: a plugin that is switched off mid-call costs seconds, not
    // the evening.
    std::thread::sleep(std::time::Duration::from_millis(2_100));
    assert!(
        !rig.peruser.any_live(),
        "a stream with no frames for longer than audio_live_s is not live"
    );

    push_session(&rig, mixed, &samples);
    rig.peruser.close_all();
    rig.drain();
    assert!(
        !rig.segments_of("vesktop").is_empty(),
        "the mixed tap must resume when no per-user stream is arriving"
    );
    rig.finish();
}

#[test]
fn nothing_is_muted_when_the_feature_is_off() {
    let mut rig = Rig::start("offnomute", false);
    let samples = fixture("clean_single_0.wav");
    let mixed = rig.store().upsert_source("vesktop", "Vesktop", 0).unwrap();
    assert!(!rig.peruser.any_live());
    push_session(&rig, mixed, &samples);
    rig.drain();
    assert!(
        !rig.segments_of("vesktop").is_empty(),
        "with [truth].audio off, the mixed tap is exactly what it was before"
    );
    rig.finish();
}

/// One capture session on `source_id`, fed through the real queue in 500 ms
/// chunks and then closed — which is what a capture thread does, and what makes
/// the last turn get written instead of sitting in the merger's window.
fn push_session(rig: &Rig, source_id: i64, samples: &[f32]) {
    let session_id = {
        let store = rig.store();
        store.begin_session(source_id, utc_now_ns()).unwrap()
    };
    let per = (SAMPLE_RATE / 2) as usize;
    let base = recalld::clock::monotonic_ns();
    let ns_of = |i: usize| (i * per) as u64 * 1_000_000_000 / SAMPLE_RATE as u64;
    for (i, chunk) in samples.chunks(per).enumerate() {
        rig.queue.push(recalld::queue::CaptureEvent::Audio(
            recalld::queue::AudioChunk {
                session_id,
                capture_mono_ns: base + ns_of(i),
                samples: chunk.to_vec(),
            },
        ));
    }
    rig.queue.push(recalld::queue::CaptureEvent::SessionEnd {
        session_id,
        mono_ns: base + ns_of(samples.len().div_ceil(per)),
    });
}

// ---------------------------------------------------------------------------
// the rules that are not about the wire
// ---------------------------------------------------------------------------

#[test]
fn a_per_user_turn_bridges_threads_like_the_microphone_does() {
    // One call is now one session per person. Without the bridge a four-handed
    // conversation would thread as four monologues that never answer each
    // other, which is worse than what the mixed tap gave.
    assert!(recalld::store::kind_bridges_threads(KIND_DISCORD_USER));
    assert!(recalld::store::kind_bridges_threads(
        recalld::store::KIND_MIC
    ));
    assert!(!recalld::store::kind_bridges_threads(
        recalld::store::KIND_APP
    ));
}

#[test]
fn the_sql_bridge_test_agrees_with_the_function() {
    // `Store::segment_turn` repeats `kind_bridges_threads` in SQL. Adding a
    // kind to one and not the other compiles, passes the assertion above, and
    // silently does not bridge — so the two are compared against a real row.
    let dir = std::env::temp_dir().join(format!("nx-recall-bridge-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let store = Store::open(&dir).unwrap();
    for (key, kind) in [
        ("mic", recalld::store::KIND_MIC),
        ("room", recalld::store::KIND_ROOM),
        ("discord:1", KIND_DISCORD_USER),
        ("VRChat.exe", recalld::store::KIND_APP),
    ] {
        let src = store.upsert_source_kind(key, key, kind, 0).unwrap();
        let session = store.begin_session(src, 0).unwrap();
        let seg = store.insert_segment(session, 0, 1_000, "x.wav", 0).unwrap();
        let (_, bridges, _) = store.segment_turn(seg).unwrap().unwrap();
        assert_eq!(
            bridges,
            recalld::store::kind_bridges_threads(kind),
            "the SQL and the function disagree about {kind}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_per_user_source_is_never_read_as_the_mixed_tap() {
    let cfg = Config::default().truth;
    assert!(is_mixed_discord_source(&cfg, "vesktop"));
    assert!(!is_mixed_discord_source(&cfg, &match_key("777")));
}
