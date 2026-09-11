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
  // Per-person highlights: a palette token and a small icon, and the list of
  // tokens the daemon will accept. The read is here as well as the write for
  // the same reason `assist.get` is — the picker builds its swatches out of
  // what the daemon offers rather than out of a list of its own, so a daemon
  // that grows an eleventh colour grows an eleventh swatch without a GUI
  // release. A method the UI calls and this set forgets is a dead button.
  'speakers.set',
  'speakers.palette',
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
  // Conversation replay (0.9.2). A read, and a deliberately thinner one than
  // `thread.get`: the turns of a conversation with a per-turn "can this
  // actually be heard right now", so the player draws its scrubber without
  // fetching a single WAV. The audio itself still comes through
  // `segments.audio`, which is already here.
  'replay.get',
  // The memory graph's Tiers 2 and 3 (0.7.0, docs/GRAPH.md). Everything the
  // Memory view reaches: three reads, one state machine, and the Tier 3
  // switch. `graph.set` is here as well as `graph.enrich` because the card
  // offers both a switch and, in future, the thread count — a method the UI
  // calls and this set forgets is a dead button.
  'graph.summary',
  'graph.get',
  'graph.set',
  'graph.enrich',
  'commitments.list',
  'commitments.set_state',
  'topics.list',
  // The accuracy round (0.8.0). Every one of these is reachable from a control
  // the UI actually offers, which is the only bar this set has: the vocabulary
  // panel reads and writes the glossary, the dashboard reads the summary, the
  // notes section reads and moves notes, the query box asks a question in
  // words, and a roster join asks for one person's brief.
  'vocab.get',
  'vocab.set',
  'accuracy.summary',
  'saved.searches.list',
  'saved.searches.save',
  'saved.searches.delete',
  'saved.moments.list',
  'saved.moments.save',
  'saved.moments.delete',
  'saved.moments.get',
  'saved.moments.move',
  'saved.collections.list',
  'saved.collections.save',
  'saved.collections.delete',
  'history.page',
  'performance.get',
  'saved.moments.related',
  'review.list',
  'review.get',
  'review.mark',
  'segments.context',
  'notes.list',
  'notes.set_state',
  'search.ask',
  // Grounded answers (0.11.0). The same box, when what was typed is a
  // question: it returns everything `search.ask` does plus one sentence, or
  // one honest refusal. A read — it writes nothing.
  'search.answer',
  'person.brief',
  // The assistant round (0.9.0). One read: the Memory view's "Yesterday" card.
  // Reminders need no new method — a reminder is a note with a date, so they
  // ride on `notes.list` and `notes.set_state`, both already here — and a
  // translation rides on the segment.
  'digest.list',
  // The translation controls (0.10.2). Two, and both are reachable from the
  // Memory view's Translation card — a read, because the card builds its
  // selector out of the languages the daemon will accept rather than a list of
  // its own, and a write, because three controls that need a restart are not
  // controls. A method the UI calls and this set forgets is a card that
  // renders empty and says nothing about why, which is exactly what happened
  // the first time this was left out.
  'assist.get',
  'assist.set',
  // The mood pass's own switch (0.12.5), reachable from the "How it sounded"
  // card's "Listen at night" toggle. A write that needs a restart is not a
  // control, the same argument `graph.set`'s entry above makes.
  'mood.get',
  'mood.set',
  // Light mode (0.13.x), reachable from the Listening card's "Lighter while a
  // game runs" control. The read rides on `status.asr.light_mode` like every
  // other switch here — a write that needs a restart is not a control.
  'asr.light.set',
  // Worlds and turn-taking (0.10.0). Two reads: the Memory view's Worlds card
  // and the person page's "How you talk". A method the UI calls and this set
  // forgets is a card that never renders and says nothing about why.
  'worlds.list',
  'person.stats',
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
  // ---- 0.10.0 -------------------------------------------------------------
  // The room microphone's own switch and the device list it needs. Same
  // argument as `mic.set`: the daemon refuses the `room` key on `sources.set`,
  // so the UI must be able to reach the method that works.
  'room.get',
  'room.set',
  'devices.list',
  // The local Markdown export. Both are reachable from the Export card, and
  // neither can reach anywhere but the folder the user picked in the dialog
  // below — the daemon refuses a path that is not an absolute local one.
  'export.preview',
  'export.run',
  // A backup you can trust (0.13.0). Reachable from the Backup card's choose
  // folder / back up now / verify buttons and its schedule switch — none of
  // it can reach anywhere but the folder the user picked in the dialog below,
  // the same rule the export methods above already keep.
  'backup.create',
  'backup.verify',
  'backup.restore',
  'backup.get',
  'backup.set',
  // The Discord ground-truth bridge (0.9.0), surfaced in the Sources view.
  // Four reads and two writes, all of them behind controls the card offers.
  'truth.status',
  'truth.users',
  'truth.summary',
  'truth.link',
  'truth.unlink',
  // ---- end 0.10.0 ---------------------------------------------------------
  // 0.12.2. Which Discord client carries the RecallBridge plugin, which is a
  // consent-shaped decision the card offers and therefore has to be reachable:
  // a control the UI draws and this set forgets is a dead button, and this one
  // decides whether a whole second call gets recorded.
  'sources.instance_role',
  'status',
]);

