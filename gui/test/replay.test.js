// Conversation replay's engine, without a window.
//
// Everything that decides whether a replay is CORRECT is timing and ownership,
// and neither needs a DOM: that a turn whose audio retention took is still held
// for its own duration rather than skipped, that the real dead air between
// turns is compressed to something bearable, that the rate moves both the
// element and the silences, and that there is only ever one sound in the app —
// a row preview starting has to take the element away from a replay, and a
// second replay has to take it from the first.
//
// The <audio> element is the only thing stubbed, and only because Node has
// none. Everything else here is the shipping code.

import test from 'node:test';
import assert from 'node:assert/strict';
import * as replay from '../src/renderer/lib/replay.js';
import { play as playPreview, stop as stopPreview, playbackState } from '../src/renderer/lib/preview.js';

// --- the stubs --------------------------------------------------------------

/** How long the fake element pretends a clip lasts, whatever the WAV says. */
const CLIP_MS = 30;

let audios = [];

class FakeAudio {
  constructor() {
    this.paused = true;
    this.currentTime = 0;
    this.playbackRate = 1;
    this.preload = '';
    this._src = '';
    this._timer = null;
    this.onplaying = null;
    this.onended = null;
    this.onerror = null;
    audios.push(this);
  }
  get src() {
    return this._src;
  }
  set src(v) {
    this._src = v;
    this.currentTime = 0;
    this.pause();
  }
  removeAttribute() {
    this._src = '';
  }
  pause() {
    this.paused = true;
    if (this._timer) clearTimeout(this._timer);
    this._timer = null;
  }
  play() {
    this.paused = false;
    this.currentTime = 0.001;
    this.onplaying?.();
    // The clip runs at the rate the caller asked for, which is the whole point
    // of the rate control: 2× has to make the conversation shorter.
    this._timer = setTimeout(() => {
      this._timer = null;
      this.paused = true;
      this.onended?.();
    }, CLIP_MS / (this.playbackRate || 1));
    return Promise.resolve();
  }
}

/**
 * A daemon, a window and an element. `turns` is what `replay.get` answers;
 * a turn with `has_audio: false` is never asked for, and one the daemon has
 * since lost answers `err:gone`, exactly as retention leaves it.
 */
function world({ turns, gone = [], asked = [] }) {
  audios = [];
  globalThis.Audio = FakeAudio;
  globalThis.Blob = globalThis.Blob ?? class {};
  globalThis.atob = globalThis.atob ?? ((s) => s);
  globalThis.URL = { createObjectURL: () => 'blob:fake', revokeObjectURL: () => {} };
  globalThis.window = {
    recall: {
      request: async (method, params) => {
        asked.push([method, params]);
        if (method === 'replay.get') return { ok: true, data: { thread: params.thread, turns } };
        if (method === 'segments.audio') {
          if (gone.includes(params.id)) return { ok: false, err: { code: 'gone', msg: 'retention took it' } };
          return { ok: true, data: { id: params.id, wav_b64: 'AAAA', duration_ms: CLIP_MS } };
        }
        return { ok: false, err: { code: 'unknown_method', msg: method } };
      },
    },
  };
  return asked;
}

const turn = (id, t_ms, dur_ms, has_audio = true) => ({
  id,
  t_ms,
  dur_ms,
  speaker: 1,
  speaker_name: 'Kira',
  text: `turn ${id}`,
  has_audio,
});

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** Wait for the engine to report something, or give up loudly. */
async function until(label, fn, timeout = 4000) {
  const end = Date.now() + timeout;
  for (;;) {
    const v = fn(replay.replayState());
    if (v) return v;
    if (Date.now() > end) throw new Error(`timed out waiting for ${label}: ${JSON.stringify(replay.replayState())}`);
    await sleep(10);
  }
}

test.afterEach(() => {
  replay.close();
  stopPreview();
});

// --- the tests --------------------------------------------------------------

