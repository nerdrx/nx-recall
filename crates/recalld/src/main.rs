//! NX Recall capture daemon — build order Steps 1-3.
//!
//! Capture allowlisted application audio from PipeWire, segment it with Silero
//! VAD, merge it into turns, transcribe it, and label the voice. No GUI and no
//! IPC yet; those are later steps and deliberately absent.

mod cli;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::info;

use recalld::analysis::AnalysisStats;
use recalld::capture;
use recalld::clock::utc_now_ns;
use recalld::config::{self, Config, SAMPLE_RATE};
use recalld::models::ModelSet;
use recalld::pipeline::{self, Pipeline, Stats};
use recalld::queue::EventQueue;
use recalld::store::Store;

use crate::cli::{Cli, Command, ModelsAction};

fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            // ONNX Runtime narrates every graph transformation at INFO, which
            // would bury our own lines; RUST_LOG can still turn it back on.
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,ort=warn")),
        )
        .init();

    let config_path = match &cli.config {
        Some(p) => p.clone(),
        None => config::default_config_path()?,
    };
    let data_dir = match &cli.data_dir {
        Some(p) => p.clone(),
        None => config::default_data_dir()?,
    };
    let cfg = Config::load(&config_path)?;

    match cli.command {
        Command::Run => cmd_run(&cfg, &data_dir),
        Command::Probe => cmd_probe(&cfg),
        Command::Sources => cmd_sources(&data_dir),
        Command::Allow { match_key } => cmd_set_rule(&config_path, &data_dir, &match_key, true),
        Command::Deny { match_key } => cmd_set_rule(&config_path, &data_dir, &match_key, false),
        Command::Models { action } => match action {
            ModelsAction::Status => cmd_models_status(&cfg),
        },
        Command::Speakers => cmd_speakers(&data_dir),
        Command::Name {
            speaker_id,
            display_name,
        } => cmd_name(&data_dir, speaker_id, &display_name),
        Command::Merge { from, into } => cmd_merge(&data_dir, from, into),
        Command::Search { query, limit } => cmd_search(&data_dir, &query.join(" "), limit),
        Command::Transcript { session, speaker } => {
            cmd_transcript(&data_dir, session, speaker.as_deref())
        }
    }
}

/// Block SIGINT/SIGTERM in this thread *before* any worker thread is spawned.
///
/// PipeWire's main loop reads these signals from a signalfd, which only sees
/// them if every thread has them blocked — a thread that spawned before the
/// mask was set inherits an empty mask, the kernel picks it for delivery, and
/// the default action kills the daemon mid-segment instead of shutting it down.
fn block_shutdown_signals() {
    // SAFETY: the set is zeroed before use and the calls take only that set.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
}

fn cmd_run(cfg: &Config, data_dir: &Path) -> Result<()> {
    block_shutdown_signals();

    let store = Store::open(data_dir)?;
    let closed = store.close_dangling_sessions(utc_now_ns())?;
    if closed > 0 {
        info!("closed {closed} session(s) left open by a previous run");
    }
    // Config is the source of truth for rules; mirror it so `sources` and any
    // later GUI see the same answer the capture path used.
    for (key, rule) in &cfg.rules {
        store.set_allowed(key, rule.allowed(), utc_now_ns())?;
    }
    let store = Arc::new(std::sync::Mutex::new(store));

    let queue = EventQueue::for_seconds(cfg.capture.queue_seconds, SAMPLE_RATE);
    let stats = Arc::new(Stats::default());
    let analysis_stats = Arc::new(AnalysisStats::default());
    info!(
        data_dir = %data_dir.display(),
        queue_capacity_samples = queue.capacity_samples(),
        "starting"
    );

    let mut pipeline = Pipeline::new(
        cfg,
        Arc::clone(&store),
        data_dir.to_path_buf(),
        Arc::clone(&stats),
        Arc::clone(&analysis_stats),
    )?;
    let nice = cfg.runtime.inference_nice;
    let cpus = cfg.runtime.inference_cpus.clone();
    let queue_for_thread = Arc::clone(&queue);
    let inference = std::thread::Builder::new()
        .name("recalld-vad".into())
        .spawn(move || {
            pipeline::deprioritise_current_thread(nice, &cpus);
            pipeline.run(queue_for_thread);
        })
        .context("spawning the inference thread")?;

    let result = capture::run(cfg, store, Arc::clone(&queue), Arc::clone(&stats));

    // Let the pipeline finish whatever is still queued before we exit.
    info!(
        backlog_seconds = queue.queued_samples() as f32 / SAMPLE_RATE as f32,
        "draining the analysis queue"
    );
    queue.close();
    if inference.join().is_err() {
        anyhow::bail!("the inference thread panicked");
    }

    info!(
        sessions = stats.sessions_opened.load(Ordering::Relaxed),
        segments = stats.segments_written.load(Ordering::Relaxed),
        frames = stats.frames_analysed.load(Ordering::Relaxed),
        analysed = analysis_stats.analysed.load(Ordering::Relaxed),
        labelled = analysis_stats.labelled.load(Ordering::Relaxed),
        refused_overlap = analysis_stats.refused_overlap.load(Ordering::Relaxed),
        dropped_buffers = queue.dropped_chunks(),
        dropped_seconds = queue.dropped_samples() as f32 / SAMPLE_RATE as f32,
        "stopped"
    );
    result
}

