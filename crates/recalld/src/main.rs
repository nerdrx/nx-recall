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
use recalld::assist::{self, AssistStop};
use recalld::bus::Bus;
use recalld::capture;
use recalld::client;
use recalld::clock::utc_now_ns;
use recalld::config::{self, Config, SAMPLE_RATE};
use recalld::control::Control;
use recalld::enrich::{self, EnrichStop};
use recalld::export;
use recalld::fetch;
use recalld::models::{self, AsrSelection, EntryState, GraphModels, Group, ModelSet};
use recalld::night::{self, NightStop};
use recalld::pipeline::{self, Pipeline, Stats};
use recalld::quality::{self, QualityStop};
use recalld::queue::EventQueue;
use recalld::reminders::{self, ReminderStop};
use recalld::retention::{self, SweeperStop};
use recalld::roster::{self, RosterStop};
use recalld::server;
use recalld::service::{Service, TruthWiring};
use recalld::store::Store;
use recalld::truth::{self, TruthStats, TruthStop};
use recalld::truthnet;

use crate::cli::{
    Cli, Command, GraphAction, IdentityAction, LangAction, MicAction, ModelsAction, NightBackend,
    NotesAction, SemanticAction, SpeakersAction, TruthAction,
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
        // ---- 0.10.0 -------------------------------------------------------
        Command::Room { action, device } => cmd_room(&cfg, &data_dir, action, device.as_deref()),
        Command::Devices => cmd_devices(),
        Command::Export {
            dir,
            from,
            to,
            speaker,
            thread,
            translations,
            dry_run,
        } => cmd_export(
            &data_dir,
            &dir,
            from.as_deref(),
            to.as_deref(),
            speaker,
            thread,
            translations,
            dry_run,
        ),
        // ---- end 0.10.0 ---------------------------------------------------
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
                night,
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
                    night,
                    single_stream: false,
                },
                no_config,
            ),
            ModelsAction::BuildNight {
                dir,
                backend,
                force,
                jobs,
            } => cmd_build_night(&cfg, &data_dir, dir.as_deref(), backend, force, jobs),
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
        // ---- 0.11.0, source-aware identity -----------------------------
        Command::Identity { action } => cmd_identity(&cfg, &data_dir, action),
        // ---- end 0.11.0 -------------------------------------------------
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
        // ---- 0.8.0, the product round ---------------------------------
        Command::Ask { question, limit } => cmd_ask(&cfg, &data_dir, &question.join(" "), limit),
        Command::Notes { action } => cmd_notes(&cfg, &data_dir, action),
        Command::Brief { speaker_id } => cmd_brief(&cfg, &data_dir, speaker_id),
        Command::Accuracy => cmd_accuracy(&cfg, &data_dir),
        // ---- end 0.8.0 -------------------------------------------------
        // ---- 0.9.0, ground truth from Discord --------------------------
        Command::Truth { action } => cmd_truth(&cfg, &data_dir, &config_path, action),
        // ---- end 0.9.0 -------------------------------------------------
        // ---- 0.9.0, the assistant ---------------------------------------
        Command::Digest { day } => cmd_digest(&cfg, &data_dir, day.as_deref()),
        // ---- 0.10.0, worlds and turn-taking ---------------------------
        Command::Stats { speaker_id, days } => cmd_stats(&cfg, &data_dir, speaker_id, days),
        Command::Worlds { limit } => cmd_worlds(&cfg, &data_dir, limit),
        // ---- end 0.10.0 -----------------------------------------------
        // ---- end 0.9.0 ---------------------------------------------------
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
    // 0.10.0: the second microphone's switch, live like the first.
    .with_room(cfg.room.clone())
    // The memory graph's Tier 3 switch is live, like the microphone's.
    .with_graph(cfg.graph.clone(), models_root.clone())
    // …and the accuracy round's idle worker reads its switches the same way.
    .with_asr(cfg.asr.clone())
    .with_night(cfg.night.clone())
    // 0.9.0: reminders, digests and translation, for the same reason again.
    .with_assist(cfg.assist.clone());
    // The three translation settings live in one place rather than being
    // threaded through `segment_json`'s dozen call sites; see `translate::LIVE`.
    // `assist.set` writes the same three, which is what makes them live.
    recalld::translate::adopt(&cfg.assist);
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

    // ---- 0.9.0: ground truth from Discord ---------------------------------
    //
    // Two pieces, deliberately independent. The listener only runs when it has
    // been asked for; the worker runs always, because truth already collected
    // is still worth labelling against after the plugin has been switched off.
    let truth_stats = Arc::new(TruthStats::default());
    let truth_stop = Arc::new(TruthStop::default());
    let truth_thread = {
        let store = Arc::clone(&store);
        let control = Arc::clone(&control);
        let bus = Arc::clone(&bus);
        let truth_cfg = cfg.truth.clone();
        let identity = cfg.identity.clone();
        let runtime = cfg.runtime.clone();
        let stats = Arc::clone(&truth_stats);
        let stop = Arc::clone(&truth_stop);
        std::thread::Builder::new()
            .name("recalld-truth".into())
            .spawn(move || {
                truth::run(
                    store, control, bus, truth_cfg, identity, runtime, stats, stop,
                )
            })
            .map_err(|e| warn!("no ground-truth worker: {e}"))
            .ok()
    };
    let truth_token_path = config::truth_token_path(config_path);
    let truth_ingest = if cfg.truth.enabled {
        // A token that cannot be written is a listener that cannot be reached,
        // so a failure here stops the ingest and not the daemon: capture is
        // the product and a measurement never costs a recording.
        match truthnet::token(&truth_token_path).and_then(|token| {
            truthnet::serve(
                Arc::clone(&store),
                Arc::clone(&truth_stats),
                token,
                cfg.truth.port,
            )
        }) {
            Ok(ingest) => Some(ingest),
            Err(e) => {
                warn!("the truth ingest could not start: {e:#}");
                None
            }
        }
    } else {
        None
    };
    service.attach_truth(Arc::new(TruthWiring {
        cfg: cfg.truth.clone(),
        stats: Arc::clone(&truth_stats),
        listening: truth_ingest.as_ref().map(|i| i.addr()),
        token_path: truth_token_path,
    }));
    // ---- end 0.9.0 --------------------------------------------------------
    // The night shift (0.9.0). Started like the two workers above it — the
    // switch is live, and every gate it has is re-read on its own loop.
    let night_stop = Arc::new(NightStop::default());
    let night_thread = {
        let store = Arc::clone(&store);
        let control = Arc::clone(&control);
        let bus = Arc::clone(&bus);
        let root = models_root.clone();
        let dir = data_dir.to_path_buf();
        let stats = Arc::clone(&control.night_stats);
        let stop = Arc::clone(&night_stop);
        let runtime = cfg.runtime.clone();
        std::thread::Builder::new()
            .name("recalld-night".into())
            .spawn(move || night::run(store, control, bus, root, dir, runtime, stats, stop))
            .map_err(|e| warn!("no night shift: {e}"))
            .ok()
    };
    // ---- 0.9.0, the assistant ------------------------------------------
    // Two threads. The scheduler is a query every thirty seconds and no model
    // at all, so it runs whatever else is switched off; the digest and
    // translation passes share one worker because they share one model.
    let reminder_stop = Arc::new(ReminderStop::default());
    let reminder_thread = {
        let store = Arc::clone(&store);
        let bus = Arc::clone(&bus);
        let assist_cfg = cfg.assist.clone();
        let stats = Arc::clone(&control.assist_stats);
        let stop = Arc::clone(&reminder_stop);
        std::thread::Builder::new()
            .name("recalld-reminders".into())
            .spawn(move || reminders::run(store, bus, assist_cfg, stats, stop))
            .map_err(|e| warn!("no reminder scheduler: {e}"))
            .ok()
    };
    let assist_stop = Arc::new(AssistStop::default());
    let assist_thread = {
        let store = Arc::clone(&store);
        let control2 = Arc::clone(&control);
        let bus = Arc::clone(&bus);
        let root = models_root.clone();
        let runtime = cfg.runtime.clone();
        let assist_cfg = cfg.assist.clone();
        let stats = Arc::clone(&control.assist_stats);
        let stop = Arc::clone(&assist_stop);
        std::thread::Builder::new()
            .name("recalld-assist".into())
            .spawn(move || {
                assist::run(store, control2, bus, root, runtime, assist_cfg, stats, stop)
            })
            .map_err(|e| warn!("no assistant worker: {e}"))
            .ok()
    };
    if !cfg.assist.translate_to.trim().is_empty() {
        info!(
            to = %cfg.assist.translate_to,
            "turns in another language will be translated by the local model"
        );
    }
    // ---- end 0.9.0 -------------------------------------------------------

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
    truth_stop.stop();
    night_stop.stop();
    reminder_stop.stop();
    assist_stop.stop();
    if let Some(s) = socket {
        s.shutdown();
    }
    if let Some(i) = truth_ingest {
        i.shutdown();
    }
    for handle in [
        roster_thread,
        sweeper_thread,
        enrich_thread,
        quality_thread,
        truth_thread,
        night_thread,
        // 0.9.0.
        reminder_thread,
        assist_thread,
    ]
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

