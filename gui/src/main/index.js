// NX Recall GUI — main process. Owns exactly three things: the window, the
// tray, and the one socket connection to recalld that both of them read.
//
// The split matters. DESIGN §8 makes the tray dropdown and the GUI the two
// pause surfaces, which means the tray has to keep working with the window
// closed — so the connection cannot live in the renderer. DESIGN §2 makes
// clients dumb views that survive daemon restarts, so this process holds no
// model of the transcript at all: it relays events and lets the renderer
// rebuild itself on resync.

import { app, BrowserWindow, Tray, Menu, Notification, nativeImage, nativeTheme } from 'electron';
import { fileURLToPath } from 'node:url';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { RecallClient, defaultSocketPath } from './client.js';
import { registerIpc, broadcast } from './ipc.js';

const __dirname = dirname(fileURLToPath(import.meta.url));
const ROOT = join(__dirname, '..', '..');
const TRAY_PNG = join(ROOT, 'assets', 'tray.png');
const TRAY_PAUSED_PNG = join(ROOT, 'assets', 'tray-paused.png');
const WINDOW_ICON = join(ROOT, 'assets', 'icons', '256x256.png');

const STATUS_POLL_MS = 3000;

app.setName('NX Recall');

// No native menu bar: Electron's default one is a grey strip of File/Edit/View
// that belongs to no design system, and this app has no menu commands — the
// rail navigates and the tray quits.
Menu.setApplicationMenu(null);

let win = null;
let tray = null;
let client = null;
let statusTimer = null;
let quitting = false;

// Everything the tray renders. Kept here, not in the renderer, precisely so a
// closed window changes nothing about what the tray can say.
const ui = {
  conn: { status: 'offline', daemon: null, seq: null, error: null, socketPath: defaultSocketPath() },
  paused: false,
  pausePending: false,
  status: null, // last `status` reply/event payload
  update: null, // {from, to} once the daemon has come back as a different version
};

// The daemon replaces itself when the hub drops a new binary under it (0.5.3)
// and reconnects on its own — but this window is still the code that shipped
// with the OLD one. The version string in the first welcome of this app run is
// the baseline; any later welcome that disagrees means an update landed and the
// app is now the stale half. Compared against the FIRST welcome, never against
// this app's own version: the two are allowed to differ (a mock, a side-loaded
// daemon), and only a CHANGE means something happened.
let firstDaemon = null;

function noteDaemonVersion(daemon) {
  if (!daemon) return;
  if (firstDaemon == null) {
    firstDaemon = daemon;
    return;
  }
  if (daemon === firstDaemon || ui.update?.to === daemon) return;
  ui.update = { from: firstDaemon, to: daemon };
  console.log(`[recall] daemon updated: ${firstDaemon} → ${daemon}`);
}

const startMinimized =
  process.argv.includes('--minimized') || process.env.NX_RECALL_START_MINIMIZED === '1';

// ---------------------------------------------------------------------------
// theme
//
// NX Clear ships two grounds (DESIGN §14.1) and the app follows the OS: the
// renderer's whole palette hangs off `prefers-color-scheme`, and setting
// `nativeTheme.themeSource` is what drives that inside Chromium — so forcing a
// theme here is the same code path an OS switch takes, which is exactly why the
// headless suite uses it to photograph both.
//
// NX_RECALL_THEME=light|dark pins it; anything else follows the desktop.
// ---------------------------------------------------------------------------

const THEME_OVERRIDE = ['light', 'dark'].includes(process.env.NX_RECALL_THEME)
  ? process.env.NX_RECALL_THEME
  : null;

// The ground each theme paints (tokens.css --clear-bg). The window has to be
// told separately: it is the colour of the frame between map and first paint,
// and getting it wrong is a white flash on dark or a black one on light.
const GROUND = { light: '#fafafc', dark: '#000000' };
const groundColor = () => (nativeTheme.shouldUseDarkColors ? GROUND.dark : GROUND.light);

