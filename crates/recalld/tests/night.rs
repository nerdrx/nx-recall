//! The night shift's mechanisms (0.9.0).
//!
//! Everything here runs without a GPU, a model or a network: the batching
//! arithmetic, the queue's guards, the gates and the vote are the parts that
//! can be wrong in a way nobody notices for a week, and all four are pure
//! enough to test on a laptop. The decoder itself is exercised by
//! `spike/night_vote_bench.py`, which is where a measurement belongs.

use std::path::PathBuf;

use recalld::config::{NightConfig, SAMPLE_RATE};
use recalld::models::FALLBACK_ASR;
use recalld::night::{
    Hours, Readings, Slot, Utterance, Vote, gpu_busy_pct_in, judge_vote, pack, parse_whisper_json,
    split_by_offsets,
};
use recalld::store::{SegmentAnalysis, Store, text_via};

// ---------------------------------------------------------------------------
// the queue
// ---------------------------------------------------------------------------

/// The night shift reads shaky rows and nothing else — not solid ones, not
/// unchecked ones, and never a row a person has corrected by hand.
#[test]
fn the_queue_is_shaky_rows_that_nobody_has_corrected() {
    let store = Store::open_in_memory().expect("store");
    let source = store.upsert_source("VRChat.exe", "VRChat", 0).expect("src");
    let session = store.begin_session(source, 0).expect("session");
    let row = |start: i64, path: &str, verdict: Option<&str>| {
        let id = store
            .insert_segment(session, start, start + 2_000_000_000, path, 0)
            .expect("segment");
        store
            .set_segment_analysis(
                id,
                &SegmentAnalysis {
                    text: Some("das war gut".into()),
                    lang: Some("de".into()),
                    lang_via: None,
                    asr_model_id: Some("m".into()),
                    overlap_frac: None,
                },
            )
            .expect("analysis");
        if let Some(v) = verdict {
            store.set_segment_confidence(id, Some(v), 1).expect("flag");
        }
        id
    };
    let shaky = row(0, "a.wav", Some("shaky"));
    let solid = row(3_000_000_000, "b.wav", Some("solid"));
    let unchecked = row(6_000_000_000, "c.wav", None);
    let corrected = row(9_000_000_000, "d.wav", Some("shaky"));
    store
        .log_operation(
            "segments.correct",
            &serde_json::json!([corrected]).to_string(),
            &serde_json::json!({"segment_id": corrected, "text": "was anderes"}).to_string(),
            2,
        )
        .expect("op");

    let queued: Vec<i64> = store
        .segments_for_night(10, FALLBACK_ASR.dir)
        .expect("queue")
        .iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(queued, vec![shaky], "only the uncorrected shaky row");
    assert!(!queued.contains(&solid));
    assert!(!queued.contains(&unchecked));
    assert!(!queued.contains(&corrected));

    // Stamped once, offered never again — including when the night read
    // nothing at all, which is the case that would otherwise loop for ever.
    store.set_segment_night(shaky, None, 7).expect("stamp");
    assert!(
        store
            .segments_for_night(10, FALLBACK_ASR.dir)
            .expect("queue")
            .is_empty(),
        "a row the night shift has considered leaves the queue"
    );
}

/// Light mode's guarantee (0.13.x): a row the 110m export decoded is queued
/// even when nobody flagged it shaky, because that decoder trades words for
/// CPU by design and the archive is owed a pass at the better one regardless.
#[test]
fn a_light_decoded_row_is_queued_even_when_solid() {
    let store = Store::open_in_memory().expect("store");
    let source = store.upsert_source("VRChat.exe", "VRChat", 0).expect("src");
    let session = store.begin_session(source, 0).expect("session");
    let row = |start: i64, model_id: &str, verdict: Option<&str>| {
        let id = store
            .insert_segment(session, start, start + 2_000_000_000, "a.wav", 0)
            .expect("segment");
        store
            .set_segment_analysis(
                id,
                &SegmentAnalysis {
                    text: Some("hallo".into()),
                    lang: None,
                    lang_via: None,
                    asr_model_id: Some(format!("{model_id}@1")),
                    overlap_frac: None,
                },
            )
            .expect("analysis");
        if let Some(v) = verdict {
            store.set_segment_confidence(id, Some(v), 1).expect("flag");
        }
        id
    };
    let light_solid = row(0, FALLBACK_ASR.dir, Some("solid"));
    let default_solid = row(
        3_000_000_000,
        "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8",
        Some("solid"),
    );

    let queued: Vec<i64> = store
        .segments_for_night(10, FALLBACK_ASR.dir)
        .expect("queue")
        .iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(
        queued,
        vec![light_solid],
        "the light row is queued despite being solid; the default row is not"
    );
    assert!(!queued.contains(&default_solid));
}

