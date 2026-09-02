// Voice preview — the one place in the app that makes a sound.
//
// The Speakers view asks "who is this?" and, until now, gave no way to listen.
// This is that: fetch a segment's WAV over the protocol (`segments.audio`,
// base64 in the reply), wrap it in a blob URL, and play it through a single
// shared <audio>. One preview at a time, always — two voices talking over each
// other is the exact confusion this feature exists to resolve.
//
// Nothing here reaches the network: the bytes come from the unix socket via the
// main process, and a blob: URL is same-origin by construction. The CSP in
// index.html allows `media-src blob:` and nothing more.

import { ask } from './store.js';

// A stuck element must not leave a row spinning forever. Real clips are
// seconds long; this is the ceiling on waiting for one to end.
const CLIP_TIMEOUT_MIN = 5000;
const CLIP_TIMEOUT_SLACK = 4000;
const CLIP_TIMEOUT_MAX = 60000;
/** How often the stall guard looks. Small enough not to blunt the ceiling. */
const CLIP_TICK_MS = 250;

let el = null;
let blobUrl = null;
// Bumped by every stop() and every play(). An async step whose token is stale
// belongs to a preview the user has already moved on from, and does nothing.
let token = 0;

const state = { key: null, phase: 'idle', index: 0, total: 0 };
const listeners = new Set();

// Everything currently parked on "tell me when this clip is over".
//
// `stop()` detaches the element's handlers and pauses it, which means a waiter
// registered before it would never hear anything again — the per-segment
// preview gets away with that because its loop re-checks the token on the way
// out, but conversation replay (lib/replay.js) awaits one turn at a time and
// would sit here until a timeout it no longer cares about. So a stop resolves
// every waiter at once, with `'stopped'`.
const waiters = new Set();

function settleWaiters(why) {
  for (const fn of [...waiters]) {
    waiters.delete(fn);
    fn(why);
  }
}

/** Subscribe to playback changes. Returns an unsubscribe function. */
export function onPlayback(fn) {
  listeners.add(fn);
  return () => listeners.delete(fn);
}

export function playbackState() {
  return { ...state };
}

/** Is this key (a row, a sheet, a banner button) the one currently sounding? */
export function isActive(key) {
  return state.phase !== 'idle' && state.key === key;
}

function emit() {
  const snap = playbackState();
  for (const fn of [...listeners]) {
    try {
      fn(snap);
    } catch (e) {
      console.error('[recall] preview listener threw', e);
    }
  }
}

function element() {
  if (el) return el;
  el = new Audio();
  el.preload = 'auto';
  // Exposed for the headless driver, which asserts that the element really
  // reached a playing state rather than that the UI said it had.
  if (typeof window !== 'undefined') window.__recallAudio = el;
  return el;
}

function release() {
  if (!blobUrl) return;
  URL.revokeObjectURL(blobUrl);
  blobUrl = null;
}

function idle() {
  state.key = null;
  state.phase = 'idle';
  state.index = 0;
  state.total = 0;
}

/** Stop whatever is playing. Safe to call when nothing is. */
export function stop() {
  token += 1;
  if (el) {
    el.onplaying = null;
    el.onended = null;
    el.onerror = null;
    el.pause();
    el.removeAttribute('src');
  }
  settleWaiters('stopped');
  release();
  if (state.phase !== 'idle') {
    idle();
    emit();
  }
}

// Throws on anything that is not base64 — which is the point: the caller has to
// treat a malformed reply as a failed clip rather than as an exception escaping
// a click handler (audit finding #25c).
function toBlob(b64) {
  if (typeof b64 !== 'string' || !b64) throw new Error('no audio in the reply');
  const bin = atob(b64);
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i += 1) bytes[i] = bin.charCodeAt(i);
  return new Blob([bytes], { type: 'audio/wav' });
}

// Resolves with 'ended' | 'error' | 'timeout'. `phase` only becomes 'playing'
// when the element itself says so, so a row's indicator means sound, not hope.
function playOne(url, durationMs, mine) {
  return new Promise((resolve) => {
    const a = element();
    let timer = null;
    const done = (why) => {
      if (timer) clearInterval(timer);
      waiters.delete(done);
      a.onplaying = null;
      a.onended = null;
      a.onerror = null;
      resolve(why);
    };
    waiters.add(done);
    a.onplaying = () => {
      if (token !== mine || state.phase === 'playing') return;
      state.phase = 'playing';
      emit();
    };
    a.onended = () => done('ended');
    a.onerror = () => done('error');
    a.src = url;
    // The guard is against a STUCK element, and an element somebody paused on
    // purpose is not stuck (conversation replay has a pause button, and a
    // wall-clock deadline would fire in the middle of a paused turn and advance
    // the playhead under the user). So the budget only burns while the clip is
    // either running or has not started at all — a pause AFTER the first frame
    // freezes it, exactly like the sound it is guarding.
    let budget = Math.min(
      CLIP_TIMEOUT_MAX,
      Math.max(CLIP_TIMEOUT_MIN, (durationMs || 0) + CLIP_TIMEOUT_SLACK)
    );
    timer = setInterval(() => {
      if (a.paused && (a.currentTime > 0 || state.phase === 'paused')) return;
      budget -= CLIP_TICK_MS;
      if (budget <= 0) done('timeout');
    }, CLIP_TICK_MS);
    const started = a.play();
    if (started?.catch) started.catch(() => done('error'));
  });
}