// ---------------------------------------------------------------------------
// window
// ---------------------------------------------------------------------------

function createWindow({ show = true } = {}) {
  win = new BrowserWindow({
    width: 1240,
    height: 820,
    minWidth: 900,
    minHeight: 560,
    show: false,
    backgroundColor: groundColor(),
    icon: WINDOW_ICON,
    title: 'NX Recall',
    webPreferences: {
      preload: join(__dirname, 'preload.cjs'),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
    },
  });

  win.loadFile(join(ROOT, 'src', 'renderer', 'index.html'));
  win.once('ready-to-show', () => {
    if (show) win.show();
    pushStateToRenderer();
  });

  // Closing the window hides it: we live in the tray, and capture keeps running
  // whether or not anyone is looking at it.
  win.on('close', (e) => {
    if (quitting) return;
    e.preventDefault();
    win.hide();
  });
  win.on('closed', () => {
    win = null;
  });

  // Nothing in this app should ever open a browser; if a link appears, it is a
  // bug, and a captured transcript is the last thing to hand to a web view.
  win.webContents.setWindowOpenHandler(() => ({ action: 'deny' }));
  win.webContents.on('will-navigate', (e) => e.preventDefault());

  return win;
}

function showWindow() {
  if (!win || win.isDestroyed()) {
    createWindow({ show: true });
    return;
  }
  if (win.isMinimized()) win.restore();
  win.show();
  win.focus();
}

// ---------------------------------------------------------------------------
// tray
// ---------------------------------------------------------------------------

function statusLine() {
  if (ui.conn.status !== 'connected') {
    return ui.conn.status === 'connecting' ? 'Connecting to recalld…' : 'Daemon offline';
  }
  if (ui.paused) return 'Paused — capturing nothing';
  const n = ui.status?.sources_capturing ?? 0;
  if (n === 0) return 'Capturing — no source is live';
  return `Capturing from ${n} source${n === 1 ? '' : 's'}`;
}

function buildTrayMenu() {
  const online = ui.conn.status === 'connected';
  return Menu.buildFromTemplate([
    { label: statusLine(), enabled: false },
    { type: 'separator' },
    {
      // The panic path. It is first in the actionable half of the menu, it is
      // one click deep, and its label always states the CURRENT state so nobody
      // has to read the status line to know what pressing it does.
      id: 'pause',
      label: ui.paused ? 'Resume capture' : 'Pause capture',
      enabled: online && !ui.pausePending,
      click: () => setPaused(!ui.paused),
    },
    { type: 'separator' },
    { label: 'Open NX Recall', click: () => showWindow() },
    { type: 'separator' },
    {
      label: 'Quit NX Recall',
      click: () => {
        quitting = true;
        app.quit();
      },
    },
  ]);
}

function updateTray() {
  if (!tray || tray.isDestroyed()) return;
  try {
    tray.setContextMenu(buildTrayMenu());
    tray.setToolTip(`NX Recall — ${statusLine()}`);
    // A glance at the tray icon answers "is it listening right now?" without
    // opening the menu: the mark goes grey while capture is paused.
    const png = ui.paused ? TRAY_PAUSED_PNG : TRAY_PNG;
    const img = nativeImage.createFromPath(png);
    if (!img.isEmpty()) tray.setImage(img);
  } catch (e) {
    console.warn('[recall] tray update failed:', e.message);
  }
}

function createTray() {
  try {
    const img = nativeImage.createFromPath(TRAY_PNG);
    tray = new Tray(img.isEmpty() ? nativeImage.createEmpty() : img);
    tray.on('click', () => showWindow());
    updateTray();
  } catch (e) {
    tray = null;
    console.warn('[recall] tray unavailable:', e.message);
  }
}

// ---------------------------------------------------------------------------
// the one shared pause path (tray + GUI + e2e all land here)
// ---------------------------------------------------------------------------

