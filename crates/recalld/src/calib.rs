//! Learned identity: thresholds and an embedding space fitted to *this*
//! install's ground truth (0.11.0).
//!
//! Everything here is pure math over vectors and scores — no store, no model,
//! no clock. That is deliberate twice over: the fit has to be testable on
//! synthetic clusters where the right answer is known, and the same functions
//! have to run inside the nightly pass, inside `recalld identity calibrate`
//! and inside the offline bench without three copies drifting apart.
//!
//! ## What is being learned, and what is not
//!
//! * **Thresholds.** The global `label_threshold` is one number for every
//!   voice, chosen once on lab audio. But a voice with twenty prototypes
//!   spanning a year of evenings scores differently from a voice with four
//!   from one call, and the operating point that is right for one is wrong for
//!   the other. Per-voice thresholds are the smallest thing that can fix that,
//!   and the only thing here that is cheap enough to refit every night.
//! * **The space.** Cosine in the extractor's raw space treats every dimension
//!   as equally informative. Within-class whitening (the WCCN of the speaker
//!   verification literature) rescales it by how much each direction varies
//!   *within* one person — the directions that move when the same person says
//!   a different sentence get shrunk, the directions that separate people do
//!   not. It is one matrix multiply, it is linear, and it keeps every voice:
//!   an LDA projection down to (classes − 1) dimensions would throw away the
//!   subspace that separates the voices ground truth has never seen, which on
//!   this install is most of them.
//! * **Not the enrol bar.** A wrong prototype is permanent (`identity.rs`),
//!   ground truth is not a large sample, and nothing here has measured the
//!   enrol decision. It keeps its global numbers.
//!
//! ## The honesty rules, encoded
//!
//! [`split_at`] is a *chronological* split, and it will not cut through a
//! timestamp: rows sharing an instant land on the same side, so a fit can
//! never see half of a moment it is later scored on. [`swap_is_safe`] is the
//! refusal rule — a refit that lowers held-out precision does not ship, no
//! matter what it does to recall.

use std::collections::BTreeMap;

use anyhow::{Result, bail};

use crate::embed::Embedding;

/// The share of truth rows, in time order, that a fit may see. The rest is
/// held out and is the only thing any gate is allowed to read.
pub const FIT_FRACTION: f64 = 0.6;

/// Precision matters more than recall: a wrong name corrupts what the user
/// later reads back as memory, a missed one costs a shrug.
pub const BETA: f64 = 0.5;

/// A learned threshold may not wander outside this. Below the floor the
/// voicebank is guessing off noise; above the ceiling it has stopped
/// recognising the person on a bad microphone day.
pub const THRESHOLD_BOUNDS: (f32, f32) = (0.30, 0.60);

/// A voice needs this many truth rows of its own before its threshold is
/// fitted rather than inherited from the global operating point.
pub const MIN_ROWS_PER_VOICE: usize = 30;

// ---- scoring ---------------------------------------------------------------

/// One arm's report card against ground truth.
///
/// `declined` is not `wrong`: the ladder saying "nobody in the bank" is a
/// different claim from it naming the wrong person, and the two cost the user
/// very different things.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Score {
    pub n: i64,
    pub correct: i64,
    pub wrong: i64,
    pub declined: i64,
}

impl Score {
    pub fn add(&mut self, labelled: Option<i64>, truth: i64) {
        self.n += 1;
        match labelled {
            Some(id) if id == truth => self.correct += 1,
            Some(_) => self.wrong += 1,
            None => self.declined += 1,
        }
    }

    /// Of the rows it put a name on, how many were right.
    pub fn precision(&self) -> f64 {
        let d = self.correct + self.wrong;
        if d == 0 {
            f64::NAN
        } else {
            self.correct as f64 / d as f64
        }
    }

    /// Of the rows it could have named, how many it named right.
    pub fn recall(&self) -> f64 {
        if self.n == 0 {
            f64::NAN
        } else {
            self.correct as f64 / self.n as f64
        }
    }

    pub fn f_beta(&self, beta: f64) -> f64 {
        f_beta(self.precision(), self.recall(), beta)
    }
}

/// F-beta, with the convention that a run which named nothing scores zero
/// rather than NaN. An arm that declines every row is not "perfectly precise";
/// it is useless, and the fit must not be able to win by choosing it.
pub fn f_beta(precision: f64, recall: f64, beta: f64) -> f64 {
    if precision.is_nan() || recall.is_nan() {
        return 0.0;
    }
    let b2 = beta * beta;
    let denom = b2 * precision + recall;
    if denom <= 0.0 {
        0.0
    } else {
        (1.0 + b2) * precision * recall / denom
    }
}

/// May a refit replace what is installed?
///
/// Precision is the veto: a new space or a new threshold that names more rows
/// by naming more of them wrongly is a regression however good its F looks,
/// because the wrong names are what end up in the transcript the user trusts.
/// Beyond that the candidate has to be an actual improvement on F-beta — an
/// exact tie keeps the incumbent, so a nightly pass cannot churn the operating
/// point on rounding noise.
pub fn swap_is_safe(incumbent: &Score, candidate: &Score) -> bool {
    let (p0, p1) = (incumbent.precision(), candidate.precision());
    // An incumbent that named nothing has no precision to defend.
    if !p0.is_nan() && (p1.is_nan() || p1 + 1e-9 < p0) {
        return false;
    }
    candidate.f_beta(BETA) > incumbent.f_beta(BETA) + 1e-9
}

/// How much held-out recall a candidate must add, in percentage points, to be
/// worth installing on recall alone.
pub const MATERIAL_RECALL_PP: f64 = 2.0;
/// Or how much of the wrong-label count it must remove, as a fraction.
pub const MATERIAL_WRONG_DROP: f64 = 0.20;

/// Is the improvement big enough to be worth changing the operating point for?
///
/// [`swap_is_safe`] answers "would this be worse?"; this answers "is this
/// enough?", and both have to say yes before anything is installed. The bar is
/// two percentage points of held-out recall, **or** a fifth of the wrong
/// labels gone — the two things a user would actually notice.
///
/// Without it a nightly pass installs every rounding-error improvement it can
/// find, and the operating point becomes a thing that moves for reasons nobody
/// can point at. A learned threshold is a claim, and a claim that buys one row
/// out of ninety-six is not worth making.
pub fn improvement_is_material(baseline: &Score, candidate: &Score) -> bool {
    let recall_up = (candidate.recall() - baseline.recall()) * 100.0;
    let wrong_down = if baseline.wrong == 0 {
        0.0
    } else {
        (baseline.wrong - candidate.wrong) as f64 / baseline.wrong as f64
    };
    (recall_up.is_finite() && recall_up >= MATERIAL_RECALL_PP - 1e-9)
        || wrong_down >= MATERIAL_WRONG_DROP - 1e-9
}

