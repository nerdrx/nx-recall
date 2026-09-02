//! `nx-recall-overlay` — live captions in the headset, and the honest answer
//! about whether that is possible on this machine.
//!
//! The user runs WiVRn, which is Monado underneath and is NOT SteamVR — so the
//! usual answer for "put a panel in front of somebody's game", an OpenVR
//! overlay, is the wrong tool and would need a runtime that is not installed.
//! The OpenXR equivalent is `XR_EXTX_overlay`: a second session, in a second
//! process, that submits composition layers into the frame the real application
//! is already producing.
//!
//! The extension is an EXTX — a cross-vendor EXPERIMENTAL extension — and no
//! amount of wanting makes a runtime implement one. So this binary's default
//! mode is not to draw anything. It is to ask, and to print what it was told:
//!
//!   nx-recall-overlay            what this runtime advertises, and the verdict
//!   nx-recall-overlay --json     the same, for a bug report
//!   nx-recall-overlay --feed     the captions themselves, on stdout, from the
//!                                daemon socket — the half that works no matter
//!                                what the runtime says
//!   nx-recall-overlay --overlay  try to actually put them in the headset
//!
//! See docs/OVERLAY.md for what the probe found on the machine this was written
//! on, and for the route that works when the answer is no.

mod feed;
mod probe;
mod raster;
mod xr;

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "nx-recall-overlay",
    about = "Live captions as an OpenXR overlay layer, and the probe that says whether this runtime can host one"
)]
struct Args {
    /// Try to open an overlay session and draw. Refuses, loudly, on a runtime
    /// that does not advertise XR_EXTX_overlay.
    #[arg(long)]
    overlay: bool,

    /// Print the captions the daemon is producing, to stdout, and stop there.
    /// This is the half of the feature that does not depend on a headset at
    /// all — and the one the desktop captions window already ships.
    #[arg(long)]
    feed: bool,

    /// The probe's findings as JSON.
    #[arg(long)]
    json: bool,

    /// The daemon's control socket. Defaults to
    /// `$XDG_RUNTIME_DIR/nx-recall.sock`, exactly as every other client does.
    #[arg(long)]
    socket: Option<PathBuf>,

    /// How many turns to keep on screen.
    #[arg(long, default_value_t = 5)]
    turns: usize,

    /// How far in front of the eye the quad sits, in metres. Only meaningful
    /// with --overlay.
    #[arg(long, default_value_t = 1.8)]
    distance: f32,

    /// How wide the quad is, in metres, at that distance.
    #[arg(long, default_value_t = 1.4)]
    width: f32,

    /// Leave the quad where the room is rather than carrying it on the head.
    /// Head-locked is the default because a caption you have to look for is not
    /// a caption.
    #[arg(long)]
    world_locked: bool,

    /// Draw one frame of captions from the daemon and write it to a PPM, with
    /// no headset and no OpenXR involved at all. This is how the drawing half
    /// is checked on a machine that has no runtime — and how you find out what
    /// a size or an opacity actually looks like.
    #[arg(long, value_name = "FILE")]
    render: Option<PathBuf>,

    /// The typeface. Defaults to whichever system sans-serif is present; there
    /// is no bundled one, because shipping a typeface means shipping a licence.
    #[arg(long)]
    font: Option<PathBuf>,

    /// The words, in pixels of the 1024x512 quad texture.
    #[arg(long, default_value_t = 34.0)]
    size: f32,

    /// How much of the scene behind the bar the ground covers.
    #[arg(long, default_value_t = 0.6)]
    opacity: f32,
}

