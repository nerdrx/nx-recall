//! PipeWire capture.
//!
//! We watch the registry for `Stream/Output/Audio` nodes — the playback streams
//! applications create — and, for allowlisted ones, open an *input* stream
//! aimed at that node with `PW_KEY_TARGET_OBJECT`. That is the same mechanism
//! as `pw-record --target <serial>`: PipeWire links our input ports directly to
//! the application's output ports, so we get that one program's audio before it
//! is mixed with anything else.
//!
//! Note for anyone coming from the Step 0 spike: there is no per-application
//! `.monitor` node to look for. Monitor ports belong to *sinks*, and capturing
//! one gives the whole mixed output of that sink. Targeting the app node is
//! what makes per-application tagging meaningful at all.
//!
//! ## The microphone
//!
//! One source here is not an application: the user's own default input device.
//! It is the same mechanism (an input-direction stream) pointed at an
//! `Audio/Source` node instead of a `Stream/Output/Audio` one, and it is
//! different from every app tap in three ways that are all deliberate:
//!
//! * It is **off by default and not an allowlist rule** — an app rule is
//!   consent about one program's output, and a microphone picks up the room.
//! * In the default `follow` mode it is only open while at least one allowed
//!   application is itself being captured, so the mic's lifetime is bounded by
//!   a conversation rather than by the daemon's.
//! * It **follows the default source** instead of pinning to one node. An app
//!   tap that reconnected would silently record a different program; a mic tap
//!   that reconnects records the same person on a different headset, which is
//!   the correct answer.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use pipewire as pw;
use pw::spa;
use pw::{properties::properties, types::ObjectType};
use spa::param::format::{MediaSubtype, MediaType};
use spa::param::format_utils;
use spa::pod::Pod;
use tracing::{debug, error, info, warn};

use crate::allowlist::{Allowlist, Decision, SourceIdent};
use crate::bus::{Bus, Topic};
use crate::clock::{monotonic_ns, utc_now_ns};
use crate::config::{Config, MicConfig, MicMode, RoomConfig, SAMPLE_RATE};
use crate::control::Control;
use crate::pipeline::Stats;
use crate::queue::{AudioChunk, CaptureEvent, EventQueue};
use crate::resample::{LinearResampler, downmix};
use crate::room::{ROOM_DISPLAY_NAME, ROOM_MATCH_KEY};
use crate::store::{KIND_MIC, KIND_ROOM, Store};

const PLAYBACK_STREAM_CLASS: &str = "Stream/Output/Audio";
/// A capture device — a microphone, a line in, a sink's monitor. The mic tap
/// aims at exactly one of these.
const AUDIO_SOURCE_CLASS: &str = "Audio/Source";
// (the mic state machine itself lives in `mic_plan`, below)
/// The session manager publishes the default devices on a metadata object under
/// this name; `default.audio.source` is the key that names the microphone.
const DEFAULT_METADATA: &str = "default";
const DEFAULT_SOURCE_KEY: &str = "default.audio.source";

/// The microphone's `sources.match_key`. It is a row in `sources` like any
/// application and is governed by `[mic]` rather than by `[rules]`, which is
/// why `sources.set` refuses it (see `service::sources_set`).
pub const MIC_MATCH_KEY: &str = "mic";
pub const MIC_DISPLAY_NAME: &str = "Microphone";

/// The `state` a `source` event carries: what this source is doing at the
/// instant the event was published (PROTOCOL, `source`).
///
/// It is the live answer and the row's `streams` count is the database's, which
/// can trail it by one queue hop: a capture that has just stopped still has an
/// open session row until the pipeline drains the end event and writes the last
/// segment. A client rendering a "capturing now" light should believe `state`.
/// On the graph, not being captured — either not allowed, or allowed and not
/// yet attached.
pub const SOURCE_SEEN: &str = "seen";
/// A capture stream is open on it right now.
pub const SOURCE_CAPTURING: &str = "capturing";
/// Was being captured; the capture was stopped while the application stayed.
pub const SOURCE_STOPPED: &str = "stopped";
/// The node left the graph — the application closed its stream or quit.
pub const SOURCE_GONE: &str = "gone";

/// How long to wait before trying the microphone again after a failed attach.
/// A device that is not there yet (or was just unplugged) must retry, not spin.
const MIC_RETRY: Duration = Duration::from_secs(5);

/// One application playback node as seen on the graph.
#[derive(Debug, Clone)]
pub struct NodeInfo {
    pub node_id: u32,
    /// `object.serial` — the value `PW_KEY_TARGET_OBJECT` matches against. The
    /// registry id is reused after a node dies; the serial never is.
    pub serial: Option<String>,
    pub ident: SourceIdent,
}

impl NodeInfo {
    fn target(&self) -> Option<String> {
        self.serial.clone().or_else(|| self.ident.node_name.clone())
    }

    /// Which *copy* of the application this node belongs to (0.11.9).
    ///
    /// `object.serial` first because PipeWire promises never to reuse it within
    /// a boot, where the registry id at `node_id` is reused as soon as a node
    /// dies; `application.process.id` second because a pid is what a human
    /// reading a log can actually match against `ps`. `None` when the node
    /// carried neither, which is a real case and must stay distinguishable from
    /// "one instance" — see `Store::begin_session_for`.
    fn instance_key(&self) -> Option<String> {
        self.serial
            .clone()
            .map(|s| format!("serial:{s}"))
            .or_else(|| self.ident.process_id.map(|p| format!("pid:{p}")))
    }
}

fn ident_from_props(props: &spa::utils::dict::DictRef) -> SourceIdent {
    SourceIdent {
        process_binary: props.get(&pw::keys::APP_PROCESS_BINARY).map(str::to_string),
        application_name: props.get(&pw::keys::APP_NAME).map(str::to_string),
        node_name: props.get(&pw::keys::NODE_NAME).map(str::to_string),
        process_id: props
            .get(&pw::keys::APP_PROCESS_ID)
            .and_then(|s| s.parse::<i64>().ok()),
    }
}

fn node_info_from_props(node_id: u32, props: &spa::utils::dict::DictRef) -> Option<NodeInfo> {
    let media_class = props.get(&pw::keys::MEDIA_CLASS)?;
    if media_class != PLAYBACK_STREAM_CLASS {
        return None;
    }
    Some(NodeInfo {
        node_id,
        serial: props.get(&pw::keys::OBJECT_SERIAL).map(str::to_string),
        ident: ident_from_props(props),
    })
}

/// Everything the microphone state machine depends on. Pulled out as data so
/// the machine itself is a pure function: live mic capture is not testable off
/// a real device, and this is the half that can be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MicSituation<'a> {
    pub enabled: bool,
    pub mode: MicMode,
    /// How many allowed applications are being captured right now. The only
    /// thing `follow` mode looks at.
    pub app_captures: usize,
    /// The session that is open, and the `node.name` it was opened for.
    /// `None` means nothing is open.
    pub open_on: Option<Option<&'a str>>,
    /// The device the tap should be on: an explicit `[mic].device`, else the
    /// default source, else `None` for "let the session manager route it".
    pub target: Option<&'a str>,
    /// Whether that target can be honoured right now. True when there is no
    /// explicit pin — the session manager routes a plain capture stream and a
    /// missing default is not an error — and, when there is one, whether that
    /// device is on the graph at all.
    ///
    /// This is the input the machine was missing (audit finding #7). Without
    /// it, a pinned microphone that is unplugged keeps `target == open_on`, so
    /// the plan is `Hold`: the session stays open forever over a dead stream,
    /// `status` reports the mic as active, and replugging does not recover
    /// because nothing ever re-resolves the name.
    pub target_on_graph: bool,
    /// Whether the open stream has reported `StreamState::Error`. A device that
    /// vanishes under a live tap lands here, and an errored stream is not a
    /// recording however open its session looks.
    pub open_failed: bool,
}

/// What to do about the microphone right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicPlan {
    /// Nothing to do — either it is already right, or it is off and closed.
    Hold,
    /// Close the open session and leave it closed.
    Close,
    /// Open a session on `target`.
    Open,
    /// Close and open again: the input device moved under us.
    Reopen,
}

/// The microphone state machine, whole.
///
/// The ordering here **is** the privacy model (DESIGN §5, and the reason the
/// mic is not an allowlist rule):
///
/// 1. Off beats everything. A disabled mic is closed, immediately.
/// 2. `follow` is open exactly while at least one allowed application is being
///    captured. VRChat opening is what opens it; VRChat closing is what closes
///    it, and neither is a decision the user has to remember to make.
/// 3. `always` ignores the app list, which is the whole difference and the
///    reason it is a deliberate second choice rather than the default.
/// 4. A device change reopens rather than silently continuing, so one session's
///    audio never spans two physical microphones.
/// 5. A session whose device is gone — an unplugged pin, or a stream the graph
///    put into `Error` — is **closed**, not held. The daemon does not get to
///    report a microphone as recording when there is no microphone; the state
///    goes honestly back to `following:idle` / `always:idle`, the session row
///    ends, and the ordinary retry re-resolves the pin by name when the device
///    comes back on a new serial.
///
/// Global pause is deliberately **not** an input: pausing must not tear down a
/// capture stream (that would cost a re-negotiation and lose the session), so
/// it is enforced where every other write is — in the pipeline, before a row or
/// a file exists. The mic is gated by the same check as everything else.
pub fn mic_plan(s: &MicSituation<'_>) -> MicPlan {
    let wanted = s.enabled
        && match s.mode {
            MicMode::Always => true,
            MicMode::Follow => s.app_captures > 0,
        };
    match (wanted, s.open_on) {
        (false, None) => MicPlan::Hold,
        (false, Some(_)) => MicPlan::Close,
        (true, None) => MicPlan::Open,
        // Rule 5 before rule 4: a tap whose device is gone is not a tap that
        // moved, and reopening it on the same missing name would only fail.
        (true, Some(_)) if s.open_failed || !s.target_on_graph => MicPlan::Close,
        (true, Some(on)) if on == s.target => MicPlan::Hold,
        (true, Some(_)) => MicPlan::Reopen,
    }
}

/// One `Audio/Source` device on the graph — a candidate for the mic tap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceNode {
    pub node_id: u32,
    pub serial: Option<String>,
    /// `node.name` — the stable id the default-source metadata refers to.
    pub name: Option<String>,
    /// `node.description`, the human label ("Yeti Stereo Microphone").
    pub description: Option<String>,
}

impl SourceNode {
    /// What `PW_KEY_TARGET_OBJECT` should be set to for this device. The serial
    /// is preferred for the same reason as an app node: registry ids get
    /// recycled, serials never do.
    fn target(&self) -> Option<String> {
        self.serial.clone().or_else(|| self.name.clone())
    }

    pub fn label(&self) -> String {
        self.description
            .clone()
            .or_else(|| self.name.clone())
            .unwrap_or_else(|| format!("node {}", self.node_id))
    }
}

fn source_node_from_props(node_id: u32, props: &spa::utils::dict::DictRef) -> Option<SourceNode> {
    if props.get(&pw::keys::MEDIA_CLASS)? != AUDIO_SOURCE_CLASS {
        return None;
    }
    Some(SourceNode {
        node_id,
        serial: props.get(&pw::keys::OBJECT_SERIAL).map(str::to_string),
        name: props.get(&pw::keys::NODE_NAME).map(str::to_string),
        description: props.get(&pw::keys::NODE_DESCRIPTION).map(str::to_string),
    })
}

