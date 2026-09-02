//! Ground truth from Discord (0.9.0), end to end.
//!
//! None of this needs a model: the whole subsystem is arithmetic over rows
//! plus one loopback socket, which is exactly why it was worth building — the
//! measurement it produces costs nothing to run and nothing to check.
//!
//! Everything here writes into a temp directory keyed on the pid and binds
//! port 0, so the suite never touches the live daemon's database, config or
//! port.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use recalld::allowlist::Allowlist;
use recalld::bus::Bus;
use recalld::config::{IdentityConfig, TruthConfig};
use recalld::control::Control;
use recalld::store::{SCHEMA_VERSION, SegmentAnalysis, Store, truth_verdict, truth_via};
use recalld::truth::{self, TruthStats, TruthStop};
use recalld::truthnet;

const MS: i64 = 1_000_000;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("nx-recall-truth-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("creating the throwaway data dir");
    dir
}

/// A store with one Discord session, ready for segments.
struct Rig {
    dir: PathBuf,
    store: Arc<Mutex<Store>>,
    control: Arc<Control>,
    bus: Arc<Bus>,
    session: i64,
    cfg: TruthConfig,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn rig(name: &str) -> Rig {
    let dir = temp_dir(name);
    let store = Store::open(&dir).expect("opening the throwaway database");
    let source = store
        .upsert_source("Discord", "Discord", 0)
        .expect("the Discord source");
    let session = store.begin_session(source, 0).expect("a session");
    let store = Arc::new(Mutex::new(store));
    let control = Control::new(
        dir.clone(),
        None,
        &Allowlist::from_rules([("Discord", true)]),
    )
    .with_identity(IdentityConfig::default());
    let bus = Bus::new(64, 32);
    let cfg = TruthConfig {
        batch_segments: 500,
        ..Default::default()
    };
    Rig {
        dir,
        store,
        control,
        bus,
        session,
        cfg,
    }
}

impl Rig {
    fn store(&self) -> std::sync::MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn segment(&self, from_ms: i64, to_ms: i64) -> i64 {
        self.store()
            .insert_segment(
                self.session,
                from_ms * MS,
                to_ms * MS,
                &format!("segments/{from_ms}.wav"),
                0,
            )
            .expect("inserting a segment")
    }

    fn label(&self) {
        truth::label_batch(
            &self.store,
            &self.control,
            &self.cfg,
            &TruthStats::default(),
            &TruthStop::default(),
        )
        .expect("a labelling pass");
    }

