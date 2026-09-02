// The renderer's whole model. It is a cache of the daemon's answers and
// nothing more: on `resync` it is thrown away and rebuilt from queries, which
// is what DESIGN §2 means by "clients are dumb views that survive daemon
// restarts". No state here is authoritative and none of it is persisted.

const OVERLAP_REFUSE = 0.1; // DESIGN §4: embeddings only run at overlap_frac ≤ 0.1
const WEAK_MATCH = 0.4; // below this the label is a guess worth flagging

/**
 * The live window while you are FOLLOWING the tail. Not a limit on what the
 * app can show — 0.7.4 pages backwards without bound — a limit on how much of
 * the tail is kept resident when nobody is reading history.
 */
export const MAX_SEGMENTS = 600;

/** One page of scrollback. */
export const PAGE_SEGMENTS = 400;

/**
 * The ceiling on resident rows, in either direction. Twenty thousand segments
 * is somewhere north of forty hours of conversation and about 20 MB of DOM;
 * past that the honest thing is to say so and point at search and the date
 * picker rather than to keep growing until the window stops scrolling.
 */
export const HARD_MAX = 20_000;

export const store = {
  conn: { status: 'offline', daemon: null, seq: null, error: null, socketPath: null },
  paused: false,
  pausePending: false,
  status: null,
  // Is that block a READING or a memory? False the moment the connection
  // drops, so the footer stops quoting a daemon that is not there while the
  // structural facts in it (storage, whether the semantic model is installed)
  // stay available to the views that want them. See `liveStatus`.
  statusLive: false,
  update: null, // {from, to} — the daemon came back as a different version

  speakers: new Map(), // id → {id, name, auto, segments, total_ms, first_seen, you}
  segments: [], // ascending by t_ms
  segById: new Map(),
  /**
   * How the resident window behaves (0.7.4). The rule the whole feature turns
   * on: **trimming must never discard rows in the direction the reader is
   * looking.**
   *
   * - `following` — the live tail. The only state in which the oldest rows may
   *   be dropped, and they are, above MAX_SEGMENTS, exactly as they always were.
   * - browsing (`following: false`) — the reader has scrolled up into history,
   *   or jumped to a search hit or a date. Tail trimming is SUSPENDED: history
   *   is precisely what it used to throw away (audit finding #12).
   *
   * `anchor` is the id of a row near the viewport, set by the view. It decides
   * which END the 20k ceiling trims from: whichever is further from it.
   * `beginning` is true once a short page has proved there is nothing older;
   * `firstMs` is when that first captured row was, for the marker.
   *
   * `detached` is the third state and the one that is easy to miss. Scrolling
   * up is browsing but the tail is still down there at the bottom of the
   * window; the DATE PICKER throws the window away and rebuilds it somewhere
   * else, so the tail is not resident at all and cannot be collapsed back to.
   * Rows arriving live while detached are counted but not filed — tonight does
   * not belong underneath July — and Follow re-asks for the tail in one query.
   */
  window: { following: true, anchor: null, capped: false, beginning: false, firstMs: null, detached: false },
  /**
   * How many live segments this client has seen arrive. A monotone counter,
   * because the row count no longer is: with a bounded window "did the feed
   * append?" and "did the list get longer?" are different questions.
   */
  appended: 0,
  sources: [],
  // The microphone switch (PROTOCOL "The microphone"). Not a source rule: it
  // hears the room rather than one program, so it has its own method, its own
  // card, and its own default (off).
  mic: { enabled: false, mode: 'follow', active: false, state: 'off', device: null, you_speaker: null },
  // The memory graph (docs/GRAPH.md, schema 7). Only the counts and the
  // worker's state live here, because only those two are wanted OUTSIDE the
  // Memory view — the rail badge needs "how many are open" wherever you are,
  // and nothing else does. The commitments themselves are that view's own.
  graph: { counts: null, enrichment: { phase: 'off' }, config: null },
  ops: new Map(), // op id → {kind, frac, done}

  /**
   * How the last resync actually went, per slice (audit finding #11).
   *
   * A resync is five independent queries and any of them can fail on its own —
   * a socket that flaps while the daemon restarts fails ALL of them, which is
   * exactly when it matters. `stale` names the slices whose query did not
   * answer, and the rule the whole thing turns on: **a slice that failed keeps
   * the data it already had.** An empty model is a claim ("no voices yet") and
   * a failed query is not entitled to make it.
   *
   * `attempt` counts consecutive failed resyncs and drives the backoff;
   * `retrying` is true while one is scheduled. While `stale` is non-empty the
   * footer says so rather than showing a green "connected" over data that is
   * quietly out of date.
   */
  resync: { stale: [], attempt: 0, retrying: false, since: null },

  loaded: false,
  lastError: null,
};

