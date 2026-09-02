//! The accuracy round's mechanisms, against the real decoder where one is
//! available.
//!
//! Two of these need a model and are gated on `NXR_MODELS=<dir>` like the rest
//! of the acceptance suite: without it they report themselves skipped and pass.
//! They are here because the whole context re-decode rests on one claim about a
//! third-party binding — that sherpa-onnx hands Rust the token timestamps it
//! hands Python — and a claim like that belongs in a test, not in a comment.

use std::path::PathBuf;

use recalld::asr::{TimedAsr, Word, words_from_tokens, words_in_span};
use recalld::config::{Config, SAMPLE_RATE};
use recalld::ingest::read_wav;
use recalld::models::ModelSet;
use recalld::quality::{Redecode, Window, judge_redecode};
use recalld::store::Store;

fn models_dir() -> Option<PathBuf> {
    let raw = std::env::var("NXR_MODELS").ok()?;
    if raw.trim().is_empty() {
        return None;
    }
    Some(PathBuf::from(raw))
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(name)
}

fn loaded_asr() -> Option<TimedAsr> {
    let dir = models_dir()?;
    let mut cfg = Config::default();
    cfg.models.dir = Some(dir);
    let mut models = ModelSet::resolve(&cfg.models)?;
    models.select_asr();
    if !models.complete() {
        eprintln!("skipping: NXR_MODELS is set but incomplete");
        return None;
    }
    Some(TimedAsr::load(&models).expect("the timed recognizer loads"))
}

/// The claim the feature is built on: the transducer, through the C API this
/// daemon binds directly, reports **where each word starts**.
///
/// `sherpa_rs::transducer` frees that information before the caller sees it,
/// which is why `TimedAsr` exists at all. If a sherpa upgrade ever stops
/// filling `timestamps` for the NeMo export, the context re-decode silently
/// becomes "keep nothing" — so it is asserted rather than assumed.
#[test]
fn the_decoder_reports_where_each_word_starts() {
    let Some(mut asr) = loaded_asr() else {
        eprintln!("skipping the timestamp check: set NXR_MODELS=<dir> to run it");
        return;
    };
    let samples = read_wav(&fixture("clean_single_0.wav")).expect("fixture");
    let (text, words) = asr.transcribe_timed(&samples);
    assert!(!text.is_empty(), "the fixture must transcribe to something");
    assert!(
        words.len() >= 5,
        "expected several timed words, got {words:?}"
    );
    assert!(
        words.windows(2).all(|w| w[0].start_s <= w[1].start_s),
        "word times must not go backwards: {words:?}"
    );
    let duration_s = samples.len() as f32 / SAMPLE_RATE as f32;
    assert!(
        words.last().unwrap().start_s < duration_s,
        "a word cannot start after the audio ends"
    );
    // The words are the transcript, in order — the span selection joins them
    // back together and must not produce a different sentence.
    let joined = words_in_span(&words, 0.0, duration_s + 1.0);
    assert_eq!(
        recalld::asr::normalise_words(&joined),
        recalld::asr::normalise_words(&text),
        "keeping every word must reproduce the transcript"
    );
}

/// The other half of the same claim: restricting the decode to a span really
/// does cut the transcript down, and the cut lands where the timestamps say.
#[test]
fn a_span_keeps_only_the_words_that_start_inside_it() {
    let Some(mut asr) = loaded_asr() else {
        eprintln!("skipping the span check: set NXR_MODELS=<dir> to run it");
        return;
    };
    let samples = read_wav(&fixture("clean_single_0.wav")).expect("fixture");
    let (_, words) = asr.transcribe_timed(&samples);
    let cut = words[words.len() / 2].start_s;
    let head = words_in_span(&words, 0.0, cut);
    let tail = words_in_span(&words, cut, f32::MAX);
    assert!(!head.is_empty() && !tail.is_empty());
    assert_eq!(
        head.split_whitespace().count() + tail.split_whitespace().count(),
        words.len(),
        "every word belongs to exactly one side of the cut"
    );
}