// ---- 0.10.0, the room microphone and the export ---------------------------

fn cmd_room(cfg: &Config, data_dir: &Path, action: MicAction, device: Option<&str>) -> Result<()> {
    // A device change is its own request, applied first, so `recalld room
    // --device x on` reads left to right and cannot be refused for having no
    // device while naming one.
    if let Some(device) = device {
        let out = call(cfg, data_dir, "room.set", json!({"device": device}))?;
        println!(
            "{:<18}{}",
            "device",
            out["device"].as_str().unwrap_or("(cleared)")
        );
    }
    let out = match action {
        MicAction::Status => call(cfg, data_dir, "room.get", json!({}))?,
        MicAction::On => call(cfg, data_dir, "room.set", json!({"enabled": true}))?,
        MicAction::Off => call(cfg, data_dir, "room.set", json!({"enabled": false}))?,
        MicAction::Follow => call(
            cfg,
            data_dir,
            "room.set",
            json!({"enabled": true, "mode": "follow"}),
        )?,
        MicAction::Always => call(
            cfg,
            data_dir,
            "room.set",
            json!({"enabled": true, "mode": "always"}),
        )?,
    };

    let state = out["state"].as_str().unwrap_or("?");
    println!("{:<18}{state}", "room microphone");
    println!(
        "{:<18}{}",
        "meaning",
        match state {
            "off" => "not recording, and not listening for a reason to",
            "needs-device" =>
                "on, but no device is pinned — run `recalld devices` and pass --device",
            "following:idle" =>
                "on, waiting — it records only while an allowed application is captured",
            "following:active" =>
                "recording the room right now, because an allowed app is captured",
            "always:active" => "recording the room right now, regardless of what is running",
            "always:idle" => "on, but the pinned device is not on the graph",
            other => other,
        }
    );
    if let Some(device) = out["device"].as_str() {
        println!("{:<18}{device}", "device");
    }
    if out["persisted"] == json!(false) {
        println!(
            "\nThe change is live but was NOT written to config.toml; it will not survive a restart."
        );
    }
    if state != "off" {
        println!(
            "\nThis microphone hears everyone in the ROOM. Their voices are matched and\n\
             enrolled like anybody else's — they are named in the voicebank, not marked\n\
             as you."
        );
    }
    Ok(())
}

