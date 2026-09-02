// The captions window — a second BrowserWindow that floats over whatever the
// user is actually doing, and the settings file behind it.
//
// It is the only surface in this app that is not the app: frameless,
// transparent, always on top, no taskbar entry, and click-through by default,
// because the whole point is to sit on top of a game and be read rather than
// used. Everything about that makes it the main process's problem — a renderer
// cannot make itself always-on-top, cannot make itself ignore a click, and must
// not be the thing that remembers where it was.
//
// It talks to the same daemon through the same preload bridge as the main
// window: no second socket, no second model. What arrives there arrives here.

import { BrowserWindow, screen } from 'electron';
import { spawn, spawnSync } from 'node:child_process';
import { accessSync, constants, readFileSync, writeFileSync, mkdirSync, watch } from 'node:fs';
import { delimiter } from 'node:path';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import { normalizeCaptionSettings } from '../renderer/lib/captions.js';

const __dirname = dirname(fileURLToPath(import.meta.url));
const ROOT = join(__dirname, '..', '..');

/**
 * The deep-space ground, and the one hard rule of this window (DESIGN §14, as
 * applied to captions): it is DARK on both of NX Clear's grounds. Every other
 * surface in the app follows the OS, because every other surface is a page you
 * look at. Captions float over somebody else's pixels, and light captions over
 * a dark game are a white slab in the middle of the screen.
 */
export const CAPTIONS_GROUND = '#06040c';

let win = null;
let settings = null;
let settingsFile = null;
let saveTimer = null;
let onChange = null;
/** The `nx-recall-overlay --desktop` child, when the layer path is the one in use. */
let layer = null;
/** One restart, not a loop. Reset every time the user asks for captions afresh. */
let layerRestarted = false;
/**
 * This desktop cannot host a layer surface — the child said so with exit 2 — so
 * every later "Captions" opens the BrowserWindow without trying again. Learned
 * once per run, because a compositor does not change under a running app.
 */
let layerRefused = false;
/**
 * The exact text of the last write THIS process made, and the watch that would
 * otherwise mistake it for somebody else's. Both halves of the feature write
 * captions.json now — see `watchSettingsFile`.
 */
let lastWritten = null;
let fileWatcher = null;
let reloadTimer = null;

// ---------------------------------------------------------------------------
// the settings file
// ---------------------------------------------------------------------------

/**
 * Where the file lives. Passed in rather than reached for, so the headless
 * suite writes into the scratch userData the e2e path already redirects to and
 * never near the real profile.
 */
export function initCaptionSettings(dir, notify = null) {
  settingsFile = join(dir, 'captions.json');
  onChange = notify;
  let raw = null;
  try {
    raw = JSON.parse(readFileSync(settingsFile, 'utf8'));
  } catch {
    // No file yet, or a file that is not JSON any more. Either way the defaults
    // are the honest answer and the next write repairs it.
    raw = null;
  }
  settings = normalizeCaptionSettings(raw);
  return settings;
}

/**
 * Which surface this desktop will actually put the captions on, and therefore
 * what the settings card is allowed to claim:
 *
 *   'layer'          — a wlr-layer-shell surface with an empty input region.
 *                      Click-through is not a setting here, it is the surface.
 *   'window-wayland' — the BrowserWindow, on Wayland, where Electron's
 *                      `setIgnoreMouseEvents` is measurably a no-op. The toggle
 *                      exists and does nothing, and the card says so.
 *   'window'         — the BrowserWindow on X11 or Windows, where the toggle is
 *                      real.
 *
 * Derived rather than remembered: a compositor does not change under a running
 * app, but which binaries are installed can, and the honest answer is the one
 * taken when somebody looks.
 */
function captionSurface() {
  if (!wantsLayerCaptions()) return 'window';
  if (layerRefused || !overlayBinary()) return 'window-wayland';
  return 'layer';
}

// Looked up once. `getCaptionSettings` is called on every frame of a slider
// drag, and a filesystem probe per frame for a fact that changes when somebody
// installs a package is a stat storm for nothing.
let overlayBinaryCache;
function overlayBinary() {
  if (overlayBinaryCache === undefined) overlayBinaryCache = findOverlayBinary();
  return overlayBinaryCache;
}