/// A replacement goes through the same door a context re-decode does: the prior
/// words in `operations`, the cross-check verdict cleared, `text_via` naming
/// the route that made the edit.
#[test]
fn a_night_replacement_keeps_the_words_it_replaces() {
    let store = Store::open_in_memory().expect("store");
    let source = store.upsert_source("VRChat.exe", "VRChat", 0).expect("src");
    let session = store.begin_session(source, 0).expect("session");
    let id = store
        .insert_segment(session, 0, 2_000_000_000, "a.wav", 0)
        .expect("segment");
    store
        .set_segment_analysis(
            id,
            &SegmentAnalysis {
                text: Some("komm ich sag the country".into()),
                lang: Some("de".into()),
                lang_via: None,
                asr_model_id: Some("parakeet".into()),
                overlap_frac: None,
            },
        )
        .expect("analysis");
    store
        .set_segment_confidence(id, Some("shaky"), 1)
        .expect("flag");

    store
        .set_segment_night(id, Some("Ich sage dir keins."), 9)
        .expect("annotate");
    store
        .set_segment_text_via(
            id,
            "Ich sage dir keins.",
            "ggml-large-v3",
            text_via::NIGHT,
            9,
        )
        .expect("replace");

    let row = store.segment_row(id).expect("row").expect("live");
    assert_eq!(row.text.as_deref(), Some("Ich sage dir keins."));
    assert_eq!(row.text_via.as_deref(), Some("night"));
    assert_eq!(row.night_text.as_deref(), Some("Ich sage dir keins."));
    assert_eq!(
        row.asr_confidence, None,
        "the verdict was about the old words and is cleared with them"
    );

    let ops = store
        .operations_of("segments.redecode", 10)
        .expect("operations");
    assert_eq!(ops.len(), 1, "one edit, one operations row");
    let prior: serde_json::Value = serde_json::from_str(&ops[0].prior_state).expect("json");
    assert_eq!(prior["text"], serde_json::json!("komm ich sag the country"));
    assert_eq!(prior["asr_model_id"], serde_json::json!("parakeet"));
    assert!(
        store
            .operations_of("segments.correct", 10)
            .expect("operations")
            .is_empty(),
        "a machine's edit must not masquerade as a person's"
    );
}

/// The annotate-only outcome: `night_text` is written and the transcript is
/// untouched. This is what ships when the vote's gate is not cleared, so it has
/// to be a first-class state rather than an absence.
#[test]
fn an_annotation_leaves_the_transcript_exactly_as_it_was() {
    let store = Store::open_in_memory().expect("store");
    let source = store.upsert_source("VRChat.exe", "VRChat", 0).expect("src");
    let session = store.begin_session(source, 0).expect("session");
    let id = store
        .insert_segment(session, 0, 2_000_000_000, "a.wav", 0)
        .expect("segment");
    store
        .set_segment_analysis(
            id,
            &SegmentAnalysis {
                text: Some("ähm ja also".into()),
                lang: Some("de".into()),
                lang_via: None,
                asr_model_id: Some("parakeet".into()),
                overlap_frac: None,
            },
        )
        .expect("analysis");
    store
        .set_segment_confidence(id, Some("shaky"), 1)
        .expect("flag");
    store
        .set_segment_night(id, Some("Ähm, ja, also dann."), 9)
        .expect("annotate");

    let row = store.segment_row(id).expect("row").expect("live");
    assert_eq!(row.text.as_deref(), Some("ähm ja also"));
    assert_eq!(row.text_via.as_deref(), Some("live"));
    assert_eq!(row.night_text.as_deref(), Some("Ähm, ja, also dann."));
    assert_eq!(
        row.asr_confidence.as_deref(),
        Some("shaky"),
        "an annotation is not a new transcript, so the verdict still stands"
    );
    assert!(
        store
            .operations_of("segments.redecode", 10)
            .expect("operations")
            .is_empty(),
        "nothing was replaced, so there is nothing to keep"
    );
}