export function speakerLabel(id) {
  if (id == null) return 'Unassigned';
  const sp = store.speakers.get(id);
  if (!sp) return `Speaker ${id}`;
  return sp.name || sp.auto || `Speaker_${String(id).padStart(2, '0')}`;
}

/**
 * What to call the voice on a segment that has none. "Unassigned" says only
 * that a field is empty; the pipeline knows more than that and the two cases
 * are not the same problem. Above the refuse line the identity was never
 * attempted because people were talking over each other — that is "several
 * voices", and no amount of listening will turn it into one name. Below it, one
 * person spoke and matched nothing in the voicebank — that is an "unknown
 * voice", and naming it is worth doing.
 */
export function segmentSpeakerLabel(seg) {
  if (seg?.speaker != null) return speakerLabel(seg.speaker);
  return (seg?.overlap_frac ?? 0) > OVERLAP_REFUSE ? 'several voices' : 'unknown voice';
}

export function isNamed(sp) {
  return !!(sp && sp.name);
}

export function isUncertain(seg) {
  if (!seg) return false;
  if (seg.speaker == null) return true;
  if ((seg.overlap_frac ?? 0) > OVERLAP_REFUSE) return true;
  // Inherited from the turns either side of it rather than heard (0.6.1). It
  // has a name and no score, and the honest reading of that is "probably, for
  // a good reason" — which is exactly what the uncertain treatment says.
  if (seg.label_via === 'proximity') return true;
  if (seg.match_score != null && seg.match_score < WEAK_MATCH) return true;
  return false;
}

// What the words themselves are worth, appended to the "?" when there is
// something to say (0.7.7). Only two of the five `lang_via` values are worth a
// sentence: the ones where the text on screen is not simply what the primary
// model heard. `model`, `classified` and `context` never changed a word.
export function languageNote(seg) {
  if (seg?.lang_via === 're-decode')
    return 'These words were re-read from the audio by the German/English arbiter, after the conversation around them suggested the first pass had heard the wrong language.';
  if (seg?.lang_via === 'mismatch')
    return 'This turn reads as a different language from the conversation around it, and nothing could settle which is right — so the original words are kept as they are.';
  return '';
}

// What the "?" says. It has to name the actual reason, and there are now two
// kinds: who said it, and what was said. A row whose SPEAKER is certain but
// whose WORDS were re-read gets only the second — "weak voice match (0.82)"
// would be a lie about a name nothing is doubting.
export function uncertainReason(seg) {
  const note = languageNote(seg);
  if (!isUncertain(seg)) return note;
  return note ? `${speakerReason(seg)} ${note}` : speakerReason(seg);
}

/// Is there anything for the "?" to say at all?
export function hasMark(seg) {
  return isUncertain(seg) || languageNote(seg) !== '';
}

// Why the NAME on a row is in doubt. "We are not sure" is useless; "two people
// were talking at once so the identity was refused" tells the user why the fix
// is theirs to make.
function speakerReason(seg) {
  const ov = seg.overlap_frac ?? 0;
  if (ov > OVERLAP_REFUSE && seg.speaker == null)
    return `Several voices overlap here (${Math.round(ov * 100)}% of the segment), so no speaker identity was claimed. Click to assign one.`;
  // Your own microphone, with the room bleeding into it — loudspeakers, most
  // likely. The NAME is not in doubt (it came from the device, not a match);
  // the words are, because more than one person is in them.
  if (ov > OVERLAP_REFUSE && isYou(seg.speaker))
    return `Recorded on your microphone, so the speaker is certain — but ${Math.round(ov * 100)}% of it overlaps another voice, so the words may be a mix. Click to correct them.`;
  if (ov > OVERLAP_REFUSE)
    return `Overlapped speech (${Math.round(ov * 100)}%) — this label is not trustworthy. Click to correct it.`;
  if (seg.speaker == null) return 'No known voice matched this segment. Click to assign one.';
  // A fragment too short for the voicebank to place, sitting inside somebody
  // else's turn. The clock named it, not the model, and saying so is the whole
  // point of the mark.
  if (seg.label_via === 'proximity')
    return `Too short to identify on its own — inherited from the surrounding turn, which was ${speakerLabel(seg.speaker)}. Click to correct it.`;
  return `Weak voice match (${(seg.match_score ?? 0).toFixed(2)}). Click to correct it.`;
}

