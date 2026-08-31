//! NX Recall capture daemon — build order Step 1.
//!
//! Capture allowlisted application audio from PipeWire, segment it with Silero
//! VAD, and store the segments. No ASR, no speaker embedding, no GUI, no IPC
//! yet; those are later steps and deliberately absent.

mod allowlist;
mod capture;
mod cli;
mod clock;
mod config;
mod pipeline;
mod queue;
mod resample;
mod store;
mod vad;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::info;

use crate::cli::{Cli, Command};
use crate::clock::utc_now_ns;
use crate::config::{Config, SAMPLE_RATE};
use crate::pipeline::{Pipeline, Stats};
use crate::queue::EventQueue;
use crate::store::Store;

/// ~640 KB, bundled so the daemon has no runtime asset lookup.
pub const VAD_MODEL: &[u8] = include_bytes!("../models/silero_vad.onnx");

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
