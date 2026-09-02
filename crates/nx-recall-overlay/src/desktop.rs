//! `--desktop`: the captions as a **wlr-layer-shell** surface.
//!
//! ## Why this exists at all
//!
//! The captions have always claimed to be click-through. On X11 that claim was
//! true: `BrowserWindow.setIgnoreMouseEvents(true)` sets an empty X11 input
//! shape and the pointer falls through to the game underneath. **Measured on
//! this machine, Electron 44 on KDE Wayland, it sets nothing and does nothing.**
//! There is no Wayland path in Chromium for "this window is scenery": a
//! `wl_surface`'s input region is compositor-side state a toplevel client does
//! not get to set through Electron's API. So the setting was a lie in the UI,
//! and a caption bar that eats a click into the game is exactly the bug the
//! setting exists to prevent.
//!
//! A Wayland client CAN set that region — `wl_surface.set_input_region` with an
//! empty `wl_region`, which is a promise the COMPOSITOR keeps rather than one
//! the toolkit tries to. That is the whole reason this module is Rust and not
//! more Electron: it is the one line Electron cannot say.
//!
//! ## The shape of it
//!
//! - `zwlr_layer_shell_v1`, layer `OVERLAY`, so it stays above a fullscreen
//!   game rather than behind it.
//! - Anchored to the BOTTOM, sized from the same `captions.json` the Electron
//!   window is sized from, with an exclusive zone of −1 so no maximised window
//!   ever gets shoved up to make room for scenery.
//! - `keyboard_interactivity: none`, and an EMPTY input region. Between them,
//!   the compositor has nothing to deliver here: no pointer, no touch, no keys.
//! - `wl_shm` and the CPU rasteriser in `raster.rs`. No GPU, no wgpu, no Vulkan
//!   — see `frame time` in docs/OVERLAY.md for what that costs.
//!
//! If the compositor does not offer `zwlr_layer_shell_v1`, this prints one line
//! and exits **2**, which the Electron side reads as "use the BrowserWindow".

use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Sender, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::json;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, FrameCallbackData, Region},
    delegate_dispatch2, delegate_registry,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use wayland_client::{
    Connection, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_output, wl_shm, wl_surface},
};

use crate::feed::{self, Captions, Turn, visible};
use crate::layout;
use crate::raster;
use crate::settings::CaptionSettings;

/// The exit code that means "this compositor cannot host a layer surface; use
/// the Electron window". Read by gui/src/main/captions.js — change one and the
/// other stops falling back.
pub const NO_LAYER_SHELL: i32 = 2;

/// How deep the turn ring is, independent of how many are shown. The desktop
/// window's `KEEP`, and for its reason: raising the `turns` slider must show
/// the turns that already happened rather than an empty bar, and an all-night
/// session must not be a leak.
const KEEP: usize = 40;

/// The rasteriser's padding at scale 1, matching `raster::Style::default()`.
const PAD: i64 = 18;

// ---------------------------------------------------------------------------
// the two things that can be decided without a compositor, and therefore tested
// ---------------------------------------------------------------------------

/// What this surface asks the compositor to deliver to it.
///
/// There is one variant that this feature is ever allowed to produce, and the
/// test below is the reason the enum exists rather than a bare call: the
/// click-through TOGGLE must not be able to reach this decision. On the layer
/// path the bar is scenery, always, and a settings file that says otherwise is
/// a settings file describing the other surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputRegion {
    /// `wl_surface.set_input_region` with a region that has no rectangles in
    /// it. Not "the client ignores clicks" — the compositor never sends any.
    Empty,
}

pub fn input_region(_settings: &CaptionSettings) -> InputRegion {
    InputRegion::Empty
}