/**
 * How a voice's languages read in one short phrase. `null`/absent is "Any",
 * the default — and the difference matters, because only a voice pinned to
 * exactly one language ever gets a transcript corrected.
 */
export const LANGUAGE_CHOICES = [
  { value: '', label: 'Any', title: 'No correction: any language is expected from this voice.' },
  { value: 'de', label: 'German', title: 'A transcript that reads as English is flagged — there is no German-constrained decoder to re-run it with.' },
  { value: 'en', label: 'English', title: 'A transcript that reads as German is decoded again with the English-only model.' },
  { value: 'de,en', label: 'German + English', title: 'Two languages: nothing is corrected, because either one is expected.' },
];

export function languageValue(sp) {
  const list = sp?.languages;
  return Array.isArray(list) && list.length ? [...list].sort().join(',') : '';
}

export function languageLabel(sp) {
  return LANGUAGE_CHOICES.find((c) => c.value === languageValue(sp))?.label ?? 'Any';
}

// -- queries ----------------------------------------------------------------

async function ask(method, params) {
  const res = await window.recall.request(method, params);
  if (!res?.ok) throw Object.assign(new Error(res?.err?.msg || 'request failed'), { code: res?.err?.code });
  return res.data;
}

export { ask };

// -- the resync, and what a failed half of one is allowed to do ---------------

/** How long before a resync that could not finish tries the rest again. */
export const RESYNC_RETRY_MIN = 1500;
export const RESYNC_RETRY_MAX = 20000;

let resyncTimer = null;

/** Stop any scheduled retry. Called whenever a fresh resync starts. */
export function cancelResyncRetry() {
  if (resyncTimer) clearTimeout(resyncTimer);
  resyncTimer = null;
  store.resync.retrying = false;
}

/** Is some part of the model known to be out of date? */
export function isCatchingUp() {
  return (store.resync.stale?.length ?? 0) > 0;
}

/**
 * One query of a resync, run so that it can fail without taking the others —
 * or the data it already has — with it.
 *
 * Three outcomes, not two. `ok` is an answer. `absent` is an HONEST no: the
 * daemon is older than the method (mic before 0.6.0, the graph before 0.7.0),
 * which is a fact about the daemon rather than a failure to refresh. Anything
 * else is a failure, and a failure means the previous slice stands.
 */
async function slice(name, run) {
  try {
    return { name, ok: true, data: await run() };
  } catch (e) {
    return { name, ok: false, absent: e?.code === 'unknown_method', error: e };
  }
}

/**
 * Full resync: every view's data re-fetched from scratch.
 *
 * Audit finding #11 lived in the `.catch(() => ({speakers: []}))` this used to
 * open with. Every slice was assigned unconditionally afterwards, so a resync
 * whose queries all rejected — the ordinary shape of a daemon restart, where
 * the socket flaps once more just as the client re-asks — replaced the whole
 * model with empty lists and set `loaded`. The footer stayed green and every
 * view said "No voices yet"; a PARTIAL failure was worse, because a voicebank
 * that came back empty while the transcript did not made the rows fall back to
 * "Speaker 12" and read as a real, catastrophic data loss.
 *
 * The rule now: a slice is only ever overwritten by an ANSWER. What failed is
 * named in `store.resync.stale`, retried with a bounded backoff, and said out
 * loud in the footer until it comes back.
 */
