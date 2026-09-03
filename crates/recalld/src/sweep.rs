//! The archive sweep for language (0.11.9): asking the identifier about the
//! turns it was never asked about.
//!
//! ## Why there is an archive to sweep at all
//!
//! Every language decision this daemon makes is made **once**, on the way in,
//! by whatever code was shipped that evening. Two things changed under rows
//! that had already been captured:
//!
//! * 0.11.0 introduced the spoken-language identifier and 0.11.6/0.11.8 the
//!   ja/ko/zh and fr routes. Nothing that came before them was ever listened
//!   to.
//! * 0.11.8 lowered `[asr].lid_min_s` from 1.5 s to 1.0 s (FINDINGS §28). The
//!   band between those two numbers is not a rounding detail: on this install's
//!   own archive it is **2,285 untagged rows**, more than the 1,866 above the
//!   old floor.
//!
//! So the archive holds turns whose language nobody ever asked a model about,
//! and the audio for most of them is still on disk. That is the whole feature:
//! walk them, ask once, and hand the answer to **the same code the live path
//! uses**.
//!
//! ## The rule that makes this safe to write at all
//!
//! A backfill is only allowed to work one way — its rows must come out
//! indistinguishable from rows the pipeline got right the first time. That is
//! the rule [`crate::langctx::repair`] already follows and it is followed here
//! literally rather than in spirit: this module contains **no routing
//! decision of its own**. It calls [`crate::asr_cjk::route_segment`] and
//! [`crate::polyglot::route_segment`], in that order, with the reading shared
//! between them, exactly as [`crate::analysis::Analyzer::after_commit`] does.
//! Provenance therefore matches by construction rather than by care:
//! `lang_via: "lid"`, `text_via: "lid"`, the `asr_model_id` of whichever
//! decoder ran, and an `operations` row `op: "segments.redecode"` carrying the
//! words that were there before.
//!
//! What is left for this module is the part the live path does not have: which
//! rows to visit, how to stop, and what to write on a row nothing routed.
//!
//! ## The sweep is stricter than the live path, in three places
//!
//! All three are measurements on this archive rather than preferences
//! (FINDINGS §30), and the third is the round's actual finding.
//!
//! §28 justified the 1.0 s live floor with a **conditioned** false-positive
//! rate: over 900 German and English FLEURS cuts, one survived every guard.
//! FLEURS is read speech. This archive is a lobby, and the same pipeline over
//! its 1,796 untagged rows heard 131 of them as Korean and 67 as Chinese — in
//! a corpus where, as far as anybody knows, nobody has ever spoken either. The
//! judge absorbs most of that and not enough of it: 44 rows survived, 36 of
//! them on voices declared German/English-only, which is **2.3%** against a 1%
//! gate.
//!
//! 1. **`[asr].lang_sweep_windows = 3`**, against the live path's 1 *as it was
//!    when this was written*. [`crate::lid`] documented `lid_windows` as the
//!    knob for "a machine that hears something this corpus did not", and this
//!    is that machine. Three overlapping windows that must agree take the
//!    routed rows from 342 to 28 and the de/en rate from 2.3% to **0.97%**,
//!    inside the gate, for three model passes instead of one.
//!
//!    0.11.10 found the same thing on the *live* rows the route had already
//!    rewritten (FINDINGS §31) and moved `lid_windows` to 3 as well, so this is
//!    no longer a difference. The field stays because the narrowing is
//!    one-directional ([`routing_cfg`]): an operator who lowers `lid_windows`
//!    for a machine short of cores lowers it for the live turn they are
//!    watching, and must not thereby lower it for four hundred archive rows.
//! 2. **`[asr].lang_sweep_min_s = 1.5`**, against the live path's 1.0. Below
//!    1.5 s nothing is ever *kept*: `[lang].arbiter_min_duration_s` is the
//!    replacement floor and both decoders refuse under it, so the 974 archive
//!    rows in that band cost a model pass each and produced no rewrites at all.
//!    On the live path that call is still worth making — the row's own
//!    translation reads `lang` — and in a batch of thousands it is not.
//! 3. **`[asr].lang_sweep_redecode = false`.** Passing the gate turned out not
//!    to be enough, and that is the thing worth writing down. The gate was
//!    written for a *live* route, where the rows that clear it are
//!    overwhelmingly real foreign turns and the false positives are a residue.
//!    On an archive of German and English there are almost no real foreign
//!    turns to be right about, so the same rate is nearly the whole output: of
//!    the nine rows the routes rewrote, a hand check says **eight are wrong**.
//!    `"Okay."` came back as `、お疲さ`; `"Oh, she has this detected beim
//!    sonar."` came back as a fluent French sentence nobody said. So the sweep
//!    ships doing the half that cannot lie — it writes `lang` and never a word
//!    — and the rewriting half is one field away for somebody who has read
//!    what `recalld lang sweep` says it would do.
//!
//! The live path is untouched by all of this. A live turn is one turn, decided
//! as it happens, against a real distribution of languages; a sweep is
//! thousands of old ones decided at 04:00 by a machine. The same evidence does
//! not buy the same confidence, and the honest way to say so is separate
//! numbers.
//!
//! ## Bounded, resumable, and it never revisits a row twice
//!
//! The work list is a query and not a cursor
//! ([`Store::segments_for_lang_sweep`]), so an interrupted run loses at most
//! the row it was on. What keeps the walk from spinning is that **every row the
//! identifier is actually spent on leaves the list**:
//!
//! * routed → `lang` is set by the route, `lang_via` is `lid` (only with
//!   `lang_sweep_redecode` on; off, nothing is ever routed);
//! * heard as `de` or `en` → `lang` is set to that, `lang_via` is
//!   [`crate::store::lang_via::SWEEP`] (see there for why it is not `lid`);
//! * heard as anything else, or no opinion → `lang` stays NULL and `lang_via`
//!   becomes `sweep` anyway. Nothing is claimed and the row is not asked about
//!   again.
//!
//! Rows [`crate::asr_cjk::pre_route`] declines — a readable transcript, or a
//! voice pinned to a language that is somebody else's business — are **not**
//! marked. They cost a string classify and no model, and a declaration can be
//! added tomorrow; marking them would freeze a decision that was free to
//! re-make. They are remembered for the length of one run so the walk moves
//! forward, which is [`crate::langctx::repair`]'s `passed` set for the same
//! reason.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tracing::{debug, info};

