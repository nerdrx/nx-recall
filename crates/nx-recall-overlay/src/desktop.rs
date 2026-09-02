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
//! - Anchored to the BOTTOM-LEFT corner, sized and placed from the same
//!   `captions.json` the Electron window is sized from, with an exclusive zone
//!   of −1 so no maximised window ever gets shoved up to make room for scenery.
//! - `keyboard_interactivity: none`, always. The bar is read, never typed into.
//! - `wl_shm` and the CPU rasteriser in `raster.rs`. No GPU, no wgpu, no Vulkan
//!   — see `frame time` in docs/OVERLAY.md for what that costs.
//!
//! ## Scenery by default, furniture on request
//!
//! 0.10.0 shipped the input region as always-empty, and that was one word too
//! strong. Click-through is what a caption bar wants nearly always — but the
//! first thing a person does with a new bar is try to move it, and an always-
//! empty region means they cannot, from either side. So `clickThrough` in
//! captions.json decides, live:
//!
//! - **on** (the default): an empty input region. Unchanged, and still the
//!   thing Electron could not say.
//! - **off**: a NULL — i.e. infinite — input region, and the bar becomes
//!   furniture. **Left-drag** moves it, and the new position is written back
//!   into `bounds` so it stays put across launches. **Scroll** changes `size`,
//!   in the settings card's own steps. **Right-click** puts `clickThrough` back
//!   on, which is the way out that does not require finding the Settings view
//!   underneath a bar that is currently eating your clicks.
//!
//! While it is furniture the stack does not fade: you cannot grab what you
//! cannot see, and a bar that vanishes twelve seconds into being moved is a bar
//! that cannot be moved.
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
    seat::{
        Capability, SeatHandler, SeatState,
        pointer::{
            CursorIcon, PointerEvent, PointerEventKind, PointerHandler, ThemeSpec, ThemedPointer,
        },
    },
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
    protocol::{wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
};

use crate::feed::{self, Captions, TranslationDisplay, Turn, visible};
use crate::layout::{self, DragStep, Screen};
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

/// `linux/input-event-codes.h`. Wayland reports raw evdev button codes and does
/// not name them; naming them here is the difference between this file and one
/// full of `0x110`.
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;

/// The `size` slider's step, from CAPTION_RANGES in
/// gui/src/renderer/lib/captions.js. One notch of the wheel is one notch of the
/// slider, so the two controls cannot disagree about what a step is.
const SIZE_STEP: f32 = 1.0;
const SIZE_MIN: f32 = 18.0;
const SIZE_MAX: f32 = 40.0;

/// How long after the last interaction the file is written. The Electron side
/// debounces its own saves by 400 ms for the same reason: a wheel spin is thirty
/// events and thirty rewrites of a file somebody is watching.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(400);

/// One notch of the wheel, in the slider's own steps and the slider's own range.
///
/// Scrolling UP makes the text bigger, which is the direction every other
/// zoom on this desktop goes; Wayland's vertical axis is positive DOWNWARD, so
/// the sign flips exactly once, here.
pub fn size_after_scroll(size: f32, notches: f32) -> f32 {
    let stepped = size - notches * SIZE_STEP;
    stepped.clamp(SIZE_MIN, SIZE_MAX).round()
}

// ---------------------------------------------------------------------------
// the two things that can be decided without a compositor, and therefore tested
// ---------------------------------------------------------------------------

/// What this surface asks the compositor to deliver to it.
///
/// 0.10.0 shipped this with one variant and a test asserting that nothing could
/// produce another: on the layer path the bar was scenery, always. That was
/// half right and one word too strong. Click-through is what a caption bar
/// wants almost all of the time — and "almost all" is a DEFAULT, not a law. The
/// first thing a person did with it was try to move the bar, and there was no
/// way to, from either side: the toggle was hidden, and the surface would not
/// have listened if it had been there.
///
/// So the setting decides, and the enum still exists for the same reason: this
/// is the one decision in the module worth being able to check without a
/// compositor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputRegion {
    /// `wl_surface.set_input_region` with a region that has no rectangles in
    /// it. Not "the client ignores clicks" — the compositor never sends any.
    Empty,
    /// A NULL region, which the protocol defines as infinite: the whole surface
    /// takes the pointer. Drag to move, scroll to resize, right-click to hand
    /// the pointer back.
    Full,
}