export async function reloadAll({ retry = true, retryMin = RESYNC_RETRY_MIN, onRepaint = null } = {}) {
  cancelResyncRetry();
  const [speakers, transcript, sources, mic, graph] = await Promise.all([
    slice('speakers', () => ask('speakers.list')),
    slice('transcript', () => ask('transcript', { limit: MAX_SEGMENTS })),
    slice('sources', () => ask('sources.list')),
    slice('mic', () => ask('mic.get')),
    slice('graph', () => ask('graph.summary')),
  ]);

  if (speakers.ok) store.speakers = new Map((speakers.data?.speakers ?? []).map((s) => [s.id, s]));
  if (transcript.ok) {
    store.segments = [...(transcript.data?.segments ?? [])].sort((a, b) => a.t_ms - b.t_ms);
    store.segById = new Map(store.segments.map((s) => [s.id, s]));
    // A resync lands you back on the live tail, which is where a resync means
    // you were. A first page SHORTER than the window is the whole archive, so
    // the beginning is already loaded and the scrollback has nowhere to go.
    store.window = {
      following: true,
      anchor: null,
      capped: false,
      beginning: store.segments.length < MAX_SEGMENTS,
      firstMs: store.segments[0]?.t_ms ?? null,
      detached: false,
    };
  }
  if (sources.ok) store.sources = sources.data?.sources ?? [];
  if (mic.ok && mic.data) applyMic(mic.data);
  if (graph.ok) store.graph = graph.data ?? { counts: null, enrichment: { phase: 'off' }, config: null };
  else if (graph.absent) store.graph = { counts: null, enrichment: { phase: 'off' }, config: null };

  const stale = [speakers, transcript, sources, mic, graph].filter((s) => !s.ok && !s.absent).map((s) => s.name);
  store.resync = {
    stale,
    attempt: stale.length ? store.resync.attempt + 1 : 0,
    retrying: false,
    since: stale.length ? (store.resync.since ?? Date.now()) : null,
  };
  // "Loaded" means the model has been filled from the daemon at least once, so
  // a half-answered resync must not be able to claim it.
  if (!stale.length) store.loaded = true;

  if (stale.length && retry) {
    // Bounded exponential backoff. The connection may be flapping under this,
    // and the client's own reconnect will drive another resync when it settles;
    // this covers the case where the socket is fine and the daemon was merely
    // too busy to answer.
    const wait = Math.min(RESYNC_RETRY_MAX, retryMin * 2 ** Math.max(0, store.resync.attempt - 1));
    store.resync.retrying = true;
    resyncTimer = setTimeout(() => {
      resyncTimer = null;
      store.resync.retrying = false;
      const before = store.resync.stale.length;
      reloadAll({ retry, retryMin, onRepaint })
        .then(() => {
          // Repaint only when the retry actually recovered something. A repaint
          // remounts the view, and doing that every twenty seconds under a
          // daemon that is simply gone would take the reader's scroll position
          // with it for as long as the outage lasts.
          if (store.resync.stale.length < before) onRepaint?.();
        })
        .catch(() => {});
    }, wait);
    if (resyncTimer.unref) resyncTimer.unref();
  }
  return store;
}

/**
 * Fold a mic block (from `mic.get`, `mic.set`, or a `mic` event) into the
 * model. `you_speaker` only travels on the first two, so an event must not be
 * allowed to erase it.
 */
export function applyMic(d) {
  store.mic = { ...store.mic, ...d };
  if (d.you_speaker !== undefined) store.mic.you_speaker = d.you_speaker;
  return store.mic;
}

/** Is this the user's own voice — the one the microphone pins? */
export function isYou(speakerId) {
  if (speakerId == null) return false;
  if (store.mic.you_speaker != null) return speakerId === store.mic.you_speaker;
  // `speakers.list` carries the same fact, and it is the one that survives a
  // daemon too old to answer `mic.get`.
  return store.speakers.get(speakerId)?.you === true;
}

/**
 * What the microphone card's chip says. Three states a person can act on, out
 * of the daemon's five: the two `always:*` collapse because "on and recording"
 * is the same thing to look at whichever mode got you there, and `always:idle`
 * is the honest "no device" case.
 */
export function micChip(state = store.mic.state) {
  switch (state) {
    case 'following:active':
    case 'always:active':
      return { text: 'capturing', cls: 'chip live', live: true };
    case 'following:idle':
      return { text: 'waiting for an allowed app', cls: 'chip' };
    case 'always:idle':
      return { text: 'no input device', cls: 'chip warn' };
    default:
      return { text: 'off', cls: 'chip' };
  }
}

/**
 * The sources a person opts in through the ALLOWLIST — everything except the
 * microphone (audit finding #25a).
 *
 * The mic is a source row on the wire (schema 4) but it is not one of these: it
 * hears the room rather than one program, it has its own card, its own method
 * and its own default. It used to be excluded by the Sources view and included
 * by the rail badge, so the badge read 2 or 1 for the same daemon depending on
 * which of the two happened to paint last. One rule, one place, both callers.
 */
export function appSources() {
  return store.sources.filter((s) => s.kind !== 'mic');
}

/** What the rail badge counts: allowed applications. */
export function allowedAppCount() {
  return appSources().filter((s) => s.allowed).length;
}