use crate::asr_cjk::{self, Cjk, Pre};
use crate::bus::Bus;
use crate::config::{AsrConfig, LangConfig, SAMPLE_RATE};
use crate::polyglot::{self, Polyglot};
use crate::store::{Store, SweepCandidate, lang_via};

/// The two languages the routes deliberately never act on, and therefore the
/// two a reading can name without anything being re-decoded.
///
/// Not [`crate::lang::KNOWN`] and not the classifier's `Lang`: this is the set
/// of tags for which "the identifier said so" is the *whole* answer, because
/// German and English already have their own arbiters reached from text
/// ([`crate::arbiter`]) and routing them by ear as well is how two features
/// start fighting over one column.
pub const STAMPABLE: &[&str] = &["de", "en"];

/// What the sweep did to one row. The variants partition every row it looks at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Swept {
    /// A decoder re-read the turn and its words won. The row is now shaped
    /// exactly like one the live route settled.
    Routed { lang: String },
    /// The identifier heard `de` or `en`. The language is written, the words
    /// are not touched — there is nothing here to re-decode.
    Stamped { lang: &'static str },
    /// The identifier ran and its answer was not one anything acts on, or it
    /// had no opinion. The row is taken off the work list and nothing is
    /// claimed about it.
    Marked,
    /// `pre_route` declined: the transcript already reads as something, or the
    /// voice is pinned to a language another feature owns. No model ran and the
    /// row is left exactly as it was — including its NULL `lang_via`.
    LeftAlone,
    /// The identifier is not installed, or would not load. Nothing was spent
    /// and nothing was written; fetch it and run again.
    Unavailable,
    /// The row names a WAV that is not there — retention took it, or it never
    /// landed. Marked, because there is nothing left to ask about.
    NoAudio,
    /// Preview only: what the run *would* have done, without running a decoder
    /// or writing anything.
    Would {
        heard: Option<String>,
        routes_to: Option<String>,
    },
}

/// What one sweep run did. Every count is a row and they partition `scanned`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Rows whose stored words or language actually moved — the ones a client
    /// showing them has to be told about.
    pub changed: Vec<i64>,
    /// Rows taken off the work list and looked at.
    pub scanned: usize,
    /// Rows a model was actually run on. The cost of the run, and the number
    /// `[asr].lang_sweep_rows_per_run` bounds.
    pub asked: usize,
    /// Rows re-decoded and rewritten, by the language they were routed to.
    pub routed: BTreeMap<String, usize>,
    /// Rows stamped `de` or `en` off the reading alone.
    pub stamped: BTreeMap<String, usize>,
    /// Asked, nothing to do, marked so it is not asked again.
    pub marked: usize,
    /// `pre_route` declined. Not marked, and free.
    pub left_alone: usize,
    /// The identifier is not installed.
    pub unavailable: usize,
    /// The WAV is gone from disk even though the row still names one.
    pub no_audio: usize,
    /// Preview only: everything the identifier said, and everything that would
    /// have routed. Empty on an `--apply` run, where the routes' own outcome is
    /// the honest record.
    pub heard: BTreeMap<String, usize>,
    pub would_route: BTreeMap<String, usize>,
}

impl SweepReport {
    fn count(map: &mut BTreeMap<String, usize>, key: &str) {
        *map.entry(key.to_string()).or_default() += 1;
    }

    /// Rows whose language moved, however it moved. What the operator reads as
    /// "this run did something".
    pub fn settled(&self) -> usize {
        self.routed.values().sum::<usize>() + self.stamped.values().sum::<usize>()
    }
}

/// The config the routes are handed: **the operator's, with two numbers
/// replaced** by the sweep's own (FINDINGS §30).
///
/// A whole [`AsrConfig`] rather than two floats threaded through three
/// functions, so that what the routes read is a real config and every other
/// knob on it — the polyglot allowlist, the two decoder switches, the
/// confidence operating point — is the operator's, unchanged. A sweep that
/// quietly widened `polyglot_languages` would be a sweep doing something the
/// live path is not allowed to do.
///
/// Both replacements are one-directional. `lang_sweep_min_s` may only *raise*
/// the identifier floor and `lang_sweep_windows` may only *add* windows,
/// because this pass is the one place where thousands of these decisions are
/// made at 04:00 by a machine rather than one at a time by a person watching.
/// An operator who writes a looser number gets the live one.
pub fn routing_cfg(cfg: &AsrConfig) -> AsrConfig {
    AsrConfig {
        lid_min_s: cfg.lang_sweep_min_s.max(cfg.lid_min_s),
        lid_windows: cfg.lang_sweep_windows.max(cfg.lid_windows),
        ..cfg.clone()
    }
}

