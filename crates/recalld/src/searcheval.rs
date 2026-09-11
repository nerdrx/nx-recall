//! `recalld search eval` — a measured answer to "is search any good", instead
//! of an opinion about it.
//!
//! ## Why this exists
//!
//! `search`, `search.semantic` and `search.ask` shipped on unit tests and a
//! 48-segment fixture (see `semantic.rs`). None of that says what happens on
//! *this* archive, with its own mix of short turns, filler, two languages and
//! thousands of near-duplicate "yeah"s. This module builds an evaluation set
//! from the archive itself — a sample of real turns, each paired with a
//! natural-language query a person would type to find it again — and scores
//! today's three retrieval paths against it: FTS-only (`store.search`),
//! semantic-only (`SemanticLeg::search`) and hybrid (RRF fusion of the two,
//! `semantic::fuse`).
//!
//! ## The two halves
//!
//! The set is split by time, oldest half first: the **fit** half is for
//! trying things during tuning, the **held-out** half is for reporting
//! whether a change actually helped. Scoring or tuning against the same rows
//! a decision was made on is how a benchmark quietly turns into a mirror.
//!
//! ## What "correct" means
//!
//! Two targets per query, both recorded: the **exact** turn the query was
//! generated from, and a softer **thread** target — any hit in the same
//! conversation. A person asking "what did she say about the portal" is
//! usually satisfied by any turn from that conversation, not only the one
//! sentence an LLM happened to paraphrase; scoring only the exact row would
//! understate quality on questions whose answer is spread across a thread.
//!
//! ## Determinism
//!
//! Sampling is systematic (evenly strided through time), not randomised —
//! the project's rule throughout `semantic.rs` is that the same input
//! produces the same output, and an eval set that reshuffled itself on every
//! `--regen` would make two runs incomparable.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::llm::Llm;
use crate::semantic::{self, Candidates, SemanticLeg};
use crate::store::Store;

/// Shortest turn worth asking about. Below this a "natural query" is really
/// just the turn's own words with the serial numbers filed off.
pub const MIN_WORDS: usize = 8;

/// How many hits each mode is scored over. 10 covers every `recall@k` this
/// module reports; nothing is ranked deeper than that.
pub const SCORE_DEPTH: usize = 10;

/// One archive turn and the question built for it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalItem {
    pub query: String,
    /// `"de"` or `"en"` — the turn's own language, so a client can break the
    /// report down by it.
    pub lang: String,
    pub target_segment_id: i64,
    pub target_thread_id: Option<i64>,
    pub turn_words: usize,
    pub t_start_ns: i64,
    /// `true` on the earlier (by time) half of the set — the one tuning is
    /// allowed to look at.
    pub fit: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EvalSet {
    pub generated_at_ms: i64,
    /// The model that phrased the queries, recorded because a re-generation
    /// with a different model is not the same benchmark.
    pub query_model_id: String,
    pub items: Vec<EvalItem>,
}

fn eval_dir(data_dir: &Path, explicit: Option<&Path>) -> PathBuf {
    explicit
        .map(Path::to_path_buf)
        .unwrap_or_else(|| data_dir.join("searcheval"))
}

fn eval_path(dir: &Path) -> PathBuf {
    dir.join("eval_set.json")
}

// ---------------------------------------------------------------------------
// building the set
// ---------------------------------------------------------------------------

struct Candidate {
    segment_id: i64,
    text: String,
    lang: String,
    thread_id: Option<i64>,
    t_start_ns: i64,
    words: usize,
}

