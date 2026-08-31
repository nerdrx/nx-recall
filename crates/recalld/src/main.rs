//! NX Recall capture daemon — build order Steps 1-4.
//!
//! Capture allowlisted application audio from PipeWire, segment it with Silero
//! VAD, merge it into turns, transcribe it, label the voice, and serve all of
//! it over a Unix socket to clients that never touch the database directly.

mod cli;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use clap::Parser;
use serde_json::{Value, json};
use tracing::{info, warn};

use recalld::analysis::AnalysisStats;
use recalld::bus::Bus;
use recalld::capture;
use recalld::client;
use recalld::clock::utc_now_ns;
use recalld::config::{self, Config, SAMPLE_RATE};
use recalld::control::Control;
use recalld::fetch;
use recalld::models::{self, EntryState, ModelSet};
use recalld::pipeline::{self, Pipeline, Stats};
use recalld::queue::EventQueue;
use recalld::retention::{self, SweeperStop};
use recalld::roster::{self, RosterStop};
use recalld::server;
use recalld::service::Service;
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
        Command::Run => cmd_run(&cfg, &data_dir, &config_path),
        Command::Probe => cmd_probe(&cfg),
        Command::Sources => cmd_sources(&data_dir),
        Command::Allow { match_key } => cmd_set_rule(&config_path, &data_dir, &match_key, true),
        Command::Deny { match_key } => cmd_set_rule(&config_path, &data_dir, &match_key, false),
        Command::Models { action } => match action {
            ModelsAction::Status { dir } => {
                cmd_models_status(&cfg, &data_dir, dir.as_deref(), false)
            }
            ModelsAction::Fetch {
                dir,
                force,
                no_config,
            } => cmd_models_fetch(
                &cfg,
                &config_path,
                &data_dir,
                dir.as_deref(),
                force,
                no_config,
            ),
        },
        Command::Speakers => cmd_speakers(&data_dir),
        Command::Name {
            speaker_id,
            display_name,
        } => cmd_name(&data_dir, speaker_id, &display_name),
        Command::Merge { from, into } => cmd_merge(&data_dir, from, into),
        Command::Split { speaker_id } => cmd_split(&cfg, &data_dir, speaker_id),
        Command::Search { query, limit } => cmd_search(&data_dir, &query.join(" "), limit),
        Command::Transcript { session, speaker } => {
            cmd_transcript(&data_dir, session, speaker.as_deref())
        }
        Command::Pause => cmd_pause(&cfg, &data_dir, true),
        Command::Resume => cmd_pause(&cfg, &data_dir, false),
        Command::Status => cmd_status(&cfg, &data_dir),
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