    fn verdict_of(&self, id: i64) -> (Option<String>, Option<String>) {
        let t = self
            .store()
            .segment_truth(id)
            .expect("reading a verdict back")
            .expect("the segment exists");
        (t.verdict, t.user_id)
    }
}

// ---------------------------------------------------------------------------
// schema
// ---------------------------------------------------------------------------

#[test]
fn the_v11_migration_is_idempotent_and_keeps_what_it_wrote() {
    // 0.9.0 wrote v11; 0.10.0's worlds took it to v12. The number moves, and
    // what this test is really about does not: re-opening must be a no-op.
    assert_eq!(SCHEMA_VERSION, 12, "0.10.0 is schema v12");
    let dir = temp_dir("schema");
    let mut seg = 0i64;
    // Three opens: the first migrates, the second and third must be no-ops
    // that neither fail nor lose a row. `add_column_if_missing` and every
    // CREATE being `IF NOT EXISTS` is what makes that true, and this is what
    // would catch it stopping being true.
    for run in 0..3 {
        let store = Store::open(&dir).expect("opening (and re-migrating) the database");
        if run == 0 {
            let src = store.upsert_source("Discord", "Discord", 0).unwrap();
            let sess = store.begin_session(src, 0).unwrap();
            seg = store
                .insert_segment(sess, 0, 1_000 * MS, "segments/a.wav", 0)
                .unwrap();
            store
                .set_segment_truth(seg, Some("u1"), truth_verdict::SINGLE, Some(0.97))
                .unwrap();
            store.upsert_discord_user("u1", "Aspen", 0).unwrap();
            store.truth_speaking_start("u1", "Aspen", None, 0).unwrap();
            store.truth_speaking_stop("u1", 900 * MS).unwrap();
        }
        // Every v11 surface still answers, and still holds what it was given.
        let t = store.segment_truth(seg).unwrap().unwrap();
        assert_eq!(t.verdict.as_deref(), Some(truth_verdict::SINGLE));
        assert_eq!(t.user_id.as_deref(), Some("u1"));
        assert_eq!(t.coverage, Some(0.97));
        assert_eq!(store.truth_span_counts().unwrap(), (1, 0));
        assert_eq!(store.discord_users().unwrap().len(), 1);
        assert!(
            store
                .segments_for_truth_enrol(0.1, 0.5, 10)
                .unwrap()
                .is_empty(),
            "unlinked, so nothing to enrol"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// spans
// ---------------------------------------------------------------------------

#[test]
fn a_start_with_no_stop_is_closed_by_the_next_start() {
    let r = rig("open-start");
    {
        let s = r.store();
        s.truth_speaking_start("u1", "Aspen", Some("c"), 0).unwrap();
        // No stop. The next start is a dropped batch, not two mouths.
        s.truth_speaking_start("u1", "Aspen", Some("c"), 500 * MS)
            .unwrap();
        let spans = s.truth_spans_between(-1, 10_000 * MS).unwrap();
        assert_eq!(spans.len(), 2);
        assert_eq!(
            spans[0].t_end_ns,
            500 * MS,
            "the first was closed at the second"
        );
    }
}

#[test]
fn a_start_with_no_stop_is_closed_by_the_timeout_and_not_by_now() {
    let r = rig("timeout");
    let s = r.store();
    s.truth_speaking_start("u1", "Aspen", None, 1_000 * MS)
        .unwrap();
    let timeout = 30_000 * MS;

    // Not yet: 29 s later the ring may genuinely still be lit.
    assert_eq!(s.truth_close_open(30_000 * MS, timeout).unwrap(), 0);
    // An hour later it certainly is not, and the span ends at the timeout —
    // the last thing anybody actually knows — not at now.
    assert_eq!(s.truth_close_open(3_600_000 * MS, timeout).unwrap(), 1);
    let spans = s.truth_spans_between(0, 3_600_000 * MS).unwrap();
    assert_eq!(spans[0].t_end_ns, 31_000 * MS);
    // Idempotent: a closed row is not closed again.
    assert_eq!(s.truth_close_open(3_600_000 * MS, timeout).unwrap(), 0);
}

#[test]
fn a_stop_with_no_start_invents_nothing() {
    let r = rig("orphan-stop");
    let s = r.store();
    assert!(!s.truth_speaking_stop("u1", 500 * MS).unwrap());
    assert!(s.truth_spans_between(0, 10_000 * MS).unwrap().is_empty());
}

#[test]
fn an_open_span_is_reported_clipped_to_the_window_it_was_asked_for() {
    let r = rig("open-clip");
    let s = r.store();
    s.truth_speaking_start("u1", "Aspen", None, 0).unwrap();
    let spans = s.truth_spans_between(0, 1_000 * MS).unwrap();
    assert_eq!(spans[0].t_end_ns, 1_000 * MS, "clipped, never unbounded");
}

// ---------------------------------------------------------------------------
// labelling
// ---------------------------------------------------------------------------

#[test]
fn the_labelling_pass_writes_the_verdict_table_onto_real_segments() {
    let r = rig("label");
    // Four segments, one second each, back to back from t = 10 s.
    let single = r.segment(10_000, 11_000);
    let overlap = r.segment(11_000, 12_000);
    let partial = r.segment(12_000, 13_000);
    let nobody = r.segment(13_000, 14_000);
    {
        let s = r.store();
        // single: u1 across almost all of it, u2 barely
        s.truth_speaking_start("u1", "Aspen", None, 10_000 * MS)
            .unwrap();
        s.truth_speaking_stop("u1", 10_950 * MS).unwrap();
        s.truth_speaking_start("u2", "Ash", None, 10_900 * MS)
            .unwrap();
        s.truth_speaking_stop("u2", 11_000 * MS).unwrap();
        // overlap: both well over the presence bar
        s.truth_speaking_start("u1", "Aspen", None, 11_000 * MS)
            .unwrap();
        s.truth_speaking_stop("u1", 11_700 * MS).unwrap();
        s.truth_speaking_start("u2", "Ash", None, 11_600 * MS)
            .unwrap();
        s.truth_speaking_stop("u2", 12_000 * MS).unwrap();
        // partial: one voice, half the span
        s.truth_speaking_start("u1", "Aspen", None, 12_000 * MS)
            .unwrap();
        s.truth_speaking_stop("u1", 12_500 * MS).unwrap();
        // nobody: a flicker under the presence bar, with truth all around
        s.truth_speaking_start("u1", "Aspen", None, 13_000 * MS)
            .unwrap();
        s.truth_speaking_stop("u1", 13_100 * MS).unwrap();
    }
    r.label();

    assert_eq!(
        r.verdict_of(single),
        (Some(truth_verdict::SINGLE.into()), Some("u1".into()))
    );
    assert_eq!(r.verdict_of(overlap).0, Some(truth_verdict::OVERLAP.into()));
    assert_eq!(r.verdict_of(overlap).1, None, "overlap belongs to nobody");
    assert_eq!(
        r.verdict_of(partial),
        (Some(truth_verdict::PARTIAL.into()), Some("u1".into()))
    );
    assert_eq!(r.verdict_of(nobody).0, Some(truth_verdict::NOBODY.into()));
}

#[test]
fn a_segment_with_no_truth_anywhere_near_is_unknown_and_stays_re_examinable() {
    let r = rig("unknown");
    let seg = r.segment(10_000, 11_000);
    r.label();
    assert_eq!(r.verdict_of(seg).0, Some(truth_verdict::UNKNOWN.into()));

    // Truth arrives late — the plugin was started after the call began, or a
    // batch finally flushed. The pass must reconsider.
    {
        let s = r.store();
        s.truth_speaking_start("u1", "Aspen", None, 10_000 * MS)
            .unwrap();
        s.truth_speaking_stop("u1", 10_950 * MS).unwrap();
    }
    r.label();
    assert_eq!(
        r.verdict_of(seg),
        (Some(truth_verdict::SINGLE.into()), Some("u1".into())),
        "an unknown that truth later covers is re-read"
    );

    // …and a settled verdict is never re-read, however much truth arrives.
    {
        let s = r.store();
        s.truth_speaking_start("u2", "Ash", None, 10_000 * MS)
            .unwrap();
        s.truth_speaking_stop("u2", 11_000 * MS).unwrap();
    }
    r.label();
    assert_eq!(
        r.verdict_of(seg).0,
        Some(truth_verdict::SINGLE.into()),
        "the queue is not a treadmill"
    );
}

#[test]
fn only_discord_sessions_are_labelled() {
    let r = rig("sources");
    let vr = {
        let s = r.store();
        let src = s.upsert_source("VRChat.exe", "VRChat", 0).unwrap();
        let sess = s.begin_session(src, 0).unwrap();
        s.truth_speaking_start("u1", "Aspen", None, 10_000 * MS)
            .unwrap();
        s.truth_speaking_stop("u1", 10_950 * MS).unwrap();
        s.insert_segment(sess, 10_000 * MS, 11_000 * MS, "segments/vr.wav", 0)
            .unwrap()
    };
    let discord = r.segment(10_000, 11_000);
    r.label();
    assert_eq!(r.verdict_of(vr).0, None, "VRChat is not Discord");
    assert_eq!(r.verdict_of(discord).0, Some(truth_verdict::SINGLE.into()));
}

// ---------------------------------------------------------------------------
// linking and the report
// ---------------------------------------------------------------------------

/// Seed `n` one-second `single` segments for `user`, of which `dominant` were
/// labelled `speaker_a` and the rest `speaker_b`.
fn seed_scored(r: &Rig, user: &str, speaker_a: i64, speaker_b: i64, n: i64, dominant: i64) {
    let s = r.store();
    for i in 0..n {
        let from = (100_000 + i * 2_000) * MS;
        let to = from + 1_000 * MS;
        let seg = s
            .insert_segment(r.session, from, to, &format!("segments/s{i}.wav"), 0)
            .unwrap();
        s.set_segment_truth(seg, Some(user), truth_verdict::SINGLE, Some(1.0))
            .unwrap();
        let heard = if i < dominant { speaker_a } else { speaker_b };
        s.set_segment_speaker(seg, Some(heard), Some(0.9)).unwrap();
    }
    s.upsert_discord_user(user, "Aspen", 0).unwrap();
}

#[test]
fn the_auto_linker_links_at_ninety_percent_and_refuses_at_eighty_nine() {
    // 89 of 100 — the case that must NOT link. At that rate the minority
    // voice is a merge somebody has to look at, not noise.
    let shy = rig("link-89");
    let a = shy.store().mint_speaker(0).unwrap();
    let b = shy.store().mint_speaker(0).unwrap();
    seed_scored(&shy, "u1", a, b, 100, 89);
    truth::link_batch(&shy.store, &shy.bus, &TruthStats::default()).unwrap();
    assert!(
        shy.store()
            .discord_user("u1")
            .unwrap()
            .unwrap()
            .speaker_id
            .is_none(),
        "89% must not link"
    );

    // 90 of 100 — links, and says the link came from truth.
    let sure = rig("link-90");
    let a = sure.store().mint_speaker(0).unwrap();
    let b = sure.store().mint_speaker(0).unwrap();
    seed_scored(&sure, "u1", a, b, 100, 90);
    truth::link_batch(&sure.store, &sure.bus, &TruthStats::default()).unwrap();
    let row = sure.store().discord_user("u1").unwrap().unwrap();
    assert_eq!(row.speaker_id, Some(a));
    assert_eq!(row.via.as_deref(), Some(truth_via::TRUTH));

    // Unanimous but only nineteen turns — not yet.
    let few = rig("link-19");
    let a = few.store().mint_speaker(0).unwrap();
    let b = few.store().mint_speaker(0).unwrap();
    seed_scored(&few, "u1", a, b, 19, 19);
    truth::link_batch(&few.store, &few.bus, &TruthStats::default()).unwrap();
    assert!(
        few.store()
            .discord_user("u1")
            .unwrap()
            .unwrap()
            .speaker_id
            .is_none()
    );
}

#[test]
fn a_hand_link_is_never_overwritten_by_the_auto_linker() {
    let r = rig("link-manual");
    let a = r.store().mint_speaker(0).unwrap();
    let b = r.store().mint_speaker(0).unwrap();
    seed_scored(&r, "u1", a, b, 100, 100);
    r.store()
        .set_discord_link("u1", Some(b), Some(truth_via::MANUAL), 0)
        .unwrap();
    truth::link_batch(&r.store, &r.bus, &TruthStats::default()).unwrap();
    let row = r.store().discord_user("u1").unwrap().unwrap();
    assert_eq!(row.speaker_id, Some(b), "a person's decision stands");
    assert_eq!(row.via.as_deref(), Some(truth_via::MANUAL));
}

#[test]
fn the_summary_scores_the_ladder_against_discord() {
    let r = rig("summary");
    let a = r.store().mint_speaker(0).unwrap();
    let b = r.store().mint_speaker(0).unwrap();
    // 100 clean turns: 90 right, 5 wrong, 5 the ladder declined.
    {
        let s = r.store();
        for i in 0..100 {
            let from = (100_000 + i * 2_000) * MS;
            let seg = s
                .insert_segment(r.session, from, from + 1_000 * MS, "segments/x.wav", 0)
                .unwrap();
            s.set_segment_truth(seg, Some("u1"), truth_verdict::SINGLE, Some(1.0))
                .unwrap();
            match i {
                0..=89 => s.set_segment_speaker(seg, Some(a), Some(0.9)).unwrap(),
                90..=94 => s.set_segment_speaker(seg, Some(b), Some(0.9)).unwrap(),
                _ => {}
            }
        }
        // A half-second turn labelled wrong: excluded, because the floor is
        // not what this is measuring.
        let short = s
            .insert_segment(r.session, 900_000 * MS, 900_500 * MS, "segments/s.wav", 0)
            .unwrap();
        s.set_segment_truth(short, Some("u1"), truth_verdict::SINGLE, Some(1.0))
            .unwrap();
        s.set_segment_speaker(short, Some(b), Some(0.9)).unwrap();

        s.upsert_discord_user("u1", "Aspen", 0).unwrap();
        s.set_discord_link("u1", Some(a), Some(truth_via::MANUAL), 0)
            .unwrap();
    }

    let cfg = TruthConfig::default();
    let identity = IdentityConfig::default();
    let out = truth::summary(&r.store(), &identity, &cfg).unwrap();
    let id = &out["identity"];
    assert_eq!(id["n"], 100, "the sub-second turn is not scored");
    assert_eq!(id["correct"], 90);
    assert_eq!(id["wrong"], 5);
    assert_eq!(id["unlabelled"], 5);
    // precision is over the turns it answered on, recall over every turn.
    assert!((id["precision"].as_f64().unwrap() - 90.0 / 95.0).abs() < 1e-9);
    assert!((id["recall"].as_f64().unwrap() - 0.90).abs() < 1e-9);
    assert_eq!(out["single"], 101);
    assert_eq!(
        id["by_speaker"][0]["speaker_id"], a,
        "broken down by the voice, not by the account alone"
    );
}

#[test]
fn a_user_with_no_link_is_counted_but_not_scored() {
    let r = rig("unlinked");
    let a = r.store().mint_speaker(0).unwrap();
    {
        let s = r.store();
        let seg = s
            .insert_segment(r.session, 100_000 * MS, 101_000 * MS, "segments/x.wav", 0)
            .unwrap();
        s.set_segment_truth(seg, Some("u1"), truth_verdict::SINGLE, Some(1.0))
            .unwrap();
        s.set_segment_speaker(seg, Some(a), Some(0.9)).unwrap();
        s.upsert_discord_user("u1", "Aspen", 0).unwrap();
    }
    let out = truth::summary(
        &r.store(),
        &IdentityConfig::default(),
        &TruthConfig::default(),
    )
    .unwrap();
    assert_eq!(out["single"], 1, "the verdict is still counted");
    assert_eq!(
        out["identity"]["n"], 0,
        "…but there is nothing to score it against"
    );
    assert!(
        out["identity"]["precision"].is_null(),
        "no answer, not a zero"
    );
}

#[test]
fn the_overlap_gate_is_scored_against_overlap_verdicts() {
    let r = rig("gate");
    let identity = IdentityConfig::default();
    let over = identity.max_overlap as f64;
    {
        let s = r.store();
        let mut at = 100_000i64;
        let mut add = |verdict: &str, frac: f32| {
            let seg = s
                .insert_segment(r.session, at * MS, (at + 1_000) * MS, "segments/x.wav", 0)
                .unwrap();
            s.set_segment_truth(seg, None, verdict, None).unwrap();
            s.set_segment_analysis(
                seg,
                &SegmentAnalysis {
                    text: Some("hallo".into()),
                    lang: None,
                    lang_via: None,
                    asr_model_id: None,
                    overlap_frac: Some(frac),
                },
            )
            .unwrap();
            at += 2_000;
        };
        // three real overlaps, two of which the gate caught
        add(truth_verdict::OVERLAP, (over + 0.1) as f32);
        add(truth_verdict::OVERLAP, (over + 0.1) as f32);
        add(truth_verdict::OVERLAP, 0.0);
        // one clean turn the gate flagged anyway
        add(truth_verdict::SINGLE, (over + 0.1) as f32);
        // three clean turns it left alone
        for _ in 0..3 {
            add(truth_verdict::SINGLE, 0.0);
        }
    }
    let out = truth::summary(&r.store(), &identity, &TruthConfig::default()).unwrap();
    let g = &out["overlap_gate"];
    assert_eq!(g["flagged_when_overlap"], 2);
    assert_eq!(g["flagged_when_single"], 1);
    assert!((g["precision"].as_f64().unwrap() - 2.0 / 3.0).abs() < 1e-9);
    assert!((g["recall"].as_f64().unwrap() - 2.0 / 3.0).abs() < 1e-9);
}

// ---------------------------------------------------------------------------
// the loopback ingest
// ---------------------------------------------------------------------------

struct Http {
    status: u16,
    body: String,
}

fn request(port: u16, head: &str, body: &str) -> Http {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connecting to the ingest");
    let req = format!(
        "{head}\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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
    // Drain the headers, then whatever body there is.
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).unwrap() == 0 || h.trim().is_empty() {
            break;
        }
    }
    let mut out = String::new();
    let _ = reader.read_to_string(&mut out);
    Http { status, body: out }
}

/// The ingest, on an ephemeral port, over a throwaway store.
fn ingest(name: &str) -> (Rig, truthnet::Ingest, u16, String) {
    let r = rig(name);
    let token = "0123456789abcdef0123456789abcdef".to_string();
    let served = truthnet::serve(
        Arc::clone(&r.store),
        Arc::new(TruthStats::default()),
        token.clone(),
        0,
    )
    .expect("binding the ingest on an ephemeral port");
    let port = served.addr().port();
    (r, served, port, token)
}

#[test]
fn the_ingest_takes_ndjson_speaking_edges_and_turns_them_into_spans() {
    let (r, served, port, token) = ingest("http-speaking");
    let body = "{\"t_ms\":1000,\"user_id\":\"u1\",\"speaking\":true,\"name\":\"Aspen\",\"channel_id\":\"c\"}\n\
                {\"t_ms\":1800,\"user_id\":\"u1\",\"speaking\":false,\"name\":\"Aspen\",\"channel_id\":\"c\"}\n";
    let res = request(
        port,
        &format!("POST /v1/discord/speaking HTTP/1.1\r\nAuthorization: Bearer {token}"),
        body,
    );
    assert_eq!(res.status, 204, "an accepted batch says nothing back");

    let spans = r.store().truth_spans_between(0, 10_000 * MS).unwrap();
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].user_id, "u1");
    assert_eq!(spans[0].t_start_ns, 1_000 * MS);
    assert_eq!(spans[0].t_end_ns, 1_800 * MS);
    // The account was learned along the way, nickname and all.
    let u = r.store().discord_user("u1").unwrap().unwrap();
    assert_eq!(u.name, "Aspen");
    assert!(u.speaker_id.is_none(), "a sighting is not a link");
    served.shutdown();
}

#[test]
fn a_leave_closes_a_ring_nobody_sent_a_stop_for() {
    let (r, served, port, token) = ingest("http-leave");
    request(
        port,
        &format!("POST /v1/discord/speaking HTTP/1.1\r\nAuthorization: Bearer {token}"),
        "{\"t_ms\":1000,\"user_id\":\"u1\",\"speaking\":true,\"name\":\"Aspen\"}\n",
    );
    request(
        port,
        &format!("POST /v1/discord/voice HTTP/1.1\r\nAuthorization: Bearer {token}"),
        "{\"t_ms\":1500,\"ev\":\"leave\",\"user_id\":\"u1\",\"name\":\"Aspen\"}\n",
    );
    let spans = r.store().truth_spans_between(0, 10_000 * MS).unwrap();
    assert_eq!(spans[0].t_end_ns, 1_500 * MS);
    served.shutdown();
}

#[test]
fn the_ingest_refuses_a_missing_or_wrong_token_and_reads_no_body() {
    let (r, served, port, token) = ingest("http-401");
    let line = "{\"t_ms\":1000,\"user_id\":\"u1\",\"speaking\":true,\"name\":\"Aspen\"}\n";
    for head in [
        "POST /v1/discord/speaking HTTP/1.1".to_string(),
        "POST /v1/discord/speaking HTTP/1.1\r\nAuthorization: Bearer wrong".to_string(),
        format!("POST /v1/discord/speaking HTTP/1.1\r\nAuthorization: {token}"),
    ] {
        assert_eq!(request(port, &head, line).status, 401, "{head}");
    }
    assert!(
        r.store()
            .truth_spans_between(0, 10_000 * MS)
            .unwrap()
            .is_empty(),
        "a refused request must not have been stored"
    );
    served.shutdown();
}

#[test]
fn a_body_over_a_megabyte_is_refused_whole() {
    let (r, served, port, token) = ingest("http-413");
    let mut body = String::with_capacity(truthnet::MAX_BODY + 4096);
    while body.len() <= truthnet::MAX_BODY {
        body.push_str("{\"t_ms\":1000,\"user_id\":\"u1\",\"speaking\":true,\"name\":\"Aspen\"}\n");
    }
    let res = request(
        port,
        &format!("POST /v1/discord/speaking HTTP/1.1\r\nAuthorization: Bearer {token}"),
        &body,
    );
    assert_eq!(res.status, 413);
    assert!(
        r.store()
            .truth_spans_between(0, 10_000 * MS)
            .unwrap()
            .is_empty(),
        "half a batch of NDJSON is a corrupted batch, not a smaller one"
    );
    served.shutdown();
}

#[test]
fn health_answers_with_the_token_and_refuses_without_it() {
    let (_r, served, port, token) = ingest("http-health");
    assert_eq!(
        request(port, "GET /v1/health HTTP/1.1", "").status,
        401,
        "health is behind the token too"
    );
    let res = request(
        port,
        &format!("GET /v1/health HTTP/1.1\r\nAuthorization: Bearer {token}"),
        "",
    );
    assert_eq!(res.status, 200);
    assert!(res.body.contains("\"ok\":true"), "{}", res.body);
    served.shutdown();
}

#[test]
fn an_unknown_route_and_a_preflight_are_both_answered_quietly() {
    let (_r, served, port, token) = ingest("http-routes");
    assert_eq!(
        request(port, "OPTIONS /v1/discord/speaking HTTP/1.1", "").status,
        204,
        "a preflight carries no Authorization header and must still pass"
    );
    assert_eq!(
        request(
            port,
            &format!("POST /v1/discord/future HTTP/1.1\r\nAuthorization: Bearer {token}"),
            "{}\n"
        )
        .status,
        204,
        "a newer plugin's route is not an error a fire-and-forget client can act on"
    );
    served.shutdown();
}

#[test]
fn one_malformed_line_does_not_poison_the_batch() {
    let (r, served, port, token) = ingest("http-malformed");
    let body = "not json at all\n\
                {\"t_ms\":1000,\"user_id\":\"u1\",\"speaking\":true,\"name\":\"Aspen\"}\n\
                {\"user_id\":\"u1\",\"speaking\":false}\n\
                {\"t_ms\":1800,\"user_id\":\"u1\",\"speaking\":false,\"name\":\"Aspen\"}\n";
    assert_eq!(
        request(
            port,
            &format!("POST /v1/discord/speaking HTTP/1.1\r\nAuthorization: Bearer {token}"),
            body
        )
        .status,
        204
    );
    let spans = r.store().truth_spans_between(0, 10_000 * MS).unwrap();
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].t_end_ns, 1_800 * MS);
    served.shutdown();
}
