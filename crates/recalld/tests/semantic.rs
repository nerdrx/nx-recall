//! Semantic search against the real model.
//!
//! Gated on `NXR_MODELS=<dir>`, like the acceptance suite: the text embedding
//! model is 118 MB and is not in the repository, so without the variable every
//! test here reports itself skipped and passes. What is *not* gated lives in
//! `src/semantic.rs`'s unit tests — fusion, ranking, the migration — because
//! those must run on every machine.
//!
//! Everything here runs against a throwaway data directory. The daemon's live
//! database is never opened.

use std::path::PathBuf;

use recalld::config::{Config, ModelsConfig};
use recalld::models::SemanticModel;
use recalld::semantic::{self, BACKFILL_BATCH, Candidates, RRF_K, SemanticLeg, TextEmbedder, Via};
use recalld::store::{SegmentAnalysis, SegmentFilter, Store};

fn models_dir() -> Option<PathBuf> {
    let raw = std::env::var("NXR_MODELS").ok()?;
    if raw.trim().is_empty() {
        return None;
    }
    Some(PathBuf::from(raw))
}

fn semantic_model() -> Option<SemanticModel> {
    let sem = SemanticModel::resolve_at(models_dir()?, &ModelsConfig::default());
    if !sem.present() {
        eprintln!(
            "SKIP: {} is not installed under {}",
            sem.model_id(),
            sem.root.display()
        );
        return None;
    }
    Some(sem)
}

macro_rules! model_or_skip {
    () => {
        match semantic_model() {
            Some(m) => m,
            None => {
                eprintln!("SKIP: set NXR_MODELS to a directory holding the semantic model");
                return;
            }
        }
    };
}

/// A small bilingual corpus in the shape real turns arrive in: short, spoken,
/// and half of it in the other language. Every pair says the same thing twice.
const CORPUS: &[(&str, &str)] = &[
    (
        "en",
        "That world with the giant floating whales was called Cetacea, I think.",
    ),
    (
        "de",
        "Die Welt mit den riesigen schwebenden Walen hieß glaube ich Cetacea.",
    ),
    (
        "en",
        "I finally got my avatar's physbones working after three hours of fighting Unity.",
    ),
    (
        "de",
        "Ich hab endlich die Physbones an meinem Avatar zum Laufen gebracht, nach drei Stunden Unity.",
    ),
    (
        "en",
        "She said her cat knocked the coffee over the keyboard right before the meeting.",
    ),
    (
        "de",
        "Sie meinte ihre Katze hat den Kaffee über die Tastatur gekippt kurz vor dem Meeting.",
    ),
    (
        "en",
        "I really need to see a dentist about this tooth, it's been aching for a week.",
    ),
    (
        "de",
        "Ich muss echt mal zum Zahnarzt wegen dem Zahn, der tut seit einer Woche weh.",
    ),
    (
        "en",
        "We got completely lost in that maze world and ended up out of bounds.",
    ),
    (
        "de",
        "Wir haben uns in der Labyrinth-Welt total verlaufen und sind außerhalb gelandet.",
    ),
    (
        "en",
        "My graphics card fans sound like a jet engine when I load that world.",
    ),
    (
        "de",
        "Die Lüfter von meiner Grafikkarte klingen wie ein Düsentriebwerk in der Welt.",
    ),
    (
        "en",
        "The train was delayed by forty minutes and nobody announced anything.",
    ),
    (
        "de",
        "Der Zug hatte vierzig Minuten Verspätung und niemand hat irgendwas durchgesagt.",
    ),
    (
        "en",
        "Someone's fridge is humming and it's coming through your mic.",
    ),
    (
        "de",
        "Irgendwo brummt ein Kühlschrank und man hört es über dein Mikro.",
    ),
    ("en", "Yeah."),
    ("de", "Ja genau."),
];