fn cmd_devices() -> Result<()> {
    let rows = capture::list_devices()?;
    if rows.is_empty() {
        println!("No capture devices on the PipeWire graph.");
        return Ok(());
    }
    println!("{:<44}{:<34}", "NODE.NAME", "DESCRIPTION");
    for row in &rows {
        println!(
            "{:<44}{:<34}{}",
            row.node_name,
            row.description.as_deref().unwrap_or(""),
            if row.is_default { "system default" } else { "" }
        );
    }
    println!(
        "\nPass one of these to `recalld room --device`. The system default is almost\n\
         always your headset — the microphone switch is already on that one."
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_export(
    data_dir: &Path,
    dir: &Path,
    from: Option<&str>,
    to: Option<&str>,
    speaker: Option<i64>,
    thread: Option<i64>,
    translations: bool,
    dry_run: bool,
) -> Result<()> {
    let when = |what: Option<&str>, label: &str| -> Result<Option<i64>> {
        match what {
            None => Ok(None),
            Some(text) => recalld::clock::parse_iso8601(text)
                .map(Some)
                .ok_or_else(|| anyhow::anyhow!("--{label} {text:?} is not an ISO-8601 date")),
        }
    };
    let req = export::ExportRequest {
        dir: dir.to_path_buf(),
        from: when(from, "from")?,
        to: when(to, "to")?,
        speaker,
        thread,
        include_translations: translations,
    };

    let store = Store::open(data_dir)?;
    let plan = match export::plan(&store, &req) {
        Ok(p) => p,
        // A refusal is a decision with a reason, not a crash: print it as the
        // sentence it is.
        Err(e) => anyhow::bail!("{e}"),
    };
    if plan.files.is_empty() {
        println!("Nothing to export in that range.");
        return Ok(());
    }

    println!(
        "{} day{}, {} conversation{}, {} turn{} — {} bytes over {} file{}:",
        plan.days(),
        if plan.days() == 1 { "" } else { "s" },
        plan.conversations(),
        if plan.conversations() == 1 { "" } else { "s" },
        plan.turns(),
        if plan.turns() == 1 { "" } else { "s" },
        plan.bytes(),
        plan.files.len(),
        if plan.files.len() == 1 { "" } else { "s" },
    );
    for file in &plan.files {
        println!(
            "  {:<16}{:>9} bytes{}",
            file.name,
            file.bytes(),
            match (file.exists, file.blocked) {
                (_, true) => "  REFUSED: not written by NX Recall",
                (true, false) => "  (rewrites an earlier export)",
                (false, false) => "",
            }
        );
    }
    if dry_run {
        println!("\n--dry-run: nothing was written.");
        return Ok(());
    }

    let bytes = match export::write(&plan, dir, |done, total| {
        println!("  wrote {done}/{total}");
    }) {
        Ok(b) => b,
        Err(e) => anyhow::bail!("{e}"),
    };
    println!(
        "\nWrote {bytes} bytes to {}.\nThese are files on your disk and nothing else — nothing \
         was sent anywhere.",
        dir.display()
    );
    Ok(())
}

// ---- end 0.10.0 -----------------------------------------------------------

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

    // The night shift (0.9.0). Reported in two halves because they fail
    // differently: the model is a byte-verified download, and the runtime is
    // compiled on this machine — so "not built" is a normal state with a
    // different remedy from "not downloaded".
    let night = models::NightModels::resolve_at(models.root.clone(), &cfg.night);
    println!();
    if night.present() {
        println!("night shift:        on   ({})", night.model_id());
        println!("  {:<14}  {}", "night.runtime", night.cli.display());
    } else {
        println!(
            "night shift:        off  (optional — model {}, runtime {})",
            if night.model_present() {
                "here"
            } else {
                "missing"
            },
            if night.runtime_present() {
                "built"
            } else {
                "not built"
            }
        );
        println!("  {}", models::NightModels::how_to_get_it());
    }
    for e in night.entries() {
        println!(
            "  {:<14}  {:>10}  {}",
            e.role,
            e.bytes().map(fetch::human).unwrap_or_else(|| "-".into()),
            e.path.display()
        );
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

/// Build the night shift's decoder (0.9.0).
///
/// **This one compiles, and it says so.** Every other asset in this program is
/// a byte-verified download; whisper.cpp publishes no release binary with a GPU
/// backend for an AMD card, so a `whisper-cli` that can use the 7900 XTX in
/// this machine has to be built on it. The recipe is pinned to a tag for the
/// same reason a download is pinned to a byte count.
///
/// Everything happens under the models directory and nothing is installed
/// system-wide. The build runs at nice 19 on half the machine's cores, because
/// a build that takes the whole box is a build nobody starts twice.
fn cmd_build_night(
    cfg: &Config,
    data_dir: &Path,
    dir: Option<&Path>,
    backend: NightBackend,
    force: bool,
    jobs: Option<usize>,
) -> Result<()> {
    use std::process::Command as Proc;

    let root = fetch::target_dir(dir, &cfg.models, data_dir);
    let install = root.join(models::NIGHT_DIR);
    let cli_path = install.join("whisper-cli");
    if cli_path.is_file() && !force {
        println!(
            "{} is already built. `--force` rebuilds it.",
            cli_path.display()
        );
        return Ok(());
    }
    let src = root.join("whisper.cpp-src");
    let build = src.join(format!("build-{}", backend.as_str()));
    let jobs = jobs.unwrap_or_else(|| (num_cpus().max(2) / 2).max(1));

    println!("models dir:  {}", root.display());
    println!(
        "source:      {} @ {}",
        src.display(),
        models::NIGHT_WHISPER_TAG
    );
    println!("backend:     {}", backend.as_str());
    println!("jobs:        {jobs} (nice 19)\n");

    std::fs::create_dir_all(&root)?;
    if !src.join(".git").is_dir() {
        run_step(
            "cloning whisper.cpp",
            Proc::new("git")
                .arg("clone")
                .arg("--depth")
                .arg("1")
                .arg("--branch")
                .arg(models::NIGHT_WHISPER_TAG)
                .arg("https://github.com/ggml-org/whisper.cpp.git")
                .arg(&src),
        )?;
    } else {
        run_step(
            "checking out the pinned tag",
            Proc::new("git").current_dir(&src).args([
                "checkout",
                "--quiet",
                models::NIGHT_WHISPER_TAG,
            ]),
        )?;
    }

    let mut configure = Proc::new("cmake");
    configure
        .arg("-S")
        .arg(&src)
        .arg("-B")
        .arg(&build)
        .arg("-DCMAKE_BUILD_TYPE=Release")
        .arg("-DWHISPER_BUILD_TESTS=OFF")
        .arg("-DWHISPER_BUILD_SERVER=OFF")
        // The binary and its .so files are copied into one flat directory and
        // opened from there for years; a build-tree RUNPATH would point at a
        // directory that no longer exists after the next `--force`.
        .arg("-DCMAKE_BUILD_WITH_INSTALL_RPATH=ON")
        .arg("-DCMAKE_INSTALL_RPATH=$ORIGIN")
        .arg("-DCMAKE_BUILD_RPATH_USE_ORIGIN=ON");
    match backend {
        NightBackend::Vulkan => {
            configure.arg("-DGGML_VULKAN=ON");
            // ggml-vulkan needs three things a desktop with a working Vulkan
            // driver still does not necessarily have: the Vulkan HEADERS, the
            // SPIRV headers (which it #includes directly, so they must be on
            // the compiler's include path, not merely findable by cmake), and
            // `glslc`. The user's machine had the driver and glslc and neither
            // header set, and cmake's answer was "Could NOT find Vulkan". The
            // two header repos are header-only and pinned to one SDK release,
            // so they are cloned beside the source rather than demanded of the
            // distribution.
            let glslc = ["/usr/bin/glslc", "/usr/local/bin/glslc"]
                .iter()
                .map(Path::new)
                .find(|p| p.is_file());
            if glslc.is_none() {
                anyhow::bail!(
                    "the Vulkan build needs `glslc` (the shaderc package) and it is not installed"
                );
            }
            if !Path::new("/usr/include/vulkan/vulkan.h").is_file() {
                let deps = src.join("deps");
                std::fs::create_dir_all(&deps)?;
                let vk = deps.join("Vulkan-Headers");
                let spv = deps.join("SPIRV-Headers");
                for (dir, tag, url) in [
                    (
                        &vk,
                        models::NIGHT_VULKAN_HEADERS_TAG,
                        "https://github.com/KhronosGroup/Vulkan-Headers.git",
                    ),
                    (
                        &spv,
                        models::NIGHT_SPIRV_HEADERS_TAG,
                        "https://github.com/KhronosGroup/SPIRV-Headers.git",
                    ),
                ] {
                    if !dir.join(".git").is_dir() {
                        run_step(
                            &format!(
                                "fetching {} @ {tag}",
                                dir.file_name().unwrap().to_string_lossy()
                            ),
                            Proc::new("git")
                                .arg("clone")
                                .arg("--depth")
                                .arg("1")
                                .arg("--branch")
                                .arg(tag)
                                .arg(url)
                                .arg(dir),
                        )?;
                    }
                }
                let libvulkan = [
                    "/usr/lib/libvulkan.so",
                    "/usr/lib/libvulkan.so.1",
                    "/usr/lib64/libvulkan.so.1",
                    "/usr/lib/x86_64-linux-gnu/libvulkan.so.1",
                ]
                .iter()
                .map(Path::new)
                .find(|p| p.is_file())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "no libvulkan.so on this machine — install the Vulkan loader first"
                    )
                })?;
                // ggml-vulkan `find_package(SPIRV-Headers)`s, which wants the
                // repo's cmake package INSTALLED somewhere, not merely checked
                // out. Header-only, so the install is a copy.
                let spv_prefix = deps.join("spirv-install");
                if !spv_prefix
                    .join("include/spirv/unified1/spirv.hpp")
                    .is_file()
                {
                    run_step(
                        "configuring SPIRV-Headers",
                        Proc::new("cmake")
                            .arg("-S")
                            .arg(&spv)
                            .arg("-B")
                            .arg(spv.join("build"))
                            .arg(format!("-DCMAKE_INSTALL_PREFIX={}", spv_prefix.display()))
                            .arg("-DSPIRV_HEADERS_ENABLE_TESTS=OFF"),
                    )?;
                    run_step(
                        "installing SPIRV-Headers",
                        Proc::new("cmake").arg("--install").arg(spv.join("build")),
                    )?;
                }
                configure
                    .arg(format!(
                        "-DVulkan_INCLUDE_DIR={}",
                        vk.join("include").display()
                    ))
                    .arg(format!("-DVulkan_LIBRARY={}", libvulkan.display()))
                    .arg(format!("-DCMAKE_PREFIX_PATH={}", spv_prefix.display()))
                    .arg(format!(
                        "-DCMAKE_CXX_FLAGS=-I{}",
                        spv_prefix.join("include").display()
                    ));
            }
        }
        NightBackend::Hip => {
            // hipBLAS and rocBLAS, which are a separate and much larger install
            // than the HIP runtime: a machine with `hipcc` does not necessarily
            // have them, and cmake's error when it does not is unhelpful enough
            // to be worth naming here.
            configure
                .arg("-DGGML_HIP=ON")
                .arg("-DAMDGPU_TARGETS=gfx1100")
                .arg("-DCMAKE_HIP_ARCHITECTURES=gfx1100");
        }
        NightBackend::Cpu => {}
    }
    run_step("configuring", &mut configure)?;
    run_step(
        "building",
        Proc::new("cmake")
            .arg("--build")
            .arg(&build)
            .arg("-j")
            .arg(jobs.to_string()),
    )?;

    // Install exactly what the daemon opens: the binary and the shared objects
    // it loads. The build tree carries a dozen other programs, and this
    // directory is on a machine's disk for as long as the feature is on.
    std::fs::create_dir_all(&install)?;
    let bin = build.join("bin");
    let mut installed = 0usize;
    for entry in std::fs::read_dir(&bin)
        .with_context(|| format!("reading {}", bin.display()))?
        .flatten()
    {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == "whisper-cli" || name.starts_with("lib") {
            std::fs::copy(entry.path(), install.join(name.as_ref()))?;
            installed += 1;
        }
    }
    if !cli_path.is_file() {
        anyhow::bail!(
            "the build finished but {} is not there — look in {}",
            cli_path.display(),
            build.display()
        );
    }
    println!("\n{installed} files installed into {}", install.display());
    if backend == NightBackend::Cpu {
        println!(
            "NOTE: this is the CPU build. Large-v3 runs at roughly 17x real time on \
             four cores (FINDINGS §11), which is why a CPU night shift was not shipped. \
             Expect it to be too slow to finish a night's backlog."
        );
    }
    let night = models::NightModels::resolve_at(root, &cfg.night);
    if !night.model_present() {
        println!(
            "\nThe model is still missing: {}",
            models::NightModels::how_to_get_it()
        );
    } else {
        println!("\nBoth halves are here. `[night].enabled = true` turns the night shift on.");
    }
    Ok(())
}