/// What the graph told us about one node. The registry hands out both classes
/// on the same callback, so the split happens once, here.
enum GraphNode {
    App(NodeInfo),
    Source(SourceNode),
}

fn graph_node_from_props(node_id: u32, props: &spa::utils::dict::DictRef) -> Option<GraphNode> {
    match props.get(&pw::keys::MEDIA_CLASS)? {
        PLAYBACK_STREAM_CLASS => node_info_from_props(node_id, props).map(GraphNode::App),
        AUDIO_SOURCE_CLASS => source_node_from_props(node_id, props).map(GraphNode::Source),
        _ => None,
    }
}

/// Is this a node class we care about? Used to skip binding the rest of the
/// graph when the registry announcement already carries `media.class`.
fn interesting_class(class: &str) -> bool {
    class == PLAYBACK_STREAM_CLASS || class == AUDIO_SOURCE_CLASS
}

/// The session manager writes `default.audio.source` as `{"name": "<node.name>"}`.
/// Parsed by hand rather than with a JSON dependency in the audio path: the
/// shape is fixed, and a value we cannot read must degrade to "no default
/// known" rather than break capture.
fn default_source_name(value: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(value).ok()?;
    let name = parsed.get("name")?.as_str()?.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// The EnumFormat pod asking for 16 kHz mono f32.
///
/// Requesting the target format here is what makes PipeWire's adapter do the
/// conversion with its own (good) resampler. `resample.rs` only ever runs if
/// the graph declines and negotiates something else.
fn target_format_param() -> Result<Vec<u8>> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(SAMPLE_RATE);
    info.set_channels(1);

    let obj = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    let bytes = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .map_err(|e| anyhow!("serialising the audio format pod: {e:?}"))?
    .0
    .into_inner();
    Ok(bytes)
}

/// Per-stream state owned by the PipeWire callbacks.
struct StreamData {
    session_id: i64,
    label: String,
    queue: Arc<EventQueue>,
    /// Negotiated format; starts at the requested 16 kHz mono and is corrected
    /// in `param_changed` if the graph picked something else.
    rate: u32,
    channels: u32,
    resampler: LinearResampler,
    warned_about_format: bool,
    /// Set when the graph puts this stream into `StreamState::Error` — a device
    /// pulled out from under a live tap. The callback runs on the PipeWire
    /// loop and `Shared` is borrowed there, so the news comes back as a flag
    /// the next `sync_mic` reads rather than as a call into the state machine
    /// (audit finding #7).
    failed: Arc<std::sync::atomic::AtomicBool>,
}

/// A live capture: the stream, its listener, and the session row it feeds.
struct Capture {
    session_id: i64,
    match_key: String,
    _stream: pw::stream::StreamRc,
    _listener: pw::stream::StreamListener<StreamData>,
}

/// The live microphone tap and everything needed to decide whether it should
/// exist right now.
struct Mic {
    cfg: MicConfig,
    /// The `sources` row. Created at start-up whether or not the mic is on, so
    /// the GUI has something to render a switch against.
    source_id: i64,
    capture: Option<MicCapture>,
    /// `default.audio.source`, as the session manager last published it.
    default_source: Option<String>,
    /// Every `Audio/Source` on the graph, so a name can be resolved to a serial
    /// (and to a label worth logging) rather than guessed at.
    nodes: HashMap<u32, SourceNode>,
    /// Set after a failed attach; nothing is retried before it passes.
    retry_after: Option<Instant>,
    /// The last state string published, so an unchanged state costs no event.
    announced: Option<&'static str>,
}

/// A live microphone capture.
struct MicCapture {
    session_id: i64,
    /// The `node.name` this session was opened for — an explicit `[mic].device`
    /// or the default source at the time. `None` means "whatever the session
    /// manager routes a plain capture stream to". Compared on every tick: when
    /// it changes, the user swapped devices and the session is reopened, so
    /// provenance never spans two physical microphones.
    target: Option<String>,
    /// Shared with the stream's callbacks: set when the graph errors it out.
    failed: Arc<std::sync::atomic::AtomicBool>,
    _stream: pw::stream::StreamRc,
    _listener: pw::stream::StreamListener<StreamData>,
}

/// The live room tap (0.10.0). Deliberately thinner than [`Mic`]: it has no
/// default to follow and no device inventory of its own — the graph's capture
/// devices are enumerated once, in `mic.nodes`, and both taps read them.
struct Room {
    cfg: RoomConfig,
    /// The `sources` row, created at start-up whether or not the room mic is
    /// on, so a client has something to render its switch against.
    source_id: i64,
    capture: Option<MicCapture>,
    retry_after: Option<Instant>,
    announced: Option<&'static str>,
}

/// Everything the registry callbacks need to mutate.
struct Shared {
    allowlist: Allowlist,
    store: Arc<std::sync::Mutex<Store>>,
    queue: Arc<EventQueue>,
    stats: Arc<Stats>,
    control: Arc<Control>,
    bus: Arc<Bus>,
    captures: HashMap<u32, Capture>,
    /// Every playback node currently on the graph, captured or not. A live
    /// `sources.set` has to be able to attach to a node we already decided to
    /// skip, which means remembering the ones we skipped.
    nodes: HashMap<u32, NodeInfo>,
    mic: Mic,
    /// The second, physical microphone (0.10.0).
    room: Room,
    format_param: Vec<u8>,
    quantum: u32,
}

impl Shared {
    /// Tell every client what a `sources` row says now.
    ///
    /// Every change this thread makes to the table gets one of these (audit
    /// finding #2). Before it, the only `source` event in the daemon came from
    /// `sources.set` — so an application that started *after* the GUI opened
    /// never appeared in the Sources list, which meant it could never be
    /// allowed without restarting the GUI. First sighting, capture start,
    /// capture stop and the node going away are all things a client cannot
    /// infer and now all publish.
    ///
    /// The payload is the `sources.list` row shape, read back from the database
    /// rather than assembled from what this thread believes, plus `state`: what
    /// this row is doing right now, in one word a view can render without
    /// cross-referencing anything.
    fn publish_source(&self, match_key: &str, state: &'static str) {
        let row = {
            let store = match self.store.lock() {
                Ok(s) => s,
                Err(p) => p.into_inner(),
            };
            match store.source_row(match_key) {
                Ok(Some(row)) => row,
                Ok(None) => {
                    warn!(key = %match_key, "no source row to announce");
                    return;
                }
                Err(e) => {
                    warn!(key = %match_key, "could not read a source row to announce: {e:#}");
                    return;
                }
            }
        };
        let allowed = if row.kind == KIND_MIC {
            self.control.mic().enabled
        } else if row.kind == KIND_ROOM {
            // 0.10.0: the room mic's switch is `[room].enabled`, never a rule,
            // for the same reason the headset's is not one.
            self.control.room().enabled
        } else {
            self.allowlist.decide(&row.match_key).captures()
        };
        let mut data = crate::service::source_json(&row, allowed);
        data["state"] = serde_json::Value::from(state);
        self.bus.publish(Topic::Sources, "source", data);
    }

    /// Record a node in the sources table and announce it, whatever the
    /// allowlist says about it. Returns the row's id, and `None` if the write
    /// failed (there is nothing to attach to a source that is not on record).
    ///
    /// Split out of `on_node` because this half needs no PipeWire core and is
    /// where the whole of finding #2 lives: a source appearing has to reach the
    /// clients that are already connected, or it can never be allowed.
    fn note_source(&mut self, node: &NodeInfo) -> Option<i64> {
        let match_key = node.ident.match_key();
        let display_name = node.ident.display_name();
        self.nodes.insert(node.node_id, node.clone());

        // Record every source we see, allowed or not. Default-deny is only
        // usable if the user can find out what was refused.
        let source_id = {
            let store = match self.store.lock() {
                Ok(s) => s,
                Err(p) => p.into_inner(),
            };
            match store.upsert_source(&match_key, &display_name, utc_now_ns()) {
                Ok(id) => id,
                Err(e) => {
                    error!("recording source {match_key}: {e:#}");
                    return None;
                }
            }
        };

        // Announced before the decision is acted on: a source the daemon
        // refused is exactly the one the user needs to be able to see in order
        // to allow it. Default-deny is only usable if what was denied shows up.
        self.publish_source(&match_key, SOURCE_SEEN);
        Some(source_id)
    }

    /// Note a node in the sources table and, if allowed, start capturing it.
    fn on_node(&mut self, core: &pw::core::CoreRc, node: NodeInfo) {
        let match_key = node.ident.match_key();
        let display_name = node.ident.display_name();
        let decision = self.allowlist.decide(&match_key);
        let Some(source_id) = self.note_source(&node) else {
            return;
        };

        if !decision.captures() {
            info!(
                node = node.node_id,
                key = %match_key,
                pid = ?node.ident.process_id,
                "not capturing ({})",
                decision.as_str()
            );
            return;
        }

        if let Err(e) = self.attach(core, &node, source_id, &match_key, &display_name) {
            error!("attaching to {match_key} (node {}): {e:#}", node.node_id);
        }
        // An allowed application starting is what opens a `follow`-mode mic.
        self.sync_taps(core);
    }

    /// Open the session row, then wire up the stream. If the stream fails to
    /// come up the row is closed immediately rather than left dangling until
    /// the next restart.
    fn attach(
        &mut self,
        core: &pw::core::CoreRc,
        node: &NodeInfo,
        source_id: i64,
        match_key: &str,
        display_name: &str,
    ) -> Result<()> {
        let session_id = {
            let store = self
                .store
                .lock()
                .map_err(|_| anyhow!("store mutex poisoned"))?;
            // 0.11.9: which copy of the app this is. Already in hand here and
            // thrown away until now; see `Store::begin_session_for` for what
            // that cost.
            store.begin_session_for(source_id, utc_now_ns(), node.instance_key().as_deref())?
        };
        let result = self.attach_stream(core, node, session_id, match_key, display_name);
        if result.is_err()
            && let Ok(store) = self.store.lock()
            && let Err(e) = store.end_session(session_id, utc_now_ns())
        {
            error!("could not close the failed session {session_id}: {e:#}");
        }
        result
    }

    fn attach_stream(
        &mut self,
        core: &pw::core::CoreRc,
        node: &NodeInfo,
        session_id: i64,
        match_key: &str,
        display_name: &str,
    ) -> Result<()> {
        let target = node
            .target()
            .ok_or_else(|| anyhow!("node has neither object.serial nor node.name"))?;

        let mut props = properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Communication",
            *pw::keys::APP_NAME => "nx-recall",
            *pw::keys::NODE_NAME => "nx-recall-capture",
        };
        props.insert(*pw::keys::TARGET_OBJECT, target.clone());
        props.insert(
            *pw::keys::NODE_LATENCY,
            format!("{}/{}", self.quantum, SAMPLE_RATE),
        );
        // If the target disappears we want an error, not a silent reconnect to
        // whatever the session manager considers the default — that would
        // attribute another program's audio to this source.
        props.insert("stream.dont-reconnect", "true");