/// Filler, so the index is the size a real one is.
///
/// Not padding for its own sake. The language-bias correction
/// (`semantic::Whitening`) is estimated from the corpus and is only estimated
/// at all past `MIN_WHITENING_ROWS`, so a fixture of eighteen sentences would
/// test a code path no user is ever on. These are also *distractors*: every one
/// of them is a plausible thing to say in a VRChat lobby, in both languages, so
/// a probe that lands on the right turn has beaten several hundred wrong ones.
const FILLER_EN: &[&str] = &[
    "I was up way too late again last night",
    "that shader looks completely different in this world",
    "give me a second, I need to restart the game",
    "the queue for that instance was ages",
    "no I meant the other one, the blue one",
    "did you see what they did with the lighting",
    "my controller battery is about to die",
    "we should probably head over before it fills up",
    "honestly I have no idea what that button does",
    "I keep falling through the floor over there",
    "someone said the update lands on Thursday",
    "that's the third time it has crashed today",
    "the music in here is way too loud for me",
    "I will be back in five, making tea",
    "you sound a bit robotic right now",
    "hold on, my cat is on the desk again",
    "the frame rate tanks whenever people arrive",
    "I finally cleaned up my avatar list",
    "that costs more than I want to spend",
    "let me check whether I still have that saved",
    "it was raining the whole weekend here",
    "I have to be up early tomorrow unfortunately",
    "did anyone else get disconnected just now",
    "the new menu is genuinely worse than the old one",
    "we tried that last week and it did not work",
];
const FILLER_DE: &[&str] = &[
    "ich war gestern wieder viel zu lange wach",
    "der Shader sieht in dieser Welt komplett anders aus",
    "warte kurz, ich muss das Spiel neu starten",
    "die Warteschlange für die Instanz war ewig",
    "nein ich meinte die andere, die blaue",
    "hast du gesehen was die mit dem Licht gemacht haben",
    "der Akku von meinem Controller ist gleich leer",
    "wir sollten rübergehen bevor es voll wird",
    "ehrlich gesagt weiß ich nicht was der Knopf macht",
    "ich falle da drüben ständig durch den Boden",
    "jemand meinte das Update kommt am Donnerstag",
    "das ist heute schon das dritte Mal abgestürzt",
    "die Musik hier drin ist mir viel zu laut",
    "bin in fünf Minuten zurück, mache Tee",
    "du klingst gerade ein bisschen roboterhaft",
    "moment, meine Katze sitzt wieder auf dem Schreibtisch",
    "die Bildrate bricht ein sobald Leute reinkommen",
    "ich hab endlich meine Avatarliste aufgeräumt",
    "das kostet mehr als ich ausgeben will",
    "lass mich schauen ob ich das noch gespeichert habe",
    "hier hat es das ganze Wochenende geregnet",
    "ich muss morgen leider früh raus",
    "wurde sonst noch jemand gerade rausgeworfen",
    "das neue Menü ist wirklich schlechter als das alte",
    "das haben wir letzte Woche probiert und es ging nicht",
];
const WHEN: &[&str] = &[
    "yesterday",
    "this morning",
    "last night",
    "on Tuesday",
    "earlier",
    "gestern",
    "heute früh",
    "gestern Abend",
    "am Dienstag",
    "vorhin",
];

/// ~520 turns: the probe corpus plus deterministic filler.
fn corpus() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = CORPUS
        .iter()
        .map(|(l, t)| ((*l).to_string(), (*t).to_string()))
        .collect();
    for (i, when) in WHEN.iter().enumerate() {
        let de = i >= WHEN.len() / 2;
        let bank = if de { FILLER_DE } else { FILLER_EN };
        for line in bank {
            out.push((
                if de { "de".into() } else { "en".into() },
                format!("{line}, {when}."),
            ));
            out.push((
                if de { "de".into() } else { "en".into() },
                format!("{when}, {line}."),
            ));
        }
    }
    out
}

struct Fixture {
    store: Store,
    corpus: Vec<(String, String)>,
    /// Row order matches `corpus`, so a text can be turned into a segment id.
    ids: Vec<i64>,
}