fn cmd_probe(cfg: &Config) -> Result<()> {
    let allowlist = cfg.allowlist();
    let nodes = capture::probe(&allowlist)?;

    if nodes.is_empty() {
        println!("No application playback streams (Stream/Output/Audio) on the graph.");
        return Ok(());
    }

    println!(
        "{:>5}  {:>8}  {:<24}  {:<28}  {:>8}  CAPTURE",
        "NODE", "SERIAL", "MATCH KEY", "APPLICATION", "PID"
    );
    for (node, decision) in &nodes {
        println!(
            "{:>5}  {:>8}  {:<24}  {:<28}  {:>8}  {}",
            node.node_id,
            node.serial.as_deref().unwrap_or("-"),
            node.ident.match_key(),
            node.ident.display_name(),
            node.ident
                .process_id
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".into()),
            decision.as_str(),
        );
        if let Some(bin) = node.ident.process_binary.as_deref()
            && bin != node.ident.match_key()
        {
            // Wine: the loader binary is shared, so say what we keyed on instead.
            println!("       (application.process.binary = {bin}, keyed on the PE name)");
        }
    }
    Ok(())
}

fn cmd_sources(data_dir: &Path) -> Result<()> {
    let store = Store::open(data_dir)?;
    let rows = store.list_sources()?;
    if rows.is_empty() {
        println!("No sources seen yet. Run `recalld run` (or `recalld probe`) first.");
        return Ok(());
    }
    println!(
        "{:>3}  {:<7}  {:<24}  {:<28}  FIRST SEEN (UTC ns)",
        "ID", "ALLOWED", "MATCH KEY", "DISPLAY NAME"
    );
    for r in rows {
        println!(
            "{:>3}  {:<7}  {:<24}  {:<28}  {}",
            r.id,
            if r.allowed { "yes" } else { "no" },
            r.match_key,
            r.display_name,
            r.first_seen
        );
    }
    Ok(())
}

fn cmd_set_rule(config_path: &Path, data_dir: &Path, match_key: &str, allowed: bool) -> Result<()> {
    let mut cfg = Config::load(config_path)?;
    cfg.set_rule(match_key, allowed);
    cfg.save(config_path)?;

    let store = Store::open(data_dir)?;
    store.set_allowed(match_key, allowed, utc_now_ns())?;

    println!(
        "{} {match_key}\n  config: {}\n  restart `recalld run` to apply.",
        if allowed { "Allowed" } else { "Denied" },
        config_path.display()
    );
    Ok(())
}

fn cmd_models_status(cfg: &Config) -> Result<()> {
    let Some(models) = ModelSet::resolve(&cfg.models) else {
        println!(
            "No [models].dir configured — recalld will capture and segment but not\n\
             transcribe or identify. Point it at a directory holding the models."
        );
        return Ok(());
    };
    println!("models dir: {}", models.root.display());
    println!("{:<14}  {:<9}  {:>10}  PATH", "ROLE", "PRESENT", "SIZE");
    for e in models.entries() {
        let size = e
            .bytes()
            .map(|b| format!("{:.1} MB", b as f64 / 1_048_576.0))
            .unwrap_or_else(|| "-".into());
        println!(
            "{:<14}  {:<9}  {:>10}  {}",
            e.role,
            if e.present() { "yes" } else { "MISSING" },
            size,
            e.path.display()
        );
    }
    println!();
    println!("asr model id:       {}", models.asr_model_id());
    println!("embedding model id: {}", models.embed_model_id());
    if models.complete() {
        println!("\nAll models present.");
    } else {
        println!(
            "\n{} model file(s) missing; analysis stays off until they are there.",
            models.missing().len()
        );
    }
    Ok(())
}

