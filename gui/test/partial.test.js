// Partial turns (0.11.0), tested without a window.
//
// Four rules, and every one of them is a rule about a row that must NOT be
// there: a partial is never a segment, never counted, never left on screen
// after the turn it describes has been written, and never shown while capture
// is paused or the socket is down. None of that is visible in a screenshot —
// the failure mode is a stale half-sentence sitting under the transcript for
// the rest of the evening — so it is checked here.

import test from 'node:test';
import assert from 'node:assert/strict';
import {
  store,
  applyEvent,
  applyConnState,
  clearPartial,
  livePartial,
  PARTIAL_STALE_MS,
} from '../src/renderer/lib/store.js';

const T0 = 1_772_486_400_000;
const NS = (ms) => String(ms) + '000000';

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

/** One `partial` event, as the daemon shapes it. */
const partial = (over = {}) => ({
  ev: 'partial',
  data: {
    session: 3,
    source: 'VRChat.exe',
    speaker: null,
    speaker_hint: null,
    t_start_ms: T0,
    t_start_ns: NS(T0),
    elapsed_ms: 900,
    text: 'the door behind',
    seq_in_turn: 0,
    final: false,
    ...over,
  },
});

/** The `segment` that eventually replaces it. */
const segment = (over = {}) => ({
  ev: 'segment',
  data: {
    id: 5001,
    session: 3,
    source: 'VRChat.exe',
    speaker: 4,
    text: 'the door behind the bar goes back into the same instance',
    t_ms: T0,
    t_ns: NS(T0),
    t_start_ns: NS(T0),
    dur_ms: 4200,
    ...over,
  },
});

// -- it is not a segment ----------------------------------------------------

test('a partial goes nowhere near the segment list', () => {
  reset();
  const change = applyEvent(partial());
  assert.deepEqual(change, { partial: true });
  assert.equal(store.segments.length, 0, 'a partial is not a row');
  assert.equal(store.segById.size, 0);
  assert.equal(store.appended, 0, 'and it is never counted as an arrival');
  assert.equal(livePartial(T0).text, 'the door behind');
});

test('a partial without the replace key is refused', () => {
  reset();
  // Without `t_start_ns` there is nothing a `segment` could match, so the row
  // could only ever leave by ageing out. Better never to draw it.
  assert.equal(applyEvent(partial({ t_start_ns: undefined })), null);
  assert.equal(applyEvent(partial({ session: undefined })), null);
  assert.equal(store.partial, null);
});

test('each partial replaces the last, so there is only ever one', () => {
  reset();
  applyEvent(partial({ text: 'the door', seq_in_turn: 0 }));
  applyEvent(partial({ text: 'the door behind the bar', seq_in_turn: 1 }));
  applyEvent(partial({ text: 'the door behind the bar goes back', seq_in_turn: 2 }));
  const p = livePartial();
  assert.equal(p.text, 'the door behind the bar goes back');
  assert.equal(p.seq_in_turn, 2);
});

// -- the replace rule -------------------------------------------------------

test('the segment for that turn takes the provisional row away', () => {
  reset();
  applyEvent(partial());
  const change = applyEvent(segment());
  assert.equal(change.partial, true, 'the caller is told the row is gone');
  assert.equal(change.added.length, 1);
  assert.equal(store.partial, null);
  assert.equal(store.segments.length, 1);
});

test('the key is (session, t_start_ns) and nothing else', () => {
  reset();
  applyEvent(partial());
  // Same start, different session: a second person in another app started
  // talking at the same millisecond. Not this turn.
  applyEvent(segment({ id: 5002, session: 9 }));
  assert.ok(store.partial, 'another session must not clear this row');

  // Same session, a different turn — the next sentence arriving first because
  // it was shorter. Also not this turn.
  applyEvent(segment({ id: 5003, t_ns: NS(T0 + 9000), t_start_ns: NS(T0 + 9000), t_ms: T0 + 9000 }));
  assert.ok(store.partial, 'a different turn must not clear this row');

  applyEvent(segment());
  assert.equal(store.partial, null);
});

