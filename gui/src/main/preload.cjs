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

  onState: (fn) => on("recall:state", fn),
  onEvent: (fn) => on("recall:event", fn),
  onResync: (fn) => on("recall:resync", fn),
  onCaughtUp: (fn) => on("recall:caughtup", fn),
  onToast: (fn) => on("recall:toast", fn),
  // Somebody clicked a reminder notification, outside the window.
  onOpenNote: (fn) => on("recall:openNote", fn),
});
