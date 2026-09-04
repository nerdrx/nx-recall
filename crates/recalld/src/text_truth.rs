//! Every hand-typed correction, kept as word-level ground truth — and the
//! decoder-choice rules fitted to it (0.12.4, schema v19).
//!
//! ## Why the table exists
//!
//! `crate::accuracy` already reads corrections out of the operations log and
//! turns them into an error rate. That answers "how wrong was it", which is a
//! dashboard question. It cannot answer the question this module is about:
//! **which of the three decoders that read this clip was right**.
//!
//! To answer that you need, on one row: the words a person typed, and beside
//! them what the live pass read, what the context re-decode read, and what the
//! night shift read. Every one of those already exists on disk — the live and
//! context readings inside `segments.redecode` operations, the night reading in
//! `segments.night_text` — and every one of them is expensive and fragile to
//! reassemble, because it means walking a log backwards and deciding which
//! state was in force when. `text_truth` does that walk **once**, at the moment
//! the truth is made, and writes the answer down.
//!
//! It is derived data. Nothing renders from it, nothing on the wire depends on
//! it, and dropping the table loses no user-visible state — it is rebuilt from
//! the operations history on the next open ([`backfill`]).
//!
//! ## How a reading is attributed to a pass
//!
//! A row's word-history is a chain: the live pass wrote text₀, a re-decode
//! replaced it with text₁ and kept text₀ in an operation, the night shift
//! replaced that and kept text₁, and so on. Each `segments.redecode` operation
//! carries the state *before* it, `{text, text_via}`, and its own timestamp.
//!
//! So for a correction at time `T` over prior text `P`:
//!
//! * every operation **at or before** `T` contributes one reading, filed under
//!   the pass its `text_via` names;
//! * `P` itself is filed under the route named by the **first operation after
//!   `T`** — that operation replaced `P`, so its `prior_state.text_via` is
//!   precisely how `P` got there — falling back to the row's current `text_via`
//!   when no machine has touched the row since;
//! * operations **after** `T` contribute nothing else, because the text they
//!   kept is the corrected text or something derived from it, and scoring the
//!   truth against itself is not a measurement.
//!
//! `night_text` is read off the segment instead, because the night shift stores
//! its reading there whether or not the vote let it win — an annotation is a
//! reading, and it is the one the vote most needs measuring.
//!
//! **`canary_text` is always NULL.** The cross-check decoder stores its
//! *verdict* and not its words (`crate::quality`; `crate::night` re-decodes the
//! clip when it needs the actual sentence). The column exists so that the day
//! that changes is a write rather than a migration, and until then the honest
//! value is the null.
//!
//! ## What is learned from it
//!
//! [`learn`] cuts the truth rows into **cells** — one voice, one source kind,
//! one duration bucket — and, per cell, measures each decoder's words against
//! the person's. The rules it may reach are deliberately few:
//!
//! * which decoder's words should win in that cell ([`Decoder`]), and
//! * whether `crate::night`'s two-of-three vote should be replaced there
//!   ([`VoteRule`]).
//!
//! Four honesty rules. Three are inherited from `crate::calib` because they are
//! the same rules and copying them with different numbers would be a second
//! opinion nobody asked for; the fourth the archive taught this pass on its
//! first run (FINDINGS §43.4):
//!
//! 1. **A minimum sample.** A cell with fewer than [`MIN_ROWS_PER_CELL`]
//!    corrections inherits the global decision rather than fitting its own.
//!    Thirty is `calib::MIN_ROWS_PER_VOICE`, for the same reason: below it the
//!    fit is describing the noise in a handful of evenings.
//! 2. **A minimum sample for the comparison too**, [`MIN_HELD_OUT`]. Not
//!    implied by the first: a decoder is scored only on the rows it read, and
//!    thirty corrections are not thirty measurements of every decoder.
//! 3. **A chronological hold-out.** [`crate::calib::split_at`] with
//!    [`crate::calib::FIT_FRACTION`], over the correction timestamps. Nothing
//!    the fit saw is allowed to score it.
//! 4. **A margin.** A rule ships only if it lowers held-out error against the
//!    truth by at least [`MARGIN_PP`] percentage points. Without it every
//!    rounding-error improvement installs itself and the decoder choice becomes
//!    a thing that moves for reasons nobody can point at — `calib`'s
//!    `improvement_is_material`, restated for this measurement.
//!
//! A cell that clears none of those ships **no rule at all**, and the shipped
//! two-of-three vote stands. That is the state this install is in today; see
//! `spike/FINDINGS.md` §43 for the count.

use std::collections::BTreeMap;

use anyhow::Result;
use serde_json::{Value, json};

use crate::canary::word_edits;
use crate::store::{SegmentFacets, Store, TextTruth, text_via};

/// How many corrections a cell needs before it is fitted rather than
/// inheriting the global decision. `crate::calib::MIN_ROWS_PER_VOICE`, for the
/// same reason it is thirty there.
pub const MIN_ROWS_PER_CELL: usize = 30;

/// How many rows a head-to-head comparison must have before its answer counts.
///
/// [`MIN_ROWS_PER_CELL`] is a floor on the cell; this is the floor on the
/// **comparison**, and the archive is why it exists rather than being implied.
/// A decoder is only scored on the rows it read, and on a real archive the
/// second and third readings are sparse: the first run of this pass over 37
/// corrections found a cell with 37 rows in it whose live-against-context
/// comparison rested on **four**, and shipped a rule off it. Thirty corrections
/// are not thirty measurements of every decoder.
///
/// Twelve is `MIN_ROWS_PER_CELL` after the 60% fit split — exactly the hold-out
/// the minimum sample was chosen to buy — and `the_two_minimums_agree` holds it
/// to that arithmetic.
pub const MIN_HELD_OUT: usize = 12;

/// How much held-out error a rule has to remove, in percentage points, before
/// it is worth changing the decoder choice for.
pub const MARGIN_PP: f64 = 2.0;

/// `settings` key holding the learned rules. Absent means "nothing has been
/// learned", which is not the same as "the defaults were chosen".
pub const RULES_KEY: &str = "text.decoder_rules";

/// How many truth rows one pass reads. A person who has corrected more than
/// this has a better fit than they need — the same ceiling, for the same
/// reason, as `accuracy::MAX_CORRECTIONS`.
pub const MAX_TRUTH_ROWS: usize = 100_000;