/// The whole installation rule in one place: safe **and** worth it.
pub fn may_install(baseline: &Score, candidate: &Score) -> bool {
    swap_is_safe(baseline, candidate) && improvement_is_material(baseline, candidate)
}

// ---- the chronological split -----------------------------------------------

/// Where the fit split ends, given timestamps in ascending order.
///
/// Returns the index of the first held-out row. Two properties the callers
/// depend on, both about not leaking:
///
/// * rows sharing a timestamp are never split apart — the boundary is pushed
///   forward past the whole instant;
/// * the result is always in `1..len` when there is more than one distinct
///   timestamp, so neither side is empty. With a single distinct timestamp
///   there is no honest split and the answer is `len`: everything is fit data
///   and there is nothing to evaluate on.
pub fn split_at(times_ascending: &[i64], fit_fraction: f64) -> usize {
    let n = times_ascending.len();
    if n == 0 {
        return 0;
    }
    debug_assert!(times_ascending.windows(2).all(|w| w[0] <= w[1]));
    if n == 1 {
        return 1;
    }
    let target = ((n as f64) * fit_fraction)
        .round()
        .clamp(1.0, (n - 1) as f64) as usize;
    // Walk forward off the shared instant.
    let mut cut = target;
    while cut < n && times_ascending[cut] == times_ascending[cut - 1] {
        cut += 1;
    }
    if cut < n {
        return cut;
    }
    // The instant ran to the end of the history. Try the other side of it
    // before giving up — a fit that is smaller than asked for is still a fit,
    // whereas an empty held-out set is not a measurement.
    let mut back = target;
    while back > 0 && times_ascending[back] == times_ascending[back - 1] {
        back -= 1;
    }
    if back > 0 { back } else { n }
}

// ---- per-voice thresholds ---------------------------------------------------

/// One judged turn, reduced to what a threshold fit needs: which voice the
/// bank put on top, how well, how far clear of the runner-up, and who it
/// actually was.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Obs {
    /// The top-ranked voice. Only this voice's threshold can change this row's
    /// outcome, which is why the fit decomposes per voice at all.
    pub top: i64,
    pub score: f32,
    /// Top score minus runner-up; `f32::INFINITY` when there was no runner-up.
    pub margin: f32,
    pub truth: i64,
}

/// A threshold fitted for one voice, with everything needed to explain it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VoiceThreshold {
    pub speaker_id: i64,
    pub threshold: f32,
    pub margin: f32,
    /// Fit rows this voice topped. The number a UI should show as
    /// "calibrated on N turns".
    pub n: usize,
    /// F-beta on the fit split at the chosen point.
    pub f_beta: f64,
    /// F-beta the global operating point scored on the same rows. The pair is
    /// the whole argument for the change.
    pub f_beta_global: f64,
}

/// A table of per-voice operating points, with the global as the fallback.
///
/// Deliberately a plain map and not a trait: the ladder must be able to answer
/// "what threshold applies to voice 7" without touching a store, and a caller
/// that has no learned values passes [`Thresholds::global`] and gets exactly
/// 0.10.2's behaviour.
#[derive(Debug, Clone, PartialEq)]
pub struct Thresholds {
    global: (f32, f32),
    per_voice: BTreeMap<i64, (f32, f32)>,
}

impl Thresholds {
    /// No learned values: every voice answers with the global pair.
    pub fn global(label_threshold: f32, margin: f32) -> Self {
        Self {
            global: (label_threshold, margin),
            per_voice: BTreeMap::new(),
        }
    }

    pub fn with(mut self, speaker_id: i64, threshold: f32, margin: f32) -> Self {
        self.per_voice.insert(speaker_id, (threshold, margin));
        self
    }

    pub fn insert(&mut self, speaker_id: i64, threshold: f32, margin: f32) {
        self.per_voice.insert(speaker_id, (threshold, margin));
    }

    /// `(label threshold, minimum margin over the runner-up)` for one voice.
    pub fn for_speaker(&self, speaker_id: i64) -> (f32, f32) {
        self.per_voice
            .get(&speaker_id)
            .copied()
            .unwrap_or(self.global)
    }

    pub fn is_empty(&self) -> bool {
        self.per_voice.is_empty()
    }

    pub fn len(&self) -> usize {
        self.per_voice.len()
    }

    pub fn learned(&self) -> impl Iterator<Item = (i64, f32, f32)> + '_ {
        self.per_voice.iter().map(|(k, v)| (*k, v.0, v.1))
    }
}

/// The margin values the fit will consider. A label margin is a *second* way
/// to be careful and is off by default (0.0); the grid is coarse because
/// there is not enough truth on any install to justify resolving it finer.
const MARGIN_GRID: [f32; 5] = [0.0, 0.02, 0.04, 0.06, 0.08];

/// Score one voice's rows at a candidate operating point.
fn score_voice(rows: &[Obs], voice: i64, threshold: f32, margin: f32) -> Score {
    let mut s = Score::default();
    for r in rows {
        let labelled = (r.score >= threshold && r.margin >= margin).then_some(voice);
        s.add(labelled, r.truth);
    }
    s
}