/// One build step, with its output on the terminal: a fifteen-minute compile
/// that prints nothing looks exactly like a hang.
fn run_step(what: &str, cmd: &mut std::process::Command) -> Result<()> {
    println!("== {what}");
    // Same posture as every other heavy thing this program starts: nice 19, so
    // a build cannot win a scheduling contest against whatever the person is
    // actually doing.
    // SAFETY: between fork and exec in a single-threaded child; one syscall,
    // no allocation, no locks.
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(cmd, || {
            libc::setpriority(libc::PRIO_PROCESS, 0, 19);
            Ok(())
        });
    }
    let status = cmd
        .status()
        .with_context(|| format!("{what} — is the tool installed?"))?;
    if !status.success() {
        anyhow::bail!("{what} failed ({})", status);
    }
    Ok(())
}

/// Cores this machine has, for the build's default job count.
fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
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

// ---- 0.8.0, the product round --------------------------------------------

/// `recalld ask "<question>"` — one query box, on the command line.
fn cmd_ask(cfg: &Config, data_dir: &Path, question: &str, limit: usize) -> Result<()> {
    let question = question.trim();
    if question.is_empty() {
        println!("Ask something: `recalld ask \"was hat Aspen gestern gesagt\"`.");
        return Ok(());
    }
    let out = call(
        cfg,
        data_dir,
        "search.ask",
        json!({"q": question, "limit": limit}),
    )?;
    let i = &out["interpretation"];

    // What it understood, FIRST and always — including when it understood
    // nothing, which is the case a user most needs to see.
    let mut facets = Vec::new();
    if let Some(who) = i["speaker_label"].as_str() {
        facets.push(format!("speaker: {who}"));
    }
    if let (Some(from), Some(to)) = (i["from_ms"].as_i64(), i["to_ms"].as_i64()) {
        facets.push(format!(
            "when: {} → {}",
            format_time(from * 1_000_000),
            format_time(to * 1_000_000)
        ));
    }
    match i["query"].as_str().unwrap_or("") {
        "" => facets.push("words: (none — the whole question was facets)".into()),
        q => facets.push(format!("words: {q}")),
    }
    println!("understood as   {}", facets.join("\n                "));
    println!(
        "search          {}",
        match i["mode"].as_str().unwrap_or("?") {
            "hybrid" => "hybrid (keyword + meaning)",
            "fts" => "keyword only — `recalld models fetch --semantic` adds meaning",
            "facets" => "no search: the transcript those facets select",
            other => other,
        }
    );
    println!();

    let hits = out["hits"].as_array().cloned().unwrap_or_default();
    if hits.is_empty() {
        println!("Nothing. Which is not the same as nothing having been said —");
        println!("drop a facet and ask again.");
        return Ok(());
    }
    for h in &hits {
        println!(
            "{:<19}  {:<16}  {}",
            format_time(
                h["t_ns"]
                    .as_str()
                    .and_then(|s| s.parse::<i64>().ok())
                    .unwrap_or(0)
            ),
            h["speaker_name"].as_str().unwrap_or("—"),
            h["snippet"]
                .as_str()
                .or_else(|| h["text"].as_str())
                .unwrap_or("")
        );
    }
    println!("\n{} hit(s).", hits.len());
    Ok(())
}

