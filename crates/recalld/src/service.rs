//! What the socket's methods actually do.
//!
//! One rule shapes this file: a request must never take longer than the work
//! it names. Reads are served inline; anything whose cost scales with the
//! database (bulk delete) returns an operation handle immediately and reports
//! progress over the event stream, so one client's purge cannot freeze
//! another client's view (DESIGN §8).
//!
//! The second rule is that every retroactive change is *broadcast*. A rename
//! is not "the client that renamed refreshes"; it is a `relabel` event with a
//! sequence number that every view applies in place.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::{Value, json};
use tracing::{info, warn};

use crate::bus::{Bus, Client, Topic};
use crate::clock::{iso8601, ns_to_ms, parse_iso8601, utc_now_ns};
use crate::config::Config;
use crate::control::Control;
use crate::proto::{Error, Request};
use crate::store::{SegmentFilter, SegmentRow, Store};

/// How many segments one delete step touches before it reports progress.
const DELETE_BATCH: usize = 200;

/// How many per-row events one split may broadcast. Beyond this the two
/// `relabel` events stand on their own and the reply asks for a re-query: a
/// client's outbox is bounded and a slow client is disconnected rather than
/// waited for (`bus`), so a big split must not be able to hang up every view.
const SPLIT_EVENT_CAP: usize = 100;

/// One NDJSON frame's byte budget. The GUI's client carries the same number as
/// its oversized-frame guard (`gui/src/main/client.js`, MAX_FRAME_BYTES) — the
/// two have to agree, or the daemon writes a reply that hangs up the client
/// that asked for it.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// The largest segment WAV `segments.audio` will put on the wire. A capped
/// 30 s segment is ~960 KB at 16 kHz mono 16-bit, so this is a corruption
/// guard rather than a working limit; base64 costs 4/3, which keeps the reply
/// under `MAX_FRAME_BYTES` with room for the JSON around it.
const MAX_AUDIO_BYTES: u64 = 10 * 1024 * 1024;

/// How many clips `speakers.sample` returns when the caller does not say.
const SAMPLE_LIMIT: usize = 3;

/// How many recent conversations `person.get` carries. The person page is a
/// way in, not an archive: past a dozen the list stops being scannable and the
/// transcript is the right surface anyway.
const PERSON_THREADS: usize = 12;

/// How many commitments `commitments.list` returns when the caller does not
/// say. The Memory view is a list of what is still owed, not an archive; past
/// this it has stopped being answerable.
const COMMITMENT_LIMIT: usize = 200;

/// Topic labels one `topics.list` carries, and conversations per label.
const TOPIC_LIMIT: usize = 100;
const TOPIC_THREADS: usize = 12;

/// What counts as a one-off voice for `speakers.prune`: at most this many
/// segments and under this much speech in total. Both are deliberately far
/// below anything a person produces in a conversation — a real voice reaches
/// three seconds in one sentence.
/// `lang.repair` over the socket, which is synchronous: rows per call by
/// default, and the hard ceiling on one call. A whole backlog is the CLI's job
/// — `recalld lang repair` runs in its own process, can be watched and can be
/// interrupted, and none of those are true of a socket request.
const REPAIR_LIMIT: usize = 100;
const REPAIR_MAX: usize = 500;
const REPAIR_BATCH: usize = 32;

const PRUNE_MAX_SEGMENTS: i64 = 1;
const PRUNE_MAX_SPEECH_NS: i64 = 3_000_000_000;

/// One segment on the wire.
///
/// Both time forms, deliberately (PROTOCOL "Field conventions"): `t_ms` is what
/// a client renders, `t_ns` is the row's real value as a **string**, because
/// 1.8e18 does not survive a JSON number in any JavaScript client.
pub fn segment_json(row: &SegmentRow) -> Value {
    json!({
        "id": row.id,
        "session": row.session_id,
        "source": row.source,
        // The speaker *id*, not the name: names change, ids are the identity,
        // and a relabel event updates every view in place because of that.
        "speaker": row.speaker_id,
        "speaker_name": row.speaker_name,
        "text": row.text,
        "t_ms": ns_to_ms(row.t_start_ns),
        "t_ns": row.t_start_ns.to_string(),
        "dur_ms": ns_to_ms(row.t_end_ns - row.t_start_ns),
        "t_start_ns": row.t_start_ns.to_string(),
        "t_end_ns": row.t_end_ns.to_string(),
        "overlap_frac": row.overlap_frac,
        "match_score": row.match_score,
        "has_audio": !row.audio_path.is_empty(),
        // Schema v5. `lang` is the transcript's language when one is known at
        // all; `label_via` is how the speaker got here, and the value a client
        // must act on is `"proximity"` — that label was inherited from the
        // turns around it rather than heard, so it is shown as uncertain.
        "lang": row.lang,
        "label_via": row.label_via,
        // How the *language* got there (0.7.7). `"model"` and `"classified"`
        // are the ordinary answers and need no UI; `"context"` is a language
        // inherited from the conversation with the text untouched; and the two
        // a client should say something about are `"re-decode"` — these words
        // were produced by an arbiter re-reading the audio, not by the primary
        // ASR — and `"mismatch"`, where the language is in doubt and `lang` is
        // therefore null.
        "lang_via": row.lang_via,
        // Schema v6: which conversation this turn is part of, or `null` on a
        // row older than threading. A client draws a boundary where this
        // changes and renders a null exactly as it always did.
        "thread": row.thread_id,
        // Schema v10 (0.8.0). `text_via` says which pass produced these words:
        // `"live"` on the way in, `"context"` after the idle worker re-read the
        // turn with the audio around it, `"arbiter"` after a language
        // re-decode. `null` on a row written before the column existed —
        // provenance nobody recorded, not provenance to guess at.
        "text_via": row.text_via,
        // What a second decoder made of them: `"solid"`, `"shaky"`, or `null`
        // when no cross-check ran. A null is NOT "fine": it means nothing has
        // looked, which is the state of every row on a machine that has not run
        // `models fetch --confidence`.
        "asr_confidence": row.asr_confidence,
    })
}

