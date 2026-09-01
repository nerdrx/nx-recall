// The renderer's whole view of the world. Two channels in (a protocol request,
// a pause), four out (state, event, resync, toast) — the renderer never learns
// the socket path and never frames a byte itself.

import { ipcMain, BrowserWindow } from 'electron';

// Only the methods the views actually call. An allowlist rather than a
// pass-through because the renderer sits behind a preload bridge and there is
// no reason for a compromised page to be able to reach `delete.run` for a
// speaker no view offered.
const ALLOWED = new Set([
  'sources.list',
  'sources.set',
  // The microphone's own switch. Deliberately a separate pair rather than an
  // extra shape on `sources.set`: the daemon refuses the `mic` key there, and
  // the UI must be able to reach the method that actually works.
  'mic.get',
  'mic.set',
  'speakers.list',
  'speakers.name',
  // Which languages a voice speaks, and the sweep for voices that are not
  // people at all. Both are 0.6.1 and both are reachable from the speakers
  // view, so both belong here — a method the UI calls and this set forgets is
  // a dead button.
  'speakers.set_languages',
  'speakers.prune',
  // Delete one voice, with DESIGN §8's choice as a parameter (0.6.4). It is the
  // only delete the ⋯ menu reaches now: `delete.run` is scoped by SEGMENTS, so
  // it silently did nothing for a voice that had none left, and it never
  // touched the voiceprint at all.
  'speakers.delete',
  'speakers.merge',
  'speakers.split',
  // Reads only, both of them: which clips identify a voice, and one clip's
  // bytes so it can be played (docs/PROTOCOL.md, "Voice preview").
  'speakers.sample',
  // The memory graph's Tier 1 (0.6.2, docs/GRAPH.md). Two reads: everything the
  // person page shows about one voice, and one conversation's segments so the
  // transcript can land on it. Both are additive and both are read-only.
  'person.get',
  'thread.get',
  'segments.audio',
  'segments.reassign',
  'segments.correct',
  'search',
  // Semantic search (0.6.5). Read-only, same facets, and the Search
  // view's mode toggle calls it directly — a method the UI offers and
  // this set forgets is a dead button.
  'search.semantic',
  'transcript',
  'delete.preview',
  'delete.run',
  'status',
]);

export function broadcast(channel, payload) {
  for (const w of BrowserWindow.getAllWindows()) {
    if (!w.isDestroyed()) w.webContents.send(channel, payload);
  }
}

export function registerIpc({ request, setPaused, getState, showWindow, relaunch }) {
  ipcMain.handle('recall:request', async (_e, method, params) => {
    if (!ALLOWED.has(method)) return { ok: false, err: { code: 'refused', msg: `method ${method} is not exposed to the UI` } };
    try {
      return { ok: true, data: await request(method, params ?? {}) };
    } catch (e) {
      return { ok: false, err: { code: e?.code ?? 'failed', msg: e?.message ?? String(e) } };
    }
  });

  // Pause is deliberately NOT a plain request: tray and window must agree on
  // one optimistic state machine, which lives in the main process.
  ipcMain.handle('recall:setPaused', async (_e, next) => ({ paused: await setPaused(!!next) }));

  ipcMain.handle('recall:getState', () => getState());

  ipcMain.handle('recall:show', () => {
    showWindow();
    return true;
  });

  // Also not a protocol request: restarting the app is an act on this process,
  // and the update banner is the one surface allowed to ask for it.
  ipcMain.handle('recall:relaunch', () => {
    relaunch();
    return true;
  });
}
