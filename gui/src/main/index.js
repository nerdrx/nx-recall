// NX Recall GUI — main process. Owns exactly three things: the window, the
// tray, and the one socket connection to recalld that both of them read.
//
// The split matters. DESIGN §8 makes the tray dropdown and the GUI the two
// pause surfaces, which means the tray has to keep working with the window
// closed — so the connection cannot live in the renderer. DESIGN §2 makes
// clients dumb views that survive daemon restarts, so this process holds no
// model of the transcript at all: it relays events and lets the renderer
// rebuild itself on resync.

import { app, BrowserWindow, Tray, Menu, nativeImage } from 'electron';
import { fileURLToPath } from 'node:url';
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
};

const startMinimized =
  process.argv.includes('--minimized') || process.env.NX_RECALL_START_MINIMIZED === '1';

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
    // The ground is true black (DESIGN §13) — matching it here kills the white
    // flash between window map and first paint.
    backgroundColor: '#000000',
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
  });
}

// ---------------------------------------------------------------------------
// daemon connection
// ---------------------------------------------------------------------------

function startClient() {
  client = new RecallClient({ socketPath: defaultSocketPath() });

  client.on('state', (st) => {
    ui.conn = st;
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
// lifecycle
// ---------------------------------------------------------------------------

async function bootstrap() {
  registerIpc({
    request: (method, params) => client.request(method, params),
    setPaused,
    getState: () => ({ conn: ui.conn, paused: ui.paused, pausePending: ui.pausePending, status: ui.status }),
    showWindow,
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