/// `recalld notes [all|done <id>|dismiss <id>|reopen <id>]`.
fn cmd_notes(cfg: &Config, data_dir: &Path, action: Option<NotesAction>) -> Result<()> {
    let state = |id: i64, state: &str| -> Result<()> {
        let n = call(
            cfg,
            data_dir,
            "notes.set_state",
            json!({"id": id, "state": state}),
        )?;
        println!(
            "note {} is {}: {}",
            n["id"].as_i64().unwrap_or(id),
            n["state"].as_str().unwrap_or(state),
            n["text"].as_str().unwrap_or("")
        );
        Ok(())
    };
    let filter = match action {
        Some(NotesAction::Done { id }) => return state(id, "done"),
        Some(NotesAction::Dismiss { id }) => return state(id, "dismissed"),
        Some(NotesAction::Reopen { id }) => return state(id, "open"),
        // The default list is what is still open: a note you have finished
        // with is not a thing to be reminded of.
        Some(NotesAction::All) => json!({}),
        None => json!({"state": "open"}),
    };
    let out = call(cfg, data_dir, "notes.list", filter)?;
    let rows = out["notes"].as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        println!(
            "No notes. Say \"Recall, merk dir …\" or \"Recall, remember …\" into the microphone\n\
             and the turn is filed here — the transcript keeps it either way."
        );
        return Ok(());
    }
    println!("{:<5}  {:<10}  {:<19}  NOTE", "ID", "STATE", "SAID AT");
    for n in &rows {
        println!(
            "{:<5}  {:<10}  {:<19}  {}",
            n["id"].as_i64().unwrap_or(0),
            n["state"].as_str().unwrap_or("?"),
            format_time(
                n["t_ns"]
                    .as_str()
                    .and_then(|s| s.parse::<i64>().ok())
                    .unwrap_or(0)
            ),
            n["text"].as_str().unwrap_or(""),
        );
    }
    Ok(())
}

// ---- 0.10.0, worlds and turn-taking --------------------------------------

/// `recalld stats <speaker_id>` — how somebody talks.
fn cmd_stats(cfg: &Config, data_dir: &Path, speaker_id: i64, days: Option<i64>) -> Result<()> {
    let mut params = json!({"id": speaker_id});
    if let Some(d) = days {
        params["days"] = json!(d);
    }
    let s = call(cfg, data_dir, "person.stats", params)?;
    let who = call(cfg, data_dir, "person.get", json!({"id": speaker_id}))
        .ok()
        .and_then(|p| {
            p["speaker"]["name"]
                .as_str()
                .or_else(|| p["speaker"]["auto"].as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| speaker_id.to_string());
    let ms = |k: &str| s[k].as_i64().unwrap_or(0);
    let dur = |v: i64| {
        if v >= 60_000 {
            format!("{}m {:02}s", v / 60_000, (v % 60_000) / 1000)
        } else {
            format!("{:.1}s", v as f64 / 1000.0)
        }
    };

    println!("{who}");
    match days {
        Some(d) => println!("{:<20}the last {d} days", "over"),
        None => println!("{:<20}everything captured", "over"),
    }
    println!(
        "{:<20}{:.0}%  ({} of {})",
        "talk share",
        s["share"].as_f64().unwrap_or(0.0) * 100.0,
        dur(ms("speech_ms")),
        dur(ms("conversation_speech_ms"))
    );
    println!("{:<20}{}", "turns", s["turns"].as_i64().unwrap_or(0));
    println!("{:<20}{}", "mean turn", dur(ms("mean_turn_ms")));
    println!(
        "{:<20}{}",
        "longest monologue",
        dur(ms("longest_monologue_ms"))
    );
    println!(
        "{:<20}{:.2}",
        "turns per minute",
        s["turns_per_minute"].as_f64().unwrap_or(0.0)
    );
    println!(
        "{:<20}{} given, {} received",
        "interruptions",
        s["interruptions_given"].as_i64().unwrap_or(0),
        s["interruptions_received"].as_i64().unwrap_or(0)
    );
    println!(
        "{:<20}{}",
        "response latency",
        match s["median_latency_ms"].as_i64() {
            // Null is not zero. Zero would say they always answered
            // instantly; null says they never answered anybody inside the cap.
            None => "never answered anybody within 5 s".to_string(),
            Some(v) => format!("{} (median)", dur(v)),
        }
    );

    let by = s["by_conversation"].as_array().cloned().unwrap_or_default();
    if !by.is_empty() {
        println!();
        println!("{:<20}{:>7}  {:>6}", "conversation", "share", "turns");
        for c in &by {
            println!(
                "{:<20}{:>6.0}%  {:>6}",
                c["thread_id"].as_i64().unwrap_or(0),
                c["share"].as_f64().unwrap_or(0.0) * 100.0,
                c["turns"].as_i64().unwrap_or(0)
            );
        }
    }

    // The caveats travel with the numbers. An approximation printed without
    // its definition is a claim.
    println!();
    for key in ["interruption", "latency"] {
        if let Some(text) = s["definitions"][key].as_str() {
            println!("{key}: {text}");
        }
    }
    Ok(())
}

/// `recalld worlds` — every place a conversation has happened.
fn cmd_worlds(cfg: &Config, data_dir: &Path, limit: usize) -> Result<()> {
    let out = call(cfg, data_dir, "worlds.list", json!({"limit": limit}))?;
    let rows = out["worlds"].as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        println!("No worlds recorded yet.");
        println!(
            "Worlds come from VRChat's own log. Nothing is recorded for a machine that has \
             not run it, and nothing is invented for conversations that predate the table."
        );
        return Ok(());
    }
    println!(
        "{:<34}{:>7}  {:<19}  who is there",
        "world", "visits", "last"
    );
    for w in &rows {
        let name = w["name"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| w["world_id"].as_str().unwrap_or("?").to_string());
        let people: Vec<&str> = w["people"]
            .as_array()
            .map(|ps| ps.iter().filter_map(|p| p["label"].as_str()).collect())
            .unwrap_or_default();
        println!(
            "{:<34}{:>7}  {:<19}  {}",
            truncate(&name, 33),
            w["visits"].as_i64().unwrap_or(0),
            w["last_ms"]
                .as_i64()
                .map(|ms| format_time(ms * 1_000_000))
                .unwrap_or_else(|| "—".into()),
            people.join(", ")
        );
        let topics: Vec<&str> = w["topics"]
            .as_array()
            .map(|ts| ts.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if !topics.is_empty() {
            println!("{:<34}{}", "", topics.join(" · "));
        }
    }
    Ok(())
}

/// Cut a display string to `n` characters — characters, not bytes, because a
/// world name is as likely to be Japanese as English.
fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
}
// ---- end 0.10.0 ----------------------------------------------------------

/// `recalld brief <speaker_id>` — what is outstanding with one person.
fn cmd_brief(cfg: &Config, data_dir: &Path, speaker_id: i64) -> Result<()> {
    let b = call(cfg, data_dir, "person.brief", json!({"id": speaker_id}))?;
    let who = b["speaker"]["name"]
        .as_str()
        .or_else(|| b["speaker"]["auto"].as_str())
        .unwrap_or("?");
    println!(
        "{who}{}",
        if b["speaker"]["you"] == json!(true) {
            "  (you)"
        } else {
            ""
        }
    );
    println!(
        "{:<16}{}",
        "last heard",
        b["last_heard_ms"]
            .as_i64()
            .map(|ms| format_time(ms * 1_000_000))
            .unwrap_or_else(|| "never".into())
    );

    let list = |label: &str, key: &str| {
        let rows = b[key].as_array().cloned().unwrap_or_default();
        if rows.is_empty() {
            println!("{label:<16}—");
            return;
        }
        for (i, c) in rows.iter().enumerate() {
            let due = c["due_ms"]
                .as_i64()
                .map(|ms| format_time(ms * 1_000_000))
                .unwrap_or_else(|| c["due_raw"].as_str().unwrap_or("—").to_string());
            println!(
                "{:<16}[{}] {:<10}  {:<19}  {}",
                if i == 0 { label } else { "" },
                c["state"].as_str().unwrap_or("?"),
                c["source"].as_str().unwrap_or("?"),
                due,
                c["what"].as_str().unwrap_or(""),
            );
        }
    };
    list("they owe you", "open_to_you");
    list("you owe them", "open_from_you");

    let topics: Vec<&str> = b["recent_topics"]
        .as_array()
        .map(|v| v.iter().filter_map(|t| t["topic"].as_str()).collect())
        .unwrap_or_default();
    println!(
        "{:<16}{}",
        "recent topics",
        if topics.is_empty() {
            "— (the local model writes these; it is off by default)".to_string()
        } else {
            topics.join(", ")
        }
    );
    for (i, n) in b["notes_mentioning"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .enumerate()
    {
        println!(
            "{:<16}{}",
            if i == 0 { "your notes" } else { "" },
            n["text"].as_str().unwrap_or("")
        );
    }
    println!("\nEvery promise here is a suggestion until you say otherwise.");
    Ok(())
}

/// `recalld accuracy` — the word error rate the corrections imply.
fn cmd_accuracy(cfg: &Config, data_dir: &Path) -> Result<()> {
    let a = call(cfg, data_dir, "accuracy.summary", json!({}))?;
    let n = a["corrections"].as_i64().unwrap_or(0);
    if n == 0 {
        println!(
            "Nothing corrected yet, so there is nothing to measure — which is not\n\
             an error rate of zero. Fix a line in the GUI and it starts counting."
        );
        return Ok(());
    }
    let pct = |v: &Value| {
        v.as_f64()
            .map(|w| format!("{:.1}%", w * 100.0))
            .unwrap_or_else(|| "—".into())
    };
    println!("{:<16}{n}", "corrections");
    println!("{:<16}{}", "estimated WER", pct(&a["estimated_wer"]));
    println!(
        "{:<16}{}",
        "since",
        a["since_ms"]
            .as_i64()
            .map(|ms| format_time(ms * 1_000_000))
            .unwrap_or_else(|| "—".into())
    );
    for (label, key, name) in [
        ("by source", "by_source", "source"),
        ("by speaker", "by_speaker", "speaker_id"),
    ] {
        for (i, row) in a[key]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .enumerate()
        {
            println!(
                "{:<16}{:<20}  {:>4}  {}",
                if i == 0 { label } else { "" },
                row[name]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| row[name]
                        .as_i64()
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "unlabelled".into())),
                row["corrections"].as_i64().unwrap_or(0),
                pct(&row["estimated_wer"]),
            );
        }
    }
    println!(
        "\nMeasured against YOUR corrections, so it is the error rate of the turns\n\
         somebody bothered to fix — biased high, and the number that moves when the\n\
         vocabulary or the window length changes."
    );
    Ok(())
}

