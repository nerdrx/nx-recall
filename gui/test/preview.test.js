// Voice preview's failure paths, without an <audio> element.
//
// `play()` is documented "Never throws: a preview that fails is a hint in the
// UI, not an exception in a click handler" — and audit finding #25c was that it
// did, on the one line nobody guards: `atob(clip.wav_b64)`. A daemon reply with
// a truncated or non-base64 payload rejected out of the click handler, which
// left the row spinning in `loading` for ever with no hint and no way back.
//
// Everything here stops before a blob or an element is ever touched, which is
// why it runs headless: a clip that cannot be decoded never reaches playback.

import test from 'node:test';
import assert from 'node:assert/strict';
import { play, playbackState, noAudioHint } from '../src/renderer/lib/preview.js';

/** The seam `ask` reads. No Electron, no socket, no DOM. */
function daemonSays(answers) {
  const asked = [];
  globalThis.window = {
    recall: {
      request: async (method, params) => {
        asked.push([method, params]);
        const fn = answers[method];
        if (!fn) return { ok: false, err: { code: 'unknown_method', msg: method } };
        try {
          return { ok: true, data: fn(params) };
        } catch (e) {
          return { ok: false, err: { code: e.code ?? 'failed', msg: e.message } };
        }
      },
    },
  };
  return asked;
}

test('a clip that is not decodable base64 is a failed clip, not an exception', async () => {
  daemonSays({ 'segments.audio': () => ({ id: 1, wav_b64: '!!! not base64 !!!', duration_ms: 1200 }) });
  const res = await play('speaker:1', [1]);
  assert.deepEqual(res, { played: 0, stopped: false, error: 'corrupt' });
  // And the row is released rather than left claiming to be loading.
  assert.equal(playbackState().phase, 'idle');
  assert.equal(playbackState().key, null);
  // The hint is the standard in-place one, not a bare error code.
  assert.match(noAudioHint(res.error), /unreadable/i);
});

test('a reply with no audio field at all is the same failure', async () => {
  daemonSays({ 'segments.audio': () => ({ id: 2, duration_ms: 900 }) });
  const res = await play('speaker:1', [2]);
  assert.equal(res.error, 'corrupt');
  assert.equal(playbackState().phase, 'idle');
});

test('a refused clip still reports the daemon code, and an empty list is empty', async () => {
  daemonSays({
    'segments.audio': () => {
      throw Object.assign(new Error('too large'), { code: 'refused' });
    },
  });
  assert.equal((await play('speaker:1', [3])).error, 'refused');
  assert.deepEqual(await play('speaker:1', []), { played: 0, stopped: false, error: 'empty' });
  assert.match(noAudioHint('refused'), /too large/i);
  assert.match(noAudioHint('gone'), /retention/i);
  assert.equal(noAudioHint(null), '');
});
