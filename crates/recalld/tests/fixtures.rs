//! Acceptance suite: the real pipeline against the golden fixtures.
//!
//! Gated on `NXR_MODELS=<dir>`, because the models are hundreds of megabytes
//! and are not in the repository. Without it every test here reports itself
//! skipped and passes, so `cargo test` stays useful on a machine that has no
//! models.
//!
//! Everything runs against a throwaway data directory. The daemon's live
//! database is never opened.

use std::path::{Path, PathBuf};

use recalld::analysis::Analyzer;
use recalld::asr::normalise_words;
use recalld::config::{Config, SAMPLE_RATE};
use recalld::ingest::{OfflinePipeline, ingest_pcm, read_wav};
use recalld::models::ModelSet;
use recalld::store::Store;
use recalld::vad::SileroVad;

fn models_dir() -> Option<PathBuf> {
    let raw = std::env::var("NXR_MODELS").ok()?;
    if raw.trim().is_empty() {
        return None;
    }
    Some(PathBuf::from(raw))
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
}

fn manifest() -> serde_json::Value {
    let text = std::fs::read_to_string(fixtures_dir().join("manifest.json"))
        .expect("fixtures/manifest.json must be readable");
    serde_json::from_str(&text).expect("fixtures/manifest.json must be valid JSON")
}

fn target_transcript(file: &str) -> String {
    manifest()["fixtures"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["file"] == file)
        .and_then(|f| f["target_transcript"].as_str())
        .unwrap_or_else(|| panic!("{file} has no target_transcript in the manifest"))
        .to_string()
}

