// CommonJS on purpose: the package is "type": "module", and an Electron
// preload running in a sandboxed context is loaded as CJS.
"use strict";

const { contextBridge, ipcRenderer } = require("electron");

const listeners = new Map();

function on(channel, fn) {
  if (!listeners.has(channel)) {
    listeners.set(channel, new Set());
    ipcRenderer.on(channel, (_e, payload) => {
      for (const cb of listeners.get(channel)) {
        try {
          cb(payload);
        } catch (e) {
          console.error("[recall] listener for", channel, "threw", e);
        }
      }
    });
  }
  listeners.get(channel).add(fn);
  return () => listeners.get(channel).delete(fn);
}

contextBridge.exposeInMainWorld("recall", {
  // request(method, params) → {ok:true,data} | {ok:false,err:{code,msg}}
  // Never throws: every view renders an error state rather than dying.
  request: (method, params) => ipcRenderer.invoke("recall:request", method, params),
  setPaused: (next) => ipcRenderer.invoke("recall:setPaused", next),
  getState: () => ipcRenderer.invoke("recall:getState"),
  show: () => ipcRenderer.invoke("recall:show"),
  relaunch: () => ipcRenderer.invoke("recall:relaunch"),
  // 0.9.0: a reminder that has come round. Main raises the OS notification —
  // the renderer never touches the Notification API itself.
  notify: (payload) => ipcRenderer.invoke("recall:notify", payload),

  // Live captions (0.8.3). Its own namespace rather than five more top-level
  // keys: the captions window is a second surface with its own lifecycle, and
  // the bridge should read as one. Every one of these is an act on a WINDOW,
  // not on the daemon, which is exactly why none of them is a `request`.
  captions: {
    get: () => ipcRenderer.invoke("recall:captions:get"),
    set: (patch) => ipcRenderer.invoke("recall:captions:set", patch),
    open: () => ipcRenderer.invoke("recall:captions:open"),
    close: () => ipcRenderer.invoke("recall:captions:close"),
    toggle: () => ipcRenderer.invoke("recall:captions:toggle"),
    state: () => ipcRenderer.invoke("recall:captions:state"),
  },

  // 0.10.0: the local Markdown export. Not protocol requests — one opens the
  // OS folder chooser, the other reveals a folder that was chosen with it —
  // and between them they are the whole of the export's contact with the world
  // outside this window (DESIGN §12: files on this disk, nothing else).
  exportFolder: {
    choose: () => ipcRenderer.invoke("recall:export:chooseFolder"),
    open: (dir) => ipcRenderer.invoke("recall:export:openFolder", dir),
  },

  // 0.13.0: a backup you can trust. Same two-channel shape as the export
  // folder above, for the same reason — everything else the feature does is
  // ordinary protocol requests (backup.create/verify/restore/get/set).
  backupFolder: {
    choose: () => ipcRenderer.invoke("recall:backup:chooseFolder"),
    open: (dir) => ipcRenderer.invoke("recall:backup:openFolder", dir),
  },

  onState: (fn) => on("recall:state", fn),
  onEvent: (fn) => on("recall:event", fn),
  onResync: (fn) => on("recall:resync", fn),
  onCaughtUp: (fn) => on("recall:caughtup", fn),
  onToast: (fn) => on("recall:toast", fn),
  // Both windows listen: the settings card so a change made in the captions
  // window's own context menu shows up in it, and the captions window so a
  // slider moved in the settings card is live before it is let go.
  onCaptionSettings: (fn) => on("recall:captions:settings", fn),
  // Somebody clicked a reminder notification, outside the window.
  onOpenNote: (fn) => on("recall:openNote", fn),
});