/// Pieces become words at the start-of-word marker, and the word's time is the
/// time of the piece that opened it. No model needed: this is the grouping
/// rule, and getting it wrong turns "Kübra" into "K", "üb" and "ra" — three
/// words with three different times, two of which fall outside any span.
#[test]
fn bpe_pieces_are_grouped_into_words_at_the_word_marker() {
    let tokens: Vec<String> = [" Al", "les", " hat", " ein", " En", "de"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let stamps = [0.0f32, 0.24, 0.40, 0.64, 0.80, 0.96];
    let words = words_from_tokens(&tokens, &stamps);
    assert_eq!(
        words,
        vec![
            Word {
                text: "Alles".into(),
                start_s: 0.0
            },
            Word {
                text: "hat".into(),
                start_s: 0.40
            },
            Word {
                text: "ein".into(),
                start_s: 0.64
            },
            Word {
                text: "Ende".into(),
                start_s: 0.80
            },
        ]
    );
    // U+2581 is the same marker in a different export's rendering.
    let other: Vec<String> = ["▁the", "▁pug"].iter().map(|s| s.to_string()).collect();
    assert_eq!(
        words_from_tokens(&other, &[1.0, 1.5])
            .iter()
            .map(|w| w.text.as_str())
            .collect::<Vec<_>>(),
        vec!["the", "pug"]
    );
}

/// A turn a person has corrected by hand is never re-decoded and never
/// cross-checked. This is the guard that matters most in the whole round: the
/// worker rewrites transcripts, and the one transcript it must not touch is the
/// one somebody typed.
#[test]
fn a_hand_corrected_turn_is_left_alone_by_both_passes() {
    let store = Store::open_in_memory().expect("store");
    let source = store.upsert_source("VRChat.exe", "VRChat", 0).expect("src");
    let session = store.begin_session(source, 0).expect("session");
    let short = |start: i64, end: i64, path: &str| {
        let id = store
            .insert_segment(session, start, end, path, 0)
            .expect("segment");
        store
            .set_segment_analysis(
                id,
                &recalld::store::SegmentAnalysis {
                    text: Some("das war gut".into()),
                    lang: None,
                    lang_via: None,
                    asr_model_id: Some("m".into()),
                    overlap_frac: None,
                },
            )
            .expect("analysis");
        id
    };
    let plain = short(0, 1_500_000_000, "a.wav");
    let corrected = short(2_000_000_000, 3_500_000_000, "b.wav");

    store
        .log_operation(
            "segments.correct",
            &serde_json::json!([corrected]).to_string(),
            &serde_json::json!({"segment_id": corrected, "text": "das war Kübra"}).to_string(),
            1,
        )
        .expect("op");

    let redecode = store
        .segments_for_context_redecode(2.5, 10)
        .expect("backlog");
    assert_eq!(
        redecode.iter().map(|c| c.id).collect::<Vec<_>>(),
        vec![plain],
        "a corrected turn must not be in the re-decode queue"
    );
    let confidence = store.segments_for_confidence(10).expect("backlog");
    assert_eq!(
        confidence.iter().map(|c| c.id).collect::<Vec<_>>(),
        vec![plain],
        "a corrected turn needs no second opinion"
    );

    // …and once the worker has been through it, it is not offered again.
    store
        .set_segment_text_from_context(plain, "das war sehr gut", "m", 5)
        .expect("write");
    assert!(
        store
            .segments_for_context_redecode(2.5, 10)
            .expect("backlog")
            .is_empty(),
        "a re-decoded turn is re-decoded once"
    );
    let row = store.segment_row(plain).expect("row").expect("live");
    assert_eq!(row.text.as_deref(), Some("das war sehr gut"));
    assert_eq!(row.text_via.as_deref(), Some("context"));
}

/// Long turns are not in the queue at all: they already carry their own
/// context, and the measured gain is a short-turn effect.
#[test]
fn only_short_turns_are_queued_for_a_context_re_decode() {
    let store = Store::open_in_memory().expect("store");
    let source = store.upsert_source("VRChat.exe", "VRChat", 0).expect("src");
    let session = store.begin_session(source, 0).expect("session");
    for (start, end) in [(0i64, 1_500_000_000i64), (2_000_000_000, 8_000_000_000)] {
        let id = store
            .insert_segment(session, start, end, "x.wav", 0)
            .expect("segment");
        store
            .set_segment_analysis(
                id,
                &recalld::store::SegmentAnalysis {
                    text: Some("etwas gesagt".into()),
                    lang: None,
                    lang_via: None,
                    asr_model_id: Some("m".into()),
                    overlap_frac: None,
                },
            )
            .expect("analysis");
    }
    let queued = store
        .segments_for_context_redecode(2.5, 10)
        .expect("backlog");
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].t_end_ns - queued[0].t_start_ns, 1_500_000_000);
    // The live pass stamped its own provenance on the way in.
    let row = store.segment_row(queued[0].id).expect("row").expect("live");
    assert_eq!(row.text_via.as_deref(), Some("live"));
    assert_eq!(row.asr_confidence, None, "nothing has checked it yet");
}

