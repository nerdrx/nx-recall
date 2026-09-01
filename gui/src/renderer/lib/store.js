// The renderer's whole model. It is a cache of the daemon's answers and
// nothing more: on `resync` it is thrown away and rebuilt from queries, which
// is what DESIGN §2 means by "clients are dumb views that survive daemon
// restarts". No state here is authoritative and none of it is persisted.

const OVERLAP_REFUSE = 0.1; // DESIGN §4: embeddings only run at overlap_frac ≤ 0.1
const WEAK_MATCH = 0.4; // below this the label is a guess worth flagging
const MAX_SEGMENTS = 600; // the live view is a window, not an archive

export const store = {
  conn: { status: 'offline', daemon: null, seq: null, error: null, socketPath: null },
  paused: false,
  pausePending: false,
  status: null,
  update: null, // {from, to} — the daemon came back as a different version

  speakers: new Map(), // id → {id, name, auto, segments, total_ms, first_seen, you}
  segments: [], // ascending by t_ms
  segById: new Map(),
  sources: [],
  // The microphone switch (PROTOCOL "The microphone"). Not a source rule: it
  // hears the room rather than one program, so it has its own method, its own
  // card, and its own default (off).
  mic: { enabled: false, mode: 'follow', active: false, state: 'off', device: null, you_speaker: null },
  ops: new Map(), // op id → {kind, frac, done}

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
  if (seg.match_score != null && seg.match_score < WEAK_MATCH) return true;
  return false;
}

// What the "?" says. It has to name the actual reason: "we are not sure" is
// useless, "two people were talking at once so the identity was refused" tells
// the user why the fix is theirs to make.
export function uncertainReason(seg) {
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
  return `Weak voice match (${(seg.match_score ?? 0).toFixed(2)}). Click to correct it.`;
}

// -- queries ----------------------------------------------------------------

async function ask(method, params) {
  const res = await window.recall.request(method, params);
  if (!res?.ok) throw Object.assign(new Error(res?.err?.msg || 'request failed'), { code: res?.err?.code });
  return res.data;
}

export { ask };

/** Full resync: every view's data re-fetched from scratch. */
export async function reloadAll() {
  const [speakers, transcript, sources, mic] = await Promise.all([
    ask('speakers.list').catch(() => ({ speakers: [] })),
    ask('transcript', { limit: MAX_SEGMENTS }).catch(() => ({ segments: [] })),
    ask('sources.list').catch(() => ({ sources: [] })),
    // A daemon older than 0.6.0 has no mic at all; its `unknown_method` is not
    // an error worth showing, it is just an older half of the app.
    ask('mic.get').catch(() => null),
  ]);

  store.speakers = new Map((speakers.speakers ?? []).map((s) => [s.id, s]));
  store.segments = [...(transcript.segments ?? [])].sort((a, b) => a.t_ms - b.t_ms);
  store.segById = new Map(store.segments.map((s) => [s.id, s]));
  store.sources = sources.sources ?? [];
  if (mic) applyMic(mic);
  store.loaded = true;
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
 * Fold a page of history into the live window without disturbing what is
 * already there. Used when a search hit is older than the window: the answer
 * to "what did she say about that world?" is the conversation around the hit,
 * so the surrounding minutes are pulled in rather than replacing the view.
 */
export function mergeSegments(list) {
  let added = 0;
  for (const seg of list ?? []) {
    if (store.segById.has(seg.id)) continue;
    store.segById.set(seg.id, seg);
    store.segments.push(seg);
    added += 1;
  }
  if (added) store.segments.sort((a, b) => a.t_ms - b.t_ms);
  while (store.segments.length > MAX_SEGMENTS) {
    const drop = store.segments.shift();
    store.segById.delete(drop.id);
  }
  return added;
}

export async function reloadSpeakers() {
  const r = await ask('speakers.list');
  store.speakers = new Map((r.speakers ?? []).map((s) => [s.id, s]));
  return store.speakers;
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
      store.segById.set(d.id, d);
      // The feed is chronological in practice, but a corrected or late segment
      // must not jump the list out of order.
      const last = store.segments[store.segments.length - 1];
      if (!last || d.t_ms >= last.t_ms) store.segments.push(d);
      else {
        const i = store.segments.findIndex((s) => s.t_ms > d.t_ms);
        store.segments.splice(i < 0 ? store.segments.length : i, 0, d);
      }
      while (store.segments.length > MAX_SEGMENTS) {
        const drop = store.segments.shift();
        store.segById.delete(drop.id);
      }
      bumpCount(d.speaker, +1, d.dur_ms);
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
      const sp = store.speakers.get(d.speaker);
      if (sp) sp.name = d.name ?? null;
      else store.speakers.set(d.speaker, { id: d.speaker, name: d.name ?? null, auto: null, segments: 0, total_ms: 0 });
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
      // The daemon's status block carries the mic too, so a client that missed
      // a `mic` event still converges on the truth.
      if (d?.mic) applyMic(d.mic);
      return { status: true, mic: true };
    }

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