test('the comparison survives the wire, where t_start_ns is a string', () => {
  reset();
  // 1.77e18 does not fit a JSON number; the whole reason both events carry the
  // value as a string is that a float would lose the last three digits and two
  // different turns would compare equal.
  const a = '1772486400123456789';
  const b = '1772486400123456799';
  assert.notEqual(a, b);
  assert.equal(Number(a), Number(b), 'as numbers these two turns are the same turn');
  applyEvent(partial({ t_start_ns: a }));
  applyEvent(segment({ t_ns: b, t_start_ns: b }));
  assert.ok(store.partial, 'a float comparison would have cleared it');
  applyEvent(segment({ t_ns: a, t_start_ns: a }));
  assert.equal(store.partial, null);
});

test('the row goes even when the segment is filed somewhere this client cannot show', () => {
  reset();
  // The reader is off in July (the date picker rebuilt the window). The turn
  // does not land in the list — and the provisional row still has to go, or it
  // would sit at the bottom of a day it does not belong to.
  applyEvent(partial());
  store.window.detached = true;
  const change = applyEvent(segment());
  assert.equal(change.detached, true);
  assert.equal(change.partial, true);
  assert.equal(store.partial, null);
});

test('a re-published archive row is still not this turn', () => {
  reset();
  applyEvent(segment({ id: 4000, t_ms: T0, t_ns: NS(T0), t_start_ns: NS(T0) }));
  applyEvent(partial({ t_start_ns: NS(T0 + 60_000), t_start_ms: T0 + 60_000 }));
  // The re-decode worker re-publishes something from July. It is older than
  // the head, so it is filed as history — and it is not the open turn either.
  const change = applyEvent(segment({ id: 900, t_ms: T0 - 86_400_000, t_ns: NS(T0 - 86_400_000), t_start_ns: NS(T0 - 86_400_000) }));
  assert.equal(change.outside, true);
  assert.ok(store.partial, 'an archive row must not clear the live tail');
});

// -- pause, disconnection and staleness -------------------------------------

test('nothing is being said while capture is paused', () => {
  reset();
  store.paused = true;
  assert.equal(applyEvent(partial()), null, 'a partial that crossed a pause is stale');
  assert.equal(store.partial, null);
});

test('pausing takes the row off the glass', () => {
  reset();
  applyEvent(partial());
  applyConnState({ conn: store.conn, paused: true });
  assert.equal(store.partial, null, 'no segment is coming to replace it');
});

test('going offline takes the row off the glass', () => {
  reset();
  applyEvent(partial());
  applyConnState({ conn: { ...store.conn, status: 'offline' }, paused: false });
  assert.equal(store.partial, null);
});

test('a row nothing ever replaced ages out', () => {
  reset();
  applyEvent(partial());
  const at = store.partial.at;
  assert.ok(livePartial(at + PARTIAL_STALE_MS - 1), 'still inside the window');
  assert.equal(livePartial(at + PARTIAL_STALE_MS + 1), null, 'an audio gap ends a turn with no event at all');
});

test('livePartial refuses over a dead socket even before anything else notices', () => {
  reset();
  applyEvent(partial());
  store.conn = { ...store.conn, status: 'offline' };
  assert.equal(livePartial(), null);
});

// -- the speaker is a guess or nothing --------------------------------------

test('a proximity hint is carried through as the uncertain claim it is', () => {
  reset();
  applyEvent(partial({ speaker: 4, speaker_hint: 'proximity' }));
  const p = livePartial();
  assert.equal(p.speaker, 4);
  assert.equal(p.speaker_hint, 'proximity');
});

test('no previous turn means no speaker, not a guess at one', () => {
  reset();
  applyEvent(partial({ speaker: null, speaker_hint: null }));
  const p = livePartial();
  assert.equal(p.speaker, null);
  assert.equal(p.speaker_hint, null);
});

test('a partial never moves a speaker count', () => {
  reset();
  store.speakers.set(4, { id: 4, name: 'Ash', segments: 10, total_ms: 40_000 });
  applyEvent(partial({ speaker: 4, speaker_hint: 'proximity' }));
  applyEvent(partial({ speaker: 4, speaker_hint: 'proximity', seq_in_turn: 1 }));
  const sp = store.speakers.get(4);
  assert.equal(sp.segments, 10, 'three partials are not three turns');
  assert.equal(sp.total_ms, 40_000);
  // …and the turn itself counts exactly once when it lands.
  applyEvent(segment({ speaker: 4 }));
  assert.equal(store.speakers.get(4).segments, 11);
});