/// Choose a label threshold and margin per voice, maximising F-beta on the
/// rows that voice topped.
///
/// The decomposition is exact rather than convenient: `decide` labels a turn
/// with the top-ranked voice or with nobody, so voice *v*'s threshold changes
/// the outcome of exactly the rows where *v* was top and of no others. Fitting
/// them jointly would be the same arithmetic with more opportunities to be
/// wrong.
///
/// A voice under `min_rows` gets nothing: it is not in the returned table and
/// therefore keeps the global point. A fitted voice whose best point does not
/// beat the global on its own fit rows also gets nothing — the fit has to earn
/// the deviation before the held-out gate is even asked.
pub fn fit_thresholds(
    obs: &[Obs],
    min_rows: usize,
    bounds: (f32, f32),
    global: (f32, f32),
) -> Vec<VoiceThreshold> {
    let mut by_voice: BTreeMap<i64, Vec<Obs>> = BTreeMap::new();
    for o in obs {
        by_voice.entry(o.top).or_default().push(*o);
    }

    let mut out = Vec::new();
    for (voice, rows) in by_voice {
        if rows.len() < min_rows {
            continue;
        }
        let base_score = score_voice(&rows, voice, global.0, global.1);
        let base = base_score.f_beta(BETA);
        let mut best: Option<(f64, i64, f32, f32)> = None;
        let mut t = bounds.0;
        while t <= bounds.1 + 1e-6 {
            for &m in &MARGIN_GRID {
                let sc = score_voice(&rows, voice, t, m);
                let f = sc.f_beta(BETA);
                // F-beta first, then **fewer wrong labels**, then the more
                // conservative point (higher threshold, then wider margin).
                //
                // The wrong-label tie-break is not decoration. A voice that
                // ground truth never once confirms — an unnamed row the bank
                // keeps putting on top of somebody else's turn — has an F of
                // zero at every threshold, because it has no correct answers
                // to be precise about. On F alone the fit would shrug and
                // leave it at the global, when the whole value on offer is
                // turning its wrong labels into declines.
                let better = match best {
                    None => true,
                    Some((bf, bw, bt, bm)) => {
                        f > bf + 1e-12
                            || ((f - bf).abs() <= 1e-12
                                && (sc.wrong < bw
                                    || (sc.wrong == bw && (t > bt || (t == bt && m > bm)))))
                    }
                };
                if better {
                    best = Some((f, sc.wrong, t, m));
                }
            }
            t += 0.01;
        }
        if let Some((f, wrong, t, m)) = best
            && (f > base + 1e-12 || ((f - base).abs() <= 1e-12 && wrong < base_score.wrong))
        {
            out.push(VoiceThreshold {
                speaker_id: voice,
                threshold: (t * 1000.0).round() / 1000.0,
                margin: (m * 1000.0).round() / 1000.0,
                n: rows.len(),
                f_beta: f,
                f_beta_global: base,
            });
        }
    }
    out
}

// ---- the learned space ------------------------------------------------------

/// A learned linear map applied before cosine: `y = A · (x − mean)`.
///
/// Stored, versioned and inspectable. `A` is square, so every voice survives
/// the map — including the ones ground truth has never named, which is the
/// property a rank-reducing LDA would give up.
#[derive(Debug, Clone, PartialEq)]
pub struct Projection {
    /// The embedding space this was fitted in. A projection is as
    /// model-specific as an embedding, and mixing them is the same error
    /// `Embedding::cosine` already refuses.
    pub model_id: String,
    pub dim: usize,
    pub mean: Vec<f32>,
    /// Row-major `dim × dim`.
    pub a: Vec<f32>,
    /// Rows the fit saw, and how many distinct people were in them. Both are
    /// provenance the user should be able to read off a chip.
    pub n_rows: usize,
    pub n_classes: usize,
    /// How it was fitted.
    pub whitening: Whitening,
}

impl Projection {
    /// The map that changes nothing. Useful as a baseline and as what a
    /// `--reset` installs conceptually.
    pub fn identity(model_id: impl Into<String>, dim: usize) -> Self {
        let mut a = vec![0.0f32; dim * dim];
        for i in 0..dim {
            a[i * dim + i] = 1.0;
        }
        Self {
            model_id: model_id.into(),
            dim,
            mean: vec![0.0; dim],
            a,
            n_rows: 0,
            n_classes: 0,
            whitening: Whitening {
                shrinkage: 0.0,
                power: 0.0,
                centre: false,
            },
        }
    }

    /// Project one embedding. The result keeps the source `model_id`, so a
    /// projected probe still refuses to be compared with a foreign vector —
    /// but it is the caller's job to project both sides, and
    /// [`project_bank`] exists so that is one call.
    pub fn apply(&self, e: &Embedding) -> Result<Embedding> {
        if e.model_id != self.model_id {
            bail!(
                "refusing to project an embedding from {} with a map fitted on {}",
                e.model_id,
                self.model_id
            );
        }
        if e.vector.len() != self.dim {
            bail!(
                "projection is {}-dimensional, the embedding is {}",
                self.dim,
                e.vector.len()
            );
        }
        let centred: Vec<f64> = e
            .vector
            .iter()
            .zip(&self.mean)
            .map(|(x, m)| (*x - *m) as f64)
            .collect();
        let out: Vec<f32> = self
            .a
            .chunks_exact(self.dim)
            .map(|row| {
                row.iter()
                    .zip(&centred)
                    .map(|(a, c)| (*a as f64) * *c)
                    .sum::<f64>() as f32
            })
            .collect();
        Ok(Embedding::new(e.model_id.clone(), out))
    }

    /// `mean`, then `a`, as little-endian f32 — the same byte order
    /// `Embedding::to_blob` uses, and for the same reason.
    pub fn to_blob(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity((self.dim + self.dim * self.dim) * 4);
        for v in self.mean.iter().chain(self.a.iter()) {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out
    }

    pub fn from_blob(model_id: impl Into<String>, dim: usize, blob: &[u8]) -> Result<Self> {
        let want = (dim + dim * dim) * 4;
        if blob.len() != want {
            bail!(
                "a {dim}-dimensional projection is {want} bytes, this blob is {}",
                blob.len()
            );
        }
        let f: Vec<f32> = blob
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes(*c))
            .collect();
        Ok(Self {
            model_id: model_id.into(),
            dim,
            mean: f[..dim].to_vec(),
            a: f[dim..].to_vec(),
            n_rows: 0,
            n_classes: 0,
            whitening: Whitening::default(),
        })
    }
}

/// Project a whole bank in one call, so a caller cannot project the probe and
/// forget the prototypes.
pub fn project_bank(p: &Projection, bank: &[(i64, Embedding)]) -> Result<Vec<(i64, Embedding)>> {
    bank.iter().map(|(id, e)| Ok((*id, p.apply(e)?))).collect()
}

/// One labelled vector for the fit.
#[derive(Debug, Clone, PartialEq)]
pub struct Labelled {
    pub class: i64,
    pub v: Vec<f32>,
}

