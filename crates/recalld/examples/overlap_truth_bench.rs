//! What does Discord's `overlap` verdict actually contain, and does the
//! segmentation gate measure the same thing? (0.11.6, FINDINGS §26)
//!
//! Run against a **copy** of a real database — never the live one:
//!
//! ```text
//! sqlite3 "file:$HOME/.local/share/nx-recall/recall.db?mode=ro" \
//!         ".backup /path/to/scratch/recall.db"
//! cargo run -p recalld --example overlap_truth_bench -- /path/to/scratch/recall.db \
//!     [--audio ~/.local/share/nx-recall] [--model <segmentation.onnx>]
//! ```
//!
//! # Why this bench exists
//!
//! §18 step 3 measured the overlap gate against the `overlap` verdict and got
//! 5.7% recall — the gate flags seventeen of the 299 turns Discord calls
//! overlapped. Two readings fit that number and they are not exclusive: the
//! verdict's bar (two users each ≥ 0.2 coverage *somewhere* in the turn) calls
//! a two-word interjection an overlapped turn, or the detector is blind on
//! this audio. Telling them apart needs a coverage-weighted truth, which is
//! [`recalld::truth::simultaneous_frac`]: what share of the turn had two
//! mouths open **at the same time**.
//!
//! Everything here is read-only. The protocol is `learn_truth_bench`'s, for
//! the same reasons: chronological 60/40 split, own account excluded from the
//! headline, and no prototype scores the segment it came from.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};

use recalld::calib::{self, FIT_FRACTION, Score, Thresholds};
use recalld::config::IdentityConfig;
use recalld::embed::Embedding;
use recalld::identity::{self, Decision};
use recalld::store::TruthSpan;
use recalld::truth;

/// Matching `recalld::truth`: a sub-second turn is a grunt the voicebank
/// refuses anyway.
const MIN_DURATION_S: f64 = 1.0;

/// The coverage-weighted bars question 2 asks for.
const POSITIVE_BARS: [f64; 3] = [0.10, 0.25, 0.50];

/// Thresholds the gate's curve is walked over. Finer and lower than
/// `calib::overlap_grid`, which starts at 0.05: the interesting region turned
/// out to be below the shipping point, not above it.
const GATE_GRID: [f32; 12] = [
    0.02, 0.04, 0.05, 0.06, 0.08, 0.10, 0.13, 0.15, 0.20, 0.25, 0.28, 0.30,
];

struct Row {
    id: i64,
    t_start_ns: i64,
    t_end_ns: i64,
    /// What the detector said, as stored by the pipeline.
    detected: f32,
    /// What the detector says under the alternative aggregation, when the
    /// audio arm ran.
    windowed: Option<f32>,
    /// The shipping aggregation, recomputed from the stored WAV — the check
    /// that `detected` is not a stale or mis-scaled number.
    fresh: Option<f32>,
    verdict: String,
    truth_user: Option<String>,
    audio_path: String,
    words: usize,
    /// The share of the turn with two or more users talking at once.
    simul: f64,
    /// Every Discord user with at least the presence bar of coverage, and
    /// how much of the turn they covered, best first.
    present: Vec<(String, f64)>,
    embedding: Option<Embedding>,
}

impl Row {
    fn duration_s(&self) -> f64 {
        (self.t_end_ns - self.t_start_ns) as f64 / 1e9
    }
    fn frac(&self, alt: bool) -> f32 {
        if alt {
            self.windowed.unwrap_or(self.detected)
        } else {
            self.detected
        }
    }
}

struct Proto {
    speaker_id: i64,
    vector: Embedding,
    source_segment_id: Option<i64>,
}