/// Every live, transcribed, German-or-English turn at least [`MIN_WORDS`]
/// long, oldest first. German and English only: those are the languages the
/// query-writing prompt below is written for, and a stray Polish turn would
/// silently get an English question nobody would type.
fn candidates(store: &Store) -> Result<Vec<Candidate>> {
    let conn = store.conn();
    let mut stmt = conn.prepare(
        "SELECT id, text, lang, thread_id, t_start_ns FROM segments
         WHERE deleted_at IS NULL AND text IS NOT NULL AND text <> ''
           AND lang IN ('de', 'en')
         ORDER BY t_start_ns ASC",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .filter_map(|(segment_id, text, lang, thread_id, t_start_ns)| {
            let words = text.split_whitespace().count();
            (words >= MIN_WORDS).then_some(Candidate {
                segment_id,
                text,
                lang,
                thread_id,
                t_start_ns,
                words,
            })
        })
        .collect();
    Ok(rows)
}

/// Evenly strided through `pool`, oldest to newest — see the module note on
/// determinism. When `pool` has fewer rows than `count`, every row is taken.
fn stride_sample(pool: &[Candidate], count: usize) -> Vec<&Candidate> {
    if pool.is_empty() || count == 0 {
        return Vec::new();
    }
    if pool.len() <= count {
        return pool.iter().collect();
    }
    // n evenly spaced picks over [0, len), n-1 as the last denominator so the
    // final pick lands on the last row rather than short of it.
    (0..count)
        .map(|i| {
            let pos = i * (pool.len() - 1) / (count - 1).max(1);
            &pool[pos]
        })
        .collect()
}

const QUERY_GBNF: &str = concat!(
    "root ::= \"{\" ws \"\\\"query\\\":\" ws str ws \"}\"\n",
    "str ::= \"\\\"\" chars \"\\\"\"\n",
    "chars ::= char chars | char\n",
    "char ::= [^\"\\\\\\x00-\\x1f] | \"\\\\\" ([\"\\\\/bfnrt] | \"u\" [0-9a-fA-F] [0-9a-fA-F] [0-9a-fA-F] [0-9a-fA-F])\n",
    "ws ::= [ \\t\\n]?\n"
);

const QUERY_SYSTEM: &str = concat!(
    "You write ONE short search-box query a person would type months later to ",
    "find the turn below again in their own voice transcript archive. Write it ",
    "in the SAME language as the turn. Paraphrase — use different words than the ",
    "turn where you can, the way someone recalling the gist would, not someone ",
    "quoting it. Three to eight words, no quotation marks, no question mark ",
    "unless a person would naturally ask it as a question. Output ONLY JSON.\n",
    "Examples:\n",
    "turn: \"ich glaub das Portal im Keller macht nur nachts auf\" ",
    "-> {\"query\": \"portal im keller nachts\"}\n",
    "turn: \"we should really fix the bitrate before Pico goes into standby\" ",
    "-> {\"query\": \"bitrate fix before standby\"}\n"
);

const QUERY_TOKENS: i32 = 40;

fn generate_query(llm: &Llm, text: &str) -> Result<Option<String>> {
    let out = llm.ask(QUERY_SYSTEM, text, QUERY_GBNF, QUERY_TOKENS)?;
    let Some(value) = crate::llm::first_json(&out) else {
        return Ok(None);
    };
    Ok(value
        .get("query")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string))
}

