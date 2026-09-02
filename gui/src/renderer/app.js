// NX Recall renderer — the controller. Owns view switching, the footer, the
// pause button, and the single path every daemon event takes into the model.
//
// It holds no socket and no protocol knowledge: the main process relays
// {state, event, resync} and this folds them into store.js. On `resync` the
// model is discarded and rebuilt from queries, which is the entire strategy for
// surviving a daemon restart (DESIGN §2).

import { h, clear, fmtBytes } from './lib/dom.js';
import {
  store,
  applyEvent,
  applyConnState,
  allowedAppCount,
  isCatchingUp,
  liveStatus,
  reloadAll,
  reloadGraph,
  mergeSegments,
  speakerByDisplayName,
  speakerLabel,
  ask,
} from './lib/store.js';
import { patchSpeakerLabels } from './lib/labels.js';
import { toast } from './lib/sheets.js';
import { stop as stopPreview, playbackState } from './lib/preview.js';
import * as replay from './lib/replay.js';
import * as transcriptView from './views/transcript.js';
import * as speakersView from './views/speakers.js';
import * as searchView from './views/search.js';
import * as sourcesView from './views/sources.js';
import * as personView from './views/person.js';
import * as memoryView from './views/memory.js';

const VIEWS = {
  transcript: transcriptView,
  speakers: speakersView,
  search: searchView,
  memory: memoryView,
  sources: sourcesView,
  // Not in the rail: the person page is pushed state, reached from a voice and
  // left with Back. The app has five places (docs/GRAPH.md) and this is not one
  // of them — you arrive at a person FROM one.
  person: personView,
};

/** Views the rail can select. Anything else is pushed. */
const RAIL_VIEWS = new Set(['transcript', 'speakers', 'search', 'memory', 'sources']);

const main = document.getElementById('main');
const footer = document.getElementById('footer');
const pauseBtn = document.getElementById('pause-btn');
const pauseLabel = document.getElementById('pause-label');
const pauseIco = document.getElementById('pause-ico');
const pauseHint = document.getElementById('pause-hint');

let currentName = 'transcript';
let currentArg = null;
let current = null;
// Where Back goes from a pushed view. One level deep, because the app is one
// level deep: you get to a person from a list of voices and you go back to it.
let returnTo = 'speakers';

const ctx = {
  go,
  jumpToSegment,
  showSpeakerInTranscript,
  openPerson,
  showThreadInTranscript,
  // 0.9.2: the same jump, but playing. Four surfaces reach it.
  replayThread,
  back,
  toast,
  resync,
  // 0.8.0: the person page's own way of asking for the bar the roster raises.
  showBrief,
  // 0.9.0: a reminder, from anywhere, lands on its note in Memory.
  openNote,
};

/**
 * Re-fetch everything and repaint. The daemon's own `resync` takes this path,
 * and so does any reply that says "re-run your queries" — `speakers.split` past
 * its event cap is the one that exists today (audit finding #17).
 *
 * A slice that fails keeps the data it had and retries itself (store.js); the
 * retry repaints through here too, so a view that came back late is not left
 * rendering the model from before it arrived.
 */
async function resync() {
  await reloadAll({ onRepaint: repaintAll }).catch(() => {});
  repaintAll();
}

function repaintAll() {
  go(currentName, currentArg);
  renderFooter();
  renderBadges();
}

// ---------------------------------------------------------------------------
// views
// ---------------------------------------------------------------------------

function go(name, arg = null) {
  if (!VIEWS[name]) return;
  // Leaving a view takes its stop button off screen, so it takes the sound too
  // — the row preview and, since 0.9.2, a conversation replay. The bar lives in
  // the transcript and a replay you cannot see or stop is a replay that has got
  // away from you.
  replay.close();
  stopPreview();
  // …and any menu it left hanging over a row that is about to stop existing.
  current?.closeMenu?.();
  // Remember the rail view a push came from, so Back is where you were rather
  // than wherever the code happens to think you should be.
  if (!RAIL_VIEWS.has(name) && RAIL_VIEWS.has(currentName)) returnTo = currentName;
  currentName = name;
  currentArg = arg;
  // `.rail-item[data-view]` — the rail also carries an ACTION now (Captions),
  // which opens a second window rather than swapping this one's contents. It
  // has no view to be selected for, and stamping aria-selected="false" on a
  // button that is not in the tablist would be a lie to a screen reader.
  for (const btn of document.querySelectorAll('.rail-item[data-view]')) {
    btn.setAttribute('aria-selected', String(btn.dataset.view === name));
  }
  clear(main);
  current = VIEWS[name].mount(main, ctx, arg);
  document.body.dataset.view = name;
}

/** A voice → the person behind it (docs/GRAPH.md). */
function openPerson(spId) {
  if (spId == null) return;
  go('person', { id: Number(spId) });
}