/// The span is read against the window's clock, not the session's — the one
/// arithmetic mistake that would silently empty every re-decode.
#[test]
fn the_window_clock_is_what_the_span_is_measured_against() {
    let window = Window {
        samples: Vec::new(),
        start_ns: 1_700_000_000_000_000_000,
        clips: 3,
    };
    let words = [
        Word {
            text: "vorher".into(),
            start_s: 0.5,
        },
        Word {
            text: "hier".into(),
            start_s: 3.2,
        },
    ];
    let turn_start = window.start_ns + 3_000_000_000;
    let turn_end = turn_start + 1_500_000_000;
    assert_eq!(
        judge_redecode(&words, &window, turn_start, turn_end, "hir", "m"),
        Redecode::Replaced {
            text: "hier".into(),
            model_id: "m".into()
        }
    );
}

/// The cross-check decoder is bound through the C API too — `sherpa-rs` 0.6.8
/// has no Canary module at all — so "does the second decoder actually decode"
/// is a claim worth a test rather than a comment.
///
/// Gated twice: on `NXR_MODELS`, and on the cross-check being installed under
/// it (`models fetch --confidence`). Not installed is a normal state.
#[test]
fn the_cross_check_decoder_agrees_with_a_clean_transcript() {
    let Some(root) = models_dir() else {
        eprintln!("skipping the cross-check: set NXR_MODELS=<dir> to run it");
        return;
    };
    let model = recalld::models::ConfidenceModel::resolve_at(root, 2);
    if !model.present() {
        eprintln!("skipping the cross-check: it is not installed under NXR_MODELS");
        return;
    }
    let mut canary = recalld::canary::Canary::load(&model, "en").expect("canary loads");
    let samples = read_wav(&fixture("clean_single_1.wav")).expect("fixture");
    let text = canary.transcribe(&samples);
    assert!(!text.is_empty(), "the fixture must transcribe to something");
    // Clean read speech: the two decoders should be well above the shipped τ,
    // which is the whole basis of reading disagreement as doubt.
    let score = recalld::canary::agreement("THE THREE FRIENDS WERE ASTOUNDED", &text);
    assert!(
        score >= 0.5,
        "expected agreement on a clean fixture, got {score} for {text:?}"
    );
    assert_eq!(
        recalld::canary::Confidence::from_agreement(score, 0.5),
        recalld::canary::Confidence::Solid
    );
}