/**
 * Fold the main process's state push into the model.
 *
 * The status block is where audit finding #25b lived. Main sets `ui.status =
 * null` the moment the connection drops; the renderer took that with an
 * `if (st.status)` guard and simply kept the last one, so the footer went on
 * quoting "queue 3 · db 1.2 GB" from a daemon that was no longer there, right
 * next to the words "daemon offline".
 *
 * The block is still KEPT, because not all of it is a live reading: whether the
 * semantic model is installed and how much is on disk are facts about this
 * machine, and blanking them would trade one wrong claim ("semantic search is
 * not installed") for another. What changes is that it is now MARKED — the
 * counters are only quoted while a daemon is behind them (`liveStatus`).
 */
export function applyConnState(st) {
  store.conn = st?.conn ?? store.conn;
  store.paused = !!st?.paused;
  store.pausePending = !!st?.pausePending;
  if (st?.status) store.status = st.status;
  store.statusLive = st?.conn?.status === 'connected' && !!st?.status;
  store.update = st?.update ?? null;
  return store;
}

/**
 * The status block, but only while a daemon is answering. What the footer's
 * counters are drawn from: offline they read "—" and go grey rather than
 * presenting the last daemon's numbers as the current ones.
 */
export function liveStatus() {
  return store.statusLive ? store.status : null;
}

// -- the resident window ----------------------------------------------------

const byTime = (a, b) => a.t_ms - b.t_ms || a.id - b.id;

function dropOldest() {
  const seg = store.segments.shift();
  if (seg) store.segById.delete(seg.id);
  return seg;
}

function dropNewest() {
  const seg = store.segments.pop();
  if (seg) store.segById.delete(seg.id);
  return seg;
}

/** Where the reader is, as an index. The tail, unless the view said otherwise. */
function anchorIndex(fallbackId = null) {
  const id = store.window.anchor ?? fallbackId;
  if (id != null) {
    const i = store.segments.findIndex((s) => s.id === id);
    if (i >= 0) return i;
  }
  return store.segments.length - 1;
}

/**
 * The 20k ceiling, trimmed from whichever END is further from the viewport.
 *
 * This is the same rule as the tail trim, generalised: the rows nearest the
 * anchor are the ones being read, so they are the last thing that may go.
 * Reaching it at all is worth saying out loud, which is what `capped` is for.
 */
export function capWindow(fallbackAnchor = null) {
  let over = store.segments.length - HARD_MAX;
  if (over <= 0) return 0;
  const dropped = over;
  let i = anchorIndex(fallbackAnchor);
  let tookFromTheOldEnd = false;
  while (over > 0 && store.segments.length) {
    // Ties go to the old end: with no anchor at all this degrades to exactly
    // the tail behaviour, which is the safe default.
    if (i >= store.segments.length - 1 - i) {
      dropOldest();
      i -= 1;
      tookFromTheOldEnd = true;
    } else {
      dropNewest();
    }
    over -= 1;
  }
  store.window.capped = true;
  if (tookFromTheOldEnd) {
    store.window.beginning = false;
    store.window.firstMs = null;
  }
  return dropped;
}

/**
 * Trim the resident window after something was added to it.
 *
 * Following the tail: drop the oldest above MAX_SEGMENTS, as ever. Browsing:
 * drop nothing at all except at the hard ceiling, and take that from the far
 * end. There is no third case, and no path here trims towards the reader.
 */
export function trimWindow(fallbackAnchor = null) {
  let dropped = 0;
  if (store.window.following) {
    while (store.segments.length > MAX_SEGMENTS) {
      dropOldest();
      dropped += 1;
    }
    if (dropped) {
      store.window.beginning = false;
      store.window.firstMs = null;
    }
  }
  return dropped + capWindow(fallbackAnchor);
}

/**
 * Follow the live tail, or stop.
 *
 * Turning it back ON collapses the window to the newest MAX_SEGMENTS in one
 * step — an hour of scrollback is not something to keep paying for once the
 * reader has gone back to watching the conversation happen.
 */
export function setFollowing(on) {
  const next = !!on;
  store.window.following = next;
  if (next) {
    store.window.anchor = null;
    collapseToTail();
  }
  return next;
}

/**
 * Come back to the live tail from a window that is not near it — what Follow
 * does after the date picker took you to another day. One query, and the model
 * is the newest MAX_SEGMENTS again; collapsing a July window locally would
 * just leave you following July.
 */