/// Everything the layer surface is configured with, worked out from the
/// settings and the output before a single Wayland request is sent.
#[derive(Debug, Clone, PartialEq)]
pub struct LayerConfig {
    pub layer: Layer,
    pub anchor: Anchor,
    /// Logical pixels.
    pub width: u32,
    pub height: u32,
    /// `(top, right, bottom, left)`, the order `set_margin` takes.
    pub margin: (i32, i32, i32, i32),
    pub keyboard: KeyboardInteractivity,
    /// −1: never reserve screen space. A caption bar that pushed a maximised
    /// window up would be furniture that rearranges the room.
    pub exclusive_zone: i32,
    pub input: InputRegion,
}

pub fn layer_config(
    settings: &CaptionSettings,
    output: (u32, u32),
    margin_override: Option<i32>,
) -> LayerConfig {
    let (width, height) = layout::surface_size(settings.bounds, output);
    let bottom =
        margin_override.unwrap_or_else(|| layout::bottom_margin(settings.bounds, height, output));
    LayerConfig {
        // OVERLAY and not TOP: TOP loses to a fullscreen window, and a
        // fullscreen window is the thing these captions are for.
        layer: Layer::Overlay,
        // BOTTOM alone, with no LEFT or RIGHT, is what centres a fixed-width
        // surface horizontally. Adding either side would stretch it.
        anchor: Anchor::BOTTOM,
        width,
        height,
        margin: (0, 0, bottom, 0),
        keyboard: KeyboardInteractivity::None,
        exclusive_zone: -1,
        input: input_region(settings),
    }
}

// ---------------------------------------------------------------------------
// the run
// ---------------------------------------------------------------------------

pub struct Options {
    pub socket: Option<PathBuf>,
    pub settings: Option<PathBuf>,
    pub output: Option<String>,
    pub margin: Option<i32>,
    pub font: Option<PathBuf>,
    /// Draw for this long and then leave. Used by the one live check that is
    /// allowed to happen on the user's own desktop; `None` means run.
    pub seconds: Option<f32>,
}