/// Build (or rebuild) the evaluation set: sample turns, ask the local LLM to
/// phrase a query for each, split fit/held-out by time, write it to `dir`.
///
/// Runs at idle priority on the caller's own affinity — the caller (the CLI
/// entry point) is expected to already be `chrt -i 0 taskset ... nice -n 19`
/// for a job this heavy, and `Llm` additionally nices/pins the child per
/// `[runtime]`.
pub fn build(store: &Store, llm: &Llm, count: usize, dir: &Path) -> Result<EvalSet> {
    crate::pipeline::deprioritise_current_thread(19, &[]);
    let pool = candidates(store)?;
    if pool.is_empty() {
        bail!("no eligible turns (>= {MIN_WORDS} words, de/en) to build an evaluation set from");
    }
    let picked = stride_sample(&pool, count);
    let median_t = picked[picked.len() / 2].t_start_ns;

    let mut items = Vec::with_capacity(picked.len());
    for (i, c) in picked.iter().enumerate() {
        let query = match generate_query(llm, &c.text) {
            Ok(Some(q)) => q,
            Ok(None) => {
                tracing::warn!(
                    segment_id = c.segment_id,
                    "the model produced no query for this turn; skipping it"
                );
                continue;
            }
            Err(e) => {
                tracing::warn!(segment_id = c.segment_id, error = %e, "query generation failed for this turn; skipping it");
                continue;
            }
        };
        eprint!("\r  {}/{} queries generated\x1b[K", i + 1, picked.len());
        items.push(EvalItem {
            query,
            lang: c.lang.clone(),
            target_segment_id: c.segment_id,
            target_thread_id: c.thread_id,
            turn_words: c.words,
            t_start_ns: c.t_start_ns,
            fit: c.t_start_ns < median_t,
        });
    }
    eprintln!();
    if items.is_empty() {
        bail!("the model produced no usable query for any sampled turn");
    }

    let generated_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let set = EvalSet {
        generated_at_ms,
        query_model_id: llm.model_id().to_string(),
        items,
    };
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::write(eval_path(dir), serde_json::to_vec_pretty(&set)?)
        .with_context(|| format!("writing {}", eval_path(dir).display()))?;
    Ok(set)
}

pub fn load(dir: &Path) -> Result<EvalSet> {
    let path = eval_path(dir);
    let bytes = std::fs::read(&path).with_context(|| {
        format!(
            "no evaluation set at {} — run `recalld search eval --regen` first",
            path.display()
        )
    })?;
    Ok(serde_json::from_slice(&bytes)?)
}

// ---------------------------------------------------------------------------
// scoring
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mode {
    /// `search` — FTS5 only.
    Keyword,
    /// `search.semantic` with `mode: "semantic"` — vector only.
    Smart,
    /// `search.semantic` with `mode: "hybrid"` — RRF fusion of both.
    Both,
}

impl Mode {
    pub const ALL: [Mode; 3] = [Mode::Keyword, Mode::Smart, Mode::Both];
    pub fn label(self) -> &'static str {
        match self {
            Mode::Keyword => "keyword",
            Mode::Smart => "smart",
            Mode::Both => "both",
        }
    }
}

/// One query's outcome under one mode: 1-based rank of the exact target and
/// of the best same-thread hit, each within the top [`SCORE_DEPTH`], or
/// `None` when it did not appear at all.
#[derive(Debug, Clone, Copy, Default)]
struct Outcome {
    exact_rank: Option<usize>,
    thread_rank: Option<usize>,
}

/// One ranked id list, per mode, for a single query — the shared work behind
/// every metric below.
/// The keyword leg, best-effort: `MATCH` is a query language and a generated
/// query landing on FTS5 syntax (a bare colon, a leading hyphen, an unbalanced
/// quote) is not this benchmark's failure to report, any more than it is
/// `search.semantic`'s (`service.rs::search_semantic` treats the same error
/// the same way, with the same reasoning, in production). It scores as a
/// miss for that query — which is the true production outcome for a client
/// that shows it as "no matches" — rather than aborting the run.
fn keyword_ids(store: &Store, query: &str) -> Vec<i64> {
    match store.search(query, SCORE_DEPTH) {
        Ok(hits) => hits.iter().map(|h| h.segment_id()).collect(),
        Err(e) => {
            tracing::warn!(query, error = %e, "the keyword leg rejected this generated query; scoring it as a miss");
            Vec::new()
        }
    }
}

fn ranked_ids(
    store: &Store,
    leg: &SemanticLeg,
    within: &Candidates,
    query: &str,
    mode: Mode,
) -> Result<Vec<i64>> {
    match mode {
        Mode::Keyword => Ok(keyword_ids(store, query)),
        Mode::Smart => Ok(leg
            .search(store, query, SCORE_DEPTH, within)?
            .into_iter()
            .map(|s| s.segment_id)
            .collect()),
        Mode::Both => {
            let keyword = keyword_ids(store, query);
            let vector: Vec<i64> = leg
                .search(store, query, SCORE_DEPTH, within)?
                .into_iter()
                .map(|s| s.segment_id)
                .collect();
            Ok(semantic::fuse(&keyword, &vector, semantic::RRF_K)
                .into_iter()
                .take(SCORE_DEPTH)
                .map(|f| f.segment_id)
                .collect())
        }
    }
}

