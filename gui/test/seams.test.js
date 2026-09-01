// The transcript's separators, and the one place they can go wrong: the seam
// between a page that was just prepended and the rows that were already there.
//
// The property every test here checks is the same one: **prepending a page and
// then repainting the whole list must produce the same separators.** A repaint
// is the definition of correct — it is the thing the view did for four
// releases — so a prepend that agrees with it cannot be wrong, and one that
// disagrees is wrong at exactly one row, which is precisely the row nobody
// notices until they are four pages into their own history.

import test from 'node:test';
import assert from 'node:assert/strict';
import { separatorPlan, separatorWalker } from '../src/renderer/lib/seams.js';

// Local midday on three consecutive days, so the day boundaries in these
// fixtures are the same ones `fmtDay` computes in whatever zone this runs in.
const noon = (d) => new Date(2026, 7, 20 + d, 12, 0, 0).getTime();

const row = (id, dayOffset, thread, minute = 0) => ({
  id,
  t_ms: noon(dayOffset) + minute * 60_000,
  thread,
});

/** The separators a full repaint of `rows` would produce. */
const repaint = (rows) => separatorPlan(rows);

/**
 * The separators a PREPEND produces: the new page walked from scratch, then
 * the row that used to be first re-decided with the walker's state carried
 * over. That second half is the seam, and it is the whole point.
 */
function prepend(older, existing) {
  const walk = separatorWalker();
  const plan = older.map((seg) => ({ id: seg.id, ...walk(seg) }));
  const seam = { id: existing[0].id, ...walk(existing[0]) };
  // Everything BELOW the seam row was already rendered and is not touched —
  // the view keeps those nodes exactly as they are, which is what makes a
  // prepend cheap and what makes the seam the only place it can be wrong.
  const untouched = separatorPlan(existing).slice(1);
  return [...plan, seam, ...untouched];
}

test('the first row on screen gets a day header and never a thread hairline', () => {
  const plan = repaint([row(1, 0, 500), row(2, 0, 500), row(3, 0, 501)]);
  assert.deepEqual(plan[0], { id: 1, day: true, thread: false, threadId: 500 });
  assert.deepEqual(plan[1], { id: 2, day: false, thread: false, threadId: 500 });
  assert.deepEqual(plan[2], { id: 3, day: false, thread: true, threadId: 501 }, 'the conversation changed');
});

test('a new day resets the thread, whatever the ids say', () => {
  // A new day IS a new conversation as far as this view is concerned, so a
  // thread id that runs across midnight does not suppress the day header — and
  // because the reset makes the row's thread differ from "the last one seen",
  // it takes a hairline under the header too. That stacking is what has
  // shipped since 0.6.2 and is pinned here so a seam cannot quietly change it.
  const plan = repaint([row(1, 0, 500), row(2, 1, 500)]);
  assert.deepEqual(plan[1], { id: 2, day: true, thread: true, threadId: 500 });
});

test('a row with no thread id gets nothing and resets nothing', () => {
  // A daemon older than 0.6.2 never sent one. Those transcripts must render
  // exactly as they always did, and an unthreaded row in the middle must not
  // make the next threaded row look like a new conversation.
  const plan = repaint([row(1, 0, 500), row(2, 0, null), row(3, 0, 500), row(4, 0, 501)]);
  assert.equal(plan[1].thread, false, 'an unthreaded row draws nothing');
  assert.equal(plan[2].thread, false, 'and did not reset the thread it sat inside');
  assert.equal(plan[3].thread, true);
});

test('the seam: a prepended page separates exactly as a repaint would', () => {
  const cases = [
    {
      name: 'same day, same conversation — the old first row loses its header',
      older: [row(1, 0, 500, 1), row(2, 0, 500, 2)],
      existing: [row(3, 0, 500, 3), row(4, 0, 500, 4)],
    },
    {
      name: 'same day, a different conversation — it gains a hairline instead',
      older: [row(1, 0, 500, 1), row(2, 0, 500, 2)],
      existing: [row(3, 0, 501, 3), row(4, 0, 501, 4)],
    },
    {
      name: 'the page ends on the previous day — the header stays, the hairline does not appear',
      older: [row(1, 0, 500, 1), row(2, 0, 500, 2)],
      existing: [row(3, 1, 501, 3), row(4, 1, 501, 4)],
    },
    {
      name: 'the page itself spans two days',
      older: [row(1, 0, 500), row(2, 1, 501), row(3, 1, 501)],
      existing: [row(4, 1, 501), row(5, 2, 502)],
    },
    {
      name: 'unthreaded history under a threaded present',
      older: [row(1, 0, null), row(2, 0, null)],
      existing: [row(3, 0, 500), row(4, 0, 500)],
    },
    {
      name: 'a one-row page',
      older: [row(1, 0, 499)],
      existing: [row(2, 0, 500), row(3, 0, 500)],
    },
  ];

  for (const c of cases) {
    assert.deepEqual(
      prepend(c.older, c.existing),
      repaint([...c.older, ...c.existing]),
      `the seam disagrees with a repaint: ${c.name}`
    );
  }
});

test('page after page after page still agrees with one repaint of the whole thing', () => {
  // Four pages walked backwards, the way the scrollback actually arrives, then
  // compared against a single repaint of the result. This is the test that
  // would catch state leaking across a seam rather than being carried through it.
  const all = [];
  for (let i = 0; i < 40; i += 1) {
    all.push(row(i + 1, Math.floor(i / 9), 500 + Math.floor(i / 4), i));
  }
  const pages = [all.slice(0, 10), all.slice(10, 20), all.slice(20, 30), all.slice(30)];

  let rendered = separatorPlan(pages[3]);
  for (let p = 2; p >= 0; p -= 1) {
    const walk = separatorWalker();
    const fresh = pages[p].map((seg) => ({ id: seg.id, ...walk(seg) }));
    // The seam row is re-decided; everything below it keeps what it had.
    const seamRow = { id: rendered[0].id, ...walk(all.find((s) => s.id === rendered[0].id)) };
    rendered = [...fresh, seamRow, ...rendered.slice(1)];
  }
  assert.deepEqual(rendered, repaint(all));
});

test('no two day headers in a row, and never two for the same day', () => {
  const rows = [row(1, 0, 500), row(2, 0, 500), row(3, 1, 500), row(4, 1, 501), row(5, 2, 501)];
  const plan = repaint(rows);
  const headers = plan.filter((p) => p.day);
  assert.equal(headers.length, 3, 'one header per day, no more');
  // No row may be a boundary twice over in the same direction, and no two
  // consecutive rows may both open a day — either would mean the state machine
  // lost track of where it was.
  assert.deepEqual(plan.map((p) => p.day), [true, false, true, false, true]);
});