test('a turn whose audio is gone is read through, not skipped', async () => {
  // The middle turn lost its audio to retention; the other two still sound.
  const asked = world({ turns: [turn(1, 0, 40), turn(2, 100, 200, false), turn(3, 400, 40)] });
  const res = await replay.start(77);
  assert.equal(res.ok, true);
  assert.equal(res.turns, 3);
  assert.equal(res.missing, 1, 'the silent turn was not counted');

  // It is HELD, not stepped over: the playhead is still on it well after a
  // sounding turn would have finished.
  await until('the playhead to reach the silent turn', (s) => s.index === 1);
  assert.equal(replay.replayState().phase, 'silent');
  await sleep(60);
  assert.equal(replay.replayState().index, 1, 'the silent turn was skipped past');

  await until('the last turn', (s) => s.index === 2);
  // …and its audio was never asked for. `has_audio: false` is the whole reason
  // this method exists: no round trip to discover a turn cannot be played.
  assert.equal(
    asked.filter(([m, p]) => m === 'segments.audio' && p.id === 2).length,
    0,
    'the client fetched audio for a turn it had been told has none'
  );
  await until('the end', (s) => s.phase === 'ended');
});

test('a turn the daemon has since lost becomes a silent one, and is counted once', async () => {
  // `has_audio` said yes and retention swept between the query and the fetch —
  // which is exactly what the contract says a client must survive.
  world({ turns: [turn(1, 0, 40), turn(2, 100, 40)], gone: [2] });
  await replay.start(9);
  await until('the end', (s) => s.phase === 'ended');
  const st = replay.replayState();
  assert.equal(st.missing, 1);
  assert.equal(st.turns[1].has_audio, false, 'the scrubber tick was not updated');
  assert.match(replay.missingNote(st), /retention/);
});

test('the note about lost audio is one sentence for the conversation, not one per row', async () => {
  const many = [turn(1, 0, 10, false), turn(2, 20, 10, false), turn(3, 40, 10, false)];
  world({ turns: many });
  await replay.start(3);
  const st = replay.replayState();
  assert.equal(st.missing, 3);
  const note = replay.missingNote(st);
  assert.equal(note.split('.').filter((s) => s.trim()).length, 1, `not one sentence: ${note}`);
  assert.match(note, /No audio is left for this conversation/);
  // A conversation that kept everything says nothing at all.
  assert.equal(replay.missingNote({ ...st, missing: 0 }), null);
});

test('dead air between turns is compressed to at most GAP_MAX_MS', async () => {
  // Half a minute of silence between two turns — real, and not something
  // anybody is going to sit through.
  world({ turns: [turn(1, 0, 20, false), turn(2, 30_000, 20, false)] });
  const at = Date.now();
  await replay.start(1);
  await until('the second turn', (s) => s.index === 1);
  const took = Date.now() - at;
  assert.ok(
    took < replay.GAP_MAX_MS + 400,
    `the gap was not compressed — ${took}ms to cross 30s of silence`
  );
  assert.ok(took >= 20, 'the first turn was not held at all');
});

test('the rate moves the element and the silences together', async () => {
  world({ turns: [turn(1, 0, 400, false), turn(2, 500, 400, false)] });
  await replay.start(2);
  assert.equal(replay.replayState().rate, 1);
  assert.equal(replay.setRate(2), 2);
  // A silent turn at 2× is over in half the time it claims to last. Without a
  // rate-aware wait it would still be sitting on turn 0 here.
  await until('the rate to carry the silent turn', (s) => s.index === 1, 900);
  assert.equal(replay.cycleRate(), 1, 'the rate did not wrap back to 1×');
  assert.equal(replay.setRate(3), 1, 'an unoffered rate was accepted');
});