pub fn input_region(settings: &CaptionSettings) -> InputRegion {
    if settings.click_through {
        InputRegion::Empty
    } else {
        InputRegion::Full
    }
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
    origin: (i32, i32),
    margin_override: Option<i32>,
) -> LayerConfig {
    let (width, height) = layout::surface_size(settings.bounds, output);
    let bottom = margin_override
        .unwrap_or_else(|| layout::bottom_margin(settings.bounds, height, output, origin));
    let left = layout::left_margin(settings.bounds, width, output, origin);
    LayerConfig {
        // OVERLAY and not TOP: TOP loses to a fullscreen window, and a
        // fullscreen window is the thing these captions are for.
        layer: Layer::Overlay,
        // BOTTOM | LEFT, not BOTTOM alone. Bottom alone centres a fixed-width
        // surface for free, which was the right trade while the bar could not be
        // moved — but "centred" is a position with no number in it, and a bar
        // you can drag needs one. Two edges, and both margins are ours to set.
        // (Anchoring to BOTH sides of an axis is what stretches a surface;
        // anchoring to one corner does not.)
        anchor: Anchor::BOTTOM.union(Anchor::LEFT),
        width,
        height,
        margin: (0, 0, bottom, left),
        // Still none, and deliberately: the bar is read, never typed into, and
        // a layer surface that took the keyboard would take it from the game.
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

/// `--list-outputs`: the desk, as JSON, and nothing else.
///
/// The settings card needs a list of screens to offer, and the card has no
/// Wayland access — it is a page in an Electron renderer. Two ways to get it
/// there were on the table; this is the one that keeps `captions.json` a
/// SETTINGS file. The alternative was for the overlay to write an `outputs`
/// block into it on every start, and that would mean a file the settings card
/// owns being rewritten with hardware state on each launch, tripping both
/// sides' echo guards, churning a file people diff, and putting a cache in the
/// same object as the things a person chose. A one-shot subprocess run when
/// the card is opened costs a few milliseconds and owns nothing.
///
/// Exits with the same code as `--desktop` when there is no layer-shell, so the
/// caller can tell "this desktop cannot" from "this desk has one screen".
pub fn list_outputs() -> Result<()> {
    let Ok(conn) = Connection::connect_to_env() else {
        eprintln!("[overlay] no Wayland display; there are no outputs to list.");
        std::process::exit(NO_LAYER_SHELL);
    };
    let (globals, mut queue) = registry_queue_init::<App>(&conn)?;
    let qh = queue.handle();
    let compositor = CompositorState::bind(&globals, &qh)
        .map_err(|e| anyhow::anyhow!("this compositor offers no wl_compositor: {e}"))?;
    let shm = Shm::bind(&globals, &qh)
        .map_err(|e| anyhow::anyhow!("this compositor offers no wl_shm: {e}"))?;
    if LayerShell::bind(&globals, &qh).is_err() {
        eprintln!("[overlay] this compositor does not offer zwlr_layer_shell_v1.");
        std::process::exit(NO_LAYER_SHELL);
    }
    let pool = SlotPool::new(4, &shm).context("could not make an shm pool")?;
    let mut app = App {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        seat_state: SeatState::new(&globals, &qh),
        compositor,
        shm,
        pool,
        layer: None,
        cfg: None,
        screens: Vec::new(),
        current: 0,
        shell: None,
        out_size: (1920, 1080),
        out_origin: (0, 0),
        scale: 1,
        configured: false,
        exit: false,
        dirty: false,
        renderer: raster::Renderer::new(raster::Style::default(), None)?,
        settings: CaptionSettings::default(),
        settings_path: PathBuf::new(),
        last_written: None,
        save_at: None,
        pointer: None,
        drag: None,
        turns: Vec::new(),
        translation_display: TranslationDisplay::default(),
        last_change: Instant::now(),
        faded_out: false,
        want_output: None,
        margin_override: None,
        frame_us: 0,
        frames: 0,
    };
    // Two: the first brings the outputs, the second their xdg-output geometry.
    queue.roundtrip(&mut app)?;
    queue.roundtrip(&mut app)?;
    let screens: Vec<serde_json::Value> = app
        .read_screens()
        .iter()
        .map(|s| {
            json!({
                "name": s.name,
                "x": s.origin.0,
                "y": s.origin.1,
                "w": s.size.0,
                "h": s.size.1,
                "scale": s.scale,
            })
        })
        .collect();
    println!("{}", serde_json::to_string(&json!({"outputs": screens}))?);
    Ok(())
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
        seat_state: SeatState::new(&globals, &qh),
        compositor,
        shm,
        pool,
        layer: None,
        cfg: None,
        screens: Vec::new(),
        current: 0,
        shell: None,
        out_size: (1920, 1080),
        out_origin: (0, 0),
        scale: 1,
        configured: false,
        exit: false,
        dirty: true,
        renderer,
        settings,
        settings_path: settings_path.clone(),
        last_written: None,
        save_at: None,
        pointer: None,
        drag: None,
        turns: Vec::new(),
        translation_display: TranslationDisplay::default(),
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
    app.shell = Some(layer_shell);
    app.create_layer(&qh);
    queue.roundtrip(&mut app)?;

    // The feed on its own thread, handing over turn lists. A socket read must
    // never sit between a configure and a commit.
    let (tx, rx) = std::sync::mpsc::channel::<Update>();
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
        // …and a pending write is its own reason to come back: the debounce
        // has to expire even on a desktop where nothing else is happening.
        let timeout = if app.animating() {
            16
        } else if app.save_at.is_some() {
            SAVE_DEBOUNCE.as_millis() as i32 / 4
        } else {
            500
        };

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
            app.reload_settings(&settings_path, &qh);
        }
        app.flush_save();

        // Whatever the feed thread has produced since the last pass, newest
        // wins: an intermediate stack nobody saw is not worth a frame.
        let mut latest = None;
        loop {
            match rx.try_recv() {
                Ok(update) => latest = Some(update),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        if let Some(update) = latest {
            app.set_turns(update);
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
fn pump(socket: &Path, tx: &Sender<Update>, wake: &Waker) -> Result<()> {
    let mut f = feed::Feed::connect(socket)?;
    let mut caps = Captions::new(KEEP);
    let mut display = TranslationDisplay::default();
    if let Ok(list) = f.call("speakers.list", json!({})) {
        caps.learn_speakers(&list);
    }
    if let Ok(mic) = f.call("mic.get", json!({})) {
        caps.learn_you(&mic);
    }
    // Which of a translated row's two lines leads. On `status` rather than
    // `assist.get` because `status` is the call every client already makes and
    // carries the three `[assist]` values (PROTOCOL, 0.10.2) — and because a
    // daemon too old for either answers `unknown_method`, which is not a
    // failure here, only "the default, then".
    if let Ok(status) = f.call("status", json!({})) {
        display = TranslationDisplay::from_envelope(&status, display);
    }
    if let Ok(tail) = f.call("transcript", json!({"limit": 1})) {
        caps.seed_tail(&tail);
    }
    // `status` is on the list for the `assist` event, which rides that topic
    // rather than getting one of its own (PROTOCOL: "No new topic, so no client
    // changes its subscription"). This one does change its subscription,
    // because until 0.10.2 it had no reason to care what the transcript's
    // layout was.
    f.call(
        "subscribe",
        json!({"topics": ["segments", "relabel", "status"]}),
    )?;
    eprintln!(
        "[overlay] subscribed; the bar shows the live feed and nothing else \
         (your voice: {}, translations {})",
        caps.you()
            .map(|id| id.to_string())
            .unwrap_or_else(|| "not known yet".into()),
        match display {
            TranslationDisplay::Main => "lead, with the original underneath",
            TranslationDisplay::Under => "under the original",
        },
    );
    loop {
        let msg = f.read()?;
        let changed = match msg["ev"].as_str() {
            Some("segment") => caps.apply(&msg["data"]),
            Some("relabel") => {
                caps.apply_relabel(&msg["data"]);
                true
            }
            // The layout changed under us — from the Memory card, from another
            // window, from the config file. Every row on screen is repainted,
            // which is unusual for a settings event and is the point of it.
            Some("assist") => {
                let next = TranslationDisplay::from_envelope(&msg["data"], display);
                let moved = next != display;
                if moved {
                    eprintln!(
                        "[overlay] translation_display is now {}",
                        if next == TranslationDisplay::Main {
                            "main"
                        } else {
                            "under"
                        }
                    );
                }
                display = next;
                moved
            }
            _ => false,
        };
        if !changed {
            continue;
        }
        // The whole ring, cut to size by the DRAW side: `turns` and `showYou`
        // change while this thread is blocked on a socket read, and a stack cut
        // to the old numbers here would not come back until somebody spoke.
        // The layout travels WITH the turns rather than beside them, for the
        // same reason the "you" marker does: the draw side has no socket, and
        // two facts arriving out of order would be one repaint in the wrong
        // shape.
        let update = Update {
            turns: caps.turns().cloned().collect(),
            display,
        };
        if tx.send(update).is_err() {
            return Ok(());
        }
        wake.wake();
    }
}

/// What the feed thread hands the draw loop: the stack, and how a translated
/// row in it is laid out.
struct Update {
    turns: Vec<Turn>,
    display: TranslationDisplay,
}

// ---------------------------------------------------------------------------
// the client
// ---------------------------------------------------------------------------

struct App {
    registry_state: RegistryState,
    output_state: OutputState,
    seat_state: SeatState,
    compositor: CompositorState,
    shm: Shm,
    pool: SlotPool,
    layer: Option<LayerSurface>,
    cfg: Option<LayerConfig>,
    /// Every output, in the global desktop coordinates `bounds` is written in.
    /// Rebuilt whenever the compositor tells us the layout changed, because a
    /// drag across a seam is arithmetic over ALL of them and not just the one
    /// the surface is on.
    screens: Vec<Screen>,
    /// Which of `screens` the surface currently lives on. A layer surface
    /// cannot change output, so moving between them means making a new one.
    current: usize,
    /// The shell, kept so the surface can be re-made on another output.
    shell: Option<LayerShell>,
    /// The output the bar is on, as (size, origin) in logical pixels. Cached at
    /// creation: the drag math needs both on every motion event, and asking the
    /// registry per frame for a fact that changes when somebody replugs a
    /// monitor is a round trip for nothing.
    out_size: (u32, u32),
    out_origin: (i32, i32),
    scale: i32,
    configured: bool,
    exit: bool,
    dirty: bool,
    renderer: raster::Renderer,
    settings: CaptionSettings,
    /// Where the settings live, so the pointer handlers can write back.
    settings_path: PathBuf,
    /// The exact text of the last write this process made. The inotify watch
    /// fires on our own writes too, and a reload that took its own echo as
    /// somebody else's change would fight a drag frame by frame.
    last_written: Option<String>,
    /// When to write. Set by a drag release or a scroll notch, cleared by the
    /// write; the debounce is the Electron side's own 400 ms.
    save_at: Option<Instant>,
    /// A pointer, if the seat has one. Themed so the cursor can say "grab" when
    /// the bar is grabbable — through `wp_cursor_shape_manager_v1` where the
    /// compositor offers it, which on this KWin it does.
    pointer: Option<ThemedPointer<(), ()>>,
    /// Where the left button went down, in surface-local coordinates, and the
    /// margins at that moment. `None` between drags.
    drag: Option<Drag>,
    turns: Vec<Turn>,
    /// `[assist] translation_display`, as the feed last heard it.
    translation_display: TranslationDisplay,
    last_change: Instant,
    faded_out: bool,
    want_output: Option<String>,
    margin_override: Option<i32>,
    frame_us: u128,
    frames: u64,
}

/// A drag in progress. `press` is the ORIGINAL press point and stays put: see
/// `layout::drag_margins` for why the loop is self-correcting rather than
/// accumulating.
#[derive(Debug, Clone, Copy)]
struct Drag {
    press: (f64, f64),
    from: (i32, i32),
    /// Whether the pointer has actually gone anywhere. A press-and-release that
    /// never moved is a click, not a move, and must not rewrite the file.
    moved: bool,
}

impl App {
    /// The output this bar goes on, and its logical size and scale.
    ///
    /// **Not** "the one under the cursor": a caption bar that moved between
    /// monitors when you reached for a menu would be a caption bar you had to
    /// chase. `--output NAME` names one, and with no name the compositor is
    /// asked to place it, which on KWin is the active output at the moment the
    /// surface appears. Documented in docs/OVERLAY.md.
    /// Every output the compositor is offering, in desk order as it lists them.
    ///
    /// The whole list, not just the chosen one: since 0.10.3 a drag can carry
    /// the bar off one screen and onto another, and deciding whether it has
    /// needs every screen's rectangle. `zxdg_output_manager_v1` is what makes
    /// them comparable — `wl_output` alone reports a mode in device pixels and
    /// no position, and a desk of two differently-scaled monitors cannot be
    /// laid out from that.
    fn read_screens(&self) -> Vec<Screen> {
        let mut out = Vec::new();
        for wl in self.output_state.outputs() {
            let Some(info) = self.output_state.info(&wl) else {
                continue;
            };
            let size = info
                .logical_size
                .or_else(|| info.modes.iter().find(|m| m.current).map(|m| m.dimensions))
                .map(|(w, h)| (w.max(1) as u32, h.max(1) as u32))
                .unwrap_or((1920, 1080));
            out.push(Screen {
                name: info
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("output-{}", out.len())),
                origin: info.logical_position.unwrap_or(info.location),
                size,
                scale: info.scale_factor.max(1),
            });
        }
        out
    }

    /// Re-read the desk, keeping `current` pointing at the same screen by NAME.
    /// Indices are positions in a list the compositor owns and they move when a
    /// monitor is unplugged; a name does not.
    fn refresh_screens(&mut self) {
        let was = self.screens.get(self.current).map(|s| s.name.clone());
        let next = self.read_screens();
        if next == self.screens {
            return;
        }
        self.screens = next;
        if let Some(name) = was
            && let Some(at) = layout::screen_named(&self.screens, &name)
        {
            self.current = at;
        }
        if let Some(here) = self.screens.get(self.current) {
            self.out_size = here.size;
            self.out_origin = here.origin;
        }
    }

    /// The `wl_output` a screen index names, for `get_layer_surface`.
    fn wl_output_for(&self, at: usize) -> Option<wl_output::WlOutput> {
        let name = self.screens.get(at)?.name.as_str();
        self.output_state.outputs().find(|wl| {
            self.output_state
                .info(wl)
                .and_then(|i| i.name)
                .is_some_and(|n| n == name)
        })
    }

    /// Which screen the bar belongs on, and why.
    ///
    /// The output is chosen EXPLICITLY and never left to the compositor:
    /// margins and `bounds` have to describe the same screen, and a placement
    /// computed for one output and honoured on another is a bar on the wrong
    /// monitor.
    ///
    /// In order: `--output`; the `output` remembered in captions.json — which
    /// is where a drag across a seam and the settings card's Screen selector
    /// both land; the screen the remembered position is on, for a file written
    /// before there was an `output` field; the screen at the desk's origin;
    /// whatever came first.
    fn pick_screen(&self) -> (usize, &'static str) {
        if self.screens.is_empty() {
            return (0, "there are no outputs");
        }
        if let Some(name) = self.want_output.as_deref() {
            if let Some(at) = layout::screen_named(&self.screens, name) {
                return (at, "--output");
            }
            eprintln!("[overlay] no output called {name}; choosing one");
        }
        if let Some(name) = self.settings.output.as_deref() {
            if let Some(at) = layout::screen_named(&self.screens, name) {
                return (at, "it is the screen captions.json remembers");
            }
            eprintln!("[overlay] the remembered screen {name} is not plugged in; choosing one");
        }
        if let Some(b) = self.settings.bounds {
            let centre = (b.x + b.width as i32 / 2, b.y + b.height as i32 / 2);
            if let Some(at) = layout::screen_at(&self.screens, centre) {
                return (at, "the remembered position is on it");
            }
        }
        if let Some(at) = self.screens.iter().position(|s| s.origin == (0, 0)) {
            return (at, "it is at the desktop origin");
        }
        (0, "it was the first one offered")
    }

    /// Make the surface, on the screen the rules pick and at the margins the
    /// settings describe.
    fn create_layer(&mut self, qh: &QueueHandle<Self>) {
        self.screens = self.read_screens();
        let (at, why) = self.pick_screen();
        self.current = at;
        let place = self.screens.get(at).cloned().unwrap_or(Screen {
            name: "?".into(),
            origin: (0, 0),
            size: (1920, 1080),
            scale: 1,
        });
        eprintln!(
            "[overlay] output {} ({}x{} at {},{}) — {why}; {} screen{} on this desk",
            place.name,
            place.size.0,
            place.size.1,
            place.origin.0,
            place.origin.1,
            self.screens.len(),
            if self.screens.len() == 1 { "" } else { "s" },
        );
        let cfg = layer_config(
            &self.settings,
            place.size,
            place.origin,
            self.margin_override,
        );
        self.build_surface(cfg, qh);
    }

    /// Re-make the surface on another screen, at the margins given.
    ///
    /// A layer surface belongs to one `wl_output` for its whole life — the
    /// protocol takes the output at creation and offers no way to change it —
    /// so crossing a seam is destroy-and-create, not a request. Which means the
    /// implicit pointer grab goes with it: **the drag ends at the hop.** The bar
    /// lands where the hand left it and the position is written down; picking it
    /// up again is one more click. Carrying the grab across would mean knowing
    /// the button is still down on a surface that has not sent a press, and
    /// wl_pointer reports transitions, not state.
    fn hop_to(&mut self, at: usize, margins: (i32, i32), qh: &QueueHandle<Self>) {
        let Some(place) = self.screens.get(at).cloned() else {
            return;
        };
        eprintln!(
            "[overlay] the bar crossed onto {} — re-making the surface there at {},{}",
            place.name, margins.0, margins.1
        );
        self.current = at;
        self.settings.output = Some(place.name.clone());
        // Dropping the old LayerSurface destroys the wl_surface with it, which
        // is what ends the drag; say so here rather than leaving a Drag that
        // will never see its release.
        self.drag = None;
        self.layer = None;
        self.configured = false;
        // Only the MARGIN is overridden. The size comes from `layer_config` as
        // it always does, so a hop that arrives alongside a size change — the
        // settings card can send both at once — does not carry the old shape
        // over, and a hop during a drag keeps the shape it had because the
        // remembered bounds still describe it.
        let cfg = LayerConfig {
            margin: (0, 0, margins.1, margins.0),
            ..layer_config(
                &self.settings,
                place.size,
                place.origin,
                self.margin_override,
            )
        };
        self.build_surface(cfg, qh);
        self.remember_position();
    }

    /// The requests themselves, shared by the first surface and every one after
    /// a hop, so the two cannot drift into describing different bars.
    fn build_surface(&mut self, cfg: LayerConfig, qh: &QueueHandle<Self>) {
        // Taken and put back rather than borrowed: `LayerShell` is a field of
        // self and everything below needs `&mut self`.
        let Some(shell) = self.shell.take() else {
            return;
        };
        let place = self.screens.get(self.current).cloned().unwrap_or(Screen {
            name: "?".into(),
            origin: (0, 0),
            size: (1920, 1080),
            scale: 1,
        });
        self.scale = place.scale;
        self.out_size = place.size;
        self.out_origin = place.origin;

        let surface = self.compositor.create_surface(qh);
        let layer = shell.create_layer_surface(
            qh,
            surface,
            cfg.layer,
            Some("nx-recall-captions"),
            self.wl_output_for(self.current).as_ref(),
        );
        layer.set_anchor(cfg.anchor);
        layer.set_size(cfg.width, cfg.height);
        let (t, r, b, l) = cfg.margin;
        layer.set_margin(t, r, b, l);
        layer.set_keyboard_interactivity(cfg.keyboard);
        layer.set_exclusive_zone(cfg.exclusive_zone);
        apply_input_region(&self.compositor, qh, layer.wl_surface(), &cfg.input);
        layer.wl_surface().set_buffer_scale(place.scale);
        layer.commit();

        eprintln!(
            "[overlay] layer surface on {}: {}x{} logical at scale {}, layer OVERLAY, \
             anchor BOTTOM|LEFT, margin {l}px from the left and {b}px from the bottom, \
             exclusive zone {}, keyboard none",
            place.name, cfg.width, cfg.height, place.scale, cfg.exclusive_zone,
        );
        self.cfg = Some(cfg);
        self.layer = Some(layer);
        self.shell = Some(shell);
        self.dirty = true;
    }

    fn set_turns(&mut self, update: Update) {
        self.turns = update.turns;
        self.translation_display = update.display;
        self.last_change = Instant::now();
        self.faded_out = false;
        self.dirty = true;
    }

    fn fade(&self) -> f32 {
        // While the bar is furniture it stays put and stays lit, even with
        // nothing said yet: you cannot grab what you cannot see, and a bar that
        // faded out mid-drag would be a bar that cannot be moved. This is the
        // one place `clickThrough` changes what is DRAWN rather than what is
        // delivered, and it is why turning it off is a mode rather than a tweak.
        if !self.settings.click_through {
            return 1.0;
        }
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

    /// captions.json was written by somebody. Possibly by us.
    ///
    /// The echo is the whole subtlety. This process now writes the file too — a
    /// drag ends, a wheel turns — and inotify does not distinguish. Without a
    /// guard, every write we make comes straight back as "the settings card
    /// changed something", which during a drag would re-place the surface from
    /// the file on the very frame the pointer is moving it. So the exact text of
    /// our own last write is kept, and a file that still says exactly that is
    /// not news. Anything else is, including a hand-edit that happens to match
    /// the values we hold — that is a no-op anyway, caught by the equality below.
    fn reload_settings(&mut self, path: &Path, qh: &QueueHandle<Self>) {
        let raw = std::fs::read_to_string(path).ok();
        if is_our_own_echo(raw.as_deref(), self.last_written.as_deref()) {
            return;
        }
        let next = CaptionSettings::load(path);
        if next == self.settings {
            return;
        }
        eprintln!(
            "[overlay] captions.json changed — turns {}, size {}, hold {}s, ground {:.2}, \
             showYou {}, clickThrough {}",
            next.turns, next.size, next.hold_s, next.opacity, next.show_you, next.click_through
        );
        self.apply_settings(next, qh);
    }

    /// Fold a new settings block in and re-send whatever is layer-shell state
    /// rather than a property of the next frame.
    ///
    /// Three things live on the compositor rather than in the pixels: the size,
    /// the margins, and the input region. Everything else — text size, ground,
    /// hold, whose turns — is decided again by `draw`.
    fn apply_settings(&mut self, next: CaptionSettings, qh: &QueueHandle<Self>) {
        let before = std::mem::replace(&mut self.settings, next);

        // A different screen is not a property that can be re-sent: the surface
        // has to be made again over there. This is the settings card's Screen
        // selector arriving, and it takes the same road a drag across a seam
        // does — including keeping the bar at the same place ON the new screen
        // rather than at the same place on the desk, because somebody who
        // picked a screen from a list meant "put it there", not "shift it 1440
        // pixels".
        if self.settings.output != before.output
            && let Some(name) = self.settings.output.clone()
            && let Some(at) = layout::screen_named(&self.screens, &name)
            && at != self.current
        {
            let margins = self.margins();
            self.hop_to(at, margins, qh);
            return;
        }

        let (Some(layer), Some(cfg)) = (self.layer.as_ref(), self.cfg.as_ref()) else {
            self.dirty = true;
            return;
        };
        let next_cfg = layer_config(
            &self.settings,
            self.out_size,
            self.out_origin,
            self.margin_override,
        );
        let mut commit = false;
        if (next_cfg.width, next_cfg.height) != (cfg.width, cfg.height) {
            layer.set_size(next_cfg.width, next_cfg.height);
            commit = true;
        }
        // Not while a drag is in flight: the person's hand is the authority on
        // where the bar is, not a file that was written a moment ago.
        if next_cfg.margin != cfg.margin && self.drag.is_none() {
            let (t, r, b, l) = next_cfg.margin;
            layer.set_margin(t, r, b, l);
            commit = true;
        }
        if before.click_through != self.settings.click_through {
            apply_input_region(&self.compositor, qh, layer.wl_surface(), &next_cfg.input);
            commit = true;
        }
        let margin = if self.drag.is_none() {
            next_cfg.margin
        } else {
            cfg.margin
        };
        self.cfg = Some(LayerConfig { margin, ..next_cfg });
        if commit {
            layer.commit();
        }
        self.dirty = true;
    }

    /// Move the surface, without touching the file. Called on every motion
    /// event of a drag; the file is written once, on release.
    fn place(&mut self, margins: (i32, i32)) {
        let (Some(layer), Some(cfg)) = (self.layer.as_ref(), self.cfg.as_mut()) else {
            return;
        };
        if cfg.margin == (0, 0, margins.1, margins.0) {
            return;
        }
        cfg.margin = (0, 0, margins.1, margins.0);
        layer.set_margin(0, 0, margins.1, margins.0);
        layer.commit();
    }

    fn margins(&self) -> (i32, i32) {
        self.cfg
            .as_ref()
            .map(|c| (c.margin.3, c.margin.2))
            .unwrap_or((0, 0))
    }

    fn size(&self) -> (u32, u32) {
        self.cfg
            .as_ref()
            .map(|c| (c.width, c.height))
            .unwrap_or((0, 0))
    }

    /// Ask for the settings to be written, once the hand has stopped.
    fn save_soon(&mut self) {
        self.save_at = Some(Instant::now() + SAVE_DEBOUNCE);
    }

    /// Write captions.json, if it is due. Called from the run loop rather than
    /// from a pointer handler, so a wheel spin is one write and not thirty.
    fn flush_save(&mut self) {
        if self.save_at.is_none_or(|at| Instant::now() < at) {
            return;
        }
        self.save_at = None;
        match self.settings.save_atomic(&self.settings_path) {
            Ok(text) => {
                // Remembered BEFORE the inotify event arrives, which is the
                // whole point: the watch fires on this write and must recognise
                // it.
                self.last_written = Some(text);
                let b = self.settings.bounds;
                eprintln!(
                    "[overlay] wrote {} — size {}, bounds {}",
                    self.settings_path.display(),
                    self.settings.size,
                    b.map(|b| format!("{}x{} at ({},{})", b.width, b.height, b.x, b.y))
                        .unwrap_or_else(|| "null".into()),
                );
            }
            Err(e) => eprintln!(
                "[overlay] could not write {}: {e}",
                self.settings_path.display()
            ),
        }
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
                translation_display: self.translation_display,
            });
            let mut shown = visible(&self.turns, self.settings.turns, self.settings.show_you);
            // Furniture you cannot see is furniture you cannot move. Turning
            // click-through off before anybody has said anything would otherwise
            // give a fully transparent surface that takes clicks and shows
            // nothing — the worst of both states. So the empty bar says what it
            // is and what can be done to it, including the way back out.
            if shown.is_empty() && !self.settings.click_through {
                shown.push(move_hint());
            }
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

/// Is this inotify event about the file this process just wrote?
///
/// Pulled out as a function of two strings so the one rule that keeps a drag
/// from fighting its own settings file can be checked without a compositor.
/// Byte identity, not value equality: a file somebody else wrote with the same
/// VALUES is caught a line later by the settings comparison, and that is a
/// no-op anyway. What must never happen is our own write coming back as news.
///
/// A missing file is never an echo — this process does not delete it, so
/// something else did, and the defaults are then genuinely the new state.
pub fn is_our_own_echo(raw: Option<&str>, last_written: Option<&str>) -> bool {
    match (raw, last_written) {
        (Some(raw), Some(ours)) => raw == ours,
        _ => false,
    }
}

/// What an empty bar says while it is being moved.
///
/// A `Turn` rather than a special case in the rasteriser: it is one line of text
/// on the same ground in the same layout, and a second drawing path for it would
/// be a second thing to keep looking like the first. `speaker: None` gives it
/// the nameless grey, which is right — nobody said this.
fn move_hint() -> Turn {
    Turn {
        id: 0,
        t_ms: 0,
        speaker: None,
        who: "captions".into(),
        text: "drag to move · scroll to resize the text · right-click to let clicks through again"
            .into(),
        shaky: false,
        lang: None,
        translation: None,
        mine: false,
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
///
/// `Full` is its opposite and is spelled with a NULL region, which the protocol
/// defines as infinite. Not a rectangle the size of the surface: a rectangle
/// would have to be re-sent on every configure, and a bar whose grabbable area
/// was one resize behind its pixels is worse than either state.
fn apply_input_region(
    compositor: &CompositorState,
    qh: &QueueHandle<App>,
    surface: &wl_surface::WlSurface,
    spec: &InputRegion,
) {
    let _ = qh;
    match spec {
        InputRegion::Empty => match Region::new(compositor) {
            Ok(region) => {
                surface.set_input_region(Some(region.wl_region()));
                eprintln!(
                    "[overlay] wl_surface.set_input_region: empty region (0 rectangles) — \
                     clicks pass through to whatever is underneath"
                );
            }
            // Said loudly rather than swallowed. A caption bar that quietly
            // became clickable is the bug this whole path exists to fix, and
            // the person needs to know before it is over their game.
            Err(e) => eprintln!(
                "[overlay] WARNING: could not create an input region ({e}); \
                 this surface may take clicks"
            ),
        },
        InputRegion::Full => {
            surface.set_input_region(None);
            eprintln!(
                "[overlay] wl_surface.set_input_region: null (infinite) — drag to move, \
                 scroll to resize the text, right-click to hand the pointer back"
            );
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

// ---------------------------------------------------------------------------
// the pointer
//
// Only reachable while `clickThrough` is off — with it on, the input region is
// empty and the compositor has nowhere to deliver any of this. That is worth
// saying plainly, because it means none of the code below can steal a click
// from a game: it is not a filter this process applies, it is a delivery the
// compositor never makes.
// ---------------------------------------------------------------------------

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {
        self.pointer = None;
        self.drag = None;
    }

    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability != Capability::Pointer || self.pointer.is_some() {
            return;
        }
        // A THEMED pointer, so the cursor can say "grab" while the bar is
        // grabbable. Where `wp_cursor_shape_manager_v1` is offered — it is, on
        // this KWin — that costs one request and no cursor theme loading.
        let shm = self.shm.wl_shm().clone();
        let surface = self.compositor.create_surface(qh);
        match self.seat_state.get_pointer_with_theme::<Self, ()>(
            qh,
            &seat,
            &shm,
            surface,
            ThemeSpec::default(),
        ) {
            Ok(pointer) => {
                eprintln!("[overlay] pointer acquired (used only while clickThrough is off)");
                self.pointer = Some(pointer);
            }
            Err(e) => {
                eprintln!("[overlay] no pointer on this seat ({e}); the bar cannot be dragged")
            }
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer {
            self.pointer = None;
            self.drag = None;
        }
    }
}

impl PointerHandler for App {
    fn pointer_frame(
        &mut self,
        conn: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        let ours = self.layer.as_ref().map(|l| l.wl_surface().clone());
        for event in events {
            if ours.as_ref() != Some(&event.surface) {
                continue; // the cursor surface, or somebody else's
            }
            match event.kind {
                PointerEventKind::Enter { .. } => {
                    // The icon has to be re-set on every enter; the protocol
                    // does not remember it for us.
                    if let Some(p) = self.pointer.as_ref() {
                        let _ = p.set_cursor(conn, CursorIcon::Grab);
                    }
                }
                PointerEventKind::Leave { .. } => {
                    // A drag that ends off the surface still ended. Keeping it
                    // would leave the bar glued to a pointer that is elsewhere.
                    if self.drag.take().is_some_and(|d| d.moved) {
                        self.remember_position();
                    }
                }
                PointerEventKind::Press { button, .. } if button == BTN_LEFT => {
                    self.drag = Some(Drag {
                        press: event.position,
                        from: self.margins(),
                        moved: false,
                    });
                    if let Some(p) = self.pointer.as_ref() {
                        let _ = p.set_cursor(conn, CursorIcon::Grabbing);
                    }
                }
                PointerEventKind::Motion { .. } => {
                    let Some(drag) = self.drag else { continue };
                    // Unclamped, and deliberately: clamping to the screen the
                    // surface is on is exactly what stopped the bar ever
                    // reaching the next one. Where it may go is now a question
                    // about the whole desk, and `drag_step` answers it.
                    let want = (
                        drag.from.0 + (event.position.0 - drag.press.0).round() as i32,
                        drag.from.1 - (event.position.1 - drag.press.1).round() as i32,
                    );
                    let Some(here) = self.screens.get(self.current) else {
                        continue;
                    };
                    let rect = here.bounds_for(want, self.size());
                    match layout::drag_step(&self.screens, self.current, rect) {
                        // Carried into the gap beside a shorter monitor, or off
                        // the edge of the desk. Nothing moves: there is no title
                        // bar to drag it back by.
                        DragStep::Nowhere => {}
                        DragStep::Stay { margins } => {
                            if margins != self.margins() {
                                if let Some(d) = self.drag.as_mut() {
                                    d.moved = true;
                                }
                                self.place(margins);
                            }
                        }
                        DragStep::Hop { screen, margins } => self.hop_to(screen, margins, qh),
                    }
                }
                PointerEventKind::Release { button, .. } if button == BTN_LEFT => {
                    let moved = self.drag.take().is_some_and(|d| d.moved);
                    if let Some(p) = self.pointer.as_ref() {
                        let _ = p.set_cursor(conn, CursorIcon::Grab);
                    }
                    // A press and release that never moved is a click, not a
                    // move, and must not rewrite a file somebody is watching.
                    if moved {
                        self.remember_position();
                    }
                }
                PointerEventKind::Release { button, .. } if button == BTN_RIGHT => {
                    // The way out. Somebody who turned click-through off and now
                    // has a bar eating their clicks cannot reach the settings
                    // card underneath it — so the bar itself has to be able to
                    // hand the pointer back.
                    eprintln!("[overlay] right-click: clicks pass through again");
                    let next = CaptionSettings {
                        click_through: true,
                        ..self.settings.clone()
                    };
                    self.apply_settings(next, qh);
                    self.save_soon();
                }
                PointerEventKind::Axis { vertical, .. } => {
                    // v120 where the compositor sends it, the deprecated
                    // discrete count where it does not, and pixels as the last
                    // resort — a touchpad reports only the last of those.
                    let notches = if vertical.value120 != 0 {
                        vertical.value120 as f32 / 120.0
                    } else if vertical.discrete != 0 {
                        vertical.discrete as f32
                    } else {
                        vertical.absolute as f32 / 53.0
                    };
                    let next = size_after_scroll(self.settings.size, notches);
                    if next != self.settings.size {
                        self.settings.size = next;
                        self.dirty = true;
                        self.save_soon();
                    }
                }
                _ => {}
            }
        }
    }
}

impl App {
    /// Where the bar has ended up, in the coordinates `bounds` is written in.
    fn remember_position(&mut self) {
        let Some(here) = self.screens.get(self.current) else {
            return;
        };
        let bounds = here.bounds_for(self.margins(), self.size());
        let name = Some(here.name.clone());
        // The screen goes down with the rectangle. `bounds` alone would be
        // enough on this desk, but not on one where a monitor is unplugged and
        // the coordinates it used to occupy now belong to another — and it is
        // the field the settings card's Screen selector writes, so both routes
        // have to mean the same thing.
        if self.settings.bounds != Some(bounds) || self.settings.output != name {
            self.settings.bounds = Some(bounds);
            self.settings.output = name;
            self.save_soon();
        }
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
    // A monitor plugged in, unplugged, moved in the arrangement, or rescaled.
    // The desk's geometry is what every drag is measured against, so it is
    // re-read rather than assumed — but the surface is NOT re-made here: the
    // compositor closes a layer surface whose output has gone, and that arrives
    // as `closed`, which is a different and much clearer event to act on.
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
        self.refresh_screens();
    }
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
        self.refresh_screens();
    }
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
        self.refresh_screens();
    }
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
    registry_handlers![OutputState, SeatState];
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

    /// The setting decides, and nothing else does. 0.10.0 asserted the opposite
    /// — that nothing could produce anything but `Empty` — and that assertion
    /// was the bug: it was a law where a default was wanted.
    #[test]
    fn the_setting_is_the_only_thing_that_decides_the_input_region() {
        let on = CaptionSettings {
            click_through: true,
            ..CaptionSettings::default()
        };
        let off = CaptionSettings {
            click_through: false,
            ..CaptionSettings::default()
        };
        assert_eq!(input_region(&on), InputRegion::Empty);
        assert_eq!(input_region(&off), InputRegion::Full);
        // And it survives the trip through the whole config, with every other
        // field moved around underneath it.
        for bounds in [
            None,
            Some(Bounds {
                x: 10,
                y: 20,
                width: 900,
                height: 200,
            }),
        ] {
            for out in [(2560u32, 1440u32), (1366, 768)] {
                assert_eq!(
                    layer_config(
                        &CaptionSettings {
                            bounds,
                            ..on.clone()
                        },
                        out,
                        (0, 0),
                        None
                    )
                    .input,
                    InputRegion::Empty
                );
                assert_eq!(
                    layer_config(
                        &CaptionSettings {
                            bounds,
                            ..off.clone()
                        },
                        out,
                        (0, 0),
                        None
                    )
                    .input,
                    InputRegion::Full
                );
            }
        }
    }

    /// Click-through is the DEFAULT. A fresh profile, a corrupt file, an empty
    /// object: all of them are a bar the pointer passes through, because that is
    /// what a caption bar is for nearly all of the time.
    #[test]
    fn a_bar_nobody_has_configured_is_still_scenery() {
        assert_eq!(
            input_region(&CaptionSettings::default()),
            InputRegion::Empty
        );
        assert_eq!(
            input_region(&CaptionSettings::normalize(&serde_json::json!({}))),
            InputRegion::Empty
        );
        assert_eq!(
            input_region(&CaptionSettings::normalize(&serde_json::json!(
                "not json at all"
            ))),
            InputRegion::Empty
        );
    }

    /// The facts that make this a caption bar rather than a window: above a
    /// fullscreen game, in a corner so both margins are ours to set, reserving
    /// nothing, and deaf — deaf in BOTH modes, because a layer surface that took
    /// the keyboard would take it from the game.
    #[test]
    fn the_surface_is_a_caption_bar_in_either_mode() {
        for click_through in [true, false] {
            let s = CaptionSettings {
                click_through,
                ..CaptionSettings::default()
            };
            let cfg = layer_config(&s, (2560, 1440), (0, 0), None);
            assert_eq!(
                cfg.layer,
                Layer::Overlay,
                "TOP loses to a fullscreen window"
            );
            assert_eq!(cfg.anchor, Anchor::BOTTOM.union(Anchor::LEFT));
            assert_eq!(
                cfg.exclusive_zone, -1,
                "a caption bar must reserve no space"
            );
            assert_eq!(cfg.keyboard, KeyboardInteractivity::None);
            assert_eq!((cfg.width, cfg.height), (1100, 340));
            // Centred by arithmetic now that BOTTOM alone no longer does it.
            assert_eq!(cfg.margin, (0, 0, layout::DEFAULT_BOTTOM_MARGIN, 730));
        }
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
        let cfg = layer_config(&s, (1920, 1080), (0, 0), None);
        assert_eq!((cfg.width, cfg.height), (800, 260));
        assert_eq!(cfg.margin, (0, 0, 1080 - 700 - 260, 100));
        // …and --margin overrules the remembered vertical position entirely.
        assert_eq!(
            layer_config(&s, (1920, 1080), (0, 0), Some(12)).margin,
            (0, 0, 12, 100)
        );
    }

    /// One notch of the wheel is one notch of the slider, in the slider's own
    /// range — and up is bigger, which means the sign of Wayland's
    /// positive-downward axis flips exactly once.
    #[test]
    fn the_wheel_moves_the_text_size_in_the_sliders_own_steps() {
        assert_eq!(
            size_after_scroll(26.0, -1.0),
            27.0,
            "scrolling up did not grow the text"
        );
        assert_eq!(size_after_scroll(26.0, 1.0), 25.0);
        assert_eq!(size_after_scroll(26.0, -3.0), 29.0);
        // The ends of the slider are the ends of the wheel.
        assert_eq!(size_after_scroll(40.0, -5.0), 40.0);
        assert_eq!(size_after_scroll(18.0, 5.0), 18.0);
        // A touchpad's fractional notches still land on a step the slider could
        // produce, rather than on 26.4.
        let after = size_after_scroll(26.0, -0.4);
        assert_eq!(after, after.round());
    }

    /// The guard itself, as three strings and a rule. Everything a drag does to
    /// the file comes back through inotify; without this the surface would be
    /// re-placed from disk on the frame the pointer is moving it.
    #[test]
    fn only_our_own_bytes_are_an_echo() {
        let ours = "{\n  \"size\": 26\n}\n";
        assert!(is_our_own_echo(Some(ours), Some(ours)));
        assert!(
            !is_our_own_echo(Some("{}"), Some(ours)),
            "somebody else's write was swallowed"
        );
        // Before this process has written anything, nothing is an echo.
        assert!(!is_our_own_echo(Some(ours), None));
        // A file that has gone is not an echo either: this process never
        // deletes it, so something else did and that is real news.
        assert!(!is_our_own_echo(None, Some(ours)));
        assert!(!is_our_own_echo(None, None));
    }

    /// The echo. This process writes captions.json now, inotify fires on its own
    /// writes, and a reload that took the echo for somebody else's change would
    /// re-place the surface from the file on the frame the pointer is moving it.
    ///
    /// The guard is the exact text, so this checks the round trip that guard
    /// depends on: what we write is byte-identical to what we would compare
    /// against, and a real edit is not.
    #[test]
    fn our_own_write_is_recognisable_and_somebody_elses_is_not() {
        let dir = std::env::temp_dir().join(format!("nx-recall-echo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("captions.json");

        let s = CaptionSettings {
            bounds: Some(Bounds {
                x: 40,
                y: 900,
                width: 1100,
                height: 340,
            }),
            ..CaptionSettings::default()
        };
        let written = s.save_atomic(&path).unwrap();
        // What the watch will read back is exactly what we recorded.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), written);
        assert_eq!(
            Some(std::fs::read_to_string(&path).unwrap()),
            Some(written.clone())
        );

        // Somebody else writing the same VALUES a different way is not
        // byte-identical — which is fine, because the second guard is value
        // equality and that one catches it.
        std::fs::write(
            &path,
            serde_json::to_string(&serde_json::json!({
                "turns": 5, "size": 26, "hold_s": 12, "opacity": 0.6,
                "showYou": true, "clickThrough": true,
                "bounds": {"x": 40, "y": 900, "width": 1100, "height": 340}
            }))
            .unwrap(),
        )
        .unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_ne!(
            raw, written,
            "the byte guard would have been enough on its own"
        );
        assert_eq!(
            CaptionSettings::load(&path),
            s,
            "…and the value guard catches it"
        );

        // A real change is neither.
        let mut other = s.clone();
        other.size = 33.0;
        other.save_atomic(&path).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_ne!(raw, written);
        assert_ne!(CaptionSettings::load(&path), s);
        let _ = std::fs::remove_dir_all(&dir);
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
