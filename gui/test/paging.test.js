// The transcript's paging contract, asserted against the MOCK daemon — the
// same expectation table the real daemon's own unit test asserts against
// (crates/recalld/src/store.rs::tests::
// a_to_only_transcript_page_is_the_newest_rows_before_it).
//
// This file exists because the two halves diverging is not a mock detail. The
// GUI is developed and verified against mockd, so a mock that answers
// `transcript` differently from recalld is a bug the suite cannot see until a
// user hits it — that has happened three times. The fixture below and the rows
// each query must return are copied verbatim from the Rust test's doc comment;
// if one side changes, both tests fail, which is the point.

import test from 'node:test';
import assert from 'node:assert/strict';
import { transcriptPage, mockTimeParam } from '../mock/mockd.js';

// Ten segments, one per 1000 ms, ids 1..10 in time order. (The daemon's fixture
// counts in nanoseconds; the mock stores milliseconds. Same ten rows, same
// spacing — the table is about ORDER and which end the limit bites.)
const FIXTURE = Array.from({ length: 10 }, (_, n) => ({
  id: n + 1,
  t_ms: (n + 1) * 1000,
  session: n < 5 ? 1 : 2,
  speaker: (n % 2) + 1,
  text: `row ${n + 1}`,
}));

const page = (params) => transcriptPage(FIXTURE, params).map((s) => s.id);

test('the shared expectation table: which end the limit bites off', () => {
  // | query                           | rows     | why                        |
  // |---------------------------------|----------|----------------------------|
  assert.deepEqual(page({ limit: 3 }), [8, 9, 10], 'unanchored: the newest 3');
  assert.deepEqual(page({ to: 8000, limit: 3 }), [5, 6, 7], 'the newest 3 before T');
  assert.deepEqual(page({ to: 5000, limit: 3 }), [2, 3, 4], 'the page before that one');
  assert.deepEqual(page({ to: 2000, limit: 3 }), [1], 'short page = the beginning');
  assert.deepEqual(page({ to: 1000, limit: 3 }), [], 'T is exclusive');
  assert.deepEqual(page({ from: 3000, limit: 3 }), [3, 4, 5], 'anchored: the oldest 3');
  assert.deepEqual(page({ from: 3000, to: 6000, limit: 10 }), [3, 4, 5], 'a bounded day, in order');
});

test('every page comes back ascending, whichever end was trimmed', () => {
  for (const q of [{ limit: 4 }, { to: 7000, limit: 4 }, { from: 2000, limit: 4 }, {}]) {
    const rows = transcriptPage(FIXTURE, q);
    const times = rows.map((s) => s.t_ms);
    assert.deepEqual(times, [...times].sort((a, b) => a - b), `${JSON.stringify(q)} came back out of order`);
  }
});

test('a session anchors the query the way a from does', () => {
  // Not an accident of the SQL: `anchored = from.is_some() || session.is_some()`.
  // A session is a bounded thing you page forwards through, so its limit takes
  // the oldest rows in it.
  assert.deepEqual(page({ session: 1, limit: 3 }), [1, 2, 3]);
  // …and a speaker filter does NOT anchor, so it still reads as a live tail.
  assert.deepEqual(page({ speaker: 1, limit: 2 }), [7, 9]);
});

test('walking the whole history backwards visits every row once and stops', () => {
  // The exact loop the GUI's infinite scrollback runs.
  const seen = [];
  let cursor;
  for (let guard = 0; guard < 50; guard += 1) {
    const rows = transcriptPage(FIXTURE, { to: cursor, limit: 4 });
    if (!rows.length) break;
    cursor = rows[0].t_ms;
    seen.unshift(...rows.map((s) => s.id));
  }
  assert.deepEqual(seen, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
});

test('a timestamp on the wire may be ISO, milliseconds, or nanoseconds', () => {
  // The daemon accepts all three (`service::time_param`) and the client pages
  // with a raw `t_ms`, so a mock that only understood ISO strings quietly
  // ignored the filter and handed back the whole archive.
  assert.equal(mockTimeParam('2026-08-31T18:05:00Z'), Date.parse('2026-08-31T18:05:00Z'));
  assert.equal(mockTimeParam(1_756_000_000_000), 1_756_000_000_000);
  assert.equal(mockTimeParam(1_756_000_000_000_000_000), 1_756_000_000_000);
  assert.equal(mockTimeParam(null), null);
  assert.equal(mockTimeParam(undefined), null);

  // …and all three forms of the same instant must select the same page. The
  // ms/ns split is a magnitude test (`>= 1e15`), so it only has anything to
  // decide about real epoch timestamps — hence a second fixture at a real one.
  const iso = new Date(8000).toISOString();
  assert.deepEqual(page({ to: iso, limit: 3 }), page({ to: 8000, limit: 3 }));

  const T0 = Date.parse('2026-08-31T18:05:00Z');
  const real = Array.from({ length: 5 }, (_, n) => ({ id: n + 1, t_ms: T0 + n * 60_000 }));
  const cut = T0 + 3 * 60_000;
  const ids = (to) => transcriptPage(real, { to, limit: 2 }).map((s) => s.id);
  assert.deepEqual(ids(cut), [2, 3]);
  assert.deepEqual(ids(new Date(cut).toISOString()), [2, 3], 'ISO and ms disagree');
  assert.deepEqual(ids(cut * 1e6), [2, 3], 'nanoseconds and ms disagree');
});
