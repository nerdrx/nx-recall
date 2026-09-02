//! Does learning from ground truth make the voicebank righter? (0.11.0)
//!
//! Run against a **copy** of a real database — never the live one:
//!
//! ```text
//! sqlite3 "file:$HOME/.local/share/nx-recall/recall.db?mode=ro" \
//!         ".backup /path/to/scratch/recall.db"
//! cargo run -p recalld --example learn_truth_bench -- /path/to/scratch/recall.db
//! ```
//!
//! # The protocol, and why each rule is in it
//!
//! * **Chronological split.** The first [`FIT_FRACTION`] of truth-labelled
//!   rows *by time* is all any fit may see; the last 40% is the only thing any
//!   gate reads. A random split would let tonight's fit be scored on tonight,
//!   which is the question nobody asked.
//! * **No self-derived prototype.** A prototype that came from the very
//!   segment being judged is dropped from the bank for that judgement, exactly
//!   as `source_prior_bench` does it. Without this the row scores 1.0 against
//!   itself and the whole measurement is a memory test.
//! * **Own account excluded.** Since 0.10.1 a `single` verdict naming the
//!   user's own Discord account is not ground truth about audio captured from
//!   the user's own client — a client does not play your microphone back to
//!   you. Both numbers are printed; the headline is own-excluded.
//! * **Every arm sees the same bank.** The bank is today's prototypes in every
//!   arm, so the comparison is like-for-like. It is *not* a claim that the
//!   bank itself is untainted by the eval rows; it is a claim that no arm is
//!   more tainted than another, which is what a comparison needs.
//!
//! Numbers live in `spike/FINDINGS.md` §18.

use std::collections::HashMap;

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};

use recalld::calib::{
    self, FIT_FRACTION, Labelled, MIN_ROWS_PER_VOICE, Obs, Projection, Score, THRESHOLD_BOUNDS,
    Thresholds, Whitening,
};
use recalld::config::IdentityConfig;
use recalld::embed::Embedding;
use recalld::identity::{self, Decision};

/// Matching `recalld::truth`: a sub-second turn is a grunt the voicebank
/// refuses anyway, and scoring one measures the floor rather than the model.
const MIN_DURATION_S: f64 = 1.0;

/// The whitening settings the projection fit chooses between. Chosen on an
/// *inner* split of the fit split — never on the held-out rows, which would be
/// the same leak the protocol exists to prevent.
/// Row bars the step-1 fit chooses between, again on an inner split.
const MIN_ROWS_GRID: [usize; 6] = [5, 10, 15, 20, 30, 50];

fn whitening_grid() -> Vec<Whitening> {
    let mut out = Vec::new();
    for centre in [false, true] {
        for power in [0.1, 0.25, 0.5, 1.0] {
            for shrinkage in [0.05, 0.2, 0.5, 0.9] {
                out.push(Whitening {
                    shrinkage,
                    power,
                    centre,
                });
            }
        }
    }
    out
}