pub fn run(opts: Options) -> Result<()> {
    let settings_path = opts
        .settings
        .clone()
        .unwrap_or_else(crate::settings::default_settings_path);
    let settings = CaptionSettings::load(&settings_path);
    eprintln!(
        "[overlay] settings {} — turns {}, size {}, hold {}s, ground {:.2}, showYou {}",
        settings_path.display(),
        settings.turns,
        settings.size,
        settings.hold_s,
        settings.opacity,
        settings.show_you,
    );

    // No Wayland at all is the same answer as no layer-shell — "not here, use
    // the window" — and it deserves the same exit code, so the Electron side
    // falls straight back instead of counting it as a crash and retrying.
    let Ok(conn) = Connection::connect_to_env() else {
        eprintln!(
            "[overlay] no Wayland display (WAYLAND_DISPLAY is unset or the socket is gone); \
             falling back to the captions window."
        );
        std::process::exit(NO_LAYER_SHELL);
    };
    let (globals, mut queue) = registry_queue_init::<App>(&conn)?;
    let qh = queue.handle();

    let compositor = CompositorState::bind(&globals, &qh)
        .map_err(|e| anyhow::anyhow!("this compositor offers no wl_compositor: {e}"))?;
    let shm = Shm::bind(&globals, &qh)
        .map_err(|e| anyhow::anyhow!("this compositor offers no wl_shm: {e}"))?;
    let Ok(layer_shell) = LayerShell::bind(&globals, &qh) else {
        // The one line, and the exit code the Electron side reads.
        eprintln!(
            "[overlay] this compositor does not offer zwlr_layer_shell_v1; \
             falling back to the captions window."
        );
        std::process::exit(NO_LAYER_SHELL);
    };
    eprintln!("[overlay] zwlr_layer_shell_v1: present");

    let renderer = raster::Renderer::new(raster::Style::default(), opts.font.as_deref())?;
    let pool = SlotPool::new(1024 * 340 * 4, &shm).context("could not make an shm pool")?;

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        compositor,
        shm,
        pool,
        layer: None,
        cfg: None,
        scale: 1,
        configured: false,
        exit: false,
        dirty: true,
        renderer,
        settings,
        turns: Vec::new(),
        last_change: Instant::now(),
        faded_out: false,
        want_output: opts.output.clone(),
        margin_override: opts.margin,
        frame_us: 0,
        frames: 0,
    };

    // One roundtrip so the outputs — and their sizes and scales — are known
    // before the surface is created. A layer surface has to state its size at
    // creation, and guessing it means a bar that resizes itself in front of the
    // person the moment it appears.
    queue.roundtrip(&mut app)?;
    app.create_layer(&layer_shell, &qh);
    queue.roundtrip(&mut app)?;

    // The feed on its own thread, handing over turn lists. A socket read must
    // never sit between a configure and a commit.
    let (tx, rx) = std::sync::mpsc::channel::<Vec<Turn>>();
    let wake = Wake::new()?;
    let socket = opts.socket.clone().unwrap_or_else(feed::default_socket);
    {
        let waker = wake.writer();
        std::thread::spawn(move || {
            if let Err(e) = pump(&socket, &tx, &waker) {
                eprintln!("[overlay] captions feed stopped: {e}");
                waker.wake();
            }
        });
    }

    let watch = SettingsWatch::new(&settings_path);
    let deadline = opts
        .seconds
        .map(|s| Instant::now() + Duration::from_secs_f32(s));

    loop {
        queue.dispatch_pending(&mut app)?;
        if app.exit {
            break;
        }
        if app.dirty {
            app.draw(&qh);
        }
        queue.flush()?;

        if deadline.is_some_and(|d| Instant::now() >= d) {
            eprintln!("[overlay] the time asked for is up; leaving.");
            break;
        }

        // Animating means the stack is inside its one second of fading and the
        // next frame is a different picture. Otherwise nothing has to happen
        // until somebody speaks or the settings file is written, and a caption
        // bar has no business waking a laptop up sixty times a second to draw
        // the same pixels.
        let timeout = if app.animating() { 16 } else { 500 };

        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        let wl_fd = guard.connection_fd().as_raw_fd();
        let mut fds = [
            pollfd(wl_fd),
            pollfd(wake.read_fd()),
            pollfd(watch.fd().unwrap_or(-1)),
        ];
        // Safety: three owned fds, a plain count, and a millisecond timeout.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                drop(guard);
                continue;
            }
            drop(guard);
            bail!("poll: {err}");
        }
        if fds[0].revents != 0 {
            guard.read()?;
        } else {
            drop(guard);
        }
        if fds[1].revents != 0 {
            wake.drain();
        }
        if fds[2].revents != 0 && watch.drain() {
            app.reload_settings(&settings_path);
        }

        // Whatever the feed thread has produced since the last pass, newest
        // wins: an intermediate stack nobody saw is not worth a frame.
        let mut latest = None;
        loop {
            match rx.try_recv() {
                Ok(turns) => latest = Some(turns),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        if let Some(turns) = latest {
            app.set_turns(turns);
        }
        // The fade is a function of the clock, so the frame it asks for has to
        // be asked for by the clock too.
        if app.animating() || app.should_clear() {
            app.dirty = true;
        }
    }
    Ok(())
}

