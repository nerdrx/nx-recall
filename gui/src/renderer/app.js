// NX Recall renderer — the controller. Owns view switching, the footer, the
// pause button, and the single path every daemon event takes into the model.
//
// It holds no socket and no protocol knowledge: the main process relays
// {state, event, resync} and this folds them into store.js. On `resync` the
// model is discarded and rebuilt from queries, which is the entire strategy for
// surviving a daemon restart (DESIGN §2).

import { h, clear } from './lib/dom.js';
import { store, applyEvent, reloadAll, mergeSegments, ask } from './lib/store.js';
import { patchSpeakerLabels } from './lib/labels.js';
import { toast } from './lib/sheets.js';
import { stop as stopPreview, playbackState } from './lib/preview.js';
import * as transcriptView from './views/transcript.js';
import * as speakersView from './views/speakers.js';
import * as searchView from './views/search.js';
import * as sourcesView from './views/sources.js';

const VIEWS = {
  transcript: transcriptView,
  speakers: speakersView,
  search: searchView,
  sources: sourcesView,
};

const main = document.getElementById('main');
const footer = document.getElementById('footer');
const pauseBtn = document.getElementById('pause-btn');
const pauseLabel = document.getElementById('pause-label');
const pauseIco = document.getElementById('pause-ico');
const pauseHint = document.getElementById('pause-hint');

let currentName = 'transcript';
let current = null;

const ctx = {
  go,
  jumpToSegment,
  showSpeakerInTranscript,
  toast,
};

// ---------------------------------------------------------------------------
// views
// ---------------------------------------------------------------------------

function go(name) {
  if (!VIEWS[name]) return;
  // Leaving a view takes its stop button off screen, so it takes the sound too.
  stopPreview();
  // …and any menu it left hanging over a row that is about to stop existing.
  current?.closeMenu?.();
  currentName = name;
  for (const btn of document.querySelectorAll('.rail-item')) {
    btn.setAttribute('aria-selected', String(btn.dataset.view === name));
  }
  clear(main);
  current = VIEWS[name].mount(main, ctx);
  document.body.dataset.view = name;
}

for (const btn of document.querySelectorAll('.rail-item')) {
  btn.addEventListener('click', () => go(btn.dataset.view));
}

/** A search hit → the conversation it sits in. */
async function jumpToSegment(seg) {
  if (!store.segById.has(seg.id)) {
    // Older than the live window: pull two minutes either side of the moment.
    try {
      const res = await ask('transcript', {
        from: new Date(seg.t_ms - 120000).toISOString(),
        to: new Date(seg.t_ms + 120000).toISOString(),
        limit: 200,
      });
      mergeSegments(res.segments ?? []);
    } catch (e) {
      toast(`Could not load that part of the transcript — ${e.message}`, 'error');
      return;
    }
  }
  go('transcript');
  current?.focusSegment?.(seg.id);
}

/** A voice → what they have been saying. Same filter the header already has. */
function showSpeakerInTranscript(spId) {
  go('transcript');
  return current?.focusSpeaker?.(spId) ?? null;
}

// ---------------------------------------------------------------------------
// pause — surface #2 (DESIGN §8). The tray dropdown is #1 and both call the
// same main-process state machine, so they can never disagree.
// ---------------------------------------------------------------------------

const ICON_PAUSE = '<rect x="6" y="5" width="4" height="14"></rect><rect x="14" y="5" width="4" height="14"></rect>';
const ICON_RESUME = '<path d="M7 4l12 8-12 8z"></path>';

function renderPause() {
  const online = store.conn.status === 'connected';
  pauseBtn.dataset.paused = String(store.paused);
  pauseBtn.disabled = !online || store.pausePending;
  pauseLabel.textContent = store.paused ? 'Resume capture' : 'Pause capture';
  pauseIco.innerHTML = store.paused ? ICON_RESUME : ICON_PAUSE;
  pauseHint.textContent = !online
    ? 'The daemon is not answering, so capture cannot be changed from here.'
    : store.paused
      ? 'Capture is stopped. Nothing is being transcribed or written.'
      : 'Capture stops immediately. Nothing is written while paused.';
}

pauseBtn.addEventListener('click', async () => {
  const next = !store.paused;
  // Reflect instantly — the main process is already optimistic, and a panic
  // button that waits for a round trip is not a panic button.
  store.paused = next;
  store.pausePending = true;
  renderPause();
  current?.refreshLiveChip?.();
  await window.recall.setPaused(next);
});