/// The whole re-decode, end to end, against the real decoder: three
/// consecutive clips of one session, the middle one re-read with its
/// neighbours, and the row rewritten with the words that fall inside it.
///
/// This is the test that would have caught every interesting way the feature
/// can fail quietly — a window built on the wrong clock, a span that keeps
/// nothing, a row marked done without being improved — because it asserts on
/// what the database says afterwards rather than on the mechanism.
#[test]
fn a_short_turn_is_re_decoded_with_its_neighbours_end_to_end() {
    let Some(mut asr) = loaded_asr() else {
        eprintln!("skipping the end-to-end re-decode: set NXR_MODELS=<dir> to run it");
        return;
    };
    let dir = std::env::temp_dir().join(format!("nx-recall-quality-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("data dir");

    let whole = read_wav(&fixture("clean_single_0.wav")).expect("fixture");
    let target = "BUT IN HIS HANDS SOLITUDE AND A VIOLIN WERE SURE TO MARRY IN MUSIC";
    let piece = whole.len() / 3;
    let store = Store::open(&dir).expect("store");
    let source = store.upsert_source("VRChat.exe", "VRChat", 0).expect("src");
    let session = store.begin_session(source, 0).expect("session");

    let mut ids = Vec::new();
    for i in 0..3 {
        let samples = &whole[i * piece..(i + 1) * piece];
        let rel = format!("segments/000001/seg-{i}.wav");
        recalld::pipeline::write_wav(&dir.join(&rel), samples).expect("wav");
        // The clips are contiguous on the session's clock, which is what makes
        // them each other's context.
        let start = (i as i64) * (piece as i64) * 1_000_000_000 / SAMPLE_RATE as i64;
        let end = start + (piece as i64) * 1_000_000_000 / SAMPLE_RATE as i64;
        let id = store
            .insert_segment(session, start, end, &rel, 0)
            .expect("segment");
        // What the live pass would have written: the slice, decoded alone —
        // NULL when the decoder made nothing of it, exactly as
        // `Analyzer::prepare` stores it. With parakeet v3 that is what happens
        // to the middle third of this fixture, which is the case the whole
        // feature exists for.
        let (alone, _) = asr.transcribe_timed(samples);
        let alone = (!recalld::asr::normalise_words(&alone).is_empty()).then_some(alone);
        store
            .set_segment_analysis(
                id,
                &recalld::store::SegmentAnalysis {
                    text: alone,
                    lang: None,
                    lang_via: None,
                    asr_model_id: Some(asr.model_id().to_string()),
                    overlap_frac: None,
                },
            )
            .expect("analysis");
        ids.push(id);
    }

    let control = recalld::control::Control::new(
        dir.clone(),
        None,
        &recalld::allowlist::Allowlist::from_rules([("VRChat.exe", true)]),
    );
    let bus = recalld::bus::Bus::new(64, 32);
    let (client, rx) = bus.attach(None);
    client.subscribe(&[recalld::bus::Topic::Segments]);
    let store = std::sync::Arc::new(std::sync::Mutex::new(store));
    let stats = recalld::quality::QualityStats::default();
    let stop = recalld::quality::QualityStop::default();
    // Every clip here is ~1.4 s, so all three are in the queue.
    let cfg = recalld::config::AsrConfig {
        context_redecode_below_s: 3.0,
        ..Default::default()
    };

    let worked = recalld::quality::redecode_batch(
        &store, &control, &bus, &mut asr, &cfg, &dir, &stats, &stop,
    )
    .expect("a pass");
    assert!(worked, "there was a backlog");

    let guard = store.lock().unwrap();
    let mut rebuilt = Vec::new();
    for id in &ids {
        let row = guard.segment_row(*id).expect("row").expect("live");
        assert!(
            row.text.as_deref().is_some_and(|t| !t.trim().is_empty()),
            "no turn may lose its words to a re-decode"
        );
        assert!(
            matches!(row.text_via.as_deref(), Some("context") | Some("live")),
            "every turn was considered: {:?}",
            row.text_via
        );
        rebuilt.extend(recalld::asr::normalise_words(row.text.as_deref().unwrap()));
    }
    // The three turns, read back in order, are the sentence the fixture holds.
    // That is the whole feature in one assertion: each clip alone decodes into
    // a fragment with no idea what came before it, and each clip re-read inside
    // its neighbours lands on its own words exactly.
    assert_eq!(
        rebuilt.join(" "),
        recalld::asr::normalise_words(target).join(" "),
        "the re-decoded turns must reassemble into the reference"
    );
    // The middle turn has a neighbour on both sides, so it is the one the
    // feature exists for, and it must have been re-read rather than skipped.
    let rewritten = guard
        .segments_for_context_redecode(3.0, 10)
        .expect("backlog");
    assert!(rewritten.is_empty(), "the whole batch was considered once");
    drop(guard);

    let announced: Vec<serde_json::Value> = std::iter::from_fn(|| rx.try_recv().ok())
        .map(|b| serde_json::from_slice(&b).unwrap())
        .filter(|e: &serde_json::Value| e["ev"] == "segment")
        .collect();
    assert_eq!(
        announced.len(),
        stats
            .redecoded_context
            .load(std::sync::atomic::Ordering::Relaxed) as usize,
        "every rewritten row is re-published, and only those"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A turn the live decoder made nothing of is the queue's most valuable
/// customer, and a turn the inference thread has not reached yet is not in the
/// queue at all. The difference between the two is `asr_model_id`, and getting
/// it wrong either skips the rescues or fights the pipeline for rows it has
/// not written yet.
#[test]
fn an_empty_transcript_is_queued_and_an_unanalysed_one_is_not() {
    let store = Store::open_in_memory().expect("store");
    let source = store.upsert_source("VRChat.exe", "VRChat", 0).expect("src");
    let session = store.begin_session(source, 0).expect("session");

    let waiting = store
        .insert_segment(session, 0, 1_000_000_000, "a.wav", 0)
        .expect("segment");
    let empty = store
        .insert_segment(session, 2_000_000_000, 3_000_000_000, "b.wav", 0)
        .expect("segment");
    // The live pass ran and produced no words: NULL text, but a model id.
    store
        .set_segment_analysis(
            empty,
            &recalld::store::SegmentAnalysis {
                text: None,
                lang: None,
                lang_via: None,
                asr_model_id: Some("parakeet".into()),
                overlap_frac: None,
            },
        )
        .expect("analysis");

    let queued: Vec<i64> = store
        .segments_for_context_redecode(2.5, 10)
        .expect("backlog")
        .iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(queued, vec![empty]);
    assert!(
        !queued.contains(&waiting),
        "the pipeline owns that row still"
    );
}

/// The confidence pass, end to end: a turn whose stored words are right comes
/// back `solid`, a turn whose stored words are nonsense comes back `shaky`, and
/// neither of them has its text touched.
///
/// The last clause is the contract ("flag only — the text is never replaced by
/// the cross-check") and it is the one a future refactor could quietly break,
/// because the cross-check is holding a better transcript at the time.
#[test]
fn the_cross_check_flags_without_rewriting() {
    let Some(root) = models_dir() else {
        eprintln!("skipping the confidence pass: set NXR_MODELS=<dir> to run it");
        return;
    };
    if !recalld::models::ConfidenceModel::resolve_at(root.clone(), 2).present() {
        eprintln!("skipping the confidence pass: the cross-check is not installed");
        return;
    }
    let dir = std::env::temp_dir().join(format!("nx-recall-confidence-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("data dir");

    let samples = read_wav(&fixture("clean_single_1.wav")).expect("fixture");
    let store = Store::open(&dir).expect("store");
    let source = store.upsert_source("VRChat.exe", "VRChat", 0).expect("src");
    let session = store.begin_session(source, 0).expect("session");
    let dur_ns = samples.len() as i64 * 1_000_000_000 / SAMPLE_RATE as i64;

    let mut ids = Vec::new();
    for (i, text) in [
        "The three friends were astounded.",
        "der Kühlschrank fährt nach Hause",
    ]
    .iter()
    .enumerate()
    {
        let rel = format!("segments/000001/conf-{i}.wav");
        recalld::pipeline::write_wav(&dir.join(&rel), &samples).expect("wav");
        let start = i as i64 * dur_ns;
        let id = store
            .insert_segment(session, start, start + dur_ns, &rel, 0)
            .expect("segment");
        store
            .set_segment_analysis(
                id,
                &recalld::store::SegmentAnalysis {
                    text: Some((*text).to_string()),
                    lang: Some("en".into()),
                    lang_via: Some("classified".into()),
                    asr_model_id: Some("parakeet".into()),
                    overlap_frac: None,
                },
            )
            .expect("analysis");
        ids.push(id);
    }

    let control = recalld::control::Control::new(
        dir.clone(),
        None,
        &recalld::allowlist::Allowlist::from_rules([("VRChat.exe", true)]),
    );
    let bus = recalld::bus::Bus::new(64, 32);
    let store = std::sync::Arc::new(std::sync::Mutex::new(store));
    let model = recalld::models::ConfidenceModel::resolve_at(root, 2);
    let mut canaries = vec![
        recalld::canary::Canary::load(&model, "en").expect("en"),
        recalld::canary::Canary::load(&model, "de").expect("de"),
    ];
    let stats = recalld::quality::QualityStats::default();
    let stop = recalld::quality::QualityStop::default();

    recalld::quality::confidence_batch(
        &store,
        &control,
        &bus,
        &mut canaries,
        &recalld::config::AsrConfig::default(),
        &dir,
        &stats,
        &stop,
    )
    .expect("a pass");

    let guard = store.lock().unwrap();
    let right = guard.segment_row(ids[0]).unwrap().unwrap();
    let wrong = guard.segment_row(ids[1]).unwrap().unwrap();
    assert_eq!(right.asr_confidence.as_deref(), Some("solid"));
    assert_eq!(wrong.asr_confidence.as_deref(), Some("shaky"));
    assert_eq!(
        right.text.as_deref(),
        Some("The three friends were astounded."),
        "a cross-check never edits a transcript"
    );
    assert_eq!(
        wrong.text.as_deref(),
        Some("der Kühlschrank fährt nach Hause"),
        "not even one it can plainly see is wrong"
    );
    drop(guard);
    let _ = std::fs::remove_dir_all(&dir);
}