export function broadcast(channel, payload) {
  for (const w of BrowserWindow.getAllWindows()) {
    if (!w.isDestroyed()) w.webContents.send(channel, payload);
  }
}

/// The longest text a reminder notification may carry. A note is a sentence
/// somebody dictated; past this it is not a notification, it is a document, and
/// the renderer's own toast is where the whole thing is readable anyway.
const MAX_NOTIFY = 220;

export function registerIpc({ request, setPaused, getState, showWindow, relaunch, captions, notify, chooseFolder, openFolder, chooseBackupFolder, openBackupFolder }) {
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

  // Live captions (0.8.3). Also not protocol requests, for the same reason
  // pause is not: every one of them is an act on a WINDOW — opening one,
  // making it click-through, remembering how big it is — and the daemon has
  // no opinion about any of it. The tray item, the rail button and
  // `nx-recall --captions` all land on the same three functions, so the three
  // surfaces can never disagree about whether the window is up.
  ipcMain.handle('recall:captions:get', () => captions.get());
  ipcMain.handle('recall:captions:set', (_e, patch) => captions.set(patch ?? {}));
  ipcMain.handle('recall:captions:open', () => captions.open());
  ipcMain.handle('recall:captions:close', () => captions.close());
  ipcMain.handle('recall:captions:toggle', () => captions.toggle());
  ipcMain.handle('recall:captions:state', () => ({ open: captions.isOpen(), settings: captions.get() }));
  // ---- 0.9.0, the assistant ----------------------------------------------
  // An OS notification for a reminder that has come round. Not a protocol
  // method, so it is not in ALLOWED above — but it IS the only way the
  // renderer can reach outside the window, so the shape it may send is fixed
  // here rather than trusted: a note id, and two strings that are truncated.
  // A page that got compromised can raise a notification about a note; it
  // cannot raise one about anything else, at any length, with any action on it.
  ipcMain.handle('recall:notify', (_e, payload) => {
    const noteId = Number(payload?.noteId);
    if (!Number.isInteger(noteId)) return false;
    const clip = (s) => String(s ?? '').slice(0, MAX_NOTIFY);
    return notify({
      noteId,
      title: clip(payload?.title) || 'Reminder',
      body: clip(payload?.body),
    });
  });
  // ---- end 0.9.0 -----------------------------------------------------------

  // ---- 0.10.0, the local Markdown export -----------------------------------
  //
  // Two acts on this process, not on the daemon, which is why neither is a
  // protocol request: opening the OS folder chooser, and revealing a folder in
  // the file manager afterwards.
  //
  // The renderer cannot name a directory itself. It asks for the dialog, the
  // person picks a folder, and the path comes back — so the only directory the
  // export can ever be pointed at is one somebody chose in a native dialog.
  // `openFolder` then only accepts a path the user picked in THIS session, so a
  // compromised page cannot use it as a general "open anything" primitive.
  ipcMain.handle('recall:export:chooseFolder', () => chooseFolder());
  ipcMain.handle('recall:export:openFolder', (_e, dir) => openFolder(String(dir ?? '')));
  // ---- end 0.10.0 ----------------------------------------------------------

  // ---- 0.13.0, a backup you can trust ---------------------------------------
  // Same reasoning as the export folder pair above: the renderer cannot name a
  // directory itself, only ask for the native dialog and get back what was
  // picked in it.
  ipcMain.handle('recall:backup:chooseFolder', () => chooseBackupFolder());
  ipcMain.handle('recall:backup:openFolder', (_e, dir) => openBackupFolder(String(dir ?? '')));
  // ---- end 0.13.0 ------------------------------------------------------------
}