/// How hard to whiten, and whether to re-centre. Two knobs rather than one
/// because the measurement said they matter separately (FINDINGS §18).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Whitening {
    /// Ledoit-Wolf shrinkage towards a scaled identity, in `[0, 1]`.
    pub shrinkage: f64,
    /// The exponent on the inverse: `Sw^{-power}`. `1.0` is the textbook
    /// half-whitening (`Sw^{-1/2}` after the square root is folded in), `0.0`
    /// is the identity map, and everything between is a partial correction.
    /// A small sample cannot support a full one — the directions it thinks are
    /// quiet are mostly directions it has not seen yet, and dividing by them
    /// is how a fit turns noise into the loudest thing in the space.
    pub power: f64,
    /// Subtract the fit split's mean before mapping.
    ///
    /// Off by default, and that is a measured decision: the extractor already
    /// applies its own `global-mean` normalisation, and re-centring on a few
    /// hundred rows dominated by whoever talked most that evening moves every
    /// *other* voice's prototypes relative to an origin that is really one
    /// person's centre.
    pub centre: bool,
}

impl Default for Whitening {
    fn default() -> Self {
        Self {
            shrinkage: 0.2,
            power: 0.5,
            centre: false,
        }
    }
}

/// Fit within-class whitening on labelled vectors.
///
/// The estimator is the pooled within-class covariance, shrunk towards a
/// scaled identity (Ledoit-Wolf's shape, with the intensity handed in rather
/// than estimated — on a few hundred rows in 192 dimensions the covariance is
/// rank-deficient by construction and the honest thing is to pick the
/// intensity by held-out measurement, which is what the bench does). The map
/// is `Sw^{-power/2}`, computed from a symmetric eigendecomposition, so it is
/// symmetric, full-rank and its own best explanation: directions along which
/// one person's own turns scatter widely get divided down.
///
/// Degenerate input cannot blow up. A class with one row contributes no
/// scatter, a direction with no variance is floored rather than inverted, and
/// fewer than two usable classes is an error rather than a matrix of
/// infinities.
pub fn fit_whitening(rows: &[Labelled], w: Whitening) -> Result<(Vec<f32>, Vec<f32>, usize)> {
    let shrinkage = w.shrinkage.clamp(0.0, 1.0);
    let dim = rows.first().map(|r| r.v.len()).unwrap_or(0);
    if dim == 0 {
        bail!("cannot fit a projection on no vectors");
    }
    if rows.iter().any(|r| r.v.len() != dim) {
        bail!("the fit rows are not all {dim}-dimensional");
    }

    // Per-class means, and the global mean.
    let mut sums: BTreeMap<i64, (Vec<f64>, usize)> = BTreeMap::new();
    let mut global = vec![0.0f64; dim];
    for r in rows {
        let e = sums.entry(r.class).or_insert_with(|| (vec![0.0; dim], 0));
        for ((slot, total), x) in e.0.iter_mut().zip(global.iter_mut()).zip(&r.v) {
            *slot += *x as f64;
            *total += *x as f64;
        }
        e.1 += 1;
    }
    let n = rows.len() as f64;
    for g in &mut global {
        *g /= n;
    }
    let usable = sums.values().filter(|(_, c)| *c >= 2).count();
    if usable < 2 {
        bail!(
            "a within-class fit needs at least two people with two turns each; \
             this sample has {usable}"
        );
    }
    let means: BTreeMap<i64, Vec<f64>> = sums
        .iter()
        .map(|(k, (s, c))| (*k, s.iter().map(|v| v / *c as f64).collect()))
        .collect();

    // Pooled within-class scatter. Only classes with two or more rows say
    // anything about within-class spread; a singleton's residual is zero and
    // its degree of freedom is zero, so it drops out of both sums.
    let mut sw = vec![0.0f64; dim * dim];
    let mut dof = 0f64;
    let mut d = vec![0.0f64; dim];
    for r in rows {
        let (_, count) = sums[&r.class];
        if count < 2 {
            continue;
        }
        let m = &means[&r.class];
        for j in 0..dim {
            d[j] = r.v[j] as f64 - m[j];
        }
        for i in 0..dim {
            let di = d[i];
            if di == 0.0 {
                continue;
            }
            for j in i..dim {
                sw[i * dim + j] += di * d[j];
            }
        }
    }
    for (_, c) in sums.values() {
        if *c >= 2 {
            dof += (*c - 1) as f64;
        }
    }
    for i in 0..dim {
        for j in i..dim {
            let v = sw[i * dim + j] / dof;
            sw[i * dim + j] = v;
            sw[j * dim + i] = v;
        }
    }

    // Shrink towards a scaled identity with the same trace.
    let mut trace = 0.0f64;
    for i in 0..dim {
        trace += sw[i * dim + i];
    }
    let mu = if trace > 0.0 { trace / dim as f64 } else { 1.0 };
    for i in 0..dim {
        for j in 0..dim {
            sw[i * dim + j] *= 1.0 - shrinkage;
        }
        sw[i * dim + i] += shrinkage * mu;
    }

    // A = Sw^{-1/2} = V diag(λ^-1/2) V'.
    let (vals, vecs) = jacobi_eigh(&mut sw, dim);
    // A floor rather than a fudge: an eigenvalue at or below zero is a
    // direction the sample never saw move, and inverting it would turn noise
    // into the loudest thing in the space.
    let floor = vals.iter().cloned().fold(0.0f64, f64::max) * 1e-6;
    let floor = if floor > 0.0 { floor } else { 1e-12 };
    let exponent = -0.5 * w.power.clamp(0.0, 1.0);
    let inv_sqrt: Vec<f64> = vals.iter().map(|&l| l.max(floor).powf(exponent)).collect();

    let mut a = vec![0.0f32; dim * dim];
    for i in 0..dim {
        for j in i..dim {
            let mut acc = 0.0f64;
            for k in 0..dim {
                acc += vecs[i * dim + k] * inv_sqrt[k] * vecs[j * dim + k];
            }
            a[i * dim + j] = acc as f32;
            a[j * dim + i] = acc as f32;
        }
    }
    let mean: Vec<f32> = if w.centre {
        global.iter().map(|v| *v as f32).collect()
    } else {
        vec![0.0; dim]
    };
    Ok((mean, a, usable))
}

/// The whole fit, packaged.
pub fn fit_projection(model_id: &str, rows: &[Labelled], w: Whitening) -> Result<Projection> {
    let (mean, a, n_classes) = fit_whitening(rows, w)?;
    Ok(Projection {
        model_id: model_id.to_string(),
        dim: mean.len(),
        mean,
        a,
        n_rows: rows.len(),
        n_classes,
        whitening: w,
    })
}