export async function followTail(limit = MAX_SEGMENTS) {
  const res = await ask('transcript', { limit });
  const rows = [...(res.segments ?? [])].sort(byTime);
  store.segments = rows;
  store.segById = new Map(rows.map((s) => [s.id, s]));
  store.window = {
    following: true,
    anchor: null,
    capped: false,
    beginning: rows.length < limit,
    firstMs: rows[0]?.t_ms ?? null,
    detached: false,
  };
  return rows.length;
}

/** Back to the newest MAX_SEGMENTS, in one step. Returns how many rows went. */
export function collapseToTail() {
  let dropped = 0;
  while (store.segments.length > MAX_SEGMENTS) {
    dropOldest();
    dropped += 1;
  }
  if (dropped) {
    // The beginning is no longer loaded, and the ceiling is no longer near.
    store.window.beginning = false;
    store.window.firstMs = null;
  }
  store.window.capped = false;
  return dropped;
}

/**
 * Prepend a page of older history — the scrollback path.
 *
 * Deliberately NOT `trimWindow`: a prepend only ever happens while browsing,
 * and the tail trim would throw away the page that was just fetched. Only the
 * ceiling applies, anchored on the row that used to be first, so the far end
 * is the new one.
 */
export function prependSegments(list) {
  const wasFirst = store.segments[0]?.id ?? null;
  let added = 0;
  for (const seg of list ?? []) {
    if (store.segById.has(seg.id)) continue;
    store.segById.set(seg.id, seg);
    store.segments.push(seg);
    added += 1;
  }
  if (added) store.segments.sort(byTime);
  // Asking for older rows is browsing, whatever the button said a moment ago.
  // Structural, not a caller's responsibility: the tail trim must not be one
  // stray live segment away from undoing the page that was just fetched.
  if (added) store.window.following = false;
  capWindow(wasFirst);
  return added;
}

/**
 * Fold a page of history into the live window without disturbing what is
 * already there. Used when a search hit is older than the window: the answer
 * to "what did she say about that world?" is the conversation around the hit,
 * so the surrounding minutes are pulled in rather than replacing the view.
 *
 * Audit finding #12 lived in the last three lines of the old version: it
 * sorted the merged page in and then `shift()`ed the oldest rows off — which
 * is the merged page itself, discarded at the exact moment the window was full,
 * i.e. every time it mattered. Two things fix it and both are needed. Merging
 * rows older than the window IS a jump into the past, so it leaves the follow
 * state; and the trim is anchored on the oldest row merged, so the ceiling
 * takes from the other end. The caller cannot get this wrong by forgetting.
 */
export function mergeSegments(list) {
  const head = store.segments[0] ?? null;
  let added = 0;
  let oldest = null;
  for (const seg of list ?? []) {
    if (store.segById.has(seg.id)) continue;
    store.segById.set(seg.id, seg);
    store.segments.push(seg);
    if (oldest == null || byTime(seg, oldest) < 0) oldest = seg;
    added += 1;
  }
  if (added) store.segments.sort(byTime);
  if (oldest && head && oldest.t_ms < head.t_ms) store.window.following = false;
  trimWindow(oldest?.id ?? null);
  return added;
}

/**
 * Throw the window away and rebuild it from one page — the date picker's path.
 * You asked to be somewhere else, so you are somewhere else, and not following.
 */
export function replaceSegments(list) {
  store.segments = [...(list ?? [])].sort(byTime);
  store.segById = new Map(store.segments.map((s) => [s.id, s]));
  store.window = {
    following: false,
    anchor: store.segments[0]?.id ?? null,
    capped: false,
    beginning: false,
    firstMs: null,
    detached: true,
  };
  return store.segments.length;
}

/**
 * One page further back. Returns how many NEW rows arrived and whether that
 * was the beginning of the archive — a page shorter than asked for is the only
 * signal the protocol gives, and it is enough (PROTOCOL "Paging the transcript").
 */
export async function loadOlderPage(limit = PAGE_SEGMENTS) {
  const oldest = store.segments[0];
  if (!oldest || store.window.beginning) {
    return { added: 0, beginning: true, rows: 0 };
  }
  // `to` is exclusive, so the row we page from is not repeated.
  const res = await ask('transcript', { to: oldest.t_ms, limit });
  const rows = res.segments ?? [];
  const added = prependSegments(rows);
  const beginning = rows.length < limit;
  if (beginning) {
    store.window.beginning = true;
    store.window.firstMs = store.segments[0]?.t_ms ?? null;
  }
  return { added, beginning, rows: rows.length };
}

