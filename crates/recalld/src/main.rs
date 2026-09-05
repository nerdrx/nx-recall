//! NX Recall capture daemon — build order Steps 1-4.
//!
//! Capture allowlisted application audio from PipeWire, segment it with Silero
//! VAD, merge it into turns, transcribe it, label the voice, and serve all of
//! it over a Unix socket to clients that never touch the database directly.

mod cli;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
    AccuracyAction, CaptureAction, Cli, Command, GraphAction, IdentityAction, LangAction,
    MicAction, ModelsAction, MoodAction, NightBackend, NotesAction, SemanticAction, SpeakersAction,
    TruthAction, TurnsAction,
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
        Command::Capture { action } => match action {
            CaptureAction::Health { days } => cmd_capture_health(&data_dir, days),
        },
        Command::Allow { match_key } => cmd_set_rule(&config_path, &data_dir, &match_key, true),
        Command::Deny { match_key } => cmd_set_rule(&config_path, &data_dir, &match_key, false),
        Command::Role {
            match_key,
            role,
            account,
        } => cmd_role(
            &cfg,
            &config_path,
            &data_dir,
            &match_key,
            &role,
            account.as_deref(),
        ),
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
                japanese,
                cjk,
                semantic,
                translator,
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
                    japanese,
                    cjk,
                    translator,
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
            // ---- 0.12.0, the archive sweep ------------------------------
            LangAction::Sweep {
                apply,
                redecode,
                batch,
                limit,
                dir,
            } => cmd_lang_sweep(
                &cfg,
                &data_dir,
                dir.as_deref(),
                apply,
                redecode,
                batch,
                limit,
            ),
            // ---- end 0.12.0 ----------------------------------------------
            // ---- 0.12.0, taking a route back ----------------------------
            LangAction::Unroute { apply, dir } => {
                cmd_lang_unroute(&cfg, &data_dir, dir.as_deref(), apply)
            } // ---- end 0.12.0 ---------------------------------------------
        },
        // ---- 0.12.4, cutting a turn where the speaker changes -----------
        //
        // In THIS process, like the language repair and the semantic backfill
        // and for the same two reasons: it is a long batch job that has to be
        // niceable and Ctrl-C-able, and it loads models the daemon may not
        // have resident.
        Command::Turns { action } => match action {
            TurnsAction::Resplit {
                apply,
                undo,
                limit,
                dir,
            } => cmd_turns_resplit(&cfg, &data_dir, dir.as_deref(), apply, undo, limit),
        },
        // ---- end 0.12.4 --------------------------------------------------
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
        // ---- 0.12.5, the mood pass's own switch -------------------------
        Command::Mood { action } => cmd_mood(&cfg, &data_dir, action),
        // ---- end 0.12.5 --------------------------------------------------
        // ---- 0.8.0, the product round ---------------------------------
        Command::Ask { question, limit } => cmd_ask(&cfg, &data_dir, &question.join(" "), limit),
        Command::Notes { action } => cmd_notes(&cfg, &data_dir, action),
        Command::Brief { speaker_id } => cmd_brief(&cfg, &data_dir, speaker_id),
        Command::Accuracy { action } => match action {
            None => cmd_accuracy(&cfg, &data_dir),
            Some(AccuracyAction::Report) => cmd_accuracy_learn(&cfg, &data_dir, false, true),
            Some(AccuracyAction::Learn { apply }) => {
                cmd_accuracy_learn(&cfg, &data_dir, apply, false)
            }
        },
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
    // 0.12.4: the mood pass, for the same reason again.
    .with_mood(cfg.mood.clone())
    // 0.9.0: reminders, digests and translation, for the same reason again.
    .with_assist(cfg.assist.clone())
    // 0.13.0: flap tolerance's grace window and the stereo probe's switch,
    // both read by `status` — see `Control::capture_json`.
    .with_capture(cfg.capture.clone());
    // The three translation settings live in one place rather than being
    // threaded through `segment_json`'s dozen call sites; see `translate::LIVE`.
    // `assist.set` writes the same three, which is what makes them live.
    recalld::translate::adopt(&cfg.assist);
    // 0.12.4: the fourth control on that card, live for the same reason and
    // read from the same place. Not folded into `translate::adopt` — a mood
    // chip is not a translation setting, and one function that owned both
    // would be a name that lied.
    recalld::mood::set_display(&cfg.assist.mood_display);
    // …and where the second backend's files are (0.11.0). Not a setting: the
    // translator is loaded on first use, and this is the disk it is loaded from.
    recalld::translate::set_models_root(models_root.clone());
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

    // ---- 0.12.1: per-user Discord audio -----------------------------------
    //
    // Built whether or not `[truth].audio` is on, for the same reason the truth
    // worker is started whether or not the ingest is: the router is what
    // reports "off" to a client that asks, and the pipeline holds it to answer
    // one question per buffer ("is a per-user stream live") whose answer is
    // `false` for free when the switch is off.
    //
    // It pushes at the SAME queue every microphone and every application pushes
    // at. That is the whole integration: a Discord user's voice becomes an
    // `AudioChunk` with a session id, and everything downstream — VAD, the
    // segmenter, ASR, the store, retention, threading — treats it as audio,
    // because it is.
    let audio_stats = Arc::new(recalld::peruser::AudioStats::default());
    let peruser = Arc::new(recalld::peruser::PerUser::new(
        Arc::clone(&store),
        Arc::clone(&queue),
        cfg.truth.clone(),
        Arc::clone(&audio_stats),
    ));
    pipeline.attach_peruser(Arc::clone(&peruser));
    // 0.12.2: which Discord instance the mute is aimed at. Shared between the
    // inference thread (the only writer of evidence) and the socket (which
    // reads the verdict for `truth.status` and moves the manual override).
    let bridge = Arc::new(recalld::bridge::Picker::new());
    bridge.load_roles(&cfg.truth.bridge_roles);
    pipeline.attach_bridge(Arc::clone(&bridge));
    if cfg.truth.audio {
        info!(
            live_s = cfg.truth.audio_live_s,
            idle_s = cfg.truth.audio_idle_s,
            enrol = cfg.truth.enrol,
            "per-user Discord audio is on: the mixed Discord tap will be muted \
             for analysis while any stream is live"
        );
    }
    // ---- end 0.12.1 -------------------------------------------------------

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
    // ---- 0.11.0: `search.answer` runs its own model call, under the same
    // `[runtime]` discipline every other one does. ----
    service.attach_answers(cfg.runtime.clone());
    // ---- end 0.11.0 ----
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
        // 0.12.5: this used to say "runs only while nothing is captured",
        // which stopped being true when the worker started standing down on
        // `enrich::gate` instead — it now works between conversations no
        // matter what is on screen, and only stands down while audio is
        // still queued for transcription, or while capture is paused.
        info!(
            "the memory graph's local model is enabled; it stands down only while \
             audio is still queued for transcription, or while capture is paused"
        );
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
        let picker = Arc::clone(&bridge);
        std::thread::Builder::new()
            .name("recalld-truth".into())
            .spawn(move || {
                truth::run(
                    store,
                    control,
                    bus,
                    truth_cfg,
                    identity,
                    runtime,
                    stats,
                    stop,
                    Some(picker),
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
                Some(Arc::clone(&peruser)),
                Some(Arc::clone(&bridge)),
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
        audio: Some(Arc::clone(&peruser)),
        bridge: Some(Arc::clone(&bridge)),
    }));
    // 0.12.1: the janitor for per-user streams. A second of granularity against
    // an idle window measured in seconds, and it exists because the case that
    // has to be noticed is precisely the one where nothing arrives any more:
    // everybody left the call, or Discord was closed mid-word. Without it the
    // last turn of every call would sit unflushed until the daemon stopped.
    let audio_sweep_stop = Arc::new(AtomicBool::new(false));
    let audio_sweep = {
        let peruser = Arc::clone(&peruser);
        let stop = Arc::clone(&audio_sweep_stop);
        std::thread::Builder::new()
            .name("recalld-peruser".into())
            .spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    peruser.sweep();
                }
                // On the way out, close what is still open through the same
                // path a quiet stream takes, so a daemon stopping mid-call
                // flushes the same last turn.
                peruser.close_all();
            })
            .map_err(|e| warn!("no per-user audio janitor: {e}"))
            .ok()
    };
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
    // ---- 0.12.4: the mood pass --------------------------------------------
    // Its own thread rather than a pass inside the night shift's or the
    // sweep's, and the reason is the one gate none of the three shares: this
    // needs SenseVoice and nothing else — no GPU, no local compile, no
    // identifier. Folding it into either would make an unrelated
    // `enabled = false` silently turn it off.
    let mood_stop = Arc::new(recalld::mood::MoodStop::default());
    let mood_thread = {
        let store = Arc::clone(&store);
        let control = Arc::clone(&control);
        let bus = Arc::clone(&bus);
        let root = models_root.clone();
        let dir = data_dir.to_path_buf();
        let whole = cfg.clone();
        let stats = Arc::clone(&control.mood_stats);
        let stop = Arc::clone(&mood_stop);
        std::thread::Builder::new()
            .name("recalld-mood".into())
            .spawn(move || recalld::mood::run(store, control, bus, root, dir, whole, stats, stop))
            .map_err(|e| warn!("no mood pass: {e}"))
            .ok()
    };
    // ---- end 0.12.4 -------------------------------------------------------
    // ---- 0.12.0: the archive language sweep -------------------------------
    // Its own thread rather than a second pass inside the night shift's, and
    // the reason is the one gate the two do not share: the night shift needs a
    // gigabyte of whisper and a local compile before it can do anything at all,
    // and the CJK half of this needs a 13 MB identifier. Folding it in would
    // have made `[night].enabled = false` — the shipped default — silently turn
    // off a feature that has nothing to do with the night shift's model.
    let sweep_stop = Arc::new(NightStop::default());
    let sweep_thread = {
        let store = Arc::clone(&store);
        let control = Arc::clone(&control);
        let bus = Arc::clone(&bus);
        let root = models_root.clone();
        let dir = data_dir.to_path_buf();
        let whole = cfg.clone();
        let stop = Arc::clone(&sweep_stop);
        std::thread::Builder::new()
            .name("recalld-sweep".into())
            .spawn(move || recalld::sweep::run(store, control, bus, root, dir, whole, stop))
            .map_err(|e| warn!("no archive language sweep: {e}"))
            .ok()
    };
    // ---- end 0.12.0 -------------------------------------------------------
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
    mood_stop.stop();
    sweep_stop.stop();
    reminder_stop.stop();
    assist_stop.stop();
    if let Some(s) = socket {
        s.shutdown();
    }
    if let Some(i) = truth_ingest {
        i.shutdown();
    }
    // 0.12.1: stopped AFTER the ingest, so no frame can open a session the
    // janitor has already closed on its way out.
    audio_sweep_stop.store(true, Ordering::Relaxed);
    for handle in [
        audio_sweep,
        roster_thread,
        sweeper_thread,
        enrich_thread,
        quality_thread,
        truth_thread,
        night_thread,
        // 0.12.4.
        mood_thread,
        // 0.12.0.
        sweep_thread,
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

/// `recalld capture health` — reads the `gaps` table directly, like
/// `cmd_speakers` reads `speakers`: it says nothing about a running daemon
/// and works on a machine where none is running.
fn cmd_capture_health(data_dir: &Path, days: i64) -> Result<()> {
    let store = Store::open(data_dir)?;
    let now = utc_now_ns();
    let window_ns = days.max(1) * 86_400 * 1_000_000_000;
    let health = store.gap_health(now, window_ns)?;

    if health.total == 0 {
        println!("No gaps in the last {days}d. Capture has been contiguous.");
        return Ok(());
    }

    println!(
        "{} gap(s) in the last {days}d ({:.0}% unexplained)\n",
        health.total,
        health.unexplained_share * 100.0
    );

    println!("BY CAUSE");
    for (cause, n) in &health.by_cause {
        let explain = pipeline::GapCause::parse(cause)
            .map(|c| c.explain())
            .unwrap_or("(unknown cause — this build is older than the row)");
        println!("  {:>5}  {:<22}  {}", n, cause, wrap_explain(explain, 68));
    }

    println!("\nTOP SOURCES");
    for s in &health.top_sources {
        println!("  {:>5}  {} ({})", s.count, s.display_name, s.match_key);
    }

    println!("\nTOP HOURS (UTC)");
    let mut hours = health.hours.clone();
    hours.sort_by_key(|h| std::cmp::Reverse(h.total));
    for h in hours.iter().take(10) {
        let by_cause: Vec<String> = h.by_cause.iter().map(|(c, n)| format!("{c}={n}")).collect();
        println!(
            "  {:>5}  {}  {}",
            h.total,
            recalld::clock::iso8601(h.hour_start_ns),
            by_cause.join(", ")
        );
    }
    Ok(())
}

/// Indent every line after the first, so a long explanation does not run
/// straight into the terminal's right edge on the CLI's fixed-width table.
fn wrap_explain(s: &str, width: usize) -> String {
    let mut out = String::new();
    let mut line_len = 0;
    for (i, word) in s.split_whitespace().enumerate() {
        if i > 0 && line_len + 1 + word.len() > width {
            out.push_str("\n                          ");
            line_len = 0;
        } else if i > 0 {
            out.push(' ');
            line_len += 1;
        }
        out.push_str(word);
        line_len += word.len();
    }
    out
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

/// `recalld role <MATCH_KEY> bridge|other|auto` (0.12.2).
///
/// Goes through the socket when a daemon is running, so the role acts on the
/// next buffer rather than at the next restart; falls back to editing the
/// config when there is nothing to talk to, because a switch you can only set
/// while the thing is running is half a switch.
fn cmd_role(
    cfg: &Config,
    config_path: &Path,
    data_dir: &Path,
    match_key: &str,
    role: &str,
    account: Option<&str>,
) -> Result<()> {
    let parsed = recalld::bridge::Role::parse(role).ok_or_else(|| {
        anyhow::anyhow!(
            "role must be one of {} — got {role:?}",
            recalld::bridge::ROLES.join(", ")
        )
    })?;
    let account = account.map(str::trim).filter(|a| !a.is_empty());
    if account.is_some() && parsed != recalld::bridge::Role::Bridge {
        anyhow::bail!(
            "--account names the bridge whose spans this client's audio carries, \
             so it only means anything with role \"bridge\""
        );
    }
    let said = match parsed {
        recalld::bridge::Role::Bridge => match account {
            Some(a) => format!(
                "{match_key} carries the RecallBridge plugin signed in as {a}: it is muted \
                 while that bridge's per-user audio arrives, and its turns are judged \
                 against that bridge's speaking spans and no other's."
            ),
            None => {
                format!(
                    "{match_key} carries the RecallBridge plugin: it is muted while per-user audio arrives."
                )
            }
        },
        recalld::bridge::Role::Other => {
            format!(
                "{match_key} does not carry the plugin: it is never muted, and its call is always recorded."
            )
        }
        recalld::bridge::Role::Auto => {
            format!(
                "{match_key} is measured again: the daemon decides which client the streams explain."
            )
        }
    };
    match call(
        cfg,
        data_dir,
        "sources.instance_role",
        json!({
            "source": match_key,
            "role": parsed.as_str(),
            "account_id": account,
        }),
    ) {
        Ok(out) => {
            println!("{said}");
            if !out["persisted"].as_bool().unwrap_or(false) {
                println!(
                    "  (the daemon could not write the config, so this lasts until it restarts)"
                );
            }
            Ok(())
        }
        Err(e) => {
            // No daemon. The file is still the truth it reads at start-up.
            let mut file = Config::load(config_path)?;
            if parsed == recalld::bridge::Role::Auto {
                file.truth.bridge_roles.remove(match_key);
            } else {
                file.truth.bridge_roles.insert(
                    match_key.to_string(),
                    recalld::bridge::role_spec(parsed, account),
                );
            }
            file.save(config_path)?;
            println!(
                "{said}\n  config: {}\n  the daemon is not running ({e:#}); it will read this at start-up.",
                config_path.display()
            );
            Ok(())
        }
    }
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

    // Japanese (0.11.0). One line for the pair, because the flag installs the
    // pair: a decoder nothing can route to never runs, and an identifier with
    // nothing to hand a turn to only writes a log line. Absent is a normal
    // state and what happens then is what happened in 0.10.3 — a Japanese turn
    // comes back as Latin nonsense.
    let japanese = models.japanese();
    let sense_voice = models.sense_voice();
    let lid = models.lid();
    println!();
    if japanese.present() && lid.present() {
        println!("japanese:           on   ({})", japanese.model_id());
    } else {
        println!("japanese:           off  (optional)");
        println!("  {}", models::CjkModel::how_to_get_it(&["ja"]));
    }
    // Korean and Chinese ride on the same identifier and are reported on their
    // own line because they are their own download (0.11.6): the bench kept two
    // decoders, so an install can legitimately have one and not the other.
    if sense_voice.present() && lid.present() {
        println!("korean/chinese:     on   ({})", sense_voice.model_id());
    } else {
        println!("korean/chinese:     off  (optional)");
        println!("  {}", models::CjkModel::how_to_get_it(&["ko", "zh"]));
    }
    for e in japanese
        .entries()
        .into_iter()
        .chain(sense_voice.entries())
        .chain(lid.entries())
    {
        println!(
            "  {:<14}  {:>10}  {}",
            e.role,
            e.bytes().map(fetch::human).unwrap_or_else(|| "-".into()),
            e.path.display()
        );
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
    // ---- 0.12.0: the archive sweep ---------------------------------------
    // Printed before the flagged count and unconditionally, because the two
    // backlogs are disjoint and an empty one of them says nothing about the
    // other: `repair` walks rows marked as a disagreement, `sweep` walks rows
    // nobody ever asked a model about at all.
    let sweep_cfg = recalld::sweep::routing_cfg(&cfg.asr);
    let (owed, swept) = store.lang_sweep_counts(sweep_cfg.lid_min_s)?;
    println!(
        "{:<20}{:.1} s and {} identifier window(s) that must agree",
        "sweep bar", sweep_cfg.lid_min_s, sweep_cfg.lid_windows
    );
    println!(
        "{:<20}{owed} never asked about, {swept} already swept",
        "sweep"
    );
    if owed > 0 {
        if models.lid().present() {
            println!("`recalld lang sweep` shows what asking would do.");
        } else {
            println!("  {}", recalld::lid::how_to_get_it());
        }
    }
    // ---- end 0.12.0 -------------------------------------------------------
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

/// `recalld lang sweep [--apply]` — the archive sweep (0.12.0).
///
/// In THIS process, like the repair above it and for the same two reasons: it
/// is a long batch job that has to be niceable and Ctrl-C-able, and it loads
/// models the daemon may not have resident. It opens its own connection to the
/// same database, which is safe while the daemon is capturing — SQLite
/// serialises the writes and each one here is a single row.
#[allow(clippy::too_many_arguments)]
fn cmd_lang_sweep(
    cfg: &Config,
    data_dir: &Path,
    dir: Option<&Path>,
    apply: bool,
    redecode: bool,
    batch: usize,
    limit: Option<usize>,
) -> Result<()> {
    use std::sync::{Arc, Mutex};

    let root = fetch::target_dir(dir, &cfg.models, data_dir);
    let models = ModelSet::resolve_at(root, &cfg.models);
    if !models.lid().present() {
        println!("{}", recalld::lid::how_to_get_it());
        return Ok(());
    }
    // Idle priority, no CPU pinning — the semantic backfill's rule and the
    // repair's: a batch job competing with a live capture never wins a
    // timeslice from a frame.
    pipeline::deprioritise_current_thread(19, &[]);

    let store = Arc::new(Mutex::new(Store::open(data_dir)?));
    let lang_cfg = cfg.lang.clone();
    // `--redecode` turns the rewriting on for THIS run only; without it the
    // config decides, and the config ships with it off (FINDINGS §30).
    let asr_cfg = recalld::config::AsrConfig {
        lang_sweep_redecode: redecode || cfg.asr.lang_sweep_redecode,
        ..cfg.asr.clone()
    };
    let pass = recalld::sweep::Pass::new(&store, None, data_dir, &asr_cfg, &lang_cfg, apply);
    // BOTH routers from the pass's own config, never the operator's: the
    // identifier is built from `lid_windows` once, at construction, and a
    // router built from the live config would be looser than the pass thinks.
    let mut cjk = recalld::asr_cjk::Cjk::new(&models, pass.asr_cfg());
    let mut poly =
        recalld::polyglot::Polyglot::new(&models, pass.asr_cfg(), &cfg.night, &cfg.runtime);
    let floor = pass.asr_cfg().lid_min_s;
    let (owed, swept) = {
        let guard = store.lock().unwrap_or_else(|p| p.into_inner());
        guard.lang_sweep_counts(floor)?
    };
    println!(
        "{owed} untagged turn(s) at or above {floor} s with audio to read; {swept} already swept"
    );
    if owed == 0 {
        return Ok(());
    }
    if pass.redecode {
        if let Some(note) = poly.startup_note(pass.asr_cfg()) {
            println!("  {note}");
        }
    } else {
        println!(
            "  transcripts will NOT be replaced — only `lang` is written. \
             Eight of the nine rewrites this measured were wrong (FINDINGS §30); \
             `--apply --redecode` turns it on anyway."
        );
    }
    if !apply {
        println!("previewing — the identifier will run, nothing will be decoded or written.");
    }

    let started = std::time::Instant::now();
    let never = || false;
    let report = recalld::sweep::run_pass(&pass, &mut cjk, &mut poly, batch, limit, &never, |r| {
        eprint!(
            "\r  {} scanned, {} asked, {} settled\x1b[K",
            r.scanned,
            r.asked,
            r.settled()
        );
    })?;
    eprintln!();

    println!(
        "scanned {} in {:.1}s; {} cost a model pass",
        report.scanned,
        started.elapsed().as_secs_f64(),
        report.asked,
    );
    if apply {
        for (tag, n) in &report.routed {
            println!("  {n:>6}  re-decoded as {tag}");
        }
        for (tag, n) in &report.stamped {
            println!("  {n:>6}  stamped {tag} off the reading alone; the words are untouched");
        }
        if report.marked > 0 {
            println!(
                "  {:>6}  asked, nothing to act on; marked so they are not asked again",
                report.marked
            );
        }
    } else {
        // The preview's two tables: what was heard, and what would have been
        // acted on. The second is a strict subset of the first and the gap
        // between them is the point — most of what the identifier says is
        // something nothing here does anything about.
        println!("what the identifier heard:");
        let mut heard: Vec<(&String, &usize)> = report.heard.iter().collect();
        heard.sort_by(|a, b| b.1.cmp(a.1));
        for (tag, n) in heard.iter().take(12) {
            println!("  {n:>6}  {tag}");
        }
        println!(
            "what the routes would re-decode{}:",
            if pass.redecode {
                ""
            } else {
                ", if --redecode were given"
            }
        );
        if report.would_route.is_empty() {
            println!("  {:>6}  nothing", 0);
        }
        for (tag, n) in &report.would_route {
            println!("  {n:>6}  {tag}");
        }
        let stampable: usize = report
            .heard
            .iter()
            .filter(|(tag, _)| recalld::sweep::STAMPABLE.contains(&tag.as_str()))
            .map(|(_, n)| *n)
            .sum();
        println!("  {stampable:>6}  would be stamped de/en; the rest marked and left alone");
        println!("`recalld lang sweep --apply` does it.");
    }
    for (n, line) in [
        (
            report.left_alone,
            "already readable, or a voice pinned to a language another pass owns — free, and \
             not owed again until their words or their voice change",
        ),
        (
            report.too_short,
            "shorter than the sweep's floor; still owed, in case the floor is lowered",
        ),
        (
            report.unavailable,
            "the identifier would not load; nothing was written, so a later run still has them",
        ),
        (report.no_audio, "the audio is gone"),
    ] {
        if n > 0 {
            println!("  {n:>6}  {line}");
        }
    }
    // An `--apply` run has moved the count itself, so the database is the
    // honest answer. A **preview** writes nothing, so its own count cannot have
    // fallen and re-reading it would print the number the operator started
    // with — the shape of the bug this release is fixing. What a preview knows
    // is how many rows it just resolved, so it subtracts them and says what
    // `--apply` would leave.
    let left = {
        let guard = store.lock().unwrap_or_else(|p| p.into_inner());
        let (now, _) = guard.lang_sweep_counts(floor)?;
        if apply {
            now
        } else {
            now.saturating_sub(report.resolved() as i64)
        }
    };
    println!(
        "{left} still owed{}",
        match () {
            _ if left == 0 => " — the archive is swept",
            _ if limit.is_some() => " — run it again to continue",
            _ => "",
        }
    );
    Ok(())
}

/// `recalld lang unroute [--apply]` — put back the rows the audio route should
/// never have rewritten (0.12.0, `crate::unroute`).
///
/// In this process rather than through the daemon, for `cmd_lang_sweep`'s
/// reasons: it is a batch over the whole archive, it wants to be niceable and
/// interruptible, and with the identifier installed it loads a model the daemon
/// may not have resident.
fn cmd_lang_unroute(cfg: &Config, data_dir: &Path, dir: Option<&Path>, apply: bool) -> Result<()> {
    use recalld::unroute::Verdict;

    pipeline::deprioritise_current_thread(19, &[]);
    let store = Store::open(data_dir)?;
    // The identifier is optional and its absence is not an error: the audio
    // test can only ever put MORE rows back, so a run without it is a strict
    // subset of a run with it.
    let root = fetch::target_dir(dir, &cfg.models, data_dir);
    let models = ModelSet::resolve_at(root, &cfg.models);
    let mut cjk = recalld::asr_cjk::Cjk::new(&models, &cfg.asr);
    let with_audio = models.lid().present() && cjk.lid_ready();
    if !with_audio {
        println!(
            "the identifier is not installed, so the guards are re-run on the stored text and \
             durations only — which can only put back FEWER rows, never more."
        );
    }

    let report = recalld::unroute::run(
        &store,
        with_audio.then_some(&mut cjk),
        data_dir,
        &cfg.asr,
        apply,
        recalld::clock::utc_now_ns(),
    )?;
    if report.looked_at() == 0 {
        println!("no turn on this install was settled by the spoken-language route.");
        return Ok(());
    }
    println!(
        "{} turn(s) were settled by the spoken-language route.\n",
        report.looked_at()
    );
    for row in &report.rows {
        let (mark, why) = match &row.verdict {
            Verdict::Revert(why) => ("<-", *why),
            Verdict::Keep => ("  ", "the guards still accept it"),
            Verdict::NoPrior => ("??", "no prior transcript was recorded; nothing to restore"),
        };
        println!(
            "{mark} {:>7}  {:>5.2}s  {}  {:?}",
            row.id, row.duration_s, row.lang, row.now
        );
        println!(
            "            {} {:?}  ({why}{})",
            if matches!(row.verdict, Verdict::Revert(_)) {
                "back to"
            } else {
                "was"
            },
            row.before,
            if row.by_audio { ", by the audio" } else { "" }
        );
    }
    println!();
    println!(
        "{} to put back, {} left alone, {} with no recorded prior.",
        report.reverted, report.kept, report.no_prior
    );
    if apply {
        println!(
            "done. Each one wrote a `segments.unroute` operation carrying what it discarded, so \
             this is undoable in its turn."
        );
    } else {
        println!("`recalld lang unroute --apply` does it.");
    }
    Ok(())
}
// ---- end 0.12.0 -----------------------------------------------------------

// ---- 0.12.4: cutting a turn where the speaker changes ---------------------

/// `recalld turns resplit [--apply|--undo]` — cut the archive's mixed turns
/// where the person talking changes (FINDINGS §39).
fn cmd_turns_resplit(
    cfg: &Config,
    data_dir: &Path,
    dir: Option<&Path>,
    apply: bool,
    undo: bool,
    limit: Option<usize>,
) -> Result<()> {
    pipeline::deprioritise_current_thread(19, &[]);
    let store = std::sync::Arc::new(std::sync::Mutex::new(Store::open(data_dir)?));
    let now = recalld::clock::utc_now_ns();

    if undo {
        let n = recalld::turnsplit::unsplit(&store, limit.unwrap_or(usize::MAX), now)?;
        println!(
            "{n} split turn(s) put back. Each row has its whole span and its own clip again,\n\
             and the pieces the split minted are deleted. The words are NOT restored: the span\n\
             is right and nothing has re-read the audio, so the honest state is a turn waiting\n\
             for the analysis leg. `recalld lang repair` or the idle worker will fill it in."
        );
        return Ok(());
    }

    // The models are not optional here, unlike the unroute pass: without the
    // embedder there is no curve and therefore no answer at all, and a run
    // that silently reported "nothing to cut" would be a lie.
    let root = fetch::target_dir(dir, &cfg.models, data_dir);
    let models = ModelSet::resolve_at(root, &cfg.models);
    let mut analyzer = recalld::analysis::Analyzer::load(
        &models,
        // The pass exists BECAUSE the live switch is off by default; making it
        // read that switch would mean the command could do nothing and not say
        // why. Every other number in the operating point is the config's.
        &recalld::config::IdentityConfig {
            split_turns: true,
            ..cfg.identity.clone()
        },
    )?;
    analyzer.set_lang_config(&cfg.lang);
    analyzer.set_truth_config(&cfg.truth);
    analyzer.set_asr_config(&models, &cfg.asr, &cfg.night, &cfg.runtime);
    let stats = recalld::analysis::AnalysisStats::default();

    let report = recalld::turnsplit::resplit(
        &store,
        &mut analyzer,
        &stats,
        data_dir,
        limit.unwrap_or(usize::MAX),
        apply,
        now,
    )?;

    println!("{:<26}{}", "turns examined", report.examined);
    if report.examined == 0 {
        println!(
            "\nNo turn on this install has a `partial` or `overlap` verdict, so there is\n\
             nothing here that Discord says holds more than one person's audio."
        );
        return Ok(());
    }
    println!("{:<26}{}", "  too short to cut", report.too_short);
    println!("{:<26}{}", "  clip already retained", report.no_audio);
    println!(
        "{:<26}{} (a piece with no words is not a turn)",
        "  cut found and refused", report.wordless
    );
    println!("{:<26}{}", "turns that change speaker", report.cuts.len());
    println!("{:<26}{}", "new rows", report.new_rows());

    if !report.cuts.is_empty() {
        println!("\n{:<9} {:<21} {:<9} CUT AT", "SEGMENT", "WHEN", "VERDICT");
        for c in report.cuts.iter().take(AUDIT_TAIL) {
            let at = c
                .at_s
                .iter()
                .map(|s| format!("{s:.2}s"))
                .collect::<Vec<_>>()
                .join(", ");
            println!(
                "{:<9} {:<21} {:<9} {at}",
                c.segment_id,
                format_time(c.t_start_ns),
                c.verdict
            );
            for (i, said) in c.said.iter().enumerate() {
                println!("          [{i}] {said:?}");
            }
        }
        if report.cuts.len() > AUDIT_TAIL {
            println!("  … and {} more", report.cuts.len() - AUDIT_TAIL);
        }
    }

    if apply {
        println!(
            "\n{} turn(s) cut. Each wrote a `turns.resplit` operation with its whole prior\n\
             state, and the original clip is still on disk, so `recalld turns resplit --undo`\n\
             puts them back.",
            report.cuts.len()
        );
    } else {
        println!("\nNothing written. Add --apply.");
    }
    Ok(())
}

// ---- end 0.12.4 -----------------------------------------------------------

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
            "on — stands down only while audio is queued for transcription, or while paused"
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
        let floor = n("min_thread_segments").max(1);
        println!(
            "{:<20}{} label(s) over {} of {} conversation(s), {} waiting, {} too short to \
             read (under {} turns)",
            "topics",
            n("topics"),
            n("threads_enriched"),
            n("threads"),
            n("threads_waiting"),
            n("threads_too_short"),
            floor,
        );
    }
    if let Some(err) = state["last_error"].as_str() {
        println!("{:<20}{err}", "last error");
    }
    println!("\nNothing here leaves the machine, and nothing acts on a guess.");
    Ok(())
}

// ---- 0.12.5, the mood pass's own switch ------------------------------------

/// `recalld mood` — how a turn sounded (docs/PROTOCOL.md "0.12.4 — how a turn
/// sounded"). Over the socket, live and persisted, `graph`'s pattern exactly.
fn cmd_mood(cfg: &Config, data_dir: &Path, action: MoodAction) -> Result<()> {
    let out = match action {
        MoodAction::On => call(cfg, data_dir, "mood.set", json!({"enabled": true}))?,
        MoodAction::Off => call(cfg, data_dir, "mood.set", json!({"enabled": false}))?,
        MoodAction::Status => call(cfg, data_dir, "mood.get", json!({}))?,
    };
    let on = out["enabled"].as_bool().unwrap_or(false);
    println!(
        "{:<20}{}",
        "the mood pass",
        if on {
            "on — listening overnight, on the niced cores"
        } else {
            "off (the default)"
        }
    );
    if !out["available"].as_bool().unwrap_or(true) {
        println!(
            "{:<20}{}",
            "decoder",
            out["how"]
                .as_str()
                .unwrap_or("the decoder this needs is not installed")
        );
    }
    // Newest turns first (0.12.5): what somebody just said is what the pass
    // reaches first, not the tail of a two-year archive.
    println!(
        "{:<20}{} read, {} to go — newest first",
        "backlog",
        out["read_total"].as_i64().unwrap_or(0),
        out["backlog"].as_i64().unwrap_or(0),
    );
    if !out["rendered"].as_bool().unwrap_or(false)
        && let Some(why) = out["why"].as_str()
    {
        println!("\n{why}");
    }
    Ok(())
}

// ---- end 0.12.5 -------------------------------------------------------------

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
    print_devices(&s["asr"]["devices"]);
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

/// Which device the models run on (0.12.4, FINDINGS §40).
///
/// One line per live model, with the reason folded to one per *runtime* rather
/// than repeated per model: the four models have two reasons between them, and
/// printing the same paragraph twice teaches a reader that it is boilerplate.
/// An older daemon has no `devices` key and gets nothing rather than a wrong
/// claim about its own hardware.
fn print_devices(devices: &Value) {
    let Some(models) = devices["live_models"].as_array() else {
        return;
    };
    println!(
        "{:<18}live on {}, night shift on {}",
        "inference",
        devices["live"].as_str().unwrap_or("?"),
        devices["night"].as_str().unwrap_or("?"),
    );
    let mut explained: Vec<&str> = Vec::new();
    for m in models {
        println!(
            "{:<18}{:<28} {:<12} {:>5.1}% of the live CPU",
            "",
            m["model"].as_str().unwrap_or("?"),
            m["device"].as_str().unwrap_or("?"),
            m["cpu_share_pct"].as_f64().unwrap_or(0.0),
        );
        let runtime = m["runtime"].as_str().unwrap_or("?");
        if !explained.contains(&runtime) {
            explained.push(runtime);
        }
    }
    for runtime in explained {
        let why = models
            .iter()
            .find(|m| m["runtime"].as_str() == Some(runtime))
            .and_then(|m| m["why"].as_str())
            .unwrap_or("");
        println!("{:<18}{runtime}: {why}", "");
    }
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
    // 0.12.4: the same corrections, as ground truth about the decoders.
    let l = &a["learned"];
    let learned = l["corrections"].as_i64().unwrap_or(0);
    let rules = l["rules"].as_i64().unwrap_or(0);
    if rules > 0 {
        println!("{:<16}{rules} from {learned} corrections", "learned rules");
    } else {
        println!(
            "{:<16}none yet — {} more corrections in one cell",
            "learned rules",
            l["needed"].as_i64().unwrap_or(0)
        );
    }
    println!(
        "\nMeasured against YOUR corrections, so it is the error rate of the turns\n\
         somebody bothered to fix — biased high, and the number that moves when the\n\
         vocabulary or the window length changes.\n\
         `recalld accuracy report` breaks it down by decoder."
    );
    Ok(())
}

/// `recalld accuracy learn|report` (0.12.4) — which decoder your corrections
/// say to believe, cell by cell.
fn cmd_accuracy_learn(cfg: &Config, data_dir: &Path, apply: bool, report: bool) -> Result<()> {
    let out = call(cfg, data_dir, "accuracy.learn", json!({"apply": apply}))?;
    let n = out["corrections"].as_i64().unwrap_or(0);
    let min = out["min_rows_per_cell"].as_i64().unwrap_or(0);
    let margin = out["margin_pp"].as_f64().unwrap_or(0.0);
    println!(
        "{n} correction{} on record, {} of them with a decoder's reading beside them.",
        if n == 1 { "" } else { "s" },
        out["measurable"].as_i64().unwrap_or(0)
    );
    println!(
        "A cell needs {min} of its own to be fitted, and a rule must take {margin:.0} points \
         off held-out error.\n"
    );

    let pct = |v: &Value| {
        v.as_f64()
            .map(|w| format!("{:.1}%", w * 100.0))
            .unwrap_or_else(|| "—".into())
    };
    let cell_line = |row: &Value| {
        let d = |name: &str| {
            row["decoders"]
                .as_array()
                .and_then(|a| a.iter().find(|d| d["decoder"] == name))
                .cloned()
                .unwrap_or(Value::Null)
        };
        println!(
            "{:<22}{:>5}{:>6}  {:>7} {:>7} {:>7}   {}",
            row["cell"].as_str().unwrap_or("?"),
            row["rows"].as_i64().unwrap_or(0),
            row["held_out"].as_i64().unwrap_or(0),
            pct(&d("live")["wer"]),
            pct(&d("context")["wer"]),
            pct(&d("night")["wer"]),
            row["verdict"].as_str().unwrap_or(""),
        );
    };
    println!(
        "{:<22}{:>5}{:>6}  {:>7} {:>7} {:>7}   verdict",
        "cell (kind/voice/len)", "rows", "held", "live", "ctx", "night"
    );
    cell_line(&out["global"]);
    for row in out["cells"].as_array().cloned().unwrap_or_default() {
        cell_line(&row);
    }

    let installed = &out["installed"];
    let have = installed["cells"].as_object().map(|m| m.len()).unwrap_or(0)
        + usize::from(!installed["global"].is_null());
    println!();
    if apply {
        println!("Installed. {have} rule(s) are now in force.");
    } else if report {
        println!("{have} rule(s) currently in force.");
    } else {
        println!("Nothing was changed. Re-run with --apply to install what is above.");
    }
    if have == 0 {
        let short = out["short_by"].as_array().cloned().unwrap_or_default();
        println!(
            "No cell has cleared the bar yet. The night shift keeps its two-of-three vote\n\
             and the live pass keeps its words, which is what 0.9.0 shipped."
        );
        for row in short.iter().take(6) {
            println!(
                "  {:<22}{} more correction(s)",
                row["cell"].as_str().unwrap_or("?"),
                row["needed"].as_i64().unwrap_or(0)
            );
        }
        println!(
            "\nCorrect transcripts in the GUI — the accuracy card on the Memory page counts\n\
             down for you. Every fix is one row of ground truth and they only ever add up."
        );
    }
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
        // ---- 0.12.1: per-user Discord audio --------------------------------
        TruthAction::Audio { state } => {
            let on = match state.trim().to_ascii_lowercase().as_str() {
                "on" | "true" | "yes" => true,
                "off" | "false" | "no" => false,
                other => anyhow::bail!("say `on` or `off`, not {other:?}"),
            };
            let mut edited = Config::load(config_path)?;
            edited.truth.audio = on;
            edited.save(config_path)?;
            if on {
                // Said in the imperative, because every one of these is a step
                // somebody will otherwise get halfway through and stop: the
                // feature needs two switches and a token, and it is silent
                // rather than broken when one of them is missing.
                println!(
                    "Per-user Discord audio ON\n  config: {}\n\n  \
                     restart `recalld run` to apply, then in Vencord → Plugins →\n  \
                     RecallBridge turn on \"Send each person's AUDIO as well\".\n  \
                     Both switches are needed; both are off by default.\n\n  \
                     Vesktop or the web client only — the Discord desktop client\n  \
                     decodes voice in a native module and no plugin can reach it.\n\n  \
                     While per-user streams are arriving the mixed Discord tap is\n  \
                     muted, so nothing is transcribed twice.{}",
                    config_path.display(),
                    if edited.truth.enrol {
                        ""
                    } else {
                        "\n\n  [truth] enrol is off, so these turns will be transcribed and\n  \
                         named but will NOT teach the voicebank. `enrol = true` in the\n  \
                         config if you want them to."
                    }
                );
            } else {
                println!(
                    "Per-user Discord audio OFF\n  config: {}\n  \
                     restart `recalld run` to apply. Frames that arrive anyway are\n  \
                     refused and counted; the mixed Discord tap carries the call again.",
                    config_path.display()
                );
            }
            Ok(())
        }
        // ---- end 0.12.1 ----------------------------------------------------
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
        // ---- 0.12.0: retro-labelling from ground truth ----------------
        TruthAction::Label { apply, limit } => cmd_truth_label(data_dir, apply, limit),
        // ---- end 0.12.0 -----------------------------------------------
        // ---- 0.12.1: the re-verdict -----------------------------------
        TruthAction::Rejudge { apply, limit } => cmd_truth_rejudge(data_dir, apply, limit),
        // ---- end 0.12.1 -----------------------------------------------
    }
}

// ---- 0.12.1: the re-verdict ------------------------------------------------

/// `recalld truth rejudge` — re-read the verdicts on disk under the
/// own-account rule (FINDINGS §34).
///
/// Opens the database directly, like `truth label` beside it: it is a bulk
/// correction to history rather than a live decision, and it has to work on a
/// machine where no daemon is running.
fn cmd_truth_rejudge(data_dir: &Path, apply: bool, limit: Option<usize>) -> Result<()> {
    let store = std::sync::Arc::new(std::sync::Mutex::new(Store::open(data_dir)?));
    let now = recalld::clock::utc_now_ns();
    // No picker: `truth rejudge` runs against a data directory, with or
    // without a daemon behind it, so a live override is not readable here. On
    // an archive that is exactly right — every span it re-judges predates the
    // scope, so `Scope::Every` is the only correct answer anyway.
    let r = recalld::truth::rejudge(&store, limit.unwrap_or(usize::MAX), apply, now, None)?;

    println!("{:<24}{}", "verdicts examined", r.examined);
    if r.examined == 0 {
        println!(
            "\nNothing to re-judge. Either no turn has a `single`, `overlap` or \
             `partial`\nverdict yet, or no Discord account is linked to your own voice — \
             with\nnothing linked to \"You\" there is no account this rule can call inaudible."
        );
        return Ok(());
    }
    println!("{:<24}{}", "verdicts that move", r.changed.len());
    println!("{:<24}{}", "  of them `overlap`", r.overlap_reassigned());
    for (from, to, n) in r.moves() {
        println!("{:<24}{from:<9} -> {to:<9} {n}", "");
    }
    if r.from_columns > 0 {
        println!(
            "{:<24}{} (the spans are gone; the verdict itself was the evidence)",
            "re-derived without spans", r.from_columns
        );
    }
    if r.unresolvable > 0 {
        println!(
            "{:<24}{} `overlap` row(s) whose spans are gone. Left exactly as they\n{:<24}\
             are: the verdict records that two accounts were present and not\n{:<24}which two.",
            "cannot be re-judged", r.unresolvable, "", ""
        );
    }
    if r.restamped > 0 {
        println!("{:<24}{}", "overlap share redone", r.restamped);
    }

    if !r.changed.is_empty() {
        println!(
            "\n{:<9} {:<21} {:<9} {:<9} WHO",
            "SEGMENT", "WHEN", "WAS", "NOW"
        );
        for m in r.changed.iter().take(AUDIT_TAIL) {
            println!(
                "{:<9} {:<21} {:<9} {:<9} {}",
                m.segment_id,
                format_time(m.t_start_ns),
                m.from,
                m.to,
                m.user_id.as_deref().unwrap_or("—"),
            );
        }
        if r.changed.len() > AUDIT_TAIL {
            println!("  … and {} more", r.changed.len() - AUDIT_TAIL);
        }
    }

    if apply {
        println!(
            "\n{} verdict(s) re-judged. Logged as `truth.rejudge`, prior state and all.\n\
             `recalld truth report` now scores identity and the overlap gate against them,\n\
             and `recalld identity calibrate` will fit the gate on the corrected corpus.",
            r.changed.len()
        );
    } else {
        println!("\nNothing written. Add --apply.");
    }
    Ok(())
}

// ---- end 0.12.1 ------------------------------------------------------------

// ---- 0.12.0: retro-labelling from ground truth -----------------------------

/// `recalld truth label` — name the blank turns Discord can already name.
///
/// Opens the database directly, like `identity repair` beside it and for the
/// same reason: this is a bulk correction to history rather than a live
/// decision, and it has to work on a machine where no daemon is running. When
/// one *is* running the two cannot corrupt each other — every write carries
/// `AND speaker_id IS NULL`, so whichever of them reaches a row first wins it
/// and the other simply does not count it.
fn cmd_truth_label(data_dir: &Path, apply: bool, limit: Option<usize>) -> Result<()> {
    let store = std::sync::Arc::new(std::sync::Mutex::new(Store::open(data_dir)?));
    let now = recalld::clock::utc_now_ns();
    let moved = recalld::truth::label_from_truth(&store, limit.unwrap_or(usize::MAX), apply, now)?;
    if moved.is_empty() {
        println!(
            "Nothing to name. Every turn Discord gave a `single` verdict to, for an\n\
             account linked to a voice, already has a speaker."
        );
        return Ok(());
    }

    println!(
        "{} turn(s) the voicebank left blank, and Discord can name{}. Each gets\n\
         `label_via = truth` and no match score — nothing was compared. No prototype\n\
         is enrolled and no voice is minted, and your own account never names a turn:\n\
         a Discord stream is the one place your own voice cannot be.\n",
        moved.len(),
        if apply { "" } else { " (preview only)" }
    );
    let mut by_user: Vec<(String, i64, usize)> = Vec::new();
    for m in &moved {
        match by_user
            .iter_mut()
            .find(|(u, s, _)| *u == m.user_name && *s == m.speaker_id)
        {
            Some((_, _, n)) => *n += 1,
            None => by_user.push((m.user_name.clone(), m.speaker_id, 1)),
        }
    }
    for (name, speaker_id, n) in &by_user {
        println!("  {:<24} → voice {speaker_id:<5} {n} turn(s)", name);
    }

    println!(
        "\n{:<9} {:<21} {:>7} {:>9}",
        "SEGMENT", "WHEN", "SECONDS", "COVERAGE"
    );
    for m in moved.iter().take(AUDIT_TAIL) {
        println!(
            "{:<9} {:<21} {:>7.1} {:>9}",
            m.segment_id,
            format_time(m.t_start_ns),
            m.duration_s,
            m.coverage
                .map(|c| format!("{c:.2}"))
                .unwrap_or_else(|| "—".into()),
        );
    }
    if moved.len() > AUDIT_TAIL {
        println!("  … and {} more", moved.len() - AUDIT_TAIL);
    }

    if apply {
        println!(
            "\n{} turn(s) named. Logged as `truth.label`, prior state and all.",
            moved.len()
        );
        println!("Restart nothing: the rows are already what every client will read next.");
    } else {
        println!("\nNothing written. Add --apply.");
    }
    Ok(())
}

// ---- end 0.12.0 ------------------------------------------------------------

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

    // ---- 0.12.0: the two queues ----
    //
    // Printed whenever there is something in them, and silent when there is
    // not. §29's finding was that 137 turns had been queued for an enrolment
    // pass that was switched off, for as long as the feature had existed, and
    // no report said so — a switch nobody can see is indistinguishable from a
    // bug, and the operator spent the evening looking for the bug.
    // ---- 0.12.1: the re-verdict, said out loud ----
    //
    // "450 overlap" and "450 overlap, and 1,341 more used to be counted here"
    // are different facts about the same install, and every measurement made
    // before the rule read the second number without knowing it. Silent until
    // the pass has run, and silent when it moved nothing: a line saying zero
    // is a claim, and an unrun pass has not made it.
    let rj = &a["rejudge"];
    if let Some(moved) = rj["changed"].as_i64().filter(|n| *n > 0) {
        println!(
            "\n{:<20}{moved} verdict(s), {} of them `overlap`",
            "re-judged",
            rj["overlap_reassigned"].as_i64().unwrap_or(0)
        );
        for m in rj["moves"].as_array().cloned().unwrap_or_default() {
            println!(
                "{:<20}{:<9} -> {:<9} {}",
                "",
                m["from"].as_str().unwrap_or("—"),
                m["to"].as_str().unwrap_or("—"),
                m["n"].as_i64().unwrap_or(0),
            );
        }
        println!(
            "{:<20}your own account is not presence on audio your own client made",
            ""
        );
        if let Some(n) = rj["unresolvable"].as_i64().filter(|n| *n > 0) {
            println!(
                "{:<20}{n} `overlap` row(s) could not be re-judged: the spans are gone",
                ""
            );
        }
    }

    let enrol = &a["enrol"];
    let enrol_waiting = enrol["waiting"].as_i64().unwrap_or(0);
    if enrol_waiting > 0 || enrol["on"].as_bool().unwrap_or(false) {
        println!(
            "\n{:<20}{}",
            "enrolment from truth",
            if enrol["on"].as_bool().unwrap_or(false) {
                "ON"
            } else {
                "OFF — `[truth] enrol = true` turns it on"
            }
        );
        println!("{:<20}{enrol_waiting} turn(s) queued", "  waiting");
    }
    let retro_waiting = a["retro_label"]["waiting"].as_i64().unwrap_or(0);
    if retro_waiting > 0 {
        println!("\n{:<20}{retro_waiting} turn(s)", "blank but nameable");
        println!(
            "{:<20}`recalld truth label` lists them; --apply names them",
            ""
        );
    }

    // 0.12.3: the bridges. Printed before the audio block because it is the
    // frame the rest of this page is read in: with two plugins, "168 spans" is
    // two different numbers about two different calls, and a report that does
    // not separate them is a report about neither.
    if let Some(b) = st["bridges"].as_object() {
        let rows = b["bridges"].as_array().cloned().unwrap_or_default();
        if !rows.is_empty() {
            println!("\n{:<20}{}", "bridges seen", rows.len());
            for r in &rows {
                println!(
                    "{:<20}{:<9} {:<20} {:<12} {} span(s), {}/min",
                    "",
                    r["kind"].as_str().unwrap_or("—"),
                    r["account_id"].as_str().unwrap_or("—"),
                    r["source"].as_str().unwrap_or("(no client mapped)"),
                    r["spans"].as_i64().unwrap_or(0),
                    r["spans_per_min"].as_f64().unwrap_or(0.0),
                );
            }
            let amb = b["ambiguous"].as_array().cloned().unwrap_or_default();
            for kind in amb {
                let kind = kind.as_str().unwrap_or("—");
                println!(
                    "\n{:<20}two {kind} bridges are live and nothing says which client is\n\
                     {:<20}which. Their spans are being pooled, so a verdict may be about\n\
                     {:<20}the other call. Settle it with\n\
                     {:<20}`recalld role <MATCH_KEY> bridge --account <USER_ID>`.",
                    "  AMBIGUOUS", "", "", ""
                );
            }
        }
    }

    // 0.12.1: per-user audio. Printed here rather than in its own command
    // because this is the page somebody reads when they want to know whether
    // the bridge is doing anything, and "the mixed tap is muted right now" is
    // the single most surprising thing the daemon can be doing to a Discord
    // recording.
    if let Some(audio) = st["audio"].as_object() {
        let on = audio["enabled"].as_bool().unwrap_or(false);
        let live = audio["live"].as_i64().unwrap_or(0);
        println!(
            "\n{:<20}{}",
            "per-user audio",
            if on {
                "ON"
            } else {
                "OFF — `recalld truth audio on` (Vesktop only)"
            }
        );
        if on {
            println!(
                "{:<20}{live} live stream(s){}",
                "  streams",
                if live > 0 {
                    " — the Discord client carrying the plugin is muted while they arrive"
                } else {
                    " — nothing arriving; Discord is recorded off the speakers"
                }
            );
            for s in audio["streams"].as_array().cloned().unwrap_or_default() {
                println!(
                    "{:<20}{:<20} {} frame(s), {} ms quiet",
                    "",
                    s["name"].as_str().unwrap_or("—"),
                    s["frames"].as_i64().unwrap_or(0),
                    s["quiet_ms"].as_i64().unwrap_or(0),
                );
            }
            // 0.12.2: WHICH client is muted, and why. The line above says
            // "the mixed Discord tap is muted" and on a two-client install
            // that sentence is ambiguous in the one way that matters.
            if let Some(mute) = audio["mute"].as_object() {
                let rows = mute["instances"].as_array().cloned().unwrap_or_default();
                if rows.is_empty() {
                    println!(
                        "{:<20}no Discord client has been heard yet in this window",
                        "  clients"
                    );
                }
                for r in rows {
                    let share = r["share"]
                        .as_f64()
                        .map(|s| format!("{:.0}%", s * 100.0))
                        .unwrap_or_else(|| "—".into());
                    println!(
                        "{:<20}{:<12} {:<16} {:<7} match {:<5} {}",
                        "  client",
                        r["source"].as_str().unwrap_or("—"),
                        r["instance_key"].as_str().unwrap_or("instance unknown"),
                        r["role"].as_str().unwrap_or("auto"),
                        share,
                        if r["muted"].as_bool().unwrap_or(false) {
                            "MUTED"
                        } else {
                            "recording"
                        },
                    );
                    println!("{:<20}{}", "", r["why"].as_str().unwrap_or(""));
                }
                println!(
                    "{:<20}`recalld role <MATCH_KEY> bridge|other|auto` overrides it",
                    ""
                );
            }
            let c = &audio["counters"];
            let n = |k: &str| c[k].as_i64().unwrap_or(0);
            println!(
                "{:<20}{} taken, {} refused, {} gap(s), {} voice(s) minted",
                "  frames",
                n("frames"),
                n("rejected"),
                n("gaps"),
                n("voices_minted"),
            );
        }
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
            prototypes,
            phantoms,
            apply,
            limit,
        } => match (foreign, prototypes, phantoms) {
            (true, ..) => cmd_identity_repair_foreign(cfg, data_dir, apply, limit),
            (_, true, _) => cmd_identity_repair_prototypes(cfg, data_dir, apply),
            (.., true) => cmd_identity_repair_phantoms(data_dir, apply),
            _ => {
                println!(
                    "`identity repair` needs --foreign, --prototypes or --phantoms. Naming \
                     what it may\ntouch is what keeps it from quietly growing another \
                     mode.\n\n  \
                     --foreign     labels the source prior questions, back to unassigned\n  \
                     --prototypes  voiceprints whose own turn Discord says was somebody else\n  \
                     --phantoms    unnamed voices that are somebody you already have"
                );
                Ok(())
            }
        },
        // ---- 0.11.0: learned identity ---------------------------------
        IdentityAction::Calibrate { apply, reset } => {
            cmd_identity_calibrate(cfg, data_dir, apply, reset)
        } // ---- end 0.11.0 -----------------------------------------------
    }
}