/**
 * The screens, for the Screen selector on the Captions card (0.10.3).
 *
 * A layer surface belongs to one `wl_output` and cannot change it, so "which
 * screen" is a real setting rather than a hint — and the card that offers it is
 * a page in a renderer with no Wayland access at all. So the overlay is asked:
 * `--list-outputs` prints the desk as one line of JSON and exits.
 *
 * A subprocess rather than an `outputs` block in captions.json, deliberately.
 * That file is what a person chose; writing hardware state into it on every
 * launch would rewrite a file both sides watch, trip both echo guards, churn
 * something people diff, and mix a cache in with the settings. This costs a few
 * milliseconds, once.
 *
 * Cached for the life of the process. A monitor plugged in mid-session is not
 * picked up until the app restarts; the OVERLAY notices immediately, which is
 * the half that matters — the list here is only what the selector offers.
 */
let outputsCache;
function captionOutputs() {
  if (outputsCache !== undefined) return outputsCache;
  outputsCache = [];
  const bin = overlayBinary();
  if (!bin || !wantsLayerCaptions()) return outputsCache;
  try {
    const run = spawnSync(bin, ['--list-outputs'], { encoding: 'utf8', timeout: 4000 });
    if (run.status !== 0) return outputsCache;
    const parsed = JSON.parse(run.stdout);
    if (Array.isArray(parsed?.outputs)) {
      outputsCache = parsed.outputs.filter((o) => typeof o?.name === 'string' && o.name);
    }
  } catch (e) {
    // No list is not a failure: the card simply does not offer the selector,
    // and the overlay keeps choosing a screen the way it always did.
    console.warn('[recall] could not list the screens:', e.message);
  }
  return outputsCache;
}

/**
 * The settings, plus the one fact about them that is not a setting.
 *
 * `surface` is deliberately outside the normalized block: `save()` writes the
 * normalized block, and `normalizeCaptionSettings` drops what it does not know,
 * so this can be read by the card and can never reach captions.json.
 */
export function getCaptionSettings() {
  if (!settings) settings = normalizeCaptionSettings(null);
  return { ...settings, surface: captionSurface(), outputs: captionOutputs() };
}

/**
 * Fold a change in and apply it. Every path lands here — the settings card, the
 * tray's click-through item, the window remembering where it was dragged to —
 * so there is exactly one place that writes the file and one place that tells
 * the windows.
 */
export function setCaptionSettings(patch) {
  const before = getCaptionSettings();
  settings = normalizeCaptionSettings({ ...before, ...(patch ?? {}) });
  if (win && !win.isDestroyed() && settings.clickThrough !== before.clickThrough) applyClickThrough();
  save();
  onChange?.(getCaptionSettings());
  return getCaptionSettings();
}

// Debounced, because dragging the window fires `moved` on every frame and a
// disk write per frame is a real cost for a fact nobody reads until next launch.
function save() {
  if (!settingsFile) return;
  if (saveTimer) clearTimeout(saveTimer);
  saveTimer = setTimeout(() => {
    saveTimer = null;
    try {
      mkdirSync(dirname(settingsFile), { recursive: true });
      const text = JSON.stringify(settings, null, 2);
      // Recorded BEFORE the write, so the watch below cannot see the change
      // before it knows the change was ours.
      lastWritten = text;
      writeFileSync(settingsFile, text);
    } catch (e) {
      console.warn('[recall] could not save the captions settings:', e.message);
    }
  }, 400);
  if (saveTimer.unref) saveTimer.unref();
}

/**
 * Two writers, one file.
 *
 * Until the caption bar could be moved, this process was the only thing that
 * wrote captions.json and reading it once at startup was enough. It is not any
 * more: on the layer path the bar itself writes `bounds` when it is dragged and
 * `size` when it is scrolled over. Without this watch, the copy held here would
 * go stale the moment somebody moved the bar, and the next slider nudge would
 * write the OLD position back — the drag would quietly undo itself.
 *
 * Same echo rule as the Rust side: the exact text we wrote is not news. Byte
 * identity rather than value equality, because value equality is the second
 * guard (`same` below) and catching an echo needs the first.
 *
 * Watching the DIRECTORY, not the file: the overlay writes atomically, through
 * a temp file and a rename, and a watch on an inode does not survive that.
 */