/// Connect, subscribe, and hand over the whole visible stack on every change.
///
/// Same rules as the desktop captions window, enforced in `feed.rs`: seed from
/// the live tail so the archive rule has a head to measure against, and only
/// `added` rows newer than that head become captions.
fn pump(socket: &Path, tx: &Sender<Vec<Turn>>, wake: &Waker) -> Result<()> {
    let mut f = feed::Feed::connect(socket)?;
    let mut caps = Captions::new(KEEP);
    if let Ok(list) = f.call("speakers.list", json!({})) {
        caps.learn_speakers(&list);
    }
    if let Ok(mic) = f.call("mic.get", json!({})) {
        caps.learn_you(&mic);
    }
    if let Ok(tail) = f.call("transcript", json!({"limit": 1})) {
        caps.seed_tail(&tail);
    }
    f.call("subscribe", json!({"topics": ["segments", "relabel"]}))?;
    eprintln!(
        "[overlay] subscribed; the bar shows the live feed and nothing else (your voice: {})",
        caps.you()
            .map(|id| id.to_string())
            .unwrap_or_else(|| "not known yet".into()),
    );
    loop {
        let msg = f.read()?;
        let changed = match msg["ev"].as_str() {
            Some("segment") => caps.apply(&msg["data"]),
            Some("relabel") => {
                caps.apply_relabel(&msg["data"]);
                true
            }
            _ => false,
        };
        if !changed {
            continue;
        }
        // The whole ring, cut to size by the DRAW side: `turns` and `showYou`
        // change while this thread is blocked on a socket read, and a stack cut
        // to the old numbers here would not come back until somebody spoke.
        if tx.send(caps.turns().cloned().collect()).is_err() {
            return Ok(());
        }
        wake.wake();
    }
}

// ---------------------------------------------------------------------------
// the client
// ---------------------------------------------------------------------------

struct App {
    registry_state: RegistryState,
    output_state: OutputState,
    compositor: CompositorState,
    shm: Shm,
    pool: SlotPool,
    layer: Option<LayerSurface>,
    cfg: Option<LayerConfig>,
    scale: i32,
    configured: bool,
    exit: bool,
    dirty: bool,
    renderer: raster::Renderer,
    settings: CaptionSettings,
    turns: Vec<Turn>,
    last_change: Instant,
    faded_out: bool,
    want_output: Option<String>,
    margin_override: Option<i32>,
    frame_us: u128,
    frames: u64,
}

impl App {
    /// The output this bar goes on, and its logical size and scale.
    ///
    /// **Not** "the one under the cursor": a caption bar that moved between
    /// monitors when you reached for a menu would be a caption bar you had to
    /// chase. `--output NAME` names one, and with no name the compositor is
    /// asked to place it, which on KWin is the active output at the moment the
    /// surface appears. Documented in docs/OVERLAY.md.
    fn pick_output(&self) -> (Option<wl_output::WlOutput>, (u32, u32), i32) {
        let mut fallback = None;
        for out in self.output_state.outputs() {
            let Some(info) = self.output_state.info(&out) else {
                continue;
            };
            let size = info
                .logical_size
                .or_else(|| info.modes.iter().find(|m| m.current).map(|m| m.dimensions))
                .map(|(w, h)| (w.max(1) as u32, h.max(1) as u32))
                .unwrap_or((1920, 1080));
            let scale = info.scale_factor.max(1);
            match &self.want_output {
                Some(name) if info.name.as_deref() == Some(name.as_str()) => {
                    return (Some(out), size, scale);
                }
                Some(_) => {}
                None => return (None, size, scale),
            }
            fallback.get_or_insert((size, scale));
        }
        if let Some(name) = &self.want_output {
            eprintln!("[overlay] no output called {name}; letting the compositor choose");
        }
        let (size, scale) = fallback.unwrap_or(((1920, 1080), 1));
        (None, size, scale)
    }

    fn create_layer(&mut self, shell: &LayerShell, qh: &QueueHandle<Self>) {
        let (output, size, scale) = self.pick_output();
        let cfg = layer_config(&self.settings, size, self.margin_override);
        self.scale = scale;

        let surface = self.compositor.create_surface(qh);
        let layer = shell.create_layer_surface(
            qh,
            surface,
            cfg.layer,
            Some("nx-recall-captions"),
            output.as_ref(),
        );
        layer.set_anchor(cfg.anchor);
        layer.set_size(cfg.width, cfg.height);
        let (t, r, b, l) = cfg.margin;
        layer.set_margin(t, r, b, l);
        layer.set_keyboard_interactivity(cfg.keyboard);
        layer.set_exclusive_zone(cfg.exclusive_zone);
        apply_input_region(&self.compositor, qh, layer.wl_surface(), &cfg.input);
        layer.wl_surface().set_buffer_scale(scale);
        layer.commit();

        eprintln!(
            "[overlay] layer surface: {}x{} logical at scale {scale}, layer OVERLAY, \
             anchor BOTTOM, margin {b}px, exclusive zone {}, keyboard none",
            cfg.width, cfg.height, cfg.exclusive_zone,
        );
        self.cfg = Some(cfg);
        self.layer = Some(layer);
    }