/// Everything one pass needs that is the same for every row in it.
///
/// A struct rather than nine more parameters, for [`crate::asr_cjk::Turn`]'s
/// reason: two `&Config` in a row are two things somebody in a hurry will
/// eventually swap.
pub struct Pass<'a> {
    /// The daemon's store, or the CLI's own. Locked **per row** and never
    /// across the whole batch: a sweep that held it for a batch would be a
    /// sweep that dropped live audio.
    pub store: &'a Arc<Mutex<Store>>,
    /// `Some` in the daemon, `None` in the CLI, which has no clients to tell.
    pub bus: Option<&'a Bus>,
    pub data_dir: &'a Path,
    pub lang_cfg: &'a LangConfig,
    /// False previews: the identifier runs, nothing else does, nothing is
    /// written. True writes.
    pub apply: bool,
    /// `[asr].lang_sweep_redecode`, and **off** on this install by measurement
    /// (FINDINGS §30). With it off the sweep asks the identifier and writes
    /// nothing but a language; with it on the routes may replace a transcript.
    pub redecode: bool,
    /// Already narrowed by [`routing_cfg`]. Private and owned so that it cannot
    /// be the operator's config by accident — the identifier itself is built
    /// from `lid_windows` at construction time, so a caller holding the wrong
    /// one would build a router the pass then thinks is stricter than it is.
    asr: AsrConfig,
}

impl<'a> Pass<'a> {
    pub fn new(
        store: &'a Arc<Mutex<Store>>,
        bus: Option<&'a Bus>,
        data_dir: &'a Path,
        asr_cfg: &AsrConfig,
        lang_cfg: &'a LangConfig,
        apply: bool,
    ) -> Self {
        Self {
            store,
            bus,
            data_dir,
            lang_cfg,
            apply,
            redecode: asr_cfg.lang_sweep_redecode,
            asr: routing_cfg(asr_cfg),
        }
    }

    /// The narrowed config, which is what **the caller must build both routers
    /// from**. [`Cjk::new`] reads `lid_windows` once, at construction.
    pub fn asr_cfg(&self) -> &AsrConfig {
        &self.asr
    }
}

/// One row, from the work list to whatever the routes made of it.
///
/// Reads the clip **without the store lock** and takes it only for the write,
/// which is [`crate::night`]'s discipline restated: model time and store-lock
/// time never overlap. The one exception is honest and bounded — the routes do
/// their own writing, so the lock is held for the length of one decode: ~50 ms
/// for a CJK clip through sherpa, and one `whisper-cli` invocation (1–2 s) for
/// the French route, which is why the background pass runs in the night window
/// and the CLI says out loud that it is a batch job.
pub fn sweep_one(
    pass: &Pass<'_>,
    cjk: &mut Cjk,
    poly: &mut Polyglot,
    row: &SweepCandidate,
) -> Result<Swept> {
    // The cheap half first, and it is the same function the live path gates on
    // — reused rather than re-derived, because a second copy of this rule is a
    // second rule. A readable transcript or a voice pinned to somebody else's
    // language costs a string scan and no disk.
    if asr_cjk::pre_route(row.declared.as_ref(), row.text.as_deref(), pass.asr_cfg())
        == Pre::Nothing
    {
        return Ok(Swept::LeftAlone);
    }
    let samples = match crate::ingest::read_wav(&pass.data_dir.join(&row.audio_path)) {
        Ok(s) if !s.is_empty() => s,
        _ => {
            if pass.apply {
                let guard = pass.store.lock().unwrap_or_else(|p| p.into_inner());
                guard.mark_segment_swept(row.id)?;
            }
            return Ok(Swept::NoAudio);
        }
    };
    // The floor is checked here as well as inside the routes, so that a row
    // under it is never counted as asked. The routes would refuse it anyway;
    // this is the difference between "we spent nothing" and "we spent nothing
    // and said we did".
    let cfg = pass.asr_cfg();
    if (samples.len() as f32 / SAMPLE_RATE as f32) < cfg.lid_min_s {
        return Ok(Swept::LeftAlone);
    }

    // Two ways to get a reading, and which one is used is the whole of
    // `[asr].lang_sweep_redecode` (FINDINGS §30).
    //
    // The re-decoding arm is the live path, literally: `route_segment` twice,
    // in the live order, sharing one reading. It is off by default because
    // eight of the nine rows it rewrote on this archive were rewritten wrongly.
    // The other arm asks the identifier and stops, which cannot damage a
    // transcript because it never touches one.
    let heard = if pass.apply && pass.redecode {
        let now = crate::clock::utc_now_ns();
        let checked = {
            let guard = pass.store.lock().unwrap_or_else(|p| p.into_inner());
            asr_cjk::route_segment(
                cjk,
                &guard,
                asr_cjk::Turn {
                    segment_id: row.id,
                    declared: row.declared.as_ref(),
                    text: row.text.as_deref(),
                    samples: &samples,
                    lang_cfg: pass.lang_cfg,
                    asr_cfg: cfg,
                },
                now,
            )?
        };
        if let Some(routed) = checked.routed {
            return Ok(Swept::Routed {
                lang: routed.lang.to_string(),
            });
        }
        // The other half of the audio route, on a turn the first one left
        // alone, reading the identifier's answer rather than paying for a
        // second pass — the live order and the live reason.
        if let Some(reading) = checked.heard.as_ref() {
            let routed = {
                let guard = pass.store.lock().unwrap_or_else(|p| p.into_inner());
                polyglot::route_segment(
                    poly,
                    &guard,
                    polyglot::Turn {
                        segment_id: row.id,
                        text: row.text.as_deref(),
                        samples: &samples,
                        heard: Some(reading),
                        lang_cfg: pass.lang_cfg,
                        asr_cfg: cfg,
                    },
                    now,
                )?
            };
            if let Some(routed) = routed {
                return Ok(Swept::Routed { lang: routed.lang });
            }
        }
        // A route that never ran its identifier spent nothing on this row, and
        // a row nothing was spent on must stay on the list — otherwise
        // installing the models later would fix nothing.
        if !checked.lid_checked {
            return Ok(Swept::Unavailable);
        }
        checked.heard
    } else {
        // `Cjk::identify` needs only the 13 MB identifier — not either decoder,
        // and not their switches — which is why this arm works on an install
        // that has no decoder at all and the arm above does not.
        match cjk.identify(&samples) {
            Some(reading) => reading,
            None => return Ok(Swept::Unavailable),
        }
    };

    if !pass.apply {
        // A preview cannot run the re-decoding arm at all: `route_segment`
        // writes, and a preview that wrote would not be one. What it can do is
        // report the two decisions the routes would make from the reading,
        // which is exactly `post_route` twice — the same functions, in the live
        // order — so an operator can see what `--redecode` would do before
        // asking for it.
        let routes_to = asr_cjk::post_route(heard.as_ref(), cfg)
            .map(str::to_string)
            .or_else(|| polyglot::post_route(heard.as_ref(), cfg));
        return Ok(Swept::Would {
            heard: heard.map(|r| r.lang),
            routes_to,
        });
    }

    let guard = pass.store.lock().unwrap_or_else(|p| p.into_inner());
    match heard
        .as_ref()
        .and_then(|r| STAMPABLE.iter().copied().find(|t| *t == r.lang))
    {
        Some(tag) => {
            guard.set_segment_language(row.id, tag, lang_via::SWEEP)?;
            Ok(Swept::Stamped { lang: tag })
        }
        None => {
            guard.mark_segment_swept(row.id)?;
            Ok(Swept::Marked)
        }
    }
}

