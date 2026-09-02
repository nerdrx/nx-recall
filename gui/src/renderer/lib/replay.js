// Conversation replay — a thread, played back, with the transcript following.
//
// The per-segment preview (lib/preview.js) answers "what did that one sound
// like?". This answers "what was that conversation?": the turns in order, the
// playing row lit and kept in view, a bar that says who is talking and how far
// through you are. It is the difference between reading a transcript and being
// in the room again.
//
// Three rules shape everything here.
//
// 1. **The transcript still reads through a silent turn.** Audio retention is
//    shorter than the transcript on purpose (DESIGN §8), so a conversation from
//    July is words with the sound gone. A player that skipped those rows would
//    race through the parts of the evening it could not play, which is exactly
//    backwards — the words are the record. So a turn with no audio is held for
//    its own duration, at the current rate, and the scrubber shows a tick where
//    the sound used to be. The bar says this ONCE for the whole conversation,
//    not once per row.
//
// 2. **Dead air is not replayed.** Real gaps between turns run to tens of
//    seconds and nobody wants to sit through them, so a gap is compressed to at
//    most GAP_MAX_MS. Enough to hear the turn-taking, not enough to be a wait.
//
// 3. **There is one sound in this app.** The element and the ownership token
//    are preview.js's, taken through `acquire()`, so a replay starting stops a
//    row preview and a row preview starting stops the replay — with no
//    coordination between the two beyond that one seam.
//
// No DOM lives here. The engine emits state and the transcript view paints it,
// which is what lets the same replay survive a repaint of the rows under it.

import { ask } from './store.js';
import { acquire } from './preview.js';

/** The longest silence replay will sit through between two turns. */
export const GAP_MAX_MS = 700;

/** The rates the bar offers. 1× first, because that is what happened. */
export const RATES = [1, 1.5, 2];

/** How many turns ahead the next clip is fetched. One: it is the next one. */
const PREFETCH = 1;

const listeners = new Set();

function blank() {
  return {
    active: false,
    thread: null,
    turns: [],
    index: 0,
    /** 'idle' | 'loading' | 'playing' | 'silent' | 'paused' | 'ended' */
    phase: 'idle',
    rate: 1,
    /** How many turns in this conversation have lost their audio. */
    missing: 0,
    error: null,
  };
}

let st = blank();
// Bumped by every start, close, seek and step. An async step whose generation
// is stale belongs to a playhead that has already moved, and does nothing.
let run = 0;
let handle = null;
let waitCtl = null; // the pausable sleep currently parked, if any
const clips = new Map(); // segment id → Promise<clip | null>

export function onReplay(fn) {
  listeners.add(fn);
  return () => listeners.delete(fn);
}

export function replayState() {
  return { ...st, turns: st.turns.map((t) => ({ ...t })) };
}

export function isReplaying() {
  return st.active;
}

/** The turn the playhead is on, or null. */
export function currentTurn() {
  return st.turns[st.index] ?? null;
}

function emit() {
  const snap = replayState();
  for (const fn of [...listeners]) {
    try {
      fn(snap);
    } catch (e) {
      console.error('[recall] replay listener threw', e);
    }
  }
}

function set(patch) {
  st = { ...st, ...patch };
  emit();
}

// ---------------------------------------------------------------------------
// the pausable sleep — a silent turn, and every compressed gap
// ---------------------------------------------------------------------------

/**
 * Wait `ms`, pausably. Resolves `'ended'` or `'stopped'`.
 *
 * A plain `setTimeout` would keep running while the user has the thing paused,
 * and the playhead would jump forward the moment they came back. This keeps the
 * remaining time and re-arms it on resume, which is what makes a silent turn
 * behave like a sounding one.
 */
function wait(ms, gen) {
  return new Promise((resolve) => {
    let remaining = Math.max(0, ms);
    let armedAt = 0;
    let id = null;
    const finish = (why) => {
      if (id) clearTimeout(id);
      id = null;
      if (waitCtl?.own === finish) waitCtl = null;
      resolve(why);
    };
    const arm = () => {
      if (id) return;
      armedAt = Date.now();
      id = setTimeout(() => finish('ended'), remaining);
    };
    waitCtl = {
      own: finish,
      pause() {
        if (!id) return;
        clearTimeout(id);
        id = null;
        remaining = Math.max(0, remaining - (Date.now() - armedAt));
      },
      resume: arm,
      cancel: () => finish('stopped'),
    };
    if (gen !== run) return finish('stopped');
    if (st.phase !== 'paused') arm();
  });
}

// ---------------------------------------------------------------------------
// audio
// ---------------------------------------------------------------------------