/// Word-level edit distance over the case- and punctuation-normalised forms.
fn wer(reference: &str, hypothesis: &str) -> f32 {
    let r = normalise_words(reference);
    let h = normalise_words(hypothesis);
    if r.is_empty() {
        return if h.is_empty() { 0.0 } else { 1.0 };
    }
    let mut prev: Vec<usize> = (0..=h.len()).collect();
    let mut curr = vec![0usize; h.len() + 1];
    for (i, rw) in r.iter().enumerate() {
        curr[0] = i + 1;
        for (j, hw) in h.iter().enumerate() {
            let sub = prev[j] + usize::from(rw != hw);
            curr[j + 1] = sub.min(prev[j + 1] + 1).min(curr[j] + 1);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[h.len()] as f32 / r.len() as f32
}

/// An isolated data directory plus a loaded pipeline.
struct Rig {
    dir: PathBuf,
    cfg: Config,
    models: ModelSet,
    store: Store,
    vad: SileroVad,
    analyzer: Analyzer,
    session: i64,
    clock_ns: i64,
}

impl Rig {
    fn new(name: &str, models: &Path) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "nx-recall-acceptance-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creating the throwaway data dir");

        let mut cfg = Config::default();
        cfg.models.dir = Some(models.to_path_buf());
        let mut set = ModelSet::resolve(&cfg.models).expect("NXR_MODELS resolves to a model set");
        // Exactly what the daemon does at start-up. A models dir holding only
        // the English-only export runs the whole suite in fallback mode, which
        // is the point: the acceptance budgets must hold either way.
        let selection = set.select_asr();
        let missing = set.missing();
        assert!(
            missing.is_empty(),
            "NXR_MODELS={} is missing {:?}",
            models.display(),
            missing.iter().map(|e| e.role).collect::<Vec<_>>()
        );
        eprintln!("  asr: {} ({selection:?})", set.asr_model_id());

        let store = Store::open(&dir).expect("opening the throwaway database");
        let source = store.upsert_source("fixtures", "fixtures", 0).unwrap();
        let session = store.begin_session(source, 0).unwrap();
        let vad = SileroVad::from_bytes(recalld::VAD_MODEL).expect("bundled VAD model");
        let analyzer = Analyzer::load(&set, &cfg.identity).expect("loading the analysis models");

        Self {
            dir,
            cfg,
            models: set,
            store,
            vad,
            analyzer,
            session,
            clock_ns: 0,
        }
    }

    /// Run one fixture through VAD, turn merging and the analysis leg.
    fn ingest(&mut self, fixture: &str) -> Vec<i64> {
        let samples = read_wav(&fixtures_dir().join(fixture))
            .unwrap_or_else(|e| panic!("reading {fixture}: {e:#}"));
        self.ingest_samples(&samples)
    }

    /// The same, from a buffer — for cases the fixture set does not contain,
    /// such as "the same person, but long enough to be worth an identity".
    fn ingest_samples(&mut self, samples: &[f32]) -> Vec<i64> {
        let t0 = self.clock_ns;
        // Space fixtures a minute apart so nothing merges across files.
        self.clock_ns += 60_000_000_000;
        let mut pipe = OfflinePipeline::new(&mut self.vad, &self.cfg, Some(&mut self.analyzer));
        ingest_pcm(&self.store, &self.dir, self.session, samples, t0, &mut pipe)
            .unwrap_or_else(|e| panic!("ingesting {} samples: {e:#}", samples.len()))
    }

    fn text_of(&self, ids: &[i64]) -> String {
        ids.iter()
            .filter_map(|id| self.store.segment_fields(*id).unwrap()["text"].clone())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The model stamped on the row. Present even when the transcript is empty:
    /// that is how "the analysis leg ran and had nothing to say" is told apart
    /// from "the analysis leg never looked at this audio".
    fn asr_model_of(&self, id: i64) -> Option<String> {
        self.store.segment_fields(id).unwrap()["asr_model_id"].clone()
    }

    fn overlap_of(&self, id: i64) -> f32 {
        self.store.segment_fields(id).unwrap()["overlap_frac"]
            .as_ref()
            .unwrap_or_else(|| panic!("segment {id} has no overlap_frac"))
            .parse()
            .unwrap()
    }

    fn speaker_of(&self, id: i64) -> Option<i64> {
        self.store.segment_fields(id).unwrap()["speaker_id"]
            .as_ref()
            .map(|v| v.parse().unwrap())
    }

    fn score_of(&self, id: i64) -> Option<f32> {
        self.store.segment_fields(id).unwrap()["match_score"]
            .as_ref()
            .map(|v| v.parse().unwrap())
    }

    // ---- 0.7.7: the conversational language prior ------------------------

    /// Is the German arbiter installed under `NXR_MODELS`? It is optional, so
    /// its absence skips rather than fails.
    fn arbiter_installed(&self) -> bool {
        self.models.arbiter(recalld::models::ARBITER_DE).present()
    }

    /// One of the three German fixtures the export actually reads as German.
    ///
    /// Which one is not fixed: the point of the fixtures is that they are real
    /// audio through the real model, and asserting *which* of them decodes
    /// cleanly would be asserting a fact about a model version.
    fn a_fixture_read_as_german(&mut self) -> Option<String> {
        for fixture in [
            "de/de_short_0.wav",
            "de/de_short_1.wav",
            "de/de_short_2.wav",
        ] {
            for id in self.ingest(fixture) {
                if let Some(text) = self.store.segment_fields(id).unwrap()["text"].clone()
                    && recalld::lang::classify(&text) == recalld::lang::Lang::De
                {
                    return Some(fixture.to_string());
                }
            }
        }
        None
    }

    /// A conversation to put turns in.
    ///
    /// Set directly rather than through `threads::assign`: the rule that
    /// decides which conversation a turn joins has its own tests, and a
    /// language test that also depended on it would fail for two reasons.
    fn new_thread(&self) -> i64 {
        self.store.create_thread(self.session, 0, 0).unwrap()
    }

    fn put_in_thread(&self, segment_id: i64, thread_id: i64) {
        let (_, _, t_end_ns) = self.store.segment_audio(segment_id).unwrap().unwrap();
        self.store
            .set_segment_thread(segment_id, thread_id, t_end_ns)
            .unwrap();
    }

    /// How many turns in `thread_id` the classifier read as German — the
    /// evidence the context is built from.
    fn german_stamps_in(&self, thread_id: i64) -> usize {
        self.store
            .thread_language_stamps(thread_id, 0, 64)
            .unwrap()
            .iter()
            .filter(|l| *l == "de")
            .count()
    }

    fn samples_of(&self, segment_id: i64) -> Vec<f32> {
        let rel = self.store.segment_audio(segment_id).unwrap().unwrap().0;
        read_wav(&self.dir.join(&rel)).unwrap()
    }

    /// Make a row say something it does not say, leaving its **audio** alone.
    ///
    /// This is the injected flip. At 3.5 s the export gets these fixtures
    /// right, so waiting for a real 12%-at-1 s event would be a test that
    /// fails most of the time; what is under test is what happens *after* a
    /// flip, and the audio under it is real German either way.
    fn flip_to(&self, segment_id: i64, text: &str) {
        self.store
            .set_segment_analysis(
                segment_id,
                &recalld::store::SegmentAnalysis {
                    text: Some(text.into()),
                    lang: recalld::lang::classify(text).tag().map(str::to_string),
                    lang_via: Some(recalld::store::lang_via::CLASSIFIED.into()),
                    asr_model_id: Some(self.models.asr_model_id()),
                    overlap_frac: Some(0.02),
                },
            )
            .unwrap();
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Every test opens with this; `None` means "no models, nothing to assert".
macro_rules! rig {
    ($name:literal) => {
        match models_dir() {
            None => {
                eprintln!("skipping {}: set NXR_MODELS=<dir> to run it", $name);
                return;
            }
            Some(dir) => Rig::new($name, &dir),
        }
    };
}

// ---- 1. ASR ------------------------------------------------------------

#[test]
fn clean_fixtures_transcribe_within_the_wer_budget() {
    let mut rig = rig!("asr");
    // Whichever export was selected — the multilingual default or the
    // English-only fallback — the budget is the same and the rows have to say
    // which one wrote them.
    let model = rig.models.asr_model_id();
    for fixture in ["clean_single_0.wav", "clean_single_1.wav"] {
        let ids = rig.ingest(fixture);
        assert!(!ids.is_empty(), "{fixture} produced no segments at all");
        for id in &ids {
            assert_eq!(
                rig.asr_model_of(*id).as_deref(),
                Some(model.as_str()),
                "{fixture} segment {id} carries the wrong ASR provenance"
            );
        }
        let got = rig.text_of(&ids);
        let want = target_transcript(fixture);
        let rate = wer(&want, &got);
        assert!(
            rate <= 0.15,
            "{fixture}: WER {:.1}% over the 15% budget\n  want: {want}\n  got:  {got}",
            rate * 100.0
        );
    }
}

// ---- 2. the overlap gate ----------------------------------------------

#[test]
fn clean_speech_reads_as_single_speaker() {
    let mut rig = rig!("overlap-clean");
    for fixture in ["clean_single_0.wav", "clean_single_1.wav"] {
        let ids = rig.ingest(fixture);
        assert!(!ids.is_empty(), "{fixture} produced no segments at all");
        for id in ids {
            let frac = rig.overlap_of(id);
            assert!(
                frac < 0.1,
                "{fixture} segment {id} read as {frac:.3} overlapped; clean speech must not"
            );
        }
    }
}

#[test]
fn equal_loudness_mixes_are_flagged_and_refused_a_speaker() {
    let mut rig = rig!("overlap-equal");
    for fixture in [
        "duo_equal_0.wav",
        "duo_equal_1.wav",
        "lobby_equal_0.wav",
        "lobby_equal_1.wav",
    ] {
        let ids = rig.ingest(fixture);
        assert!(!ids.is_empty(), "{fixture} produced no segments at all");
        let worst = ids
            .iter()
            .map(|id| rig.overlap_of(*id))
            .fold(0.0f32, f32::max);
        assert!(
            worst > 0.5,
            "{fixture} peaked at {worst:.3} overlap; the gate would let it through"
        );
        for id in &ids {
            // The manifest is explicit that guessing right here is still a
            // failure: an equal-loudness mix must be refused, not labelled.
            assert_eq!(
                rig.speaker_of(*id),
                None,
                "{fixture} segment {id} was labelled despite the overlap gate"
            );
        }
    }
    assert!(
        rig.store.list_speakers().unwrap().is_empty(),
        "no voice may be minted from equal-loudness audio"
    );
}

#[test]
fn a_dominant_talker_is_still_labelled_through_one_interferer() {
    let mut rig = rig!("overlap-dominant");
    for fixture in ["duo_dominant_0.wav", "duo_dominant_1.wav"] {
        let ids = rig.ingest(fixture);
        assert!(!ids.is_empty(), "{fixture} produced no segments at all");
        for id in &ids {
            let frac = rig.overlap_of(*id);
            assert!(
                frac < 0.1,
                "{fixture} segment {id} read as {frac:.3} overlapped; a +12 dB \
                 dominant talker must stay under the gate"
            );
        }
        assert!(
            ids.iter().any(|id| rig.speaker_of(*id).is_some()),
            "{fixture} must be labelled, not refused"
        );
    }
}

#[test]
fn dense_dominant_babble_is_transcribed_even_when_the_gate_refuses_it() {
    // The manifest makes the *label* optional here: continuous synthetic babble
    // from 3-10 talkers is the overlap detector's worst case. A refused label
    // must still never cost the words, so every one of these segments has to go
    // through ASR — which is what the stamped model id proves, transcript or no
    // transcript.
    //
    // Whether words come back is a per-fixture question, because the words only
    // exist while the dominant voice is intelligible. `lobby_dominant_0` (ten
    // continuous talkers, +12 dB, 4.2 s) is where that stops: the English-only
    // export answered it with "I know this takes people from us." — 87% WER
    // against the reference, a sentence it made up — and the multilingual
    // default returns nothing at all. Silence is the better of those two
    // answers, and this project already refuses to trade ghost words for
    // coverage (see the non-speech test), so a blank is allowed here.
    let mut rig = rig!("overlap-dense");
    let model = rig.models.asr_model_id();
    for (fixture, words_required) in [
        ("trio_dominant_0.wav", true),
        ("trio_dominant_1.wav", true),
        ("lobby_dominant_0.wav", false),
        ("lobby_dominant_1.wav", true),
    ] {
        let ids = rig.ingest(fixture);
        assert!(!ids.is_empty(), "{fixture} produced no segments at all");
        for id in &ids {
            assert_eq!(
                rig.asr_model_of(*id).as_deref(),
                Some(model.as_str()),
                "{fixture} segment {id} never reached ASR"
            );
        }
        if words_required {
            assert!(
                !normalise_words(&rig.text_of(&ids)).is_empty(),
                "{fixture} produced no transcript"
            );
        }
    }
}

/// The short-segment padding rule, pinned.
///
/// Filling a sub-window segment out with silence shifts the model's per-chunk
/// normalisation and lifts quiet interferers into "second active speaker";
/// tiling it fixes that but fabricates periodic content and blinds the gate to
/// dense equal-loudness babble. Neither may creep back in.
#[test]
fn short_segments_keep_the_gates_discrimination() {
    let rig = rig!("overlap-short");
    let mut detector = recalld::overlap::OverlapDetector::load(&rig.models.segmentation).unwrap();

    let clean = read_wav(&fixtures_dir().join("clean_single_0.wav")).unwrap();
    for seconds in [1.2f32, 2.0, 3.0] {
        let n = (seconds * SAMPLE_RATE as f32) as usize;
        let frac = detector.overlap_frac(&clean[..n.min(clean.len())]).unwrap();
        assert!(
            frac < 0.1,
            "a {seconds} s slice of clean speech read as {frac:.3} overlapped"
        );
    }

    let duo = read_wav(&fixtures_dir().join("duo_equal_0.wav")).unwrap();
    let frac = detector.overlap_frac(&duo).unwrap();
    assert!(
        frac > 0.5,
        "duo_equal_0 read as {frac:.3}; the gate would let it through"
    );

    // Ten talkers at equal loudness is where tiling silently fails.
    let lobby = read_wav(&fixtures_dir().join("lobby_equal_0.wav")).unwrap();
    let frac = detector.overlap_frac(&lobby).unwrap();
    assert!(
        frac > 0.5,
        "lobby_equal_0 (2.9 s) read as {frac:.3}; short dense babble must still flag"
    );
}

// ---- 3. identity -------------------------------------------------------

#[test]
fn one_voice_keeps_its_id_and_two_voices_do_not_collide() {
    let mut rig = rig!("identity");
    let label_threshold = rig.cfg.identity.label_threshold;

    let first = rig.ingest("clean_single_0.wav");
    let second = rig.ingest("clean_single_0.wav");
    let a = rig
        .speaker_of(first[0])
        .expect("first pass must mint a voice");
    let b = rig
        .speaker_of(second[0])
        .expect("second pass must recognise it");
    assert_eq!(a, b, "the same voice was given two identities");
    let score = rig.score_of(second[0]).expect("a match carries its score");
    assert!(
        score >= label_threshold,
        "recognition scored {score:.3}, under the {label_threshold} label threshold"
    );

    // A different person, in 1.75 s and four words — under the mint bar
    // (0.6.1), so this much of a stranger is *unknown* rather than either
    // misattributed or turned into a permanent identity.
    let other = rig.ingest("clean_single_1.wav");
    assert!(
        other.iter().all(|id| rig.speaker_of(*id) != Some(a)),
        "a second person's speech was labelled as the first"
    );
    assert_eq!(
        rig.store.list_speakers().unwrap().len(),
        1,
        "a 1.75 s turn is under the mint bar and must not become a voice"
    );

    // Given enough of that same person, they do become one — and it is not the
    // first voice.
    let short = read_wav(&fixtures_dir().join("clean_single_1.wav")).unwrap();
    let doubled: Vec<f32> = short.iter().chain(short.iter()).copied().collect();
    let c = rig
        .ingest_samples(&doubled)
        .iter()
        .find_map(|id| rig.speaker_of(*id))
        .expect("3.5 s of one person must mint a voice");
    assert_ne!(a, c, "two different speakers collapsed onto one identity");

    // Both voices are visible, and the first has been heard twice.
    let speakers = rig.store.list_speakers().unwrap();
    assert_eq!(speakers.len(), 2);
    let first_voice = speakers.iter().find(|s| s.id == a).unwrap();
    assert_eq!(
        first_voice.segments,
        first.len() as i64 + second.len() as i64
    );
}

/// The mint bar, on real audio: a grunt does not become a person.
///
/// The failure it prevents is cumulative rather than dramatic. Every
/// half-second "hm" that mints leaves a permanent row in the voicebank that
/// nobody can name, and after a few lobbies the speakers view is mostly those.
/// A slice that short still gets *matched* if it sounds like somebody known —
/// the bar sits above the label bar, not instead of it.
#[test]
fn a_fragment_too_short_to_be_a_person_does_not_mint_one() {
    let mut rig = rig!("mint-bar");
    let clean = read_wav(&fixtures_dir().join("clean_single_0.wav")).unwrap();
    let session = rig.session;

    // 1.2 s: over the identity gate's 1.0 s minimum, under the 2.0 s mint bar.
    let short = &clean[..(1.2 * SAMPLE_RATE as f32) as usize];
    let seg = rig
        .store
        .insert_segment(session, 0, 1_200_000_000, "", 0)
        .unwrap();
    rig.analyzer.process(&rig.store, seg, short, 0).unwrap();
    assert_eq!(
        rig.speaker_of(seg),
        None,
        "a 1.2 s fragment minted a voice; the mint bar is not holding"
    );
    assert!(
        rig.store.list_speakers().unwrap().is_empty(),
        "nothing may reach the voicebank from below the bar"
    );
    // The evidence is kept even so: a later reassignment or split still has the
    // vector to work from.
    assert!(rig.store.segment_embedding(seg).unwrap().is_some());

    // The same voice at length mints, and then the fragment matches it — the
    // bar is about minting, and only about minting.
    let long = rig.ingest("clean_single_0.wav");
    let voice = long
        .iter()
        .find_map(|id| rig.speaker_of(*id))
        .expect("a whole file of clean speech mints a voice");
    let again = rig
        .store
        .insert_segment(session, 60_000_000_000, 61_200_000_000, "", 0)
        .unwrap();
    rig.analyzer.process(&rig.store, again, short, 0).unwrap();
    assert_eq!(
        rig.speaker_of(again),
        Some(voice),
        "the same fragment must still be recognised once the voice is known"
    );
}

// ---- 4. non-speech -----------------------------------------------------

#[test]
fn silence_and_noise_yield_no_words_and_no_voice() {
    let mut rig = rig!("non-speech");
    for fixture in ["silence.wav", "noise.wav"] {
        let ids = rig.ingest(fixture);
        let words = normalise_words(&rig.text_of(&ids));
        assert!(
            words.is_empty(),
            "{fixture} produced ghost words: {words:?}"
        );
        for id in &ids {
            assert_eq!(
                rig.speaker_of(*id),
                None,
                "{fixture} segment {id} was given a speaker"
            );
        }
    }
    assert!(
        rig.store.list_speakers().unwrap().is_empty(),
        "non-speech must not mint a voice"
    );
}

// ---- the search path, end to end ---------------------------------------

#[test]
fn transcripts_are_searchable_and_naming_reaches_back() {
    let mut rig = rig!("search");
    let ids = rig.ingest("clean_single_0.wav");
    let speaker = rig
        .speaker_of(ids[0])
        .expect("clean speech must be labelled");

    let hits = rig.store.search("violin", 10).unwrap();
    assert_eq!(hits.len(), 1, "the transcript was not indexed");
    assert!(hits[0].snippet.to_lowercase().contains("violin"));

    rig.store.rename_speaker(speaker, "Ines", 1).unwrap();
    assert_eq!(
        rig.store.search("violin", 10).unwrap()[0].speaker(),
        Some("Ines"),
        "naming must be retroactive"
    );
    assert_eq!(
        rig.store.transcript(None, Some(speaker)).unwrap().len(),
        ids.len()
    );
}

// ---- 5. per-speaker languages (0.6.1) ----------------------------------

/// The measured case, end to end on real audio.
///
/// `spike/lang_flip.py` found the multilingual export decoding German
/// fragments as English on 12% of 1 s windows and 5% of 2 s ones, against a
/// median real turn of 2.4 s. Telling the daemon that a voice speaks English
/// only is what makes the opposite direction fixable: a German-looking
/// transcript from that voice is decoded again with the English-only export,
/// whose language is a property of the model rather than a hint.
///
/// This drives the real function the pipeline calls, on real German audio the
/// real ASR transcribed — the one thing a text injection could not test, since
/// the correction re-runs the *audio*.
///
/// Needs **both** exports installed: the multilingual one to produce a German
/// transcript in the first place, and the English-only one to re-decode with.
#[test]
fn an_english_only_voice_gets_its_german_transcript_re_examined() {
    let mut rig = rig!("lang-redecode");
    if !rig.models.has_asr_export(&recalld::models::FALLBACK_ASR) || rig.models.asr_lang().is_some()
    {
        eprintln!(
            "skipping: this needs the multilingual export in use AND the English-only \
             export installed (`recalld models fetch --fallback-asr`)"
        );
        return;
    }

    // A German clip through the real pipeline, then the segment whose text
    // actually reads as German. If the export decoded none of them that way
    // there is nothing to correct and nothing to test.
    let mut german: Option<(i64, String)> = None;
    for fixture in [
        "de/de_short_0.wav",
        "de/de_short_1.wav",
        "de/de_short_2.wav",
    ] {
        for id in rig.ingest(fixture) {
            let Some(text) = rig.store.segment_fields(id).unwrap()["text"].clone() else {
                continue;
            };
            if recalld::lang::classify(&text) == recalld::lang::Lang::De {
                german = Some((id, text));
                break;
            }
        }
        if german.is_some() {
            break;
        }
    }
    let Some((seg, before)) = german else {
        panic!("the multilingual export read none of the German fixtures as German");
    };
    // Stamped by the classifier on the way in — that is the other half of this
    // feature and it has to be true before the correction is asked for.
    assert_eq!(
        rig.store.segment_fields(seg).unwrap()["lang"].as_deref(),
        Some("de")
    );

    // Now say this voice speaks English only, and ask the daemon to act on it.
    let speaker = rig.store.create_speaker("Ines", 0).unwrap();
    rig.store
        .set_speaker_languages(speaker, Some(&["en".to_string()]))
        .unwrap();
    rig.store
        .set_segment_speaker(seg, Some(speaker), Some(0.8))
        .unwrap();

    let rel = rig.store.segment_audio(seg).unwrap().unwrap().0;
    let samples = read_wav(&rig.dir.join(&rel)).unwrap();
    let fix = rig
        .analyzer
        .correct_language(&rig.store, seg, speaker, Some(&before), &samples)
        .unwrap()
        .expect("a German transcript from an English-only voice is a disagreement");

    let after = rig.store.segment_fields(seg).unwrap();
    match fix {
        // The English-only model produced English words: they win, and the row
        // says which model wrote them.
        recalld::analysis::LanguageFix::Redecoded {
            text,
            asr_model_id,
            lang,
        } => {
            eprintln!("  re-decoded: {before:?}\n          -> {text:?}");
            assert_eq!(lang, "en", "the constraint it was decoded under");
            assert_eq!(after["text"].as_deref(), Some(text.as_str()));
            assert_eq!(
                recalld::lang::classify(&text),
                recalld::lang::Lang::En,
                "only an English-reading re-decode may replace the text: {text:?}"
            );
            assert!(!normalise_words(&text).is_empty());
            assert_eq!(after["lang"].as_deref(), Some("en"));
            assert_eq!(after["lang_via"].as_deref(), Some("re-decode"));
            assert_eq!(
                after["asr_model_id"].as_deref(),
                Some(asr_model_id.as_str())
            );
            assert!(
                asr_model_id.contains(recalld::models::FALLBACK_ASR.dir),
                "the row must name the model that produced the words it holds: {asr_model_id}"
            );
        }
        // It did not, so nothing replaces the original. The words stay — they
        // are the only record of what was said — and the row is marked as a
        // disagreement nobody could settle.
        recalld::analysis::LanguageFix::Marked { read_as } => {
            eprintln!("  marked, not re-decoded: {before:?} still reads as {read_as}");
            assert_eq!(read_as, "de");
            assert_eq!(after["text"].as_deref(), Some(before.as_str()));
            assert_eq!(after["lang"], None, "a marked row claims no language");
            assert_eq!(after["lang_via"].as_deref(), Some("mismatch"));
        }
    }
    // Either way the identity is untouched: a voice does not become less
    // recognisable by having been decoded in the wrong language.
    assert_eq!(rig.speaker_of(seg), Some(speaker));
    assert_eq!(rig.score_of(seg), Some(0.8));
}

/// The other direction, which was *not* symmetric until 0.7.7 and now is.
///
/// Up to 0.7.6 there was no German-constrained decoder in the catalogue, so a
/// German voice's English-looking transcript could only be flagged. `ARBITER_DE`
/// closes that gap, and closing it means this direction now carries the same
/// honest hazard the English one always did: a hard constraint applied to a
/// wrong declaration produces confident nonsense. The declaration is
/// load-bearing, it is reversible, and that is why the default is `any`
/// (docs/PROTOCOL.md).
///
/// So the assertion here is not "nothing happens" — it is the invariant that
/// holds either way: whatever the arbiter says, the row ends up with words and
/// a provenance that names whoever wrote them, and the *speaker* is untouched.
#[test]
fn a_german_only_voice_with_an_english_transcript_is_settled_or_flagged() {
    let mut rig = rig!("lang-mark");
    let ids = rig.ingest("clean_single_0.wav");
    let seg = ids[0];
    let before = rig.store.segment_fields(seg).unwrap()["text"]
        .clone()
        .expect("clean English speech transcribes");
    assert_eq!(recalld::lang::classify(&before), recalld::lang::Lang::En);

    let speaker = rig.store.create_speaker("Jonas", 0).unwrap();
    rig.store
        .set_speaker_languages(speaker, Some(&["de".to_string()]))
        .unwrap();
    rig.store
        .set_segment_speaker(seg, Some(speaker), Some(0.7))
        .unwrap();

    let rel = rig.store.segment_audio(seg).unwrap().unwrap().0;
    let samples = read_wav(&rig.dir.join(&rel)).unwrap();
    let fix = rig
        .analyzer
        .correct_language(&rig.store, seg, speaker, Some(&before), &samples)
        .unwrap()
        .expect("an English transcript from a German-only voice is a disagreement");
    let after = rig.store.segment_fields(seg).unwrap();
    match fix {
        // The German arbiter is installed and produced German. Confident
        // nonsense over English audio, and correctly so: the declaration said
        // this voice speaks German and a hard constraint is a hard constraint.
        recalld::analysis::LanguageFix::Redecoded {
            text,
            asr_model_id,
            lang,
        } => {
            eprintln!("  re-decoded under the German constraint: {before:?}\n       -> {text:?}");
            assert_eq!(lang, "de");
            assert_eq!(asr_model_id, recalld::models::ARBITER_DE.model_id());
            assert_eq!(after["text"].as_deref(), Some(text.as_str()));
            assert_eq!(after["lang"].as_deref(), Some("de"));
            assert_eq!(after["lang_via"].as_deref(), Some("re-decode"));
            assert_eq!(recalld::lang::classify(&text), recalld::lang::Lang::De);
        }
        // No arbiter installed, or its answer failed a guard: the 0.6.1
        // behaviour, unchanged.
        recalld::analysis::LanguageFix::Marked { read_as } => {
            assert_eq!(read_as, "en");
            assert_eq!(after["text"].as_deref(), Some(before.as_str()));
            assert_eq!(after["lang"], None);
            assert_eq!(after["lang_via"].as_deref(), Some("mismatch"));
        }
    }
    assert_eq!(rig.speaker_of(seg), Some(speaker));

    // A voice that speaks both, or none in particular, is never corrected:
    // switching language is not a mistake.
    for languages in [Some(vec!["de".to_string(), "en".to_string()]), None] {
        rig.store
            .set_speaker_languages(speaker, languages.as_deref())
            .unwrap();
        assert_eq!(
            rig.analyzer
                .correct_language(&rig.store, seg, speaker, Some(&before), &samples)
                .unwrap(),
            None
        );
    }
}

// ---- 6. the conversational language prior (0.7.7) ----------------------

/// The German flip, end to end on real German audio.
///
/// The measurement this exists for (`spike/lang_flip.py`): the multilingual
/// export decodes 12% of 1 s German fragments and 5% of 2 s ones **as
/// English**, and what comes back is 103%-WER nonsense. 0.6.1 could only notice
/// that when somebody had declared the speaker's language. 0.7.7 notices it
/// from the conversation — and, now that there is a German-constrained decoder
/// in the catalogue, can do something about it.
///
/// The flip itself is injected rather than waited for: at 3.5 s the export gets
/// these fixtures right, and a test that needed a 12% event to fire would be a
/// test that fails 88% of the time. What is NOT injected is the part under
/// test — the audio is real German, the context is read out of real
/// transcripts, and the words that replace the flip come out of the real
/// arbiter reading the real WAV.
///
/// Needs the German arbiter under `NXR_MODELS`
/// (`recalld models fetch --arbiter-de`).
#[test]
fn a_german_turn_flipped_to_english_is_re_read_by_the_arbiter() {
    let mut rig = rig!("lang-arbiter");
    if !rig.arbiter_installed() {
        eprintln!(
            "skipping: needs the German arbiter under NXR_MODELS \
             (`recalld models fetch --arbiter-de`)"
        );
        return;
    }
    let Some(fixture) = rig.a_fixture_read_as_german() else {
        panic!("the multilingual export read none of the German fixtures as German");
    };

    // A German conversation: four turns of real German audio, each stamped by
    // the real classifier on the way in. That is the context.
    let thread = rig.new_thread();
    for _ in 0..4 {
        for id in rig.ingest(&fixture) {
            rig.put_in_thread(id, thread);
        }
    }
    assert!(
        rig.german_stamps_in(thread) >= 3,
        "the context bar is three agreeing turns and this thread has {}",
        rig.german_stamps_in(thread)
    );

    // …and one more turn of the same German audio, whose transcript came back
    // English. This is the flip.
    let subject = *rig.ingest(&fixture).first().expect("a segment");
    rig.put_in_thread(subject, thread);
    let samples = rig.samples_of(subject);
    let flipped = "i think that is the only way to do it";
    rig.flip_to(subject, flipped);

    let fix = rig
        .analyzer
        .apply_language_context(&rig.store, subject, &samples)
        .unwrap()
        .expect("an English turn in a German conversation is a suspected flip");

    let after = rig.store.segment_fields(subject).unwrap();
    match fix {
        recalld::langctx::ContextFix::Redecoded {
            text,
            lang,
            asr_model_id,
        } => {
            eprintln!("  flipped: {flipped:?}\n       -> {text:?}");
            assert_eq!(lang, "de");
            assert_eq!(after["text"].as_deref(), Some(text.as_str()));
            assert_eq!(after["lang"].as_deref(), Some("de"));
            assert_eq!(after["lang_via"].as_deref(), Some("re-decode"));
            // The row must name the model that produced the words it holds —
            // and the forced language is part of that model's contract.
            assert_eq!(
                after["asr_model_id"].as_deref(),
                Some(asr_model_id.as_str())
            );
            assert_eq!(asr_model_id, recalld::models::ARBITER_DE.model_id());
            // Every guard the replacement had to clear, checked on the result
            // rather than trusted from the code path.
            assert_eq!(recalld::lang::classify(&text), recalld::lang::Lang::De);
            assert!(normalise_words(&text).len() >= 2, "{text:?}");
            assert_eq!(
                recalld::arbiter::strip_captions(&text),
                text,
                "a caption-shaped answer must never have got this far: {text:?}"
            );
        }
        // The arbiter is allowed to disagree — it is an arbiter, not an oracle.
        // What it is not allowed to do is overwrite good words with bad ones,
        // so a disagreement leaves the flip standing and flags the row.
        other => {
            eprintln!("  the arbiter did not confirm the flip: {other:?}");
            assert_eq!(after["text"].as_deref(), Some(flipped));
            assert_eq!(after["lang"], None);
            assert_eq!(after["lang_via"].as_deref(), Some("mismatch"));
        }
    }
}

/// The two guards that decide when the arbiter is *not* allowed to win.
///
/// Both are measured, and both fail in the same direction — keep the words —
/// because the alternative is replacing one wrong transcript with a differently
/// wrong one:
///
/// * **Under 1.5 s** the arbiter's word precision is 28% (54% at 1.5 s, 66% at
///   3 s, `spike/arbiter_de.py`). It is not even run.
/// * **On non-speech** Whisper narrates: `(soft music)`, `[Applause]`. Stripped
///   before the classifier sees it, that is an empty answer, and an empty
///   answer replaces nothing.
#[test]
fn the_arbiter_may_not_overwrite_words_it_cannot_beat() {
    let mut rig = rig!("lang-guards");
    if !rig.arbiter_installed() {
        eprintln!("skipping: needs the German arbiter under NXR_MODELS");
        return;
    }
    let Some(fixture) = rig.a_fixture_read_as_german() else {
        panic!("the multilingual export read none of the German fixtures as German");
    };
    let flipped = "i think that is the only way to do it";

    let thread = rig.new_thread();
    for _ in 0..4 {
        for id in rig.ingest(&fixture) {
            rig.put_in_thread(id, thread);
        }
    }
    assert!(rig.german_stamps_in(thread) >= 3);

    // -- one second of the same German audio ---------------------------------
    let short = rig.ingest(&fixture)[0];
    rig.put_in_thread(short, thread);
    let one_second: Vec<f32> = rig
        .samples_of(short)
        .into_iter()
        .take(SAMPLE_RATE as usize)
        .collect();
    rig.flip_to(short, flipped);
    let fix = rig
        .analyzer
        .apply_language_context(&rig.store, short, &one_second)
        .unwrap();
    assert_eq!(
        fix,
        Some(recalld::langctx::ContextFix::Marked { read_as: "en" }),
        "at 1.0 s the arbiter's own words are right 28% of the time: flag, never replace"
    );
    let after = rig.store.segment_fields(short).unwrap();
    assert_eq!(after["text"].as_deref(), Some(flipped), "the words stand");
    assert_eq!(after["lang"], None, "…and the language is openly in doubt");
    assert_eq!(after["lang_via"].as_deref(), Some("mismatch"));

    // -- an arbiter answer that is not worth the words it would replace ------
    // Whisper's measured habit on non-speech is to narrate: "(soft music)",
    // "[Applaus]". Every such answer is stripped to nothing and rejected, and
    // the rule that does it (`arbiter::judge`) is exercised directly in its own
    // module — a caption cannot be *produced* on demand from real audio, and a
    // test that waited for one would be a test that usually proves nothing.
    //
    // What is asserted here is the property that matters at this level: a row
    // the arbiter did not settle keeps its words. Both branches above already
    // hold that line, and so does the guard's own table.
    assert!(
        recalld::arbiter::judge("(Musik)", recalld::lang::Lang::De, &rig.cfg.lang).is_err(),
        "a narrated caption never becomes a transcript"
    );
}

/// The backlog walk, on the same real audio and through the same guards.
///
/// This is what makes the arbiter worth installing on a machine that has been
/// running since 0.6.1: every flip flagged in that time still has its WAV, and
/// what could not be settled then can be settled now.
#[test]
fn the_repair_walk_settles_a_flagged_row_from_its_audio() {
    let mut rig = rig!("lang-repair");
    if !rig.arbiter_installed() {
        eprintln!("skipping: needs the German arbiter under NXR_MODELS");
        return;
    }
    let Some(fixture) = rig.a_fixture_read_as_german() else {
        panic!("the multilingual export read none of the German fixtures as German");
    };

    let thread = rig.new_thread();
    for _ in 0..4 {
        for id in rig.ingest(&fixture) {
            rig.put_in_thread(id, thread);
        }
    }
    // A row flagged exactly as 0.6.1 would have flagged it: English-looking
    // words over German audio, language NULL, words kept.
    let flagged = rig.ingest(&fixture)[0];
    rig.put_in_thread(flagged, thread);
    rig.flip_to(flagged, "i think that is the only way to do it");
    rig.store.mark_segment_language_mismatch(flagged).unwrap();

    // …and one whose audio the retention window has taken, which no repair can
    // ever settle and which must not stop the walk.
    let gone = rig.ingest(&fixture)[0];
    rig.put_in_thread(gone, thread);
    rig.flip_to(gone, "i think that is the only way to do it");
    rig.store.mark_segment_language_mismatch(gone).unwrap();
    rig.store.forget_audio(&[gone]).unwrap();

    assert_eq!(rig.store.language_mismatch_counts().unwrap(), (2, 1));

    let mut arbiters = recalld::arbiter::Arbiters::new(&rig.models);
    let report = recalld::langctx::repair(
        &rig.store,
        &rig.dir,
        &mut arbiters,
        &rig.cfg.lang,
        8,
        None,
        |_| {},
    )
    .unwrap();

    eprintln!("  repair: {report:?}");
    assert_eq!(
        report.scanned, 1,
        "a row with no audio left is never even offered to the walk"
    );
    assert_eq!(report.no_audio, 0);
    let after = rig.store.segment_fields(flagged).unwrap();
    if report.repaired == 1 {
        assert_eq!(report.repaired_de, 1);
        assert_eq!(report.changed, vec![flagged]);
        assert_eq!(after["lang"].as_deref(), Some("de"));
        assert_eq!(after["lang_via"].as_deref(), Some("re-decode"));
        assert_eq!(
            after["asr_model_id"].as_deref(),
            Some(recalld::models::ARBITER_DE.model_id().as_str())
        );
        assert_eq!(
            recalld::lang::classify(after["text"].as_deref().unwrap()),
            recalld::lang::Lang::De
        );
    } else {
        // The arbiter disagreed. The row stays exactly as it was, which is the
        // point: a repair that could damage a transcript would be worse than no
        // repair at all.
        assert_eq!(report.kept, 1);
        assert_eq!(after["lang_via"].as_deref(), Some("mismatch"));
        assert_eq!(
            after["text"].as_deref(),
            Some("i think that is the only way to do it")
        );
    }
    // The row with no audio is still flagged and still honest about it.
    assert_eq!(
        rig.store.segment_fields(gone).unwrap()["lang_via"].as_deref(),
        Some("mismatch")
    );
}

// ---- pure helpers ------------------------------------------------------

#[test]
fn the_wer_helper_is_case_and_punctuation_insensitive() {
    assert_eq!(wer("HELLO WORLD", "Hello, world."), 0.0);
    // One substitution out of three words.
    assert!((wer("A B C", "a x c") - 1.0 / 3.0).abs() < 1e-6);
    // One insertion.
    assert!((wer("A B", "a b c") - 0.5).abs() < 1e-6);
    assert_eq!(wer("", ""), 0.0);
    assert_eq!(wer("", "ghost"), 1.0);
}

#[test]
fn the_fixture_manifest_is_present_and_shaped_as_expected() {
    let m = manifest();
    assert_eq!(m["sample_rate"], SAMPLE_RATE);
    let files = m["fixtures"].as_array().unwrap();
    assert!(files.len() >= 16);
    for f in files {
        let name = f["file"].as_str().unwrap();
        assert!(
            fixtures_dir().join(name).exists(),
            "{name} is in the manifest but not on disk"
        );
    }
}