// ---- 0.9.0: ground truth from Discord -------------------------------------

/// `recalld truth …` — the ground-truth subsystem's whole CLI surface.
fn cmd_truth(
    cfg: &Config,
    data_dir: &Path,
    config_path: &Path,
    action: Option<TruthAction>,
) -> Result<()> {
    match action.unwrap_or(TruthAction::Report) {
        TruthAction::Token => {
            let path = config::truth_token_path(config_path);
            let token = recalld::truthnet::token(&path)?;
            println!("{token}");
            eprintln!(
                "  file: {}  (0600)\n  \
                 Paste it into Vencord → Plugins → RecallBridge → Truth token,\n  \
                 and make sure the port there matches [truth].port ({}).",
                path.display(),
                cfg.truth.port
            );
            Ok(())
        }
        act @ (TruthAction::On | TruthAction::Off) => {
            let on = matches!(act, TruthAction::On);
            let mut edited = Config::load(config_path)?;
            edited.truth.enabled = on;
            edited.save(config_path)?;
            // Generated now rather than at first request, so `truth on` is
            // followed by `truth token` and not by a puzzle.
            if on {
                let path = config::truth_token_path(config_path);
                recalld::truthnet::token(&path)?;
                println!(
                    "Truth ingest ON — 127.0.0.1:{}\n  \
                     config: {}\n  token:  {}\n  \
                     restart `recalld run` to apply, then `recalld truth token`.",
                    edited.truth.port,
                    config_path.display(),
                    path.display()
                );
            } else {
                println!(
                    "Truth ingest OFF\n  config: {}\n  \
                     restart `recalld run` to apply. Truth already collected is kept, \
                     and the labelling pass keeps using it.",
                    config_path.display()
                );
            }
            Ok(())
        }
        TruthAction::Users => {
            let a = call(cfg, data_dir, "truth.users", json!({}))?;
            let users = a["users"].as_array().cloned().unwrap_or_default();
            if users.is_empty() {
                println!(
                    "Nothing has arrived yet. Turn the ingest on (`recalld truth on`),\n\
                     restart the daemon, paste `recalld truth token` into the\n\
                     RecallBridge plugin, and talk in a Discord call."
                );
                return Ok(());
            }
            println!(
                "{:<22}{:<24}{:<8}{:<18}linked by",
                "user id", "discord name", "voice", "name"
            );
            for u in users {
                println!(
                    "{:<22}{:<24}{:<8}{:<18}{}",
                    u["user_id"].as_str().unwrap_or("—"),
                    u["name"].as_str().unwrap_or("—"),
                    u["speaker"]
                        .as_i64()
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "—".into()),
                    u["speaker_name"].as_str().unwrap_or("—"),
                    u["via"].as_str().unwrap_or("—"),
                );
            }
            println!(
                "\n`discord name` is a Discord NICKNAME and is never applied to a voice —\n\
                 it is here so you can decide whether to use it. `recalld name <voice> <name>`."
            );
            Ok(())
        }
        TruthAction::Link {
            user_id,
            speaker_id,
        } => {
            let a = call(
                cfg,
                data_dir,
                "truth.link",
                json!({"user_id": user_id, "speaker_id": speaker_id}),
            )?;
            println!(
                "Linked {} ({}) → voice {} ({})",
                a["user_id"].as_str().unwrap_or("?"),
                a["name"].as_str().unwrap_or("?"),
                speaker_id,
                a["speaker_name"].as_str().unwrap_or("unnamed"),
            );
            Ok(())
        }
        TruthAction::Unlink { user_id } => {
            let a = call(cfg, data_dir, "truth.unlink", json!({"user_id": user_id}))?;
            println!(
                "Unlinked {} ({}). The verdicts on past turns stay — they are what\n\
                 Discord said, and that did not change; they simply stop being scored.",
                a["user_id"].as_str().unwrap_or("?"),
                a["name"].as_str().unwrap_or("?"),
            );
            Ok(())
        }
        TruthAction::Report => cmd_truth_report(cfg, data_dir),
    }
}