/** Leave a pushed view for the rail view it was opened from. */
function back() {
  go(RAIL_VIEWS.has(returnTo) ? returnTo : 'speakers');
}

/**
 * A conversation → the transcript, positioned on it.
 *
 * The speaker filter is CLEARED on purpose: the reason to open a thread is to
 * read what everybody said, and arriving filtered to one voice would answer a
 * question nobody asked. The thread's own span is marked instead.
 */
async function showThreadInTranscript(threadId) {
  let rows = [];
  try {
    const res = await ask('thread.get', { id: threadId });
    rows = res.segments ?? [];
  } catch (e) {
    toast(`Could not open that conversation — ${e.message}`, 'error');
    return null;
  }
  mergeSegments(rows);
  go('transcript');
  return current?.focusThread?.(threadId) ?? null;
}

/**
 * A conversation → the transcript, positioned on it AND playing (0.9.2).
 *
 * Deliberately the same route as `showThreadInTranscript`: the rows are merged
 * first so the player has something to light, and the landing is identical.
 * Replay is a way of reading the transcript, not a second place to be.
 *
 * `from` is a segment id to start on, which is what a search hit passes: you
 * searched for a line, so the conversation starts from that line.
 */
async function replayThread(threadId, { from = null } = {}) {
  let rows = [];
  try {
    const res = await ask('thread.get', { id: threadId });
    rows = res.segments ?? [];
  } catch (e) {
    toast(`Could not open that conversation — ${e.message}`, 'error');
    return null;
  }
  mergeSegments(rows);
  go('transcript');
  return (await current?.startReplay?.(threadId, { from })) ?? null;
}

for (const btn of document.querySelectorAll('.rail-item[data-view]')) {
  btn.addEventListener('click', () => go(btn.dataset.view));
}

// ---------------------------------------------------------------------------
// live captions — the rail's one non-navigating button (0.8.3)
//
// The state it reflects belongs to the MAIN process, because the window it
// describes does: it can be opened from the tray or from a command line while
// this view is not even mounted. So the button asks, and it is told.
// ---------------------------------------------------------------------------

const captionsBtn = document.getElementById('captions-btn');

async function paintCaptionsBtn(open = null) {
  const on = open ?? (await window.recall.captions.state()).open;
  captionsBtn.setAttribute('aria-pressed', String(!!on));
  captionsBtn.title = on
    ? 'Captions are on screen. They float above everything and ignore the mouse until you say otherwise.'
    : 'Put the last few turns on top of whatever you are doing, in large type.';
}

captionsBtn.addEventListener('click', async () => {
  const on = await window.recall.captions.toggle();
  await paintCaptionsBtn(on);
  // The card in Sources shows the same fact, and may be mounted right now.
  current?.update?.({ captions: true });
});

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

/**
 * A reminder → the note it is about (0.9.0).
 *
 * Every route into this ends here: the toast inside the window, the OS
 * notification outside it, and a click on the row itself. One function, so a
 * reminder always lands in the same place however it was answered.
 */