/// One source on the wire — a `sources.list` entry and the body of a `source`
/// event, from one function so the two cannot drift (audit finding #2).
///
/// `allowed` is passed in rather than read from the row: the live allowlist (or,
/// for the microphone, `[mic].enabled`) is the truth, and the mirrored column
/// can be a moment behind it between a toggle and its write.
pub fn source_json(row: &crate::store::SourceRow, allowed: bool) -> Value {
    json!({
        "id": row.id,
        "match_key": row.match_key,
        // "app" or "mic" (schema v4). The microphone is a row here like
        // anything else and is governed by `[mic]` rather than by the
        // allowlist, so a client that does not know the difference must not
        // render it as an app.
        "kind": row.kind,
        // `binary` is the process this was keyed on; `display` is what the
        // application calls itself. For a Wine program these differ, which is
        // exactly why the key is the PE name and not the shared loader
        // (DESIGN §3).
        "binary": row.match_key,
        "display": row.display_name,
        "display_name": row.display_name,
        "allowed": allowed,
        "first_seen": iso8601(row.first_seen),
        "last_seen": iso8601(row.last_seen),
        "streams": row.streams,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpState {
    Running,
    Done,
    Failed,
}

pub struct Service {
    pub store: Arc<Mutex<Store>>,
    pub control: Arc<Control>,
    pub bus: Arc<Bus>,
    next_op: AtomicU64,
    ops: Mutex<HashMap<String, OpState>>,
    /// Semantic search (0.6.5), when the optional model is installed.
    ///
    /// A `OnceLock` set after construction rather than a constructor argument:
    /// the leg is 118 MB of weights that only `recalld run` has any business
    /// loading, and every other caller of `Service::new` — the socket tests,
    /// the mic tests, the server's own tests — would otherwise have to pass a
    /// `None` it does not care about.
    semantic: std::sync::OnceLock<Arc<crate::semantic::SemanticLeg>>,
}

impl Service {
    pub fn new(store: Arc<Mutex<Store>>, control: Arc<Control>, bus: Arc<Bus>) -> Arc<Self> {
        Arc::new(Self {
            store,
            control,
            bus,
            next_op: AtomicU64::new(1),
            ops: Mutex::new(HashMap::new()),
            semantic: std::sync::OnceLock::new(),
        })
    }

    fn store(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Hand the service the loaded semantic leg. Called once, at start-up,
    /// only when the optional model is actually on disk.
    pub fn attach_semantic(&self, leg: Arc<crate::semantic::SemanticLeg>) {
        let _ = self.semantic.set(leg);
    }

    pub fn semantic(&self) -> Option<&Arc<crate::semantic::SemanticLeg>> {
        self.semantic.get()
    }

    pub fn op_state(&self, op: &str) -> Option<OpState> {
        self.ops
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(op)
            .cloned()
    }

    /// Dispatch one request. `client` is the connection it came in on, which
    /// only the subscription methods care about.
    pub fn handle(self: &Arc<Self>, client: &Arc<Client>, req: &Request) -> Result<Value, Error> {
        match req.method.as_str() {
            "subscribe" => self.subscribe(client, req),
            "events.since" => self.events_since(client, req),
            "status" => self.status(),
            "pause" => self.set_paused(true),
            "resume" => self.set_paused(false),
            "sources.list" => self.sources_list(),
            "sources.set" => self.sources_set(req),
            "mic.get" => self.mic_get(),
            "mic.set" => self.mic_set(req),
            "speakers.list" => self.speakers_list(),
            "speakers.name" => self.speakers_name(req),
            "speakers.set_languages" => self.speakers_set_languages(req),
            "speakers.prune" => self.speakers_prune(req),
            "speakers.delete" => self.speakers_delete(req),
            "speakers.merge" => self.speakers_merge(req),
            "speakers.split" => self.speakers_split(req),
            "speakers.sample" => self.speakers_sample(req),
            "person.get" => self.person_get(req),
            "thread.get" => self.thread_get(req),
            // The memory graph's Tiers 2 and 3 (0.7.0, docs/GRAPH.md).
            "graph.summary" => self.graph_summary(),
            "graph.get" => self.graph_get(),
            "graph.set" => self.graph_set(req),
            "graph.enrich" => self.graph_enrich(req),
            "commitments.list" => self.commitments_list(req),
            "commitments.set_state" => self.commitments_set_state(req),
            "topics.list" => self.topics_list(req),
            // The conversational language prior's backlog (0.7.7). The CLI has
            // its own in-process path; this is the same walk for a client that
            // is already holding a socket.
            "lang.repair" => self.lang_repair(req),
            "vocab.get" => self.vocab_get(),
            "vocab.set" => self.vocab_set(req),
            "segments.reassign" => self.segments_reassign(req),
            "segments.correct" => self.segments_correct(req),
            "segments.audio" => self.segments_audio(req),
            "search" => self.search(req),
            "search.semantic" => self.search_semantic(req),
            // ---- 0.8.0, the product round (PROTOCOL "the accuracy round") --
            "search.ask" => self.search_ask(req),
            "notes.list" => self.notes_list(req),
            "notes.set_state" => self.notes_set_state(req),
            "person.brief" => self.person_brief(req),
            "accuracy.summary" => self.accuracy_summary(),
            // ---- end 0.8.0 -------------------------------------------------
            "transcript" => self.transcript(req),
            "roster.now" => self.roster_now(),
            "operations.list" => self.operations_list(req),
            "delete.preview" => self.delete_preview(req),
            "delete.run" => self.delete_run(req),
            other => Err(Error::new(
                "unknown_method",
                format!("no method named {other:?}"),
            )),
        }
    }

    // ---- subscriptions ---------------------------------------------------

    fn subscribe(&self, client: &Arc<Client>, req: &Request) -> Result<Value, Error> {
        let mut topics = Vec::new();
        let mut unknown = Vec::new();
        match req.param("topics") {
            // No list means every topic: a client that says nothing wants the
            // whole stream, which is what a fresh GUI wants.
            None => topics.extend(Topic::ALL),
            Some(Value::Array(items)) => {
                for item in items {
                    let Some(name) = item.as_str() else {
                        return Err(Error::params("topics must be strings"));
                    };
                    match Topic::parse(name) {
                        Some(t) => topics.push(t),
                        // Unknown topics are reported, not fatal: a newer
                        // client asking for a topic we do not have yet still
                        // gets the ones we do.
                        None => unknown.push(name.to_string()),
                    }
                }
            }
            Some(_) => return Err(Error::params("topics must be an array")),
        }
        client.subscribe(&topics);
        Ok(json!({
            "topics": topics.iter().map(|t| t.as_str()).collect::<Vec<_>>(),
            "unknown": unknown,
            "seq": self.bus.current_seq(),
        }))
    }

    /// Catch-up. The batch comes back **in the reply** so the client applies it
    /// in sequence order before any live event it queued meanwhile; re-pushing
    /// it on the stream would only give it duplicates to discard.
    fn events_since(&self, client: &Arc<Client>, req: &Request) -> Result<Value, Error> {
        let since = req.i64("seq")?;
        if since < 0 {
            return Err(Error::params("seq must not be negative"));
        }
        // Which daemon that `seq` was counted by. Optional, so a client written
        // against an older daemon still works — but without it a reconnect
        // across a restart lands an old sequence inside the new ring and gets
        // somebody else's events replayed as its own (audit finding #20).
        if let Some(boot) = req.opt_str("boot")?
            && boot != crate::proto::boot_id()
        {
            return Err(Error::new(
                "resync",
                "that sequence number belongs to an earlier run of this daemon; \
                 re-run your queries and follow the stream from the current seq",
            ));
        }
        match self.bus.events_since(client, since as u64) {
            Ok((events, seq)) => Ok(json!({
                "events": events,
                "replayed": events.len(),
                "seq": seq,
                "boot": crate::proto::boot_id(),
            })),
            Err(_) => Err(Error::new(
                "resync",
                "that sequence number has fallen out of the replay buffer; \
                 re-run your queries and follow the stream from the current seq",
            )),
        }
    }

    // ---- status and pause ------------------------------------------------

    fn status(&self) -> Result<Value, Error> {
        self.status_payload().map_err(Error::from)
    }

    /// One shape, served two ways: the `status` method a client polls and the
    /// `status` event pushed on every state change (pause above all — a pause
    /// nobody can see is not a panic button).
    /// What `status` says about the semantic leg. `available: false` with the
    /// line that fixes it, rather than silence — the GUI's mode toggle renders
    /// this verbatim, so the app and the CLI say the same sentence.
    fn semantic_json(&self, store: &Store) -> anyhow::Result<Value> {
        let Some(leg) = self.semantic() else {
            return Ok(json!({
                "available": false,
                "how": crate::models::SemanticModel::how_to_get_it(),
            }));
        };
        let (resident, bytes, coverage) = leg.stats(store)?;
        Ok(json!({
            "available": true,
            "model": leg.model_id(),
            "dim": crate::semantic::DIM,
            "resident": resident,
            "resident_bytes": bytes,
            "indexed": coverage.embedded,
            "eligible": coverage.eligible,
            "pending": coverage.pending(),
        }))
    }

    /// The accuracy round's block in `status` (0.8.0): what the idle worker is
    /// allowed to do and whether the optional decoder it needs is installed.
    ///
    /// Always present, always the same shape — a client has to be able to tell
    /// "the cross-check is not installed" from "an older daemon", and a missing
    /// key cannot say either.
    fn asr_quality_json(&self) -> Value {
        let cfg = self.control.asr();
        let confidence = self
            .control
            .models_root
            .as_ref()
            .map(|root| crate::models::ConfidenceModel::resolve_at(root.clone(), 1))
            .is_some_and(|m| m.present());
        json!({
            "context_redecode": cfg.context_redecode,
            "context_redecode_below_s": cfg.context_redecode_below_s,
            "confidence": {
                "enabled": cfg.confidence,
                "available": confidence,
                "tau": cfg.confidence_tau,
                "how": (!confidence).then(crate::models::ConfidenceModel::how_to_get_it),
            },
            // The vocabulary is assembled and served; nothing is biased by it.
            // Said here rather than only in the docs, because a client showing
            // a glossary screen must not imply an effect the daemon does not
            // have (`crate::vocab`, `spike/hotwords_bench.py`).
            "vocab_applied_to_decoder": false,
        })
    }

    fn status_payload(&self) -> anyhow::Result<Value> {
        let c = &self.control;
        let (depth, capacity, dropped_chunks, dropped_samples) = match &c.queue {
            Some(q) => (
                q.queued_samples(),
                q.capacity_samples(),
                q.dropped_chunks(),
                q.dropped_samples(),
            ),
            None => (0, 0, 0, 0),
        };
        let rate = crate::config::SAMPLE_RATE as f64;
        let models = c.models();
        let allowlist = c.allowlist();
        let store = self.store();
        let sources = store.list_sources()?;
        let allowed = sources
            .iter()
            .filter(|s| allowlist.decide(&s.match_key).captures())
            .count();
        let capturing = sources
            .iter()
            .filter(|s| s.streams > 0 && allowlist.decide(&s.match_key).captures())
            .count();
        Ok(json!({
            "daemon": crate::proto::daemon_id(),
            "proto": crate::bus::PROTO,
            "schema": crate::store::SCHEMA_VERSION,
            "uptime_s": c.uptime_s(),
            "paused": c.is_paused(),
            "paused_since_utc_ns": c.paused_since_ns().map(|v| v.to_string()),
            // Whole seconds of audio waiting for the inference thread: the
            // number that means something to a person watching the footer.
            "queue_depth": (depth as f64 / rate).round() as i64,
            "drops": dropped_chunks,
            "queue": {
                "depth_samples": depth,
                "depth_seconds": depth as f64 / rate,
                "capacity_samples": capacity,
                "dropped_buffers": dropped_chunks,
                "dropped_seconds": dropped_samples as f64 / rate,
            },
            "models": models,
            "models_loaded": !models.is_empty(),
            // Semantic search (0.6.5). Always present, always a boolean:
            // a client has to be able to tell "the model is not installed"
            // from "an older daemon", and a missing key cannot.
            "semantic": self.semantic_json(&store)?,
            "sources_allowed": allowed,
            "sources_capturing": capturing,
            // The whole microphone answer in one place, plus the flat string
            // for anything that only wants to print a word (PROTOCOL).
            "mic": c.mic_json(),
            "mic_state": c.mic_state(),
            // Measured by the retention sweeper, never here: this method is
            // polled every three seconds by every open client and the answer
            // costs a walk of the data directory (0.6.1).
            "storage": c.storage_json(),
            // What the last retention sweep did, or `null` before one has run.
            // Also pushed as its own `sweep` event on this topic: a sweep that
            // quietly stopped working — or started destroying files — used to
            // be visible only in the daemon's log (0.7.5, audit finding #22).
            "last_sweep": c.last_sweep_json(),
            // The memory graph's Tier 3 worker (0.7.0). Also pushed as its own
            // `graph` event when it moves; carried here so a client that missed
            // one still converges on the truth, exactly like the mic block.
            "graph": c.graph_state().to_json(),
            // The accuracy round's idle worker (0.8.0).
            "asr": self.asr_quality_json(),
            "segments_total": store.segments_total()?,
            "counters": {
                "sessions_opened": c.stats.sessions_opened.load(Ordering::Relaxed),
                // Turns discarded because the audio had a hole in it (0.7.5,
                // audit finding #21). `drops` says buffers were lost; this says
                // a turn was.
                "gaps_discarded": c.stats.gaps_discarded.load(Ordering::Relaxed),
                "segments_written": c.stats.segments_written.load(Ordering::Relaxed),
                "frames_analysed": c.stats.frames_analysed.load(Ordering::Relaxed),
                "analysed": c.analysis.analysed.load(Ordering::Relaxed),
                "labelled": c.analysis.labelled.load(Ordering::Relaxed),
                "refused_overlap": c.analysis.refused_overlap.load(Ordering::Relaxed),
                "mic_segments": c.analysis.mic_segments.load(Ordering::Relaxed),
                "mic_enrolled": c.analysis.mic_enrolled.load(Ordering::Relaxed),
                "mic_goldens": c.analysis.mic_goldens.load(Ordering::Relaxed),
                // 0.6.1: turns that matched nobody and were too slight to mint
                // a voice, turns that took a name from their neighbours, and
                // the two outcomes of the wrong-language check.
                "too_slight": c.analysis.too_slight.load(Ordering::Relaxed),
                "proximity_labelled": c.analysis.proximity_labelled.load(Ordering::Relaxed),
                "redecoded": c.analysis.redecoded.load(Ordering::Relaxed),
                "lang_mismatch": c.analysis.lang_mismatch.load(Ordering::Relaxed),
                // 0.7.7, the conversational language prior: turns that took
                // their conversation's language rather than staying NULL, turns
                // that read as the opposite of it and were handed to an
                // arbiter, the two re-decode directions split out, and rows
                // `lang.repair` rewrote out of the backlog.
                "context_stamped": c.analysis.context_stamped.load(Ordering::Relaxed),
                "flips_suspected": c.analysis.flips_suspected.load(Ordering::Relaxed),
                "redecoded_de": c.analysis.redecoded_de.load(Ordering::Relaxed),
                "redecoded_en": c.analysis.redecoded_en.load(Ordering::Relaxed),
                "repairs": c.analysis.repairs.load(Ordering::Relaxed),
                // 0.8.0, the idle quality worker: turns re-decoded with their
                // session's audio, turns that had none to re-decode with, and
                // the two verdicts of the cross-check.
                "redecoded_context":
                    c.quality.redecoded_context.load(Ordering::Relaxed),
                "redecode_skipped_no_audio":
                    c.quality.redecode_skipped_no_audio.load(Ordering::Relaxed),
                "solid": c.quality.confidence_solid.load(Ordering::Relaxed),
                "shaky": c.quality.confidence_shaky.load(Ordering::Relaxed),
            },
            "clients": self.bus.client_count(),
            "seq": self.bus.current_seq(),
            "roster_present": store.roster_present()?.len(),
        }))
    }

    /// Push the current status to every subscriber. Called on the transitions
    /// a client cannot infer.
    fn announce_status(&self) {
        match self.status_payload() {
            Ok(payload) => {
                self.bus.publish(Topic::Status, "status", payload);
            }
            Err(e) => warn!("could not build a status event: {e:#}"),
        }
    }

    /// The panic path. Nothing here waits on the pipeline: the flag is set,
    /// the caller is answered, and the very next write the pipeline attempts
    /// is refused.
    fn set_paused(&self, paused: bool) -> Result<Value, Error> {
        let changed = if paused {
            self.control.pause()
        } else {
            self.control.resume()
        };
        if changed {
            info!(
                paused,
                "capture writes {}",
                if paused { "paused" } else { "resumed" }
            );
        }
        // Announced even when nothing changed: a client that asked for a state
        // it was already in still deserves to see the state.
        self.announce_status();
        Ok(json!({"paused": self.control.is_paused(), "changed": changed}))
    }

    // ---- sources ---------------------------------------------------------

    fn sources_list(&self) -> Result<Value, Error> {
        let rows = self.store().list_sources().map_err(Error::from)?;
        let live = self.control.allowlist();
        let mic = self.control.mic();
        Ok(json!({
            "sources": rows
                .iter()
                .map(|r| {
                    // The live answer, which may be ahead of the database for
                    // the instant between a toggle and its mirror. The
                    // microphone's switch is `[mic].enabled`, never a rule.
                    let allowed = if r.kind == crate::store::KIND_MIC {
                        mic.enabled
                    } else {
                        live.decide(&r.match_key).captures()
                    };
                    source_json(r, allowed)
                })
                .collect::<Vec<_>>(),
        }))
    }

    /// Toggle a source live. The capture loop polls the generation counter and
    /// attaches or detaches without a restart; the config file is rewritten so
    /// the decision survives one.
    fn sources_set(&self, req: &Request) -> Result<Value, Error> {
        let match_key = req.str("match_key")?.to_string();
        if match_key.trim().is_empty() {
            return Err(Error::params("match_key must not be empty"));
        }
        // The microphone is a row in `sources` and is deliberately not a rule
        // in the allowlist: an app rule is consent about one program's output,
        // and this device hears the room. Half-enabling it through the wrong
        // method would leave `[rules]` and `[mic]` disagreeing about a consent
        // decision, so the refusal names the method that actually works.
        if match_key == crate::capture::MIC_MATCH_KEY {
            return Err(Error::new(
                "refused",
                "the microphone is not an application rule — use mic.set \
                 {enabled, mode}; it hears the room rather than one program, so \
                 it has its own switch and its own default (off)",
            ));
        }
        let allowed = req
            .opt_bool("allowed")?
            .ok_or_else(|| Error::params("allowed is required"))?;

        self.store()
            .set_allowed(&match_key, allowed, utc_now_ns())
            .map_err(Error::from)?;
        self.control.set_rule(&match_key, allowed);

        let mut persisted = false;
        if let Some(path) = &self.control.config_path {
            match Config::load(path) {
                Ok(mut cfg) => {
                    cfg.set_rule(&match_key, allowed);
                    match cfg.save(path) {
                        Ok(()) => persisted = true,
                        Err(e) => warn!("could not persist the rule for {match_key}: {e:#}"),
                    }
                }
                Err(e) => warn!("could not re-read the config to persist a rule: {e:#}"),
            }
        }

        info!(key = %match_key, allowed, persisted, "source rule changed");
        // The whole row, on the same event the capture thread publishes when a
        // source appears or starts being captured (finding #2): one shape, so a
        // client folds a toggle in exactly as it folds in an arrival. `state`
        // is what the table can say from here — the capture thread announces
        // again a quarter-second later when it has actually attached.
        let row = self.store().source_row(&match_key).map_err(Error::from)?;
        match row {
            Some(row) => {
                let mut data = source_json(&row, allowed);
                data["state"] = json!(if row.streams > 0 {
                    crate::capture::SOURCE_CAPTURING
                } else {
                    crate::capture::SOURCE_SEEN
                });
                self.bus.publish(Topic::Sources, "source", data);
            }
            None => {
                self.bus.publish(
                    Topic::Sources,
                    "source",
                    json!({"match_key": match_key, "allowed": allowed}),
                );
            }
        }
        // The count of capturing sources is part of the status line, so a
        // toggle changes it.
        self.announce_status();
        Ok(json!({"match_key": match_key, "allowed": allowed, "persisted": persisted}))
    }

    // ---- microphone ------------------------------------------------------

    /// The mic block plus the pinned speaker, so a client can render both the
    /// switch and "which voice in the transcript is me" from one call.
    fn mic_get(&self) -> Result<Value, Error> {
        let you = self.store().you_speaker_id().map_err(Error::from)?;
        let mut out = self.control.mic_json();
        out["you_speaker"] = json!(you);
        Ok(out)
    }

    /// The switch. Both fields are optional and applied independently, so
    /// `recalld mic always` can change the mode without also having to know
    /// whether the switch was already on.
    ///
    /// The device pin is config-file-only on purpose: it is a machine setup
    /// decision, not something a click should be able to change under a user
    /// who has deliberately chosen one microphone out of several.
    fn mic_set(&self, req: &Request) -> Result<Value, Error> {
        let enabled = req.opt_bool("enabled")?;
        let mode = match req.opt_str("mode")? {
            None => None,
            Some(s) => Some(crate::config::MicMode::parse(s).ok_or_else(|| {
                Error::params(format!("mode must be \"follow\" or \"always\", not {s:?}"))
            })?),
        };
        if enabled.is_none() && mode.is_none() {
            return Err(Error::params("mic.set needs at least one of enabled, mode"));
        }

        let cfg = self.control.set_mic(enabled, mode);

        // Mirror the switch onto the source row, so `recalld sources` and any
        // client reading `sources.list` agree with `mic.get`.
        if let Err(e) = self.store().upsert_source_kind(
            crate::capture::MIC_MATCH_KEY,
            crate::capture::MIC_DISPLAY_NAME,
            crate::store::KIND_MIC,
            utc_now_ns(),
        ) {
            warn!("could not record the microphone source row: {e:#}");
        }
        if let Err(e) =
            self.store()
                .set_allowed(crate::capture::MIC_MATCH_KEY, cfg.enabled, utc_now_ns())
        {
            warn!("could not mirror the microphone switch onto its source row: {e:#}");
        }

        let mut persisted = false;
        if let Some(path) = &self.control.config_path {
            match Config::load(path) {
                Ok(mut file) => {
                    file.mic.enabled = cfg.enabled;
                    file.mic.mode = cfg.mode;
                    match file.save(path) {
                        Ok(()) => persisted = true,
                        Err(e) => warn!("could not persist the microphone switch: {e:#}"),
                    }
                }
                Err(e) => warn!("could not re-read the config to persist the microphone: {e:#}"),
            }
        }
        info!(
            enabled = cfg.enabled,
            mode = cfg.mode.as_str(),
            persisted,
            "microphone switch changed"
        );

        // Two events, because two things changed: the mic block (which the
        // capture thread will confirm again the moment the stream really opens)
        // and the daemon's overall state.
        let mut payload = self.control.mic_json();
        payload["persisted"] = json!(persisted);
        self.bus.publish(Topic::Status, "mic", payload.clone());
        self.announce_status();
        Ok(payload)
    }

    // ---- speakers --------------------------------------------------------

    /// Refuse a write aimed at a tombstone, naming the voice that holds the
    /// rows. `speakers.split` and `speakers.delete` already did this; the
    /// per-voice edits did not, and silently wrote onto rows nothing reads
    /// (audit finding #8). The caller must already have established that the
    /// id names a speaker at all, so a missing one is `not_found` there rather
    /// than `conflict` here.
    fn tombstone_check(store: &Store, id: i64) -> Result<(), Error> {
        let canonical = store.resolve_speaker(id).map_err(Error::from)?;
        if canonical != id {
            return Err(Error::new(
                "conflict",
                format!(
                    "speaker {id} was merged into {canonical}; use {canonical} instead — \
                     that is the voice holding the rows"
                ),
            ));
        }
        Ok(())
    }

    fn speakers_list(&self) -> Result<Value, Error> {
        let store = self.store();
        let rows = store.list_speakers().map_err(Error::from)?;
        let you = store.you_speaker_id().map_err(Error::from)?;
        drop(store);
        Ok(json!({
            "speakers": rows
                .into_iter()
                .map(|r| json!({
                    "id": r.id,
                    // The user's own voice, pinned by the microphone rather
                    // than matched (PROTOCOL). A client renders it differently
                    // because its label means something different: provenance,
                    // not a score.
                    "you": Some(r.id) == you,
                    // `null` until a person names this voice — the difference
                    // the onboarding flow is built on (DESIGN §5).
                    "name": r.name(),
                    "auto": r.auto_label,
                    // Which languages this voice speaks (schema v5). `null` is
                    // "any", the default and the only state until somebody
                    // says otherwise — it is what turns a wrong-language decode
                    // from an unfixable annoyance into a decidable question.
                    "languages": r.languages,
                    "first_seen": iso8601(r.created_at),
                    "segments": r.segments,
                    "total_ms": ns_to_ms(r.speech_ns),
                    "speech_ns": r.speech_ns.to_string(),
                }))
                .collect::<Vec<_>>(),
        }))
    }

    /// Declare which languages a voice speaks — or clear the declaration.
    ///
    /// `languages` is an array of tags, or `null` / `[]` / `["any"]` for *any*,
    /// which is the default. Only the tags the daemon's classifier knows are
    /// accepted (`de`, `en`): storing anything else would promise a correction
    /// that cannot be made, and a promise the daemon cannot keep is worse than
    /// no setting at all.
    fn speakers_set_languages(&self, req: &Request) -> Result<Value, Error> {
        let id = req.i64("id")?;
        let codes: Vec<String> = match req.param("languages") {
            // Absent or null: "any". Explicit, because it is how a client
            // clears a declaration.
            None => Vec::new(),
            Some(Value::String(one)) => vec![one.clone()],
            Some(Value::Array(items)) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    let Some(code) = item.as_str() else {
                        return Err(Error::params("languages must be an array of strings"));
                    };
                    out.push(code.to_string());
                }
                out
            }
            Some(_) => {
                return Err(Error::params(
                    "languages must be an array of strings, a string, or null",
                ));
            }
        };
        let languages = crate::lang::normalise_languages(&codes).map_err(Error::params)?;

        let store = self.store();
        let prior = store.speaker_languages(id).map_err(Error::from)?;
        let name = store
            .speaker_name(id)
            .map_err(Error::from)?
            .ok_or_else(|| Error::not_found(format!("no speaker with id {id}")))?;
        // Worse than the rename (finding #8): `speaker_languages` READS through
        // the tombstone to the canonical voice while `set_speaker_languages`
        // WRITES the tombstone, so this wrote nowhere, reported success, and
        // logged the canonical voice's languages as the prior state — the audit
        // trail lied too. Refused, like the split.
        Self::tombstone_check(&store, id)?;
        store
            .set_speaker_languages(id, languages.as_deref())
            .map_err(Error::from)?;
        store
            .log_operation(
                "speakers.set_languages",
                &json!([id]).to_string(),
                &json!({"id": id, "languages": prior}).to_string(),
                utc_now_ns(),
            )
            .map_err(Error::from)?;
        let summary = store.speaker_summary(id).map_err(Error::from)?;
        drop(store);

        // On the existing `relabel` event, carrying the *name* as well: a
        // client folds one shape into its speaker row and must not be made to
        // choose between applying the languages and keeping the name.
        let seq = self.bus.publish(
            Topic::Relabel,
            "relabel",
            json!({
                "speaker": id,
                "name": summary.as_ref().and_then(|s| s.name()),
                "languages": languages,
            }),
        );
        info!(speaker = id, ?languages, "speaker languages set");
        let _ = name;
        Ok(json!({"id": id, "languages": languages, "seq": seq}))
    }

    /// The one-off voices sweep (0.6.1): list, or delete.
    ///
    /// A grunt that slipped past the mint bar — or one minted before the bar
    /// existed — leaves a voice with a single segment and a second of speech
    /// that nobody will ever name. This finds them and, with `apply`, removes
    /// them the way `delete.run` removes anything: the segments are
    /// soft-deleted so the undo window still applies, and the identity itself
    /// goes with its prototypes and goldens.
    ///
    /// It refuses to touch the pinned "You" speaker or any voice the user has
    /// named, whatever the counts say. A name is a person saying "this one
    /// matters", and a sweep must never argue with that.
    fn speakers_prune(&self, req: &Request) -> Result<Value, Error> {
        let apply = req.opt_bool("apply")?.unwrap_or(false);
        let store = self.store();
        let you = store.you_speaker_id().map_err(Error::from)?;
        let candidates: Vec<crate::store::SpeakerSummary> = store
            .prune_candidates(PRUNE_MAX_SEGMENTS, PRUNE_MAX_SPEECH_NS)
            .map_err(Error::from)?
            .into_iter()
            .filter(|s| Some(s.id) != you)
            .collect();
        let preview: Vec<Value> = candidates
            .iter()
            .map(|s| {
                json!({
                    "id": s.id,
                    "auto": s.auto_label,
                    "name": s.name(),
                    "segments": s.segments,
                    "total_ms": ns_to_ms(s.speech_ns),
                    "speech_ns": s.speech_ns.to_string(),
                })
            })
            .collect();

        if !apply {
            drop(store);
            return Ok(json!({
                "apply": false,
                "count": preview.len(),
                "voices": preview,
                "max_segments": PRUNE_MAX_SEGMENTS,
                "max_speech_ms": PRUNE_MAX_SPEECH_NS / 1_000_000,
            }));
        }

        let at = utc_now_ns();
        let mut removed: Vec<i64> = Vec::new();
        let mut segments = 0usize;
        let mut files: Vec<String> = Vec::new();
        let mut purged_ids: Vec<i64> = Vec::new();
        for voice in &candidates {
            match store.prune_speaker(voice.id, at) {
                Ok(report) => {
                    segments += report.soft_deleted.len();
                    purged_ids.extend(report.soft_deleted.iter().copied());
                    files.extend(report.goldens.clone());
                    store
                        .log_operation(
                            "speakers.prune",
                            &json!([voice.id]).to_string(),
                            &json!({
                                "id": voice.id,
                                "auto_label": voice.auto_label,
                                "segments": report.segments,
                                "prototypes": report.prototypes,
                                "goldens": report.goldens,
                            })
                            .to_string(),
                            at,
                        )
                        .map_err(Error::from)?;
                    removed.push(voice.id);
                }
                Err(e) => warn!(speaker = voice.id, "could not prune a voice: {e:#}"),
            }
        }
        drop(store);

        // The rows are gone, so the files may go too — in that order, because a
        // file with no row is residue the reconciliation sweep understands.
        for rel in files {
            if !rel.is_empty() {
                let _ = std::fs::remove_file(self.control.data_dir.join(rel));
            }
        }
        // The swept voices' segments are soft-DELETED, not merely unlabelled —
        // without these purge events every open transcript kept showing the
        // deleted rows as "unknown voice" (audit finding #4; the GUI comment
        // always promised they'd arrive, and the mock even sent them).
        for batch in purged_ids.chunks(DELETE_BATCH) {
            self.bus
                .publish(Topic::Segments, "purge", json!({"ids": batch}));
        }
        for id in &removed {
            // A tombstone-less disappearance: the voice never existed as far as
            // any view should now be concerned.
            self.bus.publish(
                Topic::Relabel,
                "relabel",
                json!({"speaker": id, "name": Value::Null, "pruned": true}),
            );
        }
        info!(
            voices = removed.len(),
            segments, "swept one-off voices out of the voicebank"
        );
        self.announce_status();
        // `voices` is what was **removed**, not what was previewed (audit
        // finding #25). `count` only ever counted the successes, so a voice
        // that failed to prune used to be reported as gone by one field and
        // present by the other — and the client had already shown the preview
        // in its confirmation dialog, so repeating it bought nothing.
        let removed_voices: Vec<Value> = preview
            .iter()
            .filter(|v| {
                v.get("id")
                    .and_then(Value::as_i64)
                    .is_some_and(|id| removed.contains(&id))
            })
            .cloned()
            .collect();
        Ok(json!({
            "apply": true,
            "count": removed.len(),
            "removed": removed,
            "segments": segments,
            "voices": removed_voices,
        }))
    }

    /// Delete one voice — DESIGN §8's choice, finally made askable.
    ///
    /// §8 always specified two halves and only one was ever built: the segments
    /// went and the *bank entry* stayed, with its prototypes intact, so the
    /// voice went on matching new audio while its row sat in the list at
    /// "0 segments · 0s" and every further Delete matched nothing and did
    /// nothing. The missing half is the parameter:
    ///
    /// * `keep_voiceprint: true` — "keep the bank entry (still labeled going
    ///   forward)". The conversations go; the identity stays and keeps
    ///   matching. The reply says so, because a delete that leaves something
    ///   behind has to admit it.
    /// * `keep_voiceprint: false` (the default, and what the CLI does without
    ///   `--keep-voiceprint`) — "nuke it so they re-enroll fresh": prototypes,
    ///   this voice's embeddings, its goldens and their files, and the speaker
    ///   row itself.
    ///
    /// A voice with **no live segments** is the case this method exists for,
    /// so it is explicitly not an error: it succeeds and removes the ghost.
    ///
    /// Two refusals, both `refused`, both deliberate:
    ///
    /// * **The pinned "You" voice.** Deleting your own identity would not stop
    ///   you being recorded — the microphone would mint a fresh pin on the next
    ///   turn — so it is a destructive act that does not do what it looks like.
    ///   The switch that actually stops it is the microphone, and the refusal
    ///   names it. (Your *words* are still deletable: `delete.run` by date or
    ///   session takes them like anyone else's.)
    /// * **A merge target**, on the nuke path only. Other voices are tombstoned
    ///   onto this row; removing it would dangle every one of them, and
    ///   `speakers.merged_into` is a real foreign key, so the write would fail
    ///   anyway. The message names the count and points at the half that does
    ///   work — keeping the voiceprint still takes the conversations. It is the
    ///   same rule `prune_candidates` already applies for the same reason.
    fn speakers_delete(&self, req: &Request) -> Result<Value, Error> {
        let id = req.i64("id")?;
        let keep = req.opt_bool("keep_voiceprint")?.unwrap_or(false);
        let store = self.store();

        if store.speaker_name(id).map_err(Error::from)?.is_none() {
            return Err(Error::not_found(format!("no speaker with id {id}")));
        }
        // A tombstone owns no rows: deleting it would silently take the voice
        // it was merged into. Same shape of refusal `speakers.split` gives.
        let canonical = store.resolve_speaker(id).map_err(Error::from)?;
        if canonical != id {
            return Err(Error::new(
                "conflict",
                format!(
                    "speaker {id} was merged into {canonical}; delete {canonical} instead — \
                     that is the voice holding the rows"
                ),
            ));
        }
        let Some(summary) = store.speaker_summary(id).map_err(Error::from)? else {
            return Err(Error::not_found(format!("no speaker with id {id}")));
        };
        if store.you_speaker_id().map_err(Error::from)? == Some(id) {
            return Err(Error::new(
                "refused",
                "that is your own voice, pinned by your microphone rather than matched. \
                 Deleting it would not stop you being recorded — the next turn through the \
                 mic would mint the pin again. Turn the microphone off (mic.set {enabled: \
                 false}) to stop recording yourself; to remove what you have already said, \
                 delete by date or session instead",
            ));
        }
        if !keep {
            let tombstones = store.merge_tombstones(id).map_err(Error::from)?;
            if !tombstones.is_empty() {
                let n = tombstones.len();
                return Err(Error::new(
                    "refused",
                    format!(
                        "speaker {id} is a merge target: {n} other voice(s) were merged into \
                         it and point at it. Removing the voiceprint would leave them \
                         dangling. Delete with keep_voiceprint: true — that still takes every \
                         conversation — or split the voice apart first"
                    ),
                ));
            }
        }

        // Everything an audit would need to reconstruct what was here, read
        // before anything moves.
        let languages = store.speaker_languages(id).map_err(Error::from)?;
        let prototypes_before = store.prototype_count(id).map_err(Error::from)?;
        let goldens_before = store.golden_samples_for(id).map_err(Error::from)?;
        let at = utc_now_ns();
        let report = store
            .delete_speaker(id, keep, at)
            .map_err(|e| Error::new("conflict", format!("{e:#}")))?;

        // The segment ids go into the log the way `delete.run` writes them —
        // one row per batch — so one operations row never has to carry a
        // hundred thousand ids, and the identity itself gets a row of its own.
        for batch in report.soft_deleted.chunks(DELETE_BATCH) {
            store
                .log_operation(
                    "speakers.delete",
                    &serde_json::to_string(batch).unwrap_or_else(|_| "[]".into()),
                    &json!({"speaker": id, "deleted_at": at, "soft": true}).to_string(),
                    at,
                )
                .map_err(Error::from)?;
        }
        store
            .log_operation(
                "speakers.delete",
                &json!([id]).to_string(),
                &json!({
                    "id": id,
                    "display_name": summary.name(),
                    "auto_label": summary.auto_label,
                    "languages": languages,
                    "created_at": summary.created_at,
                    "keep_voiceprint": keep,
                    "segments": report.segments.len(),
                    "soft_deleted": report.soft_deleted.len(),
                    "prototypes": prototypes_before,
                    "embeddings": report.embeddings,
                    "goldens": goldens_before
                        .iter()
                        .map(|g| g.audio_path.clone())
                        .collect::<Vec<_>>(),
                    "threads": report.threads,
                    "removed_speaker": report.removed_speaker,
                })
                .to_string(),
                at,
            )
            .map_err(Error::from)?;
        drop(store);

        // Rows first, then files: a file with no row is residue the
        // reconciliation sweep understands, a row with no file is a lie.
        for rel in &report.goldens {
            if !rel.is_empty() {
                let _ = std::fs::remove_file(self.control.data_dir.join(rel));
            }
        }

        // Named rows, batched: a client drops exactly these without re-querying
        // a whole transcript, and one frame never has to carry every id.
        for batch in report.soft_deleted.chunks(DELETE_BATCH) {
            self.bus
                .publish(Topic::Segments, "purge", json!({"ids": batch}));
        }
        let name = summary.name().map(str::to_string);
        let seq = if report.removed_speaker {
            // The same shape a sweep uses: not a merge — nothing moved
            // anywhere, the id simply stops existing.
            self.bus.publish(
                Topic::Relabel,
                "relabel",
                json!({"speaker": id, "name": Value::Null, "pruned": true}),
            )
        } else {
            // The voice is still there and still matching; only its counts
            // moved, and the `purge` above already told every view by how much.
            self.bus.publish(
                Topic::Relabel,
                "relabel",
                json!({"speaker": id, "name": name, "languages": languages}),
            )
        };

        info!(
            speaker = id,
            keep_voiceprint = keep,
            segments = report.soft_deleted.len(),
            "deleted a voice"
        );
        self.announce_status();
        let msg = if report.removed_speaker {
            format!(
                "{} conversation(s) deleted and the voiceprint removed — this voice has to \
                 enrol again from scratch before it is recognised.",
                report.soft_deleted.len()
            )
        } else {
            format!(
                "{} conversation(s) deleted. The voiceprint was kept: this voice stays in the \
                 bank and will still be labelled going forward.",
                report.soft_deleted.len()
            )
        };
        Ok(json!({
            "id": id,
            "name": name,
            // The generated label, so a caller that has not got the speakers
            // list can still say which voice this was — an unnamed voice is
            // the common case here, and "Speaker 7" is not what it is called.
            "auto": summary.auto_label,
            "keep_voiceprint": keep,
            "removed_speaker": report.removed_speaker,
            "segments": report.soft_deleted.len(),
            "total_segments": report.segments.len(),
            "prototypes": report.prototypes,
            "embeddings": report.embeddings,
            "goldens": report.goldens.len(),
            "threads": report.threads,
            "msg": msg,
            "seq": seq,
        }))
    }

    fn speakers_name(&self, req: &Request) -> Result<Value, Error> {
        let id = req.i64("id")?;
        let name = req.str("name")?.trim().to_string();
        if name.is_empty() {
            return Err(Error::params("name must not be empty"));
        }
        let store = self.store();
        let prior = store
            .speaker_name(id)
            .map_err(Error::from)?
            .ok_or_else(|| Error::not_found(format!("no speaker with id {id}")))?;
        // A tombstone holds no rows: naming it writes a display name nothing
        // ever reads (every path resolves through `speaker_resolved`), while
        // the reply and the `relabel` event both claim the name took — so a
        // client draws a ghost voice that does not exist (audit finding #8).
        // Same refusal `speakers.split` gives, for the same reason.
        Self::tombstone_check(&store, id)?;
        store
            .rename_speaker(id, &name, utc_now_ns())
            .map_err(Error::from)?;
        store
            .log_operation(
                "speakers.name",
                &json!([id]).to_string(),
                &json!({"id": id, "display_name": prior}).to_string(),
                utc_now_ns(),
            )
            .map_err(Error::from)?;
        drop(store);

        let seq = self.bus.publish(
            Topic::Relabel,
            "relabel",
            json!({"speaker": id, "name": name}),
        );
        Ok(json!({"id": id, "name": name, "seq": seq}))
    }

    fn speakers_merge(&self, req: &Request) -> Result<Value, Error> {
        let from = req.i64("from")?;
        let into = req.i64("into")?;
        let store = self.store();
        let prior_from = store.speaker_name(from).map_err(Error::from)?;
        let prior_into = store.speaker_name(into).map_err(Error::from)?;
        if prior_from.is_none() {
            return Err(Error::not_found(format!("no speaker with id {from}")));
        }
        if prior_into.is_none() {
            return Err(Error::not_found(format!("no speaker with id {into}")));
        }
        let report = store
            .merge_speakers(from, into)
            .map_err(|e| Error::new("conflict", format!("{e:#}")))?;
        // Enough to undo: which rows moved, and what the tombstone was called.
        store
            .log_operation(
                "speakers.merge",
                &json!([from, into]).to_string(),
                &json!({
                    "from": report.from,
                    "into": report.into,
                    "from_display_name": prior_from,
                    "segments": report.segments,
                    "prototypes": report.prototypes,
                    "golden_samples": report.golden_samples,
                    "tombstones_repointed": report.tombstones_repointed,
                })
                .to_string(),
                utc_now_ns(),
            )
            .map_err(Error::from)?;
        let canonical = store.speaker_summary(report.into).map_err(Error::from)?;
        drop(store);
        let canonical_name = canonical
            .as_ref()
            .and_then(|s| s.name().map(str::to_string));

        // Two events, because two things are true: the tombstoned id now lives
        // at `into` (every view moves its rows), and `into` itself is the row
        // whose counts just changed.
        let seq = self.bus.publish(
            Topic::Relabel,
            "relabel",
            json!({
                "speaker": report.from,
                "name": canonical_name,
                "merged_into": report.into,
            }),
        );
        self.bus.publish(
            Topic::Relabel,
            "relabel",
            json!({"speaker": report.into, "name": canonical_name}),
        );
        Ok(json!({
            "from": report.from,
            "into": report.into,
            "segments": report.segments,
            "prototypes": report.prototypes,
            "golden_samples": report.golden_samples,
            "tombstones_repointed": report.tombstones_repointed,
            "seq": seq,
        }))
    }

    /// Cut a voice that turned out to be two people back apart.
    ///
    /// The decision lives in `split::plan` and is a refusal by default: one
    /// voice cut in half is as damaging as the false merge this undoes
    /// (FINDINGS §5), so a re-cluster that cannot show two distinct centroids
    /// changes nothing. Every write is reported — as a `relabel` for each of
    /// the two voices and a `segment` for every row that moved — because a
    /// split is the one correction where rows change *identity*, and a view
    /// that missed it would show the wrong person's words.
    ///
    /// PROTOCOL calls this an async op. The re-cluster is a brute-force pass
    /// over one speaker's vectors — thousands at the very most, sub-millisecond
    /// (DESIGN §6) — so it finishes inline; the op handle and its terminal
    /// event are still issued, so a client written to the async contract sees
    /// what it expects.
    fn speakers_split(self: &Arc<Self>, req: &Request) -> Result<Value, Error> {
        let id = req.i64("id")?;
        let store = self.store();
        if store.speaker_name(id).map_err(Error::from)?.is_none() {
            return Err(Error::not_found(format!("no speaker with id {id}")));
        }
        // A tombstone has no rows of its own: splitting it would silently cut
        // up the speaker it was merged into. Splitting *that* speaker — the
        // merge target — is exactly the intended use.
        let canonical = store.resolve_speaker(id).map_err(Error::from)?;
        if canonical != id {
            return Err(Error::new(
                "conflict",
                format!(
                    "speaker {id} was merged into {canonical}; split {canonical} instead — \
                     that is the voice holding the rows"
                ),
            ));
        }

        let Some(model) = store.speaker_embed_model(id).map_err(Error::from)? else {
            return Err(Error::new(
                "refused",
                format!("speaker {id} has no embeddings, so there is nothing to re-cluster"),
            ));
        };
        let vectors = store.speaker_vectors(id, &model).map_err(Error::from)?;
        let plan = match crate::split::plan(&self.control.identity, &vectors) {
            Ok(plan) => plan,
            Err(refusal) => {
                info!(speaker = id, "split refused: {}", refusal.message());
                return Err(Error::new(refusal.code(), refusal.message()));
            }
        };

        // Read the prior labels before anything moves: `prior_state` has to be
        // enough to put every segment back on the speaker it came from.
        let touched: Vec<i64> = plan
            .moved_segments
            .iter()
            .chain(&plan.ambiguous_segments)
            .map(|(seg, _)| *seg)
            .collect();
        let prior = store.segment_labels(&touched).map_err(Error::from)?;
        let write = crate::store::SplitWrite {
            prototypes: plan.moved_prototypes.clone(),
            segments: plan.moved_segments.clone(),
            ambiguous: plan.ambiguous_segments.clone(),
        };
        let at = utc_now_ns();
        let report = store
            .split_speaker(id, &write, at)
            .map_err(|e| Error::new("conflict", format!("{e:#}")))?;
        store
            .log_operation(
                "speakers.split",
                &json!([report.kept, report.minted]).to_string(),
                &json!({
                    "kept": report.kept,
                    "minted": report.minted,
                    "embed_model_id": model,
                    "centroid_similarity": plan.centroid_similarity,
                    "prototypes": plan.moved_prototypes,
                    // Per segment, the speaker and score it had before this
                    // ran: the whole point of the audit trail (DESIGN §6).
                    "segments": prior
                        .iter()
                        .map(|row| json!({
                            "segment_id": row.segment_id,
                            "speaker_id": row.speaker_id,
                            "match_score": row.match_score,
                        }))
                        .collect::<Vec<_>>(),
                })
                .to_string(),
                at,
            )
            .map_err(Error::from)?;

        let kept_name = store
            .speaker_summary(report.kept)
            .map_err(Error::from)?
            .and_then(|s| s.name().map(str::to_string));
        // Both the moved rows and the softened ones: an undecidable segment
        // kept its speaker but not its confidence, and a view showing scores
        // has to see that too.
        let changed: Vec<SegmentRow> = touched
            .iter()
            .filter_map(|seg| store.segment_row(*seg).ok().flatten())
            .collect();
        drop(store);

        // The existing voice first: the minted one's rows are about to arrive
        // and a client should already know both ids exist.
        let seq = self.bus.publish(
            Topic::Relabel,
            "relabel",
            json!({"speaker": report.kept, "name": kept_name}),
        );
        self.bus.publish(
            Topic::Relabel,
            "relabel",
            json!({
                "speaker": report.minted,
                "name": Value::Null,
                "auto": report.auto_label,
                // The mirror of a merge's `merged_into`: this id exists because
                // that one was cut in two.
                "split_from": report.kept,
            }),
        );
        // A speaker with more rows than the replay buffer would cost every
        // client its connection to announce one by one; past the cap the two
        // relabels stand and the reply says a re-query is needed.
        let resync = changed.len() > SPLIT_EVENT_CAP;
        if !resync {
            for row in &changed {
                self.bus
                    .publish(Topic::Segments, "segment", segment_json(row));
            }
        }

        let op = format!("op_{}", self.next_op.fetch_add(1, Ordering::SeqCst));
        self.finish_op(&op, OpState::Done);
        let result = json!({
            "op": op,
            "kept": report.kept,
            "minted": report.minted,
            "auto": report.auto_label,
            "moved_segments": report.segments,
            "moved_prototypes": report.prototypes,
            "ambiguous": report.ambiguous,
            "centroid_similarity": plan.centroid_similarity,
            "embed_model_id": model,
            // True when the row-level events were suppressed: re-run your
            // queries rather than trusting what you have.
            "resync": resync,
            "seq": seq,
        });
        info!(
            kept = report.kept,
            minted = report.minted,
            segments = report.segments,
            ambiguous = report.ambiguous,
            similarity = plan.centroid_similarity,
            "split a voice in two"
        );
        let mut done = result.clone();
        done["kind"] = json!("speakers.split");
        self.bus.publish(Topic::Ops, "op.done", done);
        Ok(result)
    }

    /// "Play me this voice." The clips a person would need to answer *who is
    /// this?*, so the naming flow does not have to page through a transcript
    /// hunting for a segment that still has audio.
    ///
    /// Only clips whose WAV is actually on disk come back: retention blanks
    /// `audio_path` when it forgets a segment, but a file can also go missing
    /// under the daemon, and a sample list whose entries answer `gone` when
    /// played is worse than a short list.
    fn speakers_sample(&self, req: &Request) -> Result<Value, Error> {
        let id = req.i64("id")?;
        let limit = req.usize_or("limit", SAMPLE_LIMIT)?.clamp(1, 20);
        let store = self.store();
        if store.speaker_name(id).map_err(Error::from)?.is_none() {
            return Err(Error::not_found(format!("no speaker with id {id}")));
        }
        // Over-fetch: the file check below drops rows, and asking for exactly
        // `limit` would hand back a short list whenever one clip has gone.
        let candidates = store
            .speaker_sample_candidates(id, (limit * 8).clamp(limit, 200))
            .map_err(Error::from)?;
        drop(store);

        let mut samples = Vec::with_capacity(limit);
        for c in candidates {
            if samples.len() == limit {
                break;
            }
            if !self.control.data_dir.join(&c.audio_path).is_file() {
                continue;
            }
            samples.push(json!({
                "segment_id": c.segment_id,
                "t_ms": ns_to_ms(c.t_start_ns),
                "t_ns": c.t_start_ns.to_string(),
                "duration_ms": ns_to_ms(c.t_end_ns - c.t_start_ns),
                "text": c.text,
                "match_score": c.match_score,
            }));
        }
        Ok(json!({"id": id, "samples": samples}))
    }

    // ---- the memory graph, Tier 1 (docs/GRAPH.md) ------------------------

    /// Everything the person page needs about one voice, in one round trip.
    ///
    /// Deliberately one method rather than five: the page is a single question
    /// ("who is this, and who do they talk to?") and answering it in pieces
    /// would let a client render half a person while the other half is still in
    /// flight. Nothing here is stored — every number is a query over live
    /// segments, so a deleted segment stops counting immediately (DESIGN §0).
    fn person_get(&self, req: &Request) -> Result<Value, Error> {
        let id = req.i64("id")?;
        let store = self.store();
        let speaker = store
            .speaker_summary(id)
            .map_err(Error::from)?
            .ok_or_else(|| Error::not_found(format!("no speaker with id {id}")))?;
        let you = store.you_speaker_id().map_err(Error::from)?;
        let totals = store.person_totals(id).map_err(Error::from)?;
        let edges = store.person_edges(id).map_err(Error::from)?;
        let threads = store
            .person_threads(id, PERSON_THREADS)
            .map_err(Error::from)?;
        // Participant *names*, resolved here rather than in the client: the
        // page lists people who may not be in the client's speaker list at all
        // (a merged-away id, a voice minted since the last query).
        let mut names: HashMap<i64, Value> = HashMap::new();
        for t in &threads {
            for p in &t.participants {
                if let std::collections::hash_map::Entry::Vacant(slot) = names.entry(*p) {
                    let summary = store.speaker_summary(*p).map_err(Error::from)?;
                    slot.insert(json!({
                        "speaker_id": p,
                        "name": summary.as_ref().and_then(|s| s.name()),
                        "auto": summary.as_ref().map(|s| s.auto_label.clone()),
                    }));
                }
            }
        }
        drop(store);

        Ok(json!({
            "id": speaker.id,
            "speaker": {
                "id": speaker.id,
                "you": Some(speaker.id) == you,
                "name": speaker.name(),
                "auto": speaker.auto_label,
                "languages": speaker.languages,
                "first_seen": iso8601(speaker.created_at),
            },
            // The same tags `speakers.list` carries, lifted to the top level
            // because the page has a chip for them and should not have to know
            // they live on the speaker row.
            "languages": speaker.languages,
            "totals": {
                "segments": totals.segments,
                "speech_ms": ns_to_ms(totals.speech_ns),
                "speech_ns": totals.speech_ns.to_string(),
                "sessions": totals.sessions,
                "threads": totals.threads,
                "first_heard_ms": totals.first_ns.map(ns_to_ms),
                "first_heard_ns": totals.first_ns.map(|v| v.to_string()),
                "last_heard_ms": totals.last_ns.map(ns_to_ms),
                "last_heard_ns": totals.last_ns.map(|v| v.to_string()),
            },
            "edges": edges
                .iter()
                .map(|e| json!({
                    "speaker_id": e.speaker_id,
                    "name": e.named_at.map(|_| e.display_name.clone()),
                    "auto": e.auto_label,
                    "threads": e.threads,
                    // Seconds, as the brief's shape asks; `speech_ms` is the
                    // same number a client can render without arithmetic.
                    "seconds": e.speech_ns as f64 / 1e9,
                    "speech_ms": ns_to_ms(e.speech_ns),
                    "last_ns": e.last_ns.to_string(),
                    "last_ms": ns_to_ms(e.last_ns),
                    "roster_seconds": e.roster_ns.map(|ns| ns as f64 / 1e9),
                }))
                .collect::<Vec<_>>(),
            "recent_threads": threads
                .iter()
                .map(|t| json!({
                    "thread_id": t.id,
                    "session": t.session_id,
                    "started_ns": t.started_ns.to_string(),
                    "started_ms": ns_to_ms(t.started_ns),
                    "ended_ns": t.ended_ns.to_string(),
                    "ended_ms": ns_to_ms(t.ended_ns),
                    "segments": t.segments,
                    "participants": t.participants
                        .iter()
                        .map(|p| names.get(p).cloned().unwrap_or(json!({"speaker_id": p})))
                        .collect::<Vec<_>>(),
                    "preview": t.preview,
                }))
                .collect::<Vec<_>>(),
        }))
    }

    /// One conversation, in order. The rows are the ordinary segment shape, so
    /// a client renders a thread with the code it already has for a transcript.
    fn thread_get(&self, req: &Request) -> Result<Value, Error> {
        let id = req.i64("id")?;
        let store = self.store();
        let summary = store
            .thread_summary(id)
            .map_err(Error::from)?
            .ok_or_else(|| Error::not_found(format!("no thread with id {id}")))?;
        let rows = store.thread_rows(id).map_err(Error::from)?;
        // A conversation whose every turn has been deleted is not a
        // conversation with nothing in it — it is gone, and saying so is the
        // only honest answer (audit finding #15). The sweeper removes the row
        // itself; until it runs, this is what a link to it answers.
        if rows.is_empty() {
            return Err(Error::not_found(format!(
                "thread {id} has no segments left; it was deleted"
            )));
        }
        let mut participants = Vec::with_capacity(summary.participants.len());
        for p in &summary.participants {
            let summary = store.speaker_summary(*p).map_err(Error::from)?;
            participants.push(json!({
                "speaker_id": p,
                "name": summary.as_ref().and_then(|s| s.name()),
                "auto": summary.as_ref().map(|s| s.auto_label.clone()),
            }));
        }
        drop(store);
        Ok(json!({
            "thread_id": summary.id,
            "session": summary.session_id,
            "started_ns": summary.started_ns.to_string(),
            "started_ms": ns_to_ms(summary.started_ns),
            "ended_ns": summary.ended_ns.to_string(),
            "ended_ms": ns_to_ms(summary.ended_ns),
            "participants": participants,
            "preview": summary.preview,
            "segments": rows.iter().map(segment_json).collect::<Vec<_>>(),
        }))
    }

    // ---- the memory graph, Tiers 2 and 3 (0.7.0, docs/GRAPH.md) ----------

    /// One commitment on the wire.
    ///
    /// Two things a client must render and must not conflate. `source` says
    /// which tier claimed this — `"rules"` is a pattern match and a guess,
    /// `"llm"` is the local model under a verdict-first grammar — and `state`
    /// says what a person has decided about it. Nothing here has ever been
    /// acted on: `candidate` means the daemon noticed something, and only a
    /// human click moves it.
    fn commitment_json(&self, c: &crate::store::CommitmentRow) -> Value {
        commitment_json(c)
    }
}

/// One commitment on the wire. A free function since 0.8.0 so `person.brief`
/// ([`crate::brief`]) can render the same shape without going through the
/// service — two spellings of a commitment is one spelling too many.
pub fn commitment_json(c: &crate::store::CommitmentRow) -> Value {
    json!({
            "id": c.id,
            "segment": c.segment_id,
            "thread": c.thread_id,
            // `name` is null until somebody names the voice; `auto` is always
            // there. The same split as every other place a person appears.
            "who": {"speaker_id": c.who_speaker_id, "name": c.who_name, "auto": c.who_auto},
            "to": c.to_speaker_id.map(|_| json!({
                "speaker_id": c.to_speaker_id, "name": c.to_name, "auto": c.to_auto,
            })),
            "what": c.what,
            // The line the claim is about, so a person can disagree with it
            // without leaving the view.
            "said": c.said,
            "due_ms": c.due_utc_ns.map(ns_to_ms),
            "due_ns": c.due_utc_ns.map(|v| v.to_string()),
            // The phrase as spoken. A resolved date nobody can trace back to a
            // word is not evidence of anything.
            "due_raw": c.due_raw,
            "due_kind": c.due_kind,
            "state": c.state,
            "source": c.source,
            "model_id": c.model_id,
            "confidence": c.confidence,
            "t_ms": ns_to_ms(c.t_start_ns),
            "t_ns": c.t_start_ns.to_string(),
            "created_ms": ns_to_ms(c.created_at),
            "updated_ms": ns_to_ms(c.updated_at),
    })
}

impl Service {
    /// Everything the Memory view needs to paint itself once, in one round trip
    /// — the same argument the person page makes.
    fn graph_summary(&self) -> Result<Value, Error> {
        let counts = self.store().graph_counts().map_err(Error::from)?;
        let cfg = self.control.graph();
        Ok(json!({
            "counts": {
                "time_refs": counts.time_refs,
                "commitments": counts.commitments,
                "open": counts.open,
                "candidates": counts.candidates,
                "confirmed": counts.confirmed,
                "done": counts.done,
                "dismissed": counts.dismissed,
                "from_rules": counts.from_rules,
                "from_llm": counts.from_llm,
                "topics": counts.topics,
                "threads": counts.threads,
                "threads_enriched": counts.threads_enriched,
                "threads_pending": counts.threads_pending,
            },
            "enrichment": self.control.graph_state().to_json(),
            "config": self.graph_config_json(&cfg),
        }))
    }

    /// The Tier 3 settings, plus the two facts a client needs to explain them:
    /// whether the model is on disk at all, and what it weighs.
    fn graph_config_json(&self, cfg: &crate::config::GraphConfig) -> Value {
        let installed = self
            .control
            .models_root
            .as_deref()
            .map(|root| crate::models::GraphModels::resolve(root, cfg))
            .map(|g| g.present())
            .unwrap_or(false);
        json!({
            "enabled": cfg.enabled,
            "installed": installed,
            "llm_threads": cfg.llm_threads,
            // The range the setting is clamped to, on the wire for the same
            // reason `download_bytes` is: a client builds its stepper out of
            // what the daemon will accept rather than out of a number in its
            // own source that can drift away from this one.
            "llm_threads_min": *crate::control::GRAPH_THREADS.start(),
            "llm_threads_max": *crate::control::GRAPH_THREADS.end(),
            "gpu_layers": cfg.gpu_layers,
            "llm_model": cfg.llm_model,
            "thread_gap_s": cfg.thread_gap_s,
            "batch_threads": cfg.batch_threads,
            "min_thread_segments": cfg.min_thread_segments,
            // What turning it on actually costs, so the copy in a client does
            // not have to hard-code numbers that could drift.
            "download_bytes": crate::models::total_download_bytes(&[crate::models::Group::Graph])
                - crate::models::total_download_bytes(&[]),
        })
    }

    fn graph_get(&self) -> Result<Value, Error> {
        let cfg = self.control.graph();
        Ok(json!({
            "config": self.graph_config_json(&cfg),
            "enrichment": self.control.graph_state().to_json(),
        }))
    }

    /// Change the Tier 3 settings, live and persisted.
    ///
    /// Live because the switch is in the UI and a switch that needs a restart
    /// is not a switch; persisted because a switch that forgets is worse.
    ///
    /// `llm_threads` is clamped to [`crate::control::GRAPH_THREADS`] rather
    /// than refused, like every other tuning number here, and the reply says
    /// what is now true. It takes effect on the **next** model call: threads
    /// are an argument to a `llama-cli` invocation, and a conversation already
    /// being read finishes at the width it started with.
    fn graph_set(&self, req: &Request) -> Result<Value, Error> {
        let enabled = req.opt_bool("enabled")?;
        let llm_threads = req
            .opt_i64("llm_threads")?
            .map(|v| v.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32);
        let gpu_layers = req.opt_i64("gpu_layers")?.map(|v| v as i32);
        if enabled.is_none() && llm_threads.is_none() && gpu_layers.is_none() {
            return Err(Error::params(
                "graph.set needs at least one of enabled, llm_threads, gpu_layers",
            ));
        }
        self.apply_graph(enabled, llm_threads, gpu_layers)
    }

    /// The one place the Tier 3 settings actually move, so `graph.set` and
    /// `graph.enrich` cannot drift into behaving differently.
    fn apply_graph(
        &self,
        enabled: Option<bool>,
        llm_threads: Option<i32>,
        gpu_layers: Option<i32>,
    ) -> Result<Value, Error> {
        if let Some(e) = enabled {
            self.control.set_graph_enabled(e);
            if !e {
                // Say "off" now rather than in twenty seconds. The worker reads
                // the switch between conversations and is authoritative about
                // every other phase, but this one it cannot contradict: the
                // switch IS off, and a client that flipped it must not be shown
                // "idle" until the next tick.
                self.control.set_graph_state(crate::enrich::GraphState {
                    phase: crate::enrich::Phase::Off,
                    reason: None,
                    thread_id: None,
                    batch_done: 0,
                    batch_total: 0,
                    ..self.control.graph_state()
                });
            }
        }
        let cfg = self.control.set_graph_tuning(llm_threads, gpu_layers);

        let mut persisted = false;
        if let Some(path) = &self.control.config_path {
            match Config::load(path) {
                Ok(mut file) => {
                    file.graph.enabled = cfg.enabled;
                    file.graph.llm_threads = cfg.llm_threads;
                    file.graph.gpu_layers = cfg.gpu_layers;
                    match file.save(path) {
                        Ok(()) => persisted = true,
                        Err(e) => warn!("could not persist the graph settings: {e:#}"),
                    }
                }
                Err(e) => warn!("could not re-read the config to persist the graph: {e:#}"),
            }
        }
        info!(
            enabled = cfg.enabled,
            threads = cfg.llm_threads,
            persisted,
            "memory graph settings changed"
        );
        let mut payload = self.graph_config_json(&cfg);
        payload["persisted"] = json!(persisted);
        // The worker reads the switch between conversations, so a client would
        // otherwise see nothing move for up to one batch pause. Say what is
        // true now and let the worker's own event correct it when it acts.
        self.bus
            .publish(Topic::Status, "graph", self.control.graph_state().to_json());
        self.announce_status();
        Ok(json!({
            "config": payload,
            "enrichment": self.control.graph_state().to_json(),
        }))
    }

    /// The imperative wrapper over the same switch: "run it" / "stop".
    ///
    /// It exists because a person pressing a button in the Memory view is not
    /// editing a setting, they are asking for the pass to happen — even though
    /// underneath it is one flag. Nothing is interrupted mid-conversation; the
    /// worker stands down at the next boundary, which is at most one model call.
    fn graph_enrich(&self, req: &Request) -> Result<Value, Error> {
        let action = req
            .opt_str("action")?
            .map(str::to_ascii_lowercase)
            .unwrap_or_else(|| "start".into());
        let start = match action.as_str() {
            "start" => true,
            "stop" => false,
            other => {
                return Err(Error::params(format!(
                    "action must be \"start\" or \"stop\", not {other:?}"
                )));
            }
        };
        let cfg = self.control.graph();
        if start != cfg.enabled {
            // Starting is turning it on, and turning it on is a decision worth
            // remembering — so it goes through the same persisted path.
            return self.apply_graph(Some(start), None, None);
        }
        Ok(json!({
            "config": self.graph_config_json(&cfg),
            "enrichment": self.control.graph_state().to_json(),
            "changed": false,
        }))
    }

    /// Open commitments, soonest-due first. `state` filters; omitting it
    /// returns every state, which is what a client showing history wants.
    fn commitments_list(&self, req: &Request) -> Result<Value, Error> {
        let state = match req.opt_str("state")? {
            None => None,
            Some(s) => Some(crate::store::commitment_state::parse(s).ok_or_else(|| {
                Error::params(format!(
                    "state must be one of {:?}, not {s:?}",
                    crate::store::commitment_state::ALL
                ))
            })?),
        };
        let limit = req.usize_or("limit", COMMITMENT_LIMIT)?.clamp(1, 500);
        let rows = self
            .store()
            .commitments(state, limit)
            .map_err(Error::from)?;
        Ok(json!({
            "state": state,
            "commitments": rows.iter().map(|c| self.commitment_json(c)).collect::<Vec<_>>(),
        }))
    }

    /// The state machine, and the only thing that moves a commitment off
    /// `candidate`. Every transition is a person's click; nothing here is ever
    /// called by the daemon itself.
    fn commitments_set_state(&self, req: &Request) -> Result<Value, Error> {
        let id = req.i64("id")?;
        let state = req
            .opt_str("state")?
            .ok_or_else(|| Error::params("state is required"))?;
        let state = crate::store::commitment_state::parse(state).ok_or_else(|| {
            Error::params(format!(
                "state must be one of {:?}, not {state:?}",
                crate::store::commitment_state::ALL
            ))
        })?;
        let row = self
            .store()
            .set_commitment_state(id, state, utc_now_ns())
            .map_err(Error::from)?
            .ok_or_else(|| Error::not_found(format!("no commitment with id {id}")))?;
        let payload = self.commitment_json(&row);
        // Broadcast, like every other retroactive change: a decision made here
        // has to reach the CLI and any other window without either re-querying.
        self.bus.publish(Topic::Ops, "commitment", payload.clone());
        Ok(payload)
    }

    /// The topic labels in use, most recently heard first.
    fn topics_list(&self, req: &Request) -> Result<Value, Error> {
        let per_topic = req.usize_or("per_topic", TOPIC_THREADS)?.clamp(1, 100);
        let limit = req.usize_or("limit", TOPIC_LIMIT)?.clamp(1, 500);
        let mut rows = self.store().topics(per_topic).map_err(Error::from)?;
        rows.truncate(limit);
        Ok(json!({
            "topics": rows.iter().map(|t| json!({
                "topic": t.topic,
                "threads": t.threads,
                "segments": t.segments,
                "last_ms": ns_to_ms(t.last_ns),
                "last_ns": t.last_ns.to_string(),
                "thread_ids": t.thread_ids,
            })).collect::<Vec<_>>(),
        }))
    }

    // ---- segments --------------------------------------------------------

    /// One segment's audio, base64 in the reply. Small by construction (a
    /// segment is capped at tens of seconds of 16 kHz mono), so it is served
    /// inline rather than through a second channel the GUI would have to grow
    /// a file path for — and a path would defeat the point of the 0700 data
    /// directory being the access control.
    fn segments_audio(&self, req: &Request) -> Result<Value, Error> {
        let id = req.i64("id")?;
        let (rel, t_start_ns, t_end_ns) = self
            .store()
            .segment_audio(id)
            .map_err(Error::from)?
            .ok_or_else(|| Error::not_found(format!("no segment with id {id}")))?;
        // A blanked path is retention having done its job, not a fault: the
        // transcript outlives the recording on purpose (DESIGN §8).
        if rel.is_empty() {
            return Err(Error::new(
                "gone",
                format!(
                    "segment {id} still has its text, but not its audio — \
                     the audio retention window expired"
                ),
            ));
        }
        let path = self.control.data_dir.join(&rel);
        let meta = std::fs::metadata(&path).map_err(|_| {
            Error::new(
                "gone",
                format!(
                    "segment {id} points at audio that is no longer on disk — \
                     the audio retention window expired or the file was removed"
                ),
            )
        })?;
        if meta.len() > MAX_AUDIO_BYTES {
            return Err(Error::new(
                "refused",
                format!(
                    "segment {id}'s audio is {} bytes, over the {MAX_AUDIO_BYTES} byte \
                     limit for one reply",
                    meta.len()
                ),
            ));
        }
        let bytes = std::fs::read(&path)
            .map_err(|e| Error::internal(format!("reading {}: {e}", path.display())))?;
        let (sample_rate, duration_ms) = wav_span(&bytes, ns_to_ms(t_end_ns - t_start_ns));
        Ok(json!({
            "id": id,
            "wav_b64": crate::b64::encode(&bytes),
            "duration_ms": duration_ms,
            "sample_rate": sample_rate,
            "bytes": bytes.len(),
        }))
    }

    /// Walk the language prior's backlog (0.7.7). Bounded and synchronous.
    ///
    /// The arbiter is loaded for the call and dropped with it, rather than kept
    /// resident the way the pipeline's is. That is deliberate: this is a
    /// maintenance method, the pipeline may already be holding its own copy of
    /// the same 160 MB, and pinning a second one for the lifetime of a daemon
    /// because somebody once ran a repair would be the wrong trade in exactly
    /// the direction this program does not make.
    ///
    /// `limit` is clamped rather than optional: the CLI path is the one that
    /// may run to completion, because it is the one a person is watching.
    fn lang_repair(&self, req: &Request) -> Result<Value, Error> {
        let limit = req.usize_or("limit", REPAIR_LIMIT)?.clamp(1, REPAIR_MAX);
        let Some(root) = self.control.models_root.clone() else {
            return Err(Error::new(
                "unavailable",
                "no [models].dir is configured, so there is no arbiter to re-read with",
            ));
        };
        let cfg = crate::config::ModelsConfig {
            dir: Some(root.clone()),
            ..Default::default()
        };
        let models = crate::models::ModelSet::resolve_at(root, &cfg);
        let mut arbiters = crate::arbiter::Arbiters::new(&models);
        let installed: Vec<String> = arbiters.installed().iter().map(|s| s.to_string()).collect();
        let store = self.store();
        if installed.is_empty() {
            let (flagged, with_audio) = store.language_mismatch_counts().map_err(Error::from)?;
            return Ok(json!({
                "arbiters": installed,
                "flagged": flagged,
                "repairable": with_audio,
                "scanned": 0,
                "repaired": 0,
                "settled": 0,
                "note": crate::models::ArbiterModel::how_to_get_it(),
            }));
        }
        let report = crate::langctx::repair(
            &store,
            &self.control.data_dir,
            &mut arbiters,
            &self.control.lang,
            REPAIR_BATCH,
            Some(limit),
            |_| {},
        )
        .map_err(Error::from)?;
        self.control
            .analysis
            .repairs
            .fetch_add(report.repaired as u64, Ordering::Relaxed);
        let (flagged, with_audio) = store.language_mismatch_counts().map_err(Error::from)?;
        // Every rewritten row is a row some client is showing the old words
        // for, so each one goes out as a `segment` event exactly as a re-decode
        // in the pipeline does. Read back from the row, never from the report:
        // the event must not be able to disagree with what a query returns.
        let payloads: Vec<Value> = report
            .changed
            .iter()
            .filter_map(|id| store.segment_row(*id).ok().flatten())
            .map(|row| segment_json(&row))
            .collect();
        drop(store);
        for p in payloads {
            self.bus.publish(Topic::Segments, "segment", p);
        }
        Ok(json!({
            "arbiters": installed,
            "scanned": report.scanned,
            "repaired": report.repaired,
            "repaired_de": report.repaired_de,
            "repaired_en": report.repaired_en,
            "settled": report.settled,
            "kept": report.kept,
            "too_short": report.too_short,
            "unavailable": report.unavailable,
            "undecidable": report.undecidable,
            "no_audio": report.no_audio,
            "flagged": flagged,
            "repairable": with_audio,
        }))
    }

    fn segments_reassign(&self, req: &Request) -> Result<Value, Error> {
        let segment_id = req.i64("segment_id")?;
        // An explicit null speaker_id means "this was nobody I can name",
        // which is a legitimate correction of a wrong label.
        let speaker_id = req.opt_i64("speaker_id")?;
        let store = self.store();
        let (prior_speaker, _) = store
            .segment_state(segment_id)
            .map_err(|_| Error::not_found(format!("no segment with id {segment_id}")))?;
        store
            .reassign_segment(segment_id, speaker_id)
            .map_err(|e| Error::new("not_found", format!("{e:#}")))?;
        store
            .log_operation(
                "segments.reassign",
                &json!([segment_id]).to_string(),
                &json!({"segment_id": segment_id, "speaker_id": prior_speaker}).to_string(),
                utc_now_ns(),
            )
            .map_err(Error::from)?;
        let row = store.segment_row(segment_id).map_err(Error::from)?;
        drop(store);

        // The whole row, on the same `segment` event a new segment arrives on:
        // a client folds an update in exactly as it folds in an arrival, and
        // cannot end up with a half-described row.
        let seq = match &row {
            Some(row) => self
                .bus
                .publish(Topic::Segments, "segment", segment_json(row)),
            None => self.bus.current_seq(),
        };
        Ok(json!({"segment_id": segment_id, "speaker": speaker_id, "seq": seq}))
    }

    fn segments_correct(&self, req: &Request) -> Result<Value, Error> {
        let segment_id = req.i64("segment_id")?;
        let text = req.str("text")?.to_string();
        let store = self.store();
        let (_, prior_text) = store
            .segment_state(segment_id)
            .map_err(|_| Error::not_found(format!("no segment with id {segment_id}")))?;
        store
            .correct_segment_text(segment_id, &text)
            .map_err(|e| Error::new("not_found", format!("{e:#}")))?;
        store
            .log_operation(
                "segments.correct",
                &json!([segment_id]).to_string(),
                &json!({"segment_id": segment_id, "text": prior_text}).to_string(),
                utc_now_ns(),
            )
            .map_err(Error::from)?;
        let row = store.segment_row(segment_id).map_err(Error::from)?;
        drop(store);

        let seq = match &row {
            Some(row) => {
                let mut data = segment_json(row);
                // Marked so a view can show that these words are the user's,
                // not the model's.
                data["corrected"] = json!(true);
                self.bus.publish(Topic::Segments, "segment", data)
            }
            None => self.bus.current_seq(),
        };
        Ok(json!({"segment_id": segment_id, "text": text, "seq": seq}))
    }

    // ---- the vocabulary (0.8.0) ------------------------------------------

    /// The glossary a person curates, the three lists the daemon assembles for
    /// itself, and their capped union.
    ///
    /// The reply carries `applied_to_decoder: false`, which is the honest half
    /// of this method: the list is real and nothing is currently biased by it.
    /// `spike/hotwords_bench.py` measured why — +9.1% relative recall on the
    /// targeted words against a +20% gate, only under a decoder that costs
    /// 1.6 pp of WER before any hotword is added, with the glossary bleeding
    /// into unrelated turns (control WER 8.3% → 29.4%) at the strongest
    /// setting. See `crate::vocab`.
    fn vocab_get(&self) -> Result<Value, Error> {
        let cap = self.control.asr().vocab_max_terms;
        let store = self.store();
        Ok(crate::vocab::read(&store, cap)
            .map_err(Error::from)?
            .to_json())
    }

    /// Replace the user glossary. Whole-list replacement, because it is the
    /// only shape that can express a deletion.
    fn vocab_set(&self, req: &Request) -> Result<Value, Error> {
        let terms = match req.param("terms") {
            Some(Value::Array(items)) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    match item.as_str() {
                        Some(s) => out.push(s.to_string()),
                        None => return Err(Error::params("terms must be an array of strings")),
                    }
                }
                out
            }
            _ => return Err(Error::params("terms must be an array of strings")),
        };
        let cap = self.control.asr().vocab_max_terms;
        let store = self.store();
        crate::vocab::set_user_terms(&store, &terms, cap).map_err(Error::from)?;
        let vocab = crate::vocab::read(&store, cap).map_err(Error::from)?;
        drop(store);
        let mut data = vocab.to_json();
        let seq = self.bus.publish(Topic::Status, "vocab", data.clone());
        data["seq"] = json!(seq);
        Ok(data)
    }

    // ---- reading ---------------------------------------------------------

    fn filter_of(&self, req: &Request) -> Result<SegmentFilter, Error> {
        Ok(SegmentFilter {
            speaker: req.opt_i64("speaker")?,
            session: req.opt_i64("session")?,
            source: req.opt_str("source")?.map(str::to_string),
            from: time_param(req, "from")?,
            to: time_param(req, "to")?,
        })
    }

    fn search(&self, req: &Request) -> Result<Value, Error> {
        let q = req.str("q")?.trim().to_string();
        if q.is_empty() {
            return Err(Error::params("q must not be empty"));
        }
        let limit = req.usize_or("limit", 50)?.clamp(1, 1000);
        let filter = self.filter_of(req)?;
        let (hits, total) = {
            let store = self.store();
            let hits = store
                .search_filtered(&q, &filter, limit)
                // A malformed FTS query is the caller's problem, not a daemon fault.
                .map_err(|e| Error::new("params", format!("{e:#}")))?;
            // The real match count, not the page size (audit finding #23:
            // 4,000 matches reported as "100 matches").
            let total = store
                .search_count(&q, &filter)
                .map_err(|e| Error::new("params", format!("{e:#}")))?;
            (hits, total)
        };
        Ok(json!({
            "total": total,
            "q": q,
            // A hit is a whole segment, not a fragment: the answer to "what did
            // she say about that world?" is the conversation around it, so the
            // client can show that without a second query.
            "hits": hits
                .into_iter()
                .map(|h| {
                    let mut row = segment_json(&h.row);
                    row["snippet"] = json!(h.snippet);
                    row
                })
                .collect::<Vec<_>>(),
        }))
    }

    /// `search.semantic` — search by meaning, optionally fused with FTS.
    ///
    /// Two modes, and the parameter is explicit rather than inferred:
    ///
    /// * `"semantic"` (the default) ranks by cosine over the transcript
    ///   vectors alone. This is the mode that finds a German sentence from an
    ///   English query, and the one that finds nothing at all when the words
    ///   are right there but the meaning is thin ("Japan").
    /// * `"hybrid"` runs FTS *and* the vector scan and fuses them with
    ///   reciprocal-rank fusion. Each hit says which leg found it, in `via`.
    ///
    /// A missing model is `err:unavailable` with the command that fixes it —
    /// never an empty result set, which a client would render as "she never
    /// said that".
    fn search_semantic(&self, req: &Request) -> Result<Value, Error> {
        use crate::semantic::{self, Via};

        let Some(leg) = self.semantic() else {
            return Err(Error::new(
                "unavailable",
                crate::models::SemanticModel::how_to_get_it(),
            ));
        };
        let q = req.str("q")?.trim().to_string();
        if q.is_empty() {
            return Err(Error::params("q must not be empty"));
        }
        let hybrid = match req.opt_str("mode")? {
            None | Some("semantic") => false,
            Some("hybrid") => true,
            Some(other) => {
                return Err(Error::params(format!(
                    "mode must be \"semantic\" or \"hybrid\", not {other:?}"
                )));
            }
        };
        let limit = req.usize_or("limit", 50)?.clamp(1, 1000);
        let filter = self.filter_of(req)?;

        let store = self.store();
        let within = semantic::candidates(&store, &filter).map_err(Error::from)?;
        let started = std::time::Instant::now();
        let scored = leg
            .search(&store, &q, limit, &within)
            .map_err(|e| Error::new("failed", format!("{e:#}")))?;
        let took_ms = started.elapsed().as_secs_f64() * 1000.0;
        let semantic_ids: Vec<i64> = scored.iter().map(|s| s.segment_id).collect();
        let scores: HashMap<i64, f32> = scored.iter().map(|s| (s.segment_id, s.score)).collect();

        // The keyword leg is best-effort in hybrid mode: `MATCH` is a query
        // language and a user typing a bare apostrophe into a search box is
        // not an error worth failing the whole search over — the vector leg
        // has no syntax at all and will still answer.
        let keyword_ids: Vec<i64> = if hybrid {
            match store.search_filtered(&q, &filter, limit) {
                Ok(hits) => hits.iter().map(|h| h.segment_id()).collect(),
                Err(e) => {
                    warn!("the keyword leg of a hybrid search failed: {e:#}");
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };

        let fused = semantic::fuse(&keyword_ids, &semantic_ids, semantic::RRF_K);
        let mut hits = Vec::with_capacity(fused.len().min(limit));
        for f in fused.iter().take(limit) {
            let Some(row) = store.segment_row(f.segment_id).map_err(Error::from)? else {
                continue;
            };
            let mut item = segment_json(&row);
            item["via"] = json!(f.via.as_str());
            item["rrf"] = json!(f.score);
            // Only present when the vector leg actually scored it: a
            // keyword-only hit has no cosine, and inventing one would be a
            // number a client could sort by and be wrong.
            if let Some(s) = scores.get(&f.segment_id) {
                item["score"] = json!(*s);
            }
            if f.via != Via::Semantic {
                item["snippet"] = json!(row.text.clone().unwrap_or_default());
            }
            hits.push(item);
        }
        drop(store);

        Ok(json!({
            "total": hits.len(),
            "q": q,
            "mode": if hybrid { "hybrid" } else { "semantic" },
            "model": leg.model_id(),
            "took_ms": took_ms,
            "hits": hits,
        }))
    }

    fn transcript(&self, req: &Request) -> Result<Value, Error> {
        let limit = req.usize_or("limit", 500)?.clamp(1, 10_000);
        let filter = self.filter_of(req)?;
        let rows = self
            .store()
            .segment_rows(&filter, limit)
            .map_err(Error::from)?;
        Ok(json!({
            "segments": rows.iter().map(segment_json).collect::<Vec<_>>(),
        }))
    }

    fn roster_now(&self) -> Result<Value, Error> {
        let rows = self.store().roster_present().map_err(Error::from)?;
        Ok(json!({
            "roster": rows
                .into_iter()
                .map(|r| json!({
                    "world_id": r.world_id,
                    "instance": r.instance,
                    "who": r.display_name,
                    "joined_at_utc_ns": r.joined_at_utc_ns,
                }))
                .collect::<Vec<_>>(),
        }))
    }

    fn operations_list(&self, req: &Request) -> Result<Value, Error> {
        let limit = req.usize_or("limit", 100)?.clamp(1, 10_000);
        let rows = self.store().operations(limit).map_err(Error::from)?;
        Ok(json!({
            "operations": rows
                .into_iter()
                .map(|r| json!({
                    "id": r.id,
                    "op": r.op,
                    // Written as JSON, handed back as JSON, so a client never
                    // has to parse a string to read its own audit trail.
                    "target_ids": serde_json::from_str::<Value>(&r.target_ids).unwrap_or(Value::Null),
                    "prior_state": serde_json::from_str::<Value>(&r.prior_state).unwrap_or(Value::Null),
                    "at_utc_ns": r.at_utc_ns,
                }))
                .collect::<Vec<_>>(),
        }))
    }

    // ---- deletion --------------------------------------------------------

    fn delete_preview(&self, req: &Request) -> Result<Value, Error> {
        let filter = self.filter_of(req)?;
        let rows = self
            .store()
            .segments_matching(&filter)
            .map_err(Error::from)?;
        let bytes: u64 = rows
            .iter()
            .filter(|(_, p)| !p.is_empty())
            .filter_map(|(_, p)| std::fs::metadata(self.control.data_dir.join(p)).ok())
            .map(|m| m.len())
            .sum();
        Ok(json!({
            "segments": rows.len(),
            "bytes": bytes,
            "everything": filter.is_everything(),
        }))
    }

    /// Bulk delete as an operation handle. Soft delete only: the rows leave
    /// every read path immediately and the retention sweeper makes it final
    /// once the undo window closes (DESIGN §8).
    fn delete_run(self: &Arc<Self>, req: &Request) -> Result<Value, Error> {
        let filter = self.filter_of(req)?;
        // An empty filter selects EVERY live segment. That must never be one
        // absent parameter away (audit finding #14: a client bug passing
        // `speaker: null` would have wiped the whole transcript and cheerfully
        // reported the count). Wiping everything requires saying so.
        if filter.is_everything() && !req.opt_bool("confirm_everything")?.unwrap_or(false) {
            return Err(Error::new(
                "refused",
                "this filter matches every live segment — pass confirm_everything: true \
                 if wiping the whole transcript is really the intent",
            ));
        }
        let rows = self
            .store()
            .segments_matching(&filter)
            .map_err(Error::from)?;
        let op = format!("op_{}", self.next_op.fetch_add(1, Ordering::SeqCst));
        self.ops
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(op.clone(), OpState::Running);

        let this = Arc::clone(self);
        let op_id = op.clone();
        let total = rows.len();
        let spawned = std::thread::Builder::new()
            .name("recalld-delete".into())
            .spawn(move || this.run_delete(op_id, rows));
        if let Err(e) = spawned {
            self.finish_op(&op, OpState::Failed);
            self.bus.publish(
                Topic::Ops,
                "op.failed",
                json!({"op": op, "kind": "delete.run", "msg": e.to_string()}),
            );
            return Err(Error::internal(format!("could not start the delete: {e}")));
        }
        Ok(json!({"op": op, "segments": total}))
    }

    fn run_delete(self: Arc<Self>, op: String, rows: Vec<(i64, String)>) {
        let total = rows.len();
        let ids: Vec<i64> = rows.iter().map(|(id, _)| *id).collect();
        let mut deleted = 0usize;
        let at = utc_now_ns();

        for batch in ids.chunks(DELETE_BATCH) {
            let outcome = {
                let store = self.store();
                store.soft_delete_segments(batch, at).and_then(|n| {
                    store.log_operation(
                        "delete.run",
                        &serde_json::to_string(batch).unwrap_or_else(|_| "[]".into()),
                        &json!({"deleted_at": at, "soft": true}).to_string(),
                        at,
                    )?;
                    Ok(n)
                })
            };
            match outcome {
                Ok(n) => deleted += n,
                Err(e) => {
                    warn!(%op, "delete failed: {e:#}");
                    self.finish_op(&op, OpState::Failed);
                    self.bus.publish(
                        Topic::Ops,
                        "op.failed",
                        json!({"op": op, "kind": "delete.run", "msg": format!("{e:#}")}),
                    );
                    return;
                }
            }
            self.bus.publish(
                Topic::Ops,
                "op.progress",
                json!({
                    "op": op,
                    "kind": "delete.run",
                    "done": deleted,
                    "total": total,
                    "frac": if total == 0 { 1.0 } else { deleted as f64 / total as f64 },
                }),
            );
            // Named rows, so a client drops exactly these and does not have to
            // re-query a whole transcript to find out what went.
            self.bus
                .publish(Topic::Segments, "purge", json!({"ids": batch}));
        }

        // A conversation whose every turn just went is not an empty
        // conversation, it is no conversation — and `prune_empty_threads` had
        // no callers at all, so a fully deleted thread stayed in the table
        // forever and `thread.get` went on answering with an empty shell
        // (audit finding #15). Here and in the sweeper's purge pass, which is
        // the other place segments leave in bulk.
        match self.store().prune_empty_threads() {
            Ok(0) => {}
            Ok(n) => info!(%op, threads = n, "removed conversations left with no turns"),
            Err(e) => warn!(%op, "could not prune empty threads: {e:#}"),
        }

        self.finish_op(&op, OpState::Done);
        info!(%op, deleted, "soft-deleted; the sweeper makes it final");
        self.bus.publish(
            Topic::Ops,
            "op.done",
            json!({
                "op": op,
                "kind": "delete.run",
                "removed": deleted,
                "total": total,
                "soft": true,
            }),
        );
        self.announce_status();
    }

    fn finish_op(&self, op: &str, state: OpState) {
        self.ops
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(op.to_string(), state);
    }
}

/// A time filter, in any of the forms a client can honestly send it.
///
/// ISO-8601 is what a browser's `Date.toISOString()` produces and what a human
/// writes. A bare number is ambiguous, so it is read by magnitude: values at or
/// above 10^15 are UTC nanoseconds (the daemon's own unit), anything smaller is
/// milliseconds since the epoch. The boundary sits 30,000 years from either
/// interpretation's plausible range.
fn time_param(req: &Request, key: &str) -> Result<Option<i64>, Error> {
    let Some(value) = req.param(key) else {
        return Ok(None);
    };
    if let Some(text) = value.as_str() {
        return parse_iso8601(text).map(Some).ok_or_else(|| {
            Error::params(format!(
                "{key} must be ISO-8601 (2026-08-31T18:46:00Z) or a number; got {text:?}"
            ))
        });
    }
    match value.as_i64() {
        Some(n) if n.abs() >= 1_000_000_000_000_000 => Ok(Some(n)),
        Some(n) => Ok(Some(n * 1_000_000)),
        None => Err(Error::params(format!("{key} must be a string or a number"))),
    }
}

/// What the WAV itself says it is: `(sample_rate, duration_ms)`.
///
/// The row's span is the fallback, not the answer. A segment's stored span is
/// the pipeline's clock; the file is what a `<audio>` element will actually
/// play, and a progress bar that disagrees with the sound is a bug report.
fn wav_span(bytes: &[u8], row_ms: i64) -> (u32, i64) {
    let Ok(reader) = hound::WavReader::new(std::io::Cursor::new(bytes)) else {
        return (16_000, row_ms);
    };
    let spec = reader.spec();
    let frames = reader.duration() as i64;
    let rate = spec.sample_rate.max(1);
    (rate, frames * 1000 / rate as i64)
}

// ===========================================================================
// 0.8.0 — the product round: search.ask, notes.*, person.brief,
// accuracy.summary (PROTOCOL "the accuracy round").
//
// Kept in one impl block on purpose. The logic lives in `ask`, `notes`,
// `brief` and `accuracy`; what is here is the socket's share of it — reading
// parameters, holding the store lock for as short as possible, and
// broadcasting.
// ===========================================================================

/// Hits one `search.ask` returns when the caller does not say. A question is
/// answered on a screen, and a screen holds twenty answers.
const ASK_LIMIT: usize = 50;

/// Notes one `notes.list` returns when the caller does not say.
const NOTES_LIMIT: usize = 200;

impl Service {
    /// `search.ask` — one query box.
    ///
    /// The parse is pure and testable ([`crate::ask`]); this is the part that
    /// needs a database: the voicebank the parser matches names against, and
    /// the search it runs with what came out.
    fn search_ask(self: &Arc<Self>, req: &Request) -> Result<Value, Error> {
        let q = req.str("q")?.trim().to_string();
        if q.is_empty() {
            return Err(Error::params("q must not be empty"));
        }
        let limit = req.usize_or("limit", ASK_LIMIT)?.clamp(1, 1000);

        // Only NAMED voices are offered to the parser: nobody types
        // "Speaker_07" into a question, and fuzzy-matching an auto-label would
        // turn a stray number into a facet.
        let named: Vec<crate::ask::Named> = self
            .store()
            .list_speakers()
            .map_err(Error::from)?
            .into_iter()
            .filter_map(|s| {
                s.name().map(|n| crate::ask::Named {
                    id: s.id,
                    label: n.to_string(),
                })
            })
            .collect();
        let interpretation = crate::ask::parse(&q, utc_now_ns(), &named);

        let filter = SegmentFilter {
            speaker: interpretation.speaker_id,
            session: None,
            source: None,
            from: interpretation.from_ns,
            to: interpretation.to_ns,
        };
        let expression = crate::ask::fts_expression(&interpretation.query);
        let (hits, mode) = self.ask_hits(&interpretation.query, &expression, &filter, limit)?;

        Ok(json!({
            "q": q,
            // What the daemon understood, handed back so the GUI can show it
            // and let the user drop a facet it got wrong. A parser that
            // guesses silently is a parser nobody can correct.
            "interpretation": {
                "query": interpretation.query,
                "speaker_id": interpretation.speaker_id,
                // Spelled as the voicebank spells it, not as it was typed.
                "speaker_label": interpretation.speaker_label,
                // Both forms, as everywhere else: `_ns` is a string because
                // 1.8e18 does not survive a JSON number in a browser.
                "from_ns": interpretation.from_ns.map(|v| v.to_string()),
                "to_ns": interpretation.to_ns.map(|v| v.to_string()),
                "from_ms": interpretation.from_ns.map(ns_to_ms),
                "to_ms": interpretation.to_ns.map(ns_to_ms),
                "mode": mode,
            },
            "total": hits.len(),
            "hits": hits,
        }))
    }

    /// The search half of `search.ask`: hybrid when the semantic model is
    /// installed, keyword when it is not, and a slice of transcript when the
    /// question was nothing but facets.
    ///
    /// A missing semantic model is NOT an error here, unlike in
    /// `search.semantic`: the caller asked a question, not for a particular
    /// engine, and keyword search is a real answer. `mode` says which ran.
    fn ask_hits(
        &self,
        query: &str,
        expression: &str,
        filter: &SegmentFilter,
        limit: usize,
    ) -> Result<(Vec<Value>, &'static str), Error> {
        use crate::ask::mode;

        if expression.is_empty() {
            // "was hat Aspen gestern gesagt" — every word was a facet. The
            // honest answer is the transcript those facets select, newest
            // first, rather than an empty result set.
            let mut rows = self
                .store()
                .segment_rows(filter, limit)
                .map_err(Error::from)?;
            rows.reverse();
            return Ok((rows.iter().map(segment_json).collect(), mode::FACETS));
        }

        let Some(leg) = self.semantic() else {
            let store = self.store();
            let hits = store
                .search_filtered(expression, filter, limit)
                .map_err(|e| Error::new("params", format!("{e:#}")))?;
            return Ok((
                hits.into_iter()
                    .map(|h| {
                        let mut row = segment_json(&h.row);
                        row["snippet"] = json!(h.snippet);
                        row
                    })
                    .collect(),
                mode::FTS,
            ));
        };

        // The same fusion `search.semantic` performs, over the parsed facets.
        // The vector leg is given the residual words as a PHRASE — it embeds
        // meaning, and quoting each word for FTS would be noise to it.
        let store = self.store();
        let within = crate::semantic::candidates(&store, filter).map_err(Error::from)?;
        let scored = leg
            .search(&store, query, limit, &within)
            .map_err(|e| Error::new("failed", format!("{e:#}")))?;
        let semantic_ids: Vec<i64> = scored.iter().map(|s| s.segment_id).collect();
        let scores: HashMap<i64, f32> = scored.iter().map(|s| (s.segment_id, s.score)).collect();
        let keyword_ids: Vec<i64> = match store.search_filtered(expression, filter, limit) {
            Ok(hits) => hits.iter().map(|h| h.segment_id()).collect(),
            Err(e) => {
                warn!("the keyword leg of a question failed: {e:#}");
                Vec::new()
            }
        };
        let fused = crate::semantic::fuse(&keyword_ids, &semantic_ids, crate::semantic::RRF_K);
        let mut hits = Vec::with_capacity(fused.len().min(limit));
        for f in fused.iter().take(limit) {
            let Some(row) = store.segment_row(f.segment_id).map_err(Error::from)? else {
                continue;
            };
            let mut item = segment_json(&row);
            item["via"] = json!(f.via.as_str());
            item["rrf"] = json!(f.score);
            if let Some(s) = scores.get(&f.segment_id) {
                item["score"] = json!(*s);
            }
            if f.via != crate::semantic::Via::Semantic {
                item["snippet"] = json!(row.text.clone().unwrap_or_default());
            }
            hits.push(item);
        }
        Ok((hits, mode::HYBRID))
    }

    /// `notes.list` — what you told yourself to remember.
    fn notes_list(&self, req: &Request) -> Result<Value, Error> {
        let state = match req.opt_str("state")? {
            None => None,
            Some(s) => Some(crate::store::note_state::parse(s).ok_or_else(|| {
                Error::params(format!(
                    "state must be one of {:?}, not {s:?}",
                    crate::store::note_state::ALL
                ))
            })?),
        };
        let limit = req.usize_or("limit", NOTES_LIMIT)?.clamp(1, 1000);
        let rows = self.store().notes(state, limit).map_err(Error::from)?;
        Ok(json!({
            "state": state,
            "notes": rows.iter().map(crate::notes::note_json).collect::<Vec<_>>(),
        }))
    }

    /// The note state machine. Like a commitment's, nothing but a person's
    /// click ever moves a row off `open`.
    fn notes_set_state(&self, req: &Request) -> Result<Value, Error> {
        let id = req.i64("id")?;
        let state = req
            .opt_str("state")?
            .ok_or_else(|| Error::params("state is required"))?;
        let state = crate::store::note_state::parse(state).ok_or_else(|| {
            Error::params(format!(
                "state must be one of {:?}, not {state:?}",
                crate::store::note_state::ALL
            ))
        })?;
        let row = self
            .store()
            .set_note_state(id, state)
            .map_err(Error::from)?
            .ok_or_else(|| Error::not_found(format!("no note with id {id}")))?;
        let payload = crate::notes::note_json(&row);
        // Broadcast, like every other retroactive change: a note ticked off in
        // the CLI has to disappear from the GUI without a re-query.
        self.bus.publish(Topic::Segments, "note", payload.clone());
        Ok(payload)
    }

    /// `person.brief` — what is outstanding with one person.
    fn person_brief(&self, req: &Request) -> Result<Value, Error> {
        let id = req.i64("id")?;
        crate::brief::brief(&self.store(), id)
            .map_err(Error::from)?
            .ok_or_else(|| Error::not_found(format!("no speaker with id {id}")))
    }

    /// `accuracy.summary` — how wrong the transcripts were, measured from the
    /// corrections somebody made to them.
    fn accuracy_summary(&self) -> Result<Value, Error> {
        crate::accuracy::summary(&self.store()).map_err(Error::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::allowlist::Allowlist;
    use crate::proto::{Incoming, parse};
    use crate::store::SegmentAnalysis;
    use std::path::PathBuf;
    use std::sync::mpsc::Receiver;

    struct Rig {
        service: Arc<Service>,
        client: Arc<Client>,
        rx: Receiver<Arc<Vec<u8>>>,
        dir: PathBuf,
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn rig(name: &str) -> Rig {
        rig_with(name, crate::config::IdentityConfig::default())
    }

    /// A rig on a chosen operating point — `speakers.split` is the one method
    /// whose behaviour the identity thresholds decide.
    fn rig_with(name: &str, identity: crate::config::IdentityConfig) -> Rig {
        let dir =
            std::env::temp_dir().join(format!("nx-recall-service-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        let control = Control::new(dir.clone(), None, &Allowlist::from_rules([("x", false)]))
            .with_identity(identity);
        let bus = Bus::new(64, 32);
        let service = Service::new(Arc::new(Mutex::new(store)), control, Arc::clone(&bus));
        let (client, rx) = bus.attach(None);
        client.subscribe(&Topic::ALL);
        Rig {
            service,
            client,
            rx,
            dir,
        }
    }

    fn call(rig: &Rig, line: &str) -> Result<Value, Error> {
        let Incoming::Request(req) = parse(line) else {
            panic!("not a request: {line}");
        };
        rig.service.handle(&rig.client, &req)
    }

    fn events(rig: &Rig) -> Vec<Value> {
        std::iter::from_fn(|| rig.rx.try_recv().ok())
            .map(|b| serde_json::from_slice(&b).unwrap())
            .collect()
    }

    fn a_segment(rig: &Rig, text: &str) -> (i64, i64) {
        let store = rig.service.store();
        let src = store.upsert_source("VRChat.exe", "VRChat.exe", 0).unwrap();
        let sess = store.begin_session(src, 0).unwrap();
        let seg = store
            .insert_segment(sess, 1_000, 2_000, "segments/a.wav", 0)
            .unwrap();
        store
            .set_segment_analysis(
                seg,
                &SegmentAnalysis {
                    text: Some(text.into()),
                    ..Default::default()
                },
            )
            .unwrap();
        (sess, seg)
    }

    // ---- lang.repair (0.7.7) ---------------------------------------------

    #[test]
    fn a_repair_with_nowhere_to_load_an_arbiter_from_says_so() {
        // No `[models].dir` at all. The honest answer is a refusal naming the
        // reason, never a "success" that scanned nothing.
        let r = rig("lang-repair-nomodels");
        let e = call(&r, r#"{"id":1,"method":"lang.repair"}"#).unwrap_err();
        assert_eq!(e.code, "unavailable");
        assert!(e.msg.contains("[models].dir"), "{}", e.msg);
    }

    #[test]
    fn a_repair_with_no_arbiter_installed_reports_the_backlog_and_changes_nothing() {
        let r = rig("lang-repair-noarbiter");
        // A models root that exists and holds nothing.
        let root = r.dir.join("models");
        std::fs::create_dir_all(&root).unwrap();
        // Wired the way the daemon wires it: the models root is set before the
        // control handle is shared, so this needs its own service rather than
        // the rig's.
        let store = Store::open(&r.dir).unwrap();
        let control = Control::new(r.dir.clone(), None, &Allowlist::from_rules([("x", false)]))
            .with_graph(crate::config::GraphConfig::default(), Some(root));
        let bus = Bus::new(64, 32);
        let service = Service::new(Arc::new(Mutex::new(store)), control, Arc::clone(&bus));
        let (client, _rx) = bus.attach(None);

        // One flagged row, with no arbiter that could ever settle it.
        {
            let store = service.store();
            let src = store.upsert_source("VRChat.exe", "VRChat.exe", 0).unwrap();
            let sess = store.begin_session(src, 0).unwrap();
            let seg = store
                .insert_segment(sess, 1_000, 2_000, "segments/a.wav", 0)
                .unwrap();
            store
                .set_segment_analysis(
                    seg,
                    &SegmentAnalysis {
                        text: Some("i think that is the only way".into()),
                        ..Default::default()
                    },
                )
                .unwrap();
            store.mark_segment_language_mismatch(seg).unwrap();
        }

        let Incoming::Request(req) = parse(r#"{"id":1,"method":"lang.repair"}"#) else {
            panic!();
        };
        let out = service.handle(&client, &req).unwrap();
        assert_eq!(out["scanned"], 0, "nothing can be re-read, so nothing was");
        assert_eq!(out["repaired"], 0);
        assert_eq!(out["flagged"], 1, "and the backlog is reported honestly");
        assert_eq!(out["arbiters"], json!([]));
        assert!(
            out["note"].as_str().unwrap().contains("--arbiter-de"),
            "the reply names the command that fixes it: {}",
            out["note"]
        );
        // The mark is still there for a later run to come back to.
        assert_eq!(
            service.store().language_mismatch_counts().unwrap(),
            (1, 1),
            "a repair that could not run must never clear a mark"
        );
    }

    #[test]
    fn an_unknown_method_is_an_error_not_a_disconnect() {
        let r = rig("unknown");
        let e = call(&r, r#"{"id":1,"method":"nope"}"#).unwrap_err();
        assert_eq!(e.code, "unknown_method");
        // The connection is still usable.
        assert!(call(&r, r#"{"id":2,"method":"status"}"#).is_ok());
    }

    // ---- speakers.split --------------------------------------------------

    /// One turn of a voice: a segment, its embedding, and the prototype it
    /// seeded — the arrangement the pipeline actually writes, and the one a
    /// split depends on (a prototype remembers `source_segment_id`).
    fn a_voiced_segment(
        rig: &Rig,
        session: i64,
        speaker: i64,
        vector: &[f32],
        golden: bool,
    ) -> i64 {
        let store = rig.service.store();
        let t = session * 1_000_000 + store.segments_total().unwrap() * 1_000;
        let seg = store
            .insert_segment(session, t, t + 1_000, "segments/x.wav", 0)
            .unwrap();
        let e = crate::embed::Embedding::new("m@1", vector.to_vec());
        store.store_embedding(seg, &e).unwrap();
        store
            .set_segment_speaker(seg, Some(speaker), Some(0.9))
            .unwrap();
        store
            .add_prototype(speaker, &e, Some(seg), golden, 20, 0)
            .unwrap();
        seg
    }

    fn a_session(rig: &Rig) -> i64 {
        let store = rig.service.store();
        let src = store.upsert_source("VRChat.exe", "VRChat.exe", 0).unwrap();
        store.begin_session(src, 0).unwrap()
    }

    /// The scenario the feature exists for: two people collapsed onto one id.
    /// Returns the surviving speaker, the tombstone, and each person's rows.
    fn a_false_merge(rig: &Rig) -> FalseMerge {
        let sess = a_session(rig);
        let (a, b) = {
            let store = rig.service.store();
            (
                store.mint_speaker(0).unwrap(),
                store.mint_speaker(0).unwrap(),
            )
        };
        let ours: Vec<i64> = [[1.0, 0.05], [1.0, 0.0], [0.98, 0.1]]
            .iter()
            .map(|v| a_voiced_segment(rig, sess, a, v, false))
            .collect();
        let theirs: Vec<i64> = [[0.0, 1.0], [0.02, 1.0], [0.0, 0.97]]
            .iter()
            .map(|v| a_voiced_segment(rig, sess, b, v, false))
            .collect();
        rig.service.store().merge_speakers(b, a).unwrap();
        FalseMerge {
            kept: a,
            tombstone: b,
            session: sess,
            ours,
            theirs,
        }
    }

    struct FalseMerge {
        kept: i64,
        tombstone: i64,
        session: i64,
        ours: Vec<i64>,
        theirs: Vec<i64>,
    }

    fn split(rig: &Rig, id: i64) -> Result<Value, Error> {
        call(
            rig,
            &format!(r#"{{"id":9,"method":"speakers.split","params":{{"id":{id}}}}}"#),
        )
    }

    #[test]
    fn a_false_merge_is_cut_back_into_two_voices() {
        let r = rig("split");
        let FalseMerge {
            kept, ours, theirs, ..
        } = a_false_merge(&r);
        events(&r);

        let out = split(&r, kept).unwrap();
        assert_eq!(out["kept"], kept);
        let minted = out["minted"].as_i64().unwrap();
        assert_ne!(minted, kept);
        assert_eq!(out["moved_segments"], 3);
        assert_eq!(out["moved_prototypes"], 3);
        assert_eq!(out["ambiguous"], 0);
        assert_eq!(out["resync"], false);
        assert!(out["auto"].as_str().unwrap().starts_with("Speaker_"));
        assert!(
            out["centroid_similarity"].as_f64().unwrap() < 0.6,
            "the two voices must be further apart than the refusal threshold"
        );

        // The two groups are on different ids again, and the id that survived
        // is the one that had a history.
        let store = r.service.store();
        let of = |seg: i64| store.segment_state(seg).unwrap().0;
        assert!(ours.iter().all(|s| of(*s) == Some(kept)));
        assert!(theirs.iter().all(|s| of(*s) == Some(minted)));
        assert_eq!(store.prototype_count(kept).unwrap(), 3);
        assert_eq!(store.prototype_count(minted).unwrap(), 3);
        drop(store);

        // Both voices are announced, the minted one saying where it came from,
        // and every moved row arrives on the same `segment` event a new row
        // would.
        let evs = events(&r);
        assert_eq!(evs[0]["ev"], "relabel");
        assert_eq!(evs[0]["data"]["speaker"], kept);
        assert_eq!(evs[0]["seq"], out["seq"]);
        assert_eq!(evs[1]["data"]["speaker"], minted);
        assert_eq!(evs[1]["data"]["name"], Value::Null);
        assert_eq!(evs[1]["data"]["split_from"], kept);
        let moved: Vec<i64> = evs
            .iter()
            .filter(|e| e["ev"] == "segment")
            .map(|e| e["data"]["id"].as_i64().unwrap())
            .collect();
        assert_eq!(moved, theirs);
        assert!(
            evs.iter().any(|e| e["ev"] == "op.done"),
            "PROTOCOL calls this an operation, so it gets a terminal event"
        );
    }

    #[test]
    fn a_split_records_the_speaker_every_segment_came_from() {
        let r = rig("split-audit");
        let FalseMerge { kept, theirs, .. } = a_false_merge(&r);
        let out = split(&r, kept).unwrap();

        let ops = call(&r, r#"{"id":1,"method":"operations.list"}"#).unwrap();
        let op = &ops["operations"][0];
        assert_eq!(op["op"], "speakers.split");
        assert_eq!(op["target_ids"], json!([kept, out["minted"]]));
        // Enough to undo: every moved segment, with the id and the score it
        // held before the re-cluster ran.
        let prior = op["prior_state"]["segments"].as_array().unwrap();
        assert_eq!(prior.len(), theirs.len());
        for (row, seg) in prior.iter().zip(&theirs) {
            assert_eq!(row["segment_id"], *seg);
            assert_eq!(row["speaker_id"], kept);
            assert!((row["match_score"].as_f64().unwrap() - 0.9).abs() < 1e-6);
        }
        assert_eq!(op["prior_state"]["embed_model_id"], "m@1");
        assert_eq!(op["prior_state"]["prototypes"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn one_voice_is_refused_rather_than_cut_in_half() {
        let r = rig("split-one-voice");
        let sess = a_session(&r);
        let spk = r.service.store().mint_speaker(0).unwrap();
        for i in 0..4 {
            a_voiced_segment(&r, sess, spk, &[1.0, 0.01 * i as f32], false);
        }
        events(&r);

        let e = split(&r, spk).unwrap_err();
        assert_eq!(e.code, "refused");
        assert!(e.msg.contains("one person"), "{}", e.msg);
        // A refusal changes nothing at all.
        assert_eq!(r.service.store().list_speakers().unwrap().len(), 1);
        assert!(events(&r).is_empty(), "a refused split broadcasts nothing");
        assert!(
            call(&r, r#"{"id":1,"method":"operations.list"}"#).unwrap()["operations"]
                .as_array()
                .unwrap()
                .is_empty(),
            "and writes no audit row"
        );
    }

    #[test]
    fn a_golden_pins_the_identity_and_two_goldens_refuse() {
        let r = rig("split-golden");
        let sess = a_session(&r);
        let spk = r.service.store().mint_speaker(0).unwrap();
        // The hand-enrolled voice is the *minority* here: goldens are ground
        // truth, so its cluster keeps the id anyway.
        let golden = a_voiced_segment(&r, sess, spk, &[0.0, 1.0], true);
        let others: Vec<i64> = [[1.0, 0.05], [1.0, 0.0], [0.98, 0.1]]
            .iter()
            .map(|v| a_voiced_segment(&r, sess, spk, v, false))
            .collect();

        let out = split(&r, spk).unwrap();
        assert_eq!(out["kept"], spk);
        let minted = out["minted"].as_i64().unwrap();
        let store = r.service.store();
        assert_eq!(
            store.segment_state(golden).unwrap().0,
            Some(spk),
            "hand-enrolled audio never leaves the speaker it was enrolled on"
        );
        assert!(
            others
                .iter()
                .all(|s| store.segment_state(*s).unwrap().0 == Some(minted))
        );
        drop(store);

        // A second golden on the other side is conflicting evidence.
        let r = rig("split-golden-conflict");
        let sess = a_session(&r);
        let spk = r.service.store().mint_speaker(0).unwrap();
        a_voiced_segment(&r, sess, spk, &[0.0, 1.0], true);
        a_voiced_segment(&r, sess, spk, &[0.02, 1.0], false);
        a_voiced_segment(&r, sess, spk, &[1.0, 0.05], true);
        a_voiced_segment(&r, sess, spk, &[1.0, 0.0], false);
        let e = split(&r, spk).unwrap_err();
        assert_eq!(e.code, "refused");
        assert!(e.msg.contains("both sides"), "{}", e.msg);
    }

    #[test]
    fn an_undecidable_segment_keeps_its_speaker_and_loses_its_confidence() {
        // A band wide enough to catch a segment sitting between the voices;
        // the default one is deliberately narrow (see `split::plan`).
        let r = rig_with(
            "split-ambiguous",
            crate::config::IdentityConfig {
                split_ambiguous_margin: 0.2,
                ..Default::default()
            },
        );
        let FalseMerge { kept, session, .. } = a_false_merge(&r);
        // Halfway between the two voices, and no prototype of its own — the
        // segment the matcher should never have been confident about.
        let fence = {
            let store = r.service.store();
            let seg = store
                .insert_segment(session, 9_000, 10_000, "segments/f.wav", 0)
                .unwrap();
            store
                .store_embedding(seg, &crate::embed::Embedding::new("m@1", vec![1.0, 1.0]))
                .unwrap();
            store
                .set_segment_speaker(seg, Some(kept), Some(0.92))
                .unwrap();
            seg
        };

        let out = split(&r, kept).unwrap();
        assert_eq!(out["ambiguous"], 1);
        let store = r.service.store();
        let (speaker, _) = store.segment_state(fence).unwrap();
        assert_eq!(speaker, Some(kept), "an undecidable segment stays put");
        let score: f32 = store.segment_fields(fence).unwrap()["match_score"]
            .as_ref()
            .unwrap()
            .parse()
            .unwrap();
        assert!(
            score < 0.92,
            "it must be less trusted than it was, not more: {score}"
        );
    }

    #[test]
    fn a_tombstone_cannot_be_split_but_the_voice_it_merged_into_can() {
        let r = rig("split-tombstone");
        let FalseMerge {
            kept, tombstone, ..
        } = a_false_merge(&r);
        let e = split(&r, tombstone).unwrap_err();
        assert_eq!(e.code, "conflict");
        assert!(e.msg.contains(&kept.to_string()), "{}", e.msg);
        // The merge target is exactly what a split is for.
        assert!(split(&r, kept).is_ok());
    }

    #[test]
    fn splitting_something_that_is_not_a_voice_says_so() {
        let r = rig("split-missing");
        assert_eq!(split(&r, 404).unwrap_err().code, "not_found");
        // A voice with no embeddings has nothing to re-cluster.
        let spk = r.service.store().mint_speaker(0).unwrap();
        let e = split(&r, spk).unwrap_err();
        assert_eq!(e.code, "refused");
        assert!(e.msg.contains("no embeddings"), "{}", e.msg);
        assert!(events(&r).is_empty());
    }

    #[test]
    fn pause_is_instant_reported_and_broadcast() {
        let r = rig("pause");
        let out = call(&r, r#"{"id":1,"method":"pause"}"#).unwrap();
        assert_eq!(out["paused"], true);
        assert_eq!(out["changed"], true);
        assert!(r.service.control.is_paused());
        assert_eq!(
            call(&r, r#"{"id":2,"method":"status"}"#).unwrap()["paused"],
            true
        );

        // Pausing twice is not an error; the state is re-announced anyway, so
        // a client that asked for what it already had still sees the truth.
        assert_eq!(
            call(&r, r#"{"id":3,"method":"pause"}"#).unwrap()["changed"],
            false
        );
        let evs = events(&r);
        assert_eq!(evs.len(), 2);
        assert!(evs.iter().all(|e| e["ev"] == "status"));
        assert_eq!(evs[0]["data"]["paused"], true);

        call(&r, r#"{"id":4,"method":"resume"}"#).unwrap();
        assert!(!r.service.control.is_paused());
    }

    #[test]
    fn a_rename_is_retroactive_and_broadcasts_a_relabel() {
        let r = rig("rename");
        let (_, seg) = a_segment(&r, "meet me at the fountain");
        let spk = {
            let store = r.service.store();
            let spk = store.mint_speaker(0).unwrap();
            store
                .set_segment_speaker(seg, Some(spk), Some(0.7))
                .unwrap();
            spk
        };

        let out = call(
            &r,
            &format!(
                r#"{{"id":1,"method":"speakers.name","params":{{"id":{spk},"name":"Kira"}}}}"#
            ),
        )
        .unwrap();
        assert_eq!(out["name"], "Kira");
        let evs = events(&r);
        assert_eq!(evs[0]["ev"], "relabel");
        assert_eq!(evs[0]["data"]["speaker"], spk);
        assert_eq!(evs[0]["data"]["name"], "Kira");
        assert_eq!(evs[0]["seq"], out["seq"]);

        // Past segments read through to the new name without re-querying. The
        // row carries the id (the identity) and the name (for display).
        let hits = call(
            &r,
            r#"{"id":2,"method":"search","params":{"q":"fountain"}}"#,
        )
        .unwrap();
        assert_eq!(hits["hits"][0]["speaker"], spk);
        assert_eq!(hits["hits"][0]["speaker_name"], "Kira");
        assert!(
            hits["hits"][0]["snippet"]
                .as_str()
                .unwrap()
                .contains("fountain")
        );

        // And the audit trail knows what it was called before.
        let ops = call(&r, r#"{"id":3,"method":"operations.list"}"#).unwrap();
        assert_eq!(ops["operations"][0]["op"], "speakers.name");
        assert_eq!(
            ops["operations"][0]["prior_state"]["display_name"],
            "Speaker_01"
        );
    }

    #[test]
    fn renaming_a_speaker_that_does_not_exist_is_not_found() {
        let r = rig("rename-missing");
        let e = call(
            &r,
            r#"{"id":1,"method":"speakers.name","params":{"id":99,"name":"Ghost"}}"#,
        )
        .unwrap_err();
        assert_eq!(e.code, "not_found");
        assert!(events(&r).is_empty(), "a failed rename broadcasts nothing");
    }

    #[test]
    fn a_merge_broadcasts_where_the_tombstoned_id_went() {
        let r = rig("merge");
        let (a, b) = {
            let store = r.service.store();
            (
                store.create_speaker("Speaker_01", 0).unwrap(),
                store.create_speaker("Wren", 0).unwrap(),
            )
        };
        // Name the survivor first: a merge relabel carries the name a view
        // should show, and an un-named voice's name is null, not its label.
        call(
            &r,
            &format!(r#"{{"id":0,"method":"speakers.name","params":{{"id":{b},"name":"Wren"}}}}"#),
        )
        .unwrap();
        events(&r);
        let out = call(
            &r,
            &format!(r#"{{"id":1,"method":"speakers.merge","params":{{"from":{a},"into":{b}}}}}"#),
        )
        .unwrap();
        assert_eq!(out["into"], b);
        let evs = events(&r);
        assert_eq!(evs[0]["ev"], "relabel");
        assert_eq!(evs[0]["data"]["speaker"], a);
        assert_eq!(evs[0]["data"]["merged_into"], b);
        assert_eq!(evs[0]["data"]["name"], "Wren");
        // And the survivor is announced too, because its counts just changed.
        assert_eq!(evs[1]["data"]["speaker"], b);
        assert_eq!(evs[1]["data"]["merged_into"], serde_json::Value::Null);

        // Merging the same pair again conflicts rather than chaining.
        assert_eq!(
            call(
                &r,
                &format!(
                    r#"{{"id":2,"method":"speakers.merge","params":{{"from":{a},"into":{b}}}}}"#
                )
            )
            .unwrap_err()
            .code,
            "conflict"
        );
    }

    #[test]
    fn reassigning_and_correcting_record_what_they_replaced() {
        let r = rig("correct");
        let (_, seg) = a_segment(&r, "the bell tolls");
        let spk = r.service.store().mint_speaker(0).unwrap();
        let _ = spk;

        call(
            &r,
            &format!(
                r#"{{"id":1,"method":"segments.reassign","params":{{"segment_id":{seg},"speaker_id":{spk}}}}}"#
            ),
        )
        .unwrap();
        call(
            &r,
            &format!(
                r#"{{"id":2,"method":"segments.correct","params":{{"segment_id":{seg},"text":"the belt holds"}}}}"#
            ),
        )
        .unwrap();

        let evs = events(&r);
        assert_eq!(evs.len(), 2);
        // One event type for "here is a segment", whether it is new or changed.
        assert!(evs.iter().all(|e| e["ev"] == "segment"));
        assert_eq!(evs[0]["data"]["speaker"], spk);
        assert_eq!(evs[1]["data"]["text"], "the belt holds");
        assert_eq!(evs[1]["data"]["corrected"], true);

        let ops = call(&r, r#"{"id":3,"method":"operations.list"}"#).unwrap();
        assert_eq!(ops["operations"][0]["op"], "segments.correct");
        assert_eq!(
            ops["operations"][0]["prior_state"]["text"],
            "the bell tolls"
        );
        assert_eq!(ops["operations"][1]["op"], "segments.reassign");
        assert_eq!(
            ops["operations"][1]["prior_state"]["speaker_id"],
            Value::Null
        );

        let hits = call(&r, r#"{"id":4,"method":"search","params":{"q":"belt"}}"#).unwrap();
        assert_eq!(hits["hits"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn sources_set_changes_the_live_rule_and_broadcasts_it() {
        let r = rig("sources");
        r.service
            .store()
            .upsert_source("VRChat.exe", "VRChat.exe", 0)
            .unwrap();
        assert_eq!(
            call(&r, r#"{"id":1,"method":"sources.list"}"#).unwrap()["sources"][0]["allowed"],
            false
        );

        let gen0 = r.service.control.rules_generation();
        call(
            &r,
            r#"{"id":2,"method":"sources.set","params":{"match_key":"VRChat.exe","allowed":true}}"#,
        )
        .unwrap();
        assert!(
            r.service.control.rules_generation() > gen0,
            "the capture loop must be told there is something to apply"
        );
        assert!(
            r.service
                .control
                .allowlist()
                .decide("VRChat.exe")
                .captures()
        );
        assert_eq!(
            call(&r, r#"{"id":3,"method":"sources.list"}"#).unwrap()["sources"][0]["allowed"],
            true
        );
        let evs = events(&r);
        assert_eq!(evs[0]["ev"], "source");
        assert_eq!(evs[0]["data"]["match_key"], "VRChat.exe");
    }

    #[test]
    fn a_missing_parameter_is_a_params_error() {
        let r = rig("params");
        assert_eq!(
            call(
                &r,
                r#"{"id":1,"method":"sources.set","params":{"match_key":"x"}}"#
            )
            .unwrap_err()
            .code,
            "params"
        );
        assert_eq!(
            call(&r, r#"{"id":2,"method":"search","params":{"q":"   "}}"#)
                .unwrap_err()
                .code,
            "params"
        );
    }

    #[test]
    fn transcript_and_search_take_the_same_filters() {
        let r = rig("filters");
        let (sess, _) = a_segment(&r, "portal world");
        let all = call(&r, r#"{"id":1,"method":"transcript"}"#).unwrap();
        assert_eq!(all["segments"].as_array().unwrap().len(), 1);
        assert_eq!(all["segments"][0]["session"], sess);

        let none = call(
            &r,
            r#"{"id":2,"method":"transcript","params":{"from":9000000}}"#,
        )
        .unwrap();
        assert!(none["segments"].as_array().unwrap().is_empty());
        let by_source = call(
            &r,
            r#"{"id":3,"method":"search","params":{"q":"portal","source":"Discord"}}"#,
        )
        .unwrap();
        assert!(by_source["hits"].as_array().unwrap().is_empty());
    }

    /// Semantic search with no model installed — which is every machine that
    /// has not opted in, so it is the *normal* path, not the sad one.
    #[test]
    fn semantic_search_without_the_model_says_how_to_get_it() {
        let r = rig("semantic-absent");
        let (_, _) = a_segment(&r, "portal world");

        let err = call(
            &r,
            r#"{"id":1,"method":"search.semantic","params":{"q":"portal"}}"#,
        )
        .unwrap_err();
        assert_eq!(err.code, "unavailable");
        let msg = err.msg.as_str();
        assert!(msg.contains("models fetch --semantic"), "{msg}");
        assert!(
            msg.contains("semantic backfill"),
            "the second half of the answer is missing: {msg}"
        );

        // Keyword search is untouched. That is the whole contract of an
        // optional model: nothing else notices it is gone.
        let hits = call(&r, r#"{"id":2,"method":"search","params":{"q":"portal"}}"#).unwrap();
        assert_eq!(hits["hits"].as_array().unwrap().len(), 1);

        // ...and `status` says so before anyone types anything, as a boolean
        // rather than as a missing key.
        let st = call(&r, r#"{"id":3,"method":"status"}"#).unwrap();
        assert_eq!(st["semantic"]["available"], false);
        assert!(
            st["semantic"]["how"]
                .as_str()
                .unwrap()
                .contains("--semantic")
        );
    }

    #[test]
    fn semantic_search_validates_its_parameters_before_it_needs_a_model() {
        let r = rig("semantic-params");
        // The unavailable check comes first on purpose: a client on a machine
        // with no model must get the actionable answer, not a quibble about
        // the mode string.
        for line in [
            r#"{"id":1,"method":"search.semantic","params":{"q":"x","mode":"nonsense"}}"#,
            r#"{"id":2,"method":"search.semantic","params":{"q":"   "}}"#,
        ] {
            assert_eq!(call(&r, line).unwrap_err().code, "unavailable", "{line}");
        }
    }

    #[test]
    fn delete_previews_before_it_runs_and_reports_progress() {
        let r = rig("delete");
        let (_, seg) = a_segment(&r, "something regrettable");
        // A real file, so the preview's byte count is a real number.
        let path = r.dir.join("segments/a.wav");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, vec![0u8; 1234]).unwrap();

        let preview = call(&r, r#"{"id":1,"method":"delete.preview"}"#).unwrap();
        assert_eq!(preview["segments"], 1);
        assert_eq!(preview["bytes"], 1234);
        assert_eq!(preview["everything"], true, "an unfiltered delete says so");

        // An unfiltered run is refused unless the caller says the quiet part
        // out loud — one absent parameter must never mean "wipe everything".
        let refused = call(&r, r#"{"id":2,"method":"delete.run"}"#).unwrap_err();
        assert_eq!(refused.code, "refused");

        let run = call(
            &r,
            r#"{"id":3,"method":"delete.run","params":{"confirm_everything":true}}"#,
        )
        .unwrap();
        let op = run["op"].as_str().unwrap().to_string();
        assert!(op.starts_with("op_"));

        // The op runs on its own thread; wait for its terminal event.
        let mut seen: Vec<Value> = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            while let Ok(b) = r.rx.try_recv() {
                seen.push(serde_json::from_slice(&b).unwrap());
            }
            if seen.iter().any(|e| e["ev"] == "op.done") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let done = seen
            .iter()
            .find(|e| e["ev"] == "op.done")
            .expect("op.done must arrive");
        assert_eq!(done["data"]["op"], op.as_str());
        assert_eq!(done["data"]["removed"], 1);
        assert!(seen.iter().any(|e| e["ev"] == "op.progress"));
        // The purge names the rows, so a client drops exactly those.
        let purge = seen
            .iter()
            .find(|e| e["ev"] == "purge")
            .expect("a purge event");
        assert_eq!(purge["data"]["ids"], serde_json::json!([seg]));
        assert_eq!(r.service.op_state(&op), Some(OpState::Done));

        // Gone from every read path, still on disk for the undo window.
        assert!(
            call(&r, r#"{"id":3,"method":"transcript"}"#).unwrap()["segments"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            call(&r, r#"{"id":4,"method":"delete.preview"}"#).unwrap()["segments"],
            0
        );
        assert!(
            path.exists(),
            "soft delete keeps the audio until the sweeper"
        );
        assert_eq!(
            r.service
                .store()
                .expired_soft_deletes(i64::MAX)
                .unwrap()
                .len(),
            1
        );
        let _ = seg;
    }

    #[test]
    fn subscribe_accepts_the_known_topics_and_names_the_rest() {
        let r = rig("subscribe");
        let out = call(
            &r,
            r#"{"id":1,"method":"subscribe","params":{"topics":["relabel","weather"]}}"#,
        )
        .unwrap();
        assert_eq!(out["topics"], json!(["relabel"]));
        assert_eq!(out["unknown"], json!(["weather"]));

        // No topic list at all means everything.
        let out = call(&r, r#"{"id":2,"method":"subscribe"}"#).unwrap();
        assert_eq!(out["topics"].as_array().unwrap().len(), Topic::ALL.len());
    }

    #[test]
    fn events_since_replays_or_asks_for_a_resync() {
        let r = rig("since");
        for _ in 0..3 {
            r.service
                .bus
                .publish(Topic::Relabel, "relabel", json!({"speaker": 1}));
        }
        while r.rx.try_recv().is_ok() {}

        let out = call(&r, r#"{"id":1,"method":"events.since","params":{"seq":1}}"#).unwrap();
        assert_eq!(out["replayed"], 2);
        assert_eq!(out["seq"], 3);
        // The batch is in the reply, in order, and not duplicated on the stream.
        let batch = out["events"].as_array().unwrap();
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0]["seq"], 2);
        assert_eq!(batch[1]["seq"], 3);
        assert!(events(&r).is_empty());

        assert_eq!(
            call(
                &r,
                r#"{"id":2,"method":"events.since","params":{"seq":9999}}"#
            )
            .unwrap_err()
            .code,
            "resync"
        );
    }

    // ---- segments.audio / speakers.sample --------------------------------

    /// A segment with a real WAV behind it, `secs` long, matched to `speaker`.
    fn a_clip(rig: &Rig, sess: i64, name: &str, secs: f32, speaker: Option<(i64, f32)>) -> i64 {
        let rel = format!("segments/{name}.wav");
        let samples = vec![0.25f32; (16_000.0 * secs) as usize];
        crate::pipeline::write_wav(&rig.dir.join(&rel), &samples).unwrap();
        let store = rig.service.store();
        let seg = store
            .insert_segment(sess, 0, (secs * 1e9) as i64, &rel, 0)
            .unwrap();
        store
            .set_segment_analysis(
                seg,
                &SegmentAnalysis {
                    text: Some(format!("clip {name}")),
                    ..Default::default()
                },
            )
            .unwrap();
        if let Some((id, score)) = speaker {
            store
                .set_segment_speaker(seg, Some(id), Some(score))
                .unwrap();
        }
        seg
    }

    #[test]
    fn a_segments_wav_comes_back_base64_with_its_own_rate_and_length() {
        let r = rig("audio-ok");
        let sess = a_session(&r);
        let seg = a_clip(&r, sess, "one", 1.5, None);

        let out = call(
            &r,
            &format!(r#"{{"id":1,"method":"segments.audio","params":{{"id":{seg}}}}}"#),
        )
        .unwrap();
        assert_eq!(out["id"], seg);
        assert_eq!(out["sample_rate"], 16_000);
        assert_eq!(out["duration_ms"], 1500);
        // The payload is a real RIFF/WAVE file, not a hopeful string: "UklG"
        // is what every base64 encoder makes of the first three bytes "RIF".
        let b64 = out["wav_b64"].as_str().unwrap();
        assert!(b64.starts_with("UklG"), "not a RIFF header: {}", &b64[..12]);
        assert_eq!(b64.len() % 4, 0, "base64 must be padded to a multiple of 4");
        assert_eq!(
            b64.len(),
            crate::b64::encoded_len(out["bytes"].as_u64().unwrap() as usize)
        );
    }

    #[test]
    fn audio_that_retention_took_is_gone_not_missing() {
        let r = rig("audio-gone");
        let sess = a_session(&r);
        let kept = a_clip(&r, sess, "kept", 1.0, None);
        let blanked = a_clip(&r, sess, "blanked", 1.0, None);
        let unlinked = a_clip(&r, sess, "unlinked", 1.0, None);

        // Retention's own move: the row and its text stay, the column empties.
        r.service.store().forget_audio(&[blanked]).unwrap();
        // And the other way round: the row still points somewhere, but the
        // file is not there any more.
        std::fs::remove_file(r.dir.join("segments/unlinked.wav")).unwrap();

        let code = |id: i64| {
            call(
                &r,
                &format!(r#"{{"id":1,"method":"segments.audio","params":{{"id":{id}}}}}"#),
            )
            .unwrap_err()
        };
        let e = code(blanked);
        assert_eq!(e.code, "gone");
        assert!(
            e.msg.contains("retention"),
            "the message must say why: {}",
            e.msg
        );
        assert_eq!(code(unlinked).code, "gone");
        // A soft-deleted segment is not "gone audio", it is not a segment.
        r.service.store().soft_delete_segments(&[kept], 1).unwrap();
        assert_eq!(code(kept).code, "not_found");
        assert_eq!(code(999_999).code, "not_found");
    }

    #[test]
    fn an_absurdly_large_wav_is_refused_rather_than_framed() {
        let r = rig("audio-big");
        let sess = a_session(&r);
        let seg = a_clip(&r, sess, "big", 0.1, None);
        // Bigger than any real segment could be, which is exactly the case the
        // cap exists for: a corrupt or hand-placed file must not become a
        // multi-megabyte frame nobody's client will accept.
        std::fs::write(
            r.dir.join("segments/big.wav"),
            vec![0u8; MAX_AUDIO_BYTES as usize + 1],
        )
        .unwrap();
        let e = call(
            &r,
            &format!(r#"{{"id":1,"method":"segments.audio","params":{{"id":{seg}}}}}"#),
        )
        .unwrap_err();
        assert_eq!(e.code, "refused");
    }

    #[test]
    fn the_audio_cap_cannot_outgrow_the_frame_budget() {
        // The two limits are one decision. If the cap ever rises past what a
        // frame can carry, the daemon starts writing replies that hang up the
        // client that asked for them — so it is asserted, not commented.
        let worst = crate::b64::encoded_len(MAX_AUDIO_BYTES as usize);
        assert!(
            worst + 4096 < MAX_FRAME_BYTES,
            "{MAX_AUDIO_BYTES} bytes encode to {worst}, which does not fit in {MAX_FRAME_BYTES}"
        );
    }

    #[test]
    fn a_voices_samples_are_its_longest_best_matched_clips_that_still_have_audio() {
        let r = rig("sample");
        let sess = a_session(&r);
        let spk = r.service.store().mint_speaker(0).unwrap();
        let other = r.service.store().mint_speaker(0).unwrap();

        let short = a_clip(&r, sess, "short", 1.0, Some((spk, 0.95)));
        let long_weak = a_clip(&r, sess, "long-weak", 6.0, Some((spk, 0.30)));
        let long_strong = a_clip(&r, sess, "long-strong", 6.0, Some((spk, 0.88)));
        let gone = a_clip(&r, sess, "gone", 9.0, Some((spk, 0.99)));
        a_clip(&r, sess, "elsewhere", 8.0, Some((other, 0.9)));

        // The best clip of all, except its file is not there — it must not be
        // offered, or the naming flow hands the user a play button that fails.
        std::fs::remove_file(r.dir.join("segments/gone.wav")).unwrap();

        let out = call(
            &r,
            &format!(r#"{{"id":1,"method":"speakers.sample","params":{{"id":{spk}}}}}"#),
        )
        .unwrap();
        let ids: Vec<i64> = out["samples"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["segment_id"].as_i64().unwrap())
            .collect();
        assert_eq!(ids, vec![long_strong, long_weak, short], "got {out:#}");
        assert!(!ids.contains(&gone), "a clip with no file was offered");

        let first = &out["samples"][0];
        assert_eq!(first["duration_ms"], 6000);
        assert_eq!(first["text"], "clip long-strong");
        // Both time forms, like every other segment-shaped reply.
        assert!(first["t_ns"].is_string());
        assert!(first["t_ms"].is_number());

        // `limit` is honoured, and the default is three.
        let one = call(
            &r,
            &format!(r#"{{"id":2,"method":"speakers.sample","params":{{"id":{spk},"limit":1}}}}"#),
        )
        .unwrap();
        assert_eq!(one["samples"].as_array().unwrap().len(), 1);
        assert_eq!(
            call(
                &r,
                r#"{"id":3,"method":"speakers.sample","params":{"id":4242}}"#
            )
            .unwrap_err()
            .code,
            "not_found"
        );
    }

    #[test]
    fn a_voice_whose_audio_has_all_aged_out_samples_empty_rather_than_failing() {
        let r = rig("sample-empty");
        let sess = a_session(&r);
        let spk = r.service.store().mint_speaker(0).unwrap();
        let seg = a_clip(&r, sess, "only", 4.0, Some((spk, 0.8)));
        r.service.store().forget_audio(&[seg]).unwrap();

        // Not an error: the voice exists, it simply has nothing to play. The
        // GUI turns this into "no audio kept for this voice", which is a fact
        // about retention rather than a failure to report.
        let out = call(
            &r,
            &format!(r#"{{"id":1,"method":"speakers.sample","params":{{"id":{spk}}}}}"#),
        )
        .unwrap();
        assert_eq!(out["samples"].as_array().unwrap().len(), 0);
    }

    // ---- 0.6.1: per-speaker languages ------------------------------------

    #[test]
    fn a_voices_languages_are_set_listed_and_broadcast() {
        let r = rig("languages");
        let spk = r.service.store().mint_speaker(0).unwrap();
        call(
            &r,
            &format!(
                r#"{{"id":1,"method":"speakers.name","params":{{"id":{spk},"name":"Kira"}}}}"#
            ),
        )
        .unwrap();
        let _ = events(&r);

        // Any, until somebody says otherwise.
        let listed = call(&r, r#"{"id":2,"method":"speakers.list"}"#).unwrap();
        assert_eq!(listed["speakers"][0]["languages"], Value::Null);

        let out = call(
            &r,
            &format!(
                r#"{{"id":3,"method":"speakers.set_languages","params":{{"id":{spk},"languages":["EN"]}}}}"#
            ),
        )
        .unwrap();
        assert_eq!(out["languages"], json!(["en"]), "case-folded on the way in");

        // It is on the list, and it was broadcast — with the name intact, so a
        // client folding the event in cannot lose one to learn the other.
        let listed = call(&r, r#"{"id":4,"method":"speakers.list"}"#).unwrap();
        assert_eq!(listed["speakers"][0]["languages"], json!(["en"]));
        let relabel = events(&r)
            .into_iter()
            .find(|e| e["ev"] == "relabel")
            .expect("setting a language is broadcast like any other relabel");
        assert_eq!(relabel["data"]["speaker"], json!(spk));
        assert_eq!(relabel["data"]["languages"], json!(["en"]));
        assert_eq!(relabel["data"]["name"], json!("Kira"));

        // Two languages sort, so one setting has one representation.
        let both = call(
            &r,
            &format!(
                r#"{{"id":5,"method":"speakers.set_languages","params":{{"id":{spk},"languages":["en","de"]}}}}"#
            ),
        )
        .unwrap();
        assert_eq!(both["languages"], json!(["de", "en"]));

        // …and clearing it is spelled several ways, all meaning "any".
        for params in [
            format!(r#"{{"id":{spk},"languages":[]}}"#),
            format!(r#"{{"id":{spk},"languages":["any"]}}"#),
            format!(r#"{{"id":{spk}}}"#),
        ] {
            let out = call(
                &r,
                &format!(r#"{{"id":6,"method":"speakers.set_languages","params":{params}}}"#),
            )
            .unwrap();
            assert_eq!(out["languages"], Value::Null, "{params}");
        }
    }

    #[test]
    fn a_language_the_daemon_cannot_classify_is_refused_rather_than_stored() {
        let r = rig("languages-refuse");
        let spk = r.service.store().mint_speaker(0).unwrap();
        // Storing `fr` would promise a correction this daemon cannot make: the
        // classifier knows two languages and the catalogue holds one
        // constrained decoder.
        let e = call(
            &r,
            &format!(
                r#"{{"id":1,"method":"speakers.set_languages","params":{{"id":{spk},"languages":["fr"]}}}}"#
            ),
        )
        .unwrap_err();
        assert_eq!(e.code, "params");
        assert!(e.msg.contains("fr"), "{}", e.msg);
        assert_eq!(
            r.service.store().speaker_languages(spk).unwrap(),
            None,
            "a refused call must not have written anything"
        );

        assert_eq!(
            call(
                &r,
                &format!(
                    r#"{{"id":2,"method":"speakers.set_languages","params":{{"id":{spk},"languages":5}}}}"#
                )
            )
            .unwrap_err()
            .code,
            "params"
        );
        assert_eq!(
            call(
                &r,
                r#"{"id":3,"method":"speakers.set_languages","params":{"id":4242,"languages":["en"]}}"#
            )
            .unwrap_err()
            .code,
            "not_found"
        );
    }

    // ---- 0.6.1: sweeping one-off voices ----------------------------------

    #[test]
    fn prune_lists_before_it_deletes_and_never_touches_you_or_a_named_voice() {
        let r = rig("prune");
        let sess = a_session(&r);
        let (grunt, named, you, real) = {
            let store = r.service.store();
            let grunt = store.mint_speaker(0).unwrap();
            let named = store.mint_speaker(0).unwrap();
            store.rename_speaker(named, "Kira", 1).unwrap();
            let you = store.ensure_you_speaker(0).unwrap();
            let real = store.mint_speaker(0).unwrap();
            (grunt, named, you, real)
        };
        // One half-second segment each for the three thin voices…
        a_clip(&r, sess, "grunt", 0.5, Some((grunt, 0.4)));
        a_clip(&r, sess, "named", 0.5, Some((named, 0.4)));
        a_clip(&r, sess, "you", 0.5, Some((you, 0.4)));
        // …and a voice that actually said something.
        a_clip(&r, sess, "real", 8.0, Some((real, 0.9)));

        let preview = call(&r, r#"{"id":1,"method":"speakers.prune"}"#).unwrap();
        assert_eq!(preview["apply"], json!(false));
        let ids: Vec<i64> = preview["voices"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["id"].as_i64().unwrap())
            .collect();
        assert_eq!(
            ids,
            vec![grunt],
            "only the nameless one-off, got {preview:#}"
        );
        assert_eq!(preview["count"], json!(1));
        assert_eq!(preview["voices"][0]["segments"], json!(1));
        // A preview changes nothing.
        assert_eq!(r.service.store().list_speakers().unwrap().len(), 4);

        let applied = call(
            &r,
            r#"{"id":2,"method":"speakers.prune","params":{"apply":true}}"#,
        )
        .unwrap();
        assert_eq!(applied["count"], json!(1));
        assert_eq!(applied["removed"], json!([grunt]));
        assert_eq!(applied["segments"], json!(1));

        let left: Vec<i64> = r
            .service
            .store()
            .list_speakers()
            .unwrap()
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert!(!left.contains(&grunt));
        for kept in [named, you, real] {
            assert!(left.contains(&kept), "speaker {kept} must survive a sweep");
        }
        // The pinned voice is still pinned, and the sweep is in the audit log.
        assert_eq!(r.service.store().you_speaker_id().unwrap(), Some(you));
        let ops = call(&r, r#"{"id":3,"method":"operations.list"}"#).unwrap();
        assert!(
            ops["operations"]
                .as_array()
                .unwrap()
                .iter()
                .any(|o| o["op"] == "speakers.prune"),
            "a sweep is an operation like any other: {ops:#}"
        );
        // Nothing left to sweep the second time.
        assert_eq!(
            call(&r, r#"{"id":4,"method":"speakers.prune"}"#).unwrap()["count"],
            json!(0)
        );
    }

    // ---- 0.6.4: deleting a voice -----------------------------------------

    /// One voice with everything a voice can own: clips on disk, embeddings,
    /// the prototypes those seeded, a golden sample with a real file, and a
    /// conversation the clips belong to. Exactly the arrangement the pipeline
    /// writes, and the one a delete has to take apart without collateral.
    struct AVoice {
        id: i64,
        segments: Vec<i64>,
        golden: String,
        thread: i64,
    }

    fn a_full_voice(r: &Rig, sess: i64, tag: &str, clips: usize) -> AVoice {
        let id = r.service.store().mint_speaker(0).unwrap();
        let thread = r.service.store().create_thread(sess, 0, 1).unwrap();
        let mut segments = Vec::new();
        for n in 0..clips {
            let seg = a_clip(r, sess, &format!("{tag}{n}"), 4.0, Some((id, 0.9)));
            let store = r.service.store();
            let e = crate::embed::Embedding::new("m@1", vec![1.0, n as f32 / 10.0]);
            store.store_embedding(seg, &e).unwrap();
            store
                .add_prototype(id, &e, Some(seg), false, 20, 0)
                .unwrap();
            store.set_segment_thread(seg, thread, 1).unwrap();
            segments.push(seg);
        }
        let golden = format!("goldens/{tag}.wav");
        std::fs::create_dir_all(r.dir.join("goldens")).unwrap();
        std::fs::write(r.dir.join(&golden), b"RIFFgolden").unwrap();
        r.service
            .store()
            .add_golden_sample(id, &golden, 4.0)
            .unwrap();
        AVoice {
            id,
            segments,
            golden,
            thread,
        }
    }

    fn thread_exists(r: &Rig, thread: i64) -> bool {
        r.service.store().thread_summary(thread).unwrap().is_some()
    }

    fn delete_speaker(r: &Rig, id: i64, keep: bool) -> Result<Value, Error> {
        call(
            r,
            &format!(
                r#"{{"id":1,"method":"speakers.delete","params":{{"id":{id},"keep_voiceprint":{keep}}}}}"#
            ),
        )
    }

    /// The reported bug, exactly: three voices at "0 SEGMENTS · 0s" that Delete
    /// could not touch, because a delete scoped by segment matched nothing and
    /// the voiceprint behind them was never in scope at all.
    #[test]
    fn a_voice_with_no_conversations_left_is_still_deletable() {
        let r = rig("delete-empty-voice");
        let sess = a_session(&r);
        let ghost = a_full_voice(&r, sess, "ghost", 1);
        // The state the user was actually in: the words are already gone, the
        // voice is not, and it still has a live prototype that would match the
        // next thing it heard.
        r.service
            .store()
            .soft_delete_segments(&ghost.segments, 1)
            .unwrap();
        assert_eq!(
            r.service
                .store()
                .speaker_summary(ghost.id)
                .unwrap()
                .unwrap()
                .segments,
            0,
            "the fixture is not the reported state"
        );
        assert_eq!(r.service.store().prototype_count(ghost.id).unwrap(), 1);

        let out = delete_speaker(&r, ghost.id, false).unwrap();
        // No live rows to take, and it still did the thing that was asked.
        assert_eq!(out["segments"], json!(0));
        assert_eq!(out["removed_speaker"], json!(true));
        assert_eq!(out["prototypes"], json!(1));
        assert!(
            r.service
                .store()
                .speaker_summary(ghost.id)
                .unwrap()
                .is_none(),
            "the empty voice is still in the bank"
        );
        assert_eq!(r.service.store().prototype_count(ghost.id).unwrap(), 0);
        assert!(
            !r.service
                .store()
                .prototypes("m@1")
                .unwrap()
                .iter()
                .any(|(sp, _)| *sp == ghost.id),
            "the ghost can still match future audio"
        );
    }

    #[test]
    fn the_nuke_path_takes_the_whole_voice_and_nothing_around_it() {
        let r = rig("delete-nuke");
        let sess = a_session(&r);
        let doomed = a_full_voice(&r, sess, "doomed", 3);
        let bystander = a_full_voice(&r, sess, "bystander", 2);

        let out = delete_speaker(&r, doomed.id, false).unwrap();
        assert_eq!(out["segments"], json!(3));
        assert_eq!(out["prototypes"], json!(3));
        assert_eq!(out["embeddings"], json!(3));
        assert_eq!(out["goldens"], json!(1));
        assert_eq!(out["keep_voiceprint"], json!(false));

        let store = r.service.store();
        // Segments: soft-deleted, not purged. The undo window is the same one
        // `delete.run` leaves behind, and the sweeper still finalises them.
        for seg in &doomed.segments {
            let f = store.segment_fields(*seg).unwrap();
            assert!(
                f["deleted_at"].is_some(),
                "segment {seg} was not soft-deleted"
            );
            assert!(f["speaker_id"].is_none(), "segment {seg} kept its label");
            assert!(
                store.segment_embedding(*seg).unwrap().is_none(),
                "segment {seg} kept its embedding"
            );
        }
        // Identity: gone, with everything the voicebank rested on.
        assert!(store.speaker_summary(doomed.id).unwrap().is_none());
        assert_eq!(store.prototype_count(doomed.id).unwrap(), 0);
        assert!(store.golden_samples_for(doomed.id).unwrap().is_empty());
        assert!(
            !r.dir.join(&doomed.golden).exists(),
            "the golden sample's file is still on disk"
        );
        // The conversation went with its last live row; the bystander's did not.
        drop(store);
        assert!(!thread_exists(&r, doomed.thread));
        assert!(thread_exists(&r, bystander.thread));

        // "and nothing else": the other voice is untouched, down to its bytes.
        let store = r.service.store();
        assert_eq!(
            store
                .speaker_summary(bystander.id)
                .unwrap()
                .unwrap()
                .segments,
            2
        );
        assert_eq!(store.prototype_count(bystander.id).unwrap(), 2);
        assert_eq!(store.golden_samples_for(bystander.id).unwrap().len(), 1);
        assert!(r.dir.join(&bystander.golden).exists());
        for seg in &bystander.segments {
            assert!(store.segment_fields(*seg).unwrap()["deleted_at"].is_none());
            assert!(store.segment_embedding(*seg).unwrap().is_some());
        }
        drop(store);

        // The audit trail: the identity row carries what was here, and the
        // segments are logged in batches the way `delete.run` logs them.
        let ops = call(&r, r#"{"id":9,"method":"operations.list"}"#).unwrap();
        let rows: Vec<&Value> = ops["operations"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|o| o["op"] == "speakers.delete")
            .collect();
        let identity = rows
            .iter()
            .find(|o| o["prior_state"]["id"] == json!(doomed.id))
            .unwrap_or_else(|| panic!("no identity row in the audit log: {ops:#}"));
        assert_eq!(identity["prior_state"]["keep_voiceprint"], json!(false));
        assert_eq!(identity["prior_state"]["segments"], json!(3));
        assert_eq!(identity["prior_state"]["prototypes"], json!(3));
        assert_eq!(identity["prior_state"]["embeddings"], json!(3));
        assert_eq!(
            identity["prior_state"]["goldens"],
            json!([doomed.golden.clone()])
        );
        assert!(
            identity["prior_state"]["auto_label"].is_string(),
            "an audit that cannot name the voice cannot audit it: {identity:#}"
        );
        let logged: Vec<i64> = rows
            .iter()
            .filter(|o| o["prior_state"]["soft"] == json!(true))
            .flat_map(|o| {
                o["target_ids"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_i64().unwrap())
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(
            logged, doomed.segments,
            "the deleted rows are not auditable"
        );

        // And the views were told: the rows by id, the identity as a
        // disappearance rather than a merge.
        let evs = events(&r);
        let purged: Vec<i64> = evs
            .iter()
            .filter(|e| e["ev"] == "purge")
            .flat_map(|e| {
                e["data"]["ids"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_i64().unwrap())
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(purged, doomed.segments);
        assert!(
            evs.iter().any(|e| e["ev"] == "relabel"
                && e["data"]["speaker"] == json!(doomed.id)
                && e["data"]["pruned"] == json!(true)),
            "no view was told the voice stopped existing: {evs:#?}"
        );
    }

    #[test]
    fn keeping_the_voiceprint_takes_the_words_and_leaves_the_voice_matching() {
        let r = rig("delete-keep");
        let sess = a_session(&r);
        let kira = a_full_voice(&r, sess, "kira", 3);

        let out = delete_speaker(&r, kira.id, true).unwrap();
        assert_eq!(out["segments"], json!(3));
        assert_eq!(out["removed_speaker"], json!(false));
        assert_eq!(out["prototypes"], json!(0));
        assert_eq!(out["embeddings"], json!(0));
        assert!(
            out["msg"].as_str().unwrap().contains("voiceprint was kept"),
            "a delete that leaves something behind has to admit it: {out:#}"
        );

        let store = r.service.store();
        // The words are gone from every read path…
        for seg in &kira.segments {
            let f = store.segment_fields(*seg).unwrap();
            assert!(f["deleted_at"].is_some(), "segment {seg} survived");
            // …but the label stays: nothing was reassigned, it was deleted.
            assert_eq!(f["speaker_id"], Some(kira.id.to_string()));
        }
        assert_eq!(store.transcript(None, Some(kira.id)).unwrap().len(), 0);
        // …and the voice is still a voice.
        let summary = store.speaker_summary(kira.id).unwrap().unwrap();
        assert_eq!(summary.segments, 0);
        assert_eq!(store.prototype_count(kira.id).unwrap(), 3);
        assert_eq!(store.golden_samples_for(kira.id).unwrap().len(), 1);
        assert!(r.dir.join(&kira.golden).exists());
        assert!(
            store
                .prototypes("m@1")
                .unwrap()
                .iter()
                .any(|(sp, _)| *sp == kira.id),
            "the kept voice can no longer match future audio"
        );
        drop(store);
        // The conversation is still an index into nothing live, so it goes.
        assert!(!thread_exists(&r, kira.thread));

        // Deleting again is not a no-op the second time round: this is the
        // path out of the ghost state the bug left people in.
        let again = delete_speaker(&r, kira.id, false).unwrap();
        assert_eq!(again["segments"], json!(0));
        assert_eq!(again["removed_speaker"], json!(true));
        assert!(
            r.service
                .store()
                .speaker_summary(kira.id)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn your_own_pinned_voice_is_refused_and_the_refusal_names_the_switch() {
        let r = rig("delete-you");
        let sess = a_session(&r);
        let you = r.service.store().ensure_you_speaker(0).unwrap();
        a_clip(&r, sess, "mine", 4.0, Some((you, 0.9)));

        for keep in [true, false] {
            let e = delete_speaker(&r, you, keep).unwrap_err();
            assert_eq!(e.code, "refused");
            assert!(
                e.msg.contains("microphone"),
                "the refusal must name the switch that does work: {}",
                e.msg
            );
        }
        // Refused means refused: the pin, the row and the words are all still
        // there.
        assert_eq!(r.service.store().you_speaker_id().unwrap(), Some(you));
        assert_eq!(
            r.service
                .store()
                .speaker_summary(you)
                .unwrap()
                .unwrap()
                .segments,
            1
        );
    }

    #[test]
    fn a_merge_target_is_refused_by_count_and_its_tombstones_never_dangle() {
        let r = rig("delete-merge-target");
        let sess = a_session(&r);
        let target = a_full_voice(&r, sess, "target", 2);
        let one = a_full_voice(&r, sess, "one", 1);
        let two = a_full_voice(&r, sess, "two", 1);
        for from in [one.id, two.id] {
            r.service.store().merge_speakers(from, target.id).unwrap();
        }

        // The nuke path would leave two tombstones pointing at a row that is
        // not there. It is refused, and the refusal says how many.
        let e = delete_speaker(&r, target.id, false).unwrap_err();
        assert_eq!(e.code, "refused");
        assert!(
            e.msg.contains('2') && e.msg.contains("keep_voiceprint"),
            "the refusal must name the count and the way out: {}",
            e.msg
        );
        // A tombstone itself is not a voice, and says which one is.
        let e = delete_speaker(&r, one.id, false).unwrap_err();
        assert_eq!(e.code, "conflict");
        assert!(e.msg.contains(&target.id.to_string()));

        // The half that does work still works, and afterwards every tombstone
        // still resolves to a row that exists — one hop, no chains, no dangle.
        let out = delete_speaker(&r, target.id, true).unwrap();
        assert_eq!(out["removed_speaker"], json!(false));
        let store = r.service.store();
        for id in [one.id, two.id] {
            let canonical = store.resolve_speaker(id).unwrap();
            assert_eq!(canonical, target.id);
            assert!(
                store.speaker_name(canonical).unwrap().is_some(),
                "tombstone {id} points at a speaker that is not there"
            );
        }
        assert!(store.speaker_summary(target.id).unwrap().is_some());
        // Every merged voice's words went too: the target owns them now.
        assert_eq!(store.transcript(None, Some(target.id)).unwrap().len(), 0);
    }

    #[test]
    fn deleting_a_voice_that_is_not_there_is_not_found() {
        let r = rig("delete-missing");
        let e = delete_speaker(&r, 999_999, false).unwrap_err();
        assert_eq!(e.code, "not_found");
    }

    // ---- 0.6.1: storage --------------------------------------------------

    #[test]
    fn status_carries_the_storage_breakdown_the_sweeper_measured() {
        let r = rig("storage-status");
        // Nothing has swept yet: null, not zeroes. A client renders "not
        // measured yet"; zeroes would be a claim about an empty disk.
        let before = call(&r, r#"{"id":1,"method":"status"}"#).unwrap();
        assert_eq!(before["storage"], Value::Null);

        r.service
            .control
            .set_storage(crate::retention::StorageUsage {
                db_bytes: 100,
                audio_bytes: 200,
                audio_files: 2,
                goldens_bytes: 50,
                models_bytes: 700,
                total_bytes: 1050,
                measured_at_utc_ns: 42,
            });
        let after = call(&r, r#"{"id":2,"method":"status"}"#).unwrap();
        assert_eq!(after["storage"]["db_bytes"], json!(100));
        assert_eq!(after["storage"]["audio_files"], json!(2));
        assert_eq!(after["storage"]["total_bytes"], json!(1050));
        // The timestamp is a string, like every other nanosecond value on the
        // wire: 1.8e18 does not survive a JSON number in a browser.
        assert_eq!(after["storage"]["measured_at_utc_ns"], json!("42"));
    }

    // ---- 0.6.2: the memory graph, Tier 1 ---------------------------------

    /// A session of turns, threaded through the live path. `script` is one
    /// speaker per turn, five seconds apart — the shape the threading rule is
    /// specified on.
    fn a_conversation(rig: &Rig, script: &[i64]) -> Vec<i64> {
        const SEC: i64 = 1_000_000_000;
        let store = rig.service.store();
        let src = store.upsert_source("VRChat.exe", "VRChat.exe", 0).unwrap();
        let sess = store.begin_session(src, 0).unwrap();
        let cfg = crate::config::GraphConfig::default();
        let mut ids = Vec::new();
        for (i, speaker) in script.iter().enumerate() {
            let at = i as i64 * 5 * SEC;
            let seg = store
                .insert_segment(sess, at, at + 3 * SEC, "segments/x.wav", 0)
                .unwrap();
            store
                .set_segment_speaker(seg, Some(*speaker), Some(0.9))
                .unwrap();
            store
                .set_segment_analysis(
                    seg,
                    &SegmentAnalysis {
                        text: Some(format!("turn {i}")),
                        ..Default::default()
                    },
                )
                .unwrap();
            crate::threads::assign(&store, &cfg, seg).unwrap();
            ids.push(seg);
        }
        ids
    }

    #[test]
    fn person_get_answers_the_whole_page_in_one_round_trip() {
        let r = rig("person-get");
        let (a, b, c) = {
            let store = r.service.store();
            let a = store.create_speaker("A", 0).unwrap();
            store.rename_speaker(a, "Kira", 1).unwrap();
            let b = store.create_speaker("B", 0).unwrap();
            let c = store.create_speaker("C", 0).unwrap();
            (a, b, c)
        };
        // A and B talk; C says one thing beside them, in their own thread.
        a_conversation(&r, &[a, b, a, b, c]);

        let p = call(
            &r,
            &format!(r#"{{"id":1,"method":"person.get","params":{{"id":{a}}}}}"#),
        )
        .unwrap();
        assert_eq!(p["id"], json!(a));
        assert_eq!(p["speaker"]["name"], json!("Kira"));
        assert_eq!(p["speaker"]["you"], json!(false));
        assert_eq!(p["totals"]["segments"], json!(2));
        assert_eq!(p["totals"]["sessions"], json!(1));
        assert_eq!(p["totals"]["threads"], json!(1));
        // Nanoseconds are strings on the wire; milliseconds are for rendering.
        assert!(p["totals"]["speech_ns"].is_string());
        assert!(p["totals"]["first_heard_ms"].is_number());

        let edges = p["edges"].as_array().unwrap();
        assert_eq!(edges.len(), 1, "C was never in A's conversation");
        assert_eq!(edges[0]["speaker_id"], json!(b));
        assert_eq!(edges[0]["threads"], json!(1));
        assert_eq!(edges[0]["seconds"], json!(6.0));
        assert!(edges[0]["last_ns"].is_string());
        // No roster in this database, so nothing is claimed about one.
        assert_eq!(edges[0]["roster_seconds"], Value::Null);

        let threads = p["recent_threads"].as_array().unwrap();
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0]["preview"], json!("turn 0"));
        let names: Vec<i64> = threads[0]["participants"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["speaker_id"].as_i64().unwrap())
            .collect();
        assert_eq!(names, vec![a, b]);
        assert_eq!(threads[0]["participants"][0]["name"], json!("Kira"));

        // A voice that is not there is not found, rather than an empty page.
        let e = call(&r, r#"{"id":2,"method":"person.get","params":{"id":9999}}"#).unwrap_err();
        assert_eq!(e.code, "not_found");
    }

    #[test]
    fn thread_get_returns_the_conversation_in_the_ordinary_segment_shape() {
        let r = rig("thread-get");
        let (a, b) = {
            let store = r.service.store();
            (
                store.create_speaker("A", 0).unwrap(),
                store.create_speaker("B", 0).unwrap(),
            )
        };
        let ids = a_conversation(&r, &[a, b, a, b]);
        let thread = {
            let store = r.service.store();
            store
                .segment_row(ids[0])
                .unwrap()
                .unwrap()
                .thread_id
                .unwrap()
        };

        let t = call(
            &r,
            &format!(r#"{{"id":1,"method":"thread.get","params":{{"id":{thread}}}}}"#),
        )
        .unwrap();
        assert_eq!(t["thread_id"], json!(thread));
        let segs = t["segments"].as_array().unwrap();
        assert_eq!(
            segs.iter()
                .map(|s| s["id"].as_i64().unwrap())
                .collect::<Vec<_>>(),
            ids
        );
        // The ordinary shape, so a client renders it with the code it has —
        // and every row names the thread it is in.
        assert_eq!(segs[0]["thread"], json!(thread));
        assert!(segs[0]["t_ns"].is_string());
        assert_eq!(t["participants"].as_array().unwrap().len(), 2);

        let e = call(&r, r#"{"id":2,"method":"thread.get","params":{"id":404}}"#).unwrap_err();
        assert_eq!(e.code, "not_found");
    }

    #[test]
    fn two_conversations_at_once_come_back_as_two() {
        let r = rig("two-threads");
        let (a, b, c, d) = {
            let store = r.service.store();
            (
                store.create_speaker("A", 0).unwrap(),
                store.create_speaker("B", 0).unwrap(),
                store.create_speaker("C", 0).unwrap(),
                store.create_speaker("D", 0).unwrap(),
            )
        };
        a_conversation(&r, &[a, b, a, b, c, d, c, d]);

        let p = call(
            &r,
            &format!(r#"{{"id":1,"method":"person.get","params":{{"id":{a}}}}}"#),
        )
        .unwrap();
        let edges: Vec<i64> = p["edges"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["speaker_id"].as_i64().unwrap())
            .collect();
        assert_eq!(
            edges,
            vec![b],
            "sharing a room is not sharing a conversation"
        );
        assert_eq!(p["totals"]["threads"], json!(1));

        let p = call(
            &r,
            &format!(r#"{{"id":2,"method":"person.get","params":{{"id":{c}}}}}"#),
        )
        .unwrap();
        let edges: Vec<i64> = p["edges"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["speaker_id"].as_i64().unwrap())
            .collect();
        assert_eq!(edges, vec![d]);
    }

    #[test]
    fn a_transcript_row_names_the_conversation_it_belongs_to() {
        let r = rig("segment-thread-field");
        let a = {
            let store = r.service.store();
            store.create_speaker("A", 0).unwrap()
        };
        a_conversation(&r, &[a, a]);
        let t = call(&r, r#"{"id":1,"method":"transcript","params":{}}"#).unwrap();
        let segs = t["segments"].as_array().unwrap();
        assert!(segs.iter().all(|s| s["thread"].is_i64()));
        assert_eq!(segs[0]["thread"], segs[1]["thread"]);
    }

    // ---- audit finding #8: writes onto a tombstone ------------------------

    /// A tombstone holds no rows. `speakers.name` wrote a display name onto one
    /// anyway — invisible, because every read resolves through
    /// `speaker_resolved` — and then reported success and broadcast a `relabel`
    /// for it, from which clients dutifully created a voice that does not
    /// exist. `speakers.split` has always refused this; these two did not.
    #[test]
    fn naming_a_tombstone_is_refused_and_names_the_voice_that_holds_the_rows() {
        let r = rig("name-tombstone");
        let FalseMerge {
            kept, tombstone, ..
        } = a_false_merge(&r);
        call(
            &r,
            &format!(
                r#"{{"id":1,"method":"speakers.name","params":{{"id":{kept},"name":"Ines"}}}}"#
            ),
        )
        .unwrap();
        events(&r);

        let e = call(
            &r,
            &format!(
                r#"{{"id":2,"method":"speakers.name","params":{{"id":{tombstone},"name":"Kestrel"}}}}"#
            ),
        )
        .unwrap_err();
        assert_eq!(e.code, "conflict");
        assert!(
            e.msg.contains(&kept.to_string()),
            "the refusal has to say which voice to use instead: {}",
            e.msg
        );
        assert!(
            events(&r).is_empty(),
            "and no relabel goes out, or clients draw a voice that is not there"
        );
        let store = r.service.store();
        assert_eq!(
            store.speaker_name(kept).unwrap().as_deref(),
            Some("Ines"),
            "the surviving voice keeps its name"
        );
        assert_ne!(
            store.speaker_name(tombstone).unwrap().as_deref(),
            Some("Kestrel")
        );
    }

    /// Worse than the rename, and the reason it is worth its own test:
    /// `speaker_languages` READS through the tombstone to the canonical voice
    /// while `set_speaker_languages` WROTE the tombstone. So the call changed
    /// nothing, answered as though it had, and filed the *canonical* voice's
    /// languages as the prior state — the audit trail lied too.
    #[test]
    fn declaring_languages_on_a_tombstone_is_refused_rather_than_written_nowhere() {
        let r = rig("langs-tombstone");
        let FalseMerge {
            kept, tombstone, ..
        } = a_false_merge(&r);
        call(
            &r,
            &format!(
                r#"{{"id":1,"method":"speakers.set_languages","params":{{"id":{kept},"languages":["de"]}}}}"#
            ),
        )
        .unwrap();
        events(&r);

        let e = call(
            &r,
            &format!(
                r#"{{"id":2,"method":"speakers.set_languages","params":{{"id":{tombstone},"languages":["en"]}}}}"#
            ),
        )
        .unwrap_err();
        assert_eq!(e.code, "conflict");
        assert!(e.msg.contains(&kept.to_string()));
        assert!(events(&r).is_empty());

        let store = r.service.store();
        assert_eq!(
            store.speaker_languages(kept).unwrap(),
            Some(vec!["de".to_string()]),
            "the canonical voice's declaration is untouched"
        );
        // Reading the tombstone resolves to the canonical voice, which is
        // exactly why writing it was invisible.
        assert_eq!(
            store.speaker_languages(tombstone).unwrap(),
            Some(vec!["de".to_string()])
        );
        let ops = store.operations(10).unwrap();
        assert!(
            !ops.iter().any(|o| o.op == "speakers.set_languages"
                && o.target_ids.contains(&tombstone.to_string())),
            "and nothing was filed about a write that did not happen"
        );
    }

    // ---- audit finding #10: deleted content coming back -------------------

    /// Delete, then split. `segment_row` had no `deleted_at` filter, and it is
    /// what the `segment` events are built from — so a split re-broadcast the
    /// deleted rows into every open client, words and all. `split_speaker`'s
    /// UPDATEs had no guard either, so it really did relabel them.
    #[test]
    fn splitting_a_voice_never_broadcasts_the_rows_that_were_deleted() {
        let r = rig("split-after-delete");
        let FalseMerge {
            kept, ours, theirs, ..
        } = a_false_merge(&r);

        // One turn from each side of the false merge is thrown away first.
        let (gone_ours, gone_theirs) = (ours[0], theirs[0]);
        {
            let store = r.service.store();
            store
                .soft_delete_segments(&[gone_ours, gone_theirs], 1_000)
                .unwrap();
        }
        events(&r);

        let out = split(&r, kept).unwrap();
        let minted = out["minted"].as_i64().unwrap();

        let announced: Vec<i64> = events(&r)
            .iter()
            .filter(|e| e["ev"] == "segment")
            .filter_map(|e| e["data"]["id"].as_i64())
            .collect();
        assert!(
            !announced.contains(&gone_ours) && !announced.contains(&gone_theirs),
            "a deleted turn must never come back as a segment event: {announced:?}"
        );
        assert!(
            !announced.is_empty(),
            "the live rows are still announced, or this proves nothing"
        );

        let store = r.service.store();
        // The deleted rows kept the speaker they had; the split did not touch
        // them, so undoing the delete cannot resurrect them onto a voice they
        // were never on.
        for seg in [gone_ours, gone_theirs] {
            assert!(
                store.segment_row(seg).unwrap().is_none(),
                "and they are still invisible to every read path"
            );
        }
        let moved = store.segment_labels(&theirs).unwrap();
        assert!(
            moved
                .iter()
                .filter(|l| l.segment_id != gone_theirs)
                .all(|l| l.speaker_id == Some(minted)),
            "the live rows still moved: the guard is about deleted ones only"
        );
        assert_eq!(
            moved
                .iter()
                .find(|l| l.segment_id == gone_theirs)
                .unwrap()
                .speaker_id,
            Some(kept),
            "the deleted row was left exactly where it was"
        );
    }

    /// The other two doors into the same room: a soft-deleted segment could
    /// still be reassigned and re-worded, and each write broadcast the deleted
    /// row as a live one.
    #[test]
    fn a_deleted_segment_cannot_be_reassigned_or_corrected() {
        let r = rig("edit-after-delete");
        let (_, seg) = a_segment(&r, "something said");
        let spk = {
            let store = r.service.store();
            store.create_speaker("Ines", 0).unwrap()
        };
        r.service
            .store()
            .soft_delete_segments(&[seg], 1_000)
            .unwrap();
        events(&r);

        let e = call(
            &r,
            &format!(
                r#"{{"id":1,"method":"segments.reassign","params":{{"segment_id":{seg},"speaker_id":{spk}}}}}"#
            ),
        )
        .unwrap_err();
        assert_eq!(e.code, "not_found");

        let e = call(
            &r,
            &format!(
                r#"{{"id":2,"method":"segments.correct","params":{{"segment_id":{seg},"text":"rewritten"}}}}"#
            ),
        )
        .unwrap_err();
        assert_eq!(e.code, "not_found");

        assert!(
            events(&r).is_empty(),
            "a refused edit announces nothing, least of all a deleted row"
        );
        let store = r.service.store();
        let (speaker, text) = store
            .segment_fields(seg)
            .map(|f| (f.get("speaker_id").cloned(), f.get("text").cloned()))
            .unwrap();
        assert!(speaker.flatten().is_none(), "nothing was assigned");
        assert_eq!(
            text.flatten().as_deref(),
            Some("something said"),
            "and nothing was rewritten"
        );
    }

    // ---- audit finding #15: threads that outlive their conversation -------

    /// A fully deleted conversation left a `threads` row that `thread.get`
    /// answered with `segments: []`, `participants: []`, `preview: null` —
    /// a conversation-shaped hole in the UI. `prune_empty_threads` existed to
    /// clean exactly this up and had no callers anywhere.
    #[test]
    fn deleting_every_turn_in_a_conversation_removes_the_conversation() {
        let r = rig("thread-after-delete");
        let a = {
            let store = r.service.store();
            store.create_speaker("A", 0).unwrap()
        };
        let segs = a_conversation(&r, &[a, a, a]);
        let thread = {
            let store = r.service.store();
            store.segment_row(segs[0]).unwrap().unwrap().thread_id
        }
        .expect("the fixture threads its turns");

        let got = call(
            &r,
            &format!(r#"{{"id":1,"method":"thread.get","params":{{"id":{thread}}}}}"#),
        )
        .unwrap();
        assert_eq!(got["segments"].as_array().unwrap().len(), 3);

        let out = call(
            &r,
            &format!(
                r#"{{"id":2,"method":"delete.run","params":{{"speaker":{a},"confirm_everything":false}}}}"#
            ),
        )
        .unwrap();
        assert_eq!(out["segments"], 3);
        // `delete.run` answers with an operation handle and finishes on its own
        // thread; wait for it the way a client would.
        let op = out["op"].as_str().unwrap().to_string();
        for _ in 0..300 {
            if r.service.op_state(&op) == Some(OpState::Done) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(r.service.op_state(&op), Some(OpState::Done));

        let e = call(
            &r,
            &format!(r#"{{"id":3,"method":"thread.get","params":{{"id":{thread}}}}}"#),
        )
        .unwrap_err();
        assert_eq!(
            e.code, "not_found",
            "a conversation with nothing left in it is gone, not empty"
        );
        assert!(
            r.service.store().thread_summary(thread).unwrap().is_none(),
            "and the row went with it"
        );
    }

    // ---- audit finding #20: a sequence number from another daemon ---------

    /// Sequence numbers restart at 0 on every start and `events.since` could
    /// only reject a `seq` *higher* than the live counter. So a client that
    /// remembered seq 3 across a restart, reconnecting once the new daemon had
    /// published more than that, was handed events 4..N of a completely
    /// different stream and applied them as its own history.
    #[test]
    fn a_sequence_number_from_an_earlier_run_is_a_resync_not_a_replay() {
        let r = rig("boot-id");
        let welcome: Value = serde_json::from_slice(&crate::proto::welcome(
            r.service.bus.current_seq(),
            crate::store::SCHEMA_VERSION,
        ))
        .unwrap();
        let boot = welcome["welcome"]["boot"].as_str().unwrap().to_string();
        assert!(!boot.is_empty(), "the welcome has to name the run");

        for i in 0..5 {
            r.service
                .bus
                .publish(Topic::Segments, "segment", json!({"id": i}));
        }

        // The ordinary catch-up, with the boot id this connection was welcomed
        // with: served.
        let out = call(
            &r,
            &format!(r#"{{"id":1,"method":"events.since","params":{{"seq":2,"boot":"{boot}"}}}}"#),
        )
        .unwrap();
        assert_eq!(out["replayed"], 3);
        assert_eq!(out["boot"], json!(boot));

        // The same seq, remembered from a daemon that is no longer running. It
        // is well inside this daemon's ring — that is exactly the trap — and it
        // must not be served.
        let e = call(
            &r,
            r#"{"id":2,"method":"events.since","params":{"seq":2,"boot":"0000000000000000"}}"#,
        )
        .unwrap_err();
        assert_eq!(e.code, "resync");
        assert!(
            e.msg.contains("earlier run"),
            "the message has to say why: {}",
            e.msg
        );

        // A client that never learned about boot ids still works, because the
        // parameter is optional — it just does not get this protection.
        assert!(call(&r, r#"{"id":3,"method":"events.since","params":{"seq":2}}"#).is_ok());
    }

    // ---- audit finding #2 / #25: what a reply and an event actually say ---

    /// A toggle now carries the whole `sources.list` row, the same shape the
    /// capture thread publishes when a source appears or starts being captured
    /// — so a client folds a toggle in exactly as it folds in an arrival.
    #[test]
    fn a_source_event_carries_the_whole_row_not_just_the_switch() {
        let r = rig("source-event-shape");
        {
            let store = r.service.store();
            store.upsert_source("VRChat.exe", "VRChat", 0).unwrap();
        }
        events(&r);
        call(
            &r,
            r#"{"id":1,"method":"sources.set","params":{"match_key":"VRChat.exe","allowed":true}}"#,
        )
        .unwrap();

        let ev = events(&r)
            .into_iter()
            .find(|e| e["ev"] == "source")
            .expect("a toggle is broadcast");
        let data = &ev["data"];
        assert_eq!(data["match_key"], json!("VRChat.exe"));
        assert_eq!(data["allowed"], json!(true));
        assert_eq!(data["display"], json!("VRChat"));
        assert_eq!(data["kind"], json!(crate::store::KIND_APP));
        assert_eq!(data["streams"], json!(0));
        assert_eq!(data["state"], json!(crate::capture::SOURCE_SEEN));
        assert!(data["id"].is_i64());

        // And it is the list's shape, field for field.
        let listed = call(&r, r#"{"id":2,"method":"sources.list"}"#).unwrap();
        let row = listed["sources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["match_key"] == json!("VRChat.exe"))
            .unwrap()
            .clone();
        let mut expected = row;
        expected["state"] = data["state"].clone();
        assert_eq!(data, &expected);
    }

    /// `speakers.prune` reported `count` as the successes and `voices` as the
    /// whole preview, so the two fields could disagree about what happened.
    /// The client had already shown the preview in its confirmation dialog;
    /// what it needs back is what actually went.
    #[test]
    fn prune_answers_with_the_voices_it_removed_not_the_ones_it_offered() {
        let r = rig("prune-reply");
        let sess = a_session(&r);
        for tag in ["a", "b"] {
            let spk = r.service.store().mint_speaker(0).unwrap();
            let store = r.service.store();
            let t = store.segments_total().unwrap() * 1_000;
            let seg = store
                .insert_segment(sess, t, t + 500_000_000, &format!("segments/{tag}.wav"), 0)
                .unwrap();
            store
                .set_segment_speaker(seg, Some(spk), Some(0.9))
                .unwrap();
        }

        let preview = call(&r, r#"{"id":1,"method":"speakers.prune"}"#).unwrap();
        assert_eq!(preview["apply"], json!(false));
        let offered = preview["voices"].as_array().unwrap().len();
        assert!(offered >= 2, "the fixture has to produce candidates");

        let out = call(
            &r,
            r#"{"id":2,"method":"speakers.prune","params":{"apply":true}}"#,
        )
        .unwrap();
        let removed: Vec<i64> = out["removed"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_i64().unwrap())
            .collect();
        let voices = out["voices"].as_array().unwrap();
        assert_eq!(
            voices.len(),
            out["count"].as_u64().unwrap() as usize,
            "`voices` and `count` describe the same set or they describe nothing"
        );
        let listed: Vec<i64> = voices.iter().map(|v| v["id"].as_i64().unwrap()).collect();
        assert_eq!(listed, removed);
    }

    /// `vocab.set` replaces the glossary, `vocab.get` reports it together with
    /// what the daemon noticed for itself, and the change is announced.
    ///
    /// The last assertion is the one that matters: the reply says out loud that
    /// nothing is biased by the list. `spike/hotwords_bench.py` is why (+9.1%
    /// relative recall against a +20% gate, and a glossary that bleeds into
    /// unrelated turns at the strongest setting), and a client showing a
    /// glossary screen must not imply an effect the daemon does not have.
    #[test]
    fn the_glossary_round_trips_and_announces_itself() {
        let r = rig("vocab");
        let (segment, _) = a_segment(&r, "we were in the great pug");
        call(
            &r,
            &format!(
                r#"{{"id":1,"method":"segments.correct","params":{{"segment_id":{segment},"text":"we were in Kübras Welt"}}}}"#
            ),
        )
        .unwrap();
        let _ = events(&r);

        let out = call(
            &r,
            r#"{"id":2,"method":"vocab.set","params":{"terms":["Vergaberecht","  spaced   term  ",""]}}"#,
        )
        .unwrap();
        assert_eq!(out["user"], json!(["Vergaberecht", "spaced term"]));
        assert_eq!(
            out["applied_to_decoder"],
            json!(false),
            "the list is assembled and served; nothing is biased by it"
        );
        let announced: Vec<Value> = events(&r)
            .into_iter()
            .filter(|e| e["ev"] == "vocab")
            .collect();
        assert_eq!(announced.len(), 1, "a change is announced exactly once");

        let got = call(&r, r#"{"id":3,"method":"vocab.get"}"#).unwrap();
        assert_eq!(got["user"], json!(["Vergaberecht", "spaced term"]));
        let corrections: Vec<&str> = got["auto"]["corrections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(
            corrections.contains(&"Kübras") && corrections.contains(&"Welt"),
            "the words a correction ADDED are the vocabulary: {corrections:?}"
        );
        let effective: Vec<&str> = got["effective"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(
            effective.first(),
            Some(&"Vergaberecht"),
            "what a person typed outranks what the daemon noticed"
        );
        assert!(effective.contains(&"Kübras"));
    }

    /// A glossary that is not a list of strings is a client bug, and it is
    /// refused rather than half-applied.
    #[test]
    fn a_glossary_of_the_wrong_shape_is_refused() {
        let r = rig("vocab-shape");
        assert!(
            call(
                &r,
                r#"{"id":1,"method":"vocab.set","params":{"terms":"Kübra"}}"#
            )
            .is_err()
        );
        assert!(
            call(
                &r,
                r#"{"id":2,"method":"vocab.set","params":{"terms":[1,2]}}"#
            )
            .is_err()
        );
        assert!(call(&r, r#"{"id":3,"method":"vocab.set"}"#).is_err());
    }

    // ---- 0.8.0: search.ask, notes.*, person.brief, accuracy.summary ------

    /// A turn at a chosen instant, so a question with a time facet has
    /// something inside its window and something outside it.
    fn a_turn_at(rig: &Rig, session: i64, t_ns: i64, speaker: Option<i64>, text: &str) -> i64 {
        let store = rig.service.store();
        let seg = store
            .insert_segment(session, t_ns, t_ns + 1_000_000_000, "segments/a.wav", 0)
            .unwrap();
        store
            .set_segment_analysis(
                seg,
                &SegmentAnalysis {
                    text: Some(text.into()),
                    ..Default::default()
                },
            )
            .unwrap();
        if let Some(spk) = speaker {
            store
                .set_segment_speaker(seg, Some(spk), Some(0.9))
                .unwrap();
        }
        seg
    }

    /// Local noon, `delta` days from today — the same calendar `ask` resolves
    /// against, so "gestern" in a question and this instant agree.
    fn local_noon(delta: i64) -> i64 {
        let now = utc_now_ns();
        let offset = crate::clock::local_offset_s(now);
        let today = (now.div_euclid(1_000_000_000) + offset).div_euclid(86_400);
        (((today + delta) * 86_400 + 12 * 3600) - offset) * 1_000_000_000
    }

    #[test]
    fn ask_says_what_it_understood_and_searches_with_it() {
        let r = rig("ask");
        let session = a_session(&r);
        let aspen = {
            let store = r.service.store();
            let id = store.create_speaker("Speaker_01", 0).unwrap();
            store.rename_speaker(id, "Aspen", 1).unwrap();
            id
        };
        let other = r.service.store().mint_speaker(0).unwrap();

        let wanted = a_turn_at(
            &r,
            session,
            local_noon(-1),
            Some(aspen),
            "the shader compiles now",
        );
        // Right words, right voice, wrong day.
        a_turn_at(
            &r,
            session,
            local_noon(-5),
            Some(aspen),
            "the shader was broken",
        );
        // Right words, right day, wrong voice.
        a_turn_at(
            &r,
            session,
            local_noon(-1),
            Some(other),
            "the shader is fine by me",
        );

        let out = call(
            &r,
            r#"{"id":1,"method":"search.ask","params":{"q":"was hat Aspen gestern über den shader gesagt?"}}"#,
        )
        .unwrap();
        let i = &out["interpretation"];
        assert_eq!(i["speaker_id"], json!(aspen));
        assert_eq!(i["speaker_label"], json!("Aspen"));
        assert_eq!(i["query"], json!("shader"), "the residual is the query");
        // Nanoseconds are strings on the wire, milliseconds are for rendering.
        assert!(i["from_ns"].is_string() && i["to_ns"].is_string());
        assert!(i["from_ms"].is_number());
        // No semantic model in a test rig, and that is not an error: the
        // question still gets a keyword answer, and the mode says so.
        assert_eq!(i["mode"], json!("fts"));

        let hits = out["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1, "both facets narrowed: {hits:?}");
        assert_eq!(hits[0]["id"], json!(wanted));
        assert!(hits[0]["snippet"].as_str().unwrap().contains("shader"));
    }

    #[test]
    fn a_question_that_is_all_facets_answers_with_the_transcript_it_selects() {
        let r = rig("ask-facets");
        let session = a_session(&r);
        let aspen = {
            let store = r.service.store();
            let id = store.create_speaker("Speaker_01", 0).unwrap();
            store.rename_speaker(id, "Aspen", 1).unwrap();
            id
        };
        a_turn_at(&r, session, local_noon(-1), Some(aspen), "one");
        a_turn_at(&r, session, local_noon(-1), Some(aspen), "two");
        a_turn_at(&r, session, local_noon(-4), Some(aspen), "long ago");

        let out = call(
            &r,
            r#"{"id":1,"method":"search.ask","params":{"q":"what did Aspen say yesterday"}}"#,
        )
        .unwrap();
        assert_eq!(out["interpretation"]["query"], json!(""));
        assert_eq!(out["interpretation"]["mode"], json!("facets"));
        let texts: Vec<&str> = out["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["text"].as_str().unwrap())
            .collect();
        assert_eq!(
            texts,
            vec!["two", "one"],
            "newest first, and only yesterday"
        );
    }

    #[test]
    fn ask_refuses_an_empty_question_rather_than_returning_everything() {
        let r = rig("ask-empty");
        let e = call(&r, r#"{"id":1,"method":"search.ask","params":{"q":"   "}}"#).unwrap_err();
        assert_eq!(e.code, "params");
    }

    #[test]
    fn notes_are_listed_moved_and_broadcast() {
        let r = rig("notes");
        let session = a_session(&r);
        let seg = a_turn_at(
            &r,
            session,
            local_noon(0),
            None,
            "Recall, merk dir: den Shader von Aspen fragen",
        );
        // What the pipeline's one call does, with the same guards.
        crate::notes::maybe_capture(&r.service.store(), &r.service.bus, seg, true, 4_2);
        let evs = events(&r);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0]["ev"], "note");
        assert_eq!(evs[0]["data"]["text"], json!("den Shader von Aspen fragen"));

        let out = call(&r, r#"{"id":1,"method":"notes.list"}"#).unwrap();
        let notes = out["notes"].as_array().unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0]["state"], json!("open"));
        assert_eq!(notes[0]["segment_id"], json!(seg));
        // The moment it was SAID, not the moment the row was written.
        assert_eq!(notes[0]["t_ns"], json!(local_noon(0).to_string()));
        let id = notes[0]["id"].as_i64().unwrap();

        let moved = call(
            &r,
            &format!(
                r#"{{"id":2,"method":"notes.set_state","params":{{"id":{id},"state":"done"}}}}"#
            ),
        )
        .unwrap();
        assert_eq!(moved["state"], json!("done"));
        let evs = events(&r);
        assert_eq!(
            evs[0]["ev"], "note",
            "a decision is broadcast like any other"
        );
        assert_eq!(evs[0]["data"]["state"], json!("done"));

        // And the list filters by state.
        let open = call(
            &r,
            r#"{"id":3,"method":"notes.list","params":{"state":"open"}}"#,
        )
        .unwrap();
        assert!(open["notes"].as_array().unwrap().is_empty());
        let done = call(
            &r,
            r#"{"id":4,"method":"notes.list","params":{"state":"done"}}"#,
        )
        .unwrap();
        assert_eq!(done["notes"].as_array().unwrap().len(), 1);

        // The turn itself is still in the transcript. A note annotates; it
        // never rewrites.
        let t = call(&r, r#"{"id":5,"method":"transcript"}"#).unwrap();
        assert_eq!(
            t["segments"][0]["text"],
            json!("Recall, merk dir: den Shader von Aspen fragen")
        );
    }

    #[test]
    fn a_note_that_is_not_there_and_a_state_that_is_not_real_are_both_refused() {
        let r = rig("notes-bad");
        let e = call(
            &r,
            r#"{"id":1,"method":"notes.set_state","params":{"id":1,"state":"done"}}"#,
        )
        .unwrap_err();
        assert_eq!(e.code, "not_found");
        let e = call(
            &r,
            r#"{"id":2,"method":"notes.set_state","params":{"id":1,"state":"maybe"}}"#,
        )
        .unwrap_err();
        assert_eq!(e.code, "params");
        let e = call(
            &r,
            r#"{"id":3,"method":"notes.list","params":{"state":"maybe"}}"#,
        )
        .unwrap_err();
        assert_eq!(e.code, "params");
        assert!(events(&r).is_empty(), "a refusal broadcasts nothing");
    }

    #[test]
    fn a_brief_answers_the_card_in_one_round_trip() {
        let r = rig("brief");
        let session = a_session(&r);
        let (you, them) = {
            let store = r.service.store();
            let you = store.ensure_you_speaker(0).unwrap();
            let them = store.create_speaker("Speaker_02", 0).unwrap();
            store.rename_speaker(them, "Aspen", 1).unwrap();
            (you, them)
        };
        let theirs = a_turn_at(
            &r,
            session,
            local_noon(-1),
            Some(them),
            "i will send you the shader tomorrow",
        );
        {
            let store = r.service.store();
            store
                .upsert_commitment(
                    &crate::store::NewCommitment {
                        segment_id: theirs,
                        thread_id: None,
                        who_speaker_id: Some(them),
                        to_speaker_id: Some(you),
                        what: "send the shader".into(),
                        due_utc_ns: None,
                        due_raw: None,
                        due_kind: None,
                        source: crate::store::commitment_source::RULES,
                        model_id: None,
                        confidence: 0.5,
                    },
                    0,
                )
                .unwrap();
        }
        let mine = a_turn_at(
            &r,
            session,
            local_noon(0),
            Some(you),
            "Recall, note ask Aspen about the portal",
        );
        crate::notes::maybe_capture(&r.service.store(), &r.service.bus, mine, true, 0);

        let b = call(
            &r,
            &format!(r#"{{"id":1,"method":"person.brief","params":{{"id":{them}}}}}"#),
        )
        .unwrap();
        assert_eq!(b["speaker"]["name"], json!("Aspen"));
        assert_eq!(b["speaker"]["you"], json!(false));
        assert!(b["last_heard_ms"].is_number());
        assert_eq!(
            b["open_to_you"].as_array().unwrap().len(),
            1,
            "what they promised is on the card: {b}"
        );
        assert_eq!(
            b["notes_mentioning"][0]["text"],
            json!("ask Aspen about the portal")
        );

        let e = call(
            &r,
            r#"{"id":2,"method":"person.brief","params":{"id":9999}}"#,
        )
        .unwrap_err();
        assert_eq!(e.code, "not_found");
    }

    #[test]
    fn accuracy_is_measured_from_the_corrections_and_from_nothing_else() {
        let r = rig("accuracy");
        let (_, seg) = a_segment(&r, "the belt holds the line");

        let empty = call(&r, r#"{"id":1,"method":"accuracy.summary"}"#).unwrap();
        assert_eq!(empty["corrections"], json!(0));
        assert_eq!(empty["estimated_wer"], Value::Null, "not zero");

        call(
            &r,
            &format!(
                r#"{{"id":2,"method":"segments.correct","params":{{"segment_id":{seg},"text":"the bell holds the line"}}}}"#
            ),
        )
        .unwrap();

        let a = call(&r, r#"{"id":3,"method":"accuracy.summary"}"#).unwrap();
        assert_eq!(a["corrections"], json!(1));
        assert_eq!(a["estimated_wer"], json!(0.2), "one word in five");
        assert_eq!(a["by_source"][0]["source"], json!("VRChat.exe"));
        assert!(a["since_ns"].is_string());
    }
}