/// `recalld truth report` — the measurement this whole subsystem exists for.
fn cmd_truth_report(cfg: &Config, data_dir: &Path) -> Result<()> {
    let st = call(cfg, data_dir, "truth.status", json!({}))?;
    let a = call(cfg, data_dir, "truth.summary", json!({}))?;

    let labelled = a["segments_labelled"].as_i64().unwrap_or(0);
    if labelled == 0 {
        println!(
            "No turn has been compared against Discord yet — which is not a score of\n\
             zero, it is an empty measurement.\n\n  \
             ingest:  {}\n  spans:   {}\n\n\
             Turn it on with `recalld truth on`, restart the daemon, paste\n\
             `recalld truth token` into the RecallBridge plugin, and have a call.",
            st["listening"].as_str().unwrap_or("not running"),
            st["spans"].as_i64().unwrap_or(0),
        );
        return Ok(());
    }

    let pct = |v: &Value| {
        v.as_f64()
            .map(|w| format!("{:.1}%", w * 100.0))
            .unwrap_or_else(|| "—".into())
    };
    let n = |k: &str| a[k].as_i64().unwrap_or(0);

    println!("{:<20}{}", "segments compared", labelled);
    for (label, key) in [
        ("  single", "single"),
        ("  overlap", "overlap"),
        ("  partial", "partial"),
        ("  nobody", "nobody"),
        ("  unknown", "unknown"),
    ] {
        println!("{label:<20}{}", n(key));
    }

    let id = &a["identity"];
    println!(
        "\n{:<20}{}",
        "identity scored on",
        id["n"].as_i64().unwrap_or(0)
    );
    println!("{:<20}{}", "  correct", id["correct"].as_i64().unwrap_or(0));
    println!("{:<20}{}", "  wrong", id["wrong"].as_i64().unwrap_or(0));
    println!(
        "{:<20}{}",
        "  declined",
        id["unlabelled"].as_i64().unwrap_or(0)
    );
    println!("{:<20}{}", "  precision", pct(&id["precision"]));
    println!("{:<20}{}", "  recall", pct(&id["recall"]));
    for (i, row) in id["by_speaker"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .enumerate()
    {
        println!(
            "{:<20}{:<8}{:<22}{:>5}  {:>5} ok  {:>5} wrong",
            if i == 0 { "  by voice" } else { "" },
            row["speaker_id"].as_i64().unwrap_or(0),
            row["user_id"].as_str().unwrap_or("—"),
            row["n"].as_i64().unwrap_or(0),
            row["correct"].as_i64().unwrap_or(0),
            row["wrong"].as_i64().unwrap_or(0),
        );
    }

    let og = &a["overlap_gate"];
    println!(
        "\n{:<20}{}",
        "overlap gate",
        og["threshold"]
            .as_f64()
            .map(|v| format!("flags above {v:.2}"))
            .unwrap_or_else(|| "—".into())
    );
    println!(
        "{:<20}{}",
        "  caught",
        og["flagged_when_overlap"].as_i64().unwrap_or(0)
    );
    println!(
        "{:<20}{}",
        "  false alarms",
        og["flagged_when_single"].as_i64().unwrap_or(0)
    );
    println!("{:<20}{}", "  precision", pct(&og["precision"]));
    println!("{:<20}{}", "  recall", pct(&og["recall"]));

    if let Some(caveat) = a["caveat"].as_str() {
        println!("\n{caveat}");
    }
    Ok(())
}

// ---- 0.9.0, the assistant -------------------------------------------------

/// `recalld digest [day]` — one paragraph per conversation.
fn cmd_digest(cfg: &Config, data_dir: &Path, day: Option<&str>) -> Result<()> {
    let day = match day.map(str::trim) {
        None => None,
        // The two words a person actually types. Resolved here rather than in
        // the daemon: "today" is a fact about the terminal's clock, and the
        // socket's `day` parameter is a calendar day so that a client can ask
        // for one without agreeing with the daemon about what time it is.
        Some("today") => Some(recalld::digest::local_day(utc_now_ns())),
        Some("yesterday") => Some(recalld::digest::local_day(
            utc_now_ns() - 86_400 * 1_000_000_000,
        )),
        Some(d) => Some(d.to_string()),
    };
    let params = match &day {
        Some(d) => json!({ "day": d }),
        None => json!({}),
    };
    let out = call(cfg, data_dir, "digest.list", params)?;
    let rows = out["digests"].as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        println!(
            "Nothing summarised{}.\n\n\
             Conversations are read once they have settled, by the local model, after\n\
             everything else it owes you. `recalld graph on` turns it on; `recalld\n\
             models fetch --graph` installs it. Conversations it read and found not\n\
             worth a paragraph are not listed, which is most short ones.",
            day.as_deref()
                .map(|d| format!(" for {d}"))
                .unwrap_or_default()
        );
        return Ok(());
    }
    for (i, d) in rows.iter().enumerate() {
        if i > 0 {
            println!();
        }
        let people: Vec<String> = d["participants"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|p| {
                        p["label"]
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| format!("speaker {}", p["speaker_id"]))
                    })
                    .collect()
            })
            .unwrap_or_default();
        println!(
            "{}  {}  ({} turns)",
            format_time(
                d["started_ns"]
                    .as_str()
                    .and_then(|s| s.parse::<i64>().ok())
                    .unwrap_or(0)
            ),
            if people.is_empty() {
                "nobody the voicebank could name".to_string()
            } else {
                people.join(" · ")
            },
            d["turns"].as_i64().unwrap_or(0),
        );
        println!("  {}", d["summary"].as_str().unwrap_or(""));
        for open in d["open"].as_array().into_iter().flatten() {
            if let Some(s) = open.as_str() {
                println!("  · still open: {s}");
            }
        }
    }
    println!(
        "\n{} conversation{} summarised. Written by the local model on this machine;\n\
         nothing left it.",
        rows.len(),
        if rows.len() == 1 { "" } else { "s" }
    );
    Ok(())
}

