//! The calibration pass: ground truth in, an operating point out (0.11.0).
//!
//! [`crate::calib`] is the math and knows nothing about a database. This is
//! the pass that reads the truth rows, replays the ladder over them, asks
//! `calib` what it would change, **measures the answer on held-out rows** and
//! only then writes anything.
//!
//! ## The gate is in the code, not in a decision somebody made once
//!
//! Nothing here trusts its own fit. Every candidate — a per-voice threshold
//! table, a learned projection — is scored against the incumbent on the last
//! 40% of truth rows by time, and [`crate::calib::swap_is_safe`] has a veto on
//! held-out precision. That is why the switch can default to on while the
//! measured answer on tonight's data is "change nothing": the pass computes,
//! compares, declines, and logs the comparison. The evening the evidence
//! supports a change, it makes it, and the same table is in `operations` to
//! say why.
//!
//! ## What it will not do
//!
//! * It will not touch the **enrol** bar. A wrong prototype is permanent and
//!   nothing has measured that decision.
//! * It will not fit a voice with fewer than [`crate::calib::MIN_ROWS_PER_VOICE`]
//!   truth rows in the fit split. Under that, a threshold is a description of
//!   one evening.
//! * It will not score a row against a prototype the row itself produced.
//!   Without that rule the whole measurement is a memory test, and the number
//!   it produces is a beautiful lie.
//!
//! The measured numbers behind the defaults are in `spike/FINDINGS.md` §18.

use anyhow::Result;
use serde_json::{Value, json};

use crate::calib::{
    self, FIT_FRACTION, GatePoint, Labelled, MIN_ROWS_PER_VOICE, Obs, Projection, Score,
    THRESHOLD_BOUNDS, Thresholds, VoiceThreshold, Whitening,
};
use crate::config::IdentityConfig;
use crate::embed::Embedding;
use crate::store::{CalibrationRow, Store, truth_via};

/// Matching `crate::truth`: a sub-second turn is a grunt the voicebank refuses
/// anyway, and calibrating on one measures the floor rather than the model.
pub const MIN_DURATION_S: f64 = 1.0;

/// The `operations` op the pass writes its before/after table under.
pub const OP: &str = "identity.calibrate";

/// The whitening settings a projection fit chooses between. Small powers
/// first: on a few hundred rows a full whitening is a claim about directions
/// the sample has not seen.
fn whitening_grid() -> Vec<Whitening> {
    let mut out = Vec::new();
    for power in [0.1, 0.25, 0.5, 1.0] {
        for shrinkage in [0.05, 0.2, 0.5, 0.9] {
            out.push(Whitening {
                shrinkage,
                power,
                // Measured off: the extractor already applies its own
                // global-mean normalisation, and re-centring on one evening's
                // rows moves every other voice relative to an origin that is
                // really whoever talked most (FINDINGS §18).
                centre: false,
            });
        }
    }
    out
}

/// One prototype-aggregate arm, measured the two ways that matter (0.12.3).
///
/// A rule and a threshold are not independent choices: a bar is a number on a
/// score scale, and the aggregate *is* the scale. Comparing a top-3 mean
/// against bars fitted under max measures the scale and not the rule — §36's
/// error, in the one place §32 left it. So every arm carries both its score
/// under the global bar and its score under bars refit for **it**, and the
/// installation gate reads the second.
#[derive(Debug, Clone, PartialEq)]
pub struct AggregateArm {
    pub rule: calib::Aggregate,
    /// Held out with every voice on the global bar.
    pub globals: Score,
    /// Held out with per-voice thresholds fitted on the fit split under this
    /// rule. Equal to `globals` when the fit proposes nothing.
    pub fitted: Score,
    /// The thresholds behind `fitted`, and what an install of this rule writes.
    pub thresholds: Vec<VoiceThreshold>,
    /// Is this the rule the box is running right now?
    pub incumbent: bool,
}

/// What one calibration run found, whether or not it wrote anything.
#[derive(Debug, Clone, Default)]
pub struct Report {
    /// Truth rows in total, and how the chronological split fell.
    pub rows: usize,
    pub fit_rows: usize,
    pub eval_rows: usize,
    /// Per-voice truth rows, `(speaker, fit, held out)`.
    pub per_voice: Vec<(i64, usize, usize)>,
    /// The thresholds the fit proposes.
    pub proposed: Vec<VoiceThreshold>,
    /// What is installed right now.
    pub installed: Vec<(i64, f32, f32, i64)>,
    /// Held-out scores: the globals, and the proposal.
    pub baseline: Score,
    pub candidate: Score,
    /// Held-out scores for the projection arm, when one could be fitted.
    pub projection: Option<(Whitening, Score)>,
    pub projection_installed: bool,
    /// A projection was installed from an earlier evening and this run's own
    /// held-out numbers did not re-earn it, so it was taken back (0.12.0).
    pub projection_cleared: bool,
    /// The best prototype-aggregate arm and what it scored held out (0.12.0).
    /// The score is the arm's **fitted** one: the operating point an install
    /// would actually put the box on.
    pub aggregate: Option<(calib::Aggregate, Score)>,
    pub aggregate_installed: calib::Aggregate,
    pub aggregate_swap: bool,
    /// Every aggregate arm, incumbent included, measured under the global bar
    /// and under bars refit for it (0.12.3).
    pub aggregates: Vec<AggregateArm>,
    /// The winning arm's own thresholds — installed with it, because the pair
    /// is what the gate approved.
    pub aggregate_thresholds: Vec<VoiceThreshold>,
    /// Did the threshold proposal clear the gate?
    pub thresholds_swap: bool,
    pub projection_swap: bool,
    /// The overlap gate's curve, and whether the shipping threshold survives.
    pub gate_curve: Vec<GatePoint>,
    pub gate_shipping: Option<GatePoint>,
    pub gate_best: Option<GatePoint>,
    /// What changed, if `apply` was asked for.
    pub written: usize,
    pub cleared: usize,
    /// Why nothing could be measured, when nothing could.
    pub note: Option<String>,
}