function watchSettingsFile() {
  if (fileWatcher || !settingsFile) return;
  try {
    mkdirSync(dirname(settingsFile), { recursive: true });
    fileWatcher = watch(dirname(settingsFile), (_event, name) => {
      if (name && name !== 'captions.json') return;
      // Coalesced: one atomic write is a rename plus a couple of events, and a
      // re-read per event is three reads for one change.
      if (reloadTimer) clearTimeout(reloadTimer);
      reloadTimer = setTimeout(reloadFromDisk, 120);
      if (reloadTimer.unref) reloadTimer.unref();
    });
    fileWatcher.unref?.();
  } catch (e) {
    // A profile directory that cannot be watched is not a reason to refuse to
    // run; it only means a drag on the bar is not reflected in an open card
    // until the next launch.
    console.warn('[recall] could not watch the captions settings:', e.message);
  }
}

/** Re-read captions.json, unless the change was ours. */
function reloadFromDisk() {
  reloadTimer = null;
  if (!settingsFile) return;
  let text = null;
  try {
    text = readFileSync(settingsFile, 'utf8');
  } catch {
    return; // gone, or unreadable this instant; the next write repairs it
  }
  if (text === lastWritten) return; // our own write, coming back
  let next;
  try {
    next = normalizeCaptionSettings(JSON.parse(text));
  } catch {
    return; // half a file, or not JSON any more
  }
  const before = settings ?? normalizeCaptionSettings(null);
  const same = Object.keys(next).every((k) =>
    k === 'bounds' ? JSON.stringify(next[k]) === JSON.stringify(before[k]) : next[k] === before[k]
  );
  if (same) return;
  settings = next;
  console.log('[recall] captions.json changed underneath us; taking it');
  if (win && !win.isDestroyed() && next.clickThrough !== before.clickThrough) applyClickThrough();
  // Told to the windows and the tray the same way a card change is: the card
  // has sliders that must show what the bar was just scrolled to.
  onChange?.(getCaptionSettings());
}

/** Stop watching. Called on the way out, with everything else. */
function unwatchSettingsFile() {
  if (reloadTimer) clearTimeout(reloadTimer);
  reloadTimer = null;
  try {
    fileWatcher?.close();
  } catch {
    // Already gone.
  }
  fileWatcher = null;
}

// ---------------------------------------------------------------------------
// the layer surface
//
// The captions have always claimed to be click-through, and on X11 they were:
// `setIgnoreMouseEvents(true)` sets an empty X11 input shape and the pointer
// falls through. MEASURED on this machine (Electron 44, KDE Wayland): under
// Wayland it sets no region and does nothing. Chromium has no way to say "this
// surface is scenery" — a `wl_surface`'s input region is not reachable from
// Electron's API — so on a Wayland desktop the toggle was a lie in the UI, and
// a caption bar that eats a click into the game is the exact bug it exists to
// prevent.
//
// So on Wayland the "Captions" surface is not this process's window at all: it
// is `nx-recall-overlay --desktop`, a wlr-layer-shell client that sets that
// empty input region itself. It reads the SAME captions.json this file writes,
// so the settings card stays the one control surface, and it subscribes to the
// same daemon socket — no second model, no second set of rules.
//
// Everything below is about keeping that child indistinguishable from a window:
// the tray item, the rail button, `--captions` and the settings card must all
// behave the same whichever surface is up.
// ---------------------------------------------------------------------------

/**
 * Whether this run should use the layer surface at all.
 *
 * Wayland, Linux, and NOT the e2e harness. The harness is the exception on
 * purpose: it runs Electron inside a headless gamescope that does not implement
 * wlr-layer-shell, but it INHERITS the developer's own `WAYLAND_DISPLAY` — so
 * without this the driven app would put a caption bar on the real desktop
 * instead of in the compositor under test. The e2e path drives the window, which
 * is also the fallback path this file must keep working.
 */
export function wantsLayerCaptions() {
  if (process.platform !== 'linux') return false;
  if (process.env.NX_RECALL_E2E !== undefined) return false;
  if (process.env.NX_RECALL_NO_LAYER === '1') return false;
  return process.env.XDG_SESSION_TYPE === 'wayland' || !!process.env.WAYLAND_DISPLAY;
}