fn cmd_speakers(data_dir: &Path) -> Result<()> {
    let store = Store::open(data_dir)?;
    let rows = store.list_speakers()?;
    if rows.is_empty() {
        println!("No voices yet.");
        return Ok(());
    }
    println!("{:>4}  {:<24}  {:>8}  SPEECH", "ID", "NAME", "SEGMENTS");
    for r in rows {
        println!(
            "{:>4}  {:<24}  {:>8}  {}",
            r.id,
            r.display_name,
            r.segments,
            format_duration(r.speech_ns)
        );
    }
    Ok(())
}

fn cmd_name(data_dir: &Path, speaker_id: i64, display_name: &str) -> Result<()> {
    let store = Store::open(data_dir)?;
    store.rename_speaker(speaker_id, display_name)?;
    let n = store.transcript(None, Some(speaker_id))?.len();
    println!("Speaker {speaker_id} is now \"{display_name}\" ({n} segment(s), past and future).");
    Ok(())
}

fn cmd_merge(data_dir: &Path, from: i64, into: i64) -> Result<()> {
    let store = Store::open(data_dir)?;
    let r = store.merge_speakers(from, into)?;
    println!("Merged speaker {} into {}:", r.from, r.into);
    println!("  {:>4} segment(s) reassigned", r.segments);
    println!("  {:>4} prototype(s) moved", r.prototypes);
    println!("  {:>4} golden sample(s) moved", r.golden_samples);
    if r.tombstones_repointed > 0 {
        println!(
            "  {:>4} earlier merge(s) re-pointed at {} (no chains)",
            r.tombstones_repointed, r.into
        );
    }
    Ok(())
}

fn cmd_search(data_dir: &Path, query: &str, limit: usize) -> Result<()> {
    if query.trim().is_empty() {
        anyhow::bail!("nothing to search for");
    }
    let store = Store::open(data_dir)?;
    let hits = store.search(query, limit)?;
    if hits.is_empty() {
        println!("No matches for {query:?}.");
        return Ok(());
    }
    for h in hits {
        println!(
            "{}  {:<20}  {}",
            format_time(h.t_start_ns),
            h.speaker.unwrap_or_else(|| "-".into()),
            h.snippet
        );
    }
    Ok(())
}

fn cmd_transcript(data_dir: &Path, session: Option<i64>, speaker: Option<&str>) -> Result<()> {
    let store = Store::open(data_dir)?;
    let speaker_id = match speaker {
        None => None,
        Some(s) => match s.parse::<i64>() {
            Ok(id) => Some(store.resolve_speaker(id)?),
            Err(_) => Some(
                store
                    .find_speaker_by_name(s)?
                    .with_context(|| format!("no speaker named {s:?}"))?,
            ),
        },
    };
    let rows = store.transcript(session, speaker_id)?;
    if rows.is_empty() {
        println!("Nothing recorded for that filter.");
        return Ok(());
    }
    for r in rows {
        let Some(text) = r.text else { continue };
        println!(
            "{}  {:<20}  {}",
            format_time(r.t_start_ns),
            r.speaker.unwrap_or_else(|| "-".into()),
            text
        );
    }
    Ok(())
}

/// UTC nanoseconds as `YYYY-MM-DD HH:MM:SS`, computed here rather than pulling
/// in a date crate for one format string.
fn format_time(utc_ns: i64) -> String {
    let secs = utc_ns.div_euclid(1_000_000_000);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        tod / 3600,
        (tod / 60) % 60,
        tod % 60
    )
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 to a calendar date.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn format_duration(ns: i64) -> String {
    let secs = ns / 1_000_000_000;
    if secs >= 3600 {
        format!("{}h{:02}m", secs / 3600, (secs / 60) % 60)
    } else if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_epoch_formats_as_the_epoch() {
        assert_eq!(format_time(0), "1970-01-01 00:00:00");
    }

    #[test]
    fn a_known_instant_formats_correctly() {
        // 2026-08-31T18:46:00Z
        assert_eq!(
            format_time(1_788_201_960_000_000_000),
            "2026-08-31 18:46:00"
        );
    }

    #[test]
    fn durations_scale_their_unit() {
        assert_eq!(format_duration(5_000_000_000), "5s");
        assert_eq!(format_duration(125_000_000_000), "2m05s");
        assert_eq!(format_duration(7_320_000_000_000), "2h02m");
    }
}