        let stream = pw::stream::StreamRc::new(core.clone(), "nx-recall-capture", props)
            .context("creating the capture stream")?;

        let data = StreamData {
            session_id,
            label: match_key.to_string(),
            queue: Arc::clone(&self.queue),
            rate: SAMPLE_RATE,
            channels: 1,
            resampler: LinearResampler::new(),
            warned_about_format: false,
            // An application tap sets `stream.dont-reconnect`, so a stream
            // error here is followed by the node-removed path closing the
            // session. Nothing reads the flag; it exists for the mic.
            failed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };

        let listener = register_stream(&stream, data)?;

        let mut params = [Pod::from_bytes(&self.format_param)
            .ok_or_else(|| anyhow!("malformed audio format pod"))?];

        // No RT_PROCESS: the callback pushes into a mutex-guarded queue, which
        // has no place on a realtime thread. Running it on the main loop costs
        // a little latency we do not care about and removes a priority-inversion
        // hazard from the audio path.
        stream
            .connect(
                spa::utils::Direction::Input,
                None,
                pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
                &mut params,
            )
            .context("connecting the capture stream")?;

        info!(
            node = node.node_id,
            session = session_id,
            key = %match_key,
            app = %display_name,
            pw_target = %target,
            pid = ?node.ident.process_id,
            "capturing"
        );
        self.stats.sessions_opened.fetch_add(1, Ordering::Relaxed);
        self.captures.insert(
            node.node_id,
            Capture {
                session_id,
                match_key: match_key.to_string(),
                _stream: stream,
                _listener: listener,
            },
        );
        // The session row exists now, so `streams` has moved: the "capturing
        // now" light in every client turns on from this event.
        self.publish_source(match_key, SOURCE_CAPTURING);
        Ok(())
    }

    /// Apply a rule change from the socket: attach to anything newly allowed,
    /// detach from anything newly denied. Runs on the PipeWire thread, from a
    /// timer, because that loop cannot be called into from outside.
    fn apply_rules(&mut self, core: &pw::core::CoreRc, allowlist: Allowlist) {
        self.allowlist = allowlist;
        let nodes: Vec<NodeInfo> = self.nodes.values().cloned().collect();

        // Detach first, so a source that was re-keyed cannot briefly hold two
        // sessions open.
        for node in &nodes {
            let key = node.ident.match_key();
            if !self.allowlist.decide(&key).captures() && self.captures.contains_key(&node.node_id)
            {
                info!(node = node.node_id, key = %key, "no longer allowed; stopping capture");
                self.stop_capture(node.node_id);
            }
        }
        for node in &nodes {
            let key = node.ident.match_key();
            if !self.allowlist.decide(&key).captures() || self.captures.contains_key(&node.node_id)
            {
                continue;
            }
            let display_name = node.ident.display_name();
            let source_id = {
                let store = match self.store.lock() {
                    Ok(s) => s,
                    Err(p) => p.into_inner(),
                };
                match store.upsert_source(&key, &display_name, utc_now_ns()) {
                    Ok(id) => id,
                    Err(e) => {
                        error!("recording source {key}: {e:#}");
                        continue;
                    }
                }
            };
            info!(node = node.node_id, key = %key, "newly allowed; starting capture");
            if let Err(e) = self.attach(core, node, source_id, &key, &display_name) {
                error!("attaching to {key} (node {}): {e:#}", node.node_id);
            }
        }
        // Allowing or denying the last app is a `follow`-mode transition too.
        self.sync_taps(core);
    }

    /// Drop a live capture and close its session. Dropping the `Capture`
    /// disconnects the stream; the pipeline writes the final segment when it
    /// sees the queued end event.
    fn stop_capture(&mut self, node_id: u32) {
        let Some(capture) = self.captures.remove(&node_id) else {
            return;
        };
        self.queue.push(CaptureEvent::SessionEnd {
            session_id: capture.session_id,
            mono_ns: monotonic_ns(),
        });
        self.publish_source(&capture.match_key, SOURCE_STOPPED);
    }

    fn on_node_removed(&mut self, core: &pw::core::CoreRc, node_id: u32) {
        let gone = self.nodes.remove(&node_id);
        // A capture device leaving is normal (headset unplugged, dock removed);
        // the mic tap re-resolves and either reopens on the new default or
        // waits, but never brings the daemon down with it.
        if self.mic.nodes.remove(&node_id).is_some() {
            self.sync_taps(core);
        }
        let Some(capture) = self.captures.remove(&node_id) else {
            // Not captured, but still worth announcing: the application quit,
            // and a list that goes on showing it as present is wrong.
            if let Some(node) = gone {
                self.publish_source(&node.ident.match_key(), SOURCE_GONE);
            }
            // An app session closing is what ends a `follow`-mode mic session.
            self.sync_taps(core);
            return;
        };
        info!(
            node = node_id,
            session = capture.session_id,
            key = %capture.match_key,
            "source went away; closing session"
        );
        // Dropping the Capture disconnects the stream. The pipeline writes the
        // final segment and stamps `ended_at_utc_ns` when it sees this event.
        self.queue.push(CaptureEvent::SessionEnd {
            session_id: capture.session_id,
            mono_ns: monotonic_ns(),
        });
        self.publish_source(&capture.match_key, SOURCE_GONE);
        self.sync_taps(core);
    }

    // ---- microphone ------------------------------------------------------

    fn on_source_node(&mut self, core: &pw::core::CoreRc, node: SourceNode) {
        let known = self.mic.nodes.get(&node.node_id);
        if known == Some(&node) {
            return;
        }
        debug!(node = node.node_id, name = ?node.name, "capture device on the graph");
        self.mic.nodes.insert(node.node_id, node);
        self.sync_taps(core);
    }

    fn on_default_source(&mut self, core: &pw::core::CoreRc, name: Option<String>) {
        if self.mic.default_source == name {
            return;
        }
        info!(default_source = ?name, "the default audio source changed");
        self.mic.default_source = name;
        self.sync_taps(core);
    }

    /// The `node.name` the tap should be on: an explicit pin, else the default
    /// source, else nothing (and the session manager routes it).
    fn mic_target(&self) -> Option<String> {
        self.mic
            .cfg
            .device_override()
            .map(str::to_string)
            .or_else(|| self.mic.default_source.clone())
    }

    /// Is the explicitly pinned `[mic].device` on the graph right now?
    ///
    /// True when there is no pin at all: a plain capture stream with no target
    /// is routed by the session manager, which is what every other client does
    /// and is not a failure. The lookup is by `node.name` and never by serial —
    /// a device that is unplugged and plugged back in keeps its name and gets a
    /// **new** `object.serial`, so a pin that resolved through the old serial
    /// could never recover (audit finding #7).
    fn pin_on_graph(&self) -> bool {
        let Some(pin) = self.mic.cfg.device_override() else {
            return true;
        };
        self.mic
            .nodes
            .values()
            .any(|n| n.name.as_deref() == Some(pin))
    }

    /// Resolve the target name to a `PW_KEY_TARGET_OBJECT` value.
    ///
    /// An explicit `[mic].device` that is not on the graph is an error, because
    /// silently falling back to the default would record a device the user
    /// specifically did not ask for. A *default* we cannot resolve to a node is
    /// not an error: connecting with no target lets the session manager route
    /// the stream, which is the same thing every other capture client does.
    fn resolve_mic_target(&self, target: Option<&str>) -> Result<Option<String>> {
        let Some(name) = target else {
            return Ok(None);
        };
        let node = self
            .mic
            .nodes
            .values()
            .find(|n| n.name.as_deref() == Some(name));
        match node {
            Some(n) => Ok(n.target()),
            None if self.mic.cfg.device_override() == Some(name) => Err(anyhow!(
                "[mic].device = {name:?} is not an Audio/Source on the graph"
            )),
            None => Ok(None),
        }
    }

    /// Bring the mic tap in line with what the switch, the mode and the graph
    /// currently say. Idempotent and cheap: called from the registry callbacks
    /// and from the 250 ms timer, so a follow-mode transition lands well inside
    /// the second the design promises.
    fn sync_mic(&mut self, core: &pw::core::CoreRc) {
        let target = self.mic_target();
        let target_on_graph = self.pin_on_graph();
        let open_failed = self
            .mic
            .capture
            .as_ref()
            .is_some_and(|c| c.failed.load(Ordering::SeqCst));
        let plan = mic_plan(&MicSituation {
            enabled: self.mic.cfg.enabled,
            mode: self.mic.cfg.mode,
            app_captures: self.captures.len(),
            open_on: self.mic.capture.as_ref().map(|c| c.target.as_deref()),
            target: target.as_deref(),
            target_on_graph,
            open_failed,
        });

        match plan {
            MicPlan::Hold => return,
            MicPlan::Close => {
                let why = if !self.mic.cfg.enabled {
                    "the microphone was switched off"
                } else if open_failed {
                    "the capture stream failed"
                } else if !target_on_graph {
                    "the pinned device left the graph"
                } else {
                    "no allowed application is capturing"
                };
                info!("microphone: {why}; closing the mic session");
                self.stop_mic();
                self.mic.retry_after = None;
                self.announce_mic();
                return;
            }
            MicPlan::Reopen => {
                info!(to = ?target, "microphone: the input device changed; reopening the session");
                self.stop_mic();
            }
            MicPlan::Open => {}
        }

        if let Some(at) = self.mic.retry_after
            && Instant::now() < at
        {
            // Still backing off, but the session is closed and the state
            // string has to say so.
            self.announce_mic();
            return;
        }
        if let Err(e) = self.attach_mic(core, target) {
            warn!("microphone: not capturing — {e:#}");
            self.mic.retry_after = Some(Instant::now() + MIC_RETRY);
        } else {
            self.mic.retry_after = None;
        }
        self.announce_mic();
    }

    fn attach_mic(&mut self, core: &pw::core::CoreRc, target: Option<String>) -> Result<()> {
        let resolved = self.resolve_mic_target(target.as_deref())?;
        let session_id = {
            let store = self
                .store
                .lock()
                .map_err(|_| anyhow!("store mutex poisoned"))?;
            store.begin_session(self.mic.source_id, utc_now_ns())?
        };
        let result = self.attach_mic_stream(core, session_id, target, resolved);
        if result.is_err()
            && let Ok(store) = self.store.lock()
            && let Err(e) = store.end_session(session_id, utc_now_ns())
        {
            error!("could not close the failed mic session {session_id}: {e:#}");
        }
        result
    }

    fn attach_mic_stream(
        &mut self,
        core: &pw::core::CoreRc,
        session_id: i64,
        target: Option<String>,
        resolved: Option<String>,
    ) -> Result<()> {
        let mut props = properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Communication",
            *pw::keys::APP_NAME => "nx-recall",
            *pw::keys::NODE_NAME => "nx-recall-mic",
        };
        props.insert(
            *pw::keys::NODE_LATENCY,
            format!("{}/{}", self.quantum, SAMPLE_RATE),
        );
        if let Some(t) = &resolved {
            props.insert(*pw::keys::TARGET_OBJECT, t.clone());
        }
        // Note the absence of `stream.dont-reconnect`, which every app tap
        // sets. For an application, reconnecting would attribute a different
        // program's audio to this source — a correctness bug. For the
        // microphone it is the opposite: the user's voice is the user's voice
        // whichever device it arrives on, so letting the graph move the tap is
        // how a headset swap degrades instead of dying.

        let stream = pw::stream::StreamRc::new(core.clone(), "nx-recall-mic", props)
            .context("creating the microphone stream")?;

        let failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let data = StreamData {
            session_id,
            label: MIC_MATCH_KEY.to_string(),
            queue: Arc::clone(&self.queue),
            rate: SAMPLE_RATE,
            channels: 1,
            resampler: LinearResampler::new(),
            warned_about_format: false,
            failed: Arc::clone(&failed),
        };
        // Resampled and stamped through exactly the same callbacks as an app
        // stream: a mic turn and a VRChat turn must be comparable on one clock.
        let listener = register_stream(&stream, data)?;

        let mut params = [Pod::from_bytes(&self.format_param)
            .ok_or_else(|| anyhow!("malformed audio format pod"))?];
        stream
            .connect(
                spa::utils::Direction::Input,
                None,
                pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
                &mut params,
            )
            .context("connecting the microphone stream")?;

        let label = resolved
            .as_deref()
            .and_then(|t| {
                self.mic
                    .nodes
                    .values()
                    .find(|n| n.target().as_deref() == Some(t))
                    .map(SourceNode::label)
            })
            .unwrap_or_else(|| "the default source".to_string());
        info!(
            session = session_id,
            mode = self.mic.cfg.mode.as_str(),
            device = %label,
            pw_target = ?resolved,
            "capturing the microphone"
        );
        self.stats.sessions_opened.fetch_add(1, Ordering::Relaxed);
        self.mic.capture = Some(MicCapture {
            session_id,
            target,
            failed,
            _stream: stream,
            _listener: listener,
        });
        Ok(())
    }

    fn stop_mic(&mut self) {
        let Some(capture) = self.mic.capture.take() else {
            return;
        };
        // Dropping the MicCapture disconnects the stream; the pipeline writes
        // the final segment and closes the row when it sees this event.
        self.queue.push(CaptureEvent::SessionEnd {
            session_id: capture.session_id,
            mono_ns: monotonic_ns(),
        });
    }

    /// Publish the mic state, but only when it actually moved. PROTOCOL: this
    /// is a `mic` event on the existing `status` topic, so no client has to
    /// subscribe to anything new and an older one ignores it (versioning rule:
    /// clients MUST ignore unknown event types).
    fn announce_mic(&mut self) {
        self.control.set_mic_active(self.mic.capture.is_some());
        let state = self.control.mic_state();
        if self.mic.announced == Some(state) {
            return;
        }
        self.mic.announced = Some(state);
        self.bus
            .publish(Topic::Status, "mic", self.control.mic_json());
    }

    // ---- the room microphone (0.10.0) ------------------------------------
    //
    // A second tap on the same graph, with the same state machine (`mic_plan`)
    // and the same stream callbacks (`register_stream`), and exactly two
    // differences: it will not open without an explicit device, and its turns
    // are ordinary turns — no pin, no `label_via: "mic"`, no You. See room.rs
    // for why those two differences are the whole feature.

    /// Bring both microphone taps in line with the graph. Every call site that
    /// used to sync the headset syncs the room as well: `follow` means the same
    /// thing for both, so an application starting or stopping moves both.
    fn sync_taps(&mut self, core: &pw::core::CoreRc) {
        self.sync_mic(core);
        self.sync_room(core);
    }

    /// The room tap's device, which is required. `None` means the switch is on
    /// and there is nothing to open — reported as `needs-device`, never as
    /// "waiting for a device that might turn up".
    fn room_target(&self) -> Option<String> {
        self.room.cfg.device_override().map(str::to_string)
    }

    fn room_pin_on_graph(&self) -> bool {
        let Some(pin) = self.room.cfg.device_override() else {
            return false;
        };
        self.mic
            .nodes
            .values()
            .any(|n| n.name.as_deref() == Some(pin))
    }

    fn sync_room(&mut self, core: &pw::core::CoreRc) {
        let target = self.room_target();
        // No device pinned: close anything open and say so. This is the branch
        // `mic_plan` cannot express, because for the headset "no target" is a
        // legitimate state (the session manager routes it) and here it is not.
        if self.room.cfg.enabled && target.is_none() {
            if self.room.capture.is_some() {
                info!("room microphone: no device is pinned; closing the session");
                self.stop_room();
            }
            self.room.retry_after = None;
            self.announce_room();
            return;
        }

        let target_on_graph = self.room_pin_on_graph();
        let open_failed = self
            .room
            .capture
            .as_ref()
            .is_some_and(|c| c.failed.load(Ordering::SeqCst));
        let plan = mic_plan(&MicSituation {
            enabled: self.room.cfg.enabled,
            mode: self.room.cfg.mode,
            app_captures: self.captures.len(),
            open_on: self.room.capture.as_ref().map(|c| c.target.as_deref()),
            target: target.as_deref(),
            target_on_graph,
            open_failed,
        });

        match plan {
            MicPlan::Hold => return,
            MicPlan::Close => {
                let why = if !self.room.cfg.enabled {
                    "the room microphone was switched off"
                } else if open_failed {
                    "the capture stream failed"
                } else if !target_on_graph {
                    "the pinned device left the graph"
                } else {
                    "no allowed application is capturing"
                };
                info!("room microphone: {why}; closing the session");
                self.stop_room();
                self.room.retry_after = None;
                self.announce_room();
                return;
            }
            MicPlan::Reopen => {
                info!(to = ?target, "room microphone: the input device changed; reopening");
                self.stop_room();
            }
            MicPlan::Open => {}
        }

        if let Some(at) = self.room.retry_after
            && Instant::now() < at
        {
            self.announce_room();
            return;
        }
        if let Err(e) = self.attach_room(core, target) {
            warn!("room microphone: not capturing — {e:#}");
            self.room.retry_after = Some(Instant::now() + MIC_RETRY);
        } else {
            self.room.retry_after = None;
        }
        self.announce_room();
    }

    fn attach_room(&mut self, core: &pw::core::CoreRc, target: Option<String>) -> Result<()> {
        let name = target
            .as_deref()
            .ok_or_else(|| anyhow!("[room].device names no input device"))?;
        // Unlike the headset, an unresolvable name is fatal to the attempt:
        // there is no "let the session manager decide" for a second mic, and
        // deciding for it would open the headset and record the user twice.
        let resolved = self
            .mic
            .nodes
            .values()
            .find(|n| n.name.as_deref() == Some(name))
            .and_then(SourceNode::target)
            .ok_or_else(|| {
                anyhow!("[room].device = {name:?} is not an Audio/Source on the graph")
            })?;

        let session_id = {
            let store = self
                .store
                .lock()
                .map_err(|_| anyhow!("store mutex poisoned"))?;
            store.begin_session(self.room.source_id, utc_now_ns())?
        };
        let result = self.attach_room_stream(core, session_id, target, resolved);
        if result.is_err()
            && let Ok(store) = self.store.lock()
            && let Err(e) = store.end_session(session_id, utc_now_ns())
        {
            error!("could not close the failed room session {session_id}: {e:#}");
        }
        result
    }

    fn attach_room_stream(
        &mut self,
        core: &pw::core::CoreRc,
        session_id: i64,
        target: Option<String>,
        resolved: String,
    ) -> Result<()> {
        let mut props = properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Communication",
            *pw::keys::APP_NAME => "nx-recall",
            *pw::keys::NODE_NAME => "nx-recall-room",
        };
        props.insert(
            *pw::keys::NODE_LATENCY,
            format!("{}/{}", self.quantum, SAMPLE_RATE),
        );
        props.insert(*pw::keys::TARGET_OBJECT, resolved.clone());
        // `stream.dont-reconnect`, unlike the headset tap. The headset may
        // follow the graph because the user's voice is the user's voice on any
        // device; a room mic that silently reconnected somewhere else would
        // record a different room, and the provenance on those turns would be
        // a lie.
        props.insert("stream.dont-reconnect", "true");

        let stream = pw::stream::StreamRc::new(core.clone(), "nx-recall-room", props)
            .context("creating the room microphone stream")?;

        let failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let data = StreamData {
            session_id,
            label: ROOM_MATCH_KEY.to_string(),
            queue: Arc::clone(&self.queue),
            rate: SAMPLE_RATE,
            channels: 1,
            resampler: LinearResampler::new(),
            warned_about_format: false,
            failed: Arc::clone(&failed),
        };
        let listener = register_stream(&stream, data)?;

        let mut params = [Pod::from_bytes(&self.format_param)
            .ok_or_else(|| anyhow!("malformed audio format pod"))?];
        stream
            .connect(
                spa::utils::Direction::Input,
                None,
                pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
                &mut params,
            )
            .context("connecting the room microphone stream")?;

        info!(
            session = session_id,
            mode = self.room.cfg.mode.as_str(),
            device = ?target,
            pw_target = %resolved,
            "capturing the room microphone — every voice in the room, matched like any other"
        );
        self.stats.sessions_opened.fetch_add(1, Ordering::Relaxed);
        self.room.capture = Some(MicCapture {
            session_id,
            target,
            failed,
            _stream: stream,
            _listener: listener,
        });
        Ok(())
    }

    fn stop_room(&mut self) {
        let Some(capture) = self.room.capture.take() else {
            return;
        };
        self.queue.push(CaptureEvent::SessionEnd {
            session_id: capture.session_id,
            mono_ns: monotonic_ns(),
        });
    }

    /// A `room` event on the existing `status` topic, published only when the
    /// state actually moved — the same discipline, and the same topic, as the
    /// headset's `mic` event, so no client changes its subscription.
    fn announce_room(&mut self) {
        self.control.set_room_active(self.room.capture.is_some());
        let state = self.control.room_state();
        if self.room.announced == Some(state) {
            return;
        }
        self.room.announced = Some(state);
        self.bus
            .publish(Topic::Status, "room", self.control.room_json());
        self.publish_source(
            ROOM_MATCH_KEY,
            if self.room.capture.is_some() {
                SOURCE_CAPTURING
            } else {
                SOURCE_SEEN
            },
        );
    }
}