/**
 * Play these segments in order under `key`. Resolves when the sequence
 * finishes, is interrupted, or gives up.
 *
 * Never throws: a preview that fails is a hint in the UI, not an exception in
 * a click handler. The result says what happened —
 *   {played, stopped, error: null | 'gone' | 'empty' | 'corrupt' | <daemon code>}
 * A `gone` from one clip does not abandon the rest: the samples are evidence
 * for a naming decision, and partial evidence still helps.
 */
export async function play(key, segmentIds) {
  stop();
  const mine = token;
  const list = (segmentIds ?? []).map(Number).filter(Number.isFinite);
  if (!list.length) return { played: 0, stopped: false, error: 'empty' };

  state.key = key;
  state.phase = 'loading';
  state.index = 0;
  state.total = list.length;
  emit();

  let played = 0;
  let error = null;
  for (let i = 0; i < list.length; i += 1) {
    if (token !== mine) return { played, stopped: true, error: null };
    state.index = i;
    if (state.phase !== 'loading') {
      state.phase = 'loading';
      emit();
    }

    let clip;
    try {
      clip = await ask('segments.audio', { id: list[i] });
    } catch (e) {
      error = error ?? e.code ?? 'failed';
      continue;
    }
    if (token !== mine) return { played, stopped: true, error: null };

    release();
    // `atob` throws on a truncated or non-base64 payload, and this function is
    // documented "never throws" — so it used to reject out of a click handler
    // and strand the row in `loading` with no hint and no way back (audit
    // finding #25c). A clip that cannot be decoded is a failed clip, exactly
    // like one the daemon refused, and the rest of the sequence still plays.
    try {
      blobUrl = URL.createObjectURL(toBlob(clip?.wav_b64));
    } catch {
      error = error ?? 'corrupt';
      continue;
    }
    const why = await playOne(blobUrl, clip.duration_ms, mine);
    if (token !== mine) return { played, stopped: true, error: null };
    if (why === 'ended') played += 1;
    else error = error ?? 'playback';
  }

  if (token === mine) {
    release();
    idle();
    emit();
  }
  return { played, stopped: false, error };
}

/**
 * "Play me this voice." Asks the daemon which clips are worth hearing, then
 * plays them in sequence — the evidence a person needs to put a name to a
 * voice, in one action.
 *
 * `error: 'empty'` means the voice has no audio left at all (retention), which
 * the caller renders as a fact about the setting rather than a failure.
 */
export async function playSpeaker(key, speakerId, { limit = 3 } = {}) {
  stop();
  let res;
  try {
    res = await ask('speakers.sample', { id: speakerId, limit });
  } catch (e) {
    return { played: 0, stopped: false, error: e.code ?? 'failed', samples: [] };
  }
  const samples = res?.samples ?? [];
  if (!samples.length) return { played: 0, stopped: false, error: 'empty', samples };
  const out = await play(key, samples.map((s) => s.segment_id));
  return { ...out, samples };
}

/**
 * Take the one audio element, for a caller that needs to drive it itself.
 *
 * Conversation replay (lib/replay.js) plays a whole thread turn by turn, with
 * pause, a rate and a scrubber, so it cannot go through `play()` — but it must
 * never be a SECOND sound. This is that seam: it takes the same element under
 * the same token, so starting a replay stops a preview and starting a preview
 * stops a replay, with no coordination between them beyond this module.
 *
 * The handle goes dead the moment anything else calls `stop()` or `play()`.
 * Every caller has to check `alive` after every await; `load()` resolving
 * `'stopped'` says the same thing.
 */
export function acquire(key) {
  stop();
  const mine = token;
  state.key = key;
  state.phase = 'loading';
  state.index = 0;
  state.total = 0;
  emit();
  return {
    get alive() {
      return token === mine;
    },
    /** The shared element, for `playbackRate`, `pause()` and `play()`. */
    audio: element(),
    /** Say where in the sequence this is, for anything watching playback. */
    mark(index, total, phase) {
      if (token !== mine) return;
      state.index = index;
      state.total = total;
      if (phase) state.phase = phase;
      emit();
    },
    /**
     * Decode a base64 WAV and play it through. Resolves
     * `'ended' | 'error' | 'timeout' | 'stopped' | 'corrupt'` and never throws,
     * for the same reason `play()` does not: a clip that will not decode is a
     * turn to read through, not an exception out of a keypress.
     */
    async load(b64, durationMs) {
      if (token !== mine) return 'stopped';
      release();
      try {
        blobUrl = URL.createObjectURL(toBlob(b64));
      } catch {
        return 'corrupt';
      }
      return playOne(blobUrl, durationMs, mine);
    },
    /**
     * Abandon the clip in flight WITHOUT giving the element up — what a jump
     * to another turn needs. `stop()` would bump the token and kill this very
     * handle, and the caller is not finished with it.
     */
    cancelClip() {
      if (token !== mine) return;
      if (el) el.pause();
      settleWaiters('stopped');
      release();
    },
    /** Give the element back. A no-op once something else has taken it. */
    release() {
      if (token !== mine) return;
      stop();
    },
  };
}

/** The message a `gone`/empty preview shows in place, never as a toast alone. */
export function noAudioHint(error) {
  if (error === 'empty' || error === 'gone') return 'no audio kept for this voice (retention)';
  if (error === 'refused') return 'that clip is too large to preview';
  if (error === 'playback') return 'this clip could not be played';
  // The daemon answered, but not with audio anybody can decode.
  if (error === 'corrupt') return 'that clip came back unreadable';
  return error ? `could not play that — ${error}` : '';
}