export async function setPaused(next) {
  // Instant, per DESIGN §8: the UI flips before the round trip and reconciles
  // after. A pause that waits on a daemon reply to draw is not a panic button.
  ui.paused = !!next;
  ui.pausePending = true;
  updateTray();
  pushStateToRenderer();
  try {
    const res = await client.request(next ? 'pause' : 'resume');
    if (res && typeof res.paused === 'boolean') ui.paused = res.paused;
  } catch (e) {
    ui.paused = !next; // the daemon never took it — do not lie about the state
    console.warn('[recall] pause failed:', e.message);
    broadcast('recall:toast', { kind: 'error', text: `Could not ${next ? 'pause' : 'resume'} capture — ${e.message}` });
  } finally {
    ui.pausePending = false;
    updateTray();
    pushStateToRenderer();
    refreshStatus();
  }
  return ui.paused;
}

/**
 * Restart this app onto the version that is already on disk. Like pause, it is
 * its own channel rather than a protocol request: it is an act on the process,
 * not on the daemon, and the renderer has no business reaching either directly.
 */
export function relaunchApp() {
  quitting = true;
  app.relaunch();
  // exit(), not quit(): quit() runs the close handler that hides the window and
  // keeps the tray alive, and the relaunched copy would then lose the single
  // instance lock race against the copy that refused to die.
  app.exit(0);
}

async function refreshStatus() {
  if (!client || client.status !== 'connected') return;
  try {
    const s = await client.request('status');
    ui.status = s;
    if (typeof s?.paused === 'boolean' && !ui.pausePending) ui.paused = s.paused;
    updateTray();
    pushStateToRenderer();
  } catch {
    /* a failed poll is not news; the connection state already says it */
  }
}

function pushStateToRenderer() {
  broadcast('recall:state', {
    // seq moves with every event, not with connection state, so it is read
    // fresh here rather than from the last `state` emission.
    conn: { ...ui.conn, seq: client?.lastSeq ?? ui.conn.seq },
    paused: ui.paused,
    pausePending: ui.pausePending,
    status: ui.status,
    update: ui.update,
  });
}

// ---------------------------------------------------------------------------
// daemon connection
// ---------------------------------------------------------------------------

function startClient() {
  client = new RecallClient({ socketPath: defaultSocketPath() });

  client.on('state', (st) => {
    ui.conn = st;
    if (st.status === 'connected') noteDaemonVersion(st.daemon);
    if (st.status !== 'connected') {
      ui.status = null;
      if (statusTimer) clearInterval(statusTimer);
      statusTimer = null;
    } else if (!statusTimer) {
      statusTimer = setInterval(refreshStatus, STATUS_POLL_MS);
      if (statusTimer.unref) statusTimer.unref();
      refreshStatus();
    }
    updateTray();
    pushStateToRenderer();
  });

  client.on('event', (evt) => {
    // The tray cares about exactly one event type; everything else is the
    // renderer's business and is relayed untouched.
    if (evt.ev === 'status' && evt.data && typeof evt.data.paused === 'boolean') {
      ui.status = evt.data;
      if (!ui.pausePending) ui.paused = evt.data.paused;
      updateTray();
    }
    broadcast('recall:event', evt);
    if (evt.ev === 'status') pushStateToRenderer();
  });

  client.on('resync', (info) => {
    // "Everything you are showing may be stale" — the renderer re-runs its
    // queries. Nothing here tries to patch the gap (PROTOCOL, Handshake).
    broadcast('recall:resync', info);
    refreshStatus();
  });

  client.on('caughtup', (info) => broadcast('recall:caughtup', info));
  client.on('warn', (msg) => console.warn('[recall]', msg));

  client.connect();
}

// ---------------------------------------------------------------------------
// 0.9.0 — reminders
// ---------------------------------------------------------------------------