// ---------------------------------------------------------------------------
// the batch
// ---------------------------------------------------------------------------

/// The round trip that matters: pack N clips, decode "them", split the lines
/// back out, and land every word in the row it came from. An error here is a
/// data-corruption bug wearing a transcription bug's costume.
#[test]
fn a_packed_batch_round_trips_every_clip_to_its_own_row() {
    let clip = |seconds: f32| vec![0.5f32; (seconds * SAMPLE_RATE as f32) as usize];
    let lengths = [1.2f32, 2.0, 0.8, 3.1];
    let packed = pack(&lengths.iter().map(|s| clip(*s)).collect::<Vec<_>>(), 1.0);
    assert_eq!(packed.slots.len(), 4);
    // Every clip is where the arithmetic says: start = sum of the ones before
    // it plus one gap each.
    let mut expect = 0.0f32;
    for (i, len) in lengths.iter().enumerate() {
        assert!(
            (packed.slots[i].from_s - expect).abs() < 1e-3,
            "clip {i} starts at {} not {expect}",
            packed.slots[i].from_s
        );
        expect += len + 1.0;
    }

    // The decoder answers with one line per clip, its edges rounded outwards by
    // a quarter second in both directions the way whisper's are.
    let said = ["erste zeile", "zweite zeile", "dritte", "vierte zeile hier"];
    let lines: Vec<Utterance> = packed
        .slots
        .iter()
        .zip(said)
        .map(|(slot, text)| Utterance {
            from_s: (slot.from_s - 0.25).max(0.0),
            to_s: slot.to_s + 0.25,
            text: text.into(),
        })
        .collect();
    assert_eq!(split_by_offsets(&lines, &packed.slots), said.to_vec());
}

#[test]
fn a_line_in_the_silence_between_two_turns_belongs_to_neither() {
    let slots = [
        Slot {
            from_s: 0.0,
            to_s: 2.0,
        },
        Slot {
            from_s: 3.0,
            to_s: 5.0,
        },
    ];
    let lines = [Utterance {
        from_s: 2.2,
        to_s: 2.7,
        text: "(Musik)".into(),
    }];
    assert_eq!(split_by_offsets(&lines, &slots), vec!["", ""]);
}

/// whisper-cli's `-oj` file, parsed as the daemon parses it: milliseconds, and
/// the language the decoder actually used.
#[test]
fn the_decoders_json_is_read_in_milliseconds() {
    let text = r#"{
        "result": { "language": "de" },
        "transcription": [
            { "offsets": { "from": 0, "to": 1500 }, "text": " Slalom bei den" },
            { "offsets": { "from": 2500, "to": 4000 }, "text": " zweite Zeile" }
        ]
    }"#;
    let (lines, lang) = parse_whisper_json(text);
    assert_eq!(lang, "de");
    assert_eq!(lines.len(), 2);
    assert!((lines[0].from_s - 0.0).abs() < 1e-6);
    assert!((lines[0].to_s - 1.5).abs() < 1e-6);
    assert!((lines[1].from_s - 2.5).abs() < 1e-6);
    assert_eq!(lines[0].text, "Slalom bei den");
}

#[test]
fn an_unreadable_answer_costs_one_batch_and_not_a_panic() {
    let (lines, lang) = parse_whisper_json("this is not json");
    assert!(lines.is_empty());
    assert!(lang.is_empty());
    let (lines, _) = parse_whisper_json("{}");
    assert!(lines.is_empty());
}

// ---------------------------------------------------------------------------
// the gates
// ---------------------------------------------------------------------------

#[test]
fn the_shipped_window_covers_the_small_hours_only() {
    let cfg = NightConfig::default();
    let hours = Hours::parse(&cfg.window).expect("the shipped window parses");
    assert!(hours.contains(4 * 60));
    assert!(!hours.contains(21 * 60));
    assert!(!hours.contains(9 * 60));
}