/**
 * One turn's WAV, or `null` if it cannot be had.
 *
 * `has_audio` from `replay.get` is a snapshot and never a promise: retention
 * can sweep between the query and this fetch, so `err:gone` here is an ordinary
 * answer and not a failure. Either way the turn becomes a silent one and the
 * scrubber's tick updates to say so.
 */
function clipFor(id) {
  if (clips.has(id)) return clips.get(id);
  const p = ask('segments.audio', { id }).catch(() => null);
  clips.set(id, p);
  // The cache exists to make "the next turn" instant, not to hold a
  // conversation of WAVs in memory. Two entries is the whole working set.
  for (const key of [...clips.keys()].slice(0, Math.max(0, clips.size - (PREFETCH + 1)))) {
    if (key !== id) clips.delete(key);
  }
  return p;
}

function prefetchFrom(index) {
  for (let i = index; i < Math.min(st.turns.length, index + PREFETCH); i += 1) {
    if (st.turns[i]?.has_audio) clipFor(st.turns[i].id);
  }
}

/** A turn that turned out to have no sound. Counted once, said once. */
function wentSilent(turn) {
  if (!turn.has_audio) return;
  turn.has_audio = false;
  set({ missing: st.missing + 1, turns: st.turns });
}

// ---------------------------------------------------------------------------
// the playhead
// ---------------------------------------------------------------------------

/**
 * Has this playhead been overtaken? Either the generation moved (a jump, a
 * close, another replay) or the ELEMENT was taken from under us — a row preview
 * starting is not a thing replay gets to ignore, and a replay parked on a
 * silent turn has no clip whose end would tell it so. Checked after every
 * await, which is the only place either can happen.
 */
function overtaken(gen) {
  return gen !== run || !handle?.alive;
}

async function playFrom(gen) {
  while (!overtaken(gen) && st.index < st.turns.length) {
    const turn = st.turns[st.index];
    handle?.mark(st.index, st.turns.length, 'loading');
    prefetchFrom(st.index + 1);

    let why = 'ended';
    if (turn.has_audio) {
      set({ phase: 'loading', error: null });
      const clip = await clipFor(turn.id);
      if (gen !== run) return;
      if (!clip?.wav_b64) {
        wentSilent(turn);
      } else {
        if (handle) handle.audio.playbackRate = st.rate;
        set({ phase: 'playing' });
        handle?.mark(st.index, st.turns.length, 'playing');
        why = (await handle?.load(clip.wav_b64, clip.duration_ms)) ?? 'stopped';
        if (overtaken(gen)) return void stoppedElsewhere(gen);
        // A clip the element refused is a turn to read through, not a turn to
        // drop: the words are still on screen and still worth the seconds.
        if (why === 'corrupt' || why === 'error') wentSilent(turn);
        else if (why === 'stopped') return void stoppedElsewhere(gen);
      }
    }

    if (!turn.has_audio) {
      set({ phase: 'silent' });
      handle?.mark(st.index, st.turns.length, 'playing');
      if ((await wait((turn.dur_ms || 0) / st.rate, gen)) === 'stopped') return;
    }
    if (overtaken(gen)) return void stoppedElsewhere(gen);

    const next = st.turns[st.index + 1];
    if (!next) break;
    // Rule 2: the room's real silences, compressed to something bearable.
    const gap = Math.min(GAP_MAX_MS, Math.max(0, next.t_ms - (turn.t_ms + (turn.dur_ms || 0))));
    if (gap && (await wait(gap / st.rate, gen)) === 'stopped') return;
    if (overtaken(gen)) return void stoppedElsewhere(gen);
    set({ index: st.index + 1 });
  }
  if (overtaken(gen)) return void stoppedElsewhere(gen);
  finish();
}

/**
 * The element was taken by something else — a row preview, a view change.
 * Replay does not fight for it; it stands down and says so.
 */
function stoppedElsewhere(gen) {
  if (gen !== run) return;
  close();
}

/**
 * The end of the conversation. The bar stays rather than vanishing — you have
 * just listened to something and "play it again" is the next thing anybody
 * wants — so the element is HELD and merely paused. Giving it back here would
 * make the play button at the end of a replay do nothing at all.
 */
function finish() {
  run += 1;
  clips.clear();
  const at = Math.max(0, st.turns.length - 1);
  handle?.mark(at, st.turns.length, 'paused');
  set({ phase: 'ended', index: at });
}

// ---------------------------------------------------------------------------
// the controls
// ---------------------------------------------------------------------------

/**
 * Play this conversation. `from` is a segment id to start on — a search hit
 * replays the thread from the line you searched for, not from the top.
 *
 * Never throws: this is called straight out of a click.
 */