/// The stream callbacks every capture shares: format negotiation, and the
/// process handler that stamps, downmixes, resamples and enqueues.
///
/// Shared between the app tap and the mic tap deliberately — "resample and
/// stamp identically" is a requirement, not a coincidence, and the way to make
/// it true is to have one implementation.
fn register_stream(
    stream: &pw::stream::StreamRc,
    data: StreamData,
) -> Result<pw::stream::StreamListener<StreamData>> {
    stream
        .add_local_listener_with_user_data(data)
        .state_changed(|_, data, old, new| {
            debug!(session = data.session_id, label = %data.label, "stream {old:?} -> {new:?}");
            if let pw::stream::StreamState::Error(msg) = &new {
                // A device vanishing (headset unplugged) lands here. Log, flag
                // it so the mic state machine can close the session instead of
                // reporting a dead stream as a recording; do not panic.
                data.failed.store(true, Ordering::SeqCst);
                warn!(session = data.session_id, label = %data.label, "capture stream error: {msg}");
            }
        })
        .param_changed(|_, data, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((media_type, media_subtype)) = format_utils::parse_format(param) else {
                return;
            };
            if media_type != MediaType::Audio || media_subtype != MediaSubtype::Raw {
                return;
            }
            let mut info = spa::param::audio::AudioInfoRaw::new();
            if info.parse(param).is_err() {
                warn!(session = data.session_id, "could not parse the negotiated audio format");
                return;
            }
            data.rate = info.rate();
            data.channels = info.channels().max(1);
            data.resampler.reset();
            if (data.rate != SAMPLE_RATE || data.channels != 1) && !data.warned_about_format {
                data.warned_about_format = true;
                warn!(
                    session = data.session_id,
                    label = %data.label,
                    "graph negotiated {} Hz / {} ch instead of 16000/1; falling back to local conversion",
                    data.rate,
                    data.channels
                );
            }
            info!(
                session = data.session_id,
                label = %data.label,
                "capturing at {} Hz, {} ch",
                data.rate,
                data.channels
            );
        })
        .process(|stream, data| {
            // Stamp before any work so the timestamp names the audio's
            // arrival, not the end of our processing.
            let capture_mono_ns = monotonic_ns();
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            let Some(d) = datas.first_mut() else { return };
            let offset = d.chunk().offset() as usize;
            let size = d.chunk().size() as usize;
            let Some(bytes) = d.data() else { return };
            let end = (offset + size).min(bytes.len());
            if end <= offset {
                return;
            }
            let raw = &bytes[offset..end];

            let mut interleaved = Vec::with_capacity(raw.len() / 4);
            for c in raw.as_chunks::<4>().0 {
                interleaved.push(f32::from_le_bytes([c[0], c[1], c[2], c[3]]));
            }

            let mono = downmix(&interleaved, data.channels as usize);
            let samples = data.resampler.process(&mono, data.rate, SAMPLE_RATE);
            if samples.is_empty() {
                return;
            }

            let evicted = data.queue.push(CaptureEvent::Audio(AudioChunk {
                session_id: data.session_id,
                capture_mono_ns,
                samples,
            }));
            if evicted > 0 {
                warn!(
                    session = data.session_id,
                    label = %data.label,
                    "VAD queue overflow: dropped {evicted} oldest buffer(s), {} total",
                    data.queue.dropped_chunks()
                );
            }
        })
        .register()
        .context("registering stream callbacks")
}