/**
 * Where `nx-recall-overlay` is, or null.
 *
 * Three places, in the order they are true:
 *   1. beside the app in the install layout — packaging/build-release.sh puts
 *      the gui in `lib/nx-recall/gui/` and the binary in `lib/nx-recall/`;
 *   2. the cargo target directory, for a checkout;
 *   3. PATH, which is where `~/.local/bin/nx-recall-overlay` lives.
 *
 * Resolved rather than shelled out to, so a missing binary is a fallback to the
 * window and not a spawn error the user has to read.
 */
export function findOverlayBinary(env = process.env, root = ROOT) {
  const exe = 'nx-recall-overlay';
  const candidates = [
    join(root, '..', exe), // lib/nx-recall/gui → lib/nx-recall/
    join(root, '..', 'target', 'release', exe),
    join(root, '..', 'target', 'debug', exe),
    ...String(env.PATH ?? '')
      .split(delimiter)
      .filter(Boolean)
      .map((dir) => join(dir, exe)),
  ];
  for (const path of candidates) {
    try {
      accessSync(path, constants.X_OK);
      return path;
    } catch {
      // Not there, or not runnable. The next candidate is the answer.
    }
  }
  return null;
}

/**
 * Start the layer surface. Returns false if it could not even be launched, in
 * which case the caller opens the window instead.
 */
function startLayer() {
  if (layer) return true;
  const bin = overlayBinary();
  if (!bin) {
    console.warn('[recall] nx-recall-overlay is not installed; captions fall back to the window');
    layerRefused = true;
    return false;
  }
  const args = ['--desktop', '--settings', settingsFile];
  // The same socket this process is already talking to. Passed rather than
  // re-derived, so a mock or a second daemon reaches both halves or neither.
  if (process.env.NX_RECALL_SOCK) args.push('--socket', process.env.NX_RECALL_SOCK);

  let child;
  try {
    child = spawn(bin, args, { stdio: ['ignore', 'inherit', 'inherit'] });
  } catch (e) {
    console.warn('[recall] could not start the captions overlay:', e.message);
    layerRefused = true;
    return false;
  }
  layer = child;
  // Only while the bar is up, and only on this path: the BrowserWindow does not
  // write the file behind this process's back, so nothing else needs a watch.
  watchSettingsFile();
  child.on('error', (e) => {
    console.warn('[recall] the captions overlay could not run:', e.message);
    if (layer === child) layer = null;
  });
  child.on('exit', (code, signal) => {
    if (layer !== child) return; // already replaced or deliberately stopped
    layer = null;
    if (signal) return; // we killed it, or the session did
    if (code === 2) {
      // The compositor does not offer zwlr_layer_shell_v1. Not a failure — an
      // answer. The window is the surface on this desktop, and asking again on
      // every toggle would just print the same line forever.
      console.log('[recall] no layer-shell here; captions fall back to the window');
      layerRefused = true;
      createCaptionsWindow({ show: true });
      onChange?.(getCaptionSettings());
      return;
    }
    if (code === 0) return; // it was asked to stop
    // Once. A crash loop behind a tray item is a crash loop nobody can see.
    if (layerRestarted) {
      console.warn(`[recall] the captions overlay exited ${code} twice; using the window`);
      layerRefused = true;
      createCaptionsWindow({ show: true });
      onChange?.(getCaptionSettings());
      return;
    }
    layerRestarted = true;
    console.warn(`[recall] the captions overlay exited ${code}; restarting it once`);
    startLayer();
  });
  return true;
}

function stopLayer() {
  const child = layer;
  layer = null;
  unwatchSettingsFile();
  if (child) child.kill('SIGTERM');
}

export function layerCaptionsRunning() {
  return !!layer;
}

/**
 * On the way out. A layer surface is a separate process, and a separate process
 * outlives its parent unless somebody says otherwise — a caption bar still on
 * the screen after the app has quit is furniture with nothing behind it.
 */
export function shutdownCaptions() {
  layerRefused = true; // nothing is to be restarted during a quit
  stopLayer();
}

// ---------------------------------------------------------------------------
// the window
// ---------------------------------------------------------------------------