fn outcome(ids: &[i64], item: &EvalItem, thread_of: &dyn Fn(i64) -> Option<i64>) -> Outcome {
    let mut out = Outcome::default();
    for (rank, &id) in ids.iter().enumerate() {
        let rank = rank + 1;
        if out.exact_rank.is_none() && id == item.target_segment_id {
            out.exact_rank = Some(rank);
        }
        if out.thread_rank.is_none()
            && item.target_thread_id.is_some()
            && thread_of(id) == item.target_thread_id
        {
            out.thread_rank = Some(rank);
        }
    }
    // The exact hit is always also a same-thread hit (it IS the thread), so
    // the softer target can never be worse than the exact one.
    if let (Some(e), Some(t)) = (out.exact_rank, out.thread_rank) {
        out.thread_rank = Some(e.min(t));
    } else if out.thread_rank.is_none() {
        out.thread_rank = out.exact_rank;
    }
    out
}

/// Aggregated metrics over one slice of items, for one mode.
#[derive(Debug, Clone, Default)]
pub struct Metrics {
    pub n: usize,
    pub recall_exact: [f64; 3], // @1, @5, @10
    pub mrr_exact: f64,
    pub recall_thread: [f64; 3],
    pub mrr_thread: f64,
}

fn aggregate(outcomes: &[Outcome]) -> Metrics {
    let n = outcomes.len();
    if n == 0 {
        return Metrics::default();
    }
    let ks = [1usize, 5, 10];
    let mut m = Metrics {
        n,
        ..Default::default()
    };
    for (i, &k) in ks.iter().enumerate() {
        m.recall_exact[i] = outcomes
            .iter()
            .filter(|o| o.exact_rank.is_some_and(|r| r <= k))
            .count() as f64
            / n as f64;
        m.recall_thread[i] = outcomes
            .iter()
            .filter(|o| o.thread_rank.is_some_and(|r| r <= k))
            .count() as f64
            / n as f64;
    }
    m.mrr_exact = outcomes
        .iter()
        .map(|o| o.exact_rank.map(|r| 1.0 / r as f64).unwrap_or(0.0))
        .sum::<f64>()
        / n as f64;
    m.mrr_thread = outcomes
        .iter()
        .map(|o| o.thread_rank.map(|r| 1.0 / r as f64).unwrap_or(0.0))
        .sum::<f64>()
        / n as f64;
    m
}

/// Wall-clock percentiles over one mode's queries, in milliseconds.
#[derive(Debug, Clone, Copy, Default)]
pub struct Latency {
    pub p50_ms: f64,
    pub p95_ms: f64,
}

fn percentiles(mut ms: Vec<f64>) -> Latency {
    if ms.is_empty() {
        return Latency::default();
    }
    ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let at = |p: f64| -> f64 {
        let idx = ((ms.len() - 1) as f64 * p).round() as usize;
        ms[idx.min(ms.len() - 1)]
    };
    Latency {
        p50_ms: at(0.50),
        p95_ms: at(0.95),
    }
}

/// One full report: every mode, over the fit half, the held-out half, and
/// (for convenience) all of it — plus latency, which is only meaningful
/// measured over the held-out half's query mix.
pub struct Report {
    pub fit: std::collections::HashMap<Mode, Metrics>,
    pub heldout: std::collections::HashMap<Mode, Metrics>,
    pub heldout_by_lang: std::collections::HashMap<(Mode, String), Metrics>,
    /// Short (< 12 words) vs long, held-out only.
    pub heldout_by_length: std::collections::HashMap<(Mode, bool), Metrics>,
    pub latency: std::collections::HashMap<Mode, Latency>,
}

