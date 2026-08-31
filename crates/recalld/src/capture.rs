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

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result, anyhow};
use pipewire as pw;
use pw::spa;
use pw::{properties::properties, types::ObjectType};
use spa::param::format::{MediaSubtype, MediaType};
use spa::param::format_utils;
use spa::pod::Pod;
use tracing::{debug, error, info, warn};

use crate::allowlist::{Allowlist, Decision, SourceIdent};
use crate::clock::{monotonic_ns, utc_now_ns};
use crate::config::{Config, SAMPLE_RATE};
use crate::pipeline::Stats;
use crate::queue::{AudioChunk, CaptureEvent, EventQueue};
use crate::resample::{LinearResampler, downmix};
use crate::store::Store;

const PLAYBACK_STREAM_CLASS: &str = "Stream/Output/Audio";

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
}

/// A live capture: the stream, its listener, and the session row it feeds.
struct Capture {
    session_id: i64,
    match_key: String,
    _stream: pw::stream::StreamRc,
    _listener: pw::stream::StreamListener<StreamData>,
}

/// Everything the registry callbacks need to mutate.
struct Shared {
    allowlist: Allowlist,
    store: Arc<std::sync::Mutex<Store>>,
    queue: Arc<EventQueue>,
    stats: Arc<Stats>,
    captures: HashMap<u32, Capture>,
    format_param: Vec<u8>,
    quantum: u32,
}

impl Shared {
    /// Note a node in the sources table and, if allowed, start capturing it.
    fn on_node(&mut self, core: &pw::core::CoreRc, node: NodeInfo) {
        let match_key = node.ident.match_key();
        let display_name = node.ident.display_name();
        let decision = self.allowlist.decide(&match_key);

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
                    return;
                }
            }
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
            store.begin_session(source_id, utc_now_ns())?
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
        };

        let listener = stream
            .add_local_listener_with_user_data(data)
            .state_changed(|_, data, old, new| {
                debug!(session = data.session_id, label = %data.label, "stream {old:?} -> {new:?}");
                if let pw::stream::StreamState::Error(msg) = &new {
                    // A device vanishing (headset unplugged) lands here. Log and
                    // let the node-remove path close the session; do not panic.
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
            .context("registering stream callbacks")?;

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
        Ok(())
    }

    fn on_node_removed(&mut self, node_id: u32) {
        let Some(capture) = self.captures.remove(&node_id) else {
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
    }
}

/// One-shot enumeration of current playback streams. Never opens a capture.
pub fn probe(allowlist: &Allowlist) -> Result<Vec<(NodeInfo, Decision)>> {
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
    // Bound proxies must outlive the roundtrip or their info events never fire.
    let proxies: Rc<RefCell<Vec<(pw::node::Node, pw::node::NodeListener)>>> =
        Rc::new(RefCell::new(Vec::new()));

    let found_reg = Rc::clone(&found);
    let proxies_reg = Rc::clone(&proxies);
    let registry_weak = registry.downgrade();
    let _reg_listener = registry
        .add_listener_local()
        .global(move |global| {
            if global.type_ != ObjectType::Node {
                return;
            }
            // The registry announcement usually carries media.class already;
            // when it does, skip anything that is not a playback stream rather
            // than binding every node on the graph.
            if let Some(props) = global.props.as_ref()
                && let Some(class) = props.get(&pw::keys::MEDIA_CLASS)
                && class != PLAYBACK_STREAM_CLASS
            {
                return;
            }
            let Some(registry) = registry_weak.upgrade() else {
                return;
            };
            let node: pw::node::Node = match registry.bind(global) {
                Ok(n) => n,
                Err(e) => {
                    debug!("could not bind node {}: {e}", global.id);
                    return;
                }
            };
            let id = global.id;
            let found = Rc::clone(&found_reg);
            let listener = node
                .add_listener_local()
                .info(move |info| {
                    if let Some(props) = info.props()
                        && let Some(n) = node_info_from_props(id, props)
                    {
                        found.borrow_mut().insert(id, n);
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

    let mut out: Vec<(NodeInfo, Decision)> = found
        .borrow()
        .values()
        .cloned()
        .map(|n| {
            let d = allowlist.decide_for(&n.ident);
            (n, d)
        })
        .collect();
    out.sort_by_key(|(n, _)| n.node_id);
    Ok(out)
}

/// The daemon loop: watch the graph and capture allowlisted sources until
/// SIGINT/SIGTERM. Returns once the loop has stopped; the caller closes the
/// queue so the inference thread can drain and exit.
pub fn run(
    cfg: &Config,
    store: Arc<std::sync::Mutex<Store>>,
    queue: Arc<EventQueue>,
    stats: Arc<Stats>,
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

    let shared = Rc::new(RefCell::new(Shared {
        allowlist: cfg.allowlist(),
        store,
        queue: Arc::clone(&queue),
        stats,
        captures: HashMap::new(),
        format_param: target_format_param()?,
        quantum: cfg.capture.quantum,
    }));

    // Node proxies stay bound for the daemon's lifetime so we keep receiving
    // property updates; they are dropped when the node goes away.
    let proxies: Rc<RefCell<HashMap<u32, (pw::node::Node, pw::node::NodeListener)>>> =
        Rc::new(RefCell::new(HashMap::new()));

    let core_for_cb = core.clone();
    let shared_reg = Rc::clone(&shared);
    let proxies_reg = Rc::clone(&proxies);
    let registry_weak = registry.downgrade();
    let _reg_listener = registry
        .add_listener_local()
        .global(move |global| {
            if global.type_ != ObjectType::Node {
                return;
            }
            if let Some(props) = global.props.as_ref()
                && let Some(class) = props.get(&pw::keys::MEDIA_CLASS)
                && class != PLAYBACK_STREAM_CLASS
            {
                return;
            }
            let Some(registry) = registry_weak.upgrade() else {
                return;
            };
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
            // sighting starts a capture.
            let started = std::cell::Cell::new(false);
            let listener = node
                .add_listener_local()
                .info(move |info| {
                    if started.get() {
                        return;
                    }
                    let Some(props) = info.props() else { return };
                    let Some(n) = node_info_from_props(id, props) else {
                        return;
                    };
                    started.set(true);
                    shared.borrow_mut().on_node(&core, n);
                })
                .register();
            proxies_reg.borrow_mut().insert(id, (node, listener));
        })
        .global_remove(move |id| {
            proxies.borrow_mut().remove(&id);
            shared.borrow_mut().on_node_removed(id);
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

    info!(
        rules = cfg.allowlist().len(),
        queue_seconds = cfg.capture.queue_seconds,
        "watching for application audio streams"
    );
    main_loop.run();
    Ok(())
}
