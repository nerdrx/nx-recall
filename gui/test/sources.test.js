// 0.11.0 — "heard on": a voice's source history, over a real socket against the
// mock daemon.
//
// A shape-and-arithmetic check, like `worlds.test.js`. What it guards is the
// thing the e2e cannot see going wrong: chips that render beautifully while
// disagreeing with the transcript underneath them. Every number here is
// re-derived from `transcript` and compared, so the mock cannot drift from
// itself — and if it does, the daemon contract it stands in for is what the
// test is really about.

import test from 'node:test';
import assert from 'node:assert/strict';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { RecallClient } from '../src/main/client.js';
import { startMock } from '../mock/mockd.js';

let n = 0;
const sockPath = () => join(tmpdir(), `nx-recall-sources-${process.pid}-${++n}.sock`);

async function rig(t) {
  const path = sockPath();
  const mock = startMock({ sockPath: path, quiet: true, feedMs: 100_000 });
  const client = new RecallClient({ socketPath: path });
  await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('never connected')), 5000);
    client.on('state', (st) => {
      if (st.status !== 'connected') return;
      clearTimeout(timer);
      resolve();
    });
    client.connect();
  });
  t.after(() => {
    client.close();
    mock.close();
  });
  return client;
}

const byId = (rows, id) => rows.find((r) => r.id === id);

test('every voice carries where it has been heard, most-heard first', async (t) => {
  const c = await rig(t);
  const { speakers } = await c.request('speakers.list');
  assert.ok(speakers.length > 0);
  for (const sp of speakers) {
    assert.ok(Array.isArray(sp.sources), `voice ${sp.id} has no sources array`);
    // Never null: a voice with no live turns has an EMPTY history, not an
    // unknown one, and a client must not have to guard the field.
    const counts = sp.sources.map((s) => s.segments);
    assert.deepEqual(counts, [...counts].sort((a, b) => b - a), 'not most-heard first');
    for (const s of sp.sources) {
      assert.equal(typeof s.source, 'string');
      assert.ok(['app', 'mic', 'room'].includes(s.kind), `odd kind ${s.kind}`);
      assert.ok(s.segments > 0, 'a source with no turns should not be listed at all');
      assert.ok(s.last_ms > 0);
      assert.equal(s.last_ns, String(s.last_ms) + '000000');
    }
    // The chips have to add up to the count beside them.
    const total = sp.sources.reduce((n, s) => n + s.segments, 0);
    assert.equal(total, sp.segments, `voice ${sp.id}'s chips do not add to its turns`);
  }
});

test('the fixture has all three shapes the feature is about', async (t) => {
  const c = await rig(t);
  const { speakers } = await c.request('speakers.list');
  const only = (id) => byId(speakers, id).sources.map((s) => s.source).sort();

  // Two voices heard ONLY on Discord — the case the prior exists for.
  assert.deepEqual(only(3), ['Discord']);
  assert.deepEqual(only(6), ['Discord']);
  // One heard on both, so "foreign to this source" is not the same as "new".
  assert.deepEqual(only(1), ['Discord', 'VRChat.exe']);
  // And the user's own voice, which the microphone follows everywhere.
  const you = speakers.find((s) => s.you);
  assert.ok(you, 'the fixture has a pinned voice');
  assert.ok(
    you.sources.some((s) => s.kind === 'mic'),
    `the pinned voice is not on the mic: ${JSON.stringify(you.sources)}`
  );
});

test('the person page and the list agree about where a voice is heard', async (t) => {
  const c = await rig(t);
  const { speakers } = await c.request('speakers.list');
  for (const id of [1, 3, 6]) {
    const page = await c.request('person.get', { id });
    assert.deepEqual(page.sources, byId(speakers, id).sources, `voice ${id} disagrees`);
  }
});

test('the chips are derived from the turns, not asserted beside them', async (t) => {
  const c = await rig(t);
  const { speakers } = await c.request('speakers.list');
  // Walk the whole transcript and count it independently.
  const seen = new Map();
  const res = await c.request('transcript', { limit: 10_000 });
  for (const seg of res.segments) {
    if (seg.speaker == null) continue;
    const per = seen.get(seg.speaker) ?? new Map();
    per.set(seg.source, (per.get(seg.source) ?? 0) + 1);
    seen.set(seg.speaker, per);
  }
  assert.ok(res.segments.length > 1000, 'the whole fixture, not one page');
  for (const sp of speakers) {
    const mine = seen.get(sp.id);
    if (!mine) continue;
    for (const s of sp.sources) {
      assert.equal(s.segments, mine.get(s.source), `voice ${sp.id} on ${s.source}`);
    }
  }
});
