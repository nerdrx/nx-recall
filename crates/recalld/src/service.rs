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
}

impl Service {
    pub fn new(store: Arc<Mutex<Store>>, control: Arc<Control>, bus: Arc<Bus>) -> Arc<Self> {
        Arc::new(Self {
            store,
            control,
            bus,
            next_op: AtomicU64::new(1),
            ops: Mutex::new(HashMap::new()),
        })
    }

    fn store(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|p| p.into_inner())
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
            "speakers.merge" => self.speakers_merge(req),
            "speakers.split" => self.speakers_split(req),
            "speakers.sample" => self.speakers_sample(req),
            "segments.reassign" => self.segments_reassign(req),
            "segments.correct" => self.segments_correct(req),
            "segments.audio" => self.segments_audio(req),
            "search" => self.search(req),
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
        match self.bus.events_since(client, since as u64) {
            Ok((events, seq)) => Ok(json!({
                "events": events,
                "replayed": events.len(),
                "seq": seq,
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
            "sources_allowed": allowed,
            "sources_capturing": capturing,
            // The whole microphone answer in one place, plus the flat string
            // for anything that only wants to print a word (PROTOCOL).
            "mic": c.mic_json(),
            "mic_state": c.mic_state(),
            "segments_total": store.segments_total()?,
            "counters": {
                "sessions_opened": c.stats.sessions_opened.load(Ordering::Relaxed),
                "segments_written": c.stats.segments_written.load(Ordering::Relaxed),
                "frames_analysed": c.stats.frames_analysed.load(Ordering::Relaxed),
                "analysed": c.analysis.analysed.load(Ordering::Relaxed),
                "labelled": c.analysis.labelled.load(Ordering::Relaxed),
                "refused_overlap": c.analysis.refused_overlap.load(Ordering::Relaxed),
                "mic_segments": c.analysis.mic_segments.load(Ordering::Relaxed),
                "mic_enrolled": c.analysis.mic_enrolled.load(Ordering::Relaxed),
                "mic_goldens": c.analysis.mic_goldens.load(Ordering::Relaxed),
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
                .into_iter()
                .map(|r| {
                    let is_mic = r.kind == crate::store::KIND_MIC;
                    json!({
                        "id": r.id,
                        "match_key": r.match_key,
                        // "app" or "mic" (schema v4). The microphone is a row
                        // here like anything else and is governed by `[mic]`
                        // rather than by the allowlist, so a client that does
                        // not know the difference must not render it as an app.
                        "kind": r.kind,
                        // `binary` is the process this was keyed on; `display`
                        // is what the application calls itself. For a Wine
                        // program these differ, which is exactly why the key is
                        // the PE name and not the shared loader (DESIGN §3).
                        "binary": r.match_key,
                        "display": r.display_name,
                        "display_name": r.display_name,
                        // The live answer, which may be ahead of the database
                        // for the instant between a toggle and its mirror. The
                        // microphone's switch is `[mic].enabled`, never a rule.
                        "allowed": if is_mic { mic.enabled } else { live.decide(&r.match_key).captures() },
                        "first_seen": iso8601(r.first_seen),
                        "last_seen": iso8601(r.last_seen),
                        "streams": r.streams,
                    })
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
        self.bus.publish(
            Topic::Sources,
            "source",
            json!({"match_key": match_key, "allowed": allowed}),
        );
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
                    "first_seen": iso8601(r.created_at),
                    "segments": r.segments,
                    "total_ms": ns_to_ms(r.speech_ns),
                    "speech_ns": r.speech_ns.to_string(),
                }))
                .collect::<Vec<_>>(),
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
        let hits = self
            .store()
            .search_filtered(&q, &filter, limit)
            // A malformed FTS query is the caller's problem, not a daemon fault.
            .map_err(|e| Error::new("params", format!("{e:#}")))?;
        Ok(json!({
            "total": hits.len(),
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

        let run = call(&r, r#"{"id":2,"method":"delete.run"}"#).unwrap();
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
}