/// What `probe` found. Nothing here opens a capture; it is the "what would you
/// record, and from where" answer.
#[derive(Debug, Default)]
pub struct Probe {
    pub apps: Vec<(NodeInfo, Decision)>,
    /// Every `Audio/Source` on the graph, node id order.
    pub sources: Vec<SourceNode>,
    /// `default.audio.source` as the session manager publishes it, and the node
    /// it resolves to. `None` for the node means the name is set but no device
    /// on the graph answers to it — which is exactly what a stale default looks
    /// like, and worth seeing.
    pub default_source: Option<String>,
}

impl Probe {
    /// The device the mic tap would open right now. `device` is `[mic].device`.
    pub fn mic_target(&self, device: Option<&str>) -> Option<&SourceNode> {
        let name = device.or(self.default_source.as_deref())?;
        self.sources
            .iter()
            .find(|n| n.name.as_deref() == Some(name))
    }
}

// ---- device enumeration (0.10.0) ------------------------------------------
//
// The room microphone needs a device *name*, and a person cannot be asked to
// type a PipeWire `node.name` from memory. `devices.list` is the answer, and it
// is a read: nothing here opens a stream, and neither path can record anything.

/// One capture device, as a client picks it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRow {
    /// `node.name` — the stable id `[room].device` is written with.
    pub node_name: String,
    /// `node.description`, the human label. `None` on a node that publishes
    /// none, in which case a client shows the name.
    pub description: Option<String>,
    /// This is `default.audio.source` — almost always the headset the `[mic]`
    /// tap is on, and therefore the one device a room mic should NOT be.
    pub is_default: bool,
}

impl DeviceRow {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "node_name": self.node_name,
            "description": self.description,
            "is_default": self.is_default,
        })
    }
}

fn devices_from(sources: &[SourceNode], default_source: Option<&str>) -> Vec<DeviceRow> {
    let mut rows: Vec<DeviceRow> = sources
        .iter()
        .filter_map(|n| {
            // A node with no `node.name` cannot be pinned, so it cannot be
            // offered: listing it would produce a device nobody can select.
            let node_name = n.name.clone()?;
            Some(DeviceRow {
                is_default: default_source == Some(node_name.as_str()),
                node_name,
                description: n.description.clone(),
            })
        })
        .collect();
    rows.sort_by(|a, b| {
        b.is_default
            .cmp(&a.is_default)
            .then_with(|| a.node_name.cmp(&b.node_name))
    });
    rows.dedup_by(|a, b| a.node_name == b.node_name);
    rows
}

/// Every `Audio/Source` on the graph right now.
///
/// The live registry first, because that is the daemon's own view and needs no
/// external program. `pw-dump` is the fallback for the case where a second
/// connection cannot be made from this process — the answer is the same graph
/// read the same way, which is why [`devices_from_pw_dump`] exists as a
/// separate, testable parser rather than as a second opinion.
pub fn list_devices() -> Result<Vec<DeviceRow>> {
    match probe(&Allowlist::default()) {
        Ok(p) => Ok(devices_from(&p.sources, p.default_source.as_deref())),
        Err(live) => match pw_dump() {
            Ok(text) => devices_from_pw_dump(&text),
            Err(dump) => Err(live.context(format!("and `pw-dump` did not work either: {dump:#}"))),
        },
    }
}

fn pw_dump() -> Result<String> {
    let out = std::process::Command::new("pw-dump")
        .output()
        .context("running pw-dump")?;
    if !out.status.success() {
        return Err(anyhow!("pw-dump exited with {}", out.status));
    }
    String::from_utf8(out.stdout).context("pw-dump did not print UTF-8")
}

/// Parse `pw-dump`'s JSON into the same rows the live registry produces.
///
/// `pw-dump` prints an array of objects. Two kinds matter: `Node`s whose props
/// carry `media.class = "Audio/Source"`, and the `default` `Metadata` object,
/// whose `default.audio.source` entry names the current input. Everything else
/// — devices, ports, links, clients, the sinks — is skipped by shape rather
/// than by position, so a version of PipeWire that reorders the dump does not
/// change the answer.
pub fn devices_from_pw_dump(text: &str) -> Result<Vec<DeviceRow>> {
    let parsed: serde_json::Value =
        serde_json::from_str(text).context("parsing pw-dump output as JSON")?;
    let objects = parsed
        .as_array()
        .ok_or_else(|| anyhow!("pw-dump did not print a JSON array"))?;

    let mut sources = Vec::new();
    let mut default_source = None;
    for obj in objects {
        // The default input, as the session manager publishes it. Its `value`
        // is either the `{"name": ...}` object the metadata carries or, in
        // some dumps, the raw string.
        if obj.get("type").and_then(|t| t.as_str()) == Some("PipeWire:Interface:Metadata") {
            let named_default = obj
                .get("props")
                .and_then(|p| p.get("metadata.name"))
                .and_then(|n| n.as_str())
                == Some(DEFAULT_METADATA);
            if named_default {
                for entry in obj
                    .get("metadata")
                    .and_then(|m| m.as_array())
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                {
                    if entry.get("key").and_then(|k| k.as_str()) != Some(DEFAULT_SOURCE_KEY) {
                        continue;
                    }
                    default_source = match entry.get("value") {
                        Some(serde_json::Value::String(s)) => default_source_name(s)
                            .or_else(|| (!s.trim().is_empty()).then(|| s.trim().to_string())),
                        Some(v) => v
                            .get("name")
                            .and_then(|n| n.as_str())
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty()),
                        None => None,
                    };
                }
            }
            continue;
        }
        let props = obj.get("info").and_then(|i| i.get("props"));
        let Some(props) = props else { continue };
        if props.get("media.class").and_then(|c| c.as_str()) != Some(AUDIO_SOURCE_CLASS) {
            continue;
        }
        sources.push(SourceNode {
            node_id: obj.get("id").and_then(|i| i.as_u64()).unwrap_or(0) as u32,
            serial: props
                .get("object.serial")
                .map(|v| v.to_string().trim_matches('"').to_string()),
            name: props
                .get("node.name")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            description: props
                .get("node.description")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        });
    }
    Ok(devices_from(&sources, default_source.as_deref()))
}

// ---- end 0.10.0 -----------------------------------------------------------