    fn set_turns(&mut self, turns: Vec<Turn>) {
        self.turns = turns;
        self.last_change = Instant::now();
        self.faded_out = false;
        self.dirty = true;
    }

    fn fade(&self) -> f32 {
        if self.turns.is_empty() {
            return 0.0;
        }
        layout::fade_at(
            self.last_change.elapsed().as_secs_f32(),
            self.settings.hold_s,
        )
    }

    fn animating(&self) -> bool {
        let f = self.fade();
        f > 0.0 && f < 1.0
    }

    /// The stack has just finished fading and the surface is still showing it.
    fn should_clear(&self) -> bool {
        self.fade() <= 0.0 && !self.faded_out && !self.turns.is_empty()
    }

    fn reload_settings(&mut self, path: &Path) {
        let next = CaptionSettings::load(path);
        if next == self.settings {
            return;
        }
        eprintln!(
            "[overlay] captions.json changed — turns {}, size {}, hold {}s, ground {:.2}, showYou {}",
            next.turns, next.size, next.hold_s, next.opacity, next.show_you
        );
        // The bar's SIZE is layer-shell state and has to be re-sent; everything
        // else only changes the next frame. `clickThrough` is read and does
        // nothing here, which is the truth this whole path exists to tell.
        if let (Some(layer), Some(cfg)) = (self.layer.as_ref(), self.cfg.as_ref()) {
            let out = (cfg.width, cfg.height);
            let next_cfg = layer_config(&next, out, self.margin_override);
            if next_cfg.width != cfg.width || next_cfg.height != cfg.height {
                layer.set_size(next_cfg.width, next_cfg.height);
                layer.commit();
            }
        }
        self.settings = next;
        self.dirty = true;
    }

    fn draw(&mut self, qh: &QueueHandle<Self>) {
        self.dirty = false;
        let Some((cw, ch)) = self.cfg.as_ref().map(|c| (c.width, c.height)) else {
            return;
        };
        if self.layer.is_none() || !self.configured {
            return;
        }
        let started = Instant::now();
        let scale = self.scale.max(1) as u32;
        let (bw, bh) = (cw * scale, ch * scale);
        let stride = bw as i32 * 4;
        let len = (stride as usize) * (bh as usize);

        // Everything that reads `self` happens BEFORE the pool hands out a
        // mutable slice: the pixels are made first, the buffer is filled after.
        let fade = self.fade();
        let pixels = (fade > 0.0).then(|| {
            // The rasteriser draws at BUFFER resolution, not logical: a caption
            // on a 2x output rendered at 1x and scaled up by the compositor is a
            // blurry caption, and the whole surface is text.
            self.renderer.set_style(raster::Style {
                width: bw,
                height: bh,
                size: self.settings.size * scale as f32,
                opacity: self.settings.opacity,
                pad: PAD * scale as i64,
            });
            let shown = visible(&self.turns, self.settings.turns, self.settings.show_you);
            self.renderer.render(&shown, None)
        });

        if self.pool.len() < len {
            let _ = self.pool.resize(len);
        }
        let Ok((buffer, canvas)) =
            self.pool
                .create_buffer(bw as i32, bh as i32, stride, wl_shm::Format::Argb8888)
        else {
            eprintln!("[overlay] could not get an shm buffer for {bw}x{bh}");
            return;
        };
        match &pixels {
            Some(surface) => to_argb8888(surface, canvas, fade),
            // Nothing at all rather than a dark slab: a faded-out caption bar
            // must be a hole in the screen, not a rectangle over the game.
            None => canvas.fill(0),
        }
        if pixels.is_none() {
            self.faded_out = true;
        }

        let layer = self.layer.as_ref().expect("checked above");
        let wl = layer.wl_surface();
        wl.set_buffer_scale(self.scale.max(1));
        wl.damage_buffer(0, 0, bw as i32, bh as i32);
        wl.frame(qh, FrameCallbackData(wl.clone()));
        if buffer.attach_to(wl).is_err() {
            eprintln!("[overlay] could not attach the buffer");
            return;
        }
        layer.commit();

        // The number that decides whether this can stay a CPU rasteriser. The
        // budget is one 60 Hz frame, 16.6 ms; docs/OVERLAY.md records what it
        // actually measured. Logged for the first frame and then rarely, because
        // a line per frame during a fade is a log nobody can read.
        self.frame_us = started.elapsed().as_micros();
        self.frames += 1;
        if self.frames == 1 || self.frames.is_multiple_of(120) {
            eprintln!(
                "[overlay] frame {} — {}x{} px rasterised and converted in {:.2} ms",
                self.frames,
                bw,
                bh,
                self.frame_us as f64 / 1000.0,
            );
        }
    }
}