fn pct(v: f64) -> String {
    if v.is_nan() {
        "—".into()
    } else {
        format!("{:.1}%", v * 100.0)
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let flag = |name: &str| -> Option<String> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let path = args
        .get(1)
        .filter(|a| !a.starts_with("--"))
        .context("usage: overlap_truth_bench <copy-of-recall.db> [--audio DIR] [--model ONNX]")?
        .clone();
    let cfg = IdentityConfig::default();
    let db = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening {path} read-only"))?;

    let you: Option<i64> = db
        .query_row(
            "SELECT value FROM settings WHERE key = 'you_speaker_id'",
            [],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .and_then(|v| v.parse().ok());
    let names: HashMap<i64, String> = db
        .prepare("SELECT id, display_name FROM speakers")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    // Discord user -> the voice it is linked to, where it is linked at all.
    let linked: HashMap<String, i64> = db
        .prepare("SELECT user_id, speaker_id FROM discord_users WHERE speaker_id IS NOT NULL")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;

    let protos: Vec<Proto> = db
        .prepare(
            "SELECT p.speaker_id, p.vector, p.embed_model_id, p.source_segment_id
             FROM speaker_prototypes p
             JOIN speakers s ON s.id = p.speaker_id
             WHERE s.merged_into IS NULL",
        )?
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<i64>>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|(speaker_id, blob, model, src)| {
            Ok(Proto {
                speaker_id,
                vector: Embedding::from_blob(model, &blob)?,
                source_segment_id: src,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    // ---- the corpus -------------------------------------------------------

    let mut rows: Vec<Row> = db
        .prepare(
            "SELECT g.id, g.t_start_ns, g.t_end_ns, COALESCE(g.overlap_frac, 0.0),
                    g.truth_verdict, g.truth_user_id, g.audio_path, COALESCE(g.text, ''),
                    (SELECT e.vector FROM embeddings e
                      WHERE e.segment_id = g.id ORDER BY e.id DESC LIMIT 1),
                    (SELECT e.embed_model_id FROM embeddings e
                      WHERE e.segment_id = g.id ORDER BY e.id DESC LIMIT 1)
             FROM segments g
             WHERE g.deleted_at IS NULL
               AND g.truth_verdict IN ('single', 'overlap', 'partial')
             ORDER BY g.t_start_ns ASC, g.id ASC",
        )?
        .query_map([], |r| {
            let text: String = r.get(7)?;
            let blob: Option<Vec<u8>> = r.get(8)?;
            let model: Option<String> = r.get(9)?;
            Ok((
                Row {
                    id: r.get(0)?,
                    t_start_ns: r.get(1)?,
                    t_end_ns: r.get(2)?,
                    detected: r.get::<_, f64>(3)? as f32,
                    windowed: None,
                    fresh: None,
                    verdict: r.get(4)?,
                    truth_user: r.get(5)?,
                    audio_path: r.get(6)?,
                    words: text.split_whitespace().count(),
                    simul: 0.0,
                    present: Vec::new(),
                    embedding: None,
                },
                blob,
                model,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|(mut row, blob, model)| {
            if let (Some(b), Some(m)) = (blob, model) {
                row.embedding = Some(Embedding::from_blob(m, &b)?);
            }
            Ok(row)
        })
        .collect::<Result<Vec<_>>>()?;

    // The coverage-weighted truth, recomputed from the spans. The same
    // function the daemon stores in `segments.truth_overlap_frac`, run here so
    // the bench works on a database that predates the column.
    let mut spans_stmt = db.prepare(
        "SELECT user_id, name, t_start_ns, COALESCE(t_end_ns, ?2)
           FROM truth_speaking
          WHERE t_start_ns < ?2 AND COALESCE(t_end_ns, ?2) > ?1",
    )?;
    let mut no_spans = 0usize;
    let mut stored_disagreements = 0usize;
    for row in &mut rows {
        let spans: Vec<TruthSpan> = spans_stmt
            .query_map(rusqlite::params![row.t_start_ns, row.t_end_ns], |r| {
                Ok(TruthSpan {
                    user_id: r.get(0)?,
                    name: r.get(1)?,
                    t_start_ns: r.get(2)?,
                    t_end_ns: r.get(3)?,
                    // The bench reads a corpus, not a live wire: every span it
                    // sees is whatever the archive holds, unscoped.
                    account_id: None,
                    client_kind: None,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if spans.is_empty() {
            no_spans += 1;
        }
        // `Audible::everyone()`: this bench is §26's, and §26's whole corpus
        // is what the own-account rule turned out to be wrong about. It keeps
        // counting every ring so its numbers stay comparable with the ones in
        // that section; §34's are measured against the re-judged database.
        row.simul = truth::simultaneous_frac(
            &spans,
            row.t_start_ns,
            row.t_end_ns,
            truth::Audible::everyone(),
        );
        row.present = truth::coverage(&spans, row.t_start_ns, row.t_end_ns)
            .into_iter()
            .filter(|c| c.frac >= recalld::store::truth_verdict::PRESENT_MIN)
            .map(|c| (c.user_id, c.frac))
            .collect();
        // If the column is there and filled, the stored number and the
        // recomputed one have to agree — that is the migration's test.
        if let Ok(Some(stored)) = db.query_row(
            "SELECT truth_overlap_frac FROM segments WHERE id = ?1",
            [row.id],
            |r| r.get::<_, Option<f64>>(0),
        ) && (stored - row.simul).abs() > 1e-9
        {
            stored_disagreements += 1;
        }
    }
    drop(spans_stmt);

    println!("overlap truth — bench");
    println!("  database        {path}");
    println!(
        "  corpus          {} verdicts: {} overlap, {} single, {} partial",
        rows.len(),
        rows.iter().filter(|r| r.verdict == "overlap").count(),
        rows.iter().filter(|r| r.verdict == "single").count(),
        rows.iter().filter(|r| r.verdict == "partial").count(),
    );
    if no_spans > 0 {
        println!(
            "  {no_spans} verdicted turns have no speaking span left on disk (counted as 0.0)"
        );
    }
    println!(
        "  stored column   {}",
        if stored_disagreements > 0 {
            format!("DISAGREES on {stored_disagreements} rows")
        } else {
            "absent or in agreement".into()
        }
    );

    // ---- the audio arm: a different aggregation of the same frames --------

    let win_seconds: f32 = flag("--win").and_then(|v| v.parse().ok()).unwrap_or(1.0);
    if let Some(model) = flag("--model") {
        let audio_root = flag("--audio")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| dirs_home().join(".local/share/nx-recall"));
        let mut det = recalld::overlap::OverlapDetector::load(std::path::Path::new(&model))
            .context("loading the segmentation model")?;
        let mut done = 0usize;
        let mut missing = 0usize;
        for row in &mut rows {
            let wav = audio_root.join(&row.audio_path);
            let Ok(samples) = recalld::ingest::read_wav(&wav) else {
                missing += 1;
                continue;
            };
            let classes = det.classes(&samples)?;
            // The window in frames, from this run's own frame rate rather
            // than an assumed one.
            let win = if classes.is_empty() {
                0
            } else {
                let fps = classes.len() as f32 / (samples.len() as f32 / 16_000.0);
                (win_seconds * fps).round() as usize
            };
            row.windowed = Some(recalld::overlap::windowed_max_frac(&classes, win));
            row.fresh = Some(recalld::overlap::stats_of(&classes).frac());
            done += 1;
        }
        println!(
            "  audio arm       {done} turns re-run ({missing} WAVs missing), \
             {win_seconds:.1} s sliding maximum"
        );
        // Is the stored number the number this model produces today? If it is
        // not, everything below is measuring bookkeeping rather than a model.
        let with = |f: &dyn Fn(&Row) -> Option<f32>| -> Vec<(f32, f32)> {
            rows.iter()
                .filter_map(|r| f(r).map(|v| (r.detected, v)))
                .collect()
        };
        let pairs = with(&|r: &Row| r.fresh);
        let worst = pairs
            .iter()
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        let disagree = pairs.iter().filter(|(a, b)| (a - b).abs() > 0.02).count();
        println!(
            "  stored vs fresh mean: {} of {} differ by more than 0.02, worst {worst:.3}",
            disagree,
            pairs.len()
        );
        for (what, sel) in [("truth `single`", "single"), ("truth `overlap`", "overlap")] {
            let f: Vec<f64> = rows
                .iter()
                .filter(|r| r.verdict == sel)
                .filter_map(|r| r.fresh.map(|v| v as f64))
                .collect();
            let w: Vec<f64> = rows
                .iter()
                .filter(|r| r.verdict == sel)
                .filter_map(|r| r.windowed.map(|v| v as f64))
                .collect();
            println!(
                "  {what:<16} fresh mean of the detector: {:.3} (median {:.3}); \
                 {win_seconds:.1}s maximum: {:.3} (median {:.3}); nonzero {}/{}",
                mean(&f),
                median(&f),
                mean(&w),
                median(&w),
                f.iter().filter(|v| **v > 0.0).count(),
                f.len()
            );
        }

        // The control: the same session over the Step-0 fixtures, so a
        // near-zero reading on real turns cannot be blamed on a broken model
        // or a broken build.
        if let Some(dir) = flag("--fixtures") {
            println!("  control — the same detector over the Step-0 fixtures:");
            let mut wavs: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|e| e == "wav"))
                .collect();
            wavs.sort();
            for wav in wavs {
                let Ok(samples) = recalld::ingest::read_wav(&wav) else {
                    continue;
                };
                let classes = det.classes(&samples)?;
                let win = if classes.is_empty() {
                    0
                } else {
                    let fps = classes.len() as f32 / (samples.len() as f32 / 16_000.0);
                    (win_seconds * fps).round() as usize
                };
                println!(
                    "    {:<24} mean {:.3}   {:.1}s max {:.3}",
                    wav.file_name().unwrap_or_default().to_string_lossy(),
                    recalld::overlap::stats_of(&classes).frac(),
                    win_seconds,
                    recalld::overlap::windowed_max_frac(&classes, win),
                );
            }
        }
    }

    // ---- question 1: what is in `overlap`? --------------------------------

    println!("\n=== 1. what Discord's `overlap` verdict contains ===");
    let overlaps: Vec<&Row> = rows.iter().filter(|r| r.verdict == "overlap").collect();
    histogram("simultaneous share of an `overlap` turn", &overlaps);
    let singles: Vec<&Row> = rows.iter().filter(|r| r.verdict == "single").collect();
    histogram("… and of a `single` turn, for contrast", &singles);
    let partials: Vec<&Row> = rows.iter().filter(|r| r.verdict == "partial").collect();
    histogram("… and of a `partial` turn", &partials);
    println!(
        "  median simultaneous share of an `overlap` turn: {:.3}   mean: {:.3}",
        median(&overlaps.iter().map(|r| r.simul).collect::<Vec<_>>()),
        mean(&overlaps.iter().map(|r| r.simul).collect::<Vec<_>>()),
    );

    // ---- question 2: the gate against a coverage-weighted truth -----------

    println!("\n=== 2. the gate against a coverage-weighted truth ===");
    // The population is `single` + `overlap`, as in §18: `partial` has no
    // clean answer either way and was never scored.
    let scored: Vec<&Row> = rows
        .iter()
        .filter(|r| r.verdict == "single" || r.verdict == "overlap")
        .collect();
    let times: Vec<i64> = scored.iter().map(|r| r.t_start_ns).collect();
    let cut = calib::split_at(&times, FIT_FRACTION).min(scored.len());
    let (fit, eval) = scored.split_at(cut);
    println!(
        "  {} turns, {} fit / {} held out (chronological, {:.0}%)",
        scored.len(),
        fit.len(),
        eval.len(),
        FIT_FRACTION * 100.0
    );
    for alt in [false, true] {
        if alt && rows.iter().all(|r| r.windowed.is_none()) {
            continue;
        }
        println!(
            "\n  --- detector aggregation: {} ---",
            if alt {
                format!("maximum over a {win_seconds:.1} s sliding window")
            } else {
                "mean over the turn (ships)".into()
            }
        );
        // §18's boolean truth, for continuity with the number it reported.
        curve_table(
            "verdict == overlap (§18's truth)",
            fit,
            eval,
            &|r: &Row| r.verdict == "overlap",
            alt,
            cfg.max_overlap,
        );
        for bar in POSITIVE_BARS {
            let label = format!("simultaneous >= {:.0}% of the turn", bar * 100.0);
            curve_table(
                &label,
                fit,
                eval,
                &|r: &Row| r.simul >= bar,
                alt,
                cfg.max_overlap,
            );
        }
        // The real question underneath: does the detector measure what
        // Discord measures?
        let d: Vec<f64> = scored.iter().map(|r| r.frac(alt) as f64).collect();
        let t: Vec<f64> = scored.iter().map(|r| r.simul).collect();
        println!(
            "  correlation with the simultaneous share: Pearson r = {:.3}, Spearman rho = {:.3} \
             (n = {})",
            pearson(&d, &t),
            spearman(&d, &t),
            d.len()
        );
        let hot: Vec<&&Row> = scored.iter().filter(|r| r.simul >= 0.5).collect();
        let hot_d: Vec<f64> = hot.iter().map(|r| r.frac(alt) as f64).collect();
        let hot_t: Vec<f64> = hot.iter().map(|r| r.simul).collect();
        if hot.len() > 2 {
            println!(
                "  … over the {} turns that are at least half simultaneous: r = {:.3}, \
                 mean detected {:.3}",
                hot.len(),
                pearson(&hot_d, &hot_t),
                mean(&hot_d)
            );
        }
    }

    // ---- question 3: the identity consequence -----------------------------

    println!("\n=== 3. what the refuse line costs and buys ===");
    for alt in [false, true] {
        if alt && rows.iter().all(|r| r.windowed.is_none()) {
            continue;
        }
        println!(
            "\n  --- {} ---",
            if alt {
                "sliding maximum".to_string()
            } else {
                "mean over the turn (ships)".into()
            }
        );
        consequence(&rows, &protos, &cfg, &linked, &names, you, alt)?;
    }

    // ---- question 4: does anything beat what ships? -----------------------

    println!("\n=== 4. the identity decision, held out, as the gate moves ===");
    let id_rows: Vec<&Row> = rows
        .iter()
        .filter(|r| {
            r.verdict == "single"
                && r.embedding.is_some()
                && r.duration_s() >= MIN_DURATION_S
                && r.truth_user
                    .as_ref()
                    .is_some_and(|u| linked.contains_key(u))
        })
        .collect();
    let times: Vec<i64> = id_rows.iter().map(|r| r.t_start_ns).collect();
    let cut = calib::split_at(&times, FIT_FRACTION).min(id_rows.len());
    let (_, id_eval) = id_rows.split_at(cut);
    println!(
        "  {} scoreable `single` turns, {} held out; own account ({you:?}) excluded",
        id_rows.len(),
        id_eval.len()
    );
    println!(
        "  {:<40}{:>5}{:>9}{:>7}{:>10}{:>11}{:>9}{:>8}",
        "arm", "n", "correct", "wrong", "declined", "precision", "recall", "F-0.5"
    );
    let base = identity_score(id_eval, &protos, &cfg, &linked, you, cfg.max_overlap, false)?;
    let mut arms: Vec<(String, Score)> = vec![(
        format!("mean, max_overlap {:.2} (ships)", cfg.max_overlap),
        base,
    )];
    for thr in GATE_GRID {
        if (thr - cfg.max_overlap).abs() < 1e-6 {
            continue;
        }
        arms.push((
            format!("mean, max_overlap {thr:.2}"),
            identity_score(id_eval, &protos, &cfg, &linked, you, thr, false)?,
        ));
    }
    if rows.iter().any(|r| r.windowed.is_some()) {
        for thr in GATE_GRID {
            arms.push((
                format!("{win_seconds:.1}s max, max_overlap {thr:.2}"),
                identity_score(id_eval, &protos, &cfg, &linked, you, thr, true)?,
            ));
        }
    }
    for (name, s) in &arms {
        println!(
            "  {:<40}{:>5}{:>9}{:>7}{:>10}{:>11}{:>9}{:>8.3}",
            name,
            s.n,
            s.correct,
            s.wrong,
            s.declined,
            pct(s.precision()),
            pct(s.recall()),
            s.f_beta(calib::BETA)
        );
    }
    let best = arms
        .iter()
        .filter(|(_, s)| s.precision() >= base.precision() - 1e-9)
        .max_by(|a, b| {
            a.1.f_beta(calib::BETA)
                .partial_cmp(&b.1.f_beta(calib::BETA))
                .unwrap()
        });
    match best {
        Some((name, s)) if s.f_beta(calib::BETA) > base.f_beta(calib::BETA) + 1e-9 => println!(
            "\n  best arm that does not lose precision: {name} (F-0.5 {:.3} vs {:.3}) => SHIP it",
            s.f_beta(calib::BETA),
            base.f_beta(calib::BETA)
        ),
        _ => println!(
            "\n  nothing beats the shipping arm without losing precision \
             (F-0.5 {:.3}) => KEEP {:.2} and the mean",
            base.f_beta(calib::BETA),
            cfg.max_overlap
        ),
    }
    Ok(())
}

fn dirs_home() -> std::path::PathBuf {
    std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default()
}

/// The distribution of the simultaneous share, in the buckets the argument is
/// about: is an `overlap` turn a brief interjection or a real collision?
fn histogram(what: &str, rows: &[&Row]) {
    const EDGES: [f64; 7] = [0.0, 0.05, 0.10, 0.25, 0.50, 0.75, 1.01];
    println!("\n  {what} (n = {})", rows.len());
    if rows.is_empty() {
        return;
    }
    let mut zero = 0usize;
    let mut bins = [0usize; 6];
    for r in rows {
        if r.simul <= 0.0 {
            zero += 1;
            continue;
        }
        for i in 0..6 {
            if r.simul > EDGES[i] && r.simul <= EDGES[i + 1] {
                bins[i] += 1;
                break;
            }
        }
    }
    let n = rows.len() as f64;
    let bar = |k: usize| "#".repeat((k as f64 / n * 60.0).round() as usize);
    println!(
        "    {:<16}{:>6}{:>8}  {}",
        "exactly 0",
        zero,
        pct(zero as f64 / n),
        bar(zero)
    );
    for i in 0..6 {
        println!(
            "    {:<16}{:>6}{:>8}  {}",
            format!("{:.2} .. {:.2}", EDGES[i], EDGES[i + 1].min(1.0)),
            bins[i],
            pct(bins[i] as f64 / n),
            bar(bins[i])
        );
    }
}

/// One ROC-style table: the gate's curve against one definition of "really
/// overlapped".
fn curve_table(
    what: &str,
    fit: &[&Row],
    eval: &[&Row],
    positive: &dyn Fn(&Row) -> bool,
    alt: bool,
    shipping: f32,
) {
    let pairs = |rows: &[&Row]| -> Vec<(bool, f32)> {
        rows.iter().map(|r| (positive(r), r.frac(alt))).collect()
    };
    let f = pairs(fit);
    let e = pairs(eval);
    println!(
        "\n  positives: {what}  ({} fit, {} held out)",
        f.iter().filter(|p| p.0).count(),
        e.iter().filter(|p| p.0).count()
    );
    let grid: Vec<f32> = GATE_GRID.to_vec();
    let fc = calib::overlap_curve(&f, &grid);
    let ec = calib::overlap_curve(&e, &grid);
    println!(
        "  {:<7}{:>16}{:>8}{:>8}{:>8}{:>8}{:>18}{:>8}{:>8}{:>8}{:>8}",
        "thr",
        "fit caught/miss",
        "false",
        "prec",
        "recall",
        "F-0.5",
        "held caught/miss",
        "false",
        "prec",
        "recall",
        "F-0.5"
    );
    for (a, b) in fc.iter().zip(&ec) {
        println!(
            "  {:<7}{:>16}{:>8}{:>8}{:>8}{:>8.3}{:>18}{:>8}{:>8}{:>8}{:>8.3}{}",
            format!("{:.2}", a.threshold),
            format!("{}/{}", a.caught, a.missed),
            a.false_refusals,
            pct(a.precision()),
            pct(a.recall()),
            a.f_beta(),
            format!("{}/{}", b.caught, b.missed),
            b.false_refusals,
            pct(b.precision()),
            pct(b.recall()),
            b.f_beta(),
            if (a.threshold - shipping).abs() < 1e-6 {
                "  <- ships"
            } else {
                ""
            }
        );
    }
}

/// Question 3: on the turns that really are mostly two people at once, what
/// happens to the ones the gate lets through?
///
/// Two readings of "wrong", because a mixed turn has more than one defensible
/// answer and one number cannot say both things:
///
/// * **outside** — the voice it named is not linked to *any* Discord user who
///   was present. That is wrong under every reading: nobody in the room said
///   it.
/// * **not the loudest** — the voice it named is not the user with the most
///   coverage in the turn. Stricter, and the one identity actually cares
///   about: a turn attributed to the interjector rather than the speaker is a
///   false memory even though the interjector was really there.
///
/// `partial` turns are left out, as they are everywhere else: one user between
/// 0.2 and 0.8 with nobody else over the bar means the rest of the audio
/// belongs to somebody Discord never listed, so every reading of "wrong" would
/// be measuring the VAD's edges rather than the gate. Turns whose only present
/// voice is the user's own account are out for §17's reason.
fn consequence(
    rows: &[Row],
    protos: &[Proto],
    cfg: &IdentityConfig,
    linked: &HashMap<String, i64>,
    names: &HashMap<i64, String>,
    you: Option<i64>,
    alt: bool,
) -> Result<()> {
    const CLASSES: [&str; 4] = [
        "truth `single`",
        "simultaneous < 25%",
        "simultaneous 25..50%",
        "simultaneous >= 50%",
    ];
    let table = Thresholds::global(cfg.label_threshold, 0.0);
    // class, refused -> (n, named, outside, not-the-loudest)
    let mut buckets: HashMap<(&str, bool), [i64; 4]> = HashMap::new();
    for c in CLASSES {
        for refused in [false, true] {
            buckets.insert((c, refused), [0; 4]);
        }
    }
    let mut examples: Vec<String> = Vec::new();
    for r in rows {
        let Some(emb) = &r.embedding else { continue };
        if r.duration_s() < MIN_DURATION_S || r.verdict == "partial" {
            continue;
        }
        // Which voices could be right here: everybody Discord had present.
        let allowed: HashSet<i64> = r
            .present
            .iter()
            .filter_map(|(u, _)| linked.get(u))
            .copied()
            .collect();
        if allowed.is_empty() || (allowed.len() == 1 && you.is_some_and(|y| allowed.contains(&y))) {
            continue;
        }
        // The user with the most of the turn, where they are linked at all.
        let loudest = r
            .present
            .iter()
            .filter_map(|(u, f)| linked.get(u).map(|s| (*s, *f)))
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
            .map(|(s, _)| s);
        let bank: Vec<(i64, Embedding)> = protos
            .iter()
            .filter(|p| p.source_segment_id != Some(r.id) && p.vector.model_id == emb.model_id)
            .map(|p| (p.speaker_id, p.vector.clone()))
            .collect();
        let ranked = identity::rank(emb, &bank)?;
        let refused = r.frac(alt) > cfg.max_overlap;
        // What the ladder would say with the gate removed, so a refused turn
        // still reveals what it was protecting against.
        let would = match identity::decide_with(
            cfg,
            &table,
            0.0,
            r.duration_s() as f32,
            r.words,
            &ranked,
        ) {
            Decision::Matched { speaker_id, .. } | Decision::Pinned { speaker_id } => {
                Some(speaker_id)
            }
            _ => None,
        };
        let class = if r.verdict == "single" {
            CLASSES[0]
        } else if r.simul >= 0.50 {
            CLASSES[3]
        } else if r.simul >= 0.25 {
            CLASSES[2]
        } else {
            CLASSES[1]
        };
        let slot = buckets.get_mut(&(class, refused)).unwrap();
        slot[0] += 1;
        if let Some(id) = would {
            slot[1] += 1;
            if !allowed.contains(&id) {
                slot[2] += 1;
            }
            if loudest.is_some_and(|l| l != id) {
                slot[3] += 1;
                if r.simul >= 0.5 && examples.len() < 10 {
                    examples.push(format!(
                        "    segment {:>7}  simul {:.2}  detected {:.2}  {}  named {} ({}), \
                         loudest was {} ({})",
                        r.id,
                        r.simul,
                        r.frac(alt),
                        if refused {
                            "REFUSED    "
                        } else {
                            "let through"
                        },
                        id,
                        names.get(&id).cloned().unwrap_or_default(),
                        loudest.unwrap_or(-1),
                        loudest
                            .and_then(|l| names.get(&l).cloned())
                            .unwrap_or_default()
                    ));
                }
            }
        }
    }
    println!(
        "  {:<24}{:>13}{:>7}{:>8}{:>10}{:>10}{:>12}{:>14}",
        "class", "gate", "n", "named", "outside", "outside%", "not loudest", "not loudest%"
    );
    for class in CLASSES {
        for refused in [false, true] {
            let b = buckets[&(class, refused)];
            if b[0] == 0 {
                println!(
                    "  {:<24}{:>13}{:>7}{:>8}{:>10}{:>10}{:>12}{:>14}",
                    class,
                    if refused { "refused" } else { "let through" },
                    0,
                    "—",
                    "—",
                    "—",
                    "—",
                    "—"
                );
                continue;
            }
            let named = b[1].max(1) as f64;
            println!(
                "  {:<24}{:>13}{:>7}{:>8}{:>10}{:>10}{:>12}{:>14}",
                class,
                if refused { "refused" } else { "let through" },
                b[0],
                b[1],
                b[2],
                pct(b[2] as f64 / named),
                b[3],
                pct(b[3] as f64 / named)
            );
        }
    }
    if !examples.is_empty() {
        println!("  turns at least half simultaneous where the loudest user lost the label:");
        for e in &examples {
            println!("{e}");
        }
    }
    Ok(())
}

/// `learn_truth_bench`'s identity metric, with the gate's threshold and
/// aggregation as arguments. This is the number question 4 has to move.
fn identity_score(
    rows: &[&Row],
    protos: &[Proto],
    cfg: &IdentityConfig,
    linked: &HashMap<String, i64>,
    you: Option<i64>,
    max_overlap: f32,
    alt: bool,
) -> Result<Score> {
    let mut cfg = cfg.clone();
    cfg.max_overlap = max_overlap;
    let table = Thresholds::global(cfg.label_threshold, 0.0);
    let mut score = Score::default();
    for r in rows {
        let (Some(emb), Some(user)) = (&r.embedding, &r.truth_user) else {
            continue;
        };
        let Some(&truth) = linked.get(user) else {
            continue;
        };
        if Some(truth) == you {
            continue;
        }
        let bank: Vec<(i64, Embedding)> = protos
            .iter()
            .filter(|p| p.source_segment_id != Some(r.id) && p.vector.model_id == emb.model_id)
            .map(|p| (p.speaker_id, p.vector.clone()))
            .collect();
        let ranked = identity::rank(emb, &bank)?;
        let label = match identity::decide_with(
            &cfg,
            &table,
            r.frac(alt),
            r.duration_s() as f32,
            r.words,
            &ranked,
        ) {
            Decision::Matched { speaker_id, .. } | Decision::Pinned { speaker_id } => {
                Some(speaker_id)
            }
            _ => None,
        };
        score.add(label, truth);
    }
    Ok(score)
}

// ---- small statistics ------------------------------------------------------

fn mean(v: &[f64]) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.iter().sum::<f64>() / v.len() as f64
}

fn median(v: &[f64]) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    s[s.len() / 2]
}

fn pearson(a: &[f64], b: &[f64]) -> f64 {
    let (ma, mb) = (mean(a), mean(b));
    let mut num = 0.0;
    let mut da = 0.0;
    let mut dbv = 0.0;
    for (x, y) in a.iter().zip(b) {
        num += (x - ma) * (y - mb);
        da += (x - ma) * (x - ma);
        dbv += (y - mb) * (y - mb);
    }
    if da <= 0.0 || dbv <= 0.0 {
        return f64::NAN;
    }
    num / (da * dbv).sqrt()
}

/// Ranks with ties averaged, then Pearson over the ranks.
fn ranks(v: &[f64]) -> Vec<f64> {
    let mut idx: Vec<usize> = (0..v.len()).collect();
    idx.sort_by(|&i, &j| v[i].partial_cmp(&v[j]).unwrap());
    let mut out = vec![0.0; v.len()];
    let mut i = 0;
    while i < idx.len() {
        let mut j = i;
        while j + 1 < idx.len() && v[idx[j + 1]] == v[idx[i]] {
            j += 1;
        }
        let r = (i + j) as f64 / 2.0 + 1.0;
        for k in i..=j {
            out[idx[k]] = r;
        }
        i = j + 1;
    }
    out
}

fn spearman(a: &[f64], b: &[f64]) -> f64 {
    pearson(&ranks(a), &ranks(b))
}