/// Score every mode over an evaluation set. `leg` must already have a warm
/// index — the caller loads/refreshes it once, not once per query.
pub fn score(store: &Store, leg: &SemanticLeg, set: &EvalSet) -> Result<Report> {
    let within = Candidates::everything();
    let thread_of = |id: i64| -> Option<i64> {
        store
            .conn()
            .query_row("SELECT thread_id FROM segments WHERE id = ?1", [id], |r| {
                r.get::<_, Option<i64>>(0)
            })
            .ok()
            .flatten()
    };

    let mut fit_out: std::collections::HashMap<Mode, Vec<Outcome>> = Default::default();
    let mut held_out: std::collections::HashMap<Mode, Vec<Outcome>> = Default::default();
    let mut held_lang: std::collections::HashMap<(Mode, String), Vec<Outcome>> = Default::default();
    let mut held_len: std::collections::HashMap<(Mode, bool), Vec<Outcome>> = Default::default();
    let mut latency_ms: std::collections::HashMap<Mode, Vec<f64>> = Default::default();

    for item in &set.items {
        for mode in Mode::ALL {
            let started = Instant::now();
            let ids = ranked_ids(store, leg, &within, &item.query, mode)?;
            let elapsed = started.elapsed().as_secs_f64() * 1000.0;
            let out = outcome(&ids, item, &thread_of);

            if item.fit {
                fit_out.entry(mode).or_default().push(out);
            } else {
                held_out.entry(mode).or_default().push(out);
                held_lang
                    .entry((mode, item.lang.clone()))
                    .or_default()
                    .push(out);
                held_len
                    .entry((mode, item.turn_words < 12))
                    .or_default()
                    .push(out);
                latency_ms.entry(mode).or_default().push(elapsed);
            }
        }
    }

    Ok(Report {
        fit: fit_out
            .into_iter()
            .map(|(m, v)| (m, aggregate(&v)))
            .collect(),
        heldout: held_out
            .into_iter()
            .map(|(m, v)| (m, aggregate(&v)))
            .collect(),
        heldout_by_lang: held_lang
            .into_iter()
            .map(|(k, v)| (k, aggregate(&v)))
            .collect(),
        heldout_by_length: held_len
            .into_iter()
            .map(|(k, v)| (k, aggregate(&v)))
            .collect(),
        latency: latency_ms
            .into_iter()
            .map(|(m, v)| (m, percentiles(v)))
            .collect(),
    })
}

// ---------------------------------------------------------------------------
// tuning arms (fit half only)
// ---------------------------------------------------------------------------

/// Whitening on (the shipped default, once the corpus clears
/// `MIN_WHITENING_ROWS`) vs off (`SemanticLeg::search_raw`) — `Whitening`'s
/// own doc comment measured this on a 48-segment synthetic fixture; this is
/// the same question asked of the archive `search eval` actually has on
/// hand. `fit` selects which half is scored: try on the fit half while
/// tuning, confirm on the held-out half before deciding anything.
pub fn whitening_arm(
    store: &Store,
    leg: &SemanticLeg,
    set: &EvalSet,
    fit: bool,
) -> Result<(Metrics, Metrics)> {
    let within = Candidates::everything();
    let thread_of = |id: i64| -> Option<i64> {
        store
            .conn()
            .query_row("SELECT thread_id FROM segments WHERE id = ?1", [id], |r| {
                r.get::<_, Option<i64>>(0)
            })
            .ok()
            .flatten()
    };
    let mut whitened = Vec::new();
    let mut raw = Vec::new();
    for item in set.items.iter().filter(|i| i.fit == fit) {
        let w_ids: Vec<i64> = leg
            .search(store, &item.query, SCORE_DEPTH, &within)?
            .into_iter()
            .map(|s| s.segment_id)
            .collect();
        whitened.push(outcome(&w_ids, item, &thread_of));
        let r_ids: Vec<i64> = leg
            .search_raw(store, &item.query, SCORE_DEPTH, &within)?
            .into_iter()
            .map(|s| s.segment_id)
            .collect();
        raw.push(outcome(&r_ids, item, &thread_of));
    }
    Ok((aggregate(&whitened), aggregate(&raw)))
}