/// One-shot enumeration of current playback streams and capture devices.
/// Never opens a capture.
pub fn probe(allowlist: &Allowlist) -> Result<Probe> {
    pw::init();

    let main_loop =
        pw::main_loop::MainLoopRc::new(None).context("creating the PipeWire main loop")?;
    let context =
        pw::context::ContextRc::new(&main_loop, None).context("creating the PipeWire context")?;
    let core = context
        .connect_rc(None)
        .context("connecting to PipeWire (is the daemon running?)")?;
    let registry = core
        .get_registry_rc()
        .context("getting the PipeWire registry")?;

    let found: Rc<RefCell<HashMap<u32, NodeInfo>>> = Rc::new(RefCell::new(HashMap::new()));
    let devices: Rc<RefCell<HashMap<u32, SourceNode>>> = Rc::new(RefCell::new(HashMap::new()));
    let default_source: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    // Bound proxies must outlive the roundtrip or their info events never fire.
    let proxies: Rc<RefCell<Vec<(pw::node::Node, pw::node::NodeListener)>>> =
        Rc::new(RefCell::new(Vec::new()));
    let metas: Rc<RefCell<Vec<(pw::metadata::Metadata, pw::metadata::MetadataListener)>>> =
        Rc::new(RefCell::new(Vec::new()));

    let found_reg = Rc::clone(&found);
    let devices_reg = Rc::clone(&devices);
    let default_reg = Rc::clone(&default_source);
    let proxies_reg = Rc::clone(&proxies);
    let metas_reg = Rc::clone(&metas);
    let registry_weak = registry.downgrade();
    let _reg_listener = registry
        .add_listener_local()
        .global(move |global| {
            let Some(registry) = registry_weak.upgrade() else {
                return;
            };
            // The default-source name lives on a metadata object, not on any
            // node: it is what the session manager would route a plain capture
            // stream to, and therefore what the mic tap follows.
            if global.type_ == ObjectType::Metadata {
                if global.props.as_ref().and_then(|p| p.get("metadata.name"))
                    != Some(DEFAULT_METADATA)
                {
                    return;
                }
                let meta: pw::metadata::Metadata = match registry.bind(global) {
                    Ok(m) => m,
                    Err(e) => {
                        debug!("could not bind the default metadata: {e}");
                        return;
                    }
                };
                let slot = Rc::clone(&default_reg);
                let listener = meta
                    .add_listener_local()
                    .property(move |_, key, _, value| {
                        if key == Some(DEFAULT_SOURCE_KEY) {
                            *slot.borrow_mut() = value.and_then(default_source_name);
                        }
                        0
                    })
                    .register();
                metas_reg.borrow_mut().push((meta, listener));
                return;
            }
            if global.type_ != ObjectType::Node {
                return;
            }
            // The registry announcement usually carries media.class already;
            // when it does, skip anything that is neither a playback stream nor
            // a capture device rather than binding every node on the graph.
            if let Some(props) = global.props.as_ref()
                && let Some(class) = props.get(&pw::keys::MEDIA_CLASS)
                && !interesting_class(class)
            {
                return;
            }
            let node: pw::node::Node = match registry.bind(global) {
                Ok(n) => n,
                Err(e) => {
                    debug!("could not bind node {}: {e}", global.id);
                    return;
                }
            };
            let id = global.id;
            let found = Rc::clone(&found_reg);
            let devices = Rc::clone(&devices_reg);
            let listener = node
                .add_listener_local()
                .info(move |info| {
                    let Some(props) = info.props() else { return };
                    match graph_node_from_props(id, props) {
                        Some(GraphNode::App(n)) => {
                            found.borrow_mut().insert(id, n);
                        }
                        Some(GraphNode::Source(n)) => {
                            devices.borrow_mut().insert(id, n);
                        }
                        None => {}
                    }
                })
                .register();
            proxies_reg.borrow_mut().push((node, listener));
        })
        .register();

    // Two roundtrips: the first drains the registry and issues the binds, the
    // second waits for the resulting info events.
    let done = Rc::new(std::cell::Cell::new(0u8));
    let pending_first = core.sync(0).context("registry sync")?;
    let core_weak = core.downgrade();
    let loop_weak = main_loop.downgrade();
    let done_cb = Rc::clone(&done);
    let pending_second = Rc::new(std::cell::Cell::new(None));
    let pending_second_cb = Rc::clone(&pending_second);
    let _core_listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id != pw::core::PW_ID_CORE {
                return;
            }
            if seq == pending_first && done_cb.get() == 0 {
                done_cb.set(1);
                if let Some(core) = core_weak.upgrade()
                    && let Ok(p) = core.sync(0)
                {
                    pending_second_cb.set(Some(p));
                    return;
                }
            }
            if done_cb.get() == 1 && Some(seq) == pending_second_cb.get() {
                done_cb.set(2);
                if let Some(l) = loop_weak.upgrade() {
                    l.quit();
                }
            }
        })
        .register();

    main_loop.run();

    let mut apps: Vec<(NodeInfo, Decision)> = found
        .borrow()
        .values()
        .cloned()
        .map(|n| {
            let d = allowlist.decide_for(&n.ident);
            (n, d)
        })
        .collect();
    apps.sort_by_key(|(n, _)| n.node_id);

    let mut sources: Vec<SourceNode> = devices.borrow().values().cloned().collect();
    sources.sort_by_key(|n| n.node_id);

    Ok(Probe {
        apps,
        sources,
        default_source: default_source.borrow().clone(),
    })
}