impl Fixture {
    fn build() -> Self {
        let store = Store::open_in_memory().unwrap();
        let source = store.upsert_source("VRChat.exe", "VRChat", 1_000).unwrap();
        let session = store.begin_session(source, 1_000).unwrap();
        let corpus = corpus();
        assert!(
            corpus.len() > recalld::semantic::MIN_WHITENING_ROWS,
            "the fixture must be big enough to exercise the language-bias correction"
        );
        let mut ids = Vec::new();
        for (i, (lang, text)) in corpus.iter().enumerate() {
            let t = 1_000 + i as i64 * 5_000_000_000;
            let id = store
                .insert_segment(session, t, t + 2_000_000_000, &format!("seg{i}.wav"), t)
                .unwrap();
            store
                .set_segment_analysis(
                    id,
                    &SegmentAnalysis {
                        text: Some(text.clone()),
                        lang: Some(lang.clone()),
                        lang_via: Some("model".into()),
                        asr_model_id: Some("test@1".into()),
                        overlap_frac: Some(0.0),
                    },
                )
                .unwrap();
            ids.push(id);
        }
        Self { store, corpus, ids }
    }

    fn id_of(&self, text: &str) -> i64 {
        let at = self
            .corpus
            .iter()
            .position(|(_, t)| t == text)
            .unwrap_or_else(|| panic!("{text:?} is not in the fixture corpus"));
        self.ids[at]
    }

    fn text_of(&self, id: i64) -> String {
        let at = self.ids.iter().position(|x| *x == id).unwrap();
        self.corpus[at].1.clone()
    }

    /// Index the whole corpus, the way `recalld semantic backfill` does.
    fn indexed(self, embedder: &mut TextEmbedder) -> Self {
        let report =
            semantic::backfill(&self.store, embedder, BACKFILL_BATCH, None, |_, _| {}).unwrap();
        assert_eq!(report.embedded, self.corpus.len());
        self
    }
}

fn embedder(sem: &SemanticModel) -> TextEmbedder {
    TextEmbedder::load(sem).expect("the catalogued model must load")
}

/// The point of the whole feature: a German query finding an English turn that
/// shares no word with it, and the other way round.
#[test]
fn a_german_query_finds_the_english_turn_and_the_other_way_round() {
    let sem = model_or_skip!();
    let mut e = embedder(&sem);
    let f = Fixture::build().indexed(&mut e);
    let leg = SemanticLeg::new(embedder(&sem));

    // The English twin is removed from the index first, so the German query
    // cannot win by finding its own paraphrase. Anything less than that is not
    // a cross-language test, it is a same-language test with a witness.
    let cases: &[(&str, &str, &str)] = &[
        (
            "Zahnschmerzen, ich sollte mal zum Arzt",
            "I really need to see a dentist about this tooth, it's been aching for a week.",
            "Ich muss echt mal zum Zahnarzt wegen dem Zahn, der tut seit einer Woche weh.",
        ),
        (
            "die Welt mit den Walen",
            "That world with the giant floating whales was called Cetacea, I think.",
            "Die Welt mit den riesigen schwebenden Walen hieß glaube ich Cetacea.",
        ),
        (
            "her cat spilled a drink on the keyboard",
            "Sie meinte ihre Katze hat den Kaffee über die Tastatur gekippt kurz vor dem Meeting.",
            "She said her cat knocked the coffee over the keyboard right before the meeting.",
        ),
        (
            "a background hum coming from a fridge",
            "Irgendwo brummt ein Kühlschrank und man hört es über dein Mikro.",
            "Someone's fridge is humming and it's coming through your mic.",
        ),
    ];

    for (query, want, hide) in cases {
        let mut allowed: std::collections::HashSet<i64> = f.ids.iter().copied().collect();
        allowed.remove(&f.id_of(hide));
        let hits = leg
            .search(&f.store, query, 3, &Candidates::Only(allowed))
            .unwrap();
        let top = f.text_of(hits[0].segment_id);
        assert_eq!(
            &top.as_str(),
            want,
            "\n  query   {query:?}\n  wanted  {want:?}\n  got     {top:?} at {:.3}\n  \
             the other language is the whole feature",
            hits[0].score
        );
        // ...and it is not a coin toss: the winner is clear of the runner-up.
        assert!(
            hits[0].score > hits[1].score,
            "{query:?}: {:.3} vs {:.3}",
            hits[0].score,
            hits[1].score
        );
    }
}