// ---------------------------------------------------------------------------
// the update banner
//
// DESIGN §2 makes clients dumb views that survive a daemon restart — which is
// precisely how this window can end up being the OLD half of the app, talking
// happily to a daemon the hub replaced under it, and never mention it. The main
// process watches the version string across welcomes; this says so, once, above
// every view, and offers the one action that fixes it.
// ---------------------------------------------------------------------------

const updateBar = document.getElementById('update-bar');
let dismissedUpdate = null; // the version the user already waved away

// "recalld/0.5.5" → "0.5.5". Anything without a slash is shown as it came.
function versionOf(daemon) {
  const s = String(daemon ?? '');
  const at = s.lastIndexOf('/');
  return at >= 0 ? s.slice(at + 1) : s;
}

function requestRestart() {
  return window.recall.relaunch();
}

function renderUpdateBar() {
  const up = store.update;
  clear(updateBar);
  const show = !!up?.to && up.to !== dismissedUpdate;
  updateBar.hidden = !show;
  if (!show) return;

  const restart = h('button', { class: 'btn small primary', id: 'update-restart' }, 'Restart');
  // Assigned as a property rather than added as a listener, on purpose: the
  // headless driver has to prove this button is wired WITHOUT pressing it —
  // relaunching mid-run would take the window out from under the driver.
  restart.onclick = requestRestart;

  updateBar.append(
    h('span', { class: 'dot' }),
    h('span', {
      class: 'update-text',
      id: 'update-text',
      text: `NX Recall was updated to ${versionOf(up.to)} — restart the app to finish`,
      title: `${up.from} → ${up.to}`,
    }),
    h('span', { class: 'spacer' }),
    restart,
    h(
      'button',
      {
        class: 'btn small',
        id: 'update-dismiss',
        'aria-label': 'Dismiss the update notice',
        title: 'Dismiss — the app keeps working, it is just the older half',
        onclick: () => {
          dismissedUpdate = up.to;
          renderUpdateBar();
        },
      },
      '✕'
    )
  );
}

// ---------------------------------------------------------------------------
// footer
// ---------------------------------------------------------------------------

function renderFooter() {
  clear(footer);
  const st = store.conn;
  const cls = st.status === 'connected' ? 'ok' : st.status === 'connecting' ? 'mid' : 'bad';
  const text =
    st.status === 'connected'
      ? `connected · ${st.daemon ?? 'recalld'}`
      : st.status === 'connecting'
        ? 'connecting…'
        : 'daemon offline — retrying';

  // Element.append() stringifies null, so every optional child is filtered out
  // rather than passed through — a literal "null" in the status bar is exactly
  // the kind of thing a screenshot review catches and a diff does not.
  const parts = [
    h('span', { class: `stat conn ${cls}` }, h('span', { class: `dot${st.status === 'connected' && !store.paused ? ' pulse' : ''}` }), text),
    h('span', { class: 'stat' }, 'seq ', h('b', { text: String(st.seq ?? '—') })),
    h('span', { class: 'stat' }, 'queue ', h('b', { text: String(store.status?.queue_depth ?? '—') })),
    h('span', { class: 'stat' }, 'drops ', h('b', { text: String(store.status?.drops ?? '—') })),
    h('span', { class: 'stat' }, 'sources ', h('b', { text: String(store.status?.sources_capturing ?? 0) })),
    store.paused ? h('span', { class: 'chip warn' }, h('span', { class: 'dot' }), 'capture paused') : null,
    h('span', { class: 'spacer' }),
  ];
  footer.append(...parts.filter(Boolean));

  for (const [opId, op] of store.ops) {
    footer.append(
      h(
        'span',
        { class: 'stat' },
        `${op.kind ?? 'working'} `,
        h('span', { class: 'op-bar' }, h('i', { style: `transform:scaleX(${Math.max(0.02, op.frac ?? 0)})` })),
        h('b', { text: `${Math.round((op.frac ?? 0) * 100)}%` })
      )
    );
    void opId;
  }

  footer.append(h('span', { class: 'stat', style: 'font-family:var(--mono);opacity:.7' }, st.socketPath ?? ''));
}

// The rail counts belong to the model, not to whichever view happens to be
// mounted — otherwise "Speakers 0" sits there until you visit Speakers.
function renderBadges() {
  const set = (id, v) => {
    const el = document.getElementById(id);
    if (el) el.textContent = String(v);
  };
  set('badge-transcript', store.segments.length);
  set('badge-speakers', store.speakers.size);
  set('badge-sources', store.sources.filter((s) => s.allowed).length);
}

// ---------------------------------------------------------------------------
// daemon wiring
// ---------------------------------------------------------------------------