/// The daemon loop: watch the graph and capture allowlisted sources until
/// SIGINT/SIGTERM. Returns once the loop has stopped; the caller closes the
/// queue so the inference thread can drain and exit.
pub fn run(
    cfg: &Config,
    store: Arc<std::sync::Mutex<Store>>,
    queue: Arc<EventQueue>,
    stats: Arc<Stats>,
    control: Arc<Control>,
    bus: Arc<Bus>,
) -> Result<()> {
    pw::init();

    let main_loop =
        pw::main_loop::MainLoopRc::new(None).context("creating the PipeWire main loop")?;
    let context =
        pw::context::ContextRc::new(&main_loop, None).context("creating the PipeWire context")?;
    let core = context
        .connect_rc(None)
        .context("connecting to PipeWire (is the daemon running?)")?;
    let registry = core
        .get_registry_rc()
        .context("getting the PipeWire registry")?;

    for sig in [pw::loop_::Signal::INT, pw::loop_::Signal::TERM] {
        let weak = main_loop.downgrade();
        // Leaked deliberately: the handler must live as long as the loop, and
        // the loop lives to the end of this function.
        std::mem::forget(main_loop.loop_().add_signal_local(sig, move || {
            info!("shutting down");
            if let Some(l) = weak.upgrade() {
                l.quit();
            }
        }));
    }

    // The microphone's row exists whether or not the mic is on, so the GUI has
    // something to render its switch against on a machine that has never
    // enabled it. Written before the loop starts, for the same reason the
    // allowlist mirror is.
    let mic_source_id = {
        let guard = store.lock().map_err(|_| anyhow!("store mutex poisoned"))?;
        guard.upsert_source_kind(MIC_MATCH_KEY, MIC_DISPLAY_NAME, KIND_MIC, utc_now_ns())?
    };
    // Same for the room microphone (0.10.0): the row exists so the switch can
    // be rendered on a machine that has never turned it on.
    let room_source_id = {
        let guard = store.lock().map_err(|_| anyhow!("store mutex poisoned"))?;
        guard.upsert_source_kind(ROOM_MATCH_KEY, ROOM_DISPLAY_NAME, KIND_ROOM, utc_now_ns())?
    };

    let shared = Rc::new(RefCell::new(Shared {
        allowlist: control.allowlist(),
        store,
        queue: Arc::clone(&queue),
        stats,
        control: Arc::clone(&control),
        bus,
        captures: HashMap::new(),
        nodes: HashMap::new(),
        mic: Mic {
            cfg: control.mic(),
            source_id: mic_source_id,
            capture: None,
            default_source: None,
            nodes: HashMap::new(),
            retry_after: None,
            announced: None,
        },
        room: Room {
            cfg: control.room(),
            source_id: room_source_id,
            capture: None,
            retry_after: None,
            announced: None,
        },
        format_param: target_format_param()?,
        quantum: cfg.capture.quantum,
    }));

    // Node proxies stay bound for the daemon's lifetime so we keep receiving
    // property updates; they are dropped when the node goes away.
    let proxies: Rc<RefCell<HashMap<u32, (pw::node::Node, pw::node::NodeListener)>>> =
        Rc::new(RefCell::new(HashMap::new()));
    // The default-devices metadata object. Bound for the daemon's lifetime:
    // every headset swap arrives as a property event on it.
    let metas: Rc<RefCell<HashMap<u32, (pw::metadata::Metadata, pw::metadata::MetadataListener)>>> =
        Rc::new(RefCell::new(HashMap::new()));

    let core_for_cb = core.clone();
    let core_for_meta = core.clone();
    let core_for_remove = core.clone();
    let shared_reg = Rc::clone(&shared);
    let shared_meta = Rc::clone(&shared);
    let shared_timer = Rc::clone(&shared);
    let shared_end = Rc::clone(&shared);
    let proxies_reg = Rc::clone(&proxies);
    let metas_reg = Rc::clone(&metas);
    let registry_weak = registry.downgrade();
    let _reg_listener = registry
        .add_listener_local()
        .global(move |global| {
            let Some(registry) = registry_weak.upgrade() else {
                return;
            };
            if global.type_ == ObjectType::Metadata {
                if global.props.as_ref().and_then(|p| p.get("metadata.name"))
                    != Some(DEFAULT_METADATA)
                {
                    return;
                }
                let meta: pw::metadata::Metadata = match registry.bind(global) {
                    Ok(m) => m,
                    Err(e) => {
                        debug!("could not bind the default metadata: {e}");
                        return;
                    }
                };
                let shared = Rc::clone(&shared_meta);
                let core = core_for_meta.clone();
                let listener = meta
                    .add_listener_local()
                    .property(move |_, key, _, value| {
                        // A `None` value is a removal: the session manager has
                        // no default source right now. Passing that through is
                        // what makes the tap wait rather than pin to a stale
                        // device name.
                        if key == Some(DEFAULT_SOURCE_KEY) {
                            let name = value.and_then(default_source_name);
                            shared.borrow_mut().on_default_source(&core, name);
                        }
                        0
                    })
                    .register();
                metas_reg.borrow_mut().insert(global.id, (meta, listener));
                return;
            }
            if global.type_ != ObjectType::Node {
                return;
            }
            if let Some(props) = global.props.as_ref()
                && let Some(class) = props.get(&pw::keys::MEDIA_CLASS)
                && !interesting_class(class)
            {
                return;
            }
            let node: pw::node::Node = match registry.bind(global) {
                Ok(n) => n,
                Err(e) => {
                    debug!("could not bind node {}: {e}", global.id);
                    return;
                }
            };
            let id = global.id;
            let shared = Rc::clone(&shared_reg);
            let core = core_for_cb.clone();
            // `info` fires once on bind with the full property set — including
            // application.process.binary, which the registry announcement does
            // not always carry — and again on any later change. Only the first
            // sighting starts an application capture; a capture *device* is
            // re-read every time, because its name and description are what the
            // mic tap resolves against and they can be corrected later.
            let started = std::cell::Cell::new(false);
            let listener = node
                .add_listener_local()
                .info(move |info| {
                    let Some(props) = info.props() else { return };
                    match graph_node_from_props(id, props) {
                        Some(GraphNode::App(n)) => {
                            if started.get() {
                                return;
                            }
                            started.set(true);
                            shared.borrow_mut().on_node(&core, n);
                        }
                        Some(GraphNode::Source(n)) => {
                            shared.borrow_mut().on_source_node(&core, n);
                        }
                        None => {}
                    }
                })
                .register();
            proxies_reg.borrow_mut().insert(id, (node, listener));
        })
        .global_remove(move |id| {
            proxies.borrow_mut().remove(&id);
            metas.borrow_mut().remove(&id);
            shared.borrow_mut().on_node_removed(&core_for_remove, id);
        })
        .register();

    let core_weak = main_loop.downgrade();
    let _core_listener = core
        .add_listener_local()
        .error(move |id, _seq, res, message| {
            error!("PipeWire error on object {id}: {message} ({res})");
            if id == pw::core::PW_ID_CORE {
                // The daemon went away. Stop cleanly instead of spinning.
                if let Some(l) = core_weak.upgrade() {
                    l.quit();
                }
            }
        })
        .register();

    // `sources.set` and `mic.set` arrive on a socket thread, which must not
    // touch this loop's objects. They bump a generation counter instead and
    // this timer notices — a quarter of a second between the click and the
    // microphone, with no cross-thread access to a PipeWire proxy anywhere.
    //
    // The mic sync runs on every tick, not only on a generation change: follow
    // mode also has to react to an application session ending for a reason the
    // socket never hears about (the app quit), and a retry after a missing
    // device has to happen on a clock rather than on an event.
    let control_timer = Arc::clone(&control);
    let core_timer = core.clone();
    let applied_rules = std::cell::Cell::new(control.rules_generation());
    let applied_mic = std::cell::Cell::new(control.mic_generation());
    let applied_room = std::cell::Cell::new(control.room_generation());
    let timer = main_loop.loop_().add_timer(move |_| {
        let rules_gen = control_timer.rules_generation();
        if rules_gen != applied_rules.get() {
            applied_rules.set(rules_gen);
            shared_timer
                .borrow_mut()
                .apply_rules(&core_timer, control_timer.allowlist());
        }
        let mic_gen = control_timer.mic_generation();
        if mic_gen != applied_mic.get() {
            applied_mic.set(mic_gen);
            shared_timer.borrow_mut().mic.cfg = control_timer.mic();
        }
        let room_gen = control_timer.room_generation();
        if room_gen != applied_room.get() {
            applied_room.set(room_gen);
            shared_timer.borrow_mut().room.cfg = control_timer.room();
        }
        shared_timer.borrow_mut().sync_taps(&core_timer);
    });
    let tick = std::time::Duration::from_millis(250);
    if let Err(e) = timer.update_timer(Some(tick), Some(tick)).into_result() {
        warn!(
            "could not arm the rule-change timer ({e:?}); source toggles and the \
             microphone switch will need a restart"
        );
    }

    info!(
        rules = control.allowlist().len(),
        queue_seconds = cfg.capture.queue_seconds,
        mic = control.mic_state(),
        room = control.room_state(),
        "watching for application audio streams"
    );
    main_loop.run();

    // Leaving the loop drops every stream; close the mic row explicitly so a
    // shutdown does not leave a dangling session for the next start-up to sweep.
    shared_end.borrow_mut().stop_mic();
    control.set_mic_active(false);
    shared_end.borrow_mut().stop_room();
    control.set_room_active(false);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn situation<'a>(
        enabled: bool,
        mode: MicMode,
        apps: usize,
        open_on: Option<Option<&'a str>>,
        target: Option<&'a str>,
    ) -> MicSituation<'a> {
        MicSituation {
            enabled,
            mode,
            app_captures: apps,
            open_on,
            target,
            // The ordinary world: the target is there and the stream is fine.
            // The tests that care take this apart themselves.
            target_on_graph: true,
            open_failed: false,
        }
    }

    #[test]
    fn a_disabled_microphone_is_closed_whatever_else_is_true() {
        for mode in [MicMode::Follow, MicMode::Always] {
            for apps in [0usize, 1, 5] {
                assert_eq!(
                    mic_plan(&situation(false, mode, apps, None, Some("mic0"))),
                    MicPlan::Hold,
                    "off and closed is nothing to do"
                );
                assert_eq!(
                    mic_plan(&situation(
                        false,
                        mode,
                        apps,
                        Some(Some("mic0")),
                        Some("mic0")
                    )),
                    MicPlan::Close,
                    "off with a session open closes it"
                );
            }
        }
    }

    #[test]
    fn follow_mode_opens_with_the_first_app_and_closes_with_the_last() {
        let t = Some("mic0");
        // Enabled, nothing captured: waiting, not recording. This is the
        // default state of the feature and the reason it is acceptable.
        assert_eq!(
            mic_plan(&situation(true, MicMode::Follow, 0, None, t)),
            MicPlan::Hold
        );
        // VRChat opens a stream.
        assert_eq!(
            mic_plan(&situation(true, MicMode::Follow, 1, None, t)),
            MicPlan::Open
        );
        // A second app changes nothing — the mic is already open.
        assert_eq!(
            mic_plan(&situation(true, MicMode::Follow, 2, Some(t), t)),
            MicPlan::Hold
        );
        // One of two closing changes nothing either.
        assert_eq!(
            mic_plan(&situation(true, MicMode::Follow, 1, Some(t), t)),
            MicPlan::Hold
        );
        // The LAST one closing is what closes the mic.
        assert_eq!(
            mic_plan(&situation(true, MicMode::Follow, 0, Some(t), t)),
            MicPlan::Close
        );
    }

    #[test]
    fn always_mode_does_not_look_at_the_application_list() {
        let t = Some("mic0");
        assert_eq!(
            mic_plan(&situation(true, MicMode::Always, 0, None, t)),
            MicPlan::Open,
            "no app is captured and it records anyway — that is the difference"
        );
        assert_eq!(
            mic_plan(&situation(true, MicMode::Always, 0, Some(t), t)),
            MicPlan::Hold
        );
        assert_eq!(
            mic_plan(&situation(true, MicMode::Always, 3, Some(t), t)),
            MicPlan::Hold
        );
        // Only the switch closes it.
        assert_eq!(
            mic_plan(&situation(false, MicMode::Always, 3, Some(t), t)),
            MicPlan::Close
        );
    }

    #[test]
    fn a_device_change_reopens_rather_than_carrying_on() {
        // One session must never span two physical microphones: the user
        // swapped headsets, and the provenance of the audio changed with it.
        assert_eq!(
            mic_plan(&situation(
                true,
                MicMode::Always,
                0,
                Some(Some("usb-yeti")),
                Some("bluetooth-headset")
            )),
            MicPlan::Reopen
        );
        // Losing the default entirely is also a change: the session manager
        // will route the new stream, and that may not be the same device.
        assert_eq!(
            mic_plan(&situation(
                true,
                MicMode::Always,
                0,
                Some(Some("usb-yeti")),
                None
            )),
            MicPlan::Reopen
        );
        // Gaining a default when the session was opened without one, likewise.
        assert_eq!(
            mic_plan(&situation(
                true,
                MicMode::Always,
                0,
                Some(None),
                Some("usb-yeti")
            )),
            MicPlan::Reopen
        );
        // The same device is not a change.
        assert_eq!(
            mic_plan(&situation(
                true,
                MicMode::Always,
                0,
                Some(Some("usb-yeti")),
                Some("usb-yeti")
            )),
            MicPlan::Hold
        );
    }

    #[test]
    fn switching_modes_while_no_app_runs_is_the_whole_difference() {
        // The one transition a user actually performs in the UI: the switch is
        // on, nothing is being captured, and the mode picker decides whether
        // the room is being recorded.
        let idle_follow = situation(true, MicMode::Follow, 0, None, Some("mic0"));
        let idle_always = MicSituation {
            mode: MicMode::Always,
            ..idle_follow
        };
        assert_eq!(mic_plan(&idle_follow), MicPlan::Hold);
        assert_eq!(mic_plan(&idle_always), MicPlan::Open);

        // …and back, with a session now open.
        let open_always = situation(true, MicMode::Always, 0, Some(Some("mic0")), Some("mic0"));
        let open_follow = MicSituation {
            mode: MicMode::Follow,
            ..open_always
        };
        assert_eq!(mic_plan(&open_always), MicPlan::Hold);
        assert_eq!(mic_plan(&open_follow), MicPlan::Close);
    }

    #[test]
    fn the_default_source_metadata_value_is_read_or_ignored_but_never_fatal() {
        assert_eq!(
            default_source_name(r#"{"name":"alsa_input.usb-Blue_Yeti"}"#).as_deref(),
            Some("alsa_input.usb-Blue_Yeti")
        );
        // Everything the session manager could hand us that we cannot use is
        // "no default known", which makes the tap wait rather than guess.
        assert_eq!(default_source_name(r#"{"name":"  "}"#), None);
        assert_eq!(default_source_name(r#"{"other":"x"}"#), None);
        assert_eq!(default_source_name("null"), None);
        assert_eq!(default_source_name("not json"), None);
        assert_eq!(default_source_name(""), None);
    }

    #[test]
    fn only_the_two_node_classes_we_act_on_are_bound() {
        assert!(interesting_class(PLAYBACK_STREAM_CLASS));
        assert!(interesting_class(AUDIO_SOURCE_CLASS));
        // A sink is emphatically not one of them: capturing a sink's monitor
        // would re-merge every application (DESIGN §3).
        assert!(!interesting_class("Audio/Sink"));
        assert!(!interesting_class("Stream/Input/Audio"));
        assert!(!interesting_class("Video/Source"));
    }

    #[test]
    fn a_capture_device_prefers_its_serial_and_always_has_a_label() {
        let full = SourceNode {
            node_id: 58,
            serial: Some("1234".into()),
            name: Some("alsa_input.usb-Blue_Yeti".into()),
            description: Some("Yeti Stereo Microphone".into()),
        };
        assert_eq!(full.target().as_deref(), Some("1234"));
        assert_eq!(full.label(), "Yeti Stereo Microphone");

        let no_serial = SourceNode {
            serial: None,
            ..full.clone()
        };
        assert_eq!(
            no_serial.target().as_deref(),
            Some("alsa_input.usb-Blue_Yeti")
        );

        let bare = SourceNode {
            node_id: 7,
            serial: None,
            name: None,
            description: None,
        };
        assert_eq!(bare.target(), None);
        assert_eq!(bare.label(), "node 7");
    }

    #[test]
    fn probe_resolves_the_pin_ahead_of_the_default() {
        let yeti = SourceNode {
            node_id: 58,
            serial: Some("1234".into()),
            name: Some("usb-yeti".into()),
            description: Some("Yeti Stereo Microphone".into()),
        };
        let webcam = SourceNode {
            node_id: 61,
            serial: Some("1240".into()),
            name: Some("usb-webcam".into()),
            description: Some("HD Webcam".into()),
        };
        let probe = Probe {
            apps: Vec::new(),
            sources: vec![yeti.clone(), webcam.clone()],
            default_source: Some("usb-webcam".into()),
        };
        assert_eq!(probe.mic_target(None), Some(&webcam));
        assert_eq!(probe.mic_target(Some("usb-yeti")), Some(&yeti));
        // A pin that names nothing on the graph resolves to nothing, rather
        // than quietly falling back to a device the user did not ask for.
        assert_eq!(probe.mic_target(Some("usb-gone")), None);

        let no_default = Probe {
            default_source: None,
            ..probe
        };
        assert_eq!(no_default.mic_target(None), None);
    }

    // ---- audit finding #2: a source nobody was told about ----------------

    /// Everything `note_source` needs, and nothing that needs a PipeWire graph.
    /// The registry callbacks cannot run in a test — there is no daemon and no
    /// core — but the half that decides what the rest of the program is told
    /// can, and it is the half the finding is about.
    fn graphless(dir: &std::path::Path, rules: &[(&str, bool)]) -> (Shared, Arc<Bus>) {
        let store = Store::open(dir).unwrap();
        let mic_source_id = store
            .upsert_source_kind(MIC_MATCH_KEY, MIC_DISPLAY_NAME, KIND_MIC, 0)
            .unwrap();
        let room_source_id = store
            .upsert_source_kind(ROOM_MATCH_KEY, ROOM_DISPLAY_NAME, KIND_ROOM, 0)
            .unwrap();
        let bus = Bus::new(64, 32);
        let allowlist = Allowlist::from_rules(rules.iter().copied());
        let control = Control::new(dir.to_path_buf(), None, &allowlist);
        (
            Shared {
                allowlist,
                store: Arc::new(std::sync::Mutex::new(store)),
                queue: crate::queue::EventQueue::new(1024),
                stats: Arc::new(Stats::default()),
                control,
                bus: Arc::clone(&bus),
                captures: HashMap::new(),
                nodes: HashMap::new(),
                mic: Mic {
                    cfg: MicConfig::default(),
                    source_id: mic_source_id,
                    capture: None,
                    default_source: None,
                    nodes: HashMap::new(),
                    retry_after: None,
                    announced: None,
                },
                room: Room {
                    cfg: RoomConfig::default(),
                    source_id: room_source_id,
                    capture: None,
                    retry_after: None,
                    announced: None,
                },
                // Never read here: nothing in this test opens a stream.
                format_param: Vec::new(),
                quantum: 1024,
            },
            bus,
        )
    }

    /// The consequence in the audit, as a test: open a client, then have an
    /// application appear. Before this, the only `source` event in the daemon
    /// came from `sources.set` — so a program launched after the GUI connected
    /// never turned up in its Sources list, and could therefore never be
    /// allowed without restarting the GUI.
    #[test]
    fn an_application_appearing_is_broadcast_to_the_clients_already_connected() {
        let dir = std::env::temp_dir().join(format!("nx-recall-capture-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (mut shared, bus) = graphless(&dir, &[("VRChat.exe", true)]);
        let (client, rx) = bus.attach(None);
        client.subscribe(&[Topic::Sources]);

        // A program that is not allowed. Default-deny only works if what was
        // denied is visible, so this is exactly the event that matters.
        let node = NodeInfo {
            node_id: 42,
            serial: Some("9001".into()),
            ident: SourceIdent {
                process_binary: Some("Discord".into()),
                application_name: Some("Discord".into()),
                node_name: Some("discord-node".into()),
                process_id: Some(4242),
            },
        };
        let key = node.ident.match_key();
        assert!(shared.note_source(&node).is_some());

        let raw = rx.try_recv().expect("a source event, on first sighting");
        let ev: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(ev["ev"], "source");
        let data = &ev["data"];
        assert_eq!(data["match_key"], serde_json::json!(key));
        assert_eq!(data["state"], serde_json::json!(SOURCE_SEEN));
        assert_eq!(data["allowed"], serde_json::json!(false));
        assert_eq!(data["kind"], serde_json::json!(crate::store::KIND_APP));
        assert_eq!(data["streams"], serde_json::json!(0));
        // The `sources.list` row shape, whole: a client folds this in exactly
        // as it folds in a list entry and cannot end up with a half-row.
        for field in [
            "id",
            "match_key",
            "kind",
            "binary",
            "display",
            "display_name",
            "allowed",
            "first_seen",
            "last_seen",
            "streams",
        ] {
            assert!(data.get(field).is_some(), "the event must carry {field}");
        }

        // And the shape really is the list's, field for field.
        let listed = {
            let store = shared.store.lock().unwrap();
            store.source_row(&key).unwrap().unwrap()
        };
        let mut expected = crate::service::source_json(&listed, false);
        expected["state"] = serde_json::json!(SOURCE_SEEN);
        assert_eq!(data, &expected);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- audit finding #7: a microphone that is not there ----------------

    /// The bug, in one table. A pinned `[mic].device` that is unplugged keeps
    /// `target == open_on` — the pin is read from the config, not from the
    /// graph — so the machine said `Hold` forever: the session row never
    /// closed, `sources_capturing` went on counting it, and `status` reported
    /// `always:active` over a stream sitting in `StreamState::Error`.
    #[test]
    fn a_pinned_microphone_that_left_the_graph_is_closed_not_held() {
        let pin = Some("usb-yeti");
        let mut s = situation(true, MicMode::Always, 0, Some(pin), pin);

        // Present: this is the ordinary steady state and must stay `Hold`.
        assert_eq!(mic_plan(&s), MicPlan::Hold);

        // Unplugged. Same name, same config, no such device.
        s.target_on_graph = false;
        assert_eq!(
            mic_plan(&s),
            MicPlan::Close,
            "a microphone that is not there must not read as recording"
        );

        // Still gone and still closed: nothing to do, and in particular no
        // attempt to reopen on a name the graph does not answer to.
        assert_eq!(
            mic_plan(&MicSituation { open_on: None, ..s }),
            MicPlan::Open,
            "with the session closed the ordinary retry takes over"
        );

        // Replugged. The device comes back on a NEW object.serial — serials are
        // never reused — which is exactly why the pin is resolved by node.name.
        s.target_on_graph = true;
        assert_eq!(mic_plan(&s), MicPlan::Hold);

        // And in follow mode the same is true, with the app list on top.
        let mut f = situation(true, MicMode::Follow, 1, Some(pin), pin);
        assert_eq!(mic_plan(&f), MicPlan::Hold);
        f.target_on_graph = false;
        assert_eq!(mic_plan(&f), MicPlan::Close);
    }

    /// A stream the graph errored out is not a recording, whatever the config
    /// still says the device is called. This is the other half of #7: the
    /// device can vanish under a live tap without the node-removed path being
    /// the thing that notices.
    #[test]
    fn a_failed_stream_closes_the_session_rather_than_reporting_it_open() {
        let t = Some("usb-yeti");
        let mut s = situation(true, MicMode::Always, 0, Some(t), t);
        assert_eq!(mic_plan(&s), MicPlan::Hold);

        s.open_failed = true;
        assert_eq!(mic_plan(&s), MicPlan::Close);

        // A failed stream on a device that has ALSO gone is still one Close,
        // not a reopen onto nothing.
        s.target_on_graph = false;
        assert_eq!(mic_plan(&s), MicPlan::Close);

        // Off beats everything, including this — rule 1 is still rule 1.
        s.enabled = false;
        assert_eq!(mic_plan(&s), MicPlan::Close);
        assert_eq!(
            mic_plan(&MicSituation { open_on: None, ..s }),
            MicPlan::Hold
        );
    }

    /// The default-source path is deliberately untouched by the two inputs
    /// above: a *default* that names nothing on the graph is not an error —
    /// connecting with no target lets the session manager route the stream,
    /// which is what every other capture client does. Only an explicit pin can
    /// be "missing".
    #[test]
    fn an_unresolvable_default_is_not_the_same_as_a_missing_pin() {
        // No pin configured: `target_on_graph` is true whatever the default
        // resolves to, so a routed stream holds rather than flapping.
        let t = Some("some-default");
        assert_eq!(
            mic_plan(&situation(true, MicMode::Always, 0, Some(t), t)),
            MicPlan::Hold
        );
        assert_eq!(
            mic_plan(&situation(true, MicMode::Always, 0, Some(None), None)),
            MicPlan::Hold
        );
    }
}

#[cfg(test)]
mod device_tests {
    use super::*;

    /// A cut-down `pw-dump`, in the shape PipeWire really prints: an array of
    /// heterogeneous objects, the interesting ones identified by `media.class`
    /// and by `metadata.name`, in no useful order.
    const DUMP: &str = r#"[
      { "id": 30, "type": "PipeWire:Interface:Device",
        "info": { "props": { "device.name": "alsa_card.usb-Yeti" } } },
      { "id": 31, "type": "PipeWire:Interface:Node",
        "info": { "props": {
          "media.class": "Audio/Sink",
          "node.name": "alsa_output.pci-0000_0c_00.4.analog-stereo",
          "node.description": "Starship/Matisse Analog Stereo" } } },
      { "id": 45, "type": "PipeWire:Interface:Node",
        "info": { "props": {
          "media.class": "Audio/Source",
          "object.serial": 812,
          "node.name": "alsa_input.usb-Blue_Yeti-00.analog-stereo",
          "node.description": "Yeti Stereo Microphone" } } },
      { "id": 46, "type": "PipeWire:Interface:Node",
        "info": { "props": {
          "media.class": "Audio/Source",
          "object.serial": 813,
          "node.name": "alsa_input.pci-0000_0c_00.4.analog-stereo",
          "node.description": "Built-in Audio Analog Stereo" } } },
      { "id": 47, "type": "PipeWire:Interface:Node",
        "info": { "props": {
          "media.class": "Stream/Output/Audio",
          "node.name": "VRChat.exe",
          "application.process.binary": "wine64-preloader" } } },
      { "id": 52, "type": "PipeWire:Interface:Metadata",
        "props": { "metadata.name": "default" },
        "metadata": [
          { "key": "default.audio.sink", "type": "Spa:String:JSON",
            "value": { "name": "alsa_output.pci-0000_0c_00.4.analog-stereo" } },
          { "key": "default.audio.source", "type": "Spa:String:JSON",
            "value": { "name": "alsa_input.usb-Blue_Yeti-00.analog-stereo" } }
        ] }
    ]"#;

    #[test]
    fn the_pw_dump_parser_finds_the_capture_devices_and_the_default() {
        let rows = devices_from_pw_dump(DUMP).expect("the canned dump parses");
        assert_eq!(
            rows.len(),
            2,
            "sinks, playback streams and devices are not capture devices: {rows:?}"
        );
        // Default first — a client renders it as the one to avoid, because it
        // is the headset the `[mic]` tap is already on.
        assert_eq!(
            rows[0].node_name,
            "alsa_input.usb-Blue_Yeti-00.analog-stereo"
        );
        assert_eq!(
            rows[0].description.as_deref(),
            Some("Yeti Stereo Microphone")
        );
        assert!(rows[0].is_default);
        assert_eq!(
            rows[1].node_name,
            "alsa_input.pci-0000_0c_00.4.analog-stereo"
        );
        assert!(!rows[1].is_default);
    }

    #[test]
    fn a_dump_with_no_default_metadata_still_lists_devices() {
        // What a bare PipeWire with no session manager looks like. Every row
        // is offerable; none is the default, and none claims to be.
        let stripped: String = DUMP
            .lines()
            .filter(|l| !l.contains("Metadata") && !l.contains("default.audio"))
            .collect::<Vec<_>>()
            .join("\n")
            .replace("\"metadata\": [\n", "")
            .replace("        ] }\n", "");
        // The filtered text is no longer valid JSON; build the case directly
        // instead of hand-editing a literal, which is what the parser's own
        // contract is about.
        let no_meta = DUMP.replace("default.audio.source", "default.audio.source.disabled");
        let rows = devices_from_pw_dump(&no_meta).expect("parses");
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter().all(|r| !r.is_default),
            "nothing may claim to be the default when nothing says so"
        );
        // Sorted by name when there is no default to hoist.
        assert!(rows[0].node_name < rows[1].node_name);
        let _ = stripped;
    }

    #[test]
    fn junk_is_refused_rather_than_guessed_at() {
        assert!(devices_from_pw_dump("not json at all").is_err());
        assert!(
            devices_from_pw_dump("{\"id\": 1}").is_err(),
            "pw-dump prints an array; an object is not one"
        );
        assert!(devices_from_pw_dump("[]").unwrap().is_empty());
        // A source with no `node.name` cannot be pinned, so it is not offered.
        let nameless = r#"[{ "id": 1, "type": "PipeWire:Interface:Node",
            "info": { "props": { "media.class": "Audio/Source" } } }]"#;
        assert!(devices_from_pw_dump(nameless).unwrap().is_empty());
    }

    #[test]
    fn a_string_valued_default_is_read_too() {
        // Some dumps print the metadata value as a JSON *string* containing
        // the object. Both shapes mean the same device.
        let dump = DUMP.replace(
            "{ \"name\": \"alsa_input.usb-Blue_Yeti-00.analog-stereo\" }",
            "\"{\\\"name\\\":\\\"alsa_input.usb-Blue_Yeti-00.analog-stereo\\\"}\"",
        );
        let rows = devices_from_pw_dump(&dump).expect("parses");
        assert!(rows[0].is_default, "{rows:?}");
    }
}