/// The same words twice must be the same vector twice — a search that
/// reshuffles between runs is a search nobody trusts, and a vector that drifts
/// makes the `text_hash` staleness check meaningless.
#[test]
fn embedding_is_deterministic_and_normalised() {
    let sem = model_or_skip!();
    let mut e = embedder(&sem);
    let a = e.embed_passage("Die Welt mit den Walen").unwrap();
    let b = e.embed_passage("Die Welt mit den Walen").unwrap();
    assert_eq!(a.vector, b.vector, "the same text must embed identically");
    assert_eq!(a.dim(), semantic::DIM);

    let norm: f32 = a.vector.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!(
        (norm - 1.0).abs() < 1e-4,
        "vectors are stored normalised so the scan is a dot product; got {norm}"
    );
    assert!((a.cosine(&b).unwrap() - 1.0).abs() < 1e-5);

    // A fresh session must agree with the first one, or a restart would
    // invalidate the index.
    let mut e2 = embedder(&sem);
    let c = e2.embed_passage("Die Welt mit den Walen").unwrap();
    assert_eq!(a.vector, c.vector);
}

/// e5's `query:`/`passage:` prefixes are load-bearing, and the failure mode is
/// silent. This pins that the two sides really are being embedded differently.
#[test]
fn the_two_prefixes_produce_different_vectors() {
    let sem = model_or_skip!();
    let mut e = embedder(&sem);
    let q = e.embed_query("die Welt mit den Walen").unwrap();
    let p = e.embed_passage("die Welt mit den Walen").unwrap();
    let cos = q.cosine(&p).unwrap();
    assert!(
        cos < 0.999,
        "query: and passage: must not be the same forward pass (cos {cos:.5}) — \
         if they are, the prefixes are not reaching the tokenizer"
    );
    assert!(
        cos > 0.8,
        "...but they are still the same sentence (cos {cos:.5})"
    );
}

/// Hybrid mode has to be strictly better than either leg alone at the thing
/// each leg is bad at: keyword misses the paraphrase, the vector leg has no
/// special affection for an exact rare word.
#[test]
fn hybrid_finds_what_each_leg_alone_misses() {
    let sem = model_or_skip!();
    let mut e = embedder(&sem);
    let f = Fixture::build().indexed(&mut e);
    let leg = SemanticLeg::new(embedder(&sem));
    let filter = SegmentFilter::default();
    let within = semantic::candidates(&f.store, &filter).unwrap();

    // "Verspätung" is in exactly one turn, verbatim. FTS nails it.
    let kw: Vec<i64> = f
        .store
        .search("Verspätung", 10)
        .unwrap()
        .iter()
        .map(|h| h.segment_id())
        .collect();
    assert_eq!(kw.len(), 1);

    // The same idea in English shares no word with either turn, so FTS finds
    // nothing at all — and the vector leg finds both.
    let none: Vec<i64> = f
        .store
        .search("running late", 10)
        .unwrap()
        .iter()
        .map(|h| h.segment_id())
        .collect();
    assert!(none.is_empty(), "the keyword leg should have no idea");
    let vec_ids: Vec<i64> = leg
        .search(&f.store, "the train was running late", 3, &within)
        .unwrap()
        .iter()
        .map(|s| s.segment_id)
        .collect();
    assert!(
        vec_ids.contains(&f.id_of(
            "Der Zug hatte vierzig Minuten Verspätung und niemand hat irgendwas durchgesagt."
        )),
        "the vector leg should find the German one"
    );

    // Fused: the row both legs found is first, and it says so.
    let fused = semantic::fuse(&kw, &vec_ids, RRF_K);
    let both: Vec<&semantic::Fused> = fused.iter().filter(|x| x.via == Via::Both).collect();
    assert_eq!(both.len(), 1);
    assert_eq!(fused[0].via, Via::Both);
    assert_eq!(fused[0].segment_id, kw[0]);
}