/**
 * Raise an OS notification for a note that has come due.
 *
 * This is the only thing this app does that reaches outside its own window, and
 * it is deliberately the narrowest thing that could: one notification, for one
 * note, that the user asked for out loud. There is no notification for a
 * commitment, for a digest, for a new segment or for anything the daemon
 * inferred — the Memory view's own copy still says "nothing here reminds you,
 * notifies you, or acts on its own", and that stays true because a note is not
 * something the app noticed, it is something you dictated.
 *
 * Clicking it brings the window up and hands the renderer the note id, so the
 * click lands on the row rather than on the app. Returns false when the desktop
 * has no notification service at all, which is a normal state and not an error:
 * the renderer's toast has already said the same thing inside the window.
 */
function raiseReminder({ noteId, title, body }) {
  if (!Notification.isSupported()) return false;
  try {
    const n = new Notification({
      title,
      body,
      // Quiet. A reminder is not an alarm, and a sound is the thing that makes
      // people turn a feature off.
      silent: true,
    });
    n.on('click', () => {
      showWindow();
      broadcast('recall:openNote', { noteId });
    });
    n.show();
    return true;
  } catch (e) {
    console.warn('[recall] could not raise a reminder notification', e);
    return false;
  }
}

// ---------------------------------------------------------------------------
// lifecycle
// ---------------------------------------------------------------------------

async function bootstrap() {
  // Before the first window: themeSource is what makes the renderer's
  // `prefers-color-scheme` report, and the window's own backgroundColor is read
  // at construction.
  nativeTheme.themeSource = THEME_OVERRIDE ?? 'system';
  nativeTheme.on('updated', () => {
    if (win && !win.isDestroyed()) win.setBackgroundColor(groundColor());
  });

  registerIpc({
    request: (method, params) => client.request(method, params),
    setPaused,
    getState: () => ({
      conn: ui.conn,
      paused: ui.paused,
      pausePending: ui.pausePending,
      status: ui.status,
      update: ui.update,
    }),
    showWindow,
    relaunch: relaunchApp,
    // 0.9.0: a reminder that has come round. See `raiseReminder`.
    notify: raiseReminder,
  });

  startClient();
  createTray();
  createWindow({ show: !startMinimized });
  if (startMinimized) console.log('[recall] started to the tray');

  if (process.env.NX_RECALL_E2E === '1') {
    const { runE2E } = await import('./e2e.js');
    runE2E({
      getWindow: () => win,
      getUi: () => ui,
      setPaused,
      buildTrayMenu,
      statusLine,
      showWindow,
      theme: () => ({
        forced: THEME_OVERRIDE,
        dark: nativeTheme.shouldUseDarkColors,
        ground: groundColor(),
      }),
      quit: () => {
        quitting = true;
        app.quit();
      },
    });
  }
}

// One GUI per machine: a second copy would open a second socket connection and
// a second tray icon, and the person double-clicking the icon wants the window
// they already have.
//
// Exception: a test-driven instance (NX_RECALL_E2E is set, even to 0). The lock
// lives in userData, so pointing userData at a scratch dir both frees the lock
// and keeps test state out of the real profile — without it, the headless suite
// silently exits whenever the installed app is running, which is exactly when a
// developer is most likely to run it.
if (process.env.NX_RECALL_E2E !== undefined) {
  app.setPath('userData', join(tmpdir(), `nx-recall-e2e-${process.pid}`));
}
if (!app.requestSingleInstanceLock()) {
  app.quit();
} else {
  app.on('second-instance', () => showWindow());
  app.whenReady().then(bootstrap);
}

app.on('activate', () => showWindow());

// Closing the last window must NOT quit — the tray is the app.
app.on('window-all-closed', () => {});

app.on('before-quit', () => {
  quitting = true;
});

app.on('will-quit', () => {
  if (statusTimer) clearInterval(statusTimer);
  if (client) client.close();
  if (tray && !tray.isDestroyed()) tray.destroy();
});

// Belt and braces: no web contents in this app may open an external target.
app.on('web-contents-created', (_e, contents) => {
  contents.setWindowOpenHandler(() => ({ action: 'deny' }));
});