fn cmd_run(cfg: &Config, data_dir: &Path, config_path: &Path) -> Result<()> {
    block_shutdown_signals();

    let store = Store::open(data_dir)?;
    let closed = store.close_dangling_sessions(utc_now_ns())?;
    if closed > 0 {
        info!("closed {closed} session(s) left open by a previous run");
    }
    // Config is the source of truth for rules at start-up; mirror it so
    // `sources` and the GUI see the same answer the capture path used. From
    // here on the live copy in `Control` is authoritative, and `sources.set`
    // writes both.
    for (key, rule) in &cfg.rules {
        store.set_allowed(key, rule.allowed(), utc_now_ns())?;
    }
    let store = Arc::new(std::sync::Mutex::new(store));

    let queue = EventQueue::for_seconds(cfg.capture.queue_seconds, SAMPLE_RATE);
    let stats = Arc::new(Stats::default());
    let analysis_stats = Arc::new(AnalysisStats::default());
    let control = Control::new(
        data_dir.to_path_buf(),
        Some(config_path.to_path_buf()),
        &cfg.allowlist(),
    )
    .with_pipeline(
        Arc::clone(&queue),
        Arc::clone(&stats),
        Arc::clone(&analysis_stats),
    )
    // The socket's own identity work (`speakers.split`) must use the same
    // operating point the pipeline labelled with, not the defaults.
    .with_identity(cfg.identity.clone());
    if let Some(models) = ModelSet::resolve(&cfg.models).filter(|m| m.complete()) {
        control.set_models(vec![models.asr_model_id(), models.embed_model_id()]);
    }
    let bus = Bus::new(cfg.socket.replay_events, cfg.socket.client_outbox);
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
        Arc::clone(&control),
        Arc::clone(&bus),
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

    // The socket, the roster and the sweeper are all optional: none of them is
    // allowed to cost the daemon its capture, so a failure here is a warning.
    let service = Service::new(Arc::clone(&store), Arc::clone(&control), Arc::clone(&bus));
    let socket = if cfg.socket.enabled {
        let path = config::socket_path(&cfg.socket, data_dir);
        match server::serve(Arc::clone(&service), &path) {
            Ok(s) => Some(s),
            Err(e) => {
                warn!("no control socket: {e:#}");
                None
            }
        }
    } else {
        None
    };

    let roster_stop = Arc::new(RosterStop::default());
    let roster_thread = if cfg.roster.enabled {
        let roster_cfg = cfg.roster.clone();
        let store = Arc::clone(&store);
        let bus = Arc::clone(&bus);
        let control = Arc::clone(&control);
        let stop = Arc::clone(&roster_stop);
        std::thread::Builder::new()
            .name("recalld-roster".into())
            .spawn(move || roster::run(&roster_cfg, store, bus, control, stop))
            .map_err(|e| warn!("no roster tailer: {e}"))
            .ok()
    } else {
        None
    };

    let sweeper_stop = Arc::new(SweeperStop::default());
    let sweeper_thread = if cfg.retention.enabled {
        let retention_cfg = cfg.retention.clone();
        let store = Arc::clone(&store);
        let dir = data_dir.to_path_buf();
        let stop = Arc::clone(&sweeper_stop);
        std::thread::Builder::new()
            .name("recalld-sweeper".into())
            .spawn(move || retention::run(&retention_cfg, store, dir, stop))
            .map_err(|e| warn!("no retention sweeper: {e}"))
            .ok()
    } else {
        None
    };

    let result = capture::run(
        cfg,
        Arc::clone(&store),
        Arc::clone(&queue),
        Arc::clone(&stats),
        Arc::clone(&control),
    );

    roster_stop.stop();
    sweeper_stop.stop();
    if let Some(s) = socket {
        s.shutdown();
    }
    for handle in [roster_thread, sweeper_thread].into_iter().flatten() {
        let _ = handle.join();
    }

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

/// `models status`. With `configured_only` false it falls back to the directory
/// a fetch would use, so `status` never says "nothing configured" about a
/// directory `fetch` just filled — the two commands read the same catalogue and
/// resolve the same path.
fn cmd_models_status(
    cfg: &Config,
    data_dir: &Path,
    dir: Option<&Path>,
    quiet_header: bool,
) -> Result<()> {
    let root = fetch::target_dir(dir, &cfg.models, data_dir);
    let models = ModelSet::resolve_at(root, &cfg.models);

    if !quiet_header && dir.is_none() && cfg.models.dir.is_none() {
        println!(
            "[models].dir is not set in config.toml — reporting on the default,\n\
             {}. Run `recalld models fetch` to populate it.\n",
            models.root.display()
        );
    }
    println!("models dir: {}", models.root.display());
    println!(
        "{:<14}  {:<9}  {:>10}  {:>10}  PATH",
        "ROLE", "STATE", "SIZE", "EXPECTED"
    );
    println!(
        "{:<14}  {:<9}  {:>10}  {:>10}  (compiled into the binary)",
        "vad",
        "ok",
        fetch::human(recalld::VAD_MODEL.len() as u64),
        fetch::human(models::VAD_MODEL_BYTES),
    );
    for e in models.entries() {
        let state = match e.state() {
            EntryState::Ok => "ok",
            EntryState::Missing => "MISSING",
            EntryState::WrongSize { .. } => "BAD SIZE",
        };
        println!(
            "{:<14}  {:<9}  {:>10}  {:>10}  {}",
            e.role,
            state,
            e.bytes().map(fetch::human).unwrap_or_else(|| "-".into()),
            e.expected.map(fetch::human).unwrap_or_else(|| "?".into()),
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
            "\n{} model file(s) missing or the wrong size; analysis stays off until\n\
             they are there. `recalld models fetch` downloads them.",
            models.missing().len()
        );
    }
    Ok(())
}

/// `models fetch` — the setup-time download of DESIGN §4. Everything it needs
/// to know about the network lives in `recalld::fetch`; this is the part that
/// decides *where*, and then leaves the config saying so.
fn cmd_models_fetch(
    cfg: &Config,
    config_path: &Path,
    data_dir: &Path,
    dir: Option<&Path>,
    force: bool,
    no_config: bool,
) -> Result<()> {
    let root = fetch::target_dir(dir, &cfg.models, data_dir);
    println!("models dir: {}", root.display());
    println!(
        "up to {} to download from github.com/k2-fsa/sherpa-onnx\n",
        fetch::human(models::total_download_bytes())
    );

    let report = fetch::fetch_models(&root, &cfg.models, &fetch::FetchOptions { force })?;

    println!(
        "\n{} downloaded, {} already present ({} transferred).",
        report.downloaded,
        report.skipped,
        fetch::human(report.bytes)
    );

    // Point the config at what we just installed. Without this a fetch into the
    // default directory would leave `models status` and the daemon still seeing
    // "analysis off", which is exactly the disagreement this command exists to
    // prevent.
    if !no_config && cfg.models.dir.as_deref() != Some(root.as_path()) {
        let mut updated = Config::load(config_path)?;
        updated.models.dir = Some(root.clone());
        updated.save(config_path)?;
        println!(
            "[models].dir set to {} in {}\n  restart `recalld run` to load them.",
            root.display(),
            config_path.display()
        );
    }

    println!();
    let mut effective = cfg.clone();
    effective.models.dir = Some(root);
    cmd_models_status(&effective, data_dir, None, true)
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
    store.rename_speaker(speaker_id, display_name, utc_now_ns())?;
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

/// `recalld split <id>` goes over the socket rather than straight at the
/// database: a split moves rows between identities, and every connected view
/// has to be told, which only the running daemon can do.
fn cmd_split(cfg: &Config, data_dir: &Path, speaker_id: i64) -> Result<()> {
    let out = call(cfg, data_dir, "speakers.split", json!({"id": speaker_id}))?;
    let kept = out["kept"].as_i64().unwrap_or(speaker_id);
    let minted = out["minted"].as_i64().unwrap_or(0);
    println!(
        "Split speaker {kept}: minted {minted} ({}) for the second voice.",
        out["auto"].as_str().unwrap_or("?")
    );
    println!(
        "  {:>4} segment(s) moved to {minted}",
        out["moved_segments"].as_i64().unwrap_or(0)
    );
    println!(
        "  {:>4} prototype(s) moved",
        out["moved_prototypes"].as_i64().unwrap_or(0)
    );
    let ambiguous = out["ambiguous"].as_i64().unwrap_or(0);
    if ambiguous > 0 {
        println!(
            "  {ambiguous:>4} segment(s) sat between the two voices; they kept speaker \
             {kept} at a reduced score"
        );
    }
    println!(
        "  the two voices score {:.3} against each other",
        out["centroid_similarity"].as_f64().unwrap_or(0.0)
    );
    println!("`recalld name {minted} <who>` once you know who it is.");
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
            format_time(h.row.t_start_ns),
            h.speaker().unwrap_or("-"),
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

/// `recalld pause` / `resume` — the scripting surface from DESIGN §8. The
/// other two are the tray dropdown and the GUI, both of which speak the same
/// socket method.
fn cmd_pause(cfg: &Config, data_dir: &Path, pause: bool) -> Result<()> {
    let out = call(cfg, data_dir, if pause { "pause" } else { "resume" }, json!({}))?;
    let paused = out["paused"].as_bool().unwrap_or(pause);
    if out["changed"].as_bool() == Some(false) {
        println!(
            "Already {}.",
            if paused { "paused" } else { "running" }
        );
    } else if paused {
        println!("Paused. Capture keeps running; nothing is written until `recalld resume`.");
    } else {
        println!("Resumed.");
    }
    Ok(())
}

fn cmd_status(cfg: &Config, data_dir: &Path) -> Result<()> {
    let s = call(cfg, data_dir, "status", json!({}))?;
    println!("{:<18}{}", "daemon", s["daemon"].as_str().unwrap_or("?"));
    println!("{:<18}{}", "uptime", format_duration(s["uptime_s"].as_i64().unwrap_or(0) * 1_000_000_000));
    println!(
        "{:<18}{}",
        "state",
        if s["paused"].as_bool() == Some(true) {
            "PAUSED (no writes)"
        } else {
            "capturing"
        }
    );
    println!(
        "{:<18}{:.1} s queued of {:.0} s, {} buffer(s) dropped",
        "queue",
        s["queue"]["depth_seconds"].as_f64().unwrap_or(0.0),
        s["queue"]["capacity_samples"].as_f64().unwrap_or(0.0) / SAMPLE_RATE as f64,
        s["queue"]["dropped_buffers"].as_i64().unwrap_or(0),
    );
    let models = s["models"]["ids"]
        .as_array()
        .map(|ids| {
            ids.iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    println!(
        "{:<18}{}",
        "models",
        if models.is_empty() {
            "none (capture and VAD only)".to_string()
        } else {
            models
        }
    );
    println!(
        "{:<18}{} segment(s), {} analysed, {} labelled, {} refused (overlap)",
        "counters",
        s["counters"]["segments_written"].as_i64().unwrap_or(0),
        s["counters"]["analysed"].as_i64().unwrap_or(0),
        s["counters"]["labelled"].as_i64().unwrap_or(0),
        s["counters"]["refused_overlap"].as_i64().unwrap_or(0),
    );
    println!(
        "{:<18}{} connected, seq {}",
        "clients",
        s["clients"].as_i64().unwrap_or(0),
        s["seq"].as_i64().unwrap_or(0)
    );
    println!("{:<18}{}", "roster", s["roster_present"].as_i64().unwrap_or(0));
    Ok(())
}

fn call(cfg: &Config, data_dir: &Path, method: &str, params: Value) -> Result<Value> {
    let path: PathBuf = config::socket_path(&cfg.socket, data_dir);
    let mut client = client::Client::connect(&path)?;
    client.call(method, params)
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
