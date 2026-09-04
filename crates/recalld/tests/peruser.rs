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

/// An older plugin's streams: no `client` on the wire, so no kind, so evidence
/// about every mixed instance. 0.12.2's behaviour, which is what an install
/// that has not updated its plugin still gets.
fn legacy_live() -> recalld::bridge::LiveKinds {
    let mut l = recalld::bridge::LiveKinds::default();
    l.insert(None);
    l
}

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
    /// 0.12.2: which Discord instance the mute is aimed at. Held by the rig
    /// because the tests set roles on it and read its verdicts, exactly as the
    /// socket does.
    bridge: Arc<recalld::bridge::Picker>,
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
        let bridge = Arc::new(recalld::bridge::Picker::new());
        pipeline.attach_bridge(Arc::clone(&bridge));

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
            Some(Arc::clone(&bridge)),
        )
        .expect("binding the ingest on an ephemeral port");
        let port = ingest.addr().port();

        Self {
            dir,
            store,
            queue,
            peruser,
            bridge,
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

/// The same, from a named bridge (0.12.3): the `client` object every
/// RecallBridge POST carries once two plugins can be pointed at one daemon.
fn line_from(
    user: &str,
    name: &str,
    t_ms: i64,
    seq: u64,
    frame: &[f32],
    kind: &str,
    account: &str,
) -> String {
    let mut v: serde_json::Value =
        serde_json::from_str(&line(user, name, t_ms, seq, frame)).expect("the line we just built");
    v["client"] = serde_json::json!({
        "kind": kind,
        "account_id": account,
        "instance": format!("{account}-run1"),
    });
    v.to_string()
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
fn the_bridges_own_discord_instance_is_muted_while_a_per_user_stream_is_live() {
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

    // One long-lived instance, because that is what a Discord client is: the
    // rule is per SESSION, and a new session is a new candidate that has to
    // earn its verdict rather than inherit one (0.12.2).
    let session = open_session(&rig, mixed);

    // Round one: the streams arrive and the same speech comes through the
    // mixed tap. Some of it IS written — the rule waits for evidence, and
    // recording a few seconds twice is the cheaper of the two mistakes.
    round(&rig, session, &samples);
    let learned = rig.segments_of("vesktop").len();

    // Round two: the rule now knows which instance the streams explain, and
    // not one more turn is written for speech they already carried.
    round(&rig, session, &samples);
    let during = rig.segments_of("vesktop").len();
    assert_eq!(
        during,
        learned,
        "the mixed tap wrote {} extra turn(s) for speech the per-user stream \
         already carried",
        during - learned
    );
    let v = rig
        .bridge
        .verdicts(recalld::clock::monotonic_ns(), &legacy_live())
        .into_iter()
        .find(|v| v.session_id == session)
        .expect("the instance is a candidate");
    assert!(v.muted, "{}", v.why);
    assert_eq!(v.source, "vesktop");
    assert!(
        v.share.unwrap_or(0.0) >= recalld::bridge::SHARE_BAR,
        "the share is what decided it: {:?}",
        v.share
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

/// **The bug 0.12.2 exists for.** Two Discord clients, one call each, one
/// plugin between them: the second client's call must not go silent for as
/// long as the first client's call lasts.
#[test]
fn a_second_discord_client_in_another_call_keeps_recording() {
    let mut rig = Rig::start("twoclients", true);
    let samples = fixture("clean_single_0.wav");
    // Two source rows, which is what this machine actually produces: Vesktop's
    // playback node is `vesktop`, the official client's is `Discord`
    // (FINDINGS §37). The rule does not depend on that — it works on two
    // instances of one source too — but this is the shape the user has.
    let vesktop = rig.store().upsert_source("vesktop", "Vesktop", 0).unwrap();
    let discord = rig.store().upsert_source("Discord", "Discord", 0).unwrap();
    let bridge_session = open_session(&rig, vesktop);
    let other_session = open_session(&rig, discord);

    for _ in 0..2 {
        // The bridge's client hears exactly what the streams carry.
        round(&rig, bridge_session, &samples);
        // The other client is in a different call: its speech is real, and the
        // per-user streams know nothing about it. Placed twelve seconds back
        // so it is inside the rolling window and outside every stream bucket,
        // which is what "a different call" means to this rule.
        push_audio(&rig, other_session, past_base(12_000), &samples);
        rig.drain();
    }

    let v = |id: i64| {
        rig.bridge
            .verdicts(recalld::clock::monotonic_ns(), &legacy_live())
            .into_iter()
            .find(|v| v.session_id == id)
            .expect("both instances are candidates")
    };
    let b = v(bridge_session);
    let o = v(other_session);
    assert!(b.muted, "the plugin's own client: {}", b.why);
    assert!(
        !o.muted,
        "the OTHER call must keep recording — this is the whole bug: {}",
        o.why
    );
    assert!(
        o.share.unwrap_or(1.0) < recalld::bridge::SHARE_BAR,
        "nothing explains the other call: {:?}",
        o.share
    );
    // Closing it is what writes its last turn — the verdicts above had to be
    // read first, because an ended session is forgotten on purpose.
    end_session(&rig, other_session);
    assert!(
        !rig.segments_of("Discord").is_empty(),
        "the second client's call was written down"
    );
    rig.finish();
}

/// A role of `other` beats a certain automatic verdict, and a role of `bridge`
/// mutes with no evidence at all. Both states act, and both are tested.
#[test]
fn a_manual_role_overrides_the_measurement_in_both_directions() {
    let mut rig = Rig::start("roles", true);
    let samples = fixture("clean_single_0.wav");
    let vesktop = rig.store().upsert_source("vesktop", "Vesktop", 0).unwrap();
    let discord = rig.store().upsert_source("Discord", "Discord", 0).unwrap();

    // Both roles are set the wrong way round from what the measurement would
    // say, which is the point: the user is allowed to be right about their own
    // machine, and neither state may be read and ignored.
    rig.bridge
        .set_role("vesktop", recalld::bridge::Role::Other, None);
    rig.bridge
        .set_role("Discord", recalld::bridge::Role::Bridge, None);
    assert_eq!(rig.bridge.role("vesktop"), recalld::bridge::Role::Other);
    assert_eq!(rig.bridge.role("Discord"), recalld::bridge::Role::Bridge);

    let t0 = recalld::clock::ns_to_ms(utc_now_ns());
    let (lines, _) = frames("777", "Aspen", t0, &samples);
    rig.post("/v1/discord/audio", &(lines.join("\n") + "\n"), Some(TOKEN));
    assert!(rig.peruser.any_live());

    // `vesktop` is carrying exactly the audio the streams are carrying, at
    // exactly the same instant — the automatic rule's clearest possible
    // "this is the bridge's client".
    push_session_at(&rig, vesktop, recalld::clock::monotonic_ns(), &samples);
    // `Discord` is twelve seconds away from anything the streams know about —
    // the automatic rule's clearest possible "this is another call".
    push_session_at(&rig, discord, past_base(12_000), &samples);
    rig.drain();

    assert!(
        !rig.segments_of("vesktop").is_empty(),
        "a client the user marked `other` is never muted, however sure the \
         measurement is"
    );
    assert!(
        rig.segments_of("Discord").is_empty(),
        "a client the user marked `bridge` is muted whenever streams are live, \
         with no evidence at all"
    );

    // `auto` is the absence of a role, not a third stored state.
    rig.bridge
        .set_role("vesktop", recalld::bridge::Role::Auto, None);
    rig.bridge
        .set_role("Discord", recalld::bridge::Role::Auto, None);
    assert!(rig.bridge.roles().is_empty());
    rig.finish();
}

/// The one thing this rule may never touch. A microphone is not a Discord
/// client, it is a person in a room, and no amount of per-user audio makes it
/// a duplicate of anything.
#[test]
fn neither_microphone_is_ever_muted_however_live_the_streams_are() {
    let mut rig = Rig::start("micsafe", true);
    let samples = fixture("clean_single_0.wav");
    let mic = rig
        .store()
        .upsert_source_kind(
            recalld::capture::MIC_MATCH_KEY,
            "Microphone",
            recalld::store::KIND_MIC,
            0,
        )
        .unwrap();
    let room = rig
        .store()
        .upsert_source_kind(
            recalld::room::ROOM_MATCH_KEY,
            "Room",
            recalld::store::KIND_ROOM,
            0,
        )
        .unwrap();

    let t0 = recalld::clock::ns_to_ms(utc_now_ns());
    let (lines, _) = frames("777", "Aspen", t0, &samples);
    rig.post("/v1/discord/audio", &(lines.join("\n") + "\n"), Some(TOKEN));
    assert!(rig.peruser.any_live(), "a frame just arrived");

    push_session(&rig, mic, &samples);
    push_session(&rig, room, &samples);
    rig.drain();
    assert!(
        !rig.segments_of(recalld::capture::MIC_MATCH_KEY).is_empty(),
        "the headset microphone is not a Discord client"
    );
    assert!(
        !rig.segments_of(recalld::room::ROOM_MATCH_KEY).is_empty(),
        "the room microphone is not a Discord client"
    );
    assert!(
        rig.bridge
            .verdicts(recalld::clock::monotonic_ns(), &legacy_live())
            .is_empty(),
        "neither microphone is even a candidate"
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
    let session_id = open_session(rig, source_id);
    let per = (SAMPLE_RATE / 2) as usize;
    let base = recalld::clock::monotonic_ns();
    push_audio(rig, session_id, base, samples);
    rig.queue.push(recalld::queue::CaptureEvent::SessionEnd {
        session_id,
        mono_ns: base
            + (samples.len().div_ceil(per) * per) as u64 * 1_000_000_000 / SAMPLE_RATE as u64,
    });
}

/// One capture session, left OPEN. 0.12.2's rule is per session, so a test
/// about a client that keeps running has to use one session for it: opening a
/// second would be a second client as far as the rule is concerned, and that
/// is exactly the distinction it exists to make.
fn open_session(rig: &Rig, source_id: i64) -> i64 {
    let store = rig.store();
    store.begin_session(source_id, utc_now_ns()).unwrap()
}

/// Feed `samples` into an open session in 500 ms chunks, starting at `base`.
fn push_audio(rig: &Rig, session_id: i64, base: u64, samples: &[f32]) {
    let per = (SAMPLE_RATE / 2) as usize;
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
}

/// Close an open session, which is what flushes its last turn.
fn end_session(rig: &Rig, session_id: i64) {
    rig.queue.push(recalld::queue::CaptureEvent::SessionEnd {
        session_id,
        mono_ns: recalld::clock::monotonic_ns(),
    });
    rig.drain();
}

/// One whole capture session on `source_id`, placed at `base`.
fn push_session_at(rig: &Rig, source_id: i64, base: u64, samples: &[f32]) -> i64 {
    let session_id = open_session(rig, source_id);
    push_audio(rig, session_id, base, samples);
    rig.queue.push(recalld::queue::CaptureEvent::SessionEnd {
        session_id,
        mono_ns: base + (samples.len() as u64 * 1_000_000_000) / SAMPLE_RATE as u64,
    });
    session_id
}

/// A capture instant `ago_ms` in the past. Used to place a second client's
/// call somewhere the per-user streams demonstrably are not — inside the
/// rolling window, outside every stream bucket.
fn past_base(ago_ms: u64) -> u64 {
    recalld::clock::monotonic_ns().saturating_sub(ago_ms * 1_000_000)
}

/// One round of a live call: the plugin posts everybody's audio, the same
/// speech arrives through the mixed tap at the same instant, and the pipeline
/// is drained. Timestamped at `now` on both legs, because that is what "the
/// same call through two taps" means and it is the whole input to the share.
fn round(rig: &Rig, session_id: i64, samples: &[f32]) {
    let t0 = recalld::clock::ns_to_ms(utc_now_ns());
    let (lines, _) = frames("777", "Aspen", t0, samples);
    rig.post("/v1/discord/audio", &(lines.join("\n") + "\n"), Some(TOKEN));
    assert!(rig.peruser.any_live(), "a frame just arrived");
    push_audio(rig, session_id, recalld::clock::monotonic_ns(), samples);
    rig.drain();
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

// ---------------------------------------------------------------------------
// 6. 0.12.3: two bridges
// ---------------------------------------------------------------------------

/// One person, heard by two clients. Keyed on the user id alone these two runs
/// share a stream — their sequence numbers interleave, every second frame reads
/// as a hole, and the two calls' audio lands in one session. Keyed on (account,
/// user) they are two streams, which is what they are.
#[test]
fn the_same_person_heard_by_two_bridges_is_two_streams() {
    let rig = Rig::start("two-bridges", true);
    let speech = vec![0.05f32; SAMPLE_RATE as usize / 2];
    let mut body = String::new();
    for seq in 0..2u64 {
        let t = 1_000 + (seq as i64) * 500;
        body.push_str(&line_from(
            "aspen", "Aspen", t, seq, &speech, "vesktop", "acct-v",
        ));
        body.push('\n');
        body.push_str(&line_from(
            "aspen", "Aspen", t, seq, &speech, "discord", "acct-d",
        ));
        body.push('\n');
    }
    let (status, _) = rig.post("/v1/discord/audio", &body, Some(TOKEN));
    assert_eq!(status, 204);

    let st = rig.peruser.status();
    let rows = st["streams"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "one person, two ears: {st}");
    let accounts: Vec<&str> = rows
        .iter()
        .map(|r| r["account_id"].as_str().unwrap())
        .collect();
    assert_eq!(accounts, vec!["acct-d", "acct-v"]);
    assert!(rows.iter().all(|r| r["user_id"] == "aspen"));
    assert_ne!(
        rows[0]["session_id"], rows[1]["session_id"],
        "two calls are not one session"
    );
    // Neither run saw a hole: the sequence numbers were never braided.
    assert_eq!(rig.stats.gaps.load(Ordering::Relaxed), 0);

    // And the liveness the mute rule reads says WHOSE streams, not just that
    // there are some.
    let live = rig.peruser.live_kinds();
    assert!(live.any());
    assert!(live.explains("vesktop"));
    assert!(live.explains("Discord"));
}

/// The rule, from the other side: only Vesktop's bridge is streaming, so only
/// Vesktop's tap can be a duplicate. The official client's call keeps
/// recording, whatever its share looks like.
#[test]
fn only_the_streaming_bridges_client_is_a_candidate_for_the_mute() {
    let rig = Rig::start("one-bridge-two-clients", true);
    let speech = vec![0.05f32; SAMPLE_RATE as usize / 2];
    let mut body = String::new();
    for seq in 0..2u64 {
        let t = 1_000 + (seq as i64) * 500;
        body.push_str(&line_from(
            "aspen", "Aspen", t, seq, &speech, "vesktop", "acct-v",
        ));
        body.push('\n');
    }
    let (status, _) = rig.post("/v1/discord/audio", &body, Some(TOKEN));
    assert_eq!(status, 204);

    let live = rig.peruser.live_kinds();
    assert!(
        live.explains("vesktop"),
        "the bridge's own client is the one that could be a duplicate"
    );
    assert!(
        !live.explains("Discord"),
        "the official client's call is not in these streams and must keep recording"
    );
}