// ---------------------------------------------------------------------------
// the CLI command
// ---------------------------------------------------------------------------

fn print_metrics_row(label: &str, m: &Metrics) {
    println!(
        "  {:<10} n={:<4} R@1 {:.2}  R@5 {:.2}  R@10 {:.2}  MRR {:.3}   (thread-soft: R@1 {:.2}  R@5 {:.2}  R@10 {:.2}  MRR {:.3})",
        label,
        m.n,
        m.recall_exact[0],
        m.recall_exact[1],
        m.recall_exact[2],
        m.mrr_exact,
        m.recall_thread[0],
        m.recall_thread[1],
        m.recall_thread[2],
        m.mrr_thread,
    );
}

pub fn command(
    cfg: &crate::config::Config,
    data_dir: &Path,
    regen: bool,
    dir: Option<&Path>,
    count: usize,
) -> Result<()> {
    let dir = eval_dir(data_dir, dir);
    let store = Store::open(data_dir)?;

    let root = crate::fetch::target_dir(None, &cfg.models, data_dir);
    let sem = crate::models::SemanticModel::resolve_at(root.clone(), &cfg.models);
    if !sem.present() {
        bail!("{}", crate::models::SemanticModel::how_to_get_it());
    }
    let leg = SemanticLeg::new(crate::semantic::TextEmbedder::load(&sem)?);
    // Warm the index once, up front, rather than once per query inside score().
    {
        leg.warm_index(&store)?;
    }

    let set = if regen {
        let graph = crate::models::GraphModels::resolve(&root, &cfg.graph);
        if !graph.present() {
            bail!("generating queries needs the local LLM — `recalld models fetch --graph`");
        }
        let llm = Llm::resolve(&root, &cfg.graph, &cfg.runtime)
            .context("resolving the local LLM for query generation")?;
        println!(
            "building an evaluation set of {count} turns using {}...",
            llm.model_id()
        );
        build(&store, &llm, count, &dir)?
    } else {
        load(&dir)?
    };

    let fit_n = set.items.iter().filter(|i| i.fit).count();
    println!(
        "evaluation set: {} items ({} fit / {} held-out), queried by {}",
        set.items.len(),
        fit_n,
        set.items.len() - fit_n,
        set.query_model_id
    );

    let report = score(&store, &leg, &set)?;

    println!("\nfit half (tuning only — do not report these numbers):");
    for mode in Mode::ALL {
        if let Some(m) = report.fit.get(&mode) {
            print_metrics_row(mode.label(), m);
        }
    }

    println!("\ntuning arm — whitening on vs off (fit half, smart mode):");
    let (whitened_fit, raw_fit) = whitening_arm(&store, &leg, &set, true)?;
    print_metrics_row("whitened", &whitened_fit);
    print_metrics_row("raw", &raw_fit);
    if raw_fit.recall_exact[1] > whitened_fit.recall_exact[1] {
        println!(
            "  raw led on the fit half (R@5 {:.2} vs {:.2}) — confirming on held-out:",
            raw_fit.recall_exact[1], whitened_fit.recall_exact[1]
        );
        let (whitened_ho, raw_ho) = whitening_arm(&store, &leg, &set, false)?;
        print_metrics_row("whitened/ho", &whitened_ho);
        print_metrics_row("raw/ho", &raw_ho);
    }

    println!("\nheld-out half (what to report):");
    for mode in Mode::ALL {
        if let Some(m) = report.heldout.get(&mode) {
            print_metrics_row(mode.label(), m);
        }
        if let Some(l) = report.latency.get(&mode) {
            println!(
                "    latency: p50 {:.1} ms  p95 {:.1} ms",
                l.p50_ms, l.p95_ms
            );
        }
    }

    println!("\nheld-out, by query language:");
    for mode in Mode::ALL {
        for lang in ["de", "en"] {
            if let Some(m) = report.heldout_by_lang.get(&(mode, lang.to_string())) {
                print_metrics_row(&format!("{}/{}", mode.label(), lang), m);
            }
        }
    }

    println!("\nheld-out, by turn length (short: < 12 words):");
    for mode in Mode::ALL {
        for (short, tag) in [(true, "short"), (false, "long")] {
            if let Some(m) = report.heldout_by_length.get(&(mode, short)) {
                print_metrics_row(&format!("{}/{}", mode.label(), tag), m);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(fit: bool) -> EvalItem {
        EvalItem {
            query: "q".into(),
            lang: "en".into(),
            target_segment_id: 1,
            target_thread_id: Some(9),
            turn_words: 10,
            t_start_ns: 0,
            fit,
        }
    }

    #[test]
    fn stride_sample_covers_the_full_range_and_never_exceeds_count() {
        let pool: Vec<Candidate> = (0..1000)
            .map(|i| Candidate {
                segment_id: i,
                text: String::new(),
                lang: "en".into(),
                thread_id: None,
                t_start_ns: i,
                words: 10,
            })
            .collect();
        let picked = stride_sample(&pool, 300);
        assert_eq!(picked.len(), 300);
        assert_eq!(picked.first().unwrap().t_start_ns, 0);
        assert_eq!(picked.last().unwrap().t_start_ns, 999);
        // Strictly increasing: no duplicate picked twice, no reordering.
        assert!(picked.windows(2).all(|w| w[0].t_start_ns < w[1].t_start_ns));
    }

    #[test]
    fn stride_sample_of_a_small_pool_takes_everything() {
        let pool: Vec<Candidate> = (0..5)
            .map(|i| Candidate {
                segment_id: i,
                text: String::new(),
                lang: "en".into(),
                thread_id: None,
                t_start_ns: i,
                words: 10,
            })
            .collect();
        assert_eq!(stride_sample(&pool, 300).len(), 5);
    }

    #[test]
    fn the_exact_hit_is_never_a_worse_thread_rank_than_itself() {
        let ids = vec![7, 1, 3]; // target (1) at rank 2
        let out = outcome(&ids, &item(true), &|id| {
            if id == 7 { Some(9) } else { None }
        });
        assert_eq!(out.exact_rank, Some(2));
        // rank 1 (id 7) is in the same thread and comes first — the soft
        // target must take the better of the two, not just the exact one.
        assert_eq!(out.thread_rank, Some(1));
    }

    #[test]
    fn a_miss_on_both_targets_is_none_not_zero() {
        let out = outcome(&[42, 43], &item(true), &|_| None);
        assert_eq!(out.exact_rank, None);
        assert_eq!(out.thread_rank, None);
    }

    #[test]
    fn aggregate_recall_and_mrr_on_a_tiny_known_case() {
        // ranks: 1, 3, miss -> R@1 = 1/3, R@5 = 2/3, MRR = (1 + 1/3 + 0)/3
        let outs = vec![
            Outcome {
                exact_rank: Some(1),
                thread_rank: Some(1),
            },
            Outcome {
                exact_rank: Some(3),
                thread_rank: Some(3),
            },
            Outcome {
                exact_rank: None,
                thread_rank: None,
            },
        ];
        let m = aggregate(&outs);
        assert!((m.recall_exact[0] - 1.0 / 3.0).abs() < 1e-9);
        assert!((m.recall_exact[1] - 2.0 / 3.0).abs() < 1e-9);
        assert!((m.mrr_exact - (1.0 + 1.0 / 3.0) / 3.0).abs() < 1e-9);
    }

    #[test]
    fn percentiles_of_a_single_value_are_that_value() {
        let l = percentiles(vec![12.5]);
        assert_eq!(l.p50_ms, 12.5);
        assert_eq!(l.p95_ms, 12.5);
    }
}
