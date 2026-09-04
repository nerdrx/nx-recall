//! Cutting a turn where the speaker changes, against the real models.
//!
//! The unit tests in `crate::turnsplit` prove the arithmetic with synthetic
//! vectors. These prove the two claims that only real audio can settle, and
//! they prove **both states of the switch**, because a feature that ships off
//! has to be tested off:
//!
//! * off, a turn is one row and the words are the decoder's own string;
//! * on, two speakers glued together are cut where they meet, one speaker
//!   alone is not cut at all, and the pieces' words are the turn's words.
//!
//! Gated on `NXR_MODELS=<dir>` like the rest of the acceptance suite: without
//! it they report themselves skipped and pass.

use std::path::PathBuf;

use recalld::analysis::Analyzer;
use recalld::asr::normalise_words;
use recalld::config::{Config, IdentityConfig, SAMPLE_RATE};
use recalld::ingest::read_wav;
use recalld::models::ModelSet;

fn models_dir() -> Option<PathBuf> {
    let raw = std::env::var("NXR_MODELS").ok()?;
    if raw.trim().is_empty() {
        return None;
    }
    Some(PathBuf::from(raw))
}

fn fixture(name: &str) -> Vec<f32> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(name);
    read_wav(&path).expect("fixture")
}

/// The bar these fixtures need, and why it is not the shipped 0.85.
///
/// The shipped bar is fitted where the false-split rate crosses 1% on **this
/// install's Discord-decoded audio**, where two 1.5 s windows of the *same*
/// person already score `1 - cos ≈ 0.62`. The fixtures are studio LibriSpeech:
/// the same-speaker floor there is 0.31–0.49 and the cross-speaker peak at the
/// join is 0.839, so the whole curve sits below a bar calibrated against
/// Discord's noise. The peak is unmistakable in its own turn — twice the
/// turn's median — and 0.011 under a threshold that was never measured on this
/// material (`spike/turnsplit_fixture.py`, FINDINGS §39).
///
/// So these tests move the bar and say so, rather than the bar moving to suit
/// the tests. `the_shipped_bar_does_not_fire_on_studio_audio` pins the fact.
const STUDIO_DISTANCE: f32 = 0.80;

fn analyzer_at(split_turns: bool, distance: f32) -> Option<Analyzer> {
    let dir = models_dir()?;
    let mut cfg = Config::default();
    cfg.models.dir = Some(dir);
    let mut models = ModelSet::resolve(&cfg.models)?;
    models.select_asr();
    if !models.complete() {
        eprintln!("skipping: NXR_MODELS is set but incomplete");
        return None;
    }
    let identity = IdentityConfig {
        split_turns,
        split_turn_distance: distance,
        ..Default::default()
    };
    Some(Analyzer::load(&models, &identity).expect("the analyzer loads"))
}

fn analyzer(split_turns: bool) -> Option<Analyzer> {
    analyzer_at(split_turns, STUDIO_DISTANCE)
}

/// Two different LibriSpeech readers back to back: speaker 6295 for 5.17 s,
/// then speaker 3170. One change point, and nothing else in the turn.
fn two_speakers() -> (Vec<f32>, f32) {
    let a = fixture("clean_single_0.wav");
    let b = fixture("clean_single_1.wav");
    let at = a.len() as f32 / SAMPLE_RATE as f32;
    let mut glued = a;
    glued.extend_from_slice(&b);
    (glued, at)
}

#[test]
fn with_the_switch_off_a_turn_is_one_row() {
    let Some(mut a) = analyzer(false) else {
        eprintln!("skipping: set NXR_MODELS=<dir> to run it");
        return;
    };
    let (samples, _) = two_speakers();
    let plan = a.plan_split(&samples).expect("planning");
    assert_eq!(plan.pieces.len(), 1, "the default must never cut");
    assert!(!plan.cut());
    assert!(!plan.wordless);
    assert_eq!(plan.pieces[0].0.from, 0);
    assert_eq!(plan.pieces[0].0.to, samples.len());
    // And the words are the decoder's own string, punctuation and all, rather
    // than one rebuilt from the word list.
    let words = normalise_words(&plan.pieces[0].1);
    assert!(
        words.contains(&"SOLITUDE".to_string()) && words.contains(&"ASTOUNDED".to_string()),
        "both readers' words are in the one row: {:?}",
        plan.pieces[0].1
    );
}