/// A backfill that is killed halfway must finish the job on the next run, and
/// a completed one must have nothing left to do.
#[test]
fn the_backfill_resumes_and_then_has_nothing_left() {
    let sem = model_or_skip!();
    let mut e = embedder(&sem);
    let f = Fixture::build();
    let model_id = sem.model_id();

    let before = semantic::coverage(&f.store, &model_id).unwrap();
    assert_eq!(before.eligible, f.corpus.len() as i64);
    assert_eq!(before.embedded, 0);

    // Interrupted after four.
    let first = semantic::backfill(&f.store, &mut e, 2, Some(4), |_, _| {}).unwrap();
    assert_eq!(first.embedded, 4);
    assert_eq!(first.batches, 2);
    let mid = semantic::coverage(&f.store, &model_id).unwrap();
    assert_eq!(mid.embedded, 4);
    assert_eq!(mid.pending(), f.corpus.len() as i64 - 4);

    // Run again: it picks up exactly the rest, not the whole corpus again.
    let mut batches_seen = 0;
    let second = semantic::backfill(&f.store, &mut e, 8, None, |_, _| batches_seen += 1).unwrap();
    assert_eq!(second.embedded, f.corpus.len() - 4);
    assert!(batches_seen >= 2);
    let done = semantic::coverage(&f.store, &model_id).unwrap();
    assert!(done.complete(), "{done:?}");

    // A third run does nothing at all.
    let third = semantic::backfill(&f.store, &mut e, 8, None, |_, _| {}).unwrap();
    assert_eq!(third.embedded, 0);
    assert_eq!(third.batches, 0);
}

/// A corrected transcript is a stale vector, and the backfill has to notice
/// without being told which row changed.
#[test]
fn correcting_a_transcript_makes_its_vector_stale() {
    let sem = model_or_skip!();
    let mut e = embedder(&sem);
    let f = Fixture::build().indexed(&mut e);
    let model_id = sem.model_id();
    assert!(semantic::coverage(&f.store, &model_id).unwrap().complete());

    let id = f.id_of("Yeah.");
    f.store
        .correct_segment_text(id, "Yeah, the pretzels at that bakery by the station.")
        .unwrap();

    let pending = semantic::pending_segments(&f.store, &model_id, 10).unwrap();
    assert_eq!(
        pending.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        vec![id],
        "exactly the corrected row, found by hash rather than by being told"
    );

    let again = semantic::backfill(&f.store, &mut e, 8, None, |_, _| {}).unwrap();
    assert_eq!(again.embedded, 1);
    // ...and the new words are now findable by meaning.
    let leg = SemanticLeg::new(embedder(&sem));
    let hits = leg
        .search(
            &f.store,
            "Brezeln von der Bäckerei",
            1,
            &Candidates::everything(),
        )
        .unwrap();
    assert_eq!(hits[0].segment_id, id);
}