/// Walk the work list, oldest first, until it dries up or the budget runs out.
///
/// `limit` bounds **the rows a model is spent on**, not the rows looked at, and
/// that distinction is the difference between a nightly pass that works and one
/// that does not: this archive holds 2,300 untagged rows `pre_route` declines
/// for free, and a budget spent walking past those would take a fortnight to
/// reach the first row worth asking about. `batch` is how much is taken from
/// the database at a time; `progress` is called once per batch, and `stop` is
/// checked between rows so a daemon shutting down does not wait for a GPU.
///
/// Resumable by construction — see the module note. The only state a run keeps
/// is `passed`, the rows it decided to leave alone, which exists because those
/// rows are still in the work list and would otherwise be handed back for ever.
/// A run whose whole work list is `passed` ends when the query stops returning
/// anything new, which is the walk's real termination condition.
pub fn run_pass(
    pass: &Pass<'_>,
    cjk: &mut Cjk,
    poly: &mut Polyglot,
    batch: usize,
    limit: Option<usize>,
    stop: &dyn Fn() -> bool,
    mut progress: impl FnMut(&SweepReport),
) -> Result<SweepReport> {
    let mut report = SweepReport::default();
    let batch = batch.clamp(1, 512);
    let floor = pass.asr_cfg().lid_min_s;
    let mut passed: HashSet<i64> = HashSet::new();
    let mut published = 0usize;

    loop {
        if stop() {
            break;
        }
        if limit.is_some_and(|l| report.asked >= l) {
            break;
        }
        let take = batch;
        // Over-read by what has already been passed over: those rows are still
        // in the work list — deliberately, they were never marked — so without
        // this a batch that is entirely readable would be handed back unchanged
        // for ever.
        let rows: Vec<SweepCandidate> = {
            let guard = pass.store.lock().unwrap_or_else(|p| p.into_inner());
            guard.segments_for_lang_sweep(floor, passed.len() + take)?
        }
        .into_iter()
        .filter(|r| !passed.contains(&r.id))
        .take(take)
        .collect();
        if rows.is_empty() {
            break;
        }
        for row in rows {
            if stop() || limit.is_some_and(|l| report.asked >= l) {
                break;
            }
            report.scanned += 1;
            match sweep_one(pass, cjk, poly, &row)? {
                Swept::Routed { lang } => {
                    report.asked += 1;
                    SweepReport::count(&mut report.routed, &lang);
                    report.changed.push(row.id);
                }
                Swept::Stamped { lang } => {
                    report.asked += 1;
                    SweepReport::count(&mut report.stamped, lang);
                    report.changed.push(row.id);
                }
                Swept::Marked => {
                    report.asked += 1;
                    report.marked += 1;
                }
                Swept::Would { heard, routes_to } => {
                    report.asked += 1;
                    SweepReport::count(&mut report.heard, heard.as_deref().unwrap_or("(none)"));
                    if let Some(tag) = routes_to {
                        SweepReport::count(&mut report.would_route, &tag);
                    }
                    // A preview writes nothing, so every row it looks at is
                    // still in the work list when it looks again.
                    passed.insert(row.id);
                }
                Swept::LeftAlone => {
                    report.left_alone += 1;
                    passed.insert(row.id);
                }
                Swept::Unavailable => {
                    report.unavailable += 1;
                    passed.insert(row.id);
                }
                Swept::NoAudio => report.no_audio += 1,
            }
        }
        // Told after the batch, which is the night shift's order: the words are
        // committed before anybody is told they changed. `published` is the
        // high-water mark rather than a re-scan of `changed`, so a row is
        // announced exactly once however many batches the run takes.
        if let Some(bus) = pass.bus
            && published < report.changed.len()
        {
            let guard = pass.store.lock().unwrap_or_else(|p| p.into_inner());
            for id in &report.changed[published..] {
                crate::pipeline::publish_segment(bus, &guard, *id);
            }
        }
        published = report.changed.len();
        progress(&report);
    }
    Ok(report)
}

// ---------------------------------------------------------------------------
// the nightly pass
// ---------------------------------------------------------------------------

