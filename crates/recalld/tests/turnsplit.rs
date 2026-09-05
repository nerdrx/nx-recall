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
use recalld::embed::Embedding;
use recalld::ingest::read_wav;
use recalld::models::ModelSet;

/// The bank the detector's voicebank veto reads (FINDINGS §52): every
/// prototype, `(speaker_id, source_segment_id, embedding)` — the same shape
/// `Store::prototypes_with_source` hands the live path. Built from the
/// analyzer's own embedder over each speaker's whole clip, which is what an
/// install that has actually enrolled these two people would have on file.
/// An empty bank vetoes every boundary, so a test of the switch **on** needs
/// this or it is indistinguishable from the switch being off.
fn seeded_bank(
    a: &mut Analyzer,
    a_clip: &[f32],
    b_clip: &[f32],
) -> Vec<(i64, Option<i64>, Embedding)> {
    let ea = a
        .prepare(a_clip)
        .expect("preparing reader A")
        .embedding
        .expect("clean solo audio embeds");
    let eb = a
        .prepare(b_clip)
        .expect("preparing reader B")
        .embedding
        .expect("clean solo audio embeds");
    vec![(1, None, ea), (2, None, eb)]
}

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

/// The distance bar these fixtures need — which, since §52, **is** the
/// shipped one.
///
/// §39 shipped 0.85, fitted where the false-split rate crosses 1% on this
/// install's Discord-decoded audio, where two 1.5 s windows of the *same*
/// person already score `1 - cos ≈ 0.62`. The fixtures are studio LibriSpeech:
/// the same-speaker floor there is 0.31–0.49 and the cross-speaker peak at the
/// join is 0.839 — under 0.85, over 0.80. §52 moved the shipped bar to 0.80
/// because the voicebank veto needed the room, and the peak clearing it on a
/// domain nobody fitted it against is a coincidence worth stating plainly
/// rather than claiming as design: the veto is what makes the detector travel
/// (`identity::rank_with`'s argmax carries no domain-specific scale), the
/// distance bar is still one absolute number and it happens to still work
/// here. `spike/turnsplit_fixture.py`, FINDINGS §39 and §52.
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
    let (a_clip, b_clip) = (fixture("clean_single_0.wav"), fixture("clean_single_1.wav"));
    let bank = seeded_bank(&mut a, &a_clip, &b_clip);
    let (samples, _) = two_speakers();
    let plan = a.plan_split(&samples, &bank, None).expect("planning");
    assert_eq!(
        plan.pieces.len(),
        1,
        "the switch is off; a full bank must not matter"
    );
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
    let (a_clip, b_clip) = (fixture("clean_single_0.wav"), fixture("clean_single_1.wav"));
    let bank = seeded_bank(&mut a, &a_clip, &b_clip);
    let (samples, at) = two_speakers();
    let plan = a.plan_split(&samples, &bank, None).expect("planning");
    assert!(
        plan.cut(),
        "two readers back to back, both enrolled, must be found: {:?}",
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

/// A cold-start install — the switch is on but nobody has been enrolled yet —
/// must not cut anything, because the voicebank veto has no second voice to
/// disagree with (FINDINGS §52). This is the measured shape, not a fallback
/// path: an empty bank is passed exactly as the live pipeline would pass one
/// before its first two people exist.
#[test]
fn with_no_bank_the_switch_being_on_still_cuts_nothing() {
    let Some(mut a) = analyzer(true) else {
        eprintln!("skipping: set NXR_MODELS=<dir> to run it");
        return;
    };
    let (samples, _) = two_speakers();
    let plan = a.plan_split(&samples, &[], None).expect("planning");
    assert!(
        !plan.cut(),
        "an empty bank must veto every candidate, not just most of them: {:?}",
        plan.pieces.iter().map(|(_, t)| t).collect::<Vec<_>>()
    );
}

#[test]
fn the_pieces_words_are_the_turns_words() {
    let Some(mut a) = analyzer(true) else {
        eprintln!("skipping: set NXR_MODELS=<dir> to run it");
        return;
    };
    let (a_clip, b_clip) = (fixture("clean_single_0.wav"), fixture("clean_single_1.wav"));
    let bank = seeded_bank(&mut a, &a_clip, &b_clip);
    let (samples, _) = two_speakers();
    let split = a.plan_split(&samples, &bank, None).expect("planning");
    assert!(split.cut(), "this test is about a turn that was cut");

    // The claim the whole design rests on: the pieces' words, concatenated in
    // order, are the words of one decode of the whole turn — none lost at the
    // cut, none spelled twice. Compared against the same analyzer's own
    // uncut reading of the same audio.
    let mut off = analyzer(false).expect("models were there a moment ago");
    let whole = off.plan_split(&samples, &[], None).expect("planning");
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
/// that travels — in both directions.
///
/// **Without a bank**, the shipped bar cuts nothing, whatever the distance
/// curve says: the veto has no second voice to disagree with. **With both
/// readers enrolled**, it does cut, and at the right place — the join peaks at
/// 0.839, over the shipped 0.80. That the peak clears a bar fitted on a
/// different install's Discord audio is not a design claim about the distance
/// arm (it is still one absolute number, and §39 measured it does *not*
/// travel at 0.85); it is a measured fact about 0.80 worth pinning so a later
/// round that moves the bar again sees this test move with it
/// (`spike/turnsplit_fixture.py`, FINDINGS §39, §52).
#[test]
fn the_shipped_bar_needs_the_bank_but_not_a_different_bar() {
    let Some(mut a) = analyzer_at(true, IdentityConfig::default().split_turn_distance) else {
        eprintln!("skipping: set NXR_MODELS=<dir> to run it");
        return;
    };
    let (samples, at) = two_speakers();

    assert!(
        !a.plan_split(&samples, &[], None).expect("planning").cut(),
        "an unenrolled install must not cut studio audio either"
    );

    let (a_clip, b_clip) = (fixture("clean_single_0.wav"), fixture("clean_single_1.wav"));
    let bank = seeded_bank(&mut a, &a_clip, &b_clip);
    let plan = a.plan_split(&samples, &bank, None).expect("planning");
    assert!(
        plan.cut(),
        "the shipped 0.80 no longer fires on LibriSpeech once both readers \
         are enrolled — re-read FINDINGS §52 and re-measure before lowering it"
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
}

#[test]
fn one_speaker_alone_is_not_cut() {
    let Some(mut a) = analyzer(true) else {
        eprintln!("skipping: set NXR_MODELS=<dir> to run it");
        return;
    };
    // 5.17 s of one reader: long enough for eight legal cut points and no
    // reason to take any of them. Enrolled against a second, different
    // voice — a bank that could in principle disagree, and does not, because
    // there is only one person in this clip.
    let a_clip = fixture("clean_single_0.wav");
    let b_clip = fixture("clean_single_1.wav");
    let bank = seeded_bank(&mut a, &a_clip, &b_clip);
    let samples = a_clip;
    let plan = a.plan_split(&samples, &bank, None).expect("planning");
    assert!(
        !plan.cut(),
        "one voice was cut into {} pieces: {:?}",
        plan.pieces.len(),
        plan.pieces.iter().map(|(_, t)| t).collect::<Vec<_>>()
    );
}