#[test]
fn with_the_switch_on_two_speakers_are_cut_where_they_meet() {
    let Some(mut a) = analyzer(true) else {
        eprintln!("skipping: set NXR_MODELS=<dir> to run it");
        return;
    };
    let (samples, at) = two_speakers();
    let plan = a.plan_split(&samples).expect("planning");
    assert!(
        plan.cut(),
        "two readers back to back must be found: {:?}",
        plan.pieces.iter().map(|(_, t)| t).collect::<Vec<_>>()
    );
    let cuts: Vec<f32> = plan
        .pieces
        .iter()
        .skip(1)
        .map(|(p, _)| p.from as f32 / SAMPLE_RATE as f32)
        .collect();
    assert!(
        cuts.iter().any(|c| (c - at).abs() <= 0.5),
        "a cut within ±0.5 s of {at:.2}s, got {cuts:?}"
    );
    // Every piece clears the ladder's floor, which is the whole point of
    // having one.
    for (p, _) in &plan.pieces {
        assert!(
            p.len() >= SAMPLE_RATE as usize,
            "a piece shorter than the ladder can label: {p:?}"
        );
    }
}

#[test]
fn the_pieces_words_are_the_turns_words() {
    let Some(mut a) = analyzer(true) else {
        eprintln!("skipping: set NXR_MODELS=<dir> to run it");
        return;
    };
    let (samples, _) = two_speakers();
    let split = a.plan_split(&samples).expect("planning");
    assert!(split.cut(), "this test is about a turn that was cut");

    // The claim the whole design rests on: the pieces' words, concatenated in
    // order, are the words of one decode of the whole turn — none lost at the
    // cut, none spelled twice. Compared against the same analyzer's own
    // uncut reading of the same audio.
    let mut off = analyzer(false).expect("models were there a moment ago");
    let whole = off.plan_split(&samples).expect("planning");
    let expected = normalise_words(&whole.pieces[0].1);
    let joined: Vec<String> = split
        .pieces
        .iter()
        .flat_map(|(_, t)| normalise_words(t))
        .collect();
    assert_eq!(joined, expected, "the split must not touch the transcript");
    assert!(!expected.is_empty(), "the fixture has words in it");
}

/// The operating point is fitted to one install's audio, and this pins how far
/// that goes.
///
/// The shipped 0.85 does **not** fire on two studio readers glued together —
/// the join peaks at 0.839. That is not a bug in the detector (the peak is
/// exactly where the readers meet, and it is twice the turn's own median) and
/// not a reason to lower the bar to suit a fixture: it is the measured cost of
/// a threshold calibrated where same-speaker windows score 0.62. If a later
/// round makes the detector domain-independent — a contrast bar, or a
/// different extractor — this test is the one that will start failing, and it
/// should be read as the good news it is.
#[test]
fn the_shipped_bar_does_not_fire_on_studio_audio() {
    let Some(mut a) = analyzer_at(true, IdentityConfig::default().split_turn_distance) else {
        eprintln!("skipping: set NXR_MODELS=<dir> to run it");
        return;
    };
    let (samples, _) = two_speakers();
    assert!(
        !a.plan_split(&samples).expect("planning").cut(),
        "the shipped bar now fires on LibriSpeech — re-read FINDINGS §39 and \
         re-measure the false-split rate before celebrating"
    );
}

#[test]
fn one_speaker_alone_is_not_cut() {
    let Some(mut a) = analyzer(true) else {
        eprintln!("skipping: set NXR_MODELS=<dir> to run it");
        return;
    };
    // 5.17 s of one reader: long enough for eight legal cut points and no
    // reason to take any of them.
    let samples = fixture("clean_single_0.wav");
    let plan = a.plan_split(&samples).expect("planning");
    assert!(
        !plan.cut(),
        "one voice was cut into {} pieces: {:?}",
        plan.pieces.len(),
        plan.pieces.iter().map(|(_, t)| t).collect::<Vec<_>>()
    );
}