// ---------------------------------------------------------------------------
// the cell
// ---------------------------------------------------------------------------

/// How long a turn is, in the three sizes that behave differently.
///
/// The boundaries are the ones the rest of the daemon already reasons in:
/// under two seconds is the "H", "Yeah", "Mm-hmm" end of the corpus where the
/// cross-check refuses to have an opinion at all, and past six seconds a turn
/// carries enough context that a decoder's language model is doing most of the
/// work. On this archive they split the corrections 32 / 5 / 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Bucket {
    Short,
    Mid,
    Long,
}

/// Under this many seconds a turn is [`Bucket::Short`].
pub const SHORT_MAX_S: f64 = 2.0;
/// Under this many seconds a turn is [`Bucket::Mid`]; at or above it, long.
pub const MID_MAX_S: f64 = 6.0;

impl Bucket {
    pub fn of_ns(duration_ns: i64) -> Self {
        let s = duration_ns.max(0) as f64 / 1e9;
        if s < SHORT_MAX_S {
            Bucket::Short
        } else if s < MID_MAX_S {
            Bucket::Mid
        } else {
            Bucket::Long
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Bucket::Short => "short",
            Bucket::Mid => "mid",
            Bucket::Long => "long",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "short" => Some(Bucket::Short),
            "mid" => Some(Bucket::Mid),
            "long" => Some(Bucket::Long),
            _ => None,
        }
    }
}

/// One cut of the corpus: whose voice, what kind of source, how long a turn.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Cell {
    pub source_kind: String,
    /// `None` is a real cell, not a missing one: an unlabelled voice is a voice
    /// the identity leg could not name, and it has its own error rate.
    pub speaker_id: Option<i64>,
    pub bucket: Bucket,
}

impl Cell {
    pub fn of(facets: &SegmentFacets) -> Self {
        Self {
            source_kind: facets.source_kind.clone(),
            speaker_id: facets.speaker_id,
            bucket: Bucket::of_ns(facets.duration_ns),
        }
    }

    fn of_row(row: &TextTruth) -> Self {
        Self {
            source_kind: row.source_kind.clone(),
            speaker_id: row.speaker_id,
            bucket: Bucket::of_ns(row.duration_ns),
        }
    }

    /// `app/25/short`. The key the rules map is stored under, and the label the
    /// per-cell table prints.
    pub fn key(&self) -> String {
        format!(
            "{}/{}/{}",
            self.source_kind,
            self.speaker_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "-".into()),
            self.bucket.as_str()
        )
    }
}

// ---------------------------------------------------------------------------
// the decoders and the vote
// ---------------------------------------------------------------------------

/// Which pass's words a cell's rule prefers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decoder {
    Live,
    Context,
    Night,
}

impl Decoder {
    pub const ALL: [Decoder; 3] = [Decoder::Live, Decoder::Context, Decoder::Night];

    pub fn as_str(self) -> &'static str {
        match self {
            Decoder::Live => "live",
            Decoder::Context => "context",
            Decoder::Night => "night",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "live" => Some(Decoder::Live),
            "context" => Some(Decoder::Context),
            "night" => Some(Decoder::Night),
            _ => None,
        }
    }

    fn reading(self, row: &TextTruth) -> Option<&str> {
        match self {
            Decoder::Live => row.live_text.as_deref(),
            Decoder::Context => row.context_text.as_deref(),
            Decoder::Night => row.night_text.as_deref(),
        }
    }
}

/// What `crate::night` is allowed to do in one cell.
///
/// [`VoteRule::TwoOfThree`] is the shipped rule and the default everywhere: the
/// night reading replaces a row only when the cross-check decoder independently
/// agrees with it and both disagree with the stored words. The other two are
/// the only replacements a measurement over corrections could justify, and
/// neither is reachable without clearing every gate in [`learn`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VoteRule {
    /// The 0.9.0 rule. `crate::night::judge_vote`.
    #[default]
    TwoOfThree,
    /// The night reading may take the row on its own, with every guard still
    /// applied. Shipped for a cell only where the night decoder measurably
    /// beats the live one against the user's own corrections.
    NightWins,
    /// The night reading is never allowed to replace anything here; it is kept
    /// beside the row as an annotation. Shipped for a cell where the night
    /// decoder is measurably *worse* than the live one.
    KeepLive,
}

impl VoteRule {
    pub fn as_str(self) -> &'static str {
        match self {
            VoteRule::TwoOfThree => "two_of_three",
            VoteRule::NightWins => "night_wins",
            VoteRule::KeepLive => "keep_live",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "two_of_three" => Some(VoteRule::TwoOfThree),
            "night_wins" => Some(VoteRule::NightWins),
            "keep_live" => Some(VoteRule::KeepLive),
            _ => None,
        }
    }
}

/// One cell's shipped decision.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellRule {
    pub winner: Decoder,
    pub vote: VoteRule,
    /// Corrections the decision was fitted on, and how many of them were held
    /// out. Carried so a rule can explain itself without a second query.
    pub rows: usize,
    pub held_out: usize,
    /// Held-out error of the incumbent (`live`) and of the winner, over the
    /// rows both of them read.
    pub wer_live: f64,
    pub wer_winner: f64,
}

impl CellRule {
    fn to_json(self) -> Value {
        json!({
            "winner": self.winner.as_str(),
            "vote": self.vote.as_str(),
            "rows": self.rows,
            "held_out": self.held_out,
            "wer_live": self.wer_live,
            "wer_winner": self.wer_winner,
        })
    }

    fn from_json(v: &Value) -> Option<Self> {
        Some(Self {
            winner: Decoder::parse(v.get("winner")?.as_str()?)?,
            vote: VoteRule::parse(v.get("vote")?.as_str()?)?,
            rows: v.get("rows").and_then(Value::as_u64).unwrap_or(0) as usize,
            held_out: v.get("held_out").and_then(Value::as_u64).unwrap_or(0) as usize,
            wer_live: v
                .get("wer_live")
                .and_then(Value::as_f64)
                .unwrap_or(f64::NAN),
            wer_winner: v
                .get("wer_winner")
                .and_then(Value::as_f64)
                .unwrap_or(f64::NAN),
        })
    }
}

/// Every rule this install has learned, as `settings` keeps them.
///
/// An empty `Rules` is the shipped state and the correct one on a machine with
/// no corrections: `for_cell` then answers with the defaults, which is exactly
/// what 0.9.0 did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Rules {
    pub learned_ns: i64,
    /// How many truth rows the pass that wrote these read.
    pub corrections: usize,
    /// The fallback for a cell too small to fit its own, when one was learned.
    pub global: Option<CellRule>,
    pub cells: BTreeMap<String, CellRule>,
}

