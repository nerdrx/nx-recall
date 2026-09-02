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
use recalld::enrich::{self, EnrichStop};
use recalld::fetch;
use recalld::models::{self, AsrSelection, EntryState, GraphModels, Group, ModelSet};
use recalld::pipeline::{self, Pipeline, Stats};
use recalld::quality::{self, QualityStop};
use recalld::queue::EventQueue;
use recalld::retention::{self, SweeperStop};
use recalld::roster::{self, RosterStop};
use recalld::server;
use recalld::service::Service;
use recalld::store::Store;

use crate::cli::{
    Cli, Command, GraphAction, LangAction, MicAction, ModelsAction, SemanticAction, SpeakersAction,
};

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
        Command::Mic { action } => cmd_mic(&cfg, &data_dir, action),
        Command::Models { action } => match action {
            ModelsAction::Status { dir } => {
                cmd_models_status(&cfg, &data_dir, dir.as_deref(), false)
            }
            ModelsAction::Fetch {
                dir,
                force,
                fallback_asr,
                graph,
                arbiter_de,
                confidence,
                semantic,
                no_config,
            } => cmd_models_fetch(
                &cfg,
                &config_path,
                &data_dir,
                dir.as_deref(),
                &fetch::FetchOptions {
                    force,
                    fallback_asr,
                    graph,
                    semantic,
                    arbiter_de,
                    confidence,
                    single_stream: false,
                },
                no_config,
            ),
        },
        Command::Speakers { action } => match action {
            None => cmd_speakers(&data_dir),
            Some(SpeakersAction::Prune { apply }) => cmd_prune(&cfg, &data_dir, apply),
            Some(SpeakersAction::Delete {
                speaker_id,
                keep_voiceprint,
            }) => cmd_delete_speaker(&cfg, &data_dir, speaker_id, keep_voiceprint),
        },
        Command::Languages { speaker_id, codes } => {
            cmd_languages(&cfg, &data_dir, speaker_id, &codes)
        }
        // The conversational language prior (0.7.7). Like the semantic
        // backfill, the repair runs in THIS process: it is a long batch job
        // that has to be niceable and Ctrl-C-able, and it loads a model the
        // daemon may not have resident.
        Command::Lang { action } => match action.unwrap_or(LangAction::Status) {
            LangAction::Status => cmd_lang_status(&cfg, &data_dir, None),
            LangAction::Repair { batch, limit, dir } => {
                cmd_lang_repair(&cfg, &data_dir, dir.as_deref(), batch, limit)
            }
        },
        Command::Name {
            speaker_id,
            display_name,
        } => cmd_name(&data_dir, speaker_id, &display_name),
        Command::Merge { from, into } => cmd_merge(&data_dir, from, into),
        Command::Split { speaker_id } => cmd_split(&cfg, &data_dir, speaker_id),
        Command::Search {
            query,
            limit,
            smart,
        } => cmd_search(&cfg, &data_dir, &query.join(" "), limit, smart),
        // Semantic search (0.6.5). Both actions run in THIS process rather than
        // over the socket: the backfill is a long batch job that must be
        // niceable and Ctrl-C-able, and neither wants to be an async op.
        Command::Semantic { action } => match action {
            SemanticAction::Status => recalld::semantic::status_command(&data_dir, &cfg),
            SemanticAction::Backfill { batch, limit, dir } => {
                recalld::semantic::backfill_command(&data_dir, &cfg, dir.as_deref(), batch, limit)
            }
        },
        Command::Transcript { session, speaker } => {
            cmd_transcript(&data_dir, session, speaker.as_deref())
        }
        Command::Pause => cmd_pause(&cfg, &data_dir, true),
        Command::Resume => cmd_pause(&cfg, &data_dir, false),
        Command::Status => cmd_status(&cfg, &data_dir),
        Command::Graph { action } => cmd_graph(&cfg, &data_dir, action),
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

/// Has the file we are executing been replaced on disk? After an unlink+copy
/// upgrade the kernel keeps us on the old inode and marks the exe link as
/// deleted — the unambiguous "a new version is installed" signal.
fn exe_replaced(link_target: &std::ffi::OsStr) -> bool {
    link_target.as_encoded_bytes().ends_with(b" (deleted)")
}

/// An update should update: when the binary on disk is replaced (NX Hub
/// unlinks + copies), drain and exit cleanly so systemd's Restart=always
/// brings up the new version. Gated on NXR_EXIT_ON_UPGRADE=1, which only the
/// unit file sets — a `recalld run` in a terminal never exits by surprise.
fn spawn_upgrade_watcher() {
    if std::env::var_os("NXR_EXIT_ON_UPGRADE").as_deref() != Some(std::ffi::OsStr::new("1")) {
        return;
    }
    std::thread::Builder::new()
        .name("recalld-upgrade".into())
        .spawn(|| {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(10));
                match std::fs::read_link("/proc/self/exe") {
                    Ok(p) if exe_replaced(p.as_os_str()) => {
                        info!("binary replaced on disk — restarting onto the new version");
                        // The signalfd shutdown path: identical to systemctl stop,
                        // so the queue drains and the session closes cleanly. Must
                        // be PROCESS-directed (kill, not raise): every thread
                        // blocks SIGTERM for the signalfd, and a thread-directed
                        // signal would sit pending on this thread unseen.
                        unsafe { libc::kill(libc::getpid(), libc::SIGTERM) };
                        return;
                    }
                    _ => {}
                }
            }
        })
        .ok();
}