/// Why the sweep may not run right now, or `None` for "go ahead".
///
/// The clock and the idle rule are the night shift's own, read off `[night]`
/// deliberately: this is the same kind of work — a batch of old rows through a
/// model, on a machine somebody may be sitting at — and giving it a second
/// clock to configure would mean two answers to one question. The pause is
/// every worker's.
///
/// What it does **not** borrow is `[night].enabled`. The night shift needs a
/// gigabyte of whisper and a local compile; this needs a 13 MB identifier, and
/// with `lang_sweep_redecode` off it needs no decoder at all. An install that
/// has the identifier and not the night shift should still get its archive
/// read.
///
/// Nor is the GPU ceiling here, and that is not an omission: the only part of
/// this pass that touches the GPU is the French re-decode, and
/// [`crate::polyglot::Polyglot::redecode`] checks
/// `[night].gpu_busy_max_pct` itself, per turn, before it writes a WAV. Putting
/// a second copy of the check in front of a pass that mostly does not use the
/// card would stop the identifier — which runs on four niced CPU cores —
/// because something else was drawing frames.
///
/// Re-read **between rows**, not once per run (see [`run`]): the point of a
/// clock is that somebody can take their machine back at 04:00.
pub fn gate(
    control: &crate::control::Control,
    asr_cfg: &AsrConfig,
    night_cfg: &crate::config::NightConfig,
    minute: u32,
    idle_min: i64,
) -> Option<String> {
    if !asr_cfg.lang_sweep {
        return Some("the archive language sweep is off".to_string());
    }
    if control.is_paused() {
        return Some("capture is paused — nothing is written down, including this".to_string());
    }
    let in_window =
        crate::night::Hours::parse(&night_cfg.window).is_some_and(|h| h.contains(minute));
    let idle_enough = night_cfg.also_when_idle_min > 0 && idle_min >= night_cfg.also_when_idle_min;
    if !in_window && !idle_enough {
        return Some(format!(
            "outside {} and the machine has been busy within the last {} minutes",
            night_cfg.window, night_cfg.also_when_idle_min
        ));
    }
    None
}

/// The background thread. Started whether or not the feature is on, like the
/// night shift's and the quality worker's: the switch is live and something has
/// to be watching it.
///
/// One run per opening of the gate, bounded by
/// `[asr].lang_sweep_rows_per_run`, then a minute's sleep and the gates again.
/// Nothing here is urgent: the rows have been waiting since 0.10.
pub fn run(
    store: Arc<Mutex<Store>>,
    control: Arc<crate::control::Control>,
    bus: Arc<Bus>,
    models_root: Option<std::path::PathBuf>,
    data_dir: std::path::PathBuf,
    cfg: crate::config::Config,
    stop: Arc<crate::night::NightStop>,
) {
    crate::pipeline::background_current_thread(
        cfg.runtime.inference_nice,
        &cfg.runtime.inference_cpus,
    );
    let Some(root) = models_root else {
        debug!("no models root: the archive language sweep will not run");
        return;
    };
    let models = crate::models::ModelSet::resolve_at(root, &cfg.models);
    let mut said = String::new();

    loop {
        if stop.stopped() {
            debug!("the archive language sweep stopped");
            return;
        }
        let night_cfg = control.night();
        match gate(
            &control,
            &cfg.asr,
            &night_cfg,
            local_minute_now(),
            control.idle_minutes(),
        ) {
            Some(reason) => {
                if said != reason {
                    debug!("the archive language sweep is standing down: {reason}");
                    said = reason;
                }
            }
            None => {
                said.clear();
                let pass = Pass::new(&store, Some(&bus), &data_dir, &cfg.asr, &cfg.lang, true);
                // Both routers are built per run rather than held: a sweep that
                // finds nothing to do — the normal state, once the archive is
                // caught up — must not keep 655 MB of Parakeet resident for the
                // life of the daemon. And both from the PASS's config, never
                // the operator's: `Cjk::new` reads `lid_windows` once.
                let mut cjk = Cjk::new(&models, pass.asr_cfg());
                let mut poly = Polyglot::new(&models, pass.asr_cfg(), &night_cfg, &cfg.runtime);
                // The gate again, between every row rather than once at the
                // top of the night — the night shift's rule, for the night
                // shift's reason: a person who sits down at 04:00 gets their
                // cores back within one turn, not within four hundred.
                let stopped = || {
                    stop.stopped()
                        || gate(
                            &control,
                            &cfg.asr,
                            &night_cfg,
                            local_minute_now(),
                            control.idle_minutes(),
                        )
                        .is_some()
                };
                match run_pass(
                    &pass,
                    &mut cjk,
                    &mut poly,
                    32,
                    Some(cfg.asr.lang_sweep_rows_per_run),
                    &stopped,
                    |_| {},
                ) {
                    Ok(r) if r.settled() > 0 => info!(
                        scanned = r.scanned,
                        asked = r.asked,
                        settled = r.settled(),
                        "the archive language sweep settled some rows"
                    ),
                    Ok(_) => {}
                    Err(e) => tracing::warn!("an archive language sweep failed: {e:#}"),
                }
            }
        }
        let step = std::time::Duration::from_millis(250);
        let mut slept = std::time::Duration::ZERO;
        while slept < std::time::Duration::from_secs(60) {
            if stop.stopped() {
                break;
            }
            std::thread::sleep(step);
            slept += step;
        }
    }
}