window.recall.onState((st) => {
  store.conn = st.conn ?? store.conn;
  store.paused = !!st.paused;
  store.pausePending = !!st.pausePending;
  if (st.status) store.status = st.status;
  store.update = st.update ?? null;
  renderPause();
  renderUpdateBar();
  renderFooter();
  current?.update?.({ status: true, conn: true });
  current?.refreshLiveChip?.();
});

window.recall.onEvent((evt) => {
  if (typeof evt?.seq === 'number') store.conn.seq = evt.seq;
  const change = applyEvent(evt, {
    onSpeakersChanged() {
      renderBadges();
      current?.update?.({ speakers: true });
    },
  });
  if (!change) {
    renderFooter();
    return;
  }
  renderBadges();
  // One path, every view: a rename made here, in the CLI, or in another client
  // all arrive as the same broadcast and repaint the same way.
  if (change.relabel) patchSpeakerLabels(change.relabel);
  current?.update?.(change);
  renderFooter();
  if (change.opFinished) {
    const d = change.opFinished;
    toast(
      d.failed
        ? `${d.kind ?? 'Operation'} failed — ${d.msg ?? 'no reason given'}`
        : `${d.kind ?? 'Operation'} finished${d.removed != null ? ` — ${d.removed} segments deleted` : ''}.`,
      d.failed ? 'error' : 'ok'
    );
  }
});

window.recall.onResync(async (info) => {
  // Everything on screen may be stale. Rebuild from queries and remount.
  await reloadAll().catch(() => {});
  go(currentName);
  renderFooter();
  renderBadges();
  if (info?.reason && info.reason !== 'first-connect') {
    toast(
      info.reason === 'daemon-restart'
        ? 'recalld restarted — the views were reloaded.'
        : 'Reconnected after a gap — the views were reloaded.',
      ''
    );
  }
});

window.recall.onCaughtUp((info) => {
  if (info?.replayed) toast(`Caught up on ${info.replayed} missed event${info.replayed === 1 ? '' : 's'}.`, '');
});

window.recall.onToast((t) => toast(t.text, t.kind));

// ---------------------------------------------------------------------------
// keyboard
// ---------------------------------------------------------------------------

document.addEventListener('keydown', (e) => {
  if (e.target.matches?.('input, textarea, select')) return;
  if ((e.ctrlKey || e.metaKey) && e.key === 'f') {
    e.preventDefault();
    go('search');
    current?.focusQuery?.();
    return;
  }
  const map = { 1: 'transcript', 2: 'speakers', 3: 'search', 4: 'sources' };
  if ((e.ctrlKey || e.altKey) && map[e.key]) {
    e.preventDefault();
    go(map[e.key]);
  }
});

// ---------------------------------------------------------------------------
// boot
// ---------------------------------------------------------------------------

(async function boot() {
  const st = await window.recall.getState();
  store.conn = st.conn ?? store.conn;
  store.paused = !!st.paused;
  store.status = st.status ?? null;
  store.update = st.update ?? null;
  renderPause();
  renderUpdateBar();
  renderFooter();
  await reloadAll().catch(() => {});
  go('transcript');
  renderFooter();
  renderBadges();

  // Exposed for the headless driver only (scripts/headless_test.sh). It reads
  // and clicks the real DOM; this is just a handle onto the same model the UI
  // renders, so a passing check means the UI itself is right.
  window.__recallDebug = {
    store,
    go,
    view: () => currentName,
    counts: () => ({
      segments: store.segments.length,
      rows: document.querySelectorAll('#seg-list .seg').length,
      speakers: store.speakers.size,
      sources: store.sources.length,
    }),
    // The update banner, and whether its Restart really goes anywhere. The
    // driver may not press it — a relaunch would end the run — so the wiring is
    // read instead: the button's own handler, and the bridge it calls.
    update: () => {
      const btn = document.getElementById('update-restart');
      return {
        shown: !updateBar.hidden,
        version: store.update?.to ?? null,
        text: (document.getElementById('update-text') || {}).textContent ?? '',
        restart: {
          present: !!btn,
          wired: !!btn && btn.onclick === requestRestart,
          ipc: typeof window.recall.relaunch === 'function',
        },
      };
    },
    // Voice preview: the driver asserts against the real <audio> element, not
    // against the UI's opinion of it.
    audio: () => {
      const a = window.__recallAudio ?? null;
      return {
        ...playbackState(),
        exists: !!a,
        paused: a ? a.paused : null,
        ended: a ? a.ended : null,
        currentTime: a ? a.currentTime : 0,
        readyState: a ? a.readyState : 0,
        error: a?.error ? a.error.code : null,
      };
    },
  };
})();