export async function reloadSpeakers() {
  const r = await ask('speakers.list');
  store.speakers = new Map((r.speakers ?? []).map((s) => [s.id, s]));
  return store.speakers;
}

/** The graph's counts, re-asked. What the rail badge is drawn from. */
export async function reloadGraph() {
  const g = await ask('graph.summary');
  store.graph = g;
  return g;
}

// -- event application ------------------------------------------------------

/**
 * Fold one daemon event into the model.
 * Returns a change hint the controller uses to decide what to repaint:
 *   {added:[seg], updated:[seg], purged:[id], relabel:[id], sources:bool,
 *    status:bool, ops:bool}
 * Unknown event types return null — PROTOCOL "Versioning rules".
 */
// A freshly MINTED voice reaches clients only inside the segment that minted
// it — there is no relabel broadcast for coming into existence — so an unknown
// speaker id on a segment is the cue to re-pull the list. Debounced: a burst
// of segments from a new voice costs one query.
let speakerRefresh = null;
function noteUnknownSpeaker(id, onDone) {
  if (id == null || store.speakers.has(id) || speakerRefresh) return;
  speakerRefresh = setTimeout(() => {
    speakerRefresh = null;
    reloadSpeakers()
      .then(() => onDone?.())
      .catch(() => {});
  }, 250);
}

export function applyEvent(evt, opts = {}) {
  const d = evt?.data;
  switch (evt?.ev) {
    case 'segment': {
      if (!d || d.id == null) return null;
      noteUnknownSpeaker(d.speaker, opts.onSpeakersChanged);
      const known = store.segById.get(d.id);
      if (known) {
        Object.assign(known, d);
        return { updated: [known] };
      }
      store.appended += 1;
      bumpCount(d.speaker, +1, d.dur_ms);
      // The window is somewhere else entirely — the date picker rebuilt it on
      // another day. Tonight's rows must not pile up under July's, and the
      // tail is one query away the moment Follow comes back on.
      if (store.window.detached) return { detached: true };
      store.segById.set(d.id, d);
      // The feed is chronological in practice, but a corrected or late segment
      // must not jump the list out of order.
      const last = store.segments[store.segments.length - 1];
      if (!last || d.t_ms >= last.t_ms) store.segments.push(d);
      else {
        const i = store.segments.findIndex((s) => s.t_ms > d.t_ms);
        store.segments.splice(i < 0 ? store.segments.length : i, 0, d);
      }
      // Bounded while following the tail, unbounded (to the ceiling) while the
      // reader is up in history: a live row arriving must never be the thing
      // that scrolls the page they are reading out from under them.
      trimWindow();
      return { added: [d] };
    }

    case 'purge': {
      const ids = Array.isArray(d?.ids) ? d.ids : [];
      for (const id of ids) {
        const seg = store.segById.get(id);
        if (!seg) continue;
        bumpCount(seg.speaker, -1, -seg.dur_ms);
        store.segById.delete(id);
      }
      const set = new Set(ids);
      store.segments = store.segments.filter((s) => !set.has(s.id));
      return { purged: ids };
    }

    case 'relabel': {
      if (!d || d.speaker == null) return null;
      // Two shapes on one event (PROTOCOL): a rename, and a merge that
      // tombstones one id into another. Both are retroactive and neither
      // requires the client to re-query.
      if (d.merged_into != null) {
        const from = d.speaker;
        const into = d.merged_into;
        const a = store.speakers.get(from);
        const b = store.speakers.get(into);
        if (a && b) {
          b.segments = (b.segments ?? 0) + (a.segments ?? 0);
          b.total_ms = (b.total_ms ?? 0) + (a.total_ms ?? 0);
        }
        store.speakers.delete(from);
        for (const seg of store.segments) if (seg.speaker === from) seg.speaker = into;
        return { relabel: [from, into], merged: { from, into } };
      }
      // A swept one-off voice (0.6.1): the identity is gone, not merged, and
      // its rows arrive separately as a `purge`.
      if (d.pruned) {
        store.speakers.delete(d.speaker);
        for (const seg of store.segments) if (seg.speaker === d.speaker) seg.speaker = null;
        return { relabel: [d.speaker], speakers: true };
      }
      const sp = store.speakers.get(d.speaker);
      if (sp) {
        sp.name = d.name ?? null;
        // Only when the event carries it: a plain rename must not silently
        // clear a language declaration it never mentioned.
        if (d.languages !== undefined) sp.languages = d.languages;
      } else {
        store.speakers.set(d.speaker, {
          id: d.speaker,
          name: d.name ?? null,
          auto: null,
          languages: d.languages ?? null,
          segments: 0,
          total_ms: 0,
        });
      }
      return { relabel: [d.speaker] };
    }

    case 'source': {
      if (!d?.match_key) return null;
      const s = store.sources.find((x) => x.match_key === d.match_key);
      if (s) Object.assign(s, d);
      else store.sources.push(d);
      return { sources: true };
    }

    case 'status': {
      store.status = d ?? null;
      // An event only ever arrives over a live socket, so this block is a
      // reading by definition.
      store.statusLive = !!d;
      // The daemon's status block carries the mic too, so a client that missed
      // a `mic` event still converges on the truth.
      if (d?.mic) applyMic(d.mic);
      // …and the graph worker's state, for exactly the same reason (0.7.0).
      if (d?.graph) store.graph = { ...store.graph, enrichment: d.graph };
      return { status: true, mic: true, graph: d?.graph ?? null };
    }

    // The memory graph's Tier 3 worker, on the status topic. It arrives when
    // the switch moves and while a batch runs, so the Memory card follows a
    // pass without that view polling anything.
    case 'graph': {
      if (!d) return null;
      store.graph = { ...store.graph, enrichment: d };
      return { graph: d };
    }

    // A commitment moved states — here, in the CLI, or in another window. It
    // is retroactive like a rename, so it is a broadcast and not a re-query.
    //
    // The counts behind the rail badge are NOT adjusted here. They are the
    // daemon's arithmetic and this client does not get to guess at it; the
    // controller re-asks `graph.summary`, which is one small query and is
    // always right.
    case 'commitment':
      return !d || d.id == null ? null : { commitment: d };

    // PROTOCOL: the mic block, on the status topic. It arrives both when the
    // switch moves and when the capture thread opens or closes the stream —
    // the latter is the only way a follow-mode transition is visible.
    case 'mic': {
      if (!d) return null;
      applyMic(d);
      return { mic: true };
    }

    case 'op.progress': {
      if (!d?.op) return null;
      store.ops.set(d.op, { kind: d.kind, frac: d.frac ?? 0, done: false });
      return { ops: true };
    }
    case 'op.done':
    case 'op.failed': {
      if (!d?.op) return null;
      store.ops.delete(d.op);
      return { ops: true, opFinished: { ...d, failed: evt.ev === 'op.failed' } };
    }

    case 'roster':
      // Stored for the session view that Step 5 adds; nothing renders it yet,
      // and an unknown event must never be an error.
      return null;

    default:
      return null;
  }
}