/// Local minute of day, from the system clock. The night shift's own, which is
/// private to it; duplicated here rather than made public because it is four
/// lines and making it public would suggest it is an interface.
fn local_minute_now() -> u32 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let local = now + crate::clock::local_offset_s(now * 1_000_000_000);
    (local.rem_euclid(86_400) / 60) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn candidate(
        id: i64,
        duration_s: f32,
        text: Option<&str>,
        declared: Option<&[&str]>,
    ) -> SweepCandidate {
        SweepCandidate {
            id,
            duration_s,
            audio_path: format!("segments/000001/seg-{id}.wav"),
            text: text.map(str::to_string),
            declared: declared.map(tags),
        }
    }

    fn store() -> Arc<Mutex<Store>> {
        Arc::new(Mutex::new(Store::open_in_memory().expect("a store")))
    }

    fn routers(cfg: &AsrConfig) -> (Cjk, Polyglot) {
        let models = crate::models::ModelSet::resolve_at(
            std::path::PathBuf::from("/nonexistent/nx-recall-sweep"),
            &crate::config::ModelsConfig::default(),
        );
        (
            Cjk::new(&models, cfg),
            Polyglot::new(
                &models,
                cfg,
                &crate::config::NightConfig::default(),
                &crate::config::RuntimeConfig::default(),
            ),
        )
    }

    #[test]
    fn the_sweep_asks_exactly_the_rows_the_live_path_would_have_asked_about() {
        // Not a re-implementation of the rule — an assertion that the rule is
        // the live one. If `pre_route` ever changes, this test changes with it
        // and the sweep needs no edit at all.
        let readable = candidate(1, 2.0, Some("ich glaube das ist der einzige weg"), None);
        let pinned = candidate(2, 2.0, Some("mumble"), Some(&["de"]));
        let unreadable = candidate(3, 2.0, Some("Sima Sen Okenki Deska."), None);
        let silent = candidate(4, 2.0, None, None);
        let cfg = AsrConfig::default();
        for (row, want) in [
            (&readable, Pre::Nothing),
            (&pinned, Pre::Nothing),
            (&unreadable, Pre::AskLid),
            (&silent, Pre::AskLid),
        ] {
            assert_eq!(
                asr_cjk::pre_route(row.declared.as_ref(), row.text.as_deref(), &cfg),
                want,
                "row {}",
                row.id
            );
        }
        // And a voice pinned to one of the three goes straight to its decoder,
        // sweep or no sweep: a declaration outranks a reading, retroactively as
        // well as live.
        let japanese = candidate(5, 2.0, Some("Wanky Daska."), Some(&["ja"]));
        assert_eq!(
            asr_cjk::pre_route(japanese.declared.as_ref(), japanese.text.as_deref(), &cfg),
            Pre::Direct("ja")
        );
    }

    #[test]
    fn a_row_pre_route_declines_costs_no_model_and_is_never_marked() {
        // The rule that keeps a re-run honest: nothing was learned about this
        // row, so nothing is written about it, so a declaration added tomorrow
        // still reaches it. It is remembered for the length of the run only.
        let store = store();
        let cfg = AsrConfig::default();
        let (mut cjk, mut poly) = routers(&cfg);
        let lang_cfg = LangConfig::default();
        let pass = Pass::new(
            &store,
            None,
            std::path::Path::new("/nonexistent"),
            &cfg,
            &lang_cfg,
            true,
        );
        let row = candidate(1, 2.0, Some("i think that is the only way"), None);
        assert_eq!(
            sweep_one(&pass, &mut cjk, &mut poly, &row).unwrap(),
            Swept::LeftAlone
        );
        // Not even the WAV was looked for — the path above does not exist and
        // this did not fail.
    }

    #[test]
    fn without_the_identifier_nothing_is_written_and_nothing_is_marked() {
        // The failure that would be worst: a machine with no models marking its
        // whole archive as swept, so that installing them later fixes nothing.
        let store = store();
        let cfg = AsrConfig::default();
        let (mut cjk, mut poly) = routers(&cfg);
        assert!(!cjk.ready(), "no models means nothing is ready");
        let lang_cfg = LangConfig::default();
        let pass = Pass::new(
            &store,
            None,
            std::path::Path::new("/nonexistent"),
            &cfg,
            &lang_cfg,
            true,
        );
        // A row whose audio is missing is marked (there is nothing left to ask
        // about); a row whose audio is there but has no identifier is not.
        let row = candidate(1, 2.0, Some("Sima Sen Okenki Deska."), None);
        assert_eq!(
            sweep_one(&pass, &mut cjk, &mut poly, &row).unwrap(),
            Swept::NoAudio,
            "a missing WAV is the only thing a modelless machine can conclude"
        );
    }

    #[test]
    fn the_sweeps_floor_is_its_own_and_is_never_below_the_live_one() {
        // The measured narrowing (FINDINGS §30). A sweep floor an operator sets
        // BELOW the live floor is not honoured: this pass is the one place
        // where a lower floor is decided by a machine at 04:00 rather than by a
        // person watching, and it may only ever be stricter than the live path.
        let strict = AsrConfig {
            lid_min_s: 1.0,
            lang_sweep_min_s: 1.5,
            lang_sweep_windows: 3,
            ..AsrConfig::default()
        };
        assert_eq!(routing_cfg(&strict).lid_min_s, 1.5);
        assert_eq!(routing_cfg(&strict).lid_windows, 3);
        // Both narrowings are one-directional: an operator who writes a looser
        // number than the live path gets the live path's.
        let wishful = AsrConfig {
            lid_min_s: 1.0,
            lid_windows: 2,
            lang_sweep_min_s: 0.4,
            lang_sweep_windows: 1,
            ..AsrConfig::default()
        };
        assert_eq!(
            routing_cfg(&wishful).lid_min_s,
            1.0,
            "the sweep may be stricter than the live path and never looser"
        );
        assert_eq!(routing_cfg(&wishful).lid_windows, 2);
        // And everything else on the config is the operator's, untouched: a
        // sweep that widened the allowlist would be doing something the live
        // path is not allowed to do.
        assert_eq!(
            routing_cfg(&wishful).polyglot_languages,
            wishful.polyglot_languages
        );
        assert_eq!(
            routing_cfg(&wishful).lid_min_confidence,
            wishful.lid_min_confidence
        );
        assert_eq!(routing_cfg(&wishful).japanese, wishful.japanese);
        // The shipped pair, and the reason the two exist at all. The floor is
        // still strictly stricter; the window count is only *no looser* since
        // 0.11.10, when the live path moved to three windows on the same
        // evidence this pass did (FINDINGS §31). The field stays because the
        // narrowing is one-directional: an operator who lowers `lid_windows`
        // for a machine short of cores must not thereby lower it for four
        // hundred archive rows decided at 04:00.
        let shipped = AsrConfig::default();
        assert!(shipped.lang_sweep_min_s > shipped.lid_min_s);
        assert!(shipped.lang_sweep_windows >= shipped.lid_windows);
        let thrifty = AsrConfig {
            lid_windows: 1,
            ..AsrConfig::default()
        };
        assert_eq!(
            routing_cfg(&thrifty).lid_windows,
            3,
            "the sweep keeps three"
        );
    }

    #[test]
    fn the_sweep_ships_writing_a_language_and_never_a_word() {
        // FINDINGS §30, and the whole shape of the round. The routes' 0.97%
        // de/en false-positive rate is inside the 1% gate §22 and §28 shipped
        // under — and on an archive with almost no real foreign turns to be
        // right about, that rate was nearly the whole output: 8 of 9 rewrites
        // wrong. A rate that is fine as a tax on a benefit is not a benefit.
        let shipped = AsrConfig::default();
        assert!(shipped.lang_sweep, "the sweep itself is on");
        assert!(
            !shipped.lang_sweep_redecode,
            "and the half that can rewrite a transcript is not"
        );
        let store = store();
        let lang_cfg = LangConfig::default();
        let pass = Pass::new(
            &store,
            None,
            std::path::Path::new("/nonexistent"),
            &shipped,
            &lang_cfg,
            true,
        );
        assert!(!pass.redecode);
        // Turning it on is supported and is one field, not a fork of the pass.
        let opted_in = AsrConfig {
            lang_sweep_redecode: true,
            ..AsrConfig::default()
        };
        let pass = Pass::new(
            &store,
            None,
            std::path::Path::new("/nonexistent"),
            &opted_in,
            &lang_cfg,
            true,
        );
        assert!(pass.redecode);
        // Either way the narrowings stand: the switch decides whether a
        // transcript may be replaced, not how sure the identifier has to be.
        assert_eq!(pass.asr_cfg().lid_windows, shipped.lang_sweep_windows);
        assert_eq!(pass.asr_cfg().lid_min_s, shipped.lang_sweep_min_s);
    }

    #[test]
    fn only_de_and_en_are_stamped_off_a_reading_alone() {
        // Everything else either routes — and then a decoder's words and a
        // judge stand behind the tag — or is marked with no claim at all. The
        // two here are the two the routes deliberately never act on, so "the
        // identifier said so" is the whole of the available evidence and the
        // `lang_via` says which.
        assert_eq!(STAMPABLE, ["de", "en"]);
        for tag in STAMPABLE {
            assert!(
                crate::lang::KNOWN.contains(tag),
                "{tag} is stamped and unknown"
            );
            assert!(!polyglot::is_routable(tag), "{tag} is stamped AND routed");
            assert!(!asr_cjk::CJK.contains(tag), "{tag} is stamped AND routed");
        }
        // And the stamp is its own provenance: a consumer reading `lid` is
        // promised a re-decode, and these rows never had one.
        assert_eq!(lang_via::SWEEP, "sweep");
        assert_ne!(lang_via::SWEEP, asr_cjk::LANG_VIA_LID);
        assert_ne!(lang_via::SWEEP, lang_via::CLASSIFIED);
        assert_ne!(lang_via::SWEEP, lang_via::GUESSED);
    }

    #[test]
    fn a_swept_row_leaves_the_work_list_and_a_left_alone_row_does_not() {
        let store = Store::open_in_memory().expect("a store");
        let source = store
            .upsert_source("VRChat.exe", "VRChat.exe", 1)
            .expect("a source");
        let session = store.begin_session(source, 0).expect("a session");
        let mut ids = Vec::new();
        for i in 0..3 {
            let id = store
                .insert_segment(
                    session,
                    i * 10_000_000_000,
                    i * 10_000_000_000 + 2_000_000_000,
                    &format!("segments/000001/seg-{i}.wav"),
                    0,
                )
                .expect("a segment");
            store
                .set_segment_text_via(id, "Sima Sen Okenki Deska.", "m@1", "live", 0)
                .expect("text");
            ids.push(id);
        }
        // All three are owed a sweep.
        assert_eq!(
            store.segments_for_lang_sweep(1.0, 10).unwrap().len(),
            3,
            "every untagged row with audio is on the list"
        );
        // One stamped, one marked: both leave.
        store
            .set_segment_language(ids[0], "de", lang_via::SWEEP)
            .unwrap();
        store.mark_segment_swept(ids[1]).unwrap();
        let left = store.segments_for_lang_sweep(1.0, 10).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, ids[2]);
        // The counts a `lang status` line reads say the same thing.
        assert_eq!(store.lang_sweep_counts(1.0).unwrap(), (1, 2));
        // And the floor really is a floor: at 2.5 s none of these 2 s rows is
        // on the list at all.
        assert!(store.segments_for_lang_sweep(2.5, 10).unwrap().is_empty());
    }

    #[test]
    fn the_mismatch_backlog_belongs_to_lang_repair_and_the_sweep_never_takes_it() {
        // Two features, one column. `lang repair` walks rows marked as a
        // disagreement and this walks rows nobody ever asked about; a sweep
        // that overwrote `mismatch` with `sweep` would silently empty the other
        // one's queue.
        let store = Store::open_in_memory().expect("a store");
        let source = store
            .upsert_source("VRChat.exe", "VRChat.exe", 1)
            .expect("a source");
        let session = store.begin_session(source, 0).expect("a session");
        let id = store
            .insert_segment(session, 0, 2_000_000_000, "segments/000001/a.wav", 0)
            .expect("a segment");
        store
            .set_segment_text_via(id, "mumble", "m@1", "live", 0)
            .expect("text");
        store.mark_segment_language_mismatch(id).expect("a mark");
        assert!(store.segments_for_lang_sweep(1.0, 10).unwrap().is_empty());
        assert_eq!(store.language_mismatch_backlog(10).unwrap().len(), 1);
    }

    #[test]
    fn a_sweep_stamp_is_not_evidence_about_the_conversation() {
        // The conversational prior is built out of turns something *read*. A
        // sweep stamp is one second of audio nobody could read, judged by
        // nothing, and letting it vote would turn this feature's uncertainty
        // into the prior that decides other rows.
        let store = Store::open_in_memory().expect("a store");
        let source = store
            .upsert_source("VRChat.exe", "VRChat.exe", 1)
            .expect("a source");
        let session = store.begin_session(source, 0).expect("a session");
        let mut ids = Vec::new();
        for i in 0..3 {
            let id = store
                .insert_segment(
                    session,
                    i * 1_000_000_000,
                    i * 1_000_000_000 + 500_000_000,
                    "a.wav",
                    0,
                )
                .expect("a segment");
            store.set_segment_thread(id, 7, 0).expect("a thread");
            ids.push(id);
        }
        store
            .set_segment_language(ids[0], "de", lang_via::CLASSIFIED)
            .unwrap();
        store
            .set_segment_language(ids[1], "en", lang_via::SWEEP)
            .unwrap();
        store
            .set_segment_language(ids[2], "en", lang_via::CONTEXT)
            .unwrap();
        assert_eq!(
            store.thread_language_stamps(7, -1, 10).unwrap(),
            vec!["de".to_string()],
            "only the turn whose words were read votes"
        );
    }

    // ---- the nightly gate --------------------------------------------------

    fn control() -> Arc<crate::control::Control> {
        crate::control::Control::new(
            std::path::PathBuf::from("/nonexistent"),
            None,
            &crate::allowlist::Allowlist::from_rules([("VRChat.exe", true)]),
        )
    }

    #[test]
    fn the_nightly_gate_is_the_night_shifts_clock_and_not_its_switch() {
        let night = crate::config::NightConfig::default();
        assert!(!night.enabled, "the night shift ships off");
        let on = AsrConfig::default();
        // Inside the window, with the night shift itself switched off: the CJK
        // half needs 13 MB and a sherpa session, not a gigabyte of whisper, and
        // an install that has one and not the other still gets its Japanese
        // back.
        assert!(gate(&control(), &on, &night, 4 * 60, 0).is_none());
        // Outside it, and busy.
        assert!(gate(&control(), &on, &night, 14 * 60, 2).is_some());
        // Outside it, and idle for long enough.
        assert!(gate(&control(), &on, &night, 14 * 60, 40).is_none());
        // Paused beats the clock, at any hour: nothing is written down while
        // capture is paused, including this.
        let paused = control();
        paused.pause();
        assert!(
            gate(&paused, &on, &night, 4 * 60, 999)
                .unwrap()
                .contains("paused")
        );
        // And the switch.
        let off = AsrConfig {
            lang_sweep: false,
            ..AsrConfig::default()
        };
        assert!(
            gate(&control(), &off, &night, 4 * 60, 0)
                .unwrap()
                .contains("off")
        );
    }

    #[test]
    fn the_budget_counts_model_passes_and_a_free_row_never_spends_one() {
        // The bug this rule exists to prevent, and it is not hypothetical: this
        // archive holds 2,300 untagged rows whose transcripts already read as
        // something. A nightly budget of 400 spent walking past those reaches
        // the first row worth asking about on night six.
        //
        // The walk still terminates on an archive that is nothing but those:
        // every one goes into `passed`, the query stops returning anything new,
        // and the loop ends — which is what this exercises, with no models
        // installed and therefore no way to spend the budget at all.
        let store = Store::open_in_memory().expect("a store");
        let source = store
            .upsert_source("VRChat.exe", "VRChat.exe", 1)
            .expect("a source");
        let session = store.begin_session(source, 0).expect("a session");
        for i in 0..5 {
            let id = store
                .insert_segment(
                    session,
                    i * 10_000_000_000,
                    i * 10_000_000_000 + 2_000_000_000,
                    "segments/000001/a.wav",
                    0,
                )
                .expect("a segment");
            store
                .set_segment_text_via(id, "ich glaube das ist der einzige weg", "m@1", "live", 0)
                .expect("text");
        }
        let store = Arc::new(Mutex::new(store));
        let cfg = AsrConfig::default();
        let lang_cfg = LangConfig::default();
        let pass = Pass::new(
            &store,
            None,
            std::path::Path::new("/nonexistent"),
            &cfg,
            &lang_cfg,
            true,
        );
        let (mut cjk, mut poly) = routers(pass.asr_cfg());
        let never = || false;
        let report = run_pass(&pass, &mut cjk, &mut poly, 2, Some(1), &never, |_| {}).unwrap();
        assert_eq!(report.asked, 0, "nothing cost a model");
        assert_eq!(
            report.left_alone, 5,
            "and the budget of 1 did not stop the walk after one free row"
        );
        assert_eq!(report.scanned, 5);
        // Nothing was written, so the work list is exactly as long as it was.
        let guard = store.lock().unwrap();
        assert_eq!(guard.segments_for_lang_sweep(1.5, 10).unwrap().len(), 5);
    }

    #[test]
    fn a_report_counts_a_row_once_however_it_was_settled() {
        let mut r = SweepReport::default();
        SweepReport::count(&mut r.routed, "ja");
        SweepReport::count(&mut r.routed, "ja");
        SweepReport::count(&mut r.routed, "fr");
        SweepReport::count(&mut r.stamped, "de");
        assert_eq!(r.settled(), 4);
        assert_eq!(r.routed.get("ja"), Some(&2));
        assert_eq!(SweepReport::default().settled(), 0);
    }
}