// ---- end 0.9.0 ------------------------------------------------------------

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

// ---- 0.11.0: source-aware identity ----------------------------------------

/// `recalld identity …` — the report, and the one narrow repair it justifies.
///
/// Reads the database directly, like `recalld speakers` and `recalld lang`: it
/// says nothing about a running daemon and has to work on a machine where none
/// is running. The repair writes directly for the same reason the merge does —
/// it is a bulk correction to history, not a live decision.
fn cmd_identity(cfg: &Config, data_dir: &Path, action: Option<IdentityAction>) -> Result<()> {
    match action.unwrap_or(IdentityAction::Audit) {
        IdentityAction::Audit => cmd_identity_audit(cfg, data_dir),
        IdentityAction::Repair {
            foreign,
            apply,
            limit,
        } => {
            if !foreign {
                println!(
                    "`identity repair` needs --foreign. It is the only thing it can repair,\n\
                     and naming it is what keeps it from quietly growing a second mode."
                );
                return Ok(());
            }
            cmd_identity_repair(cfg, data_dir, apply, limit)
        }
    }
}

/// How many of the questioned labels the tail prints.
const AUDIT_TAIL: usize = 20;

fn cmd_identity_audit(cfg: &Config, data_dir: &Path) -> Result<()> {
    let store = Store::open(data_dir)?;
    let report = recalld::identity_prior::audit(&store, &cfg.identity)?;

    println!(
        "{:<20}{}",
        "prior",
        if cfg.identity.source_prior {
            format!(
                "ON — a voice foreign to a source needs {:.2}, and {:+.2} on the best \
                 native candidate",
                cfg.identity.label_threshold + cfg.identity.foreign_source_margin,
                cfg.identity.enroll_margin
            )
        } else {
            "OFF — `[identity].source_prior = true` turns it on. This report is what \
             it would see."
                .to_string()
        }
    );
    println!(
        "{:<20}a voice is foreign to a source once it has {} turn(s) elsewhere and none there",
        "foreign after", cfg.identity.foreign_after_segments
    );
    println!(
        "{:<20}{}",
        "hard presence",
        if cfg.identity.presence_hard {
            "on — Discord may exclude a linked account it saw say nothing (live rule only)"
        } else {
            "off"
        }
    );

    println!("\n=== where each voice has been heard ===");
    if report.matrix.is_empty() {
        println!("No voices yet.");
    }
    for (id, name, sources) in &report.matrix {
        if sources.is_empty() {
            println!("{id:>4}  {name:<24}  —  (no live turns)");
            continue;
        }
        let cells = sources
            .iter()
            .map(|s| {
                format!(
                    "{} {} (last {})",
                    s.match_key,
                    s.segments,
                    recalld::clock::iso8601(s.last_ns)
                )
            })
            .collect::<Vec<_>>()
            .join("  ·  ");
        println!("{id:>4}  {name:<24}  {cells}");
    }

    println!("\n=== labels the rule questions ===");
    println!("{:<20}{}", "labels considered", report.considered);
    println!("{:<20}{}", "questioned", report.foreign.len());
    if report.foreign.is_empty() {
        println!(
            "Nothing to look at: no label ever pointed at a voice with no prior history\n\
             on the source it was heard on."
        );
        return Ok(());
    }
    println!(
        "\nJudged by replaying the labels in the order they were made, so each row is\n\
         the FIRST of its run — `followed` says how many more landed on that voice and\n\
         source afterwards.\n"
    );
    let tail = report
        .foreign
        .iter()
        .rev()
        .take(AUDIT_TAIL)
        .collect::<Vec<_>>();
    println!(
        "{:>8}  {:>5}  {:<20}  {:<14}  {:>6}  {:<10}  {:>8}  WHEN",
        "SEGMENT", "VOICE", "NAME", "SOURCE", "SCORE", "VIA", "FOLLOWED"
    );
    for f in tail {
        println!(
            "{:>8}  {:>5}  {:<20}  {:<14}  {:>6}  {:<10}  {:>8}  {}",
            f.segment_id,
            f.speaker_id,
            f.speaker_name,
            f.source,
            f.match_score
                .map(|s| format!("{s:.3}"))
                .unwrap_or_else(|| "—".into()),
            f.label_via.as_deref().unwrap_or("—"),
            f.followed_by,
            recalld::clock::iso8601(f.t_start_ns)
        );
    }
    println!(
        "\n`recalld identity repair --foreign` lists what it would unassign; \
         add --apply to write."
    );
    Ok(())
}

/// The repair. One verb, one direction: **to unassigned**.
///
/// It never re-points a row at another voice, and it is not allowed to grow
/// that ability. The audit's finding is that a label is not supported by where
/// the audio came from — which argues against the name the row has and for no
/// other name at all. A sweep that guessed again in bulk would take one wrong
/// label and make a hundred, with no human anywhere in it.
fn cmd_identity_repair(
    cfg: &Config,
    data_dir: &Path,
    apply: bool,
    limit: Option<usize>,
) -> Result<()> {
    let store = Store::open(data_dir)?;
    let report = recalld::identity_prior::audit(&store, &cfg.identity)?;
    let rows: Vec<_> = report
        .foreign
        .iter()
        .rev()
        .take(limit.unwrap_or(usize::MAX))
        .collect();
    if rows.is_empty() {
        println!("Nothing the rule questions. Nothing to repair.");
        return Ok(());
    }
    println!(
        "{} label(s){}. Each becomes UNASSIGNED — never another voice — and keeps its\n\
         transcript, its audio and its embedding, so a later reassignment still has\n\
         everything to argue from.\n",
        rows.len(),
        if apply { "" } else { ", preview only" }
    );
    let mut done = 0usize;
    for f in &rows {
        println!(
            "  segment {:>7}  {:<20} on {:<14} at {}",
            f.segment_id,
            f.speaker_name,
            f.source,
            f.match_score
                .map(|s| format!("{s:.3}"))
                .unwrap_or_else(|| "—".into()),
        );
        if apply && store.unassign_segment_speaker(f.segment_id)? {
            done += 1;
        }
    }
    if apply {
        println!("\n{done} label(s) unassigned.");
        println!("Restart nothing: the rows are already what every client will read next.");
    } else {
        println!("\nNothing written. Add --apply.");
    }
    Ok(())
}

// ---- end 0.11.0 -----------------------------------------------------------

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