impl Rules {
    /// What to do in this cell: its own rule, else the global one, else the
    /// shipped defaults.
    pub fn for_cell(&self, cell: &Cell) -> CellRule {
        self.cells
            .get(&cell.key())
            .copied()
            .or(self.global)
            .unwrap_or(CellRule {
                winner: Decoder::Live,
                vote: VoteRule::TwoOfThree,
                rows: 0,
                held_out: 0,
                wer_live: f64::NAN,
                wer_winner: f64::NAN,
            })
    }

    /// The vote `crate::night` should run for one segment.
    pub fn vote_for(&self, cell: &Cell) -> VoteRule {
        self.for_cell(cell).vote
    }

    pub fn is_empty(&self) -> bool {
        self.global.is_none() && self.cells.is_empty()
    }

    pub fn to_json(&self) -> Value {
        json!({
            "learned_ns": self.learned_ns.to_string(),
            "learned_ms": crate::clock::ns_to_ms(self.learned_ns),
            "corrections": self.corrections,
            "min_rows_per_cell": MIN_ROWS_PER_CELL,
            "margin_pp": MARGIN_PP,
            "global": self.global.map(CellRule::to_json),
            "cells": self
                .cells
                .iter()
                .map(|(k, r)| (k.clone(), r.to_json()))
                .collect::<serde_json::Map<_, _>>(),
        })
    }

    pub fn from_json(v: &Value) -> Self {
        Self {
            learned_ns: v
                .get("learned_ns")
                .and_then(Value::as_str)
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
            corrections: v.get("corrections").and_then(Value::as_u64).unwrap_or(0) as usize,
            global: v.get("global").and_then(CellRule::from_json),
            cells: v
                .get("cells")
                .and_then(Value::as_object)
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| Some((k.clone(), CellRule::from_json(v)?)))
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    /// Read what is installed. A malformed or absent value is an empty `Rules`
    /// and never an error: a night shift must not stop because a setting could
    /// not be parsed.
    pub fn load(store: &Store) -> Self {
        match store.setting(RULES_KEY) {
            Ok(Some(raw)) => serde_json::from_str::<Value>(&raw)
                .map(|v| Rules::from_json(&v))
                .unwrap_or_default(),
            _ => Rules::default(),
        }
    }

    pub fn save(&self, store: &Store) -> Result<()> {
        store.set_setting(RULES_KEY, &self.to_json().to_string())
    }
}

// ---------------------------------------------------------------------------
// writing the truth down
// ---------------------------------------------------------------------------

/// Which pass a `text_via` names, for the purpose of filing a reading.
///
/// `arbiter` and `lid` are filed with `context`: all three are the same act —
/// a machine reading the clip again with more information than the live pass
/// had — and splitting them would make three cells out of one where the
/// archive has barely enough for one.
fn slot_of(via: Option<&str>) -> Decoder {
    match via {
        Some(text_via::NIGHT) => Decoder::Night,
        Some(text_via::CONTEXT) | Some(text_via::ARBITER) | Some(text_via::LID) => Decoder::Context,
        // `live`, and the NULL on every row written before `text_via` existed.
        _ => Decoder::Live,
    }
}

/// The three readings, assembled from one row's history. Pure, so the
/// attribution rule in this module's header can be tested without a database.
///
/// `priors` must be oldest first. `at` is when the correction happened,
/// `prior_text` the words it replaced, `current_via` the row's `text_via`
/// (which `segments.correct` does not touch, so after a correction it still
/// names the route of the words that were replaced).
pub fn attribute(
    priors: &[crate::store::RedecodePrior],
    at: i64,
    prior_text: Option<&str>,
    current_via: Option<&str>,
    row_night: Option<&str>,
) -> [Option<String>; 3] {
    let mut out: [Option<String>; 3] = [None, None, None];
    let index = |d: Decoder| match d {
        Decoder::Live => 0,
        Decoder::Context => 1,
        Decoder::Night => 2,
    };
    for p in priors.iter().filter(|p| p.at_utc_ns <= at) {
        if let Some(text) = p.text.as_deref().filter(|t| !t.trim().is_empty()) {
            out[index(slot_of(p.text_via.as_deref()))] = Some(text.to_string());
        }
    }
    // The words the correction replaced belong to whichever route put them
    // there — named by the operation that later replaced *them*, or by the row
    // itself when nothing has.
    let via = priors
        .iter()
        .find(|p| p.at_utc_ns > at)
        .and_then(|p| p.text_via.clone())
        .or_else(|| current_via.map(str::to_string));
    if let Some(text) = prior_text.filter(|t| !t.trim().is_empty()) {
        out[index(slot_of(via.as_deref()))] = Some(text.to_string());
    }
    // The night shift writes its reading to the row whether or not the vote let
    // it win, so the row is the better source: an annotation is a reading.
    if let Some(night) = row_night.filter(|t| !t.trim().is_empty()) {
        out[2] = Some(night.to_string());
    }
    out
}

/// Record one correction as ground truth. Called by `segments.correct` after
/// the row has been rewritten, and by [`backfill`] for the ones that predate
/// this table.
///
/// `Ok(false)` means the row was already there — the natural key is
/// `(segment_id, created_ns)`, so replaying history is a no-op rather than a
/// duplicate.
pub fn record(
    store: &Store,
    segment_id: i64,
    truth_text: &str,
    prior_text: Option<&str>,
    at_utc_ns: i64,
) -> Result<bool> {
    let Some(facets) = store.segment_facets(segment_id)? else {
        // Purged between the correction and this call. There is nothing left to
        // attach a measurement to.
        return Ok(false);
    };
    let priors = store.redecode_priors(segment_id)?;
    let [live, context, night] = attribute(
        &priors,
        at_utc_ns,
        prior_text,
        facets.text_via.as_deref(),
        facets.night_text.as_deref(),
    );
    store.insert_text_truth(&TextTruth {
        segment_id,
        truth_text: truth_text.to_string(),
        live_text: live,
        context_text: context,
        night_text: night,
        // The cross-check keeps a verdict, not words. See the header.
        canary_text: None,
        asr_confidence: facets.asr_confidence.clone(),
        speaker_id: facets.speaker_id,
        source_kind: facets.source_kind.clone(),
        duration_ns: facets.duration_ns,
        created_ns: at_utc_ns,
    })
}

/// Fill the table from the corrections already on disk. Idempotent, and run on
/// every open by `Store::apply_v19` — the whole cost on an archive with no new
/// corrections is one indexed query.
///
/// The corrected text is recovered exactly as `accuracy::corrections` recovers
/// it, because it is the same recovery and two of them would drift: each
/// correction's result is the next correction of that segment's `prior_state`,
/// and the last one's result is the row as it now stands.
pub fn backfill(store: &Store) -> Result<usize> {
    let ops = store.operations_of(crate::accuracy::OP, MAX_TRUTH_ROWS)?;
    let mut by_segment: BTreeMap<i64, Vec<(i64, Option<String>)>> = BTreeMap::new();
    for op in &ops {
        let Ok(prior) = serde_json::from_str::<Value>(&op.prior_state) else {
            continue;
        };
        let Some(segment_id) = prior["segment_id"].as_i64() else {
            continue;
        };
        by_segment
            .entry(segment_id)
            .or_default()
            .push((op.at_utc_ns, prior["text"].as_str().map(str::to_string)));
    }

    let mut written = 0usize;
    for (segment_id, entries) in by_segment {
        let Some(facets) = store.segment_facets(segment_id)? else {
            continue;
        };
        let current = facets.text.clone().unwrap_or_default();
        for (i, (at, prior_text)) in entries.iter().enumerate() {
            let truth = match entries.get(i + 1) {
                Some((_, Some(next))) => next.clone(),
                // The next correction replaced a row that had no words, which
                // cannot be this one's result; and there is nothing else to
                // read it from.
                Some((_, None)) => continue,
                None => current.clone(),
            };
            if record(store, segment_id, &truth, prior_text.as_deref(), *at)? {
                written += 1;
            }
        }
    }
    Ok(written)
}

// ---------------------------------------------------------------------------
// the measurement
// ---------------------------------------------------------------------------

/// One decoder's score in one cell.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Score {
    /// Rows this decoder actually read — it is not scored on rows it never saw.
    pub rows: usize,
    pub edits: usize,
    pub words: usize,
}