/// Straight-alpha RGBA to the premultiplied little-endian ARGB8888 that
/// `wl_shm` means by `Argb8888` — which is B, G, R, A in memory order — with
/// the stack's fade folded in on the way.
///
/// Premultiplying is not optional: a Wayland compositor reads the ARGB formats
/// as premultiplied, and handing it straight alpha paints a bright halo around
/// every glyph over a dark game.
fn to_argb8888(surface: &raster::Surface, out: &mut [u8], fade: f32) {
    let fade = fade.clamp(0.0, 1.0);
    for (src, dst) in surface
        .pixels
        .as_chunks::<4>()
        .0
        .iter()
        .zip(out.as_chunks_mut::<4>().0.iter_mut())
    {
        let a = (src[3] as f32 / 255.0) * fade;
        dst[0] = (src[2] as f32 * a).round() as u8;
        dst[1] = (src[1] as f32 * a).round() as u8;
        dst[2] = (src[0] as f32 * a).round() as u8;
        dst[3] = (a * 255.0).round() as u8;
    }
}

/// The load-bearing request. An empty `wl_region` is not "the client ignores
/// clicks" — it is the compositor being told there is nowhere on this surface
/// to deliver a pointer or a touch to, which is why it holds for a game that
/// has grabbed the pointer, for touch, and for a client that is busy.
fn apply_input_region(
    compositor: &CompositorState,
    qh: &QueueHandle<App>,
    surface: &wl_surface::WlSurface,
    spec: &InputRegion,
) {
    match spec {
        InputRegion::Empty => {
            let _ = qh;
            match Region::new(compositor) {
                Ok(region) => {
                    surface.set_input_region(Some(region.wl_region()));
                    eprintln!(
                        "[overlay] wl_surface.set_input_region: empty region (0 rectangles) — \
                         the compositor will deliver no pointer, touch or keyboard here"
                    );
                }
                // Said loudly rather than swallowed. A caption bar that quietly
                // became clickable is the bug this whole path exists to fix, and
                // the person needs to know before it is over their game.
                Err(e) => eprintln!(
                    "[overlay] WARNING: could not create an input region ({e}); \
                     this surface may take clicks"
                ),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// wayland handlers
// ---------------------------------------------------------------------------

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        if new_factor.max(1) != self.scale {
            self.scale = new_factor.max(1);
            self.dirty = true;
        }
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wayland_client::protocol::wl_output::Transform,
    ) {
    }

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {
        // The compositor is ready for another. Whether there is another to give
        // is the fade's business, not this callback's.
    }

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        eprintln!("[overlay] the compositor closed the layer surface");
        self.exit = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let (w, h) = configure.new_size;
        if let Some(cfg) = self.cfg.as_mut() {
            // A zero here means "you choose", which we already did.
            if w > 0 {
                cfg.width = w;
            }
            if h > 0 {
                cfg.height = h;
            }
        }
        self.configured = true;
        self.dirty = true;
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

// One blanket impl rather than a macro per protocol: this smithay-client-toolkit
// routes every event through the object's own user data (`Dispatch2`), which is
// the same shape nx-wisp's layer surface uses on this KWin.
delegate_dispatch2!(App);
delegate_registry!(App);

// ---------------------------------------------------------------------------
// waking up
// ---------------------------------------------------------------------------

fn pollfd(fd: i32) -> libc::pollfd {
    libc::pollfd {
        fd,
        events: if fd < 0 { 0 } else { libc::POLLIN },
        revents: 0,
    }
}

/// A pipe the feed thread writes one byte to, so the draw loop can sleep on
/// `poll` instead of spinning. A channel alone cannot be polled beside a
/// Wayland fd, and a 60 Hz wake-up for a bar that changes twice a minute is a
/// laptop battery spent on nothing.
struct Wake {
    read: i32,
    write: i32,
}

#[derive(Clone, Copy)]
struct Waker {
    fd: i32,
}

impl Waker {
    fn wake(&self) {
        // Safety: one byte into a pipe we own; a full pipe is already a wake-up.
        unsafe { libc::write(self.fd, [1u8].as_ptr() as *const libc::c_void, 1) };
    }
}

impl Wake {
    fn new() -> Result<Self> {
        let mut fds = [0i32; 2];
        // Safety: a two-element array, as pipe2 requires.
        let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
        if rc != 0 {
            bail!("pipe2: {}", std::io::Error::last_os_error());
        }
        Ok(Self {
            read: fds[0],
            write: fds[1],
        })
    }
    fn writer(&self) -> Waker {
        Waker { fd: self.write }
    }
    fn read_fd(&self) -> i32 {
        self.read
    }
    fn drain(&self) {
        let mut buf = [0u8; 64];
        // Safety: a non-blocking read into a stack buffer; short reads are fine.
        while unsafe { libc::read(self.read, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) } > 0
        {
        }
    }
}

impl Drop for Wake {
    fn drop(&mut self) {
        // Safety: two fds this struct owns and nothing else holds.
        unsafe {
            libc::close(self.read);
            libc::close(self.write);
        }
    }
}

/// An inotify watch on the DIRECTORY captions.json lives in, not on the file.
///
/// A watch on the file itself survives exactly one atomic write: the moment
/// anything replaces the inode, the watch is pointing at a file nobody has any
/// more. Watching the directory catches both the in-place write Electron does
/// today and the rename a future one might.
struct SettingsWatch {
    fd: Option<i32>,
}

impl SettingsWatch {
    fn new(path: &Path) -> Self {
        let Some(dir) = path.parent() else {
            return Self { fd: None };
        };
        let Ok(c) = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()) else {
            return Self { fd: None };
        };
        // Safety: inotify_init1 with two documented flags.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            eprintln!("[overlay] no inotify; the settings card will not reach the bar live");
            return Self { fd: None };
        }
        // Safety: an owned fd and a NUL-terminated path that outlives the call.
        let wd = unsafe {
            libc::inotify_add_watch(
                fd,
                c.as_ptr(),
                libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO | libc::IN_CREATE,
            )
        };
        if wd < 0 {
            eprintln!("[overlay] could not watch {}", dir.display());
            // Safety: our own fd.
            unsafe { libc::close(fd) };
            return Self { fd: None };
        }
        eprintln!("[overlay] watching {} for settings changes", dir.display());
        Self { fd: Some(fd) }
    }

    fn fd(&self) -> Option<i32> {
        self.fd
    }

    /// Empty the queue and say whether anything happened. The events are not
    /// parsed: the answer to "did captions.json change" is re-reading it and
    /// comparing, which `reload_settings` does anyway, and a userData directory
    /// full of Chromium's own files makes name-matching the fragile half.
    fn drain(&self) -> bool {
        let Some(fd) = self.fd else { return false };
        let mut buf = [0u8; 4096];
        let mut any = false;
        // Safety: a non-blocking read into a stack buffer sized well past one
        // inotify_event plus a name.
        while unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) } > 0 {
            any = true;
        }
        any
    }
}