/**
 * Where a captions window goes when it has never been placed: a wide, short bar
 * across the bottom third of the primary display, clear of the taskbar.
 *
 * The shape is chosen for the second thing this window is for. wlx-overlay-s
 * mirrors a Wayland window into the headset as a flat quad, and a 16:5-ish bar
 * is what reads at arm's length in a headset — a tall square of text does not.
 */
function defaultBounds() {
  const area = screen.getPrimaryDisplay().workAreaSize;
  const width = Math.min(1100, Math.round(area.width * 0.62));
  const height = Math.min(340, Math.round(area.height * 0.3));
  return {
    width,
    height,
    x: Math.round((area.width - width) / 2),
    y: Math.round(area.height - height - 96),
  };
}

function applyClickThrough() {
  if (!win || win.isDestroyed()) return;
  // `forward: true` keeps move events flowing so the page can still light up a
  // hover affordance; nothing in it is clickable while this is on.
  win.setIgnoreMouseEvents(!!getCaptionSettings().clickThrough, { forward: true });
}

function rememberBounds() {
  if (!win || win.isDestroyed()) return;
  setCaptionSettings({ bounds: win.getBounds() });
}

export function createCaptionsWindow({ show = true } = {}) {
  if (win && !win.isDestroyed()) {
    if (show) win.showInactive();
    return win;
  }
  const s = getCaptionSettings();
  win = new BrowserWindow({
    ...(s.bounds ?? defaultBounds()),
    minWidth: 240,
    minHeight: 90,
    show: false,
    frame: false,
    transparent: true,
    // A caption bar is furniture, not a program: it must not be alt-tabbable,
    // must not appear in the switcher, and must not steal focus when it opens.
    skipTaskbar: true,
    focusable: false,
    resizable: true,
    movable: true,
    minimizable: false,
    maximizable: false,
    fullscreenable: false,
    hasShadow: false,
    // Transparent means transparent: a background colour here paints a slab
    // behind the page and the settable ground opacity stops meaning anything.
    backgroundColor: '#00000000',
    title: 'NX Recall captions',
    // "screen-saver" is the level that stays above a fullscreen game on the
    // desktops this ships to; "floating" loses to one.
    alwaysOnTop: true,
    webPreferences: {
      preload: join(__dirname, 'preload.cjs'),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
    },
  });
  win.setAlwaysOnTop(true, 'screen-saver');
  // Follow the user between workspaces: captions you have to go and find are
  // not captions.
  win.setVisibleOnAllWorkspaces(true, { visibleOnFullScreen: true });

  win.loadFile(join(ROOT, 'src', 'renderer', 'captions.html'));
  win.once('ready-to-show', () => {
    applyClickThrough();
    if (show) win.showInactive();
  });

  win.on('moved', rememberBounds);
  win.on('resized', rememberBounds);
  win.on('closed', () => {
    win = null;
  });

  win.webContents.setWindowOpenHandler(() => ({ action: 'deny' }));
  win.webContents.on('will-navigate', (e) => e.preventDefault());
  return win;
}

export function getCaptionsWindow() {
  return win && !win.isDestroyed() ? win : null;
}

/**
 * Captions on. One entry point for the tray item, the rail button, `--captions`
 * and the settings card, so all four get whichever surface this desktop can
 * actually host — and the same one as each other.
 */
export function showCaptions() {
  if (wantsLayerCaptions() && !layerRefused) {
    layerRestarted = false;
    if (startLayer()) return true;
  }
  createCaptionsWindow({ show: true });
  const w = getCaptionsWindow();
  // showInactive, never show: taking focus would pull the user out of the game
  // the captions are for.
  if (w && !w.isVisible()) w.showInactive();
  return true;
}

export function hideCaptions() {
  stopLayer();
  const w = getCaptionsWindow();
  if (w) w.destroy();
  win = null;
  return true;
}

/** What the tray item and the rail button both call. */
export function toggleCaptions() {
  if (captionsAreOpen()) {
    hideCaptions();
    return false;
  }
  showCaptions();
  return true;
}

export function captionsAreOpen() {
  if (layer) return true;
  const w = getCaptionsWindow();
  return !!w && w.isVisible();
}