/// Symmetric eigendecomposition by cyclic Jacobi rotations.
///
/// Hand-rolled on purpose: the alternative is a linear algebra dependency for
/// one 192×192 symmetric matrix that is decomposed once a night. Jacobi is the
/// one algorithm in this family short enough to read and check, and it is
/// unconditionally stable on symmetric input.
///
/// `a` is overwritten. Returns `(eigenvalues, eigenvectors)` where column `k`
/// of the row-major `dim × dim` eigenvector matrix — that is, entries
/// `vecs[i * dim + k]` — is the eigenvector for `vals[k]`.
pub fn jacobi_eigh(a: &mut [f64], dim: usize) -> (Vec<f64>, Vec<f64>) {
    let mut v = vec![0.0f64; dim * dim];
    for i in 0..dim {
        v[i * dim + i] = 1.0;
    }
    if dim == 0 {
        return (Vec::new(), v);
    }
    for _sweep in 0..100 {
        let mut off = 0.0f64;
        for i in 0..dim {
            for j in (i + 1)..dim {
                off += a[i * dim + j] * a[i * dim + j];
            }
        }
        if off.sqrt() <= 1e-12 {
            break;
        }
        for p in 0..dim {
            for q in (p + 1)..dim {
                let apq = a[p * dim + q];
                if apq.abs() < 1e-300 {
                    continue;
                }
                let app = a[p * dim + p];
                let aqq = a[q * dim + q];
                let theta = (aqq - app) / (2.0 * apq);
                let t = if theta >= 0.0 {
                    1.0 / (theta + (1.0 + theta * theta).sqrt())
                } else {
                    -1.0 / (-theta + (1.0 + theta * theta).sqrt())
                };
                let c = 1.0 / (1.0 + t * t).sqrt();
                let s = t * c;
                for k in 0..dim {
                    let akp = a[k * dim + p];
                    let akq = a[k * dim + q];
                    a[k * dim + p] = c * akp - s * akq;
                    a[k * dim + q] = s * akp + c * akq;
                }
                for k in 0..dim {
                    let apk = a[p * dim + k];
                    let aqk = a[q * dim + k];
                    a[p * dim + k] = c * apk - s * aqk;
                    a[q * dim + k] = s * apk + c * aqk;
                }
                for k in 0..dim {
                    let vkp = v[k * dim + p];
                    let vkq = v[k * dim + q];
                    v[k * dim + p] = c * vkp - s * vkq;
                    v[k * dim + q] = s * vkp + c * vkq;
                }
            }
        }
    }
    let vals = (0..dim).map(|i| a[i * dim + i]).collect();
    (vals, v)
}

// ---- the overlap gate against truth ----------------------------------------

/// One point on the overlap gate's curve: what refusing above `threshold`
/// would have done to turns Discord itself called overlapped or single.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GatePoint {
    pub threshold: f32,
    /// Genuinely overlapped turns the gate refused.
    pub caught: i64,
    /// Single-speaker turns the gate refused: identity thrown away for
    /// nothing.
    pub false_refusals: i64,
    /// Overlapped turns that got through and were scored anyway.
    pub missed: i64,
    /// Single-speaker turns correctly let through.
    pub passed: i64,
}

impl GatePoint {
    /// Of the turns it refused, how many were really overlapped.
    pub fn precision(&self) -> f64 {
        let d = self.caught + self.false_refusals;
        if d == 0 {
            f64::NAN
        } else {
            self.caught as f64 / d as f64
        }
    }
    /// Of the overlapped turns, how many it refused.
    pub fn recall(&self) -> f64 {
        let d = self.caught + self.missed;
        if d == 0 {
            f64::NAN
        } else {
            self.caught as f64 / d as f64
        }
    }
    pub fn f_beta(&self) -> f64 {
        f_beta(self.precision(), self.recall(), BETA)
    }
}

/// The gate's curve over a grid of thresholds.
///
/// `rows` is `(was really overlapped, measured overlap fraction)`. The rule
/// being measured is `identity::gate`'s: refuse when the fraction is strictly
/// greater than the threshold.
pub fn overlap_curve(rows: &[(bool, f32)], grid: &[f32]) -> Vec<GatePoint> {
    grid.iter()
        .map(|&threshold| {
            let mut p = GatePoint {
                threshold,
                caught: 0,
                false_refusals: 0,
                missed: 0,
                passed: 0,
            };
            for &(is_overlap, frac) in rows {
                match (is_overlap, frac > threshold) {
                    (true, true) => p.caught += 1,
                    (true, false) => p.missed += 1,
                    (false, true) => p.false_refusals += 1,
                    (false, false) => p.passed += 1,
                }
            }
            p
        })
        .collect()
}