/// A model that is not installed is not an error state. Everything keeps
/// working and the daemon says exactly how to get it.
#[test]
fn an_absent_model_is_a_state_not_a_failure() {
    let dir = std::env::temp_dir().join(format!("nxr-sem-absent-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let sem = SemanticModel::resolve_at(dir.clone(), &ModelsConfig::default());
    assert!(!sem.present());
    let how = SemanticModel::how_to_get_it();
    assert!(how.contains("models fetch --semantic"), "{how}");
    assert!(how.contains("semantic backfill"), "{how}");

    // Half an install is absent, not broken: a truncated model must not be
    // loaded and must not be reported as ready.
    std::fs::create_dir_all(sem.model.parent().unwrap()).unwrap();
    std::fs::write(&sem.model, b"not a model").unwrap();
    assert!(!sem.present());

    // ...and the store still opens, still migrates, and still searches.
    let store = Store::open(&dir).unwrap();
    let source = store.upsert_source("VRChat.exe", "VRChat", 1).unwrap();
    let session = store.begin_session(source, 1).unwrap();
    let id = store.insert_segment(session, 1, 2, "a.wav", 1).unwrap();
    store.correct_segment_text(id, "portal world").unwrap();
    assert_eq!(store.search("portal", 10).unwrap().len(), 1);
    assert_eq!(
        semantic::coverage(&store, &sem.model_id())
            .unwrap()
            .embedded,
        0
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The socket method, with a real model behind it.
///
/// The service's *absent* path is covered by its own unit tests; this is the
/// present one, which is where the JSON shaping lives — `via`, `rrf`, and the
/// rule that a keyword-only hit carries no invented cosine.
#[test]
fn the_socket_method_answers_in_both_modes() {
    use recalld::allowlist::Allowlist;
    use recalld::bus::{Bus, Topic};
    use recalld::control::Control;
    use recalld::proto::Incoming;
    use recalld::service::Service;
    use std::sync::{Arc, Mutex};

    let sem = model_or_skip!();
    let dir = std::env::temp_dir().join(format!("nxr-sem-svc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // A file-backed store, because that is what the daemon has.
    let store = Store::open(&dir).unwrap();
    let source = store.upsert_source("VRChat.exe", "VRChat", 1).unwrap();
    let session = store.begin_session(source, 1).unwrap();
    let mut ids = Vec::new();
    for (i, (_, text)) in CORPUS.iter().enumerate() {
        let t = 1_000 + i as i64 * 5_000_000_000;
        let id = store
            .insert_segment(session, t, t + 2_000_000_000, &format!("s{i}.wav"), t)
            .unwrap();
        store.correct_segment_text(id, text).unwrap();
        ids.push(id);
    }

    let control = Control::new(dir.clone(), None, &Allowlist::from_rules([("x", false)]));
    let bus = Bus::new(64, 32);
    let service = Service::new(Arc::new(Mutex::new(store)), control, Arc::clone(&bus));
    let (client, _rx) = bus.attach(None);
    client.subscribe(&Topic::ALL);

    let mut e = embedder(&sem);
    {
        let guard = service.store.lock().unwrap();
        semantic::backfill(&guard, &mut e, BACKFILL_BATCH, None, |_, _| {}).unwrap();
    }
    service.attach_semantic(Arc::new(SemanticLeg::new(embedder(&sem))));

    let call = |line: &str| {
        let Incoming::Request(req) = recalld::proto::parse(line) else {
            panic!("not a request: {line}");
        };
        service.handle(&client, &req)
    };

    // `status` now says the leg is on, and how much of the transcript it has.
    let st = call(r#"{"id":1,"method":"status"}"#).unwrap();
    assert_eq!(st["semantic"]["available"], true);
    assert_eq!(st["semantic"]["dim"], 384);
    assert_eq!(st["semantic"]["pending"], 0);
    assert_eq!(st["semantic"]["indexed"], CORPUS.len() as i64);

    // Pure semantic: an English query, and the German turn is in the answer.
    let res = call(
        r#"{"id":2,"method":"search.semantic","params":{"q":"her cat spilled a drink on the keyboard","limit":3}}"#,
    )
    .unwrap();
    assert_eq!(res["mode"], "semantic");
    assert_eq!(res["model"], sem.model_id());
    let hits = res["hits"].as_array().unwrap();
    assert!(!hits.is_empty());
    let texts: Vec<&str> = hits.iter().map(|h| h["text"].as_str().unwrap()).collect();
    assert!(
        texts
            .iter()
            .any(|t| t.contains("Katze") && t.contains("Tastatur")),
        "the German turn should be here: {texts:?}"
    );
    for hit in hits {
        assert_eq!(hit["via"], "semantic");
        assert!(hit["score"].as_f64().unwrap() > 0.0);
    }

    // Hybrid: the keyword leg finds the German turn verbatim, the vector leg
    // finds both, and the row they agree on leads and says so.
    let res = call(
        r#"{"id":3,"method":"search.semantic","params":{"q":"Verspätung","mode":"hybrid","limit":5}}"#,
    )
    .unwrap();
    assert_eq!(res["mode"], "hybrid");
    let hits = res["hits"].as_array().unwrap();
    assert_eq!(hits[0]["via"], "both", "{hits:#?}");
    assert!(
        hits[0]["text"].as_str().unwrap().contains("Verspätung"),
        "{hits:#?}"
    );
    for hit in hits {
        // A keyword-only hit has no cosine and none is invented for it.
        if hit["via"] == "keyword" {
            assert!(hit["score"].is_null(), "{hit:#?}");
        }
        assert!(hit["rrf"].as_f64().unwrap() > 0.0);
    }
    // Ordered by the fusion score, descending.
    let scores: Vec<f64> = hits.iter().map(|h| h["rrf"].as_f64().unwrap()).collect();
    let mut sorted = scores.clone();
    sorted.sort_by(|a, b| b.partial_cmp(a).unwrap());
    assert_eq!(scores, sorted);

    // A facet narrows the vector leg too, not just the keyword one.
    let none = call(
        r#"{"id":4,"method":"search.semantic","params":{"q":"Verspätung","source":"Discord.exe"}}"#,
    )
    .unwrap();
    assert_eq!(none["total"], 0);

    let bad = call(r#"{"id":5,"method":"search.semantic","params":{"q":"x","mode":"sideways"}}"#)
        .unwrap_err();
    assert_eq!(bad.code, "params");

    let _ = std::fs::remove_dir_all(&dir);
}

/// End-to-end query latency at a realistic index size, on the real model.
///
/// Not part of the suite — it writes 100k rows and takes a minute — but it is
/// the number the brute force is justified by, so it lives next to the code it
/// justifies rather than in somebody's terminal history.
///
///   cargo test --release --test semantic -- --ignored --nocapture
#[test]
#[ignore = "benchmark: run it explicitly with --ignored"]
fn query_latency_at_a_hundred_thousand_segments() {
    let sem = model_or_skip!();
    let f = Fixture::build();
    let mut e = embedder(&sem);

    // Real vectors for the real corpus, then that corpus repeated until the
    // index is 100k rows. Repeats rather than noise: random unit vectors are
    // uniformly far apart and would flatter the top-k insertion path.
    let base: Vec<recalld::embed::Embedding> = f
        .corpus
        .iter()
        .map(|(_, t)| e.embed_passage(t).unwrap())
        .collect();

    let store = &f.store;
    let source = store.upsert_source("bulk", "bulk", 1).unwrap();
    let session = store.begin_session(source, 1).unwrap();
    const N: usize = 100_000;
    let t0 = std::time::Instant::now();
    for i in f.ids.len()..N {
        let t = 10_000_000_000i64 + i as i64;
        let id = store
            .insert_segment(session, t, t + 1, "bulk.wav", t)
            .unwrap();
        store
            .correct_segment_text(id, &format!("bulk {i}"))
            .unwrap();
        semantic::put_segment_vector(store, id, &base[i % base.len()], i as i64).unwrap();
    }
    eprintln!("seeded {N} vectors in {:.1}s", t0.elapsed().as_secs_f64());

    let leg = SemanticLeg::new(embedder(&sem));
    let within = semantic::candidates(store, &SegmentFilter::default()).unwrap();

    let t0 = std::time::Instant::now();
    let first = leg
        .search(store, "die Welt mit den Walen", 50, &within)
        .unwrap();
    eprintln!(
        "cold (load {N} vectors from SQLite + fit the correction + query): {:.0} ms",
        t0.elapsed().as_secs_f64() * 1000.0
    );
    assert_eq!(first.len(), 50);

    let queries = [
        "die Welt mit den Walen",
        "her cat spilled a drink on the keyboard",
        "Zahnschmerzen, ich sollte mal zum Arzt",
        "the train was running late",
        "Kühlschrank brummt im Hintergrund",
    ];
    let mut worst = 0.0f64;
    for q in queries {
        let t0 = std::time::Instant::now();
        for _ in 0..10 {
            leg.search(store, q, 50, &within).unwrap();
        }
        let ms = t0.elapsed().as_secs_f64() * 100.0;
        worst = worst.max(ms);
        eprintln!("  warm {q:>42?}: {ms:.1} ms");
    }
    eprintln!("worst warm query at {N} segments: {worst:.1} ms");
    assert!(
        worst < 250.0,
        "a search box has to feel instant: {worst:.1} ms"
    );
}

/// The daemon's own config has to resolve the model the fetch installs. This is
/// the seam that a renamed directory would break silently.
#[test]
fn the_default_config_resolves_what_the_catalogue_installs() {
    let cfg = Config::default();
    let sem = SemanticModel::resolve_at(PathBuf::from("/models"), &cfg.models);
    assert_eq!(
        sem.model,
        PathBuf::from("/models/multilingual-e5-small-int8/model.onnx")
    );
    assert_eq!(
        sem.tokenizer,
        PathBuf::from("/models/multilingual-e5-small-int8/tokenizer.json")
    );
    assert_eq!(sem.model_id(), "multilingual-e5-small-int8@1");
    for e in sem.entries() {
        assert!(
            e.expected.is_some(),
            "{} has no catalogued size — fetch and status would disagree",
            e.role
        );
    }
}