function openNote(noteId) {
  go('memory');
  return current?.focusNote?.(noteId) ?? null;
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
// the brief bar (0.8.0)
//
// A roster JOIN naming a voice the user has named is the one moment in this
// app where showing something unasked is worth it: you are about to talk to
// somebody, and what is open between you is a thing you would want to have
// remembered ten seconds ago rather than ten minutes later.
//
// Everything about it is bounded on purpose. It only ever fires for a NAMED
// voice — an unnamed one has nothing to brief you about and the roster cannot
// be linked to it anyway. It is one line. It is dismissible. It never sounds,
// never blocks, and never re-raises itself for the same person inside ten
// minutes, because a flaky instance can bounce somebody in and out four times
// in a minute and four identical bars is not four pieces of information.
// ---------------------------------------------------------------------------

const briefBar = document.getElementById('brief-bar');
/** How long one person's join stays "the same arrival". */
const BRIEF_DEBOUNCE_MS = 10 * 60 * 1000;
const briefSeen = new Map(); // speaker id → when they were last briefed
let briefShown = null;

function closeBrief() {
  briefShown = null;
  clear(briefBar);
  briefBar.hidden = true;
}

/**
 * One person's standing account, in a sentence. The order is what a person
 * actually wants first: what they owe you, then what you owe them, then what
 * you were last talking about — the third being the one that makes a hello
 * easier and the first two being the ones that are easy to be embarrassed by.
 */
function briefLine(brief, name) {
  const what = (c) => c?.what ?? 'something';
  const when = (c) => (c?.due_raw ? ` (${c.due_raw})` : '');
  const parts = [];
  const owed = brief.open_to_you ?? [];
  const owes = brief.open_from_you ?? [];
  const topics = brief.recent_topics ?? [];
  if (owed.length) parts.push(`owes you: ${owed.map((c) => what(c) + when(c)).join(', ')}`);
  if (owes.length) parts.push(`you owe: ${owes.map((c) => what(c) + when(c)).join(', ')}`);
  if (topics.length) parts.push(`last talked about: ${topics.slice(0, 2).join(', ')}`);
  // Nothing open and nothing remembered is still worth one honest clause. The
  // bar has already been raised by the join; going silent now would read as a
  // bug rather than as "there is nothing to say".
  if (!parts.length) parts.push('nothing open between you');
  return `${name} joined — ${parts.join(' · ')}`;
}

function renderBrief(brief, sp) {
  const name = sp?.name || speakerLabel(sp?.id);
  briefShown = sp?.id ?? null;
  clear(briefBar);
  briefBar.hidden = false;
  briefBar.append(
    h('span', { class: 'dot' }),
    h('span', { class: 'update-text', id: 'brief-text', text: briefLine(brief, name), title: briefLine(brief, name) }),
    h('span', { class: 'spacer' }),
    h(
      'button',
      {
        class: 'btn small',
        id: 'brief-open',
        title: `Open ${name}'s page`,
        onclick: () => {
          const id = sp?.id;
          closeBrief();
          if (id != null) openPerson(id);
        },
      },
      'Open'
    ),
    h(
      'button',
      {
        class: 'btn small',
        id: 'brief-dismiss',
        'aria-label': 'Dismiss this brief',
        title: 'Dismiss',
        onclick: closeBrief,
      },
      '✕'
    )
  );
}

/**
 * A roster join arrived. Match it to a named voice, debounce, ask, show.
 *
 * Exported onto the debug handle rather than kept private because the headless
 * driver has to be able to prove the DEBOUNCE, and a rule you can only observe
 * by waiting ten minutes is a rule nobody tests.
 */
async function onRosterJoin(data) {
  const sp = speakerByDisplayName(data?.who ?? data?.display_name);
  // Not a voice this user has named — there is nothing to brief, and guessing
  // at a link between a display name and an unnamed voice is exactly the kind
  // of confident wrong answer this app does not make.
  if (!sp) return { skipped: 'unlinked', who: data?.who ?? null };
  const now = Date.now();
  const last = briefSeen.get(sp.id) ?? 0;
  if (now - last < BRIEF_DEBOUNCE_MS) return { skipped: 'debounced', speaker: sp.id };
  briefSeen.set(sp.id, now);
  try {
    const brief = await ask('person.brief', { id: sp.id });
    renderBrief(brief, sp);
    return { shown: sp.id, name: sp.name };
  } catch (e) {
    // A daemon too old to brief is not an error anybody needs to see: the join
    // itself was never rendered before 0.8.0 either.
    return { skipped: 'failed', error: e.message };
  }
}

/** The "Brief" entry on a person page header calls this. */
async function showBrief(spId) {
  const sp = store.speakers.get(Number(spId));
  if (!sp) return null;
  briefSeen.set(sp.id, Date.now());
  try {
    renderBrief(await ask('person.brief', { id: sp.id }), sp);
    return { shown: sp.id };
  } catch (e) {
    toast(`Could not put together a brief — ${e.message}`, 'error');
    return null;
  }
}

// ---------------------------------------------------------------------------
// footer
// ---------------------------------------------------------------------------

function renderFooter() {
  clear(footer);
  const st = store.conn;
  const online = st.status === 'connected';
  // A resync that could not finish is not "connected" in the sense a green dot
  // makes: the socket is back, the views are not (audit finding #11). It is a
  // quiet mid state and it names itself, rather than a green light over data
  // the app knows is stale.
  const catching = online && isCatchingUp();
  const cls = catching ? 'mid' : online ? 'ok' : st.status === 'connecting' ? 'mid' : 'bad';
  const text = catching
    ? 'reconnected — still catching up'
    : online
      ? `connected · ${st.daemon ?? 'recalld'}`
      : st.status === 'connecting'
        ? 'connecting…'
        : 'daemon offline — retrying';

  // Numbers only mean something while a daemon is answering. Offline, the last
  // ones it said are not "the current queue depth", they are a memory — so they
  // go grey and read "—" rather than sitting next to "daemon offline" as if
  // they were live (audit finding #25b).
  // Grey says "there is no daemon behind these"; "—" says "no number yet".
  // They are different states and the boot moment — connected, first status
  // still in flight — is neither offline nor stale.
  const live = liveStatus();
  const statCls = online ? 'stat' : 'stat stale';

  // Element.append() stringifies null, so every optional child is filtered out
  // rather than passed through — a literal "null" in the status bar is exactly
  // the kind of thing a screenshot review catches and a diff does not.
  const parts = [
    h(
      'span',
      { class: `stat conn ${cls}`, id: 'conn-stat', title: catching ? 'Some views could not be re-fetched yet — retrying.' : null },
      h('span', { class: `dot${online && !catching && !store.paused ? ' pulse' : ''}` }),
      text
    ),
    h('span', { class: statCls }, 'seq ', h('b', { text: String(st.seq ?? '—') })),
    h('span', { class: statCls }, 'queue ', h('b', { text: String(live?.queue_depth ?? '—') })),
    h('span', { class: statCls }, 'drops ', h('b', { text: String(live?.drops ?? '—') })),
    h('span', { class: statCls }, 'sources ', h('b', { text: String(live?.sources_capturing ?? '—') })),
    // The two halves of disk usage that actually move, in the one place a
    // person already looks for "what is this program doing". The full
    // breakdown, and what each part means, is the Sources view's card.
    live?.storage
      ? h(
          'span',
          { class: 'stat', id: 'storage-stat', title: 'Database and recordings on disk — the breakdown is in Sources' },
          'db ',
          h('b', { text: fmtBytes(live.storage.db_bytes ?? 0) }),
          ' · audio ',
          h('b', { text: fmtBytes(live.storage.audio_bytes ?? 0) })
        )
      : null,
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
  // The microphone is excluded here for the same reason the Sources view
  // excludes it from its list: it is not an application on the allowlist, it
  // has its own card. One rule in store.js, so the badge and the view can no
  // longer disagree by one (audit finding #25a).
  set('badge-sources', allowedAppCount());
  // What is still owed. Deliberately the OPEN count and not the total: a badge
  // is a number you are meant to act on, and a settled commitment is not one.
  // It is also why nothing else in this feature ever nags — this is the only
  // place the app mentions a commitment you did not ask to see.
  set('badge-memory', store.graph?.counts?.open ?? 0);
}

// ---------------------------------------------------------------------------
// daemon wiring
// ---------------------------------------------------------------------------

window.recall.onState((st) => {
  applyConnState(st);
  // A replay is a stream of `segments.audio` calls, so a daemon that has gone
  // away means the next turn cannot arrive — and a bar that keeps counting over
  // a dead socket is claiming to play a conversation it cannot fetch. It stops,
  // and the footer already says why.
  if (store.conn.status !== 'connected') replay.close();
  renderPause();
  renderUpdateBar();
  renderFooter();
  current?.update?.({ status: true, conn: true, mic: true });
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
  // The rail badge is the daemon's own count of what is still open, so a
  // commitment moving states costs one small re-query rather than arithmetic
  // this client would have to get right (docs/GRAPH.md).
  if (change.commitment) {
    reloadGraph()
      .then(renderBadges)
      .catch(() => {});
  }
  // One path, every view: a rename made here, in the CLI, or in another client
  // all arrive as the same broadcast and repaint the same way.
  if (change.relabel) patchSpeakerLabels(change.relabel);
  // Somebody walked in. The bar this raises sits above every view rather than
  // inside one, so it belongs to the controller and not to a view.
  if (change.rosterJoin) void onRosterJoin(change.rosterJoin);
  // 0.9.0: a note you asked to be brought back. Two surfaces, because the app
  // is very often not the window you are looking at — an OS notification for
  // when it is behind a headset, and a toast for when it is not. Both land on
  // the same row.
  if (change.reminder) onReminder(change.reminder);
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

/**
 * A reminder came round.
 *
 * The toast is clickable and stays a little longer than an ordinary one: it is
 * the only toast in this app that is a thing to act on rather than a report of
 * something that already happened.
 */
function onReminder(r) {
  const text = r.text || 'a note you left yourself';
  const el = toast(`Reminder — ${text}`, 'ok');
  el.classList.add('clickable');
  el.title = 'Open this note';
  el.addEventListener('click', () => void openNote(r.note_id));
  // Outside the window too. `notify` never throws and answers false where the
  // desktop has no notification service; the toast has already said it either
  // way, so there is nothing to report.
  void window.recall.notify?.({
    noteId: r.note_id,
    title: 'NX Recall — reminder',
    body: text,
  });
}

// Somebody clicked the notification while the app was behind something else.
window.recall.onOpenNote?.((d) => {
  if (d?.noteId != null) void openNote(d.noteId);
});

window.recall.onResync(async (info) => {
  // Everything on screen may be stale. Rebuild from queries and remount.
  await resync();
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
  const map = { 1: 'transcript', 2: 'speakers', 3: 'search', 4: 'memory', 5: 'sources' };
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
  applyConnState(st);
  renderPause();
  renderUpdateBar();
  renderFooter();
  // A first load that only half answered keeps whatever it did get, says so in
  // the footer, and retries itself — the retry repaints through repaintAll.
  await reloadAll({ onRepaint: repaintAll }).catch(() => {});
  go('transcript');
  renderFooter();
  renderBadges();
  await paintCaptionsBtn().catch(() => {});
  // A setting changed anywhere — this card, the tray's click-through item, the
  // captions window being dragged. One broadcast, every surface.
  window.recall.onCaptionSettings((next) => {
    void paintCaptionsBtn();
    current?.update?.({ captions: next });
  });

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
      // 0.7.4: the row count stopped being a proxy for "did the feed append?".
      // The window is bounded while following, so it can be full and still be
      // moving. This counter only ever goes up, and it goes up per live segment.
      appended: store.appended,
    }),
    // 0.7.4, the scrollback. Everything the driver has to read back about the
    // window: which state it is in, how far back it reaches, whether the
    // beginning marker is really on screen, and how the seams came out.
    scrollback: () => {
      const bodyEl = document.getElementById('transcript-body');
      const rows = [...document.querySelectorAll('#seg-list .seg')];
      const ids = rows.map((r) => Number(r.dataset.seg));
      return {
        following: store.window.following,
        detached: store.window.detached,
        pressed: (document.getElementById('follow-btn') || {}).getAttribute?.('aria-pressed') ?? null,
        beginning: store.window.beginning,
        capped: store.window.capped,
        segments: store.segments.length,
        rows: rows.length,
        // A prepend must not duplicate, and the list must stay in time order.
        unique: new Set(ids).size,
        ordered: store.segments.every((s, i, a) => i === 0 || a[i - 1].t_ms <= s.t_ms),
        firstId: ids[0] ?? null,
        lastId: ids[ids.length - 1] ?? null,
        firstMs: store.segments[0]?.t_ms ?? null,
        daySeps: document.querySelectorAll('#seg-list .day-sep').length,
        threadSeps: document.querySelectorAll('#seg-list .thread-sep').length,
        // No two day separators may name the same day, and none may sit
        // directly against another — both are what a bad seam looks like.
        dayLabels: [...document.querySelectorAll('#seg-list .day-sep')].map((d) => d.textContent),
        adjacentSeps: [...document.querySelectorAll('#seg-list .day-sep')].filter(
          (d) => d.previousElementSibling?.classList.contains('day-sep')
        ).length,
        beginMark: {
          shown: !document.getElementById('transcript-beginning')?.hidden,
          text: document.getElementById('transcript-beginning')?.textContent ?? '',
        },
        note: {
          shown: !document.getElementById('window-note')?.hidden,
          text: document.getElementById('window-note')?.textContent ?? '',
        },
        datePicker: !!document.getElementById('transcript-date'),
        scrollTop: bodyEl?.scrollTop ?? null,
        scrollHeight: bodyEl?.scrollHeight ?? null,
        clientHeight: bodyEl?.clientHeight ?? null,
      };
    },
    /** Drive the scrollback from the driver without faking wheel events. */
    loadOlder: () => current?.loadOlder?.() ?? null,
    jumpToDay: (day) => current?.jumpToDay?.(day) ?? null,
    // The microphone: the model's view of the switch, and what the DOM is
    // actually showing for it, so the driver can assert on both.
    mic: () => {
      const chip = document.getElementById('mic-chip');
      const toggle = document.getElementById('mic-toggle');
      return {
        ...store.mic,
        chip: chip ? chip.textContent : null,
        pressed: toggle ? toggle.getAttribute('aria-pressed') : null,
        mode: store.mic.mode,
        modePressed: [...document.querySelectorAll('#mic-modes .mode-opt')].map((b) => [
          b.dataset.mode,
          b.getAttribute('aria-pressed'),
        ]),
        warning: (document.getElementById('mic-warning') || {}).textContent ?? '',
        // "You" rows in the live transcript, and the rest, so the driver can
        // prove the treatment is DISTINCT rather than merely present.
        youRows: document.querySelectorAll('#seg-list .seg.you').length,
        otherRows: document.querySelectorAll('#seg-list .seg:not(.you)').length,
      };
    },
    // 0.6.1. Three facts the driver has to be able to read back: what a voice
    // is declared to speak, how many voices a sweep would take, and what the
    // storage card is actually rendering.
    speaker: (id) => {
      const sp = store.speakers.get(Number(id)) ?? null;
      const chip = document.querySelector(`.sp-row[data-speaker="${id}"] .chip.lang`);
      return {
        languages: sp?.languages ?? null,
        chip: chip ? chip.textContent : null,
      };
    },
    sweep: () => {
      const btn = document.getElementById('sweep-voices');
      return { present: !!btn, label: btn ? btn.textContent : null };
    },
    storage: () => ({
      status: store.status?.storage ?? null,
      rows: [...document.querySelectorAll('#storage-rows .storage-row')].map((r) => [
        r.dataset.storage,
        r.querySelector('.storage-bytes').textContent,
      ]),
      note: (document.getElementById('storage-note') || {}).textContent ?? '',
      footer: (document.getElementById('storage-stat') || {}).textContent ?? '',
    }),
    // Segments whose speaker was inherited from the turns around them rather
    // than heard: they must read as uncertain, with a "?" that says why.
    proximity: () => {
      const rows = [...document.querySelectorAll('#seg-list .seg')].filter((r) => {
        const seg = store.segById.get(Number(r.dataset.seg));
        return seg?.label_via === 'proximity';
      });
      return {
        rows: rows.length,
        uncertain: rows.filter((r) => r.classList.contains('uncertain')).length,
        why: rows[0]?.querySelector('.qmark')?.title ?? '',
      };
    },
    // 0.6.2, the memory graph. Three things the driver has to read back: what
    // the person page is actually showing, and whether the transcript really
    // separates and marks conversations.
    person: () => {
      const strip = [...document.querySelectorAll('#person-strip .person-stat')].map((s) => [
        s.dataset.stat,
        s.querySelector('b').textContent,
      ]);
      return {
        mounted: currentName === 'person',
        id: currentArg?.id ?? null,
        name: (document.getElementById('person-name') || {}).textContent ?? '',
        sub: (document.getElementById('person-sub') || {}).textContent ?? '',
        strip,
        // The rendered date is minute-resolution; this is the instant behind
        // it, so "the page followed the feed" is a fact rather than a guess.
        lastHeardMs: Number(document.querySelector('#person-strip [data-stat="last-heard"]')?.dataset.ms ?? 0),
        segments: Number(
          document.querySelector('#person-strip [data-stat="segments"] b')?.textContent?.replace(/\D/g, '') ?? 0
        ),
        edges: [...document.querySelectorAll('#edge-list .edge-row')].map((r) => ({
          id: Number(r.dataset.edge),
          name: r.querySelector('.edge-name').textContent,
        })),
        threads: [...document.querySelectorAll('#thread-list .thread-row')].map((r) => ({
          id: Number(r.dataset.thread),
          names: r.querySelector('.thread-names').textContent,
          preview: r.querySelector('.thread-preview').textContent,
        })),
        back: !!document.getElementById('person-back'),
      };
    },
    threads: () => ({
      separators: [...document.querySelectorAll('#seg-list .thread-sep')].map((s) => s.textContent),
      marked: document.querySelectorAll('#seg-list .seg.in-thread').length,
      markedThread: document.querySelector('#seg-list .seg.in-thread')?.dataset.thread ?? null,
      filter: (document.getElementById('transcript-filter') || {}).value ?? null,
      rows: document.querySelectorAll('#seg-list .seg').length,
    }),
    // 0.7.0, the memory graph's Tiers 2 and 3. Everything the driver has to be
    // able to read back about the Memory view: the rows and, on each, the two
    // facts that must never be conflated — which tier claimed it and what a
    // person has decided about it — plus the three states the enrichment card
    // has copy for.
    memory: () => ({
      mounted: currentName === 'memory',
      sub: (document.getElementById('memory-sub') || {}).textContent ?? '',
      badge: (document.getElementById('badge-memory') || {}).textContent ?? '',
      commitments: [...document.querySelectorAll('#commit-list .commit-row')].map((r) => ({
        id: Number(r.dataset.commitment),
        state: r.dataset.state,
        source: r.dataset.source,
        // A row renders its new state optimistically and stays disabled until
        // the daemon confirms. A driver that pressed a button in that window
        // would be pressing nothing, so it has to be able to see the gap.
        pending: r.classList.contains('pending'),
        who: r.querySelector('.commit-name')?.textContent ?? '',
        to: r.querySelector('.commit-to')?.textContent ?? '',
        what: r.querySelector('.commit-what')?.textContent ?? '',
        said: r.querySelector('.commit-said')?.textContent ?? '',
        due: r.querySelector('.commit-due')?.firstChild?.textContent ?? '',
        undated: !!r.querySelector('.commit-due.undated'),
        // The visible mark, not the dataset: a class nobody can see is not a
        // distinction a person can act on.
        srcChip: r.querySelector('.chip.src')?.textContent ?? '',
        srcTitle: r.querySelector('.chip.src')?.title ?? '',
        actions: [...r.querySelectorAll('[data-act]')].map((b) => b.dataset.act),
      })),
      empty: (document.getElementById('commitments-empty') || {}).textContent ?? '',
      note: (document.getElementById('commitments-note') || {}).textContent ?? '',
      topics: [...document.querySelectorAll('#topic-list .topic-row')].map((r) => ({
        topic: r.dataset.topic,
        threads: Number(r.dataset.threads),
        text: r.textContent,
      })),
      enrichment: {
        phase: store.graph?.enrichment?.phase ?? null,
        enabled: store.graph?.config?.enabled ?? null,
        chip: (document.getElementById('enrich-chip') || {}).textContent ?? '',
        pressed: (document.getElementById('enrich-toggle') || {}).getAttribute?.('aria-pressed') ?? null,
        progress: (document.getElementById('enrich-progress-text') || {}).textContent ?? '',
        reason: (document.getElementById('enrich-reason') || {}).textContent ?? '',
        facts: [...document.querySelectorAll('#enrich-facts li')].map((li) => li.textContent),
        // 0.7.2: how much of the machine the model may use. The rendered value,
        // not the store's — the point of the control is that a person can see
        // what they set, and the hint that says when it takes effect.
        threads: (document.getElementById('enrich-threads-value') || {}).textContent ?? '',
        threadsPending:
          (document.getElementById('enrich-threads') || {}).dataset?.pending === 'true',
        threadsHint: (document.getElementById('enrich-threads-hint') || {}).textContent ?? '',
        threadsConfig: store.graph?.config?.llm_threads ?? null,
        note: (document.getElementById('enrich-note') || {}).textContent ?? '',
        counts: (document.getElementById('enrich-counts') || {}).textContent ?? '',
      },
    }),
    // 0.8.0. Everything the driver has to read back about the accuracy round:
    // the two marks a row can now wear, the sheet's inline fix, the three new
    // Memory cards, the interpretation pills, and the brief bar.
    accuracy: () => ({
      // Rows the second decoder disagreed with, and the mark they wear. The
      // mark is read off the DOM rather than off the model, because a class
      // nobody can see is not a distinction anybody can act on.
      shakyRows: document.querySelectorAll('#seg-list .seg.shaky').length,
      shakyMarks: document.querySelectorAll('#seg-list .seg.shaky .shaky-mark').length,
      // …and rows the cross-check agreed with must NOT wear one: a badge on
      // every row is not a badge.
      solidMarks: document.querySelectorAll('#seg-list .seg:not(.shaky) .shaky-mark').length,
      tip: (document.querySelector('#seg-list .shaky-mark') || {}).title ?? '',
      searchShaky: document.querySelectorAll('#search-results .seg.shaky .shaky-mark').length,
      sheet: {
        reader: (document.getElementById('segment-text') || {}).textContent ?? null,
        editing: !!document.getElementById('correct-text') && !document.getElementById('correct-text').hidden,
        hint: (document.getElementById('segment-fix-hint') || {}).textContent ?? '',
        via: (document.getElementById('segment-text-via') || {}).textContent ?? '',
        shaky: (document.getElementById('segment-shaky') || {}).textContent ?? '',
      },
      notes: [...document.querySelectorAll('#note-list .note-row')].map((r) => ({
        id: Number(r.dataset.note),
        state: r.dataset.state,
        segment: Number(r.dataset.segment),
        pending: r.classList.contains('pending'),
        text: r.querySelector('.note-text')?.textContent ?? '',
        acts: [...r.querySelectorAll('[data-note-act]')].map((b) => b.dataset.noteAct),
        // 0.9.0: a note with a date is a reminder, and the chip is the whole
        // difference. Read off the DOM, because a date nobody can see is not a
        // reminder anybody can act on.
        due: r.dataset.due ? Number(r.dataset.due) : null,
        fired: r.dataset.fired === 'true',
        dueChip: r.querySelector('[data-due="chip"]')?.textContent ?? null,
        snoozes: [...r.querySelectorAll('[data-snooze]')].map((b) => Number(b.dataset.snooze)),
      })),
      notesEmpty: (document.getElementById('notes-empty') || {}).textContent ?? '',
      dash: {
        corrections: (document.getElementById('accuracy-corrections') || {}).textContent ?? '',
        wer: (document.getElementById('accuracy-wer') || {}).textContent ?? '',
        bySource: [...document.querySelectorAll('[data-acc="by-source"] .acc-row')].map((r) => r.dataset.accRow),
        bySpeaker: [...document.querySelectorAll('[data-acc="by-speaker"] .acc-row')].map((r) => r.dataset.accRow),
        empty: (document.getElementById('accuracy-empty') || {}).textContent ?? '',
        note: (document.getElementById('accuracy-note') || {}).textContent ?? '',
      },
      vocab: {
        user: [...document.querySelectorAll('#vocab-user .vocab-chip')].map((c) => c.dataset.term),
        effective: (document.getElementById('vocab-effective') || {}).textContent ?? '',
        counts: Object.fromEntries(
          [...document.querySelectorAll('[data-count]')].map((c) => [c.dataset.count, Number(c.textContent)])
        ),
        // An auto term must not offer a remove button: you cannot argue with
        // what was heard, and a button that did nothing would say you could.
        autoRemovable: document.querySelectorAll('.vocab-chip.auto .vocab-chip-x').length,
      },
    }),
    // 0.9.0. Everything the driver has to read back about the assistant round:
    // the Yesterday card, and a translated turn in the transcript.
    assistant: () => ({
      digests: {
        shown: !document.getElementById('digest-card')?.hidden,
        sub: (document.getElementById('digest-sub') || {}).textContent ?? '',
        note: (document.getElementById('digest-note') || {}).textContent ?? '',
        rows: [...document.querySelectorAll('.digest-row')].map((r) => ({
          thread: Number(r.dataset.digest),
          day: r.dataset.day,
          summary: r.querySelector('.digest-text')?.textContent ?? '',
          people: [...r.querySelectorAll('.chip.person')].map((c) => Number(c.dataset.sp)),
          open: [...r.querySelectorAll('.digest-open-item')].map((o) => o.textContent),
        })),
        groups: [...document.querySelectorAll('[data-digests]')].map((g) => g.dataset.digests),
      },
      translated: {
        // Rows with a second line under the words, and the words themselves,
        // which must still be the ORIGINAL: a translation is a reading of the
        // transcript and never a replacement for it.
        rows: document.querySelectorAll('#seg-list .txt.has-translation').length,
        pairs: [...document.querySelectorAll('#seg-list .txt.has-translation')]
          .slice(0, 3)
          .map((t) => ({
            said: t.querySelector('.txt-said')?.textContent ?? '',
            reading: t.querySelector('.txt-translated')?.textContent ?? '',
            lang: t.querySelector('.txt-translated')?.dataset.translation ?? '',
            via: t.querySelector('.txt-translated')?.dataset.via ?? '',
          })),
        // A row with no translation must render exactly as it always did.
        plain: document.querySelectorAll('#seg-list .txt:not(.has-translation)').length,
      },
    }),
    ask: () => ({
      pills: [...document.querySelectorAll('#ask-pills .ask-pill')].map((p) => ({
        facet: p.dataset.facet,
        text: p.querySelector('.ask-pill-text')?.textContent ?? '',
        removable: !!p.querySelector('.ask-pill-x'),
      })),
      shown: !document.getElementById('ask-pills')?.hidden,
      hits: document.querySelectorAll('#search-results .seg').length,
      sub: (document.getElementById('search-sub') || {}).textContent ?? '',
      advanced: (document.getElementById('search-advanced') || {}).getAttribute?.('aria-expanded') ?? null,
      facetsHidden: !!document.getElementById('search-facets')?.hidden,
      mode: [...document.querySelectorAll('.seg-ctl .seg-opt')].map((b) => [b.id, b.getAttribute('aria-pressed')]),
    }),
    brief: () => ({
      shown: !briefBar.hidden,
      speaker: briefShown,
      text: (document.getElementById('brief-text') || {}).textContent ?? '',
      open: !!document.getElementById('brief-open'),
      dismiss: !!document.getElementById('brief-dismiss'),
    }),
    // 0.8.3, live captions. What this window can say about them: the rail
    // button's state, and every control on the settings card with the value it
    // is actually rendering — the driver moves the real sliders.
    captions: () => ({
      pressed: captionsBtn.getAttribute('aria-pressed'),
      card: !!document.getElementById('captions-card'),
      open: !!document.getElementById('captions-open'),
      values: Object.fromEntries(
        [...document.querySelectorAll('#captions-card [data-cap]')].map((el) => [
          el.dataset.cap,
          el.type === 'checkbox' || el.getAttribute('role') === 'switch'
            ? el.getAttribute('aria-pressed') ?? String(el.checked)
            : el.value,
        ])
      ),
      shown: Object.fromEntries(
        [...document.querySelectorAll('#captions-card [data-cap-value]')].map((el) => [
          el.dataset.capValue,
          el.textContent,
        ])
      ),
    }),
    // The one line behind the native-widget fix: without `color-scheme: dark`
    // Chromium draws <select> option popups light-on-light over this palette.
    colorScheme: () => getComputedStyle(document.documentElement).colorScheme,
    // What a resync managed to refresh, and what the footer is saying about
    // it. The driver has to be able to tell "connected" from "connected but
    // still missing half the model" (audit finding #11).
    resync: () => ({
      stale: [...(store.resync?.stale ?? [])],
      attempt: store.resync?.attempt ?? 0,
      retrying: !!store.resync?.retrying,
      conn: store.conn.status,
      connText: (document.getElementById('conn-stat') || {}).textContent ?? '',
      footer: (document.getElementById('footer') || {}).textContent ?? '',
      stats: [...document.querySelectorAll('#footer .stat')].map((s) => [s.textContent, s.classList.contains('stale')]),
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
    // Conversation replay (0.9.2): the engine's state, the bar the transcript
    // drew from it, and the row it lit. One read, because the whole feature is
    // "these three agree".
    replay: () => ({
      ...(current?.replayUi?.() ?? { ...replay.replayState(), shown: false }),
      view: currentName,
      // The one sound in the app, shared with the row preview: if this says
      // playing while the bar says closed, they have come apart.
      audioPaused: window.__recallAudio ? window.__recallAudio.paused : null,
      audioRate: window.__recallAudio ? window.__recallAudio.playbackRate : null,
    }),
    startReplay: (thread, opts) => replayThread(thread, opts ?? {}),
    replayToggle: () => replay.toggle(),
    replayRate: (r) => replay.setRate(r),
    replayClose: () => replay.close(),
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