/// The gate grid the bench and the calibrate command both walk.
pub fn overlap_grid() -> Vec<f32> {
    (5..=30).map(|i| i as f32 / 100.0).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- the chronological split ----------------------------------------

    #[test]
    fn the_split_lands_where_the_fraction_says() {
        let t: Vec<i64> = (0..10).collect();
        assert_eq!(split_at(&t, 0.6), 6);
        assert_eq!(split_at(&t, 0.5), 5);
    }

    #[test]
    fn the_split_never_cuts_through_one_instant() {
        // Six rows share the moment the naive cut would land in. Splitting
        // there would let the fit see half of a moment it is then scored on.
        let t = vec![1, 2, 3, 4, 5, 5, 5, 5, 5, 9];
        let cut = split_at(&t, 0.6);
        assert_eq!(cut, 9);
        assert_ne!(t[cut - 1], t[cut]);
    }

    #[test]
    fn a_single_instant_has_no_honest_split() {
        let t = vec![7; 20];
        assert_eq!(split_at(&t, 0.6), 20);
    }

    #[test]
    fn an_empty_history_splits_at_zero() {
        assert_eq!(split_at(&[], 0.6), 0);
    }

    #[test]
    fn neither_side_is_empty_when_there_are_two_instants() {
        for frac in [0.01, 0.5, 0.99] {
            let cut = split_at(&[1, 2], frac);
            assert!((1..2).contains(&cut), "fraction {frac} gave {cut}");
        }
    }

    // ---- scoring and the refusal rule ------------------------------------

    #[test]
    fn declining_everything_scores_zero_rather_than_perfect() {
        let s = Score {
            n: 10,
            correct: 0,
            wrong: 0,
            declined: 10,
        };
        assert!(s.precision().is_nan());
        assert_eq!(s.f_beta(BETA), 0.0);
    }

    #[test]
    fn a_refit_that_lowers_precision_never_swaps() {
        let incumbent = Score {
            n: 100,
            correct: 80,
            wrong: 2,
            declined: 18,
        };
        // More right answers, but also many more wrong ones.
        let candidate = Score {
            n: 100,
            correct: 95,
            wrong: 5,
            declined: 0,
        };
        assert!(candidate.recall() > incumbent.recall());
        assert!(candidate.precision() < incumbent.precision());
        assert!(!swap_is_safe(&incumbent, &candidate));
    }

    #[test]
    fn a_refit_that_holds_precision_and_lifts_recall_swaps() {
        let incumbent = Score {
            n: 100,
            correct: 60,
            wrong: 2,
            declined: 38,
        };
        let candidate = Score {
            n: 100,
            correct: 75,
            wrong: 2,
            declined: 23,
        };
        assert!(swap_is_safe(&incumbent, &candidate));
    }

    #[test]
    fn a_one_row_improvement_is_safe_but_not_worth_installing() {
        // The measured shape of this round: precision holds, F-0.5 ticks up,
        // one more row gets a name and not one wrong label goes away.
        let baseline = Score {
            n: 95,
            correct: 80,
            wrong: 7,
            declined: 8,
        };
        let candidate = Score {
            n: 95,
            correct: 81,
            wrong: 7,
            declined: 7,
        };
        assert!(swap_is_safe(&baseline, &candidate));
        assert!(!improvement_is_material(&baseline, &candidate));
        assert!(!may_install(&baseline, &candidate));
    }

    #[test]
    fn two_points_of_recall_is_worth_installing() {
        let baseline = Score {
            n: 100,
            correct: 80,
            wrong: 7,
            declined: 13,
        };
        let candidate = Score {
            n: 100,
            correct: 82,
            wrong: 7,
            declined: 11,
        };
        assert!(improvement_is_material(&baseline, &candidate));
        assert!(may_install(&baseline, &candidate));
    }

    #[test]
    fn a_fifth_of_the_wrong_labels_is_worth_installing_on_its_own() {
        // Recall does not move at all; the wrong labels become declines.
        let baseline = Score {
            n: 100,
            correct: 70,
            wrong: 10,
            declined: 20,
        };
        let candidate = Score {
            n: 100,
            correct: 70,
            wrong: 8,
            declined: 22,
        };
        assert_eq!(baseline.recall(), candidate.recall());
        assert!(improvement_is_material(&baseline, &candidate));
        assert!(may_install(&baseline, &candidate));
    }

    #[test]
    fn a_material_improvement_that_costs_precision_still_does_not_install() {
        // The veto outranks the materiality bar, not the other way round.
        let baseline = Score {
            n: 100,
            correct: 80,
            wrong: 2,
            declined: 18,
        };
        let candidate = Score {
            n: 100,
            correct: 95,
            wrong: 5,
            declined: 0,
        };
        assert!(improvement_is_material(&baseline, &candidate));
        assert!(!may_install(&baseline, &candidate));
    }

    #[test]
    fn an_exact_tie_keeps_the_incumbent() {
        let s = Score {
            n: 50,
            correct: 40,
            wrong: 3,
            declined: 7,
        };
        assert!(!swap_is_safe(&s, &s));
    }

    #[test]
    fn an_incumbent_that_named_nothing_has_no_precision_to_defend() {
        let incumbent = Score {
            n: 20,
            correct: 0,
            wrong: 0,
            declined: 20,
        };
        let candidate = Score {
            n: 20,
            correct: 9,
            wrong: 1,
            declined: 10,
        };
        assert!(swap_is_safe(&incumbent, &candidate));
    }

    // ---- per-voice thresholds --------------------------------------------

    fn obs(top: i64, score: f32, truth: i64) -> Obs {
        Obs {
            top,
            score,
            margin: f32::INFINITY,
            truth,
        }
    }

    #[test]
    fn a_voice_below_the_row_count_keeps_the_global() {
        let rows: Vec<Obs> = (0..29).map(|_| obs(1, 0.9, 1)).collect();
        assert!(
            fit_thresholds(&rows, MIN_ROWS_PER_VOICE, THRESHOLD_BOUNDS, (0.35, 0.0)).is_empty()
        );
    }

    #[test]
    fn a_voice_whose_imposters_score_low_learns_a_higher_threshold() {
        // Forty of this voice's own turns land at 0.62, and twenty other
        // people's turns land between 0.36 and 0.44 — over the global 0.35
        // and therefore wrong labels today.
        let mut rows: Vec<Obs> = (0..40).map(|_| obs(1, 0.62, 1)).collect();
        for i in 0..20 {
            rows.push(obs(1, 0.36 + i as f32 * 0.004, 2));
        }
        let fitted = fit_thresholds(&rows, MIN_ROWS_PER_VOICE, THRESHOLD_BOUNDS, (0.35, 0.0));
        assert_eq!(fitted.len(), 1);
        let f = fitted[0];
        assert_eq!(f.speaker_id, 1);
        assert!(
            f.threshold > 0.44 && f.threshold <= 0.60,
            "threshold was {}",
            f.threshold
        );
        assert!(f.f_beta > f.f_beta_global);
        assert_eq!(f.n, 60);
    }

    #[test]
    fn a_voice_ground_truth_never_confirms_learns_to_stop_guessing() {
        // Forty turns where this voice was top and was wrong every time — an
        // unnamed row the bank keeps putting in front of other people. Its
        // F-0.5 is zero at every threshold, so an F-only fit would leave it
        // alone; the wrong-label tie-break raises its bar instead.
        let rows: Vec<Obs> = (0..40)
            .map(|i| obs(1, 0.36 + (i % 5) as f32 * 0.01, 2))
            .collect();
        let fitted = fit_thresholds(&rows, MIN_ROWS_PER_VOICE, THRESHOLD_BOUNDS, (0.35, 0.0));
        assert_eq!(fitted.len(), 1);
        assert!(fitted[0].threshold > 0.40, "{}", fitted[0].threshold);
        assert_eq!(fitted[0].f_beta, 0.0);
    }

    #[test]
    fn a_voice_the_global_already_suits_is_left_alone() {
        // Every row is this voice's own and every one is far over the bar:
        // there is nothing a different threshold could buy, so nothing is
        // written and the voice keeps the global point.
        let rows: Vec<Obs> = (0..50).map(|_| obs(1, 0.9, 1)).collect();
        assert!(
            fit_thresholds(&rows, MIN_ROWS_PER_VOICE, THRESHOLD_BOUNDS, (0.35, 0.0)).is_empty()
        );
    }

    #[test]
    fn a_learned_threshold_stays_inside_its_bounds() {
        // All wrong, at every score: the fit would love to refuse everything,
        // but it may not leave the bounded range to do it.
        let rows: Vec<Obs> = (0..40).map(|i| obs(1, 0.30 + i as f32 * 0.01, 2)).collect();
        for f in fit_thresholds(&rows, MIN_ROWS_PER_VOICE, THRESHOLD_BOUNDS, (0.35, 0.0)) {
            assert!(f.threshold >= THRESHOLD_BOUNDS.0 - 1e-6);
            assert!(f.threshold <= THRESHOLD_BOUNDS.1 + 1e-6);
        }
    }

    #[test]
    fn the_fit_prefers_precision_to_recall() {
        // Ten of the voice's own turns at 0.80, and ten imposters at 0.50.
        // Recall alone would keep the bar low and take all twenty; F-0.5 puts
        // it above 0.50 and takes ten, all right.
        let mut rows: Vec<Obs> = (0..10).map(|_| obs(1, 0.80, 1)).collect();
        rows.extend((0..10).map(|_| obs(1, 0.50, 2)));
        let fitted = fit_thresholds(&rows, 10, THRESHOLD_BOUNDS, (0.35, 0.0));
        assert_eq!(fitted.len(), 1);
        assert!(fitted[0].threshold > 0.50);
    }

    #[test]
    fn one_voices_threshold_cannot_move_another_voices_rows() {
        let mut rows: Vec<Obs> = (0..40).map(|_| obs(1, 0.62, 1)).collect();
        rows.extend((0..20).map(|i| obs(1, 0.36 + i as f32 * 0.004, 2)));
        let alone = fit_thresholds(&rows, MIN_ROWS_PER_VOICE, THRESHOLD_BOUNDS, (0.35, 0.0));
        // The same rows, plus a second voice's, fitted together.
        rows.extend((0..40).map(|_| obs(9, 0.41, 9)));
        let together = fit_thresholds(&rows, MIN_ROWS_PER_VOICE, THRESHOLD_BOUNDS, (0.35, 0.0));
        let one = together.iter().find(|f| f.speaker_id == 1).unwrap();
        assert_eq!(*one, alone[0]);
    }

    #[test]
    fn the_thresholds_table_falls_back_to_the_global() {
        let t = Thresholds::global(0.35, 0.0).with(7, 0.52, 0.04);
        assert_eq!(t.for_speaker(7), (0.52, 0.04));
        assert_eq!(t.for_speaker(8), (0.35, 0.0));
        assert_eq!(t.len(), 1);
    }

    // ---- the learned space -----------------------------------------------

    /// Textbook whitening at a given shrinkage: the setting the synthetic
    /// tests exercise, where the sample is large enough to support it.
    fn full(shrinkage: f64) -> Whitening {
        Whitening {
            shrinkage,
            power: 1.0,
            centre: true,
        }
    }

    /// A cheap deterministic normal, so the tests need no rng dependency and
    /// are byte-identical on every machine.
    struct Rng(u64);
    impl Rng {
        fn next_f64(&mut self) -> f64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
            ((self.0 >> 11) as f64) / ((1u64 << 53) as f64)
        }
        fn normal(&mut self) -> f64 {
            // Irwin-Hall: twelve uniforms, mean 6, variance 1.
            (0..12).map(|_| self.next_f64()).sum::<f64>() - 6.0
        }
    }

    /// Two Gaussians whose separating direction is *narrow* and whose noise
    /// direction is wide. Cosine in the raw space is dominated by the noise;
    /// whitening should undo exactly that.
    fn two_gaussians(dim: usize, n: usize, seed: u64) -> Vec<Labelled> {
        let mut rng = Rng(seed);
        let mut out = Vec::new();
        for i in 0..(2 * n) {
            let class = (i % 2) as i64;
            let mut v = vec![0.0f32; dim];
            // Dimension 0 separates the two people, by a little.
            v[0] = if class == 0 { -0.35 } else { 0.35 };
            v[0] += (rng.normal() * 0.05) as f32;
            // Dimension 1 is loud within-person noise and says nothing.
            v[1] = (rng.normal() * 3.0) as f32;
            for x in v.iter_mut().skip(2) {
                *x = (rng.normal() * 0.4) as f32;
            }
            out.push(Labelled { class, v });
        }
        out
    }

    /// Score a nearest-class-mean-by-cosine rule, held out.
    fn held_out_f(fit: &[Labelled], eval: &[Labelled], p: Option<&Projection>) -> f64 {
        let map = |v: &Vec<f32>| -> Vec<f32> {
            match p {
                None => v.clone(),
                Some(p) => p
                    .apply(&Embedding::new("m@1", v.clone()))
                    .unwrap()
                    .vector
                    .clone(),
            }
        };
        let mut sums: BTreeMap<i64, (Vec<f64>, usize)> = BTreeMap::new();
        for r in fit {
            let v = map(&r.v);
            let e = sums
                .entry(r.class)
                .or_insert_with(|| (vec![0.0; v.len()], 0));
            for (a, b) in e.0.iter_mut().zip(&v) {
                *a += *b as f64;
            }
            e.1 += 1;
        }
        let protos: Vec<(i64, Embedding)> = sums
            .iter()
            .map(|(c, (s, n))| {
                (
                    *c,
                    Embedding::new("m@1", s.iter().map(|v| (v / *n as f64) as f32).collect()),
                )
            })
            .collect();
        let mut score = Score::default();
        for r in eval {
            let probe = Embedding::new("m@1", map(&r.v));
            let best = protos
                .iter()
                .map(|(c, e)| (*c, probe.cosine(e).unwrap()))
                .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
                .unwrap();
            score.add((best.1 >= 0.0).then_some(best.0), r.class);
        }
        score.f_beta(BETA)
    }

    #[test]
    fn a_projection_that_separates_two_gaussians_raises_held_out_f() {
        let rows = two_gaussians(16, 200, 0xC0FFEE);
        let cut = rows.len() * 6 / 10;
        let (fit, eval) = rows.split_at(cut);
        let p = fit_projection("m@1", fit, full(0.05)).unwrap();
        let before = held_out_f(fit, eval, None);
        let after = held_out_f(fit, eval, Some(&p));
        assert!(
            after > before,
            "whitening made it worse: {before:.3} -> {after:.3}"
        );
        assert_eq!(p.n_classes, 2);
        assert_eq!(p.dim, 16);
    }

    #[test]
    fn a_degenerate_class_does_not_blow_up_the_fit() {
        // One class of many rows, one class of exactly one row, and a
        // dimension that never moves at all.
        let mut rows = two_gaussians(8, 30, 7);
        for r in &mut rows {
            r.v[5] = 0.0;
        }
        rows.push(Labelled {
            class: 99,
            v: vec![0.0; 8],
        });
        let p = fit_projection("m@1", &rows, full(0.1)).unwrap();
        assert!(p.a.iter().all(|v| v.is_finite()), "the map has non-finites");
        assert!(p.mean.iter().all(|v| v.is_finite()));
        // And it still maps a real vector to something usable.
        let y = p.apply(&Embedding::new("m@1", rows[0].v.clone())).unwrap();
        assert!(y.vector.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn a_fit_with_nothing_to_learn_from_is_an_error_not_a_matrix() {
        assert!(fit_projection("m@1", &[], full(0.1)).is_err());
        // One person, however many turns: there is no *within versus between*
        // to estimate.
        let one: Vec<Labelled> = (0..50)
            .map(|i| Labelled {
                class: 1,
                v: vec![i as f32, 1.0, 2.0],
            })
            .collect();
        assert!(fit_projection("m@1", &one, full(0.1)).is_err());
    }

    #[test]
    fn a_projection_refuses_a_vector_from_another_space() {
        let p = Projection::identity("m@1", 4);
        assert!(p.apply(&Embedding::new("other@1", vec![1.0; 4])).is_err());
        assert!(p.apply(&Embedding::new("m@1", vec![1.0; 5])).is_err());
    }

    #[test]
    fn the_identity_projection_changes_nothing() {
        let p = Projection::identity("m@1", 3);
        let e = Embedding::new("m@1", vec![0.3, -0.7, 2.0]);
        assert_eq!(p.apply(&e).unwrap(), e);
    }

    #[test]
    fn a_projection_survives_a_round_trip_through_a_blob() {
        let rows = two_gaussians(6, 20, 42);
        let p = fit_projection("m@1", &rows, full(0.2)).unwrap();
        let back = Projection::from_blob("m@1", 6, &p.to_blob()).unwrap();
        assert_eq!(back.mean, p.mean);
        assert_eq!(back.a, p.a);
        assert!(Projection::from_blob("m@1", 7, &p.to_blob()).is_err());
    }

    #[test]
    fn jacobi_reproduces_a_known_decomposition() {
        // Diagonal-plus-rotation: eigenvalues 1 and 3.
        let mut a = vec![2.0, 1.0, 1.0, 2.0];
        let (vals, vecs) = jacobi_eigh(&mut a, 2);
        let mut sorted = vals.clone();
        sorted.sort_by(|x, y| x.partial_cmp(y).unwrap());
        assert!((sorted[0] - 1.0).abs() < 1e-9, "{sorted:?}");
        assert!((sorted[1] - 3.0).abs() < 1e-9, "{sorted:?}");
        // Columns are orthonormal.
        for k in 0..2 {
            let norm: f64 = (0..2).map(|i| vecs[i * 2 + k].powi(2)).sum();
            assert!((norm - 1.0).abs() < 1e-9);
        }
    }

    #[test]
    fn whitening_makes_the_within_class_covariance_isotropic() {
        // The point of the map, checked directly: after it, one person's own
        // turns scatter about equally in every direction.
        let rows = two_gaussians(8, 300, 99);
        let p = fit_projection("m@1", &rows, full(0.0)).unwrap();
        let mapped: Vec<Labelled> = rows
            .iter()
            .map(|r| Labelled {
                class: r.class,
                v: p.apply(&Embedding::new("m@1", r.v.clone())).unwrap().vector,
            })
            .collect();
        let (_, a2, _) = fit_whitening(&mapped, full(0.0)).unwrap();
        // Sw of the mapped rows should be the identity, so its inverse square
        // root should be too.
        for i in 0..8 {
            for j in 0..8 {
                let want = if i == j { 1.0 } else { 0.0 };
                assert!(
                    (a2[i * 8 + j] - want).abs() < 5e-2,
                    "entry ({i},{j}) was {}",
                    a2[i * 8 + j]
                );
            }
        }
    }

    // ---- the overlap gate -------------------------------------------------

    #[test]
    fn the_overlap_curve_counts_all_four_cells() {
        let rows = vec![
            (true, 0.50),  // caught at every threshold in the grid
            (true, 0.02),  // missed at every threshold
            (false, 0.50), // refused for nothing
            (false, 0.01), // passed
        ];
        let pts = overlap_curve(&rows, &[0.10]);
        assert_eq!(pts[0].caught, 1);
        assert_eq!(pts[0].missed, 1);
        assert_eq!(pts[0].false_refusals, 1);
        assert_eq!(pts[0].passed, 1);
        assert!((pts[0].precision() - 0.5).abs() < 1e-9);
        assert!((pts[0].recall() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn the_gate_is_strictly_greater_than_like_the_ladder() {
        // `identity::gate` refuses when the fraction is *over* the threshold;
        // a turn sitting exactly on it goes through.
        let pts = overlap_curve(&[(true, 0.10)], &[0.10]);
        assert_eq!(pts[0].caught, 0);
        assert_eq!(pts[0].missed, 1);
    }

    #[test]
    fn raising_the_gate_never_catches_more() {
        let rows: Vec<(bool, f32)> = (0..100)
            .map(|i| (i % 3 == 0, (i as f32 % 31.0) / 100.0))
            .collect();
        let pts = overlap_curve(&rows, &overlap_grid());
        for w in pts.windows(2) {
            assert!(w[1].caught <= w[0].caught);
            assert!(w[1].false_refusals <= w[0].false_refusals);
        }
    }
}