// ---- 0.11.0: learned identity ----------------------------------------------

/// `recalld identity calibrate` — the fit, the held-out table, and the one
/// write it justifies.
///
/// Reads the database directly, like the audit beside it: it says nothing
/// about a running daemon and has to work where none is running. Everything
/// the pass would install is printed before it is installed, because a learned
/// number nobody can see is indistinguishable from a magic one.
fn cmd_identity_calibrate(cfg: &Config, data_dir: &Path, apply: bool, reset: bool) -> Result<()> {
    let store = Store::open(data_dir)?;
    let now = recalld::clock::utc_now_ns();
    if reset {
        let (cleared, dropped) = recalld::identity_learn::reset(&store, now)?;
        println!(
            "{cleared} voice(s) back on the global operating point{}, and a voice is \
             scored on its best prototype again.",
            if dropped {
                ", the learned space dropped"
            } else {
                ""
            }
        );
        return Ok(());
    }

    let name_of = |id: i64| -> String { store.speaker_name(id).ok().flatten().unwrap_or_default() };
    let report = recalld::identity_learn::calibrate(&store, &cfg.identity, apply, now)?;

    println!(
        "{:<22}{}",
        "learning",
        if cfg.identity.learn {
            "ON — the nightly pass may install what clears the gate"
        } else {
            "OFF — `[identity].learn = true` turns the nightly refit on. This \
             report is what it would see."
        }
    );
    println!(
        "{:<22}{} truth rows, {} fit / {} held out (chronological, 60/40)",
        "ground truth", report.rows, report.fit_rows, report.eval_rows
    );
    for (id, fit, held) in &report.per_voice {
        println!(
            "  {:<20}fit {fit:<6} held out {held}",
            format!("{id} {}", name_of(*id))
        );
    }
    if let Some(note) = &report.note {
        println!("\n{note}.");
        return Ok(());
    }

    println!("\ninstalled now");
    if report.installed.is_empty() {
        println!(
            "  nothing — every voice is on the global {:.2}.",
            cfg.identity.label_threshold
        );
    }
    for (id, t, m, n) in &report.installed {
        println!(
            "  {:<20}threshold {t:.2}  margin {m:.2}  (calibrated on {n} turns)",
            format!("{id} {}", name_of(*id))
        );
    }
    println!(
        "  learned space        {}",
        if report.projection_installed {
            "installed"
        } else {
            "none — cosine runs in the extractor's own space"
        }
    );

    println!("\nthe fit proposes");
    if report.proposed.is_empty() {
        println!(
            "  nothing. A voice needs {} truth rows in the fit split and a point that \
             beats\n  the global on them.",
            recalld::calib::MIN_ROWS_PER_VOICE
        );
    }
    for v in &report.proposed {
        println!(
            "  {:<20}threshold {:.2}  margin {:.2}  on {} turns  (F-0.5 {:.3} vs {:.3} global)",
            format!("{} {}", v.speaker_id, name_of(v.speaker_id)),
            v.threshold,
            v.margin,
            v.n,
            v.f_beta,
            v.f_beta_global
        );
    }

    let row = |what: &str, s: &recalld::calib::Score| {
        let pct = |v: f64| {
            if v.is_nan() {
                "—".to_string()
            } else {
                format!("{:.1}%", v * 100.0)
            }
        };
        println!(
            "  {:<28}{:>5}{:>9}{:>7}{:>10}{:>11}{:>9}{:>8.3}",
            what,
            s.n,
            s.correct,
            s.wrong,
            s.declined,
            pct(s.precision()),
            pct(s.recall()),
            s.f_beta(recalld::calib::BETA)
        );
    };
    println!("\nheld out — the only rows any verdict reads");
    println!(
        "  {:<28}{:>5}{:>9}{:>7}{:>10}{:>11}{:>9}{:>8}",
        "arm", "n", "correct", "wrong", "declined", "precision", "recall", "F-0.5"
    );
    row(
        &format!("the globals ({})", report.aggregate_installed.as_str()),
        &report.baseline,
    );
    row("+ per-voice thresholds", &report.candidate);
    if let Some((w, s)) = &report.projection {
        row(
            &format!("+ learned space (p{:.2}/s{:.2})", w.power, w.shrinkage),
            s,
        );
    }

    // Every rule for turning a voice's prototypes into one score, each one on
    // the global bar and on the bars it earns for itself. A bar is a number on
    // a score scale and the aggregate IS the scale, so the second row is the
    // one that describes what installing that rule would do (FINDINGS §45).
    if !report.aggregates.is_empty() {
        println!("\nhow a voice's prototypes become one score");
        println!(
            "  {:<28}{:>5}{:>9}{:>7}{:>10}{:>11}{:>9}{:>8}",
            "arm", "n", "correct", "wrong", "declined", "precision", "recall", "F-0.5"
        );
        for arm in &report.aggregates {
            let mark = if arm.incumbent { "  <- installed" } else { "" };
            row(&format!("{}, global bar", arm.rule.as_str()), &arm.globals);
            println!(
                "{}",
                format_args!(
                    "  {:<28}{:>5}{:>9}{:>7}{:>10}{:>11}{:>9}{:>8.3}{mark}",
                    format!("{}, bars refit for it", arm.rule.as_str()),
                    arm.fitted.n,
                    arm.fitted.correct,
                    arm.fitted.wrong,
                    arm.fitted.declined,
                    if arm.fitted.precision().is_nan() {
                        "—".to_string()
                    } else {
                        format!("{:.1}%", arm.fitted.precision() * 100.0)
                    },
                    if arm.fitted.recall().is_nan() {
                        "—".to_string()
                    } else {
                        format!("{:.1}%", arm.fitted.recall() * 100.0)
                    },
                    arm.fitted.f_beta(recalld::calib::BETA),
                )
            );
        }
        if let Some((a, _)) = &report.aggregate {
            let bars = report
                .aggregate_thresholds
                .iter()
                .map(|v| {
                    format!(
                        "{} {:.2}/{:.2}",
                        name_of(v.speaker_id),
                        v.threshold,
                        v.margin
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            println!(
                "  best challenger: {} — installing it writes {}",
                a.as_str(),
                if bars.is_empty() {
                    "no per-voice bar; every voice on the global".to_string()
                } else {
                    bars
                }
            );
        }
    }
    // The mint path, priced (0.12.2). Every row above scores a decline as a
    // free non-answer; the live daemon turns one into a new `Speaker_NN` seeded
    // with the turn's own audio. When the two pairs differ, the difference is
    // the phantoms the candidate would invent, and `may_install` is not allowed
    // to see only the first pair — that is how 2026-09-04 happened.
    if report.baseline_minting != report.baseline || report.candidate_minting != report.candidate {
        println!("\nthe same two arms with the MINT path in — a decline the daemon would turn");
        println!("into a new voice is scored as the wrong name, because that is what it is");
        row("the globals, minting", &report.baseline_minting);
        row("+ per-voice, minting", &report.candidate_minting);
    }
    println!(
        "\n  thresholds: {}",
        verdict_line(report.thresholds_swap, &report.proposed.is_empty())
    );
    println!(
        "  space:      {}",
        match &report.projection {
            None => "no whitening beat the raw space on the inner split; none offered".into(),
            Some(_) => verdict_line(report.projection_swap, &false),
        }
    );
    println!(
        "  scoring:    {}",
        match &report.aggregate {
            None => "no other rule to compare against".into(),
            Some(_) => verdict_line(report.aggregate_swap, &false),
        }
    );
    if report.projection_cleared {
        println!(
            "  the learned space installed on an earlier evening was TAKEN BACK: this \
             run's\n              own held-out numbers do not re-earn it."
        );
    }

    println!("\nthe overlap gate, held out");
    println!(
        "  {:<8}{:>10}{:>10}{:>10}{:>12}{:>10}{:>9}",
        "thr", "caught", "missed", "false", "precision", "recall", "F-0.5"
    );
    for p in &report.gate_curve {
        let here = (p.threshold - cfg.identity.max_overlap).abs() < 1e-6;
        println!(
            "  {:<8}{:>10}{:>10}{:>10}{:>12}{:>10}{:>9.3}{}",
            format!("{:.2}", p.threshold),
            p.caught,
            p.missed,
            p.false_refusals,
            if p.precision().is_nan() {
                "—".into()
            } else {
                format!("{:.1}%", p.precision() * 100.0)
            },
            if p.recall().is_nan() {
                "—".into()
            } else {
                format!("{:.1}%", p.recall() * 100.0)
            },
            p.f_beta(),
            if here { "   <- shipping" } else { "" }
        );
    }
    if let (Some(best), Some(cur)) = (report.gate_best, report.gate_shipping) {
        println!(
            "  best on the fit split was {:.2}; held out it scores {:.3} against the \
             shipping {:.2}'s {:.3} -> {}",
            best.threshold,
            best.f_beta(),
            cfg.identity.max_overlap,
            cur.f_beta(),
            if best.f_beta() > cur.f_beta() + 1e-9 {
                "the curve argues for a change"
            } else {
                "keep what ships"
            }
        );
    }

    if apply {
        println!(
            "\nwrote {} threshold(s), cleared {}{}{}.",
            report.written,
            report.cleared,
            if report.projection_cleared {
                ", dropped a learned space nothing re-earned"
            } else {
                ""
            },
            match (&report.aggregate, report.aggregate_swap) {
                (Some((a, _)), true) => format!(
                    ", a voice is now scored by {} with the {} bar(s) refit for it",
                    a.as_str(),
                    report.aggregate_thresholds.len()
                ),
                _ => String::new(),
            }
        );
    } else {
        println!(
            "\nNothing was written. `recalld identity calibrate --apply` installs what cleared the gate."
        );
    }
    Ok(())
}

// ---- 0.12.0: `recalld identity repair --prototypes` ------------------------

/// The one repair that is a correctness fix rather than an operating point:
/// throwing out a prototype that is a recording of somebody else.
/// `recalld identity repair --phantoms` (0.12.2).
///
/// The other half of `--prototypes`: that one deletes the vectors a mint burst
/// wrote, this one gives the *turns* back. Preview by default, `--apply` writes,
/// never automatic — the same rule, for the same reason.
fn cmd_identity_repair_phantoms(data_dir: &Path, apply: bool) -> Result<()> {
    let store = Store::open(data_dir)?;
    let now = recalld::clock::utc_now_ns();
    let report = recalld::identity_learn::repair_phantoms(&store, apply, now)?;

    if let Some(note) = &report.note {
        println!("{note}.");
    }
    if report.found.is_empty() {
        println!("No unnamed voice in the bank looks like somebody you already have.");
        return Ok(());
    }
    println!(
        "{:>5}  {:<16}{:>7}{:>11}{:>7}{:>9}  WHAT DISCORD SAYS",
        "VOICE", "NAME", "PROTOS", "CONDEMNED", "ROWS", "COVERED"
    );
    for p in &report.found {
        let says = if p.says.is_empty() {
            "—".to_string()
        } else {
            p.says
                .iter()
                .map(|(_, n, c)| format!("{n} {c}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        println!(
            "{:>5}  {:<16}{:>7}{:>11}{:>7}{:>9}  {says}",
            p.speaker_id, p.name, p.prototypes, p.condemned, p.rows, p.covered
        );
        match (&p.target, &p.refused) {
            (Some((v, n)), _) => println!("       → merge into {n} ({v}); {} row(s) move", p.rows),
            (None, Some(why)) => println!("       → left alone: {why}"),
            (None, None) => {}
        }
    }
    let movable: usize = report.candidates().map(|p| p.rows).sum();
    let n = report.candidates().count();
    println!(
        "\n{n} voice(s) would be merged away and {movable} row(s) relabelled. A row that \
         carries its\nown Discord verdict goes to that verdict's voice, not to the majority's."
    );
    if apply {
        println!(
            "\nMerged {}, relabelled {}, carried {} prototype(s) over. The prior state of \
             every row\nis in `operations` under `{}`.",
            report.merged,
            report.relabelled,
            report.prototypes_moved,
            recalld::identity_learn::PHANTOM_OP
        );
    } else {
        println!(
            "\nNothing was written. `recalld identity repair --phantoms --apply` does it.\n\
             A merge is a tombstone, not a deletion, and every row's prior speaker is \
             recorded first."
        );
    }
    Ok(())
}

fn cmd_identity_repair_prototypes(cfg: &Config, data_dir: &Path, apply: bool) -> Result<()> {
    let store = Store::open(data_dir)?;
    let now = recalld::clock::utc_now_ns();
    // The install's operating point, not the crate's: a before/after table
    // measured at a `max_overlap` nobody is running describes nobody's machine.
    let report = recalld::identity_learn::repair_prototypes(&store, &cfg.identity, apply, now)?;

    if let Some(note) = &report.note {
        println!("{note}.");
    }
    if report.condemned.is_empty() {
        println!(
            "No prototype in the bank contradicts Discord's own verdict about the turn \
             it came from."
        );
        return Ok(());
    }
    println!(
        "{} prototype(s) whose own turn Discord says was somebody else:\n",
        report.condemned.len()
    );
    for c in &report.condemned {
        let owner = format!("{} ({})", c.owner_name, c.owner);
        let said = format!("{} ({})", c.truth_name, c.truth_speaker);
        println!(
            "  prototype {:<6} filed under {owner:<22} but segment {} was {said} \
             ({:.0}% of it)",
            c.prototype_id,
            c.source_segment_id,
            c.coverage * 100.0
        );
    }

    if let Some((before, after)) = &report.measured {
        let pct = |v: f64| {
            if v.is_nan() {
                "—".to_string()
            } else {
                format!("{:.1}%", v * 100.0)
            }
        };
        let row = |what: &str, s: &recalld::calib::Score| {
            println!(
                "  {:<28}{:>5}{:>9}{:>7}{:>10}{:>11}{:>9}{:>8.3}",
                what,
                s.n,
                s.correct,
                s.wrong,
                s.declined,
                pct(s.precision()),
                pct(s.recall()),
                s.f_beta(recalld::calib::BETA)
            );
        };
        println!(
            "\nheld out, on the rows whose own verdict this did NOT read\n  {:<28}{:>5}\
             {:>9}{:>7}{:>10}{:>11}{:>9}{:>8}",
            "arm", "n", "correct", "wrong", "declined", "precision", "recall", "F-0.5"
        );
        row("the bank as it is", before);
        row("with these removed", after);
    }

    if apply {
        println!("\nRemoved {}.", report.deleted);
    } else {
        println!(
            "\nNothing was removed. `recalld identity repair --prototypes --apply` \
             deletes them.\nDeleting a prototype is permanent; the segments and their \
             transcripts are untouched."
        );
    }
    Ok(())
}

/// One line saying whether an arm may ship, in the language of the rule that
/// decided it.
fn verdict_line(swap: bool, nothing_proposed: &bool) -> String {
    if *nothing_proposed {
        "nothing proposed, so nothing to gate".into()
    } else if swap {
        "PASS — precision held, and the gain is big enough to be worth the change".into()
    } else {
        "REFUSED — it cost held-out precision, or it bought too little to be worth \
         moving the operating point for (two points of recall, or a fifth of the \
         wrong labels)"
            .into()
    }
}

// ---- end 0.11.0 -----------------------------------------------------------

/// How many of the questioned labels the tail prints.
const AUDIT_TAIL: usize = 20;

/// A mint burst, as the audit counts one (0.12.2). Three is the smallest run
/// that cannot be two strangers arriving together, and ten minutes is longer
/// than the gap between two turns of somebody who is talking — so a run that
/// clears both is a cascade rather than an evening. Both are a *report's*
/// numbers, not an operating point: nothing is refused or written on them.
const MINT_BURST_K: usize = 3;
const MINT_BURST_WINDOW_MIN: i64 = 10;

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

    // A cap that a merge quietly lifted. `add_prototype` never writes past
    // `max_prototypes`, so anything over it arrived by `merge_speakers`, which
    // re-points a collapsed voice's prototypes and does not re-apply the cap.
    // The inherited vectors are the ones that look like drift and are not:
    // FINDINGS §44 measured the voice itself as stable over the archive's 3.4
    // days (turn-to-turn cosine −0.02/day, r = −0.05) while the inherited
    // prototypes sit 0.19 cosine below the enrolled ones.
    let over = store.oversized_banks(cfg.identity.max_prototypes)?;
    println!("\n=== banks over the cap ===");
    if over.is_empty() {
        println!(
            "None: every voice is within `[identity].max_prototypes` = {}.",
            cfg.identity.max_prototypes
        );
    } else {
        println!(
            "`max_prototypes` is {}, and `add_prototype` never writes past it — so these\n\
             came from a merge, which moves a collapsed voice's prototypes and does not\n\
             re-apply the cap. Inherited prototypes match their new voice's own turns far\n\
             less well than enrolled ones, which reads as the voice having changed.\n",
            cfg.identity.max_prototypes
        );
        println!(
            "{:>5}  {:<24}  {:>11}  {:>9}",
            "VOICE", "NAME", "PROTOTYPES", "OVER BY"
        );
        for (id, name, n) in &over {
            println!(
                "{:>5}  {:<24}  {:>11}  {:>9}",
                id,
                name,
                n,
                n - cfg.identity.max_prototypes as i64
            );
        }
        println!(
            "\nNothing here is trimmed automatically: every automatic trim was measured\n\
             held out and refused (FINDINGS §44). `recalld identity repair --prototypes`\n\
             is the one command that deletes, and it only removes prototypes whose own\n\
             source turn ground truth says was somebody else."
        );
    }

    // A cascade, after the fact. FINDINGS §46: a fitted label bar above a
    // voice's typical own-turn score turns that voice's turns into mints, and
    // because a same-evening recording outscores an older bank, each phantom
    // then wins the next turn or mints the one after it. The signature is a
    // *run* of mints from one source whose seed turns Discord says were all one
    // person already in the bank, and nothing named it until it had happened.
    let bursts = store.mint_bursts(MINT_BURST_K, MINT_BURST_WINDOW_MIN * 60_000_000_000)?;
    println!("\n=== mint bursts ===");
    println!(
        "{MINT_BURST_K} or more voices minted from one source, each within \
         {MINT_BURST_WINDOW_MIN} minutes of\nthe last."
    );
    if bursts.is_empty() {
        println!("\nNone. New voices arrive one at a time, which is what a new person looks like.");
    }
    for b in &bursts {
        let span = (b.last_ns - b.first_ns) as f64 / 60e9;
        println!(
            "\n{} minted on {} over {:.0} min, from {}",
            b.minted.len(),
            recalld::clock::iso8601(b.first_ns),
            span,
            b.source_key
        );
        println!(
            "  {}",
            b.minted
                .iter()
                .map(|(id, n, _)| format!("{n} ({id})"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        if let [(id, name, n)] = b.merged_into.as_slice()
            && *n == b.minted.len()
        {
            println!(
                "  All {n} were later merged into {name} ({id}) by hand — the same finding, \
                 arriving\n  from the other end. FINDINGS §44: those inherited prototypes are \
                 the ones that\n  match their new voice's own turns least well."
            );
        }
        if b.seeds_with_a_verdict == 0 {
            println!(
                "  Discord has no verdict on any of the turns they were minted from, so \
                 this is\n  reported and not accused: it may simply be a room filling up."
            );
            continue;
        }
        let says = b
            .seeds_say
            .iter()
            .map(|(v, n, c)| format!("{n} ({v}) × {c}"))
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "  the turns they were minted from: {says}  \
             ({} of {} carry a verdict)",
            b.seeds_with_a_verdict,
            b.minted.len()
        );
        if b.seeds_say.len() == 1 {
            println!(
                "  Every seed turn is one voice you already have. That is the cascade, not \
                 a crowd:\n  `recalld identity repair --phantoms` gives those turns back."
            );
        }
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
fn cmd_identity_repair_foreign(
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