struct Row {
    id: i64,
    t_start_ns: i64,
    overlap_frac: f32,
    duration_s: f64,
    words: usize,
    truth: i64,
    embedding: Embedding,
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

fn header(what: &str) {
    println!(
        "  {:<34}{:>5}{:>9}{:>7}{:>10}{:>11}{:>9}{:>8}",
        what, "n", "correct", "wrong", "declined", "precision", "recall", "F-0.5"
    );
}

fn line(name: &str, s: Score) {
    println!(
        "  {:<34}{:>5}{:>9}{:>7}{:>10}{:>11}{:>9}{:>8.3}",
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

fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .context("usage: learn_truth_bench <copy-of-recall.db>")?;
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

    // Every `single` turn with a linked account, a stored embedding and a
    // second of audio, in time order.
    let rows: Vec<Row> = db
        .prepare(
            "SELECT g.id, g.t_start_ns, COALESCE(g.overlap_frac, 0.0),
                    (g.t_end_ns - g.t_start_ns), COALESCE(g.text, ''),
                    d.speaker_id, e.vector, e.embed_model_id
             FROM segments g
             JOIN discord_users d ON d.user_id = g.truth_user_id
             JOIN embeddings e ON e.id = (
                 SELECT MAX(x.id) FROM embeddings x WHERE x.segment_id = g.id)
             WHERE g.deleted_at IS NULL
               AND g.truth_verdict = 'single'
               AND d.speaker_id IS NOT NULL
               AND (g.t_end_ns - g.t_start_ns) >= ?1
             ORDER BY g.t_start_ns ASC, g.id ASC",
        )?
        .query_map([(MIN_DURATION_S * 1e9) as i64], |r| {
            let text: String = r.get(4)?;
            let blob: Vec<u8> = r.get(6)?;
            let model: String = r.get(7)?;
            Ok((
                Row {
                    id: r.get(0)?,
                    t_start_ns: r.get(1)?,
                    overlap_frac: r.get::<_, f64>(2)? as f32,
                    duration_s: r.get::<_, i64>(3)? as f64 / 1e9,
                    words: text.split_whitespace().count(),
                    truth: r.get(5)?,
                    embedding: Embedding::new("", Vec::new()),
                },
                blob,
                model,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|(mut row, blob, model)| {
            row.embedding = Embedding::from_blob(model, &blob)?;
            Ok(row)
        })
        .collect::<Result<Vec<_>>>()?;

    // The protocol's split is one chronological cut across every row. On a
    // corpus where each person talks in blocks that can leave a voice with no
    // fit rows at all, so `--stratify` is offered as a *secondary* reading:
    // each voice's own first 60% by time. It is weaker — one voice's held-out
    // rows can precede another's fit rows — and is reported as such, never as
    // the headline.
    let stratify = std::env::args().any(|a| a == "--stratify");
    let (fit, eval): (Vec<&Row>, Vec<&Row>) = if stratify {
        let mut per: HashMap<i64, Vec<&Row>> = HashMap::new();
        for r in &rows {
            per.entry(r.truth).or_default().push(r);
        }
        let mut f: Vec<&Row> = Vec::new();
        let mut e: Vec<&Row> = Vec::new();
        let mut ks: Vec<i64> = per.keys().copied().collect();
        ks.sort();
        for k in ks {
            let v = &per[&k];
            let t: Vec<i64> = v.iter().map(|r| r.t_start_ns).collect();
            let c = calib::split_at(&t, FIT_FRACTION).min(v.len());
            f.extend(&v[..c]);
            e.extend(&v[c..]);
        }
        f.sort_by_key(|r| r.t_start_ns);
        e.sort_by_key(|r| r.t_start_ns);
        (f, e)
    } else {
        let times: Vec<i64> = rows.iter().map(|r| r.t_start_ns).collect();
        let cut = calib::split_at(&times, FIT_FRACTION).min(rows.len());
        (rows[..cut].iter().collect(), rows[cut..].iter().collect())
    };
    let fit = fit.as_slice();
    let eval = eval.as_slice();
    let fit_ids: std::collections::HashSet<i64> = fit.iter().map(|r| r.id).collect();

    println!("learned identity — bench");
    println!("  database        {path}");
    println!(
        "  truth rows      {} total, {} fit / {} held out ({}, {:.0}%)",
        rows.len(),
        fit.len(),
        eval.len(),
        if stratify {
            "per-voice chronological (SECONDARY)"
        } else {
            "chronological"
        },
        FIT_FRACTION * 100.0
    );
    if let (Some(a), Some(b)) = (rows.first(), rows.last()) {
        println!(
            "  span            segment {} .. {} ({:.1} h)",
            a.id,
            b.id,
            (b.t_start_ns - a.t_start_ns) as f64 / 3.6e12
        );
    }
    println!("  own account     speaker {you:?} (excluded from the headline)");
    {
        let mut per: HashMap<i64, (usize, usize)> = HashMap::new();
        for r in &rows {
            let e = per.entry(r.truth).or_default();
            if fit_ids.contains(&r.id) {
                e.0 += 1
            } else {
                e.1 += 1
            }
        }
        let mut ks: Vec<_> = per.keys().copied().collect();
        ks.sort();
        for k in ks {
            let (f, e) = per[&k];
            println!(
                "    voice {k:<3} {:<12} fit {f:<5} held out {e}",
                names.get(&k).cloned().unwrap_or_default()
            );
        }
    }
    if eval.is_empty() {
        println!("\nnothing is held out; there is no measurement to make.");
        return Ok(());
    }

    // ---- step 1: per-voice thresholds, in the raw space -------------------

    let raw_fit_obs = observe(fit, &protos, &cfg, None)?;

    println!("\n=== step 1: per-voice thresholds (raw space) ===");
    // How many rows a voice must have before its threshold is fitted is
    // itself a choice, and it is the choice that decides whether the voices
    // producing the wrong labels are in scope at all. It is picked on an
    // *inner* split of the fit split, never on the held-out rows.
    let min_rows = {
        let inner_times: Vec<i64> = fit.iter().map(|r| r.t_start_ns).collect();
        let inner_cut = calib::split_at(&inner_times, FIT_FRACTION).min(fit.len());
        let (inner_fit, inner_eval) = fit.split_at(inner_cut);
        let inner_obs = observe(inner_fit, &protos, &cfg, None)?;
        let inner_base = judge(inner_eval, &protos, &cfg, None, None, you)?;
        println!(
            "  choosing the row bar on an inner split ({} / {} rows); baseline there: \
             precision {}, F-0.5 {:.3}",
            inner_fit.len(),
            inner_eval.len(),
            pct(inner_base.clean.precision()),
            inner_base.clean.f_beta(calib::BETA)
        );
        println!(
            "  {:<10}{:>8}{:>9}{:>7}{:>12}{:>10}",
            "min rows", "voices", "correct", "wrong", "precision", "F-0.5"
        );
        let mut best = (inner_base.clean.f_beta(calib::BETA), MIN_ROWS_PER_VOICE);
        for bar in MIN_ROWS_GRID {
            let fitted = calib::fit_thresholds(
                &inner_obs,
                bar,
                THRESHOLD_BOUNDS,
                (cfg.label_threshold, 0.0),
            );
            let mut t = Thresholds::global(cfg.label_threshold, 0.0);
            for v in &fitted {
                t.insert(v.speaker_id, v.threshold, v.margin);
            }
            let sc = judge(inner_eval, &protos, &cfg, None, Some(&t), you)?;
            let f = sc.clean.f_beta(calib::BETA);
            println!(
                "  {:<10}{:>8}{:>9}{:>7}{:>12}{:>10.3}",
                bar,
                fitted.len(),
                sc.clean.correct,
                sc.clean.wrong,
                pct(sc.clean.precision()),
                f
            );
            // Precision is the veto here too, exactly as in `swap_is_safe`.
            if f > best.0 + 1e-9 && sc.clean.precision() >= inner_base.clean.precision() - 1e-9 {
                best = (f, bar);
            }
        }
        println!("  chosen row bar: {} (inner F-0.5 {:.3})", best.1, best.0);
        best.1
    };

    let learned = calib::fit_thresholds(
        &raw_fit_obs,
        min_rows,
        THRESHOLD_BOUNDS,
        (cfg.label_threshold, 0.0),
    );
    let mut table = Thresholds::global(cfg.label_threshold, 0.0);
    for v in &learned {
        table.insert(v.speaker_id, v.threshold, v.margin);
    }

    if learned.is_empty() {
        println!("  no voice cleared {min_rows} fit rows with a point that beats the global.");
    }
    println!(
        "  {:<8}{:<14}{:>7}{:>12}{:>9}{:>14}{:>14}",
        "voice", "name", "n", "threshold", "margin", "F-0.5 (fit)", "F-0.5 global"
    );
    for v in &learned {
        println!(
            "  {:<8}{:<14}{:>7}{:>12.2}{:>9.2}{:>14.3}{:>14.3}",
            v.speaker_id,
            names.get(&v.speaker_id).cloned().unwrap_or_default(),
            v.n,
            v.threshold,
            v.margin,
            v.f_beta,
            v.f_beta_global
        );
    }
    // What the rows that did NOT clear the bar look like, so a reader can see
    // whether the bar or the data is what stopped them.
    {
        let mut per: HashMap<i64, usize> = HashMap::new();
        for o in &raw_fit_obs {
            *per.entry(o.top).or_default() += 1;
        }
        let mut ks: Vec<_> = per.keys().copied().collect();
        ks.sort_by_key(|k| std::cmp::Reverse(per[k]));
        let skipped: Vec<String> = ks
            .iter()
            .filter(|k| !learned.iter().any(|v| v.speaker_id == **k))
            .map(|k| {
                format!(
                    "{k} ({}) n={}",
                    names.get(k).cloned().unwrap_or_default(),
                    per[k]
                )
            })
            .collect();
        if !skipped.is_empty() {
            println!("  kept the global: {}", skipped.join(", "));
        }
    }

    let base = judge(eval, &protos, &cfg, None, None, you)?;
    let step1 = judge(eval, &protos, &cfg, None, Some(&table), you)?;

    println!("\n  held out ({} rows)", eval.len());
    header("arm");
    line("0.10.2 baseline", base.all);
    line("0.10.2, own excluded", base.clean);
    line("+ per-voice thresholds", step1.all);
    line("+ per-voice, own excluded", step1.clean);
    gate_verdict("step 1", &base.clean, &step1.clean);

    println!("\n  the wrong labels the baseline leaves, held out:");
    for (seg, named, score, truth) in &base.wrong {
        println!(
            "    segment {seg:>7}  named {named} ({}) at {score:.2}, really {truth} ({})",
            names.get(named).cloned().unwrap_or_default(),
            names.get(truth).cloned().unwrap_or_default()
        );
    }

    // Diagnostic only, and marked: what every row bar would have scored on the
    // held-out rows. Reading a choice off this table is exactly the leak the
    // protocol forbids — it is printed so the shape of the trade-off is
    // visible, not so a number can be picked out of it.
    println!("\n  DIAGNOSTIC (not a selection surface): every row bar, held out");
    println!(
        "  {:<10}{:>8}{:>9}{:>7}{:>10}{:>12}{:>9}{:>8}",
        "min rows", "voices", "correct", "wrong", "declined", "precision", "recall", "F-0.5"
    );
    for bar in MIN_ROWS_GRID {
        let fitted = calib::fit_thresholds(
            &raw_fit_obs,
            bar,
            THRESHOLD_BOUNDS,
            (cfg.label_threshold, 0.0),
        );
        let mut t = Thresholds::global(cfg.label_threshold, 0.0);
        for v in &fitted {
            t.insert(v.speaker_id, v.threshold, v.margin);
        }
        let sc = judge(eval, &protos, &cfg, None, Some(&t), you)?.clean;
        println!(
            "  {:<10}{:>8}{:>9}{:>7}{:>10}{:>12}{:>9}{:>8.3}",
            bar,
            fitted.len(),
            sc.correct,
            sc.wrong,
            sc.declined,
            pct(sc.precision()),
            pct(sc.recall()),
            sc.f_beta(calib::BETA)
        );
    }

    // ---- step 2: a learned projection -------------------------------------

    println!("\n=== step 2: a learned within-class projection ===");
    let fit_labelled: Vec<Labelled> = fit
        .iter()
        .map(|r| Labelled {
            class: r.truth,
            v: r.embedding.vector.clone(),
        })
        .collect();
    let model_id = rows[0].embedding.model_id.clone();

    // Pick the whitening on an inner chronological split of the fit split.
    let inner_times: Vec<i64> = fit.iter().map(|r| r.t_start_ns).collect();
    let inner_cut = calib::split_at(&inner_times, FIT_FRACTION).min(fit.len());
    let mut chosen: Option<(f64, Whitening, Projection)> = None;
    println!(
        "  choosing the whitening on an inner split of the fit split ({} / {} rows)",
        inner_cut,
        fit.len().saturating_sub(inner_cut)
    );
    if inner_cut < fit.len() {
        let (inner_fit, inner_eval) = fit.split_at(inner_cut);
        let inner_labelled: Vec<Labelled> = inner_fit
            .iter()
            .map(|r| Labelled {
                class: r.truth,
                v: r.embedding.vector.clone(),
            })
            .collect();
        let inner_base = judge(inner_eval, &protos, &cfg, None, None, you)?;
        println!(
            "  {:<8}{:<8}{:<9}{:>6}{:>9}{:>7}{:>12}{:>10}",
            "shrink", "power", "centred", "n", "correct", "wrong", "precision", "F-0.5"
        );
        println!(
            "  {:<8}{:<8}{:<9}{:>6}{:>9}{:>7}{:>12}{:>10.3}",
            "—",
            "—",
            "baseline",
            inner_base.clean.n,
            inner_base.clean.correct,
            inner_base.clean.wrong,
            pct(inner_base.clean.precision()),
            inner_base.clean.f_beta(calib::BETA)
        );
        for w in whitening_grid() {
            let Ok(p) = calib::fit_projection(&model_id, &inner_labelled, w) else {
                continue;
            };
            let s = judge(inner_eval, &protos, &cfg, Some(&p), None, you)?;
            println!(
                "  {:<8.2}{:<8.2}{:<9}{:>6}{:>9}{:>7}{:>12}{:>10.3}",
                w.shrinkage,
                w.power,
                if w.centre { "yes" } else { "no" },
                s.clean.n,
                s.clean.correct,
                s.clean.wrong,
                pct(s.clean.precision()),
                s.clean.f_beta(calib::BETA)
            );
            let f = s.clean.f_beta(calib::BETA);
            if chosen.as_ref().is_none_or(|(bf, _, _)| f > *bf) {
                chosen = Some((f, w, p));
            }
        }
    }
    let Some((inner_f, w, _)) = chosen else {
        println!("  the inner split had nothing to choose on; step 2 is unmeasurable.");
        return step3(&db, &cfg);
    };
    // Refit on the whole fit split at the chosen setting.
    let proj = calib::fit_projection(&model_id, &fit_labelled, w)?;
    println!(
        "  chosen: shrinkage {:.2}, power {:.2}, centred {} (inner F-0.5 {inner_f:.3});\n  \
         fitted on {} rows over {} people, {}x{} floats",
        w.shrinkage, w.power, w.centre, proj.n_rows, proj.n_classes, proj.dim, proj.dim
    );

    let step2 = judge(eval, &protos, &cfg, Some(&proj), None, you)?;
    // Thresholds refitted *in the projected space* — the raw-space ones are
    // numbers about a different geometry and would mean nothing here.
    let proj_fit_obs = observe(fit, &protos, &cfg, Some(&proj))?;
    let proj_learned = calib::fit_thresholds(
        &proj_fit_obs,
        MIN_ROWS_PER_VOICE,
        THRESHOLD_BOUNDS,
        (cfg.label_threshold, 0.0),
    );
    let mut proj_table = Thresholds::global(cfg.label_threshold, 0.0);
    for v in &proj_learned {
        proj_table.insert(v.speaker_id, v.threshold, v.margin);
    }
    let step12 = judge(eval, &protos, &cfg, Some(&proj), Some(&proj_table), you)?;

    println!("\n  held out ({} rows)", eval.len());
    header("arm");
    line("0.10.2 baseline, own excluded", base.clean);
    line("projection, baseline thresholds", step2.clean);
    line("projection + per-voice", step12.clean);
    for v in &proj_learned {
        println!(
            "    in-space threshold for voice {} ({}): {:.2} margin {:.2}, n={}",
            v.speaker_id,
            names.get(&v.speaker_id).cloned().unwrap_or_default(),
            v.threshold,
            v.margin,
            v.n
        );
    }
    gate_verdict("step 2 (space alone)", &base.clean, &step2.clean);
    gate_verdict("step 2 + step 1", &base.clean, &step12.clean);
    println!(
        "  swap rule (projection over baseline): {}",
        if calib::swap_is_safe(&base.clean, &step2.clean) {
            "SWAP"
        } else {
            "REFUSE"
        }
    );

    step3(&db, &cfg)
}

/// Step 3: the overlap gate against Discord's own overlap verdicts.
fn step3(db: &Connection, cfg: &IdentityConfig) -> Result<()> {
    println!("\n=== step 3: the overlap gate against truth ===");
    let rows: Vec<(i64, bool, f32)> = db
        .prepare(
            "SELECT t_start_ns, truth_verdict, COALESCE(overlap_frac, 0.0)
             FROM segments
             WHERE deleted_at IS NULL AND truth_verdict IN ('single', 'overlap')
             ORDER BY t_start_ns ASC, id ASC",
        )?
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)? == "overlap",
                r.get::<_, f64>(2)? as f32,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let times: Vec<i64> = rows.iter().map(|r| r.0).collect();
    let cut = calib::split_at(&times, FIT_FRACTION);
    let pairs: Vec<(bool, f32)> = rows.iter().map(|r| (r.1, r.2)).collect();
    let (fit, eval) = pairs.split_at(cut.min(pairs.len()));
    println!(
        "  {} verdicts ({} overlap, {} single); {} fit / {} held out",
        rows.len(),
        rows.iter().filter(|r| r.1).count(),
        rows.iter().filter(|r| !r.1).count(),
        fit.len(),
        eval.len()
    );

    let grid = calib::overlap_grid();
    let fit_curve = calib::overlap_curve(fit, &grid);
    let eval_curve = calib::overlap_curve(eval, &grid);
    println!(
        "  {:<8}{:>18}{:>10}{:>10}{:>10}{:>22}{:>10}{:>10}{:>10}",
        "thr",
        "fit: caught/miss",
        "false",
        "prec",
        "F-0.5",
        "held out: caught/miss",
        "false",
        "prec",
        "F-0.5"
    );
    for (f, e) in fit_curve.iter().zip(&eval_curve) {
        let here = (f.threshold - cfg.max_overlap).abs() < 1e-6;
        println!(
            "  {:<8}{:>18}{:>10}{:>10}{:>10.3}{:>22}{:>10}{:>10}{:>10.3}{}",
            format!("{:.2}", f.threshold),
            format!("{}/{}", f.caught, f.missed),
            f.false_refusals,
            pct(f.precision()),
            f.f_beta(),
            format!("{}/{}", e.caught, e.missed),
            e.false_refusals,
            pct(e.precision()),
            e.f_beta(),
            if here { "   <- shipping" } else { "" }
        );
    }
    let best = fit_curve
        .iter()
        .max_by(|a, b| a.f_beta().partial_cmp(&b.f_beta()).unwrap())
        .copied();
    let current_eval = eval_curve
        .iter()
        .find(|p| (p.threshold - cfg.max_overlap).abs() < 1e-6)
        .copied();
    if let (Some(best), Some(cur)) = (best, current_eval) {
        let best_eval = eval_curve
            .iter()
            .find(|p| (p.threshold - best.threshold).abs() < 1e-6)
            .copied();
        println!(
            "\n  best on the fit split: {:.2} (F-0.5 {:.3}); the shipping {:.2} scores {:.3} there",
            best.threshold,
            best.f_beta(),
            cfg.max_overlap,
            fit_curve
                .iter()
                .find(|p| (p.threshold - cfg.max_overlap).abs() < 1e-6)
                .map(|p| p.f_beta())
                .unwrap_or(f64::NAN)
        );
        if let Some(be) = best_eval {
            println!(
                "  held out: {:.2} scores {:.3}, shipping {:.2} scores {:.3} -> {}",
                best.threshold,
                be.f_beta(),
                cfg.max_overlap,
                cur.f_beta(),
                if be.f_beta() > cur.f_beta() + 1e-9 {
                    "SHIP the new threshold"
                } else {
                    "KEEP the shipping threshold; report the curve"
                }
            );
        }
    }
    Ok(())
}

/// A pair of scores: everything, and everything but the user's own account.
struct Arm {
    all: Score,
    clean: Score,
    /// `(segment, the voice it named, the score, who it really was)` for every
    /// own-excluded row it got wrong. The list is the point: a precision
    /// number says how much is wrong, this says what to fix.
    wrong: Vec<(i64, i64, f32, i64)>,
}

/// Reduce the fit split to the observations a threshold fit needs.
fn observe(
    rows: &[&Row],
    protos: &[Proto],
    cfg: &IdentityConfig,
    proj: Option<&Projection>,
) -> Result<Vec<Obs>> {
    let bank = bank_of(protos, proj)?;
    let mut out = Vec::new();
    for r in rows {
        let (probe, bank) = prepared(r, &bank, proj)?;
        if identity::gate(cfg, r.overlap_frac, r.duration_s as f32).is_some() {
            continue;
        }
        let ranked = identity::rank(&probe, &bank)?;
        let Some(top) = ranked.first() else { continue };
        let runner_up = ranked.get(1).map(|c| c.score).unwrap_or(f32::NEG_INFINITY);
        out.push(Obs {
            top: top.speaker_id,
            score: top.score,
            margin: top.score - runner_up,
            truth: r.truth,
        });
    }
    Ok(out)
}

/// Replay the ladder over a set of rows and score it.
fn judge(
    rows: &[&Row],
    protos: &[Proto],
    cfg: &IdentityConfig,
    proj: Option<&Projection>,
    table: Option<&Thresholds>,
    you: Option<i64>,
) -> Result<Arm> {
    let fallback = Thresholds::global(cfg.label_threshold, 0.0);
    let table = table.unwrap_or(&fallback);
    let projected = bank_of(protos, proj)?;
    let mut arm = Arm {
        all: Score::default(),
        clean: Score::default(),
        wrong: Vec::new(),
    };
    for r in rows {
        let (probe, bank) = prepared(r, &projected, proj)?;
        let ranked = identity::rank(&probe, &bank)?;
        let label = match identity::decide_with(
            cfg,
            table,
            r.overlap_frac,
            r.duration_s as f32,
            r.words,
            &ranked,
        ) {
            Decision::Matched { speaker_id, .. } | Decision::Pinned { speaker_id } => {
                Some(speaker_id)
            }
            // A mint is a decline against ground truth, not a wrong answer:
            // "nobody in the bank" is a different claim from "this person".
            _ => None,
        };
        arm.all.add(label, r.truth);
        if Some(r.truth) != you {
            arm.clean.add(label, r.truth);
            if let Some(id) = label
                && id != r.truth
            {
                arm.wrong.push((
                    r.id,
                    id,
                    ranked.first().map(|c| c.score).unwrap_or(0.0),
                    r.truth,
                ));
            }
        }
    }
    Ok(arm)
}

/// The whole bank, projected once. Projecting it per row would be the same
/// matrix multiply a hundred times over and is the only thing here slow enough
/// to notice.
fn bank_of(protos: &[Proto], proj: Option<&Projection>) -> Result<Vec<Proto>> {
    protos
        .iter()
        .map(|p| {
            Ok(Proto {
                speaker_id: p.speaker_id,
                vector: match proj {
                    None => p.vector.clone(),
                    Some(m) => m.apply(&p.vector)?,
                },
                source_segment_id: p.source_segment_id,
            })
        })
        .collect()
}

/// The probe and the bank for one judgement, projected if a map is in play.
///
/// The bank drops every prototype this very segment produced — without it a
/// row scores 1.0 against itself and the measurement is a memory test.
fn prepared(
    r: &Row,
    bank: &[Proto],
    proj: Option<&Projection>,
) -> Result<(Embedding, Vec<(i64, Embedding)>)> {
    let probe = match proj {
        None => r.embedding.clone(),
        Some(p) => p.apply(&r.embedding)?,
    };
    let bank = bank
        .iter()
        .filter(|p| p.source_segment_id != Some(r.id) && p.vector.model_id == probe.model_id)
        .map(|p| (p.speaker_id, p.vector.clone()))
        .collect();
    Ok((probe, bank))
}

/// The gate this round was given: held-out precision must not fall, and either
/// recall rises two points or the wrong-label count falls by a fifth.
fn gate_verdict(what: &str, base: &Score, arm: &Score) {
    let p_ok = arm.precision() >= base.precision() - 1e-9;
    let recall_up = (arm.recall() - base.recall()) * 100.0;
    let wrong_down = if base.wrong == 0 {
        0.0
    } else {
        (base.wrong - arm.wrong) as f64 / base.wrong as f64 * 100.0
    };
    let moved = recall_up >= 2.0 - 1e-9 || wrong_down >= 20.0 - 1e-9;
    println!(
        "  gate [{what}]: precision {} ({:+.1} pp), recall {:+.1} pp, wrong {} -> {} ({:+.0}%)  => {}",
        if p_ok { "held" } else { "FELL" },
        (arm.precision() - base.precision()) * 100.0,
        recall_up,
        base.wrong,
        arm.wrong,
        -wrong_down,
        if p_ok && moved { "PASS" } else { "FAIL" }
    );
}