test('pause holds the playhead, and resuming carries on from it', async () => {
  world({ turns: [turn(1, 0, 600, false), turn(2, 700, 40)] });
  await replay.start(4);
  await until('playing', (s) => s.phase === 'silent');
  assert.equal(replay.toggle(), 'paused');
  const at = replay.replayState().index;
  await sleep(250);
  assert.equal(replay.replayState().index, at, 'the playhead moved while paused');
  assert.equal(replay.replayState().phase, 'paused');
  replay.toggle();
  assert.notEqual(replay.replayState().phase, 'paused');
  await until('the turn after the paused one', (s) => s.index === 1, 2000);
});

test('a jump moves the playhead without giving the sound up', async () => {
  world({ turns: [turn(1, 0, 40), turn(2, 100, 40), turn(3, 200, 40), turn(4, 300, 40)] });
  await replay.start(5);
  assert.equal(replay.jump(2), 2);
  assert.equal(replay.replayState().index, 2);
  assert.equal(replay.prev(), 1);
  // Clamped at both ends rather than running off them.
  assert.equal(replay.jump(-4), 0);
  assert.equal(replay.jump(99), 3);
  assert.equal(replay.isReplaying(), true, 'a jump ended the replay');
});

test('a second replay stops the first, and a row preview stops the replay', async () => {
  world({ turns: [turn(1, 0, 900, false), turn(2, 1000, 900, false)] });
  await replay.start(11);
  await until('the first replay to be running', (s) => s.active && s.thread === 11);

  // Two conversations playing over each other is the exact confusion the one
  // shared element exists to prevent.
  await replay.start(12);
  const st = replay.replayState();
  assert.equal(st.thread, 12);
  assert.equal(st.index, 0, 'the second replay inherited the first one\'s playhead');

  // …and the per-row preview wins the element in the same way, which is what
  // takes the bar down rather than leaving it counting over somebody else's
  // clip. The replay is on a SILENT turn here on purpose: it has no clip whose
  // end would tell it it had been overtaken, and that is exactly the case that
  // used to keep counting after the preview took the element.
  await until('the replay to be on its silent turn', (s) => s.phase === 'silent');
  await playPreview('seg:1', [1]);
  await until('the replay to stand down', (s) => !s.active);
  assert.equal(playbackState().key, null, 'the preview did not finish cleanly');
});

test('close puts everything back, and a replay of nothing says so', async () => {
  world({ turns: [turn(1, 0, 900, false)] });
  await replay.start(6);
  assert.equal(replay.isReplaying(), true);
  assert.equal(replay.close(), true);
  assert.equal(replay.isReplaying(), false);
  assert.equal(replay.replayState().thread, null);
  assert.equal(replay.close(), false, 'closing twice claimed to have stopped something');

  world({ turns: [] });
  const empty = await replay.start(7);
  assert.deepEqual({ ok: empty.ok, error: empty.error }, { ok: false, error: 'empty' });
  assert.match(replay.startError('empty'), /nothing left/i);
  assert.match(replay.startError('not_found'), /no longer in the transcript/i);
});

test('a conversation the daemon will not answer for is a message, not a throw', async () => {
  audios = [];
  globalThis.Audio = FakeAudio;
  globalThis.window = {
    recall: {
      request: async () => ({ ok: false, err: { code: 'not_found', msg: 'no thread 404' } }),
    },
  };
  const res = await replay.start(404);
  assert.equal(res.ok, false);
  assert.equal(res.error, 'not_found');
  assert.equal(replay.isReplaying(), false);
});

test('a search hit replays from its own line, not from the top', async () => {
  world({ turns: [turn(1, 0, 900, false), turn(2, 1000, 900, false), turn(3, 2000, 900, false)] });
  const res = await replay.start(8, { from: 3 });
  assert.equal(res.index, 2);
  assert.equal(replay.currentTurn().id, 3);
  // A line that is not in this conversation starts at the top rather than
  // refusing: the conversation is still the thing you asked for.
  await replay.start(8, { from: 999 });
  assert.equal(replay.replayState().index, 0);
});