impl Score {
    fn add(&mut self, edits: usize, words: usize) {
        self.rows += 1;
        self.edits += edits;
        self.words += words;
    }

    /// The bounded corpus edit share: word edits summed over the rows, divided
    /// by the summed longer word count of each pair. `None` with nothing to
    /// divide by — which is not an error rate of zero.
    ///
    /// A share and not a mean of per-line rates, which is the mistake 0.10.1
    /// found on the accuracy card: one two-word line retyped as ten scores 400%
    /// on its own and drags a thirteen-line average past 100%.
    pub fn wer(&self) -> Option<f64> {
        (self.words > 0).then(|| self.edits as f64 / self.words as f64)
    }
}

/// What one cell's measurement found, whether or not it produced a rule.
#[derive(Debug, Clone)]
pub struct CellReport {
    pub cell: Cell,
    pub rows: usize,
    pub held_out: usize,
    /// Held-out score per decoder, in [`Decoder::ALL`] order.
    pub scores: [Score; 3],
    /// The rule this cell ships, or `None` — the two-of-three vote stands and
    /// the live words keep the row.
    pub rule: Option<CellRule>,
    /// Why, in one line, for the table and the card.
    pub verdict: String,
}

impl CellReport {
    fn to_json(&self) -> Value {
        json!({
            "cell": self.cell.key(),
            "source_kind": self.cell.source_kind,
            "speaker_id": self.cell.speaker_id,
            "bucket": self.cell.bucket.as_str(),
            "rows": self.rows,
            "held_out": self.held_out,
            "decoders": Decoder::ALL
                .iter()
                .enumerate()
                .map(|(i, d)| json!({
                    "decoder": d.as_str(),
                    "rows": self.scores[i].rows,
                    "wer": self.scores[i].wer(),
                }))
                .collect::<Vec<_>>(),
            "rule": self.rule.map(CellRule::to_json),
            "verdict": self.verdict,
        })
    }
}

/// Everything one `recalld accuracy learn` produced.
#[derive(Debug, Clone)]
pub struct Learned {
    pub corrections: usize,
    /// Corrections that could be scored against at least one decoder's reading.
    pub measurable: usize,
    pub global: CellReport,
    pub cells: Vec<CellReport>,
    pub rules: Rules,
    /// What the pass would need before it could decide anything, per cell.
    pub short_by: Vec<(Cell, usize)>,
}

impl Learned {
    pub fn to_json(&self, applied: bool, installed: &Rules) -> Value {
        json!({
            "corrections": self.corrections,
            "measurable": self.measurable,
            "min_rows_per_cell": MIN_ROWS_PER_CELL,
            "margin_pp": MARGIN_PP,
            "fit_fraction": crate::calib::FIT_FRACTION,
            "global": self.global.to_json(),
            "cells": self.cells.iter().map(CellReport::to_json).collect::<Vec<_>>(),
            "short_by": self
                .short_by
                .iter()
                .map(|(cell, need)| json!({"cell": cell.key(), "needed": need}))
                .collect::<Vec<_>>(),
            "rules": self.rules.to_json(),
            "applied": applied,
            "installed": installed.to_json(),
        })
    }
}

/// Score one decoder against the truth on the rows it read.
fn score(rows: &[&TextTruth], decoder: Decoder) -> Score {
    let mut s = Score::default();
    for row in rows {
        let Some(heard) = decoder.reading(row) else {
            continue;
        };
        let (edits, words) = word_edits(heard, &row.truth_text);
        if words == 0 {
            // A correction that emptied a line. A deletion is not an error
            // rate: there is nothing to divide by.
            continue;
        }
        s.add(edits, words);
    }
    s
}

/// Score two decoders on **exactly the rows both of them read**, which is the
/// only fair comparison: a decoder that answered on the four easy turns and
/// stayed silent on the twenty hard ones must not win on its average.
fn head_to_head(rows: &[&TextTruth], a: Decoder, b: Decoder) -> (Score, Score) {
    let both: Vec<&TextTruth> = rows
        .iter()
        .copied()
        .filter(|r| a.reading(r).is_some() && b.reading(r).is_some())
        .collect();
    (score(&both, a), score(&both, b))
}