export async function start(thread, { from = null, rate = null } = {}) {
  close();
  const gen = ++run;
  let res;
  try {
    res = await ask('replay.get', { thread });
  } catch (e) {
    st = { ...blank(), error: e.code ?? 'failed' };
    emit();
    return { ok: false, error: e.code ?? 'failed', message: e.message };
  }
  if (gen !== run) return { ok: false, error: 'stopped' };
  const turns = (res?.turns ?? []).map((t) => ({
    id: Number(t.id),
    t_ms: Number(t.t_ms) || 0,
    dur_ms: Number(t.dur_ms) || 0,
    speaker: t.speaker ?? null,
    speaker_name: t.speaker_name ?? null,
    text: t.text ?? '',
    has_audio: !!t.has_audio,
  }));
  if (!turns.length) {
    st = { ...blank(), error: 'empty' };
    emit();
    return { ok: false, error: 'empty' };
  }
  const at = from == null ? 0 : Math.max(0, turns.findIndex((t) => t.id === Number(from)));
  handle = acquire(`replay:${thread}`);
  st = {
    ...blank(),
    active: true,
    thread,
    turns,
    index: at,
    phase: 'loading',
    rate: RATES.includes(rate) ? rate : 1,
    missing: turns.filter((t) => !t.has_audio).length,
  };
  emit();
  void playFrom(gen);
  return { ok: true, thread, turns: turns.length, index: at, missing: st.missing };
}

/** Space. Returns the phase it landed in. */
export function toggle() {
  if (!st.active) return st.phase;
  if (st.phase === 'ended') {
    void jump(0);
    return 'loading';
  }
  if (st.phase === 'paused') {
    set({ phase: st.turns[st.index]?.has_audio ? 'playing' : 'silent' });
    handle?.mark(st.index, st.turns.length, 'playing');
    waitCtl?.resume();
    const p = handle?.audio?.play?.();
    if (p?.catch) p.catch(() => {});
    return st.phase;
  }
  waitCtl?.pause();
  handle?.audio?.pause?.();
  set({ phase: 'paused' });
  handle?.mark(st.index, st.turns.length, 'paused');
  return 'paused';
}

/** Move the playhead. The clip cache survives, so stepping back is instant. */
export function jump(index) {
  if (!st.active) return null;
  const at = Math.max(0, Math.min(st.turns.length - 1, Math.trunc(index)));
  const gen = ++run;
  waitCtl?.cancel();
  // Silence what is sounding without giving the element up — `stop()` would
  // bump preview's own token and kill the handle we are about to keep using.
  handle?.cancelClip();
  set({ index: at, phase: 'loading' });
  void playFrom(gen);
  return at;
}

export const next = () => jump(st.index + 1);
export const prev = () => jump(st.index - 1);

/** 1× / 1.5× / 2×, applied to the clip playing right now as well as the rest. */
export function setRate(rate) {
  if (!RATES.includes(rate)) return st.rate;
  set({ rate });
  if (handle?.audio) handle.audio.playbackRate = rate;
  return rate;
}

/** Step to the next rate in the list, wrapping. What the button does. */
export function cycleRate() {
  return setRate(RATES[(RATES.indexOf(st.rate) + 1) % RATES.length]);
}

/** Close the bar and give the sound back. Safe to call when nothing is playing. */
export function close() {
  const was = st.active;
  run += 1;
  waitCtl?.cancel();
  waitCtl = null;
  clips.clear();
  // Only if it is still ours: once a row preview has taken the element,
  // stopping it would silence somebody else's click.
  if (handle?.alive) handle.release();
  handle = null;
  st = blank();
  if (was) emit();
  return was;
}

// ---------------------------------------------------------------------------
// what the bar says
// ---------------------------------------------------------------------------

/**
 * Rule 1, in words, once. `null` when every turn in this conversation still has
 * its audio — which is the common case for anything recent, and a line that
 * only appears when it is true is a line people read.
 */
export function missingNote(state = st) {
  const n = state.missing;
  if (!n) return null;
  if (n === state.turns.length) {
    return 'No audio is left for this conversation — replay reads through it at the pace it was said.';
  }
  return `${n} turn${n === 1 ? '' : 's'} here lost ${n === 1 ? 'its' : 'their'} audio to retention — replay reads through ${n === 1 ? 'it' : 'them'} at the pace it was said.`;
}

/** Why a replay would not start, in a sentence. */
export function startError(error) {
  if (error === 'empty') return 'There is nothing left of that conversation to play.';
  if (error === 'not_found') return 'That conversation is no longer in the transcript.';
  return error ? `Could not replay that conversation — ${error}` : '';
}
