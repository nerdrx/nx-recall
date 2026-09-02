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
            "proposed": self.proposed.iter().map(|v| json!({
                "speaker": v.speaker_id,
                "threshold": v.threshold,
                "margin": v.margin,
                "n": v.n,
                "f_beta": v.f_beta,
                "f_beta_global": v.f_beta_global,
            })).collect::<Vec<_>>(),
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
    let ranked = crate::identity::rank(&probe, &live)?;
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
) -> Result<Score> {
    let mut s = Score::default();
    for row in rows {
        // The user's own account is not ground truth about audio captured
        // from the user's own Discord client (0.10.1): a client does not play
        // your microphone back to you.
        if Some(row.truth_speaker_id) == you {
            continue;
        }
        let (label, _) = replay(cfg, thresholds, bank, row, proj)?;
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

    // ---- step 1: per-voice thresholds ------------------------------------

    let fit_obs: Vec<Obs> = fit
        .iter()
        .filter(|r| crate::identity::gate(cfg, r.overlap_frac, r.duration_s).is_none())
        .filter_map(|r| replay(cfg, &globals, &bank, r, None).ok().and_then(|x| x.1))
        .collect();
    report.proposed = calib::fit_thresholds(&fit_obs, MIN_ROWS_PER_VOICE, THRESHOLD_BOUNDS, global);
    let mut candidate = Thresholds::global(global.0, global.1);
    for v in &report.proposed {
        candidate.insert(v.speaker_id, v.threshold, v.margin);
    }

    report.baseline = judge(cfg, &globals, &bank, &eval, None, you)?;
    report.candidate = judge(cfg, &candidate, &bank, &eval, None, you)?;
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
        let inner_base = judge(cfg, &globals, &bank, inner_eval, None, you)?;
        let mut best: Option<(f64, Whitening)> = None;
        for w in whitening_grid() {
            let Ok(p) = calib::fit_projection(&model_id, &inner_labelled, w) else {
                continue;
            };
            let Ok(pb) = project_bank(&p, &bank) else {
                continue;
            };
            let s = judge(cfg, &globals, &pb, inner_eval, Some(&p), you)?;
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
                let s = judge(cfg, &globals, &pb, &eval, Some(&p), you)?;
                report.projection_swap = calib::may_install(&report.baseline, &s);
                report.projection = Some((w, s));
                if apply && report.projection_swap {
                    let version = now_utc_ns;
                    store.install_projection(&p, version, now_utc_ns)?;
                }
            }
        }
    }

    // ---- write, if the gate said so --------------------------------------

    if apply && report.thresholds_swap {
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
    if apply && (report.written > 0 || report.cleared > 0 || report.projection_installed) {
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

/// `recalld identity calibrate --reset`: back to the globals, everywhere.
pub fn reset(store: &Store, now_utc_ns: i64) -> Result<(usize, bool)> {
    let cleared = store.clear_learned_thresholds(None)?;
    let dropped = store.clear_projection()?;
    if cleared > 0 || dropped {
        store.log_operation(
            OP,
            "[]",
            &json!({ "reset": true, "cleared": cleared, "projection": dropped }).to_string(),
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

    use crate::store::Store;

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
}