/// `--render`: one frame, from the real daemon, to a file. No OpenXR.
fn render_once(args: &Args, out: &std::path::Path) -> Result<()> {
    let path = args
        .socket
        .clone()
        .unwrap_or_else(feed::default_socket);
    let mut f = feed::Feed::connect(&path)?;
    let mut caps = feed::Captions::new(args.turns);
    if let Ok(list) = f.call("speakers.list", serde_json::json!({})) {
        caps.learn_speakers(&list);
    }
    // Here — and only here — the recent history IS what we want: a still frame
    // of an empty bar proves nothing. The live path never does this.
    let tail = f.call("transcript", serde_json::json!({"limit": args.turns}))?;
    caps.seed_tail(&tail);
    for seg in tail["segments"].as_array().into_iter().flatten() {
        caps.seed_turn(seg);
    }
    let style = raster::Style {
        size: args.size,
        opacity: args.opacity,
        ..raster::Style::default()
    };
    let renderer = raster::Renderer::new(style, args.font.as_deref())?;
    let turns: Vec<feed::Turn> = caps.turns().cloned().collect();
    let surface = renderer.render(&turns, None);
    surface.write_ppm(out)?;
    println!(
        "{} turns drawn at {}x{} -> {}",
        turns.len(),
        surface.width,
        surface.height,
        out.display()
    );
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();

    if args.feed {
        return feed::run(args.socket.as_deref(), args.turns);
    }

    if let Some(out) = args.render.as_deref() {
        return render_once(&args, out);
    }

    let found = probe::Probe::run()?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&found.to_json())?);
    } else {
        found.print();
    }

    if args.overlay {
        anyhow::ensure!(
            found.overlay,
            "this runtime ({} {}) does not advertise XR_EXTX_overlay, so there is no overlay \
             session to open. See docs/OVERLAY.md for the route that works here.",
            found.runtime,
            found.runtime_version,
        );
        return run_overlay(&args);
    }

    Ok(())
}

/// `--overlay`: the captions, in the headset.
///
/// The banner is not decoration. This path has never been executed — see the
/// warning at the top of xr.rs — and somebody starting it needs to know that
/// before it is between them and their game rather than after.
fn run_overlay(args: &Args) -> Result<()> {
    eprintln!();
    eprintln!("!! --overlay has never been run against a live OpenXR runtime.");
    eprintln!("!! The extension probe above is a measurement; this is a bring-up.");
    eprintln!("!! If it misbehaves, ^C is safe: an overlay session owns no frame.");
    eprintln!();

    let cfg = xr::Config {
        placement: xr::Placement {
            distance: args.distance,
            width: args.width,
            world_locked: args.world_locked,
        },
        font: args.font.clone(),
        size: args.size,
        opacity: args.opacity,
    };
    let renderer = xr::renderer_for(&cfg)?;

    // The feed runs on its own thread and hands over finished PIXELS, not
    // turns: rasterising on the frame loop would put a font renderer between
    // xrWaitFrame and xrEndFrame, which is the one place in an XR program where
    // nothing slow is allowed to happen.
    let (tx, rx) = std::sync::mpsc::channel();
    let socket = args.socket.clone().unwrap_or_else(feed::default_socket);
    let turns = args.turns;
    let feed_renderer = xr::renderer_for(&cfg)?;
    std::thread::spawn(move || {
        if let Err(e) = pump(&socket, turns, &feed_renderer, &tx) {
            eprintln!("captions feed stopped: {e}");
        }
    });

    xr::run(cfg, &renderer, rx)
}

/// Connect, subscribe, and rasterise a new surface every time the stack moves.
fn pump(
    socket: &std::path::Path,
    turns: usize,
    renderer: &raster::Renderer,
    tx: &std::sync::mpsc::Sender<raster::Surface>,
) -> Result<()> {
    let mut f = feed::Feed::connect(socket)?;
    let mut caps = feed::Captions::new(turns);
    if let Ok(list) = f.call("speakers.list", serde_json::json!({})) {
        caps.learn_speakers(&list);
    }
    // The same seeding the desktop captions window does, and for the same
    // reason: without a newest-seen timestamp the first event might be a
    // re-published archive row and there is nothing to compare it against.
    if let Ok(tail) = f.call("transcript", serde_json::json!({"limit": 1})) {
        caps.seed_tail(&tail);
    }
    f.call("subscribe", serde_json::json!({"topics": ["segments", "relabel"]}))?;
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
        let list: Vec<feed::Turn> = caps.turns().cloned().collect();
        if tx.send(renderer.render(&list, None)).is_err() {
            return Ok(()); // the frame loop went away
        }
    }
}