function bumpCount(speakerId, n, ms) {
  if (speakerId == null) return;
  const sp = store.speakers.get(speakerId);
  if (!sp) return;
  sp.segments = Math.max(0, (sp.segments ?? 0) + n);
  sp.total_ms = Math.max(0, (sp.total_ms ?? 0) + (ms ?? 0));
}

// -- onboarding -------------------------------------------------------------

/**
 * DESIGN §5: "onboarding is naming". Two named voices covered 38% of all speech
 * in the field recording, so the first-run flow is not a settings page — it is
 * "these voices are most of your conversations, who are they?". This decides
 * when to ask: when unnamed speakers hold a real share of total speech time.
 */
export function onboardingCandidates({ minShare = 0.25, top = 3 } = {}) {
  // "Who are they?" is not a question about yourself. The pinned voice is the
  // one identity in the bank that was never guessed at, so it is out of the
  // naming flow entirely — out of the candidates AND out of the denominator,
  // because a user who talks a lot must not be able to suppress the question
  // about everybody else.
  const all = [...store.speakers.values()].filter((s) => !isYou(s.id));
  const total = all.reduce((n, s) => n + (s.total_ms ?? 0), 0);
  if (!total) return { show: false, share: 0, speakers: [] };
  const unnamed = all.filter((s) => !isNamed(s)).sort((a, b) => (b.total_ms ?? 0) - (a.total_ms ?? 0));
  const unnamedMs = unnamed.reduce((n, s) => n + (s.total_ms ?? 0), 0);
  const share = unnamedMs / total;
  const picks = unnamed.slice(0, top).filter((s) => (s.total_ms ?? 0) > 0);
  return { show: share >= minShare && picks.length > 0, share, speakers: picks, total };
}
