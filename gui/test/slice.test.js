// Sliced turns (0.12.4), tested without a window.
//
// A slice is a `partial`'s twin on the wire and its opposite in meaning, and
// every rule below is about the one word that separates them. A partial
// REPLACES the provisional row — each one is a better reading of the same
// audio, so the last one wins. A slice EXTENDS it — each one is a reading of
// NEW audio, so they accumulate, and the row that finally lands carries all of
// them.
//
// The failure this file exists to catch is a renderer that treats the two
// alike: fed slices, a partial-shaped client would show only the last piece of
// a monologue and silently drop the first thirty seconds of it. None of that
// is visible in a screenshot — the row looks fine, it is just missing most of
// the sentence — so it is checked here.

import test from 'node:test';
import assert from 'node:assert/strict';
import {
  store,
  applyEvent,
  applyConnState,
  clearPartial,
  livePartial,
  PARTIAL_STALE_MS,
  SLICE_STALE_MS,
} from '../src/renderer/lib/store.js';

const T0 = 1_772_486_400_000;
const NS = (ms) => String(ms) + '000000';

const PIECES = [
  'so the way the portal network actually works',
  'is that every instance keeps its own copy of the graph',
  'which is why the door behind the bar only works once',
];

function reset() {
  store.speakers = new Map();
  store.segments = [];
  store.segById = new Map();
  store.appended = 0;
  store.paused = false;
  store.conn = { status: 'connected', daemon: 'mock', seq: 1, error: null, socketPath: null };
  store.window = { following: true, anchor: null, capped: false, beginning: false, firstMs: null, detached: false };
  clearPartial();
}

/** One `slice` event, as the daemon shapes it (PROTOCOL "0.12.4"). */
const slice = (i, over = {}) => ({
  ev: 'slice',
  data: {
    session: 3,
    source: 'VRChat.exe',
    speaker: null,
    speaker_hint: null,
    t_start_ms: T0,
    t_start_ns: NS(T0),
    elapsed_ms: (i + 1) * 6200,
    text: PIECES[i],
    text_so_far: PIECES.slice(0, i + 1).join(' '),
    seq: i,
    final: false,
    ...over,
  },
});

/** The `segment` that ends the turn: same start, so it replaces the row. */
const finalSegment = (over = {}) => ({
  ev: 'segment',
  data: {
    id: 9001,
    session: 3,
    source: 'VRChat.exe',
    speaker: 4,
    text: PIECES.join(' '),
    t_ms: T0,
    t_ns: NS(T0),
    t_start_ns: NS(T0),
    t_end_ns: NS(T0 + 19000),
    dur_ms: 19000,
    ...over,
  },
});

test('a slice draws the words SO FAR, not just the newest piece', () => {
  reset();
  applyEvent(slice(0));
  assert.equal(livePartial().text, PIECES[0]);
  applyEvent(slice(1));
  // The whole claim of the feature in one assertion: the row grew.
  assert.equal(livePartial().text, `${PIECES[0]} ${PIECES[1]}`);
  applyEvent(slice(2));
  assert.equal(livePartial().text, PIECES.join(' '));
});

test('the growing row is one row: nothing reaches the segment list', () => {
  reset();
  for (let i = 0; i < PIECES.length; i += 1) {
    const change = applyEvent(slice(i));
    assert.deepEqual(change, { partial: true }, `slice ${i} was filed as something else`);
  }
  assert.equal(store.segments.length, 0, 'a slice became a segment');
  assert.equal(store.segById.size, 0, 'a slice was indexed by id');
  assert.equal(store.appended, 0, 'a slice was counted as an arrival');
});

test('a client accumulating `text` itself would double a redelivered slice — so it does not', () => {
  reset();
  applyEvent(slice(0));
  applyEvent(slice(1));
  // The same event again, as a reconnect or a replayed ring would deliver it.
  applyEvent(slice(1));
  assert.equal(
    livePartial().text,
    `${PIECES[0]} ${PIECES[1]}`,
    'the redelivered slice was appended a second time'
  );
});