#[test]
fn the_gpu_check_reads_the_card_with_the_vram() {
    let dir = std::env::temp_dir().join(format!("nxr-night-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let device = dir.join("card1").join("device");
    std::fs::create_dir_all(&device).expect("mkdir");
    std::fs::write(
        device.join("mem_info_vram_total"),
        (24u64 << 30).to_string(),
    )
    .expect("vram");
    std::fs::write(device.join("gpu_busy_percent"), "73").expect("busy");
    assert_eq!(gpu_busy_pct_in(&dir), Some(73));
    // Nothing to read at all is `None`, and `None` shuts the gate.
    assert_eq!(gpu_busy_pct_in(&PathBuf::from("/nonexistent-sysfs")), None);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// the vote
// ---------------------------------------------------------------------------

/// The table the feature rests on, including §12's hallucinations. Each row is
/// (live, canary, night, night language, row language, may replace) and what
/// the vote must do with it.
#[test]
fn the_vote_table() {
    struct Case {
        why: &'static str,
        live: &'static str,
        canary: Option<&'static str>,
        night: &'static str,
        night_lang: Option<&'static str>,
        row_lang: Option<&'static str>,
        replace: bool,
        expect_replace: bool,
    }
    let cases = [
        Case {
            why: "two readings agree against the row",
            live: "komm ich sag the country",
            canary: Some("ich sage dir keins"),
            night: "Ich sage dir keins.",
            night_lang: Some("de"),
            row_lang: Some("de"),
            replace: true,
            expect_replace: true,
        },
        Case {
            why: "the cross-check is with the row, so the night is outvoted",
            live: "das war ganz gut",
            canary: Some("das war ganz gut"),
            night: "Das war ganz schlecht.",
            night_lang: Some("de"),
            row_lang: Some("de"),
            replace: true,
            expect_replace: false,
        },
        Case {
            why: "an Arabic hallucination on a German row (FINDINGS §12)",
            live: "ähm ja also",
            canary: Some("شكرا لمشاهدتكم"),
            night: "شكرا لمشاهدتكم",
            night_lang: Some("de"),
            row_lang: Some("de"),
            replace: true,
            expect_replace: false,
        },
        Case {
            why: "a Finnish hallucination on a German row (FINDINGS §12)",
            live: "ja genau",
            canary: Some("Kiitos kun katsoitte"),
            night: "Kiitos kun katsoitte.",
            night_lang: Some("de"),
            row_lang: Some("de"),
            replace: true,
            expect_replace: false,
        },
        Case {
            why: "English on a German row is information, not permission",
            live: "ich hab das gestern gemacht",
            canary: Some("I did that yesterday"),
            night: "I did that yesterday.",
            night_lang: Some("en"),
            row_lang: Some("de"),
            replace: true,
            expect_replace: false,
        },
        Case {
            why: "a row with no language can never satisfy the language guard",
            live: "mhm ok",
            canary: Some("Mhm okay dann"),
            night: "Mhm okay dann.",
            night_lang: None,
            row_lang: None,
            replace: true,
            expect_replace: false,
        },
        Case {
            why: "one word is not evidence",
            live: "was hast du gesagt",
            canary: Some("Ja"),
            night: "Ja.",
            night_lang: Some("de"),
            row_lang: Some("de"),
            replace: true,
            expect_replace: false,
        },
        Case {
            why: "no cross-check reading means no majority",
            live: "hallo zusammen",
            canary: None,
            night: "Hallo zusammen alle.",
            night_lang: Some("de"),
            row_lang: Some("de"),
            replace: true,
            expect_replace: false,
        },
        Case {
            why: "the shipped switch: annotate, never replace",
            live: "komm ich sag the country",
            canary: Some("ich sage dir keins"),
            night: "Ich sage dir keins.",
            night_lang: Some("de"),
            row_lang: Some("de"),
            replace: false,
            expect_replace: false,
        },
    ];
    for case in cases {
        let vote = judge_vote(
            &Readings {
                live: case.live,
                canary: case.canary,
                night: case.night,
                night_lang: case.night_lang,
                row_lang: case.row_lang,
            },
            0.5,
            case.replace,
        );
        let replaced = matches!(vote, Vote::Replace { .. });
        assert_eq!(replaced, case.expect_replace, "{}: {vote:?}", case.why);
    }
}
