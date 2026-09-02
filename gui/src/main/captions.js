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
import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
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

export function getCaptionSettings() {
  if (!settings) settings = normalizeCaptionSettings(null);
  return { ...settings };
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
      writeFileSync(settingsFile, JSON.stringify(settings, null, 2));
    } catch (e) {
      console.warn('[recall] could not save the captions settings:', e.message);
    }
  }, 400);
  if (saveTimer.unref) saveTimer.unref();
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

export function showCaptions() {
  createCaptionsWindow({ show: true });
  const w = getCaptionsWindow();
  // showInactive, never show: taking focus would pull the user out of the game
  // the captions are for.
  if (w && !w.isVisible()) w.showInactive();
  return true;
}

export function hideCaptions() {
  const w = getCaptionsWindow();
  if (w) w.destroy();
  win = null;
  return true;
}

/** What the tray item and the rail button both call. */
export function toggleCaptions() {
  const w = getCaptionsWindow();
  if (w && w.isVisible()) {
    hideCaptions();
    return false;
  }
  showCaptions();
  return true;
}

export function captionsAreOpen() {
  const w = getCaptionsWindow();
  return !!w && w.isVisible();
}