impl Drop for SettingsWatch {
    fn drop(&mut self) {
        if let Some(fd) = self.fd {
            // Safety: our own fd.
            unsafe { libc::close(fd) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Bounds;

    /// The point of the whole module, as an assertion rather than a hope: there
    /// is no settings file, and no value of the click-through toggle, that puts
    /// a rectangle in this surface's input region.
    #[test]
    fn the_input_region_is_empty_whatever_the_settings_say() {
        for click_through in [true, false] {
            let s = CaptionSettings {
                click_through,
                ..CaptionSettings::default()
            };
            assert_eq!(input_region(&s), InputRegion::Empty);
            assert_eq!(
                layer_config(&s, (2560, 1440), None).input,
                InputRegion::Empty
            );
        }
    }

    /// The four facts that make this a caption bar rather than a window: above
    /// a fullscreen game, anchored at the bottom, reserving nothing, and deaf.
    #[test]
    fn the_surface_is_scenery_at_the_bottom_of_the_screen() {
        let cfg = layer_config(&CaptionSettings::default(), (2560, 1440), None);
        assert_eq!(
            cfg.layer,
            Layer::Overlay,
            "TOP loses to a fullscreen window"
        );
        assert_eq!(cfg.anchor, Anchor::BOTTOM);
        assert_eq!(
            cfg.exclusive_zone, -1,
            "a caption bar must reserve no space"
        );
        assert_eq!(cfg.keyboard, KeyboardInteractivity::None);
        assert_eq!((cfg.width, cfg.height), (1100, 340));
        assert_eq!(cfg.margin, (0, 0, layout::DEFAULT_BOTTOM_MARGIN, 0));
    }

    #[test]
    fn the_remembered_bar_and_an_explicit_margin_are_both_honoured() {
        let s = CaptionSettings {
            bounds: Some(Bounds {
                x: 100,
                y: 700,
                width: 800,
                height: 260,
            }),
            ..CaptionSettings::default()
        };
        let cfg = layer_config(&s, (1920, 1080), None);
        assert_eq!((cfg.width, cfg.height), (800, 260));
        assert_eq!(cfg.margin, (0, 0, 1080 - 700 - 260, 0));
        // …and --margin overrules the remembered position entirely.
        assert_eq!(
            layer_config(&s, (1920, 1080), Some(12)).margin,
            (0, 0, 12, 0)
        );
    }

    /// Premultiplied, in `wl_shm`'s byte order, with the fade folded in. Getting
    /// either wrong is a bright halo around every glyph over a dark game.
    #[test]
    fn the_buffer_is_premultiplied_bgra_and_the_fade_is_in_it() {
        let s = raster::Surface {
            width: 1,
            height: 1,
            // R=200 G=100 B=50 at half alpha, straight.
            pixels: vec![200, 100, 50, 128],
        };
        let mut out = [0u8; 4];
        to_argb8888(&s, &mut out, 1.0);
        let a = 128.0f32 / 255.0;
        assert_eq!(out[3], 128);
        assert_eq!(out[0], (50.0 * a).round() as u8, "blue is not first");
        assert_eq!(out[1], (100.0 * a).round() as u8);
        assert_eq!(out[2], (200.0 * a).round() as u8, "red is not third");

        // Half faded: alpha halves and so does every premultiplied channel.
        let mut half = [0u8; 4];
        to_argb8888(&s, &mut half, 0.5);
        assert_eq!(half[3], 64);
        assert!(half[2] < out[2]);

        // Gone: nothing at all, so the surface is a hole rather than a slab.
        let mut gone = [0u8; 4];
        to_argb8888(&s, &mut gone, 0.0);
        assert_eq!(gone, [0, 0, 0, 0]);
    }
}