test('the row says it is growing, which is a different claim from provisional', () => {
  reset();
  applyEvent(slice(0));
  assert.equal(livePartial().growing, true);
  // A partial is NOT growing: its words may be taken back, and a renderer keys
  // its ink off exactly this.
  reset();
  applyEvent({
    ev: 'partial',
    data: { session: 3, t_start_ms: T0, t_start_ns: NS(T0), text: 'the door', seq_in_turn: 0 },
  });
  assert.ok(!livePartial().growing);
});

test('the final segment replaces the growing row by (session, t_start_ns)', () => {
  reset();
  applyEvent(slice(0));
  applyEvent(slice(1));
  assert.ok(livePartial(), 'the row is not up before the turn ends');
  const change = applyEvent(finalSegment());
  assert.equal(change.partial, true, 'the segment did not announce that it replaced the row');
  assert.equal(store.partial, null, 'the growing row survived the turn that replaces it');
  assert.equal(store.segments.length, 1, 'one turn is one row');
  assert.equal(store.segments[0].text, PIECES.join(' '));
});

test('a segment for a DIFFERENT turn leaves the growing row alone', () => {
  reset();
  applyEvent(slice(0));
  // Same session, different start: somebody else's turn landing while this one
  // is still being said. Clearing on it would blank the live row mid-sentence.
  applyEvent(finalSegment({ id: 9002, t_ms: T0 - 5000, t_ns: NS(T0 - 5000), t_start_ns: NS(T0 - 5000) }));
  assert.ok(livePartial(), 'another turn landing took the growing row down');
  // …and so does a different session at the same instant.
  applyEvent(finalSegment({ id: 9003, session: 4 }));
  assert.ok(livePartial(), 'another session took the growing row down');
});

test('a growing row outlives a partial staleness window but not the VAD cap', () => {
  reset();
  applyEvent(slice(0));
  const at = store.partial.at;
  // Six seconds of quiet between two slices is NORMAL — slices are cut at
  // pauses, at least `slice_after_s` apart — and would blink a partial off.
  assert.ok(
    livePartial(at + PARTIAL_STALE_MS + 1000),
    'the growing row was aged out on the partial rule'
  );
  // Past the VAD's own turn cap there is no `segment` coming: the row is
  // orphaned and must go.
  assert.equal(livePartial(at + SLICE_STALE_MS + 1), null);
});

test('nothing grows while capture is paused or the socket is down', () => {
  reset();
  store.paused = true;
  assert.equal(applyEvent(slice(0)), null, 'a slice crossed a pause');
  assert.equal(store.partial, null);

  reset();
  applyEvent(slice(0));
  store.paused = true;
  assert.equal(livePartial(), null, 'a growing row survived a pause');

  reset();
  applyEvent(slice(0));
  // Offline means nothing is arriving to replace the row, so it goes — and it
  // goes from the MODEL, not merely from the view: no `segment` is coming.
  applyConnState({ conn: { ...store.conn, status: 'offline' }, paused: false });
  assert.equal(store.partial, null, 'a growing row survived the socket going away');
  assert.equal(livePartial(), null);
});

test('a slice with no replace key is refused rather than stranded', () => {
  reset();
  // Without both halves of `(session, t_start_ns)` the row could never be
  // replaced and would sit on the glass until it aged out.
  assert.equal(applyEvent(slice(0, { t_start_ns: null })), null);
  assert.equal(applyEvent(slice(0, { session: null })), null);
  assert.equal(store.partial, null);
});

test('a daemon that sends only `text` still draws something', () => {
  reset();
  // `text_so_far` is the contract, but a client that fell back to nothing at
  // all would show a blank row rather than a short one.
  applyEvent(slice(1, { text_so_far: undefined }));
  assert.equal(livePartial().text, PIECES[1]);
});