impl Report {
    /// The JSON a client and the operations log both read. One shape, so the
    /// audit trail and the wire never disagree about what happened.
    pub fn to_json(&self) -> Value {
        json!({
            "rows": self.rows,
            "fit_rows": self.fit_rows,
            "eval_rows": self.eval_rows,
            "per_voice": self.per_voice.iter().map(|(s, f, e)| json!({
                "speaker": s, "fit": f, "held_out": e,
            })).collect::<Vec<_>>(),
            "proposed": self.proposed.iter().map(threshold_json).collect::<Vec<_>>(),
            "installed": self.installed.iter().map(|(s, t, m, n)| json!({
                "speaker": s, "threshold": t, "margin": m, "n": n,
            })).collect::<Vec<_>>(),
            "baseline": score_json(&self.baseline),
            "candidate": score_json(&self.candidate),
            "projection": self.projection.as_ref().map(|(w, s)| json!({
                "shrinkage": w.shrinkage,
                "power": w.power,
                "centred": w.centre,
                "score": score_json(s),
            })),
            "projection_installed": self.projection_installed,
            "projection_cleared": self.projection_cleared,
            "aggregate": self.aggregate.as_ref().map(|(a, s)| json!({
                "rule": a.as_str(),
                "score": score_json(s),
            })),
            "aggregate_installed": self.aggregate_installed.as_str(),
            "aggregate_swap": self.aggregate_swap,
            "aggregates": self.aggregates.iter().map(|a| json!({
                "rule": a.rule.as_str(),
                "incumbent": a.incumbent,
                "globals": score_json(&a.globals),
                "fitted": score_json(&a.fitted),
                "thresholds": a.thresholds.iter().map(threshold_json).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "aggregate_thresholds": self.aggregate_thresholds.iter()
                .map(threshold_json).collect::<Vec<_>>(),
            "thresholds_swap": self.thresholds_swap,
            "projection_swap": self.projection_swap,
            "gate": {
                "shipping": self.gate_shipping.map(gate_json),
                "best": self.gate_best.map(gate_json),
                "curve": self.gate_curve.iter().copied().map(gate_json).collect::<Vec<_>>(),
            },
            "written": self.written,
            "cleared": self.cleared,
            "note": self.note,
        })
    }
}

fn threshold_json(v: &VoiceThreshold) -> Value {
    json!({
        "speaker": v.speaker_id,
        "threshold": v.threshold,
        "margin": v.margin,
        "n": v.n,
        "f_beta": v.f_beta,
        "f_beta_global": v.f_beta_global,
    })
}

fn score_json(s: &Score) -> Value {
    json!({
        "n": s.n,
        "correct": s.correct,
        "wrong": s.wrong,
        "declined": s.declined,
        "precision": finite(s.precision()),
        "recall": finite(s.recall()),
        "f_beta": finite(s.f_beta(calib::BETA)),
    })
}

fn gate_json(p: GatePoint) -> Value {
    json!({
        "threshold": p.threshold,
        "caught": p.caught,
        "missed": p.missed,
        "false_refusals": p.false_refusals,
        "passed": p.passed,
        "precision": finite(p.precision()),
        "recall": finite(p.recall()),
        "f_beta": finite(p.f_beta()),
    })
}

/// NaN is not a number JSON can carry, and `null` is the honest rendering of
/// "there was nothing to divide by".
fn finite(v: f64) -> Option<f64> {
    v.is_finite().then_some(v)
}

/// The bank, once, with each prototype's source segment kept so a replay can
/// drop the ones a row produced itself.
type Bank = Vec<(i64, Option<i64>, Embedding)>;

/// Rank and decide one row against the bank, minus its own prototypes.
fn replay(
    cfg: &IdentityConfig,
    thresholds: &Thresholds,
    bank: &Bank,
    row: &CalibrationRow,
    proj: Option<&Projection>,
    agg: calib::Aggregate,
) -> Result<(Option<i64>, Option<Obs>)> {
    let probe = match proj {
        None => row.embedding.clone(),
        Some(p) => p.apply(&row.embedding)?,
    };
    let live: Vec<(i64, Embedding)> = bank
        .iter()
        .filter(|(_, src, _)| *src != Some(row.segment_id))
        .map(|(sp, _, e)| (*sp, e.clone()))
        .collect();
    let ranked = crate::identity::rank_with(&probe, &live, agg)?;
    let obs = ranked.first().map(|top| Obs {
        top: top.speaker_id,
        score: top.score,
        margin: top.score - ranked.get(1).map(|c| c.score).unwrap_or(f32::NEG_INFINITY),
        truth: row.truth_speaker_id,
    });
    let label = match crate::identity::decide_with(
        cfg,
        thresholds,
        row.overlap_frac,
        row.duration_s,
        row.words,
        &ranked,
    ) {
        crate::identity::Decision::Matched { speaker_id, .. }
        | crate::identity::Decision::Pinned { speaker_id } => Some(speaker_id),
        // A mint is a decline against ground truth, not a wrong answer:
        // "nobody in the bank" is a different claim from "this person".
        _ => None,
    };
    Ok((label, obs))
}

/// Score a set of rows under one operating point.
fn judge(
    cfg: &IdentityConfig,
    thresholds: &Thresholds,
    bank: &Bank,
    rows: &[&CalibrationRow],
    proj: Option<&Projection>,
    you: Option<i64>,
    agg: calib::Aggregate,
) -> Result<Score> {
    let mut s = Score::default();
    for row in rows {
        // The user's own account is not ground truth about audio captured
        // from the user's own Discord client (0.10.1): a client does not play
        // your microphone back to you.
        if Some(row.truth_speaker_id) == you {
            continue;
        }
        let (label, _) = replay(cfg, thresholds, bank, row, proj, agg)?;
        s.add(label, row.truth_speaker_id);
    }
    Ok(s)
}

/// Project a whole bank once. Doing it per row would be the same matrix
/// multiply a hundred times over.
fn project_bank(p: &Projection, bank: &Bank) -> Result<Bank> {
    bank.iter()
        .map(|(sp, src, e)| Ok((*sp, *src, p.apply(e)?)))
        .collect()
}

/// Run the pass. `apply` is the only thing that separates the preview from the
/// write; every measurement happens either way.
pub fn calibrate(
    store: &Store,
    cfg: &IdentityConfig,
    apply: bool,
    now_utc_ns: i64,
) -> Result<Report> {
    let mut report = Report {
        installed: store
            .learned_thresholds()?
            .into_iter()
            .map(|r| (r.speaker_id, r.threshold, r.margin, r.n))
            .collect(),
        projection_installed: store.installed_projection()?.is_some(),
        ..Report::default()
    };

    // ---- step 3 first: it needs no embeddings and no bank ----------------
    {
        let rows = store.truth_overlap_rows_in_order()?;
        let times: Vec<i64> = rows.iter().map(|r| r.0).collect();
        let cut = calib::split_at(&times, FIT_FRACTION).min(rows.len());
        let pairs: Vec<(bool, f32)> = rows.iter().map(|r| (r.1, r.2)).collect();
        let (fit, eval) = pairs.split_at(cut);
        let grid = calib::overlap_grid();
        report.gate_curve = calib::overlap_curve(eval, &grid);
        // The candidate is chosen on the FIT split and only then looked up in
        // the held-out curve — choosing it from the held-out curve would be
        // reading the answer off the exam.
        let fit_best = calib::overlap_curve(fit, &grid)
            .into_iter()
            .max_by(|a, b| a.f_beta().partial_cmp(&b.f_beta()).unwrap());
        report.gate_shipping = report
            .gate_curve
            .iter()
            .find(|p| (p.threshold - cfg.max_overlap).abs() < 1e-6)
            .copied();
        report.gate_best = fit_best.and_then(|b| {
            report
                .gate_curve
                .iter()
                .find(|p| (p.threshold - b.threshold).abs() < 1e-6)
                .copied()
        });
    }

    let rows = store.truth_calibration_rows(MIN_DURATION_S)?;
    report.rows = rows.len();
    if rows.len() < 2 {
        report.note = Some("not enough truth rows to split".into());
        return Ok(report);
    }
    let model_id = rows[0].embedding.model_id.clone();
    let bank: Bank = store.prototypes_with_source(&model_id)?;
    let you = store.you_speaker_id()?;

    let times: Vec<i64> = rows.iter().map(|r| r.t_start_ns).collect();
    let cut = calib::split_at(&times, FIT_FRACTION).min(rows.len());
    let fit: Vec<&CalibrationRow> = rows[..cut].iter().collect();
    let eval: Vec<&CalibrationRow> = rows[cut..].iter().collect();
    report.fit_rows = fit.len();
    report.eval_rows = eval.len();
    {
        use std::collections::BTreeMap;
        let mut per: BTreeMap<i64, (usize, usize)> = BTreeMap::new();
        for (i, r) in rows.iter().enumerate() {
            let e = per.entry(r.truth_speaker_id).or_default();
            if i < cut { e.0 += 1 } else { e.1 += 1 }
        }
        report.per_voice = per.into_iter().map(|(k, (f, e))| (k, f, e)).collect();
    }
    if eval.is_empty() {
        report.note = Some("every truth row shares one instant; nothing is held out".into());
        return Ok(report);
    }

    let global = (cfg.label_threshold, 0.0);
    let globals = Thresholds::global(global.0, global.1);
    // Every arm below is measured under the aggregate the daemon uses *right
    // now*, so "would this be better?" means better than what the user has,
    // not better than a rule nothing is running.
    let base_agg = store.learned_aggregate()?;
    report.aggregate_installed = base_agg;

    // ---- step 1: per-voice thresholds ------------------------------------

    let fit_obs: Vec<Obs> = fit
        .iter()
        .filter(|r| crate::identity::gate(cfg, r.overlap_frac, r.duration_s).is_none())
        .filter_map(|r| {
            replay(cfg, &globals, &bank, r, None, base_agg)
                .ok()
                .and_then(|x| x.1)
        })
        .collect();
    report.proposed = calib::fit_thresholds(&fit_obs, MIN_ROWS_PER_VOICE, THRESHOLD_BOUNDS, global);
    let mut candidate = Thresholds::global(global.0, global.1);
    for v in &report.proposed {
        candidate.insert(v.speaker_id, v.threshold, v.margin);
    }

    report.baseline = judge(cfg, &globals, &bank, &eval, None, you, base_agg)?;
    report.candidate = judge(cfg, &candidate, &bank, &eval, None, you, base_agg)?;
    // Safe AND worth it: `swap_is_safe` vetoes a precision loss,
    // `improvement_is_material` refuses to move the operating point for a row
    // or two. Both, or nothing changes.
    report.thresholds_swap =
        !report.proposed.is_empty() && calib::may_install(&report.baseline, &report.candidate);

    // ---- step 2: a learned projection ------------------------------------

    // The whitening is chosen on an INNER split of the fit split. Choosing it
    // on the held-out rows would be the same leak the protocol exists to stop,
    // dressed up as a hyperparameter.
    let inner_times: Vec<i64> = fit.iter().map(|r| r.t_start_ns).collect();
    let inner_cut = calib::split_at(&inner_times, FIT_FRACTION).min(fit.len());
    if inner_cut < fit.len() {
        let (inner_fit, inner_eval) = fit.split_at(inner_cut);
        let inner_labelled: Vec<Labelled> = inner_fit
            .iter()
            .map(|r| Labelled {
                class: r.truth_speaker_id,
                v: r.embedding.vector.clone(),
            })
            .collect();
        let inner_base = judge(cfg, &globals, &bank, inner_eval, None, you, base_agg)?;
        let mut best: Option<(f64, Whitening)> = None;
        for w in whitening_grid() {
            let Ok(p) = calib::fit_projection(&model_id, &inner_labelled, w) else {
                continue;
            };
            let Ok(pb) = project_bank(&p, &bank) else {
                continue;
            };
            let s = judge(cfg, &globals, &pb, inner_eval, Some(&p), you, base_agg)?;
            let f = s.f_beta(calib::BETA);
            // The inner baseline has to be beaten before a setting is even a
            // candidate: "the least bad whitening" is not a reason to whiten.
            if f > inner_base.f_beta(calib::BETA) + 1e-9
                && best.as_ref().is_none_or(|(bf, _)| f > *bf)
            {
                best = Some((f, w));
            }
        }
        if let Some((_, w)) = best {
            let all: Vec<Labelled> = fit
                .iter()
                .map(|r| Labelled {
                    class: r.truth_speaker_id,
                    v: r.embedding.vector.clone(),
                })
                .collect();
            if let Ok(p) = calib::fit_projection(&model_id, &all, w) {
                let pb = project_bank(&p, &bank)?;
                let s = judge(cfg, &globals, &pb, &eval, Some(&p), you, base_agg)?;
                report.projection_swap = calib::may_install(&report.baseline, &s);
                report.projection = Some((w, s));
                if apply && report.projection_swap {
                    let version = now_utc_ns;
                    store.install_projection(&p, version, now_utc_ns)?;
                }
            }
        }
    }

    // A projection installed on some earlier evening that TONIGHT's held-out
    // numbers do not re-earn does not get to stay (0.12.0).
    //
    // The pass used only ever to *write* projections, and the asymmetry was a
    // live bug: one evening's `--apply` installed a whitening fitted from 240
    // rows over three voices, every later run measured it, refused it, and
    // left it in the table anyway. The daemon went on labelling every turn in
    // a space its own numbers called worse — twenty-one correct labels and
    // 4.5 pp of held-out recall, gone, silently (§32 step 0). Thresholds have
    // had this rule since 0.11.0: a learned value nothing re-derived tonight
    // is a value nothing stands behind.
    //
    // Only when the pass actually measured something. A run that could not
    // split, or could not fit a candidate at all, has no verdict to refuse it
    // with, and absence of evidence is not refusal.
    if apply && report.projection_installed && !report.projection_swap {
        report.projection_cleared = store.clear_projection()?;
    }

    // ---- step 4: how a voice's prototypes become one score (0.12.0) --------

    // Measured last and installed independently, because it is a different
    // kind of claim from the two above: not a number fitted to this install
    // but a choice between rules, each of which is scored end to end on the
    // held-out rows exactly as the incumbent is. No hyperparameter search, so
    // no inner split — the grid IS the arms, and every one of them is judged
    // on the same rows by the same gate.
    //
    // Two things 0.12.3 changed, both of them corrections rather than features
    // (§45):
    //
    // * **Every arm refits its own thresholds.** A bar is a number on a score
    //   scale and the aggregate *is* the scale, so scoring a top-3 mean against
    //   bars fitted under max measures the scale and not the rule. That is §36's
    //   error, and it was still here: the arms were compared under the globals
    //   while the box ran per-voice bars, and on the live install the two
    //   comparisons do not agree.
    // * **The incumbent is an arm too.** The loop used to `continue` past the
    //   installed rule, so its own operating point was never in the table the
    //   report prints, and the thing the winner was compared against was the
    //   globals row rather than what the box is actually doing.
    {
        let mut arms: Vec<AggregateArm> = Vec::new();
        let mut grid = calib::aggregate_grid();
        if !grid.contains(&base_agg) {
            grid.insert(0, base_agg);
        }
        for a in grid {
            let (g, f, thresholds) = if a == base_agg {
                // Step 1 measured exactly this pair. Re-deriving it would be a
                // second answer to one question.
                (report.baseline, report.candidate, report.proposed.clone())
            } else {
                let obs: Vec<Obs> = fit
                    .iter()
                    .filter(|r| crate::identity::gate(cfg, r.overlap_frac, r.duration_s).is_none())
                    .filter_map(|r| {
                        replay(cfg, &globals, &bank, r, None, a)
                            .ok()
                            .and_then(|x| x.1)
                    })
                    .collect();
                let props =
                    calib::fit_thresholds(&obs, MIN_ROWS_PER_VOICE, THRESHOLD_BOUNDS, global);
                let mut t = Thresholds::global(global.0, global.1);
                for v in &props {
                    t.insert(v.speaker_id, v.threshold, v.margin);
                }
                (
                    judge(cfg, &globals, &bank, &eval, None, you, a)?,
                    judge(cfg, &t, &bank, &eval, None, you, a)?,
                    props,
                )
            };
            arms.push(AggregateArm {
                rule: a,
                globals: g,
                fitted: f,
                thresholds,
                incumbent: a == base_agg,
            });
        }
        // The comparison that decides: the challenger's own operating point
        // against the incumbent's own operating point.
        let incumbent = arms
            .iter()
            .find(|x| x.incumbent)
            .map(|x| x.fitted)
            .unwrap_or(report.baseline);
        let best = arms
            .iter()
            .filter(|x| !x.incumbent)
            .max_by(|x, y| {
                x.fitted
                    .f_beta(calib::BETA)
                    .partial_cmp(&y.fitted.f_beta(calib::BETA))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .cloned();
        if let Some(b) = best {
            report.aggregate_swap = calib::may_install(&incumbent, &b.fitted);
            report.aggregate = Some((b.rule, b.fitted));
            report.aggregate_thresholds = b.thresholds.clone();
            if apply && report.aggregate_swap {
                store.set_learned_aggregate(b.rule)?;
            }
        }
        report.aggregates = arms;
    }

    // ---- write, if the gate said so --------------------------------------

    if apply && report.aggregate_swap {
        // A threshold is a number on a score scale, and the scale just moved.
        // Every learned bar in the table was fitted against the *old* rule for
        // turning a voice's prototypes into one score, and against the new one
        // it means something nobody measured — 0.41 under max cosine and 0.41
        // under a top-3 mean are not the same operating point. So they go.
        //
        // Until 0.12.3 that was the whole rule, and the next pass refitted:
        // one run changed the scale, the next calibrated to it. The gap was
        // real. What the gate approves is a **pair** — this rule with these
        // bars, measured together on the held-out rows — and installing half
        // of it left the box on an operating point nothing had measured until
        // the following evening. The winning arm now brings its own thresholds
        // with it, and they are the ones the gate read (§45).
        report.cleared = store.clear_learned_thresholds(None)?;
        for v in &report.aggregate_thresholds {
            if store.set_learned_threshold(
                v.speaker_id,
                v.threshold,
                v.margin,
                truth_via::LEARNED,
                v.n as i64,
                now_utc_ns,
            )? {
                report.written += 1;
            }
        }
    } else if apply && report.thresholds_swap {
        let keep: Vec<i64> = report.proposed.iter().map(|v| v.speaker_id).collect();
        // Every voice not in the proposal goes back to the global. A learned
        // value nothing re-derived tonight is a value nothing stands behind.
        let stale: Vec<i64> = report
            .installed
            .iter()
            .map(|(id, ..)| *id)
            .filter(|id| !keep.contains(id))
            .collect();
        report.cleared = store.clear_learned_thresholds(Some(&stale))?;
        for v in &report.proposed {
            if store.set_learned_threshold(
                v.speaker_id,
                v.threshold,
                v.margin,
                truth_via::LEARNED,
                v.n as i64,
                now_utc_ns,
            )? {
                report.written += 1;
            }
        }
    }
    // An `operations` row is a record that something CHANGED. The third term
    // used to be `report.projection_installed`, which is set at the top of this
    // function from `store.installed_projection()` — "a projection is already
    // there", a fact about the past. Once any evening had installed one, every
    // later run logged an operation whose whole content was "nothing was
    // written, nothing was cleared, nothing was swapped": a six-hourly entry in
    // the audit trail for a pass that did not touch the database. The term that
    // means "this run installed a projection" is `projection_swap`, which is
    // also the condition guarding the `install_projection` call above.
    if apply
        && (report.written > 0
            || report.cleared > 0
            || report.projection_swap
            || report.projection_cleared
            || report.aggregate_swap)
    {
        let targets: Vec<i64> = report.proposed.iter().map(|v| v.speaker_id).collect();
        store.log_operation(
            OP,
            &json!(targets).to_string(),
            &report.to_json().to_string(),
            now_utc_ns,
        )?;
    }
    Ok(report)
}

// ---- 0.12.0: `recalld identity repair --prototypes` -------------------------

/// The `operations` op the repair writes its before/after table under.
pub const REPAIR_OP: &str = "identity.repair";

/// What one repair run found, whether or not it removed anything.
#[derive(Debug, Clone, Default)]
pub struct RepairReport {
    pub condemned: Vec<crate::store::CondemnedPrototype>,
    pub deleted: usize,
    /// Held-out identity, before and after the removal, measured exactly as
    /// [`calibrate`] measures everything else. `None` when there was not
    /// enough truth to split — in which case the command reports what it
    /// found and says it could not measure it, rather than guessing.
    pub measured: Option<(Score, Score)>,
    pub note: Option<String>,
}

impl RepairReport {
    pub fn to_json(&self) -> Value {
        json!({
            "condemned": self.condemned.iter().map(|c| json!({
                "prototype": c.prototype_id,
                "owner": c.owner,
                "owner_name": c.owner_name,
                "truth_speaker": c.truth_speaker,
                "truth_name": c.truth_name,
                "segment": c.source_segment_id,
                "coverage": c.coverage,
            })).collect::<Vec<_>>(),
            "deleted": self.deleted,
            "before": self.measured.as_ref().map(|(b, _)| score_json(b)),
            "after": self.measured.as_ref().map(|(_, a)| score_json(a)),
            "note": self.note,
        })
    }
}

/// Remove prototypes ground truth says are recordings of somebody else.
///
/// ## Why this is not part of the nightly pass
///
/// [`calibrate`] fits *values* — a threshold, a matrix, a rule — and the
/// installation gate it uses ([`calib::may_install`]) exists to stop a
/// six-hourly job moving the operating point for a row or two. This is not
/// that. Nothing here is fitted; a prototype whose own source turn Discord
/// says was a different person is simply wrong, and the check has no
/// parameter to overfit. On this install the removal clears the precision veto
/// and misses the nightly materiality bar (§32 step 2: precision +0.3 pp,
/// recall +1.8 pp against a bar of 2.0, wrong 13 → 12), and the honest answer
/// to that is not to lower the bar — it is to notice that an operator running
/// a deliberate, previewed, audited repair is making a different decision from
/// a cron job, and to let them make it. So: **operator-run, preview by
/// default, never automatic.**
///
/// The before/after table is measured anyway and printed, because a command
/// that deletes from the voicebank is not allowed to be a leap of faith.
pub fn repair_prototypes(
    store: &Store,
    cfg: &IdentityConfig,
    apply: bool,
    now_utc_ns: i64,
) -> Result<RepairReport> {
    let mut report = RepairReport::default();
    let rows = store.truth_calibration_rows(MIN_DURATION_S)?;
    let Some(model_id) = rows
        .first()
        .map(|r| r.embedding.model_id.clone())
        .or(store.embed_model_ids()?.into_iter().next())
    else {
        report.note = Some("no embeddings, so no bank to repair".into());
        return Ok(report);
    };
    report.condemned = store.condemned_prototypes(&model_id)?;

    // Measure before deleting, so a preview and an apply print the same table.
    // The operating point is the *install's*, not the crate defaults: this
    // install's `max_overlap` is 0.06 against a default of 0.1, and a table
    // measured at the default describes a machine nobody is running (§45).
    let you = store.you_speaker_id()?;
    let bank: Bank = store.prototypes_with_source(&model_id)?;
    let agg = store.learned_aggregate()?;
    let thresholds = store.threshold_table((cfg.label_threshold, 0.0))?;
    let times: Vec<i64> = rows.iter().map(|r| r.t_start_ns).collect();
    let cut = calib::split_at(&times, FIT_FRACTION).min(rows.len());
    if cut < rows.len() {
        let doomed: Vec<i64> = report.condemned.iter().map(|c| c.prototype_id).collect();
        // The one honesty rule this measurement needs: a held-out row whose
        // own verdict is what condemned a prototype must not be scored, or the
        // repair is being graded on the answer it was handed. It is not the
        // chronological rule the fitted arms use, because nothing here is
        // fitted — it is the same rule as "no row is scored against a
        // prototype it produced itself", one step further out.
        let consumed: Vec<i64> = report
            .condemned
            .iter()
            .map(|c| c.source_segment_id)
            .collect();
        let eval: Vec<&CalibrationRow> = rows[cut..]
            .iter()
            .filter(|r| !consumed.contains(&r.segment_id))
            .collect();
        let repaired: Bank = store.prototypes_with_source_excluding(&model_id, &doomed)?;
        let before = judge(cfg, &thresholds, &bank, &eval, None, you, agg)?;
        let after = judge(cfg, &thresholds, &repaired, &eval, None, you, agg)?;
        report.measured = Some((before, after));
    } else {
        report.note = Some("not enough truth to hold anything out".into());
    }

    if apply && !report.condemned.is_empty() {
        let ids: Vec<i64> = report.condemned.iter().map(|c| c.prototype_id).collect();
        report.deleted = store.delete_prototypes(&ids)?;
        store.log_operation(
            REPAIR_OP,
            &json!(report.condemned.iter().map(|c| c.owner).collect::<Vec<_>>()).to_string(),
            &report.to_json().to_string(),
            now_utc_ns,
        )?;
    }
    Ok(report)
}

/// `recalld identity calibrate --reset`: back to the globals, everywhere.
pub fn reset(store: &Store, now_utc_ns: i64) -> Result<(usize, bool)> {
    let cleared = store.clear_learned_thresholds(None)?;
    let dropped = store.clear_projection()?;
    let un_aggregated = store.clear_learned_aggregate()?;
    if cleared > 0 || dropped || un_aggregated {
        store.log_operation(
            OP,
            "[]",
            &json!({
                "reset": true,
                "cleared": cleared,
                "projection": dropped,
                "aggregate": un_aggregated,
            })
            .to_string(),
            now_utc_ns,
        )?;
    }
    Ok((cleared, dropped))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_with_nothing_in_it_still_renders() {
        let r = Report::default();
        let j = r.to_json();
        assert_eq!(j["rows"], 0);
        assert_eq!(j["thresholds_swap"], false);
        // NaN would make this `null`, not a number that lies.
        assert!(j["baseline"]["precision"].is_null());
    }

    #[test]
    fn the_report_says_what_it_would_change_before_anything_is_written() {
        let mut r = Report::default();
        r.proposed.push(VoiceThreshold {
            speaker_id: 7,
            threshold: 0.52,
            margin: 0.04,
            n: 61,
            f_beta: 0.91,
            f_beta_global: 0.88,
        });
        let j = r.to_json();
        assert_eq!(j["proposed"][0]["speaker"], 7);
        assert_eq!(j["proposed"][0]["n"], 61);
        assert_eq!(j["written"], 0);
    }

    use crate::store::{Store, truth_verdict};

    /// A store with two linked voices, a prototype each, and `n` truth turns
    /// apiece — enough shape for the pass to run end to end.
    ///
    /// The vectors are deliberately *easy*: each person's turns sit right on
    /// their own prototype. A fit on data this clean has nothing to buy, which
    /// is exactly the case the gate has to refuse.
    fn a_truthful_store(n: usize) -> Store {
        let s = Store::open_in_memory().unwrap();
        let src = s.upsert_source("Discord", "Discord", 1).unwrap();
        let sess = s.begin_session(src, 0).unwrap();
        let sec = 1_000_000_000i64;
        let people = [("u1", vec![1.0f32, 0.0, 0.0]), ("u2", vec![0.0, 1.0, 0.0])];
        for (i, (user, centre)) in people.iter().enumerate() {
            let sp = s.mint_speaker(0).unwrap();
            s.upsert_discord_user(user, user, 0).unwrap();
            s.set_discord_link(user, Some(sp), Some(truth_via::MANUAL), 0)
                .unwrap();
            s.add_prototype(
                sp,
                &Embedding::new("m@1", centre.clone()),
                None,
                false,
                20,
                0,
            )
            .unwrap();
            for k in 0..n {
                let t = ((i * n + k) as i64 + 1) * 10 * sec;
                let seg = s.insert_segment(sess, t, t + 5 * sec, "a.wav", 0).unwrap();
                let mut v = centre.clone();
                // A little wobble, so the classes are clouds and not points.
                v[2] = (k % 7) as f32 * 0.01;
                s.store_embedding(seg, &Embedding::new("m@1", v)).unwrap();
                s.set_segment_truth(seg, Some(user), "single", Some(0.95))
                    .unwrap();
            }
        }
        s
    }

    #[test]
    fn an_empty_store_reports_rather_than_dividing_by_zero() {
        let s = Store::open_in_memory().unwrap();
        let r = calibrate(&s, &IdentityConfig::default(), true, 1).unwrap();
        assert_eq!(r.rows, 0);
        assert!(r.note.is_some());
        assert_eq!(r.written, 0);
        assert!(s.learned_thresholds().unwrap().is_empty());
    }

    #[test]
    fn a_fit_with_nothing_to_buy_writes_nothing_even_with_apply() {
        // The rule this round turns on: `--apply` is permission to install
        // what cleared the gate, not permission to install.
        let s = a_truthful_store(60);
        let r = calibrate(&s, &IdentityConfig::default(), true, 1).unwrap();
        assert!(r.rows >= 100, "{} truth rows", r.rows);
        assert!(r.eval_rows > 0);
        assert!(!r.thresholds_swap, "nothing here is worth changing");
        assert_eq!(r.written, 0);
        assert!(s.learned_thresholds().unwrap().is_empty());
        assert!(s.installed_projection().unwrap().is_none());
        // And no operation was logged, because nothing happened.
        assert!(s.operations_of(OP, 10).unwrap().is_empty());
    }

    #[test]
    fn a_run_that_changes_nothing_logs_nothing() {
        // Tonight's fit has nothing to buy — the fixture's classes sit on
        // their own prototypes — so nothing is written, nothing is cleared, no
        // projection is swapped in and no aggregate is installed.
        let s = a_truthful_store(60);
        let r = calibrate(&s, &IdentityConfig::default(), true, 2).unwrap();
        assert_eq!(r.written, 0);
        assert_eq!(r.cleared, 0);
        assert!(!r.thresholds_swap);
        assert!(!r.projection_swap);
        assert!(!r.projection_cleared);
        assert!(!r.aggregate_swap);

        // The pass runs every six hours. A row here writes an entry in the
        // audit trail per evening recording a change that did not happen.
        assert!(
            s.operations_of(OP, 10).unwrap().is_empty(),
            "a pass that changed nothing must leave no trace in the audit trail"
        );

        // Twice more, to make the point that it is per-run and not a one-off.
        calibrate(&s, &IdentityConfig::default(), true, 3).unwrap();
        calibrate(&s, &IdentityConfig::default(), true, 4).unwrap();
        assert!(s.operations_of(OP, 10).unwrap().is_empty());
    }

    #[test]
    fn the_split_is_chronological_and_holds_something_back() {
        let s = a_truthful_store(50);
        let r = calibrate(&s, &IdentityConfig::default(), false, 1).unwrap();
        assert_eq!(r.fit_rows + r.eval_rows, r.rows);
        assert!(r.fit_rows > r.eval_rows, "{} / {}", r.fit_rows, r.eval_rows);
        assert!(r.eval_rows > 0);
    }

    #[test]
    fn a_preview_never_writes() {
        let s = a_truthful_store(60);
        let before = s.learned_thresholds().unwrap();
        calibrate(&s, &IdentityConfig::default(), false, 1).unwrap();
        assert_eq!(s.learned_thresholds().unwrap(), before);
    }

    #[test]
    fn a_reset_puts_every_voice_back_and_says_how_many() {
        let s = a_truthful_store(4);
        let a = s.mint_speaker(0).unwrap();
        s.set_learned_threshold(a, 0.5, 0.02, truth_via::LEARNED, 40, 1)
            .unwrap();
        let (cleared, dropped) = reset(&s, 2).unwrap();
        assert_eq!(cleared, 1);
        assert!(!dropped);
        assert!(s.learned_thresholds().unwrap().is_empty());
        // The reset that had nothing to do logs nothing.
        assert_eq!(s.operations_of(OP, 10).unwrap().len(), 1);
        assert_eq!(reset(&s, 3).unwrap(), (0, false));
        assert_eq!(s.operations_of(OP, 10).unwrap().len(), 1);
    }

    /// A store where one voice in the bank is a **persistent imposter**: an
    /// unlinked row whose prototype outscores the right answer on one person's
    /// turns, at 0.40 against the global bar of 0.35. Raising that one voice's
    /// bar turns every one of those wrong names into a decline, which is
    /// precisely the trade the gate exists to approve.
    fn a_store_with_an_imposter(per_person: usize) -> Store {
        let s = Store::open_in_memory().unwrap();
        let src = s.upsert_source("Discord", "Discord", 1).unwrap();
        let sess = s.begin_session(src, 0).unwrap();
        let sec = 1_000_000_000i64;
        let at = |deg: f64| -> Embedding {
            let r = deg.to_radians();
            Embedding::new("m@1", vec![r.cos() as f32, r.sin() as f32])
        };

        let a = s.mint_speaker(0).unwrap();
        let b = s.mint_speaker(0).unwrap();
        let imposter = s.mint_speaker(0).unwrap();
        s.upsert_discord_user("ua", "ua", 0).unwrap();
        s.upsert_discord_user("ub", "ub", 0).unwrap();
        s.set_discord_link("ua", Some(a), Some(truth_via::MANUAL), 0)
            .unwrap();
        s.set_discord_link("ub", Some(b), Some(truth_via::MANUAL), 0)
            .unwrap();
        // A's turns sit at 0 degrees and A's prototype is right beside them.
        s.add_prototype(a, &at(5.0), None, false, 20, 0).unwrap();
        // B's turns sit at 90 degrees. B's own prototype is 69.5 degrees off
        // them (cosine 0.35, exactly the global bar) and the imposter's is
        // 66.4 degrees off (cosine 0.40), so the imposter wins every one.
        s.add_prototype(b, &at(159.5), None, false, 20, 0).unwrap();
        s.add_prototype(imposter, &at(156.42), None, false, 20, 0)
            .unwrap();

        // Interleaved in time, so the chronological split sees both people on
        // each side of the cut.
        for k in 0..per_person {
            for (user, deg) in [("ua", 0.0), ("ub", 90.0)] {
                let t = ((k * 2) as i64 + i64::from(user == "ub") + 1) * 10 * sec;
                let seg = s.insert_segment(sess, t, t + 5 * sec, "a.wav", 0).unwrap();
                // A wobble far smaller than the 0.01 threshold grid.
                s.store_embedding(seg, &at(deg + (k % 5) as f64 * 0.05))
                    .unwrap();
                s.set_segment_truth(seg, Some(user), "single", Some(0.95))
                    .unwrap();
            }
        }
        s
    }

    #[test]
    fn a_fit_that_removes_wrong_names_installs_and_logs_what_it_did() {
        let s = a_store_with_an_imposter(60);
        let cfg = IdentityConfig::default();

        // Before: the imposter takes every one of B's turns.
        let preview = calibrate(&s, &cfg, false, 1).unwrap();
        assert!(preview.baseline.wrong > 0, "{:?}", preview.baseline);
        assert!(
            preview.thresholds_swap,
            "the gate should approve this: {:?} -> {:?}",
            preview.baseline, preview.candidate
        );
        assert_eq!(preview.written, 0, "a preview writes nothing");

        let r = calibrate(&s, &cfg, true, 42).unwrap();
        assert!(r.written > 0);
        assert!(r.candidate.wrong < r.baseline.wrong);
        assert!(r.candidate.precision() > r.baseline.precision());

        // The learned value is on the imposter, with its provenance.
        let learned = s.learned_thresholds().unwrap();
        assert_eq!(learned.len(), r.written);
        let row = &learned[0];
        assert_eq!(row.via, truth_via::LEARNED);
        assert!(row.threshold > 0.40, "threshold was {}", row.threshold);
        assert!(row.n >= crate::calib::MIN_ROWS_PER_VOICE as i64);
        assert_eq!(row.at_ns, 42);

        // And the before/after table is in the audit trail.
        let ops = s.operations_of(OP, 10).unwrap();
        assert_eq!(ops.len(), 1);
        assert!(ops[0].prior_state.contains("\"baseline\""));

        // The ladder now reads the learned bar for that voice and the global
        // for everyone else.
        let table = s.threshold_table((cfg.label_threshold, 0.0)).unwrap();
        assert_eq!(table.len(), r.written);
        assert!(table.for_speaker(row.speaker_id).0 > 0.40);
    }

    #[test]
    fn a_second_run_takes_back_a_value_the_evidence_no_longer_supports() {
        let s = a_store_with_an_imposter(60);
        let cfg = IdentityConfig::default();
        calibrate(&s, &cfg, true, 1).unwrap();
        let learned = s.learned_thresholds().unwrap();
        assert!(!learned.is_empty());

        // A voice with a learned value that this evening's fit does not
        // re-derive goes back to the global: a number nothing stands behind is
        // worse than no number.
        let stray = s.mint_speaker(0).unwrap();
        s.set_learned_threshold(stray, 0.59, 0.0, truth_via::LEARNED, 99, 1)
            .unwrap();
        let r = calibrate(&s, &cfg, true, 2).unwrap();
        assert!(r.cleared >= 1);
        assert!(
            s.learned_thresholds()
                .unwrap()
                .iter()
                .all(|l| l.speaker_id != stray)
        );
    }

    // ---- 0.12.0 -----------------------------------------------------------

    #[test]
    fn a_projection_tonights_evidence_refuses_does_not_stay_installed() {
        // The bug §32 found on the live box. `--apply` installed a projection
        // one evening from 240 rows over three voices; every later run
        // measured it, refused it — and left it in the table, because the pass
        // only ever *wrote* projections. The daemon then labelled every turn
        // in a space its own held-out numbers said was worse.
        let s = a_truthful_store(60);
        let mut p = crate::calib::Projection::identity("m@1", 3);
        p.a[0] = 12.0; // not the identity: a map that actually changes scores
        s.install_projection(&p, 1, 1).unwrap();

        let r = calibrate(&s, &IdentityConfig::default(), true, 2).unwrap();
        assert!(!r.projection_swap, "tonight's fit does not re-earn it");
        assert!(r.projection_cleared, "so it must not survive the run");
        assert!(s.installed_projection().unwrap().is_none());
        // Un-installing IS a change, so unlike a run that did nothing it
        // leaves a row in the audit trail saying so.
        assert_eq!(s.operations_of(OP, 10).unwrap().len(), 1);
    }

    #[test]
    fn a_pass_that_cannot_measure_leaves_a_projection_alone() {
        // Absence of evidence is not refusal. With nothing held out there is
        // no number to refuse it with, and dropping it would be a change made
        // for no reason.
        let s = Store::open_in_memory().unwrap();
        let p = crate::calib::Projection::identity("m@1", 3);
        s.install_projection(&p, 1, 1).unwrap();
        let r = calibrate(&s, &IdentityConfig::default(), true, 2).unwrap();
        assert!(r.note.is_some());
        assert!(!r.projection_cleared);
        assert!(s.installed_projection().unwrap().is_some());
    }

    /// A store where one voice owns **one lucky prototype** that sits closer
    /// to another person's turns than that person's own prototypes do.
    ///
    /// Max cosine hands every one of A's turns to the lucky voice. A top-3
    /// mean asks the lucky voice's whole record to support the claim, it
    /// cannot, and A wins them back. This is the shape of the Rowan/Aspen
    /// confusion on the real install (§32 step 3).
    fn a_store_with_a_lucky_prototype(per_person: usize) -> Store {
        let s = Store::open_in_memory().unwrap();
        let src = s.upsert_source("Discord", "Discord", 1).unwrap();
        let sess = s.begin_session(src, 0).unwrap();
        let sec = 1_000_000_000i64;
        let at = |deg: f64| -> Embedding {
            let r = deg.to_radians();
            Embedding::new("m@1", vec![r.cos() as f32, r.sin() as f32])
        };
        let a = s.mint_speaker(0).unwrap();
        let b = s.mint_speaker(0).unwrap();
        for (u, sp) in [("ua", a), ("ub", b)] {
            s.upsert_discord_user(u, u, 0).unwrap();
            s.set_discord_link(u, Some(sp), Some(truth_via::MANUAL), 0)
                .unwrap();
        }
        // A talks at 0 degrees; A's own prototypes are a consistent 5-9 off.
        for d in [5.0, 7.0, 9.0] {
            s.add_prototype(a, &at(d), None, false, 20, 0).unwrap();
        }
        // B talks at 42 degrees, and B's three prototypes are all over the
        // place — including one that landed at 4 degrees, right on top of A.
        for d in [4.0, 40.0, 45.0] {
            s.add_prototype(b, &at(d), None, false, 20, 0).unwrap();
        }
        for k in 0..per_person {
            for (user, deg) in [("ua", 0.0), ("ub", 42.0)] {
                let t = ((k * 2) as i64 + i64::from(user == "ub") + 1) * 10 * sec;
                let seg = s.insert_segment(sess, t, t + 5 * sec, "a.wav", 0).unwrap();
                s.store_embedding(seg, &at(deg + (k % 5) as f64 * 0.05))
                    .unwrap();
                s.set_segment_truth(seg, Some(user), truth_verdict::SINGLE, Some(0.95))
                    .unwrap();
            }
        }
        s
    }

    #[test]
    fn the_pass_measures_how_a_voices_prototypes_become_one_score() {
        let s = a_store_with_a_lucky_prototype(60);
        let cfg = IdentityConfig::default();
        let r = calibrate(&s, &cfg, false, 1).unwrap();
        let (agg, score) = r.aggregate.expect("the arm was measured");
        // Which k wins is the data's business, not the test's. What is under
        // test is that asking a voice's whole record beats asking its luckiest
        // prototype; pinning the k here would make this a description of the
        // fixture rather than of the mechanism.
        assert!(
            matches!(agg, crate::calib::Aggregate::TopK(_)),
            "{:?}",
            r.aggregate
        );
        assert!(
            score.wrong < r.baseline.wrong,
            "{score:?} vs {:?}",
            r.baseline
        );
        assert!(r.aggregate_swap);
        assert_eq!(
            s.learned_aggregate().unwrap(),
            crate::calib::Aggregate::Max,
            "a preview writes nothing"
        );
    }

    #[test]
    fn an_aggregate_that_clears_the_gate_is_installed_and_logged() {
        let s = a_store_with_a_lucky_prototype(60);
        let r = calibrate(&s, &IdentityConfig::default(), true, 7).unwrap();
        assert!(r.aggregate_swap);
        assert!(matches!(
            s.learned_aggregate().unwrap(),
            crate::calib::Aggregate::TopK(_)
        ));
        let ops = s.operations_of(OP, 10).unwrap();
        assert_eq!(ops.len(), 1);
        assert!(ops[0].prior_state.contains("\"aggregate\""));
    }

    #[test]
    fn changing_the_scoring_rule_takes_back_thresholds_fitted_on_the_old_scale() {
        // 0.41 under max cosine and 0.41 under a top-3 mean are not the same
        // operating point. A run that moves the scale must not leave numbers
        // behind that were measured against the other one — every bar the box
        // ends up with has to have been fitted under the rule it ends up with.
        let s = a_store_with_a_lucky_prototype(60);
        let a = s.mint_speaker(0).unwrap();
        s.set_learned_threshold(a, 0.44, 0.02, truth_via::LEARNED, 90, 1)
            .unwrap();

        let r = calibrate(&s, &IdentityConfig::default(), true, 3).unwrap();
        assert!(r.aggregate_swap);
        assert!(r.cleared >= 1);
        let stray_survived = s
            .learned_thresholds()
            .unwrap()
            .iter()
            .any(|l| l.speaker_id == a);
        assert!(
            !stray_survived,
            "a bar fitted on the old scale must not stay"
        );
        // Whatever IS installed came from the winning arm's own refit, which is
        // the pair the held-out gate approved.
        let refit: Vec<i64> = r
            .aggregate_thresholds
            .iter()
            .map(|v| v.speaker_id)
            .collect();
        for l in s.learned_thresholds().unwrap() {
            assert!(refit.contains(&l.speaker_id), "{l:?} was not refit");
        }
        assert_eq!(r.written, refit.len());

        // And the next run calibrates to the rule that is now installed.
        let again = calibrate(&s, &IdentityConfig::default(), true, 4).unwrap();
        assert!(!again.aggregate_swap, "already there");
        assert!(matches!(
            s.learned_aggregate().unwrap(),
            crate::calib::Aggregate::TopK(_)
        ));
    }

    // ---- 0.12.3: every aggregate, with thresholds refit for it -------------

    #[test]
    fn every_aggregate_is_measured_both_ways_including_the_installed_one() {
        // The pass used to measure the other rules under the global bar only,
        // and to skip the incumbent entirely. Both halves were a scale error
        // waiting to happen (§36): a bar fitted under max is not the same
        // operating point under a top-3 mean, and an incumbent nothing measures
        // cannot be compared with anything.
        let s = a_store_with_a_lucky_prototype(60);
        let r = calibrate(&s, &IdentityConfig::default(), false, 1).unwrap();
        let rules: Vec<String> = r.aggregates.iter().map(|a| a.rule.as_str()).collect();
        assert_eq!(rules, vec!["max", "top-2", "top-3", "top-4", "top-5"]);
        assert_eq!(
            r.aggregates.iter().filter(|a| a.incumbent).count(),
            1,
            "exactly one arm is what the box runs"
        );
        for arm in &r.aggregates {
            assert_eq!(arm.globals.n, r.baseline.n, "{} globals", arm.rule.as_str());
            assert_eq!(arm.fitted.n, r.baseline.n, "{} fitted", arm.rule.as_str());
        }
        // The incumbent's two rows ARE step 1's two rows: one measurement, not
        // two that could disagree.
        let inc = r.aggregates.iter().find(|a| a.incumbent).unwrap();
        assert_eq!(inc.globals, r.baseline);
        assert_eq!(inc.fitted, r.candidate);
    }

    #[test]
    fn an_aggregate_is_judged_on_the_operating_point_it_would_install() {
        // What the gate compares is (rule + the bars fitted for that rule)
        // against (the installed rule + the bars fitted for *it*) — never a
        // candidate's score against the incumbent's bars, which is §36's error.
        let s = a_store_with_a_lucky_prototype(60);
        let r = calibrate(&s, &IdentityConfig::default(), false, 1).unwrap();
        let (rule, score) = r.aggregate.expect("an arm was measured");
        let arm = r
            .aggregates
            .iter()
            .find(|a| a.rule == rule)
            .expect("the winner is in the table");
        assert_eq!(arm.fitted, score, "the reported score is the fitted one");
        assert_eq!(arm.thresholds, r.aggregate_thresholds);
        assert!(!arm.incumbent);
        // And the winner is the best fitted arm among the challengers.
        let best = r
            .aggregates
            .iter()
            .filter(|a| !a.incumbent)
            .map(|a| a.fitted.f_beta(calib::BETA))
            .fold(f64::NEG_INFINITY, f64::max);
        assert!((score.f_beta(calib::BETA) - best).abs() < 1e-12);
    }

    /// A store shaped like the live box of 2026-09-04 22:00: a **phantom
    /// voice** minted from one person's own turns, holding three tight
    /// prototypes of them, while that person's own bank is one good prototype
    /// and two vectors merged in from elsewhere that match nobody (§44.3).
    ///
    /// Max asks "could this be them?" and A's one good prototype answers yes.
    /// A top-3 mean asks A's whole record and the record is mostly junk, so the
    /// phantom — whose three prototypes all agree — wins A's turns. This is the
    /// case where the *generous* rule is the right one, and the pass has to be
    /// able to say so and take a top-k back.
    fn a_store_with_a_phantom_voice(per_person: usize) -> Store {
        let s = Store::open_in_memory().unwrap();
        let src = s.upsert_source("Discord", "Discord", 1).unwrap();
        let sess = s.begin_session(src, 0).unwrap();
        let sec = 1_000_000_000i64;
        let at = |deg: f64| -> Embedding {
            let r = deg.to_radians();
            Embedding::new("m@1", vec![r.cos() as f32, r.sin() as f32])
        };
        let a = s.mint_speaker(0).unwrap();
        let b = s.mint_speaker(0).unwrap();
        let phantom = s.mint_speaker(0).unwrap();
        for (u, sp) in [("ua", a), ("ub", b)] {
            s.upsert_discord_user(u, u, 0).unwrap();
            s.set_discord_link(u, Some(sp), Some(truth_via::MANUAL), 0)
                .unwrap();
        }
        // A talks at 0 degrees. One prototype of A is right beside that; the
        // other two came in through a merge and are nowhere near it.
        for d in [1.0, 80.0, 85.0] {
            s.add_prototype(a, &at(d), None, false, 20, 0).unwrap();
        }
        // The phantom is three recordings of A, filed under a voice of its own.
        for d in [2.0, 3.0, 4.0] {
            s.add_prototype(phantom, &at(d), None, false, 20, 0)
                .unwrap();
        }
        for d in [89.0, 91.0, 92.0] {
            s.add_prototype(b, &at(d), None, false, 20, 0).unwrap();
        }
        for k in 0..per_person {
            for (user, deg) in [("ua", 0.0), ("ub", 90.0)] {
                let t = ((k * 2) as i64 + i64::from(user == "ub") + 1) * 10 * sec;
                let seg = s.insert_segment(sess, t, t + 5 * sec, "a.wav", 0).unwrap();
                s.store_embedding(seg, &at(deg + (k % 5) as f64 * 0.05))
                    .unwrap();
                s.set_segment_truth(seg, Some(user), truth_verdict::SINGLE, Some(0.95))
                    .unwrap();
            }
        }
        s
    }

    /// A store where the two comparisons **disagree**, which is the whole
    /// reason the arms refit.
    ///
    /// An unlinked imposter takes every one of B's turns under max cosine, and
    /// takes them cleanly: one bar on that one voice turns seventy wrong names
    /// into seventy declines. Under a top-2 mean the imposter loses B's first
    /// group and keeps the second, so it tops too few rows for a bar to be
    /// fitted at all and the wrong names stay.
    ///
    /// Read off the **global** bar, top-2 is far ahead — 0.77 against 0.46 —
    /// and the pass that only ever measured the globals installed it. Read off
    /// each rule's own operating point, max is ahead at 0.81 and top-2 costs
    /// precision. The second reading is the one that describes the box.
    fn a_store_where_the_globals_and_the_bars_disagree() -> Store {
        let s = Store::open_in_memory().unwrap();
        let src = s.upsert_source("Discord", "Discord", 1).unwrap();
        let sess = s.begin_session(src, 0).unwrap();
        let sec = 1_000_000_000i64;
        // Three orthogonal directions, so a prototype's cosine to each group of
        // turns is just its coordinate and the arithmetic above is exact.
        let at = |x: f32, y: f32, z: f32| -> Embedding {
            let slack = (1.0 - x * x - y * y - z * z).max(0.0).sqrt();
            Embedding::new("m@1", vec![x, y, z, slack])
        };
        let a = s.mint_speaker(0).unwrap();
        let b = s.mint_speaker(0).unwrap();
        let imposter = s.mint_speaker(0).unwrap();
        for (u, sp) in [("ua", a), ("ub", b)] {
            s.upsert_discord_user(u, u, 0).unwrap();
            s.set_discord_link(u, Some(sp), Some(truth_via::MANUAL), 0)
                .unwrap();
        }
        s.add_prototype(a, &at(0.0, 0.0, 1.0), None, false, 20, 0)
            .unwrap();
        // B's two prototypes are close together on its first group of turns and
        // both weak on the second.
        s.add_prototype(b, &at(0.40, 0.30, 0.0), None, false, 20, 0)
            .unwrap();
        s.add_prototype(b, &at(0.38, 0.29, 0.0), None, false, 20, 0)
            .unwrap();
        // The imposter's best prototype beats both of B's on both groups; its
        // second is a dud on the first group and strong on the second.
        s.add_prototype(imposter, &at(0.42, 0.42, 0.0), None, false, 20, 0)
            .unwrap();
        s.add_prototype(imposter, &at(0.10, 0.36, 0.0), None, false, 20, 0)
            .unwrap();

        // Six of A's turns, four of B's first group and three of its second,
        // repeating — so the chronological split sees the same mixture on both
        // sides of the cut.
        let pattern = [
            ("ua", 0),
            ("ub", 1),
            ("ua", 0),
            ("ub", 2),
            ("ua", 0),
            ("ub", 1),
            ("ua", 0),
            ("ub", 2),
            ("ua", 0),
            ("ub", 1),
            ("ua", 0),
            ("ub", 2),
            ("ub", 1),
        ];
        for k in 0..130usize {
            let (user, group) = pattern[k % pattern.len()];
            let t = (k as i64 + 1) * 10 * sec;
            let seg = s.insert_segment(sess, t, t + 5 * sec, "a.wav", 0).unwrap();
            let v = match group {
                0 => at(0.0, 0.0, 1.0),
                1 => at(1.0, 0.0, 0.0),
                _ => at(0.0, 1.0, 0.0),
            };
            s.store_embedding(seg, &v).unwrap();
            s.set_segment_truth(seg, Some(user), truth_verdict::SINGLE, Some(0.95))
                .unwrap();
        }
        s
    }

    #[test]
    fn a_rule_that_only_wins_on_the_global_bar_is_not_installed() {
        // The failure this round exists to stop. Under the global bar the
        // challenger is far ahead; under the bars each rule earns for itself
        // the incumbent is ahead and the challenger costs precision. The gate
        // reads the second, so nothing moves.
        let s = a_store_where_the_globals_and_the_bars_disagree();
        let r = calibrate(&s, &IdentityConfig::default(), true, 3).unwrap();

        let inc = r.aggregates.iter().find(|x| x.incumbent).unwrap();
        assert_eq!(inc.rule, crate::calib::Aggregate::Max);
        let top2 = r
            .aggregates
            .iter()
            .find(|x| x.rule == crate::calib::Aggregate::TopK(2))
            .unwrap();
        assert!(
            top2.globals.f_beta(calib::BETA) > inc.globals.f_beta(calib::BETA) + 0.05,
            "the globals prefer top-2: {:?} vs {:?}",
            top2.globals,
            inc.globals
        );
        assert!(
            inc.fitted.f_beta(calib::BETA) > top2.fitted.f_beta(calib::BETA),
            "each rule on its own bars prefers max: {:?} vs {:?}",
            inc.fitted,
            top2.fitted
        );
        assert!(inc.fitted.precision() > top2.fitted.precision());
        assert!(!inc.thresholds.is_empty(), "max earns a bar here");
        assert!(
            top2.thresholds.is_empty(),
            "top-2 tops too few rows to fit one"
        );

        assert!(!r.aggregate_swap, "{:?}", r.aggregate);
        assert_eq!(s.learned_aggregate().unwrap(), crate::calib::Aggregate::Max);
    }

    #[test]
    fn the_pass_takes_back_a_top_k_the_corpus_no_longer_supports() {
        // The half that did not exist: `aggregate_grid` was walked with the
        // installed rule skipped, so a box that had learned `top-3` could move
        // to another top-k but could never go home to max. On this fixture max
        // is right and the top-k means are not, and the pass has to say so.
        let s = a_store_with_a_phantom_voice(60);
        s.set_learned_aggregate(crate::calib::Aggregate::TopK(3))
            .unwrap();
        let r = calibrate(&s, &IdentityConfig::default(), true, 9).unwrap();
        assert!(r.aggregate_swap, "{:?}", r.aggregate);
        assert_eq!(r.aggregate.unwrap().0, crate::calib::Aggregate::Max);
        assert_eq!(
            s.learned_aggregate().unwrap(),
            crate::calib::Aggregate::Max,
            "the box is back on the rule its own numbers support"
        );
        let ops = s.operations_of(OP, 10).unwrap();
        assert_eq!(ops.len(), 1);
        assert!(ops[0].prior_state.contains("\"aggregates\""));
    }

    #[test]
    fn installing_a_rule_installs_the_bars_that_were_measured_with_it() {
        // The pair the gate approved is (rule, bars). Installing the rule and
        // clearing the bars would put the box on an operating point nothing in
        // this run measured, and leave it there until the next pass.
        let s = a_store_where_the_globals_and_the_bars_disagree();
        s.set_learned_aggregate(crate::calib::Aggregate::TopK(2))
            .unwrap();
        let r = calibrate(&s, &IdentityConfig::default(), true, 11).unwrap();
        assert!(r.aggregate_swap);
        assert_eq!(r.aggregate.unwrap().0, crate::calib::Aggregate::Max);
        assert!(
            !r.aggregate_thresholds.is_empty(),
            "the winning rule earned a bar; it is half of what was approved"
        );
        let installed = s.learned_thresholds().unwrap();
        assert_eq!(installed.len(), r.aggregate_thresholds.len());
        for v in &r.aggregate_thresholds {
            let row = installed
                .iter()
                .find(|l| l.speaker_id == v.speaker_id)
                .unwrap_or_else(|| panic!("voice {} was measured but not written", v.speaker_id));
            assert!((row.threshold - v.threshold).abs() < 1e-6);
            assert_eq!(row.n, v.n as i64);
            assert_eq!(row.via, truth_via::LEARNED);
        }
    }

    #[test]
    fn an_aggregate_with_nothing_to_buy_is_not_installed() {
        // The fixture where every voice already sits on its own prototypes:
        // there is no confusion for an aggregate to resolve, so the pass must
        // leave the shipped rule alone rather than churn it.
        let s = a_truthful_store(60);
        let r = calibrate(&s, &IdentityConfig::default(), true, 1).unwrap();
        assert!(!r.aggregate_swap);
        assert_eq!(s.learned_aggregate().unwrap(), crate::calib::Aggregate::Max);
    }

    // ---- `identity repair --prototypes` -----------------------------------

    /// A store whose bank contains one prototype that Discord's own verdict
    /// says is a different person — the three `Speaker_5x` rows of §32 step 2,
    /// in miniature.
    fn a_store_with_a_wrong_prototype() -> (Store, i64, i64) {
        let s = a_store_with_an_imposter(30);
        let src = s.upsert_source("Discord", "Discord", 1).unwrap();
        let sess = s.begin_session(src, 0).unwrap();
        let phantom = s.mint_speaker(0).unwrap();
        let seg = s
            .insert_segment(sess, 9_000_000_000, 12_000_000_000, "x.wav", 0)
            .unwrap();
        s.set_segment_truth(seg, Some("ua"), truth_verdict::SINGLE, Some(0.94))
            .unwrap();
        let p = s
            .add_prototype(
                phantom,
                &Embedding::new("m@1", vec![1.0, 0.02]),
                Some(seg),
                false,
                20,
                0,
            )
            .unwrap()
            .unwrap();
        (s, phantom, p)
    }

    #[test]
    fn a_repair_preview_names_the_prototype_and_deletes_nothing() {
        let (s, phantom, p) = a_store_with_a_wrong_prototype();
        let r = repair_prototypes(&s, &IdentityConfig::default(), false, 1).unwrap();
        assert_eq!(r.condemned.len(), 1, "{:?}", r.condemned);
        assert_eq!(r.condemned[0].prototype_id, p);
        assert_eq!(r.condemned[0].owner, phantom);
        assert_eq!(r.deleted, 0);
        assert!(!s.speaker_prototypes(phantom, "m@1").unwrap().is_empty());
        assert!(s.operations_of(REPAIR_OP, 10).unwrap().is_empty());
    }

    #[test]
    fn a_repair_apply_removes_it_and_says_so_in_the_audit_trail() {
        let (s, phantom, _p) = a_store_with_a_wrong_prototype();
        let r = repair_prototypes(&s, &IdentityConfig::default(), true, 5).unwrap();
        assert_eq!(r.deleted, 1);
        assert!(s.speaker_prototypes(phantom, "m@1").unwrap().is_empty());
        let ops = s.operations_of(REPAIR_OP, 10).unwrap();
        assert_eq!(ops.len(), 1);
        assert!(ops[0].prior_state.contains("\"condemned\""));
        // Idempotent: the evidence is gone with the prototype.
        let again = repair_prototypes(&s, &IdentityConfig::default(), true, 6).unwrap();
        assert_eq!(again.deleted, 0);
        assert_eq!(s.operations_of(REPAIR_OP, 10).unwrap().len(), 1);
    }

    #[test]
    fn a_repair_measures_at_the_installs_operating_point_not_the_defaults() {
        // The table this command prints is the whole argument for a permanent
        // deletion, and until 0.12.3 it was measured with `IdentityConfig::
        // default()` — on the live box a `max_overlap` of 0.1 against the 0.06
        // the daemon actually gates at. A number that describes nobody's
        // machine is not evidence for deleting from this one.
        let (s, _phantom, _p) = a_store_with_a_wrong_prototype();
        let cfg = IdentityConfig {
            // Longer than every turn in the fixture, so this gate refuses all.
            min_duration_s: 6.0,
            ..IdentityConfig::default()
        };
        let r = repair_prototypes(&s, &cfg, false, 1).unwrap();
        let (before, after) = r.measured.expect("there was enough truth to measure");
        assert_eq!(before.declined, before.n, "this gate refuses every turn");
        assert_eq!(after.declined, after.n);
        // And with the fixture's own operating point it names people again.
        let d = repair_prototypes(&s, &IdentityConfig::default(), false, 1).unwrap();
        assert!(d.measured.unwrap().0.declined < before.declined);
    }

    #[test]
    fn a_repair_reports_what_it_would_do_to_the_held_out_numbers() {
        // The command is not allowed to be a leap of faith: it prints the same
        // before/after table `calibrate` does, measured the same way.
        let (s, _phantom, _p) = a_store_with_a_wrong_prototype();
        let r = repair_prototypes(&s, &IdentityConfig::default(), false, 1).unwrap();
        let (before, after) = r.measured.expect("there was enough truth to measure");
        assert_eq!(before.n, after.n, "the same held-out rows, both sides");
        assert!(after.wrong <= before.wrong);
    }
}
