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
            "speakers.list" => self.speakers_list(),
            "speakers.name" => self.speakers_name(req),
            "speakers.merge" => self.speakers_merge(req),
            "speakers.split" => Err(Error::new(
                "unimplemented",
                "speakers.split lands with the re-cluster step; \
                 speakers.merge and segments.reassign are the corrections available now",
            )),
            "segments.reassign" => self.segments_reassign(req),
            "segments.correct" => self.segments_correct(req),
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
            "segments_total": store.segments_total()?,
            "counters": {
                "sessions_opened": c.stats.sessions_opened.load(Ordering::Relaxed),
                "segments_written": c.stats.segments_written.load(Ordering::Relaxed),
                "frames_analysed": c.stats.frames_analysed.load(Ordering::Relaxed),
                "analysed": c.analysis.analysed.load(Ordering::Relaxed),
                "labelled": c.analysis.labelled.load(Ordering::Relaxed),
                "refused_overlap": c.analysis.refused_overlap.load(Ordering::Relaxed),
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
        Ok(json!({
            "sources": rows
                .into_iter()
                .map(|r| {
                    json!({
                        "id": r.id,
                        "match_key": r.match_key,
                        // `binary` is the process this was keyed on; `display`
                        // is what the application calls itself. For a Wine
                        // program these differ, which is exactly why the key is
                        // the PE name and not the shared loader (DESIGN §3).
                        "binary": r.match_key,
                        "display": r.display_name,
                        "display_name": r.display_name,
                        // The live answer, which may be ahead of the database
                        // for the instant between a toggle and its mirror.
                        "allowed": live.decide(&r.match_key).captures(),
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

    // ---- speakers --------------------------------------------------------

    fn speakers_list(&self) -> Result<Value, Error> {
        let rows = self.store().list_speakers().map_err(Error::from)?;
        Ok(json!({
            "speakers": rows
                .into_iter()
                .map(|r| json!({
                    "id": r.id,
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

        let seq = self
            .bus
            .publish(Topic::Relabel, "relabel", json!({"speaker": id, "name": name}));
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
        let canonical_name = canonical.as_ref().and_then(|s| s.name().map(str::to_string));

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

    // ---- segments --------------------------------------------------------

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
        let rows = self.store().segment_rows(&filter, limit).map_err(Error::from)?;
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
        let rows = self.store().segments_matching(&filter).map_err(Error::from)?;
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
        let rows = self.store().segments_matching(&filter).map_err(Error::from)?;
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
        let dir = std::env::temp_dir().join(format!(
            "nx-recall-service-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        let control = Control::new(dir.clone(), None, &Allowlist::from_rules([("x", false)]));
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

    #[test]
    fn split_says_so_rather_than_pretending() {
        let r = rig("split");
        assert_eq!(
            call(&r, r#"{"id":1,"method":"speakers.split","params":{"id":1}}"#)
                .unwrap_err()
                .code,
            "unimplemented"
        );
    }

    #[test]
    fn pause_is_instant_reported_and_broadcast() {
        let r = rig("pause");
        let out = call(&r, r#"{"id":1,"method":"pause"}"#).unwrap();
        assert_eq!(out["paused"], true);
        assert_eq!(out["changed"], true);
        assert!(r.service.control.is_paused());
        assert_eq!(call(&r, r#"{"id":2,"method":"status"}"#).unwrap()["paused"], true);

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
            store.set_segment_speaker(seg, Some(spk), Some(0.7)).unwrap();
            spk
        };

        let out = call(
            &r,
            &format!(r#"{{"id":1,"method":"speakers.name","params":{{"id":{spk},"name":"Kira"}}}}"#),
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
        let hits = call(&r, r#"{"id":2,"method":"search","params":{"q":"fountain"}}"#).unwrap();
        assert_eq!(hits["hits"][0]["speaker"], spk);
        assert_eq!(hits["hits"][0]["speaker_name"], "Kira");
        assert!(hits["hits"][0]["snippet"].as_str().unwrap().contains("fountain"));

        // And the audit trail knows what it was called before.
        let ops = call(&r, r#"{"id":3,"method":"operations.list"}"#).unwrap();
        assert_eq!(ops["operations"][0]["op"], "speakers.name");
        assert_eq!(ops["operations"][0]["prior_state"]["display_name"], "Speaker_01");
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
                &format!(r#"{{"id":2,"method":"speakers.merge","params":{{"from":{a},"into":{b}}}}}"#)
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
        assert_eq!(ops["operations"][0]["prior_state"]["text"], "the bell tolls");
        assert_eq!(ops["operations"][1]["op"], "segments.reassign");
        assert_eq!(ops["operations"][1]["prior_state"]["speaker_id"], Value::Null);

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
        assert!(r.service.control.allowlist().decide("VRChat.exe").captures());
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
            call(&r, r#"{"id":1,"method":"sources.set","params":{"match_key":"x"}}"#)
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
        let purge = seen.iter().find(|e| e["ev"] == "purge").expect("a purge event");
        assert_eq!(purge["data"]["ids"], serde_json::json!([seg]));
        assert_eq!(r.service.op_state(&op), Some(OpState::Done));

        // Gone from every read path, still on disk for the undo window.
        assert!(
            call(&r, r#"{"id":3,"method":"transcript"}"#).unwrap()["segments"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(call(&r, r#"{"id":4,"method":"delete.preview"}"#).unwrap()["segments"], 0);
        assert!(path.exists(), "soft delete keeps the audio until the sweeper");
        assert_eq!(
            r.service.store().expired_soft_deletes(i64::MAX).unwrap().len(),
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
            call(&r, r#"{"id":2,"method":"events.since","params":{"seq":9999}}"#)
                .unwrap_err()
                .code,
            "resync"
        );
    }
}