fn cmd_run(cfg: &Config, data_dir: &Path, config_path: &Path) -> Result<()> {
    block_shutdown_signals();
    spawn_upgrade_watcher();

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
    // Same mirror for the microphone, which is a source row but never a rule.
    store.upsert_source_kind(
        capture::MIC_MATCH_KEY,
        capture::MIC_DISPLAY_NAME,
        recalld::store::KIND_MIC,
        utc_now_ns(),
    )?;
    store.set_allowed(capture::MIC_MATCH_KEY, cfg.mic.enabled, utc_now_ns())?;
    let store = Arc::new(std::sync::Mutex::new(store));

    let queue = EventQueue::for_seconds(cfg.capture.queue_seconds, SAMPLE_RATE);
    let stats = Arc::new(Stats::default());
    let analysis_stats = Arc::new(AnalysisStats::default());
    let models_root = ModelSet::resolve(&cfg.models).map(|m| m.root);
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
    .with_identity(cfg.identity.clone())
    // …and `lang.repair` must use the same guards the pipeline re-decoded with.
    .with_lang(cfg.lang.clone())
    .with_mic(cfg.mic.clone())
    // The memory graph's Tier 3 switch is live, like the microphone's.
    .with_graph(cfg.graph.clone(), models_root.clone())
    // …and the accuracy round's idle worker reads its switches the same way.
    .with_asr(cfg.asr.clone());
    // The ids clients see must be the ids that will be written on segments, so
    // resolve the ASR fallback here exactly as the pipeline does.
    if let Some(mut models) = ModelSet::resolve(&cfg.models) {
        models.select_asr();
        if models.complete() {
            control.set_models(vec![models.asr_model_id(), models.embed_model_id()]);
        }
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
    // Semantic search (0.6.5). Optional in the strongest sense: not installed
    // means keyword search, exactly as before, with no warning — the user did
    // not ask for the feature. One leg, shared by the inference thread (which
    // embeds each turn) and the socket (which embeds each query), because the
    // weights are 118 MB and two copies would be 236.
    let semantic = match recalld::models::SemanticModel::resolve(&cfg.models) {
        Some(sem) if sem.present() => {
            match recalld::semantic::TextEmbedder::load(&sem) {
                Ok(e) => {
                    let leg = Arc::new(recalld::semantic::SemanticLeg::new(e));
                    info!(model = %sem.model_id(), "semantic search is on");
                    pipeline.attach_semantic(Arc::clone(&leg));
                    Some(leg)
                }
                // Loud, because this one IS a broken install: the files are
                // there at the catalogued size and still would not load.
                Err(e) => {
                    warn!("semantic search is off: {e:#}");
                    None
                }
            }
        }
        _ => None,
    };

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
    if let Some(leg) = semantic {
        service.attach_semantic(leg);
    }
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

    // What is on disk, before anything has swept: a daemon that has just come
    // up must be able to answer "how much space is this using" without waiting
    // out a sweep interval. The sweeper re-measures after every pass.
    let models_dir = models_root.clone();
    control.set_storage(retention::measure(
        data_dir,
        models_dir.as_deref(),
        utc_now_ns(),
    ));

    // The memory graph's Tier 3 worker (GRAPH.md). Started whether or not it is
    // enabled: the switch is live, so something has to be watching it — and it
    // is the thing that reports "off" to every client that asks.
    let enrich_stop = Arc::new(EnrichStop::default());
    let enrich_thread = {
        let store = Arc::clone(&store);
        let control = Arc::clone(&control);
        let bus = Arc::clone(&bus);
        let root = models_root.clone();
        let runtime = cfg.runtime.clone();
        let stop = Arc::clone(&enrich_stop);
        std::thread::Builder::new()
            .name("recalld-graph".into())
            .spawn(move || enrich::run(store, control, bus, root, runtime, stop))
            .map_err(|e| warn!("no enrichment worker: {e}"))
            .ok()
    };
    if cfg.graph.enabled {
        info!("the memory graph's local model is enabled; it runs only while nothing is captured");
    }

    // The accuracy round's idle worker (0.8.0). Started for the same reason
    // the enrichment worker is: both its switches are live, and it has to be
    // there to notice them being turned on.
    let quality_stop = Arc::new(QualityStop::default());
    let quality_thread = {
        let store = Arc::clone(&store);
        let control = Arc::clone(&control);
        let bus = Arc::clone(&bus);
        let root = models_root.clone();
        let dir = data_dir.to_path_buf();
        let stats = Arc::clone(&control.quality);
        let stop = Arc::clone(&quality_stop);
        let runtime = cfg.runtime.clone();
        // Resolved exactly as the pipeline resolves it, fallback included, so
        // the worker never re-decodes with a different export than the one that
        // wrote the words it is replacing.
        let models = ModelSet::resolve(&cfg.models).and_then(|mut m| {
            m.select_asr();
            m.complete().then_some(m)
        });
        std::thread::Builder::new()
            .name("recalld-quality".into())
            .spawn(move || {
                quality::run(store, control, bus, models, root, dir, runtime, stats, stop)
            })
            .map_err(|e| warn!("no transcript quality worker: {e}"))
            .ok()
    };

    let sweeper_stop = Arc::new(SweeperStop::default());
    let sweeper_thread = if cfg.retention.enabled {
        let retention_cfg = cfg.retention.clone();
        let store = Arc::clone(&store);
        let dir = data_dir.to_path_buf();
        let models = models_dir.clone();
        let control = Arc::clone(&control);
        let sweeper_bus = Arc::clone(&bus);
        let stop = Arc::clone(&sweeper_stop);
        std::thread::Builder::new()
            .name("recalld-sweeper".into())
            .spawn(move || {
                retention::run(
                    &retention_cfg,
                    store,
                    dir,
                    models,
                    control,
                    sweeper_bus,
                    stop,
                )
            })
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
        Arc::clone(&bus),
    );

    roster_stop.stop();
    sweeper_stop.stop();
    enrich_stop.stop();
    quality_stop.stop();
    if let Some(s) = socket {
        s.shutdown();
    }
    for handle in [roster_thread, sweeper_thread, enrich_thread, quality_thread]
        .into_iter()
        .flatten()
    {
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
        mic_segments = analysis_stats.mic_segments.load(Ordering::Relaxed),
        mic_enrolled = analysis_stats.mic_enrolled.load(Ordering::Relaxed),
        mic_goldens = analysis_stats.mic_goldens.load(Ordering::Relaxed),
        too_slight = analysis_stats.too_slight.load(Ordering::Relaxed),
        proximity = analysis_stats.proximity_labelled.load(Ordering::Relaxed),
        redecoded = analysis_stats.redecoded.load(Ordering::Relaxed),
        redecoded_de = analysis_stats.redecoded_de.load(Ordering::Relaxed),
        redecoded_en = analysis_stats.redecoded_en.load(Ordering::Relaxed),
        lang_mismatch = analysis_stats.lang_mismatch.load(Ordering::Relaxed),
        // The conversational language prior (0.7.7).
        context_stamped = analysis_stats.context_stamped.load(Ordering::Relaxed),
        flips_suspected = analysis_stats.flips_suspected.load(Ordering::Relaxed),
        repairs = analysis_stats.repairs.load(Ordering::Relaxed),
        dropped_buffers = queue.dropped_chunks(),
        dropped_seconds = queue.dropped_samples() as f32 / SAMPLE_RATE as f32,
        gaps_discarded = stats.gaps_discarded.load(Ordering::Relaxed),
        "stopped"
    );
    result
}

fn cmd_probe(cfg: &Config) -> Result<()> {
    let allowlist = cfg.allowlist();
    let probe = capture::probe(&allowlist)?;

    if probe.apps.is_empty() {
        println!("No application playback streams (Stream/Output/Audio) on the graph.");
    } else {
        println!(
            "{:>5}  {:>8}  {:<24}  {:<28}  {:>8}  CAPTURE",
            "NODE", "SERIAL", "MATCH KEY", "APPLICATION", "PID"
        );
        for (node, decision) in &probe.apps {
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
    }

    // The microphone half. Nothing here opens a stream either — it is the
    // "which device would the mic tap land on" answer, which is the only part
    // of the mic path that can be checked without recording anything.
    println!();
    let pin = cfg.mic.device_override();
    let resolved = probe.mic_target(pin);
    let state = if cfg.mic.enabled {
        cfg.mic.mode.as_str()
    } else {
        "off"
    };
    match (&probe.default_source, resolved) {
        (_, Some(node)) => println!(
            "default source:  {:<24}  {:<28}  node {}, serial {}  [{}]",
            node.name.as_deref().unwrap_or("-"),
            node.label(),
            node.node_id,
            node.serial.as_deref().unwrap_or("-"),
            state,
        ),
        (Some(name), None) => println!(
            "default source:  {name}\n  \
             (the session manager names it, but no Audio/Source on the graph \
             answers to that name)  [{state}]"
        ),
        (None, None) if pin.is_some() => println!(
            "default source:  [mic].device = {}  — not on the graph  [{state}]",
            pin.unwrap_or("?")
        ),
        (None, None) => println!(
            "default source:  none published (the mic tap would let the session \
             manager route it)  [{state}]"
        ),
    }
    if probe.sources.is_empty() {
        println!("capture devices: none (no Audio/Source nodes on the graph)");
    } else {
        println!("capture devices:");
        for node in &probe.sources {
            let here = resolved.is_some_and(|n| n.node_id == node.node_id);
            println!(
                "  {}{:>5}  {:<40}  {}",
                if here { "*" } else { " " },
                node.node_id,
                node.name.as_deref().unwrap_or("-"),
                node.label(),
            );
        }
    }
    Ok(())
}

/// `recalld mic` — the scripting surface for the one source that is not an
/// application. Goes over the socket rather than at the config file: the switch
/// is live, and the GUI has to see it move.
fn cmd_mic(cfg: &Config, data_dir: &Path, action: MicAction) -> Result<()> {
    let out = match action {
        MicAction::Status => call(cfg, data_dir, "mic.get", json!({}))?,
        MicAction::On => call(cfg, data_dir, "mic.set", json!({"enabled": true}))?,
        MicAction::Off => call(cfg, data_dir, "mic.set", json!({"enabled": false}))?,
        MicAction::Follow => call(
            cfg,
            data_dir,
            "mic.set",
            json!({"enabled": true, "mode": "follow"}),
        )?,
        MicAction::Always => call(
            cfg,
            data_dir,
            "mic.set",
            json!({"enabled": true, "mode": "always"}),
        )?,
    };

    let state = out["state"].as_str().unwrap_or("?");
    println!("{:<18}{state}", "microphone");
    println!(
        "{:<18}{}",
        "meaning",
        match state {
            "off" => "not recording, and not listening for a reason to",
            "following:idle" =>
                "on, waiting — it records only while an allowed application is captured",
            "following:active" =>
                "recording the room right now, because an allowed app is captured",
            "always:active" => "recording the room right now, regardless of what is running",
            "always:idle" => "on, but no input device opened yet — check `recalld probe`",
            other => other,
        }
    );
    if let Some(device) = out["device"].as_str() {
        println!("{:<18}{device} (pinned in config.toml)", "device");
    }
    if let Some(you) = out["you_speaker"].as_i64() {
        println!("{:<18}speaker {you}", "your voice");
    }
    if out["persisted"] == json!(false) {
        println!(
            "\nThe change is live but was NOT written to config.toml; it will not survive a restart."
        );
    }
    if state != "off" {
        println!(
            "\nThe microphone hears the ROOM, not the game — anyone near you is recorded,\n\
             whether or not they are in the instance."
        );
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
    let mut models = ModelSet::resolve_at(root, &cfg.models);
    // Report on the set the daemon would actually load, not on the one the
    // config names: those differ exactly when the fallback is carrying the
    // install, which is the case this output exists to make obvious.
    let selection = models.select_asr();

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
    println!(
        "asr set:            {}{}",
        models.asr_dir_name(),
        models
            .asr_export()
            .map(|e| format!("  ({})", e.note))
            .unwrap_or_else(|| "  (not in the catalogue — used as configured)".into())
    );
    println!("asr model id:       {}", models.asr_model_id());
    println!("embedding model id: {}", models.embed_model_id());

    // Semantic search, listed separately because it is OPTIONAL: it is absent
    // on a healthy install, and putting it in the table above would make the
    // normal state look like a broken one.
    let sem = models::SemanticModel::resolve_at(models.root.clone(), &cfg.models);
    println!();
    if sem.present() {
        println!("semantic search:    on   ({})", sem.model_id());
        for e in sem.entries() {
            println!(
                "  {:<14}  {:>10}  {}",
                e.role,
                e.bytes().map(fetch::human).unwrap_or_else(|| "-".into()),
                e.path.display()
            );
        }
    } else {
        println!("semantic search:    off  (optional)");
        println!("  {}", models::SemanticModel::how_to_get_it());
    }

    // The flip arbiter (0.7.7), listed the same way and for the same reason:
    // absent is a normal, correct state — the daemon then flags a suspected
    // flip instead of re-reading it.
    let arbiter = models::ArbiterModel::resolve_at(models.root.clone(), models::ARBITER_DE);
    println!();
    if arbiter.present() {
        println!("german arbiter:     on   ({})", arbiter.model_id());
        for e in arbiter.entries() {
            println!(
                "  {:<14}  {:>10}  {}",
                e.role,
                e.bytes().map(fetch::human).unwrap_or_else(|| "-".into()),
                e.path.display()
            );
        }
    } else {
        println!("german arbiter:     off  (optional)");
        println!("  {}", models::ArbiterModel::how_to_get_it());
    }

    // The transcript cross-check (0.8.0). Same shape, same reason: without it
    // every `asr_confidence` is null, which says "nothing has checked these
    // words" and is a correct, quiet answer.
    let confidence = models::ConfidenceModel::resolve_at(models.root.clone(), 1);
    println!();
    if confidence.present() {
        println!("cross-check:        on   ({})", confidence.model_id("de"));
        for e in confidence.entries() {
            println!(
                "  {:<14}  {:>10}  {}",
                e.role,
                e.bytes().map(fetch::human).unwrap_or_else(|| "-".into()),
                e.path.display()
            );
        }
    } else {
        println!("cross-check:        off  (optional)");
        println!("  {}", models::ConfidenceModel::how_to_get_it());
    }

    if selection == AsrSelection::Fallback {
        // The set is complete and analysis will run — but on the English-only
        // model, which is a 103% WER answer to a German lobby. Say so before
        // the reassuring "All models present".
        println!("\nFALLBACK ACTIVE — the default multilingual ASR set is not installed:");
        for e in models::default_asr_entries(&models.root) {
            println!("  {:<14}  {}", e.role, fetch::describe(e.state(), &e.path));
        }
        println!(
            "Transcription runs on {} instead.\n\
             `recalld models fetch` installs {}\n  ({}).",
            models::FALLBACK_ASR.dir,
            models::DEFAULT_ASR.dir,
            models::DEFAULT_ASR.note,
        );
    }

    // The optional groups, under their own heading and never mixed in with the
    // required table: a machine that has not fetched a 1.9 GB model it never
    // asked for is not an incomplete machine.
    println!();
    println!("OPTIONAL  ({})", Group::Graph.note());
    let graph = GraphModels::resolve(&models.root, &cfg.graph);
    for e in graph.entries() {
        println!(
            "{:<14}  {:<9}  {:>10}  {:>10}  {}",
            e.role,
            match e.state() {
                EntryState::Ok => "ok",
                EntryState::Missing => "not here",
                EntryState::WrongSize { .. } => "BAD SIZE",
            },
            e.bytes().map(fetch::human).unwrap_or_else(|| "-".into()),
            e.expected.map(fetch::human).unwrap_or_else(|| "?".into()),
            e.path.display()
        );
    }
    if graph.present() {
        println!(
            "the memory graph's model is installed ({}); it is {} in config.toml",
            graph.model_id(),
            if cfg.graph.enabled {
                "enabled"
            } else {
                "still off"
            }
        );
    } else {
        println!(
            "not installed, and not required. `recalld models fetch --graph` adds it ({}).",
            fetch::human(
                models::total_download_bytes(&[Group::Graph]) - models::total_download_bytes(&[])
            )
        );
    }

    if models.complete() && selection == AsrSelection::Fallback {
        println!("\nAnalysis runs — on the fallback ASR. The default set is not here.");
    } else if models.complete() {
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
    opts: &fetch::FetchOptions,
    no_config: bool,
) -> Result<()> {
    let root = fetch::target_dir(dir, &cfg.models, data_dir);
    println!("models dir: {}", root.display());
    println!(
        "up to {} to download\n",
        fetch::human(models::total_download_bytes(&opts.extra_groups()))
    );

    let report = fetch::fetch_models(&root, &cfg.models, opts)?;

    println!(
        "\n{} downloaded, {} already present ({} transferred{}).",
        report.downloaded,
        report.skipped,
        fetch::human(report.bytes),
        if report.connections > 1 {
            format!(", up to {} connections at once", report.connections)
        } else {
            String::new()
        }
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
    println!(
        "{:>4}  {:<24}  {:>8}  {:<9}  SPEECH",
        "ID", "NAME", "SEGMENTS", "SPEAKS"
    );
    for r in rows {
        println!(
            "{:>4}  {:<24}  {:>8}  {:<9}  {}",
            r.id,
            r.display_name,
            r.segments,
            r.languages
                .as_ref()
                .map(|l| l.join("+"))
                .unwrap_or_else(|| "any".into()),
            format_duration(r.speech_ns)
        );
    }
    Ok(())
}

/// `recalld languages <id> <codes|any>` — over the socket, because a language
/// declaration changes what the pipeline does with the *next* segment and every
/// open client shows the setting.
fn cmd_languages(cfg: &Config, data_dir: &Path, speaker_id: i64, codes: &str) -> Result<()> {
    let list: Vec<String> = codes
        .split(',')
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .collect();
    let out = call(
        cfg,
        data_dir,
        "speakers.set_languages",
        json!({"id": speaker_id, "languages": list}),
    )?;
    match out["languages"].as_array() {
        Some(langs) if !langs.is_empty() => {
            let names: Vec<&str> = langs.iter().filter_map(|v| v.as_str()).collect();
            let spoken: Vec<&str> = names
                .iter()
                .map(|c| match *c {
                    "de" => "German",
                    "en" => "English",
                    other => other,
                })
                .collect();
            println!("Speaker {speaker_id} speaks {}.", spoken.join(" and "));
            if names.len() == 1 && names[0] == "en" {
                println!(
                    "A transcript from this voice that reads as German will be decoded again\n\
                     with the English-only model, which cannot produce German at all."
                );
            } else if names.len() == 1 {
                println!(
                    "A transcript from this voice that reads as English will be decoded again\n\
                     with the German arbiter — Whisper, with its language token forced to\n\
                     German. Without it installed (`recalld models fetch --arbiter-de`) the\n\
                     words are kept as they are and the row is flagged instead."
                );
            } else {
                println!("Two languages: nothing is corrected — either one is expected.");
            }
        }
        _ => println!("Speaker {speaker_id} speaks any language; nothing will be corrected."),
    }
    Ok(())
}

/// `recalld lang` — what the language prior has flagged, and what could settle
/// it. Reads the database directly: it says nothing about a running daemon and
/// has to work on a machine where none is running.
fn cmd_lang_status(cfg: &Config, data_dir: &Path, dir: Option<&Path>) -> Result<()> {
    let store = Store::open(data_dir)?;
    let (flagged, with_audio) = store.language_mismatch_counts()?;
    let root = fetch::target_dir(dir, &cfg.models, data_dir);
    let models = ModelSet::resolve_at(root, &cfg.models);
    let arbiters = recalld::arbiter::Arbiters::new(&models);
    let installed = arbiters.installed();

    println!(
        "{:<20}{} of the last {} clear turn(s) in a conversation, {:.0}% agreeing",
        "context",
        cfg.lang.context_min_clear,
        cfg.lang.context_window,
        cfg.lang.context_min_agree * 100.0
    );
    println!(
        "{:<20}{:.1} s and {} word(s) minimum to replace a transcript",
        "replacement bar", cfg.lang.arbiter_min_duration_s, cfg.lang.arbiter_min_words
    );
    println!(
        "{:<20}{}",
        "arbiters",
        if installed.is_empty() {
            "none installed — a suspected flip can only be flagged".to_string()
        } else {
            installed.join(", ")
        }
    );
    if !installed.contains(&"de") {
        println!("  {}", models::ArbiterModel::how_to_get_it());
    }
    if !installed.contains(&"en") {
        println!(
            "  the English arbiter is not installed. \
             `recalld models fetch --fallback-asr` installs {} ({}).",
            models::FALLBACK_ASR.dir,
            models::FALLBACK_ASR.note
        );
    }
    println!("{:<20}{flagged}", "flagged");
    if flagged == 0 {
        println!("nothing to repair.");
        return Ok(());
    }
    println!(
        "{:<20}{with_audio} still have their audio; {} do not and can never be settled",
        "  repairable",
        flagged - with_audio
    );
    if !installed.is_empty() {
        println!("`recalld lang repair` re-reads them.");
    }
    Ok(())
}

/// `recalld lang repair` — the retroactive half of 0.7.7.
fn cmd_lang_repair(
    cfg: &Config,
    data_dir: &Path,
    dir: Option<&Path>,
    batch: usize,
    limit: Option<usize>,
) -> Result<()> {
    let root = fetch::target_dir(dir, &cfg.models, data_dir);
    let models = ModelSet::resolve_at(root, &cfg.models);
    let mut arbiters = recalld::arbiter::Arbiters::new(&models);
    if arbiters.installed().is_empty() {
        println!("no arbiter is installed, so nothing can be re-read.");
        println!("  {}", models::ArbiterModel::how_to_get_it());
        return Ok(());
    }

    // Idle priority, no CPU pinning — the same rule the semantic backfill
    // follows: a batch job competing with a live capture never wins a timeslice
    // from a frame.
    pipeline::deprioritise_current_thread(19, &[]);

    let store = Store::open(data_dir)?;
    let (before, with_audio) = store.language_mismatch_counts()?;
    println!("{before} flagged transcript(s), {with_audio} with audio to re-read");
    if with_audio == 0 {
        return Ok(());
    }
    let started = std::time::Instant::now();
    let report = recalld::langctx::repair(
        &store,
        data_dir,
        &mut arbiters,
        &cfg.lang,
        batch,
        limit,
        |r| {
            eprint!(
                "\r  {} scanned, {} repaired, {} settled\x1b[K",
                r.scanned, r.repaired, r.settled
            );
        },
    )?;
    eprintln!();

    let (after, _) = store.language_mismatch_counts()?;
    println!(
        "scanned {} in {:.1}s: {} repaired ({} de, {} en), {} settled",
        report.scanned,
        started.elapsed().as_secs_f64(),
        report.repaired,
        report.repaired_de,
        report.repaired_en,
        report.settled,
    );
    // Everything that stayed flagged, and why — the reasons are actionable and
    // a bare "12 left" is not.
    for (n, line) in [
        (report.kept, "the arbiter's own answer failed a guard"),
        (
            report.too_short,
            "under the replacement bar; flagged, never replaced",
        ),
        (
            report.unavailable,
            "no arbiter installed for that language — fetch it and run again",
        ),
        (
            report.undecidable,
            "nothing says which language to expect: no declaration, no conversation context",
        ),
        (report.no_audio, "the audio is gone"),
    ] {
        if n > 0 {
            println!("  {n:>6}  {line}");
        }
    }
    println!(
        "{after} still flagged{}",
        if report.scanned > 0 && after > 0 && limit.is_some() {
            " — run it again to continue"
        } else {
            ""
        }
    );
    Ok(())
}

/// `recalld speakers prune [--apply]` — the one-off voice sweep.
fn cmd_prune(cfg: &Config, data_dir: &Path, apply: bool) -> Result<()> {
    let out = call(cfg, data_dir, "speakers.prune", json!({"apply": apply}))?;
    let voices = out["voices"].as_array().cloned().unwrap_or_default();
    if voices.is_empty() {
        println!("No one-off voices: every voice has more than a moment of speech.");
        return Ok(());
    }
    println!("{:>4}  {:<24}  {:>8}  SPEECH", "ID", "NAME", "SEGMENTS");
    for v in &voices {
        println!(
            "{:>4}  {:<24}  {:>8}  {}",
            v["id"].as_i64().unwrap_or(0),
            v["name"]
                .as_str()
                .unwrap_or_else(|| v["auto"].as_str().unwrap_or("?")),
            v["segments"].as_i64().unwrap_or(0),
            format_duration(
                v["speech_ns"]
                    .as_str()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0)
            )
        );
    }
    if apply {
        println!(
            "\nSwept {} voice(s); {} segment(s) deleted (undoable until the retention\n\
             window closes). Your own voice and every named voice were left alone.",
            out["count"].as_i64().unwrap_or(0),
            out["segments"].as_i64().unwrap_or(0),
        );
    } else {
        println!(
            "\n{} voice(s) with at most {} segment and under {} ms of speech.\n\
             Nothing was changed — `recalld speakers prune --apply` deletes them.",
            out["count"].as_i64().unwrap_or(0),
            out["max_segments"].as_i64().unwrap_or(1),
            out["max_speech_ms"].as_i64().unwrap_or(3000),
        );
    }
    Ok(())
}

/// `recalld speakers delete <id> [--keep-voiceprint]` — DESIGN §8's choice.
///
/// Over the socket rather than straight at the database: a voice leaving the
/// bank changes what the *next* segment matches, and every open client has to
/// drop the row. Only the running daemon can say both.
fn cmd_delete_speaker(
    cfg: &Config,
    data_dir: &Path,
    speaker_id: i64,
    keep_voiceprint: bool,
) -> Result<()> {
    let out = call(
        cfg,
        data_dir,
        "speakers.delete",
        json!({"id": speaker_id, "keep_voiceprint": keep_voiceprint}),
    )?;
    let who = out["name"]
        .as_str()
        .or_else(|| out["auto"].as_str())
        .map(str::to_string)
        .unwrap_or_else(|| format!("Speaker {speaker_id}"));
    let segments = out["segments"].as_i64().unwrap_or(0);
    println!(
        "{who}: {segments} conversation(s) deleted (undoable until the retention window \
         closes)."
    );
    if out["removed_speaker"].as_bool().unwrap_or(false) {
        println!(
            "  {:>4} prototype(s), {} embedding(s) and {} golden sample(s) removed with the \
             voice itself.",
            out["prototypes"].as_i64().unwrap_or(0),
            out["embeddings"].as_i64().unwrap_or(0),
            out["goldens"].as_i64().unwrap_or(0),
        );
        println!("  This voice is out of the bank: it has to enrol again to be recognised.");
    } else {
        println!("  The voiceprint was kept — this voice is still labelled going forward.");
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

fn cmd_search(cfg: &Config, data_dir: &Path, query: &str, limit: usize, smart: bool) -> Result<()> {
    if query.trim().is_empty() {
        anyhow::bail!("nothing to search for");
    }
    let store = Store::open(data_dir)?;
    if smart {
        return cmd_search_smart(cfg, data_dir, &store, query, limit);
    }
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

/// `recalld search --smart`: the same fusion the socket serves, in one process.
///
/// Deliberately not a socket call. This is the surface you reach for while
/// debugging the index — including on a machine where the daemon is not
/// running — so it opens the database itself.
fn cmd_search_smart(
    cfg: &Config,
    data_dir: &Path,
    store: &Store,
    query: &str,
    limit: usize,
) -> Result<()> {
    use recalld::semantic;

    let root = fetch::target_dir(None, &cfg.models, data_dir);
    let sem = models::SemanticModel::resolve_at(root, &cfg.models);
    if !sem.present() {
        anyhow::bail!("{}", models::SemanticModel::how_to_get_it());
    }
    let leg = semantic::SemanticLeg::new(semantic::TextEmbedder::load(&sem)?);

    let filter = recalld::store::SegmentFilter::default();
    let within = semantic::candidates(store, &filter)?;
    let started = std::time::Instant::now();
    let vector = leg.search(store, query, limit, &within)?;
    let elapsed = started.elapsed();

    let keyword: Vec<i64> = store
        .search(query, limit)
        .unwrap_or_default()
        .iter()
        .map(|h| h.segment_id())
        .collect();
    let semantic_ids: Vec<i64> = vector.iter().map(|s| s.segment_id).collect();
    let fused = semantic::fuse(&keyword, &semantic_ids, semantic::RRF_K);

    if fused.is_empty() {
        println!("No matches for {query:?}.");
        return Ok(());
    }
    let by_score: std::collections::HashMap<i64, f32> =
        vector.iter().map(|s| (s.segment_id, s.score)).collect();
    for f in fused.iter().take(limit) {
        let Some(row) = store.segment_row(f.segment_id)? else {
            continue;
        };
        println!(
            "{}  {:<9}{:>6}  {:<20}  {}",
            format_time(row.t_start_ns),
            f.via.as_str(),
            by_score
                .get(&f.segment_id)
                .map(|s| format!("{s:.3}"))
                .unwrap_or_else(|| "-".into()),
            row.speaker_name.as_deref().unwrap_or("-"),
            row.text.as_deref().unwrap_or("")
        );
    }
    eprintln!(
        "\n{} hit(s); the vector leg searched in {:.1} ms",
        fused.len().min(limit),
        elapsed.as_secs_f64() * 1000.0
    );
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
    let out = call(
        cfg,
        data_dir,
        if pause { "pause" } else { "resume" },
        json!({}),
    )?;
    let paused = out["paused"].as_bool().unwrap_or(pause);
    if out["changed"].as_bool() == Some(false) {
        println!("Already {}.", if paused { "paused" } else { "running" });
    } else if paused {
        println!("Paused. Capture keeps running; nothing is written until `recalld resume`.");
    } else {
        println!("Resumed.");
    }
    Ok(())
}

/// `recalld graph` — the memory graph's scripting surface (docs/GRAPH.md).
///
/// Over the socket, like the microphone and for the same reasons: the Tier 3
/// switch is live, and every connected client has to be told when it moves.
fn cmd_graph(cfg: &Config, data_dir: &Path, action: GraphAction) -> Result<()> {
    match action {
        GraphAction::Commitments => return cmd_graph_commitments(cfg, data_dir),
        GraphAction::Topics => return cmd_graph_topics(cfg, data_dir),
        _ => {}
    }
    let out = match action {
        GraphAction::On => call(cfg, data_dir, "graph.enrich", json!({"action": "start"}))?,
        GraphAction::Off => call(cfg, data_dir, "graph.enrich", json!({"action": "stop"}))?,
        _ => call(cfg, data_dir, "graph.summary", json!({}))?,
    };
    let config = &out["config"];
    let state = &out["enrichment"];
    let counts = &out["counts"];

    println!(
        "{:<20}{}",
        "local model",
        if config["enabled"].as_bool() == Some(true) {
            "on — runs only while nothing is being captured"
        } else {
            "off (the default)"
        }
    );
    println!(
        "{:<20}{}",
        "model on disk",
        if config["installed"].as_bool() == Some(true) {
            "yes"
        } else {
            "no — `recalld models fetch --graph` installs it"
        }
    );
    let phase = state["phase"].as_str().unwrap_or("off");
    println!(
        "{:<20}{phase}{}",
        "worker",
        match state["reason"].as_str() {
            Some(reason) => format!(" — {reason}"),
            None => String::new(),
        }
    );
    if phase == "running" {
        println!(
            "{:<20}{} of {} conversation(s) in this batch",
            "progress",
            state["batch_done"].as_i64().unwrap_or(0),
            state["batch_total"].as_i64().unwrap_or(0),
        );
    }
    if counts.is_object() {
        let n = |k: &str| counts[k].as_i64().unwrap_or(0);
        println!(
            "{:<20}{} open ({} candidate, {} confirmed), {} done, {} dismissed",
            "commitments",
            n("open"),
            n("candidates"),
            n("confirmed"),
            n("done"),
            n("dismissed"),
        );
        println!(
            "{:<20}{} from rules, {} from the model",
            "  sourced",
            n("from_rules"),
            n("from_llm")
        );
        println!("{:<20}{}", "time references", n("time_refs"));
        println!(
            "{:<20}{} label(s) over {} of {} conversation(s), {} not looked at yet",
            "topics",
            n("topics"),
            n("threads_enriched"),
            n("threads"),
            n("threads_pending"),
        );
    }
    if let Some(err) = state["last_error"].as_str() {
        println!("{:<20}{err}", "last error");
    }
    println!("\nNothing here leaves the machine, and nothing acts on a guess.");
    Ok(())
}

fn cmd_graph_commitments(cfg: &Config, data_dir: &Path) -> Result<()> {
    let out = call(cfg, data_dir, "commitments.list", json!({}))?;
    let rows = out["commitments"].as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        println!(
            "No commitments. Nothing has been noticed, which is not the same as nothing having been promised."
        );
        return Ok(());
    }
    println!(
        "{:<4}  {:<10}  {:<7}  {:<18}  {:<18}  WHAT",
        "ID", "STATE", "SOURCE", "WHO", "DUE"
    );
    for c in &rows {
        let who = c["who"]["name"]
            .as_str()
            .or_else(|| c["who"]["auto"].as_str())
            .unwrap_or("?");
        let due = c["due_ms"]
            .as_i64()
            .map(|ms| format_time(ms * 1_000_000))
            .unwrap_or_else(|| c["due_raw"].as_str().unwrap_or("—").to_string());
        println!(
            "{:<4}  {:<10}  {:<7}  {:<18}  {:<18}  {}",
            c["id"].as_i64().unwrap_or(0),
            c["state"].as_str().unwrap_or("?"),
            c["source"].as_str().unwrap_or("?"),
            who,
            due,
            c["what"].as_str().unwrap_or(""),
        );
    }
    println!(
        "\n`source: rules` is a pattern match and a guess; `llm` is the local model.\nNothing acts on either — they are suggestions until you say otherwise."
    );
    Ok(())
}

fn cmd_graph_topics(cfg: &Config, data_dir: &Path) -> Result<()> {
    let out = call(cfg, data_dir, "topics.list", json!({}))?;
    let rows = out["topics"].as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        println!(
            "No topics yet. They are written by the local model, which is off by default —\n\
             `recalld graph on` turns it on."
        );
        return Ok(());
    }
    println!(
        "{:>6}  {:>8}  {:<19}  TOPIC",
        "THREADS", "SEGMENTS", "LAST HEARD"
    );
    for t in &rows {
        println!(
            "{:>6}  {:>8}  {:<19}  {}",
            t["threads"].as_i64().unwrap_or(0),
            t["segments"].as_i64().unwrap_or(0),
            t["last_ms"]
                .as_i64()
                .map(|ms| format_time(ms * 1_000_000))
                .unwrap_or_else(|| "—".into()),
            t["topic"].as_str().unwrap_or(""),
        );
    }
    Ok(())
}

fn cmd_status(cfg: &Config, data_dir: &Path) -> Result<()> {
    let s = call(cfg, data_dir, "status", json!({}))?;
    println!("{:<18}{}", "daemon", s["daemon"].as_str().unwrap_or("?"));
    println!(
        "{:<18}{}",
        "uptime",
        format_duration(s["uptime_s"].as_i64().unwrap_or(0) * 1_000_000_000)
    );
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
    println!(
        "{:<18}{}",
        "microphone",
        s["mic_state"].as_str().unwrap_or("off")
    );
    let models = s["models"]
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
    // The language prior (0.7.7), on its own line because it is four numbers
    // about one thing and folding them into `counters` would bury the only one
    // that asks for an action — `flagged`, which `recalld lang repair` clears.
    let n = |key: &str| s["counters"][key].as_i64().unwrap_or(0);
    println!(
        "{:<18}{} stamped from context, {} suspected flip(s), {} re-decoded ({} de, {} en), \
         {} flagged",
        "language",
        n("context_stamped"),
        n("flips_suspected"),
        n("redecoded"),
        n("redecoded_de"),
        n("redecoded_en"),
        n("lang_mismatch"),
    );
    println!(
        "{:<18}{} connected, seq {}",
        "clients",
        s["clients"].as_i64().unwrap_or(0),
        s["seq"].as_i64().unwrap_or(0)
    );
    println!(
        "{:<18}{}",
        "roster",
        s["roster_present"].as_i64().unwrap_or(0)
    );
    print_storage(&s["storage"]);
    Ok(())
}

/// The disk breakdown, in the four parts that behave differently: audio is
/// capped by `[retention].audio_days` and self-limiting, the database grows
/// forever and is the memory, goldens are retention-exempt on purpose, and the
/// models are a fixed one-off. A single total would hide all four.
fn print_storage(storage: &Value) {
    let Some(block) = storage.as_object() else {
        println!(
            "{:<18}not measured yet (the retention sweeper measures it once per pass)",
            "storage"
        );
        return;
    };
    let n = |key: &str| block.get(key).and_then(Value::as_u64).unwrap_or(0);
    println!("{:<18}{} total", "storage", fetch::human(n("total_bytes")));
    println!(
        "{:<18}{:>10}  transcripts, voices and the search index",
        "  database",
        fetch::human(n("db_bytes"))
    );
    println!(
        "{:<18}{:>10}  {} file(s), capped by the audio retention window",
        "  audio",
        fetch::human(n("audio_bytes")),
        n("audio_files"),
    );
    println!(
        "{:<18}{:>10}  kept clips, deliberately exempt from retention",
        "  goldens",
        fetch::human(n("goldens_bytes"))
    );
    println!(
        "{:<18}{:>10}  fixed; `recalld models status` lists them",
        "  models",
        fetch::human(n("models_bytes"))
    );
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
    #[test]
    fn exe_replaced_detects_the_deleted_suffix() {
        use std::ffi::OsStr;
        assert!(super::exe_replaced(OsStr::new(
            "/home/u/.local/lib/nx-recall/recalld (deleted)"
        )));
        assert!(!super::exe_replaced(OsStr::new(
            "/home/u/.local/lib/nx-recall/recalld"
        )));
        // a path that merely CONTAINS the marker mid-string is not a match
        assert!(!super::exe_replaced(OsStr::new("/tmp/x (deleted)/recalld")));
    }

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