/// One cell's whole decision.
fn judge(cell: Cell, rows: &[&TextTruth], inherit: Option<CellRule>) -> CellReport {
    let times: Vec<i64> = rows.iter().map(|r| r.created_ns).collect();
    let cut = crate::calib::split_at(&times, crate::calib::FIT_FRACTION);
    let eval: Vec<&TextTruth> = rows[cut.min(rows.len())..].to_vec();
    let scores = [
        score(&eval, Decoder::Live),
        score(&eval, Decoder::Context),
        score(&eval, Decoder::Night),
    ];

    if rows.len() < MIN_ROWS_PER_CELL {
        return CellReport {
            cell,
            rows: rows.len(),
            held_out: eval.len(),
            scores,
            rule: inherit,
            verdict: format!(
                "{} of {MIN_ROWS_PER_CELL} corrections — inheriting {}",
                rows.len(),
                inherit.map_or("the shipped rule", |_| "the global rule")
            ),
        };
    }
    if eval.is_empty() {
        return CellReport {
            cell,
            rows: rows.len(),
            held_out: 0,
            scores,
            rule: inherit,
            verdict: "every correction landed in one instant — no honest hold-out".into(),
        };
    }

    // The incumbent is the live pass: those are the words a row carries unless
    // something took them.
    let live = score(&eval, Decoder::Live);
    let Some(live_wer) = live.wer() else {
        return CellReport {
            cell,
            rows: rows.len(),
            held_out: eval.len(),
            scores,
            rule: inherit,
            verdict: "the live pass read none of the held-out rows".into(),
        };
    };

    let mut best: Option<(Decoder, f64, f64)> = None;
    let mut thin: Vec<(Decoder, usize)> = Vec::new();
    for challenger in [Decoder::Context, Decoder::Night] {
        let (mine, theirs) = head_to_head(&eval, Decoder::Live, challenger);
        // The comparison needs a sample of its own, not the cell's — see
        // `MIN_HELD_OUT`. A decoder that read four of the held-out rows has an
        // opinion, not a measurement.
        if mine.rows < MIN_HELD_OUT {
            thin.push((challenger, mine.rows));
            continue;
        }
        let (Some(base), Some(cand)) = (mine.wer(), theirs.wer()) else {
            continue;
        };
        if (base - cand) * 100.0 >= MARGIN_PP - 1e-9 && best.is_none_or(|(_, _, b)| cand < b) {
            best = Some((challenger, base, cand));
        }
    }

    if let Some((winner, base, cand)) = best {
        let vote = match winner {
            Decoder::Night => VoteRule::NightWins,
            // A context re-decode is not the night shift's decision to make,
            // so the vote there stays what it was.
            _ => VoteRule::TwoOfThree,
        };
        let rule = CellRule {
            winner,
            vote,
            rows: rows.len(),
            held_out: eval.len(),
            wer_live: base,
            wer_winner: cand,
        };
        return CellReport {
            cell,
            rows: rows.len(),
            held_out: eval.len(),
            scores,
            rule: Some(rule),
            verdict: format!(
                "{} beats live held-out, {:.1}% → {:.1}%",
                winner.as_str(),
                base * 100.0,
                cand * 100.0
            ),
        };
    }

    // Nothing beat the live pass. The one rule left worth shipping is the
    // refusal: if the night decoder is measurably WORSE here, its vote should
    // not be able to take a row in this cell at all.
    let (live_vs_night, night) = head_to_head(&eval, Decoder::Live, Decoder::Night);
    if live_vs_night.rows >= MIN_HELD_OUT
        && let (Some(base), Some(cand)) = (live_vs_night.wer(), night.wer())
        && (cand - base) * 100.0 >= MARGIN_PP - 1e-9
    {
        let rule = CellRule {
            winner: Decoder::Live,
            vote: VoteRule::KeepLive,
            rows: rows.len(),
            held_out: eval.len(),
            wer_live: base,
            wer_winner: base,
        };
        return CellReport {
            cell,
            rows: rows.len(),
            held_out: eval.len(),
            scores,
            rule: Some(rule),
            verdict: format!(
                "night is worse held-out, {:.1}% against live's {:.1}% — it may not replace here",
                cand * 100.0,
                base * 100.0
            ),
        };
    }

    CellReport {
        cell,
        rows: rows.len(),
        held_out: eval.len(),
        scores,
        rule: None,
        verdict: if thin.len() == 2 {
            format!(
                "no challenger read {MIN_HELD_OUT} held-out rows ({}) — the shipped vote stands",
                thin.iter()
                    .map(|(d, n)| format!("{} on {n}", d.as_str()))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        } else {
            format!(
                "nothing clears {MARGIN_PP:.0} pp against live's {:.1}% — the shipped vote stands",
                live_wer * 100.0
            )
        },
    }
}

/// The whole pass. Reads; writes nothing. [`Learned::rules`] is what `--apply`
/// would install.
pub fn learn(store: &Store) -> Result<Learned> {
    let all = store.text_truth_rows(MAX_TRUTH_ROWS)?;
    let refs: Vec<&TextTruth> = all.iter().collect();
    let measurable = refs
        .iter()
        .filter(|r| Decoder::ALL.iter().any(|d| d.reading(r).is_some()))
        .count();

    // The global cell first: it is what every under-sampled cell inherits.
    let global = judge(
        Cell {
            source_kind: "*".into(),
            speaker_id: None,
            bucket: Bucket::Short,
        },
        &refs,
        None,
    );
    let inherit = global.rule;

    let mut by_cell: BTreeMap<Cell, Vec<&TextTruth>> = BTreeMap::new();
    for row in &refs {
        by_cell.entry(Cell::of_row(row)).or_default().push(row);
    }

    let mut cells = Vec::new();
    let mut short_by = Vec::new();
    for (cell, rows) in by_cell {
        if rows.len() < MIN_ROWS_PER_CELL {
            short_by.push((cell.clone(), MIN_ROWS_PER_CELL - rows.len()));
        }
        cells.push(judge(cell, &rows, inherit));
    }
    cells.sort_by(|a, b| b.rows.cmp(&a.rows).then(a.cell.key().cmp(&b.cell.key())));

    let mut rules = Rules {
        learned_ns: crate::clock::utc_now_ns(),
        corrections: all.len(),
        global: global.rule,
        cells: BTreeMap::new(),
    };
    for report in &cells {
        // Only a cell that fitted its OWN rule is written down. A cell echoing
        // the global one is not a claim about that cell.
        if report.rows >= MIN_ROWS_PER_CELL
            && let Some(rule) = report.rule
            && Some(rule) != inherit
        {
            rules.cells.insert(report.cell.key(), rule);
        }
    }

    Ok(Learned {
        corrections: all.len(),
        measurable,
        global,
        cells,
        rules,
        short_by,
    })
}

/// The block `accuracy.summary` carries, and the card renders (0.12.4).
///
/// Deliberately says both numbers: what has been learned, and what it would
/// take to learn anything. On an archive with a handful of corrections the
/// honest answer is the second one, and a card that printed only "0 rules"
/// would leave the user with no idea what to do about it.
pub fn summary_json(store: &Store) -> Result<Value> {
    let installed = Rules::load(store);
    let corrections = store.text_truth_count()?;
    Ok(json!({
        "corrections": corrections,
        "min_rows_per_cell": MIN_ROWS_PER_CELL,
        "margin_pp": MARGIN_PP,
        "rules": installed.cells.len() + usize::from(installed.global.is_some()),
        "learned_ns": (installed.learned_ns > 0).then(|| installed.learned_ns.to_string()),
        "learned_ms": (installed.learned_ns > 0).then(|| crate::clock::ns_to_ms(installed.learned_ns)),
        "ready": corrections as usize >= MIN_ROWS_PER_CELL,
        "needed": MIN_ROWS_PER_CELL.saturating_sub(corrections.max(0) as usize),
        "installed": installed.to_json(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{RedecodePrior, SegmentAnalysis};

    fn prior(at: i64, text: &str, via: Option<&str>) -> RedecodePrior {
        RedecodePrior {
            at_utc_ns: at,
            text: Some(text.into()),
            text_via: via.map(str::to_string),
        }
    }

    // ---- attribution ----

    #[test]
    fn with_no_redecode_at_all_the_replaced_words_are_the_live_pass() {
        let out = attribute(&[], 100, Some("das war gut"), Some("live"), None);
        assert_eq!(out[0].as_deref(), Some("das war gut"));
        assert_eq!(out[1], None);
        assert_eq!(out[2], None);
    }

    #[test]
    fn a_row_written_before_text_via_existed_is_still_the_live_pass() {
        let out = attribute(&[], 100, Some("das war gut"), None, None);
        assert_eq!(out[0].as_deref(), Some("das war gut"));
    }

    #[test]
    fn the_context_redecode_and_the_words_it_replaced_are_two_readings() {
        // The live pass wrote "komm ich sag", a context re-decode replaced it,
        // and the person then corrected the re-decode's words.
        let priors = [prior(50, "komm ich sag", Some("live"))];
        let out = attribute(
            &priors,
            100,
            Some("komm ich sage dir"),
            Some("context"),
            None,
        );
        assert_eq!(out[0].as_deref(), Some("komm ich sag"));
        assert_eq!(out[1].as_deref(), Some("komm ich sage dir"));
    }

    #[test]
    fn the_route_of_the_replaced_words_comes_from_the_operation_that_replaced_them() {
        // A machine rewrote the row AFTER the correction. Its `prior_state`
        // names the route of the words the correction replaced, and the row's
        // current `text_via` describes something newer.
        let priors = [prior(200, "was der mensch tippte", Some("context"))];
        let out = attribute(
            &priors,
            100,
            Some("was der mensch tippte"),
            Some("night"),
            None,
        );
        assert_eq!(
            out[1].as_deref(),
            Some("was der mensch tippte"),
            "filed under the route the LATER operation named"
        );
        assert_eq!(out[0], None);
    }

    #[test]
    fn an_operation_after_the_correction_is_never_a_reading() {
        // Otherwise the corrected text comes back as a decoder's opinion and
        // the truth is scored against itself.
        let priors = [prior(200, "die korrigierten worte", Some("live"))];
        let out = attribute(&priors, 100, None, Some("live"), None);
        assert_eq!(out[0], None, "the post-correction state is not a reading");
    }

    #[test]
    fn the_night_annotation_is_a_reading_even_though_it_never_won() {
        let out = attribute(
            &[],
            100,
            Some("live words"),
            Some("live"),
            Some("night words"),
        );
        assert_eq!(out[0].as_deref(), Some("live words"));
        assert_eq!(out[2].as_deref(), Some("night words"));
    }

    #[test]
    fn arbiter_and_lid_are_filed_with_the_context_redecode() {
        for via in ["arbiter", "lid", "context"] {
            let out = attribute(&[], 100, Some("x y"), Some(via), None);
            assert_eq!(out[1].as_deref(), Some("x y"), "{via}");
        }
    }

    // ---- buckets and cells ----

    #[test]
    fn the_duration_buckets_are_the_three_the_corpus_actually_has() {
        assert_eq!(Bucket::of_ns(1_500_000_000), Bucket::Short);
        assert_eq!(Bucket::of_ns(2_000_000_000), Bucket::Mid);
        assert_eq!(Bucket::of_ns(5_999_000_000), Bucket::Mid);
        assert_eq!(Bucket::of_ns(6_000_000_000), Bucket::Long);
    }

    #[test]
    fn an_unlabelled_voice_is_its_own_cell_and_not_a_missing_one() {
        let c = Cell {
            source_kind: "app".into(),
            speaker_id: None,
            bucket: Bucket::Short,
        };
        assert_eq!(c.key(), "app/-/short");
    }

    // ---- the rules ----

    #[test]
    fn no_rules_means_the_shipped_vote_and_the_live_words() {
        let r = Rules::default();
        let cell = Cell {
            source_kind: "app".into(),
            speaker_id: Some(1),
            bucket: Bucket::Short,
        };
        assert_eq!(r.vote_for(&cell), VoteRule::TwoOfThree);
        assert_eq!(r.for_cell(&cell).winner, Decoder::Live);
    }

    #[test]
    fn a_cell_rule_beats_the_global_one_and_the_global_one_beats_the_default() {
        let cell = Cell {
            source_kind: "app".into(),
            speaker_id: Some(1),
            bucket: Bucket::Short,
        };
        let other = Cell {
            source_kind: "mic".into(),
            speaker_id: Some(2),
            bucket: Bucket::Mid,
        };
        let mut r = Rules {
            global: Some(CellRule {
                winner: Decoder::Live,
                vote: VoteRule::KeepLive,
                rows: 40,
                held_out: 16,
                wer_live: 0.2,
                wer_winner: 0.2,
            }),
            ..Default::default()
        };
        r.cells.insert(
            cell.key(),
            CellRule {
                winner: Decoder::Night,
                vote: VoteRule::NightWins,
                rows: 40,
                held_out: 16,
                wer_live: 0.5,
                wer_winner: 0.3,
            },
        );
        assert_eq!(r.vote_for(&cell), VoteRule::NightWins);
        assert_eq!(r.vote_for(&other), VoteRule::KeepLive);
    }

    #[test]
    fn the_rules_survive_a_round_trip_through_settings() {
        let mut r = Rules {
            learned_ns: 1_700_000_000_000_000_000,
            corrections: 41,
            global: None,
            cells: BTreeMap::new(),
        };
        r.cells.insert(
            "app/25/short".into(),
            CellRule {
                winner: Decoder::Night,
                vote: VoteRule::NightWins,
                rows: 33,
                held_out: 13,
                wer_live: 0.44,
                wer_winner: 0.31,
            },
        );
        let store = Store::open_in_memory().unwrap();
        r.save(&store).unwrap();
        assert_eq!(Rules::load(&store), r);
    }

    #[test]
    fn a_corrupt_setting_is_no_rules_rather_than_an_error() {
        let store = Store::open_in_memory().unwrap();
        store.set_setting(RULES_KEY, "{not json").unwrap();
        assert!(Rules::load(&store).is_empty());
    }

    // ---- the table, end to end ----

    struct Seed {
        store: Store,
        session: i64,
    }

    fn seed(kind: &str) -> Seed {
        let store = Store::open_in_memory().unwrap();
        let src = store
            .upsert_source_kind("VRChat.exe", "VRChat.exe", kind, 0)
            .unwrap();
        let session = store.begin_session(src, 0).unwrap();
        Seed { store, session }
    }

    fn a_turn(s: &Seed, t: i64, dur_ns: i64, speaker: Option<i64>, text: &str) -> i64 {
        let id = s
            .store
            .insert_segment(s.session, t, t + dur_ns, "segments/a.wav", 0)
            .unwrap();
        s.store
            .set_segment_analysis(
                id,
                &SegmentAnalysis {
                    text: Some(text.into()),
                    ..Default::default()
                },
            )
            .unwrap();
        if let Some(spk) = speaker {
            s.store
                .set_segment_speaker(id, Some(spk), Some(0.9))
                .unwrap();
        }
        id
    }

    fn correct(s: &Seed, segment_id: i64, was: Option<&str>, now: &str, at: i64) {
        s.store.correct_segment_text(segment_id, now).unwrap();
        s.store
            .log_operation(
                crate::accuracy::OP,
                &format!("[{segment_id}]"),
                &json!({"segment_id": segment_id, "text": was}).to_string(),
                at,
            )
            .unwrap();
    }

    #[test]
    fn a_correction_becomes_one_truth_row_with_the_readings_beside_it() {
        let s = seed("app");
        let spk = s.store.create_speaker("Aspen", 0).unwrap();
        let seg = a_turn(&s, 1_000, 1_500_000_000, Some(spk), "the belt holds");
        s.store
            .set_segment_night(seg, Some("the bell holds the line"), 50)
            .unwrap();
        correct(&s, seg, Some("the belt holds"), "the bell holds", 100);
        assert!(record(&s.store, seg, "the bell holds", Some("the belt holds"), 100).unwrap());

        let rows = s.store.text_truth_rows(10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].truth_text, "the bell holds");
        assert_eq!(rows[0].live_text.as_deref(), Some("the belt holds"));
        assert_eq!(
            rows[0].night_text.as_deref(),
            Some("the bell holds the line")
        );
        assert_eq!(rows[0].canary_text, None, "the cross-check stores no words");
        assert_eq!(rows[0].speaker_id, Some(spk));
        assert_eq!(rows[0].source_kind, "app");
        assert_eq!(Bucket::of_ns(rows[0].duration_ns), Bucket::Short);
    }

    #[test]
    fn recording_the_same_correction_twice_writes_one_row() {
        let s = seed("app");
        let seg = a_turn(&s, 1_000, 1_000_000_000, None, "a b c");
        correct(&s, seg, Some("a b c"), "a b d", 100);
        assert!(record(&s.store, seg, "a b d", Some("a b c"), 100).unwrap());
        assert!(!record(&s.store, seg, "a b d", Some("a b c"), 100).unwrap());
        assert_eq!(s.store.text_truth_count().unwrap(), 1);
    }

    #[test]
    fn the_backfill_recovers_both_corrections_of_a_turn_corrected_twice() {
        let s = seed("app");
        let seg = a_turn(&s, 1_000, 1_000_000_000, None, "a b c d");
        correct(&s, seg, Some("a b c d"), "a b c e", 10);
        correct(&s, seg, Some("a b c e"), "a b c f", 20);
        // The migration already ran on open, so start from a clean table.
        s.store
            .conn()
            .execute("DELETE FROM text_truth", [])
            .unwrap();
        assert_eq!(backfill(&s.store).unwrap(), 2);
        let rows = s.store.text_truth_rows(10).unwrap();
        assert_eq!(
            rows[0].truth_text, "a b c e",
            "measured against what replaced it"
        );
        assert_eq!(rows[0].live_text.as_deref(), Some("a b c d"));
        assert_eq!(
            rows[1].truth_text, "a b c f",
            "and the last against the row"
        );
        assert_eq!(backfill(&s.store).unwrap(), 0, "idempotent");
    }

    // ---- the fit ----

    /// Thirty-two corrections in one cell, where the night reading is right and
    /// the live one is wrong on every second row.
    fn many(s: &Seed, n: usize, night_right: bool) {
        for i in 0..n {
            let t = 1_000 + i as i64 * 1_000_000_000;
            let seg = a_turn(s, t, 1_000_000_000, Some(1), "wrong words here");
            let night = if night_right {
                "right words here"
            } else {
                "totally different nonsense entirely"
            };
            s.store.set_segment_night(seg, Some(night), t).unwrap();
            record(
                &s.store,
                seg,
                "right words here",
                Some("wrong words here"),
                t,
            )
            .unwrap();
        }
    }

    #[test]
    fn under_the_minimum_sample_a_cell_ships_nothing_and_says_what_it_needs() {
        let s = seed("app");
        s.store.create_speaker("Aspen", 0).unwrap();
        many(&s, 10, true);
        let out = learn(&s.store).unwrap();
        assert_eq!(out.corrections, 10);
        assert!(out.rules.is_empty(), "{:?}", out.rules);
        assert_eq!(out.short_by.len(), 1);
        assert_eq!(out.short_by[0].1, MIN_ROWS_PER_CELL - 10);
        assert!(out.cells[0].verdict.contains("of 30 corrections"));
    }

    #[test]
    fn a_night_decoder_that_is_right_earns_the_vote_and_one_that_is_wrong_loses_it() {
        let s = seed("app");
        s.store.create_speaker("Aspen", 0).unwrap();
        many(&s, 40, true);
        let out = learn(&s.store).unwrap();
        let rule = out.rules.global.expect("a global rule");
        assert_eq!(rule.winner, Decoder::Night);
        assert_eq!(rule.vote, VoteRule::NightWins);
        assert!(rule.wer_winner < rule.wer_live);

        let s = seed("app");
        s.store.create_speaker("Aspen", 0).unwrap();
        many(&s, 40, false);
        let out = learn(&s.store).unwrap();
        let rule = out.rules.global.expect("a global rule");
        assert_eq!(rule.winner, Decoder::Live);
        assert_eq!(rule.vote, VoteRule::KeepLive);
    }

    #[test]
    fn a_decoder_that_only_ties_ships_no_rule() {
        let s = seed("app");
        s.store.create_speaker("Aspen", 0).unwrap();
        for i in 0..40 {
            let t = 1_000 + i as i64 * 1_000_000_000;
            let seg = a_turn(&s, t, 1_000_000_000, Some(1), "right words here");
            s.store
                .set_segment_night(seg, Some("right words here"), t)
                .unwrap();
            record(
                &s.store,
                seg,
                "right words here",
                Some("right words here"),
                t,
            )
            .unwrap();
        }
        let out = learn(&s.store).unwrap();
        assert!(out.rules.is_empty(), "{:?}", out.rules);
        assert!(out.global.verdict.contains("shipped vote stands"));
    }

    #[test]
    fn the_hold_out_is_chronological_and_the_fit_never_scores_itself() {
        // Live is wrong on the first half and right on the second. A pass that
        // scored on everything would see a middling live error; held out
        // chronologically it sees the good half only, and does not hand the
        // row to the night decoder on the strength of history it was fitted on.
        let s = seed("app");
        s.store.create_speaker("Aspen", 0).unwrap();
        for i in 0..40i64 {
            let t = 1_000 + i * 1_000_000_000;
            let live = if i < 24 {
                "wrong words here"
            } else {
                "right words here"
            };
            let seg = a_turn(&s, t, 1_000_000_000, Some(1), live);
            s.store
                .set_segment_night(seg, Some("right words here"), t)
                .unwrap();
            record(&s.store, seg, "right words here", Some(live), t).unwrap();
        }
        let out = learn(&s.store).unwrap();
        assert_eq!(out.global.held_out, 16);
        assert!(
            out.rules.global.is_none(),
            "the held-out half has live already right: {:?}",
            out.global.verdict
        );
    }

    #[test]
    fn the_two_minimums_agree() {
        // `MIN_HELD_OUT` is what `MIN_ROWS_PER_CELL` buys after the split, and
        // it is written as a literal so it can be read; this is the arithmetic
        // it stands for.
        let times: Vec<i64> = (0..MIN_ROWS_PER_CELL as i64).collect();
        let cut = crate::calib::split_at(&times, crate::calib::FIT_FRACTION);
        assert_eq!(MIN_ROWS_PER_CELL - cut, MIN_HELD_OUT);
    }

    #[test]
    fn a_challenger_that_read_a_handful_of_held_out_rows_ships_nothing() {
        // The archive's own failure, in a test: 40 corrections in the cell, a
        // context re-decode on four of them, and it "wins" by a mile. Four
        // measurements are not a rule.
        let s = seed("app");
        s.store.create_speaker("Aspen", 0).unwrap();
        for i in 0..40i64 {
            let t = 1_000 + i * 1_000_000_000;
            let seg = a_turn(&s, t, 1_000_000_000, Some(1), "wrong words here");
            let ctx = (i >= 37).then_some("right words here");
            s.store
                .insert_text_truth(&crate::store::TextTruth {
                    segment_id: seg,
                    truth_text: "right words here".into(),
                    live_text: Some("wrong words here".into()),
                    context_text: ctx.map(str::to_string),
                    source_kind: "app".into(),
                    speaker_id: Some(1),
                    duration_ns: 1_000_000_000,
                    created_ns: t,
                    ..Default::default()
                })
                .unwrap();
        }
        let out = learn(&s.store).unwrap();
        assert!(
            out.rules.is_empty(),
            "{:?} / {}",
            out.rules,
            out.global.verdict
        );
        assert!(
            out.global.verdict.contains("held-out rows"),
            "{}",
            out.global.verdict
        );
    }

    #[test]
    fn a_decoder_is_only_scored_on_the_rows_it_read() {
        let s = seed("app");
        s.store.create_speaker("Aspen", 0).unwrap();
        // Night read four easy rows perfectly and nothing else; live read all
        // forty and got a third of them wrong. Night must not win on average.
        for i in 0..40i64 {
            let t = 1_000 + i * 1_000_000_000;
            let live = if i % 3 == 0 {
                "wrong words here"
            } else {
                "right words here"
            };
            let seg = a_turn(&s, t, 1_000_000_000, Some(1), live);
            if i >= 36 {
                s.store
                    .set_segment_night(seg, Some("right words here"), t)
                    .unwrap();
            }
            record(&s.store, seg, "right words here", Some(live), t).unwrap();
        }
        let out = learn(&s.store).unwrap();
        let night = out.global.scores[2];
        assert_eq!(night.rows, 4, "scored on four rows, not forty");
    }

    #[test]
    fn the_summary_block_says_how_many_more_corrections_are_needed() {
        let s = seed("app");
        s.store.create_speaker("Aspen", 0).unwrap();
        many(&s, 7, true);
        let j = summary_json(&s.store).unwrap();
        assert_eq!(j["corrections"], json!(7));
        assert_eq!(j["ready"], json!(false));
        assert_eq!(j["needed"], json!(MIN_ROWS_PER_CELL - 7));
        assert_eq!(j["rules"], json!(0));
        assert_eq!(j["learned_ns"], Value::Null);
    }
}
