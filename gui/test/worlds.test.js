// The 0.10.0 contract, over a real socket against the mock daemon.
//
// These are shape-and-arithmetic checks, not UI checks: the views are driven by
// the headless e2e suite, and what this file guards is the thing a view cannot
// see going wrong — a share that does not add to one, a world facet that
// silently widens to everywhere, a null latency arriving as a zero.

import test from 'node:test';
import assert from 'node:assert/strict';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { RecallClient } from '../src/main/client.js';
import { startMock } from '../mock/mockd.js';

let n = 0;
const sockPath = () => join(tmpdir(), `nx-recall-worlds-${process.pid}-${++n}.sock`);

/// The same shape `client.test.js` uses: a mock with its feed effectively off
/// (nothing here is about live events), and both halves closed in `after`.
async function rig(t) {
  const path = sockPath();
  const mock = startMock({ sockPath: path, quiet: true, feedMs: 100_000 });
  const client = new RecallClient({ socketPath: path });
  await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('never connected')), 5000);
    const on = (st) => {
      if (st.status !== 'connected') return;
      clearTimeout(timer);
      resolve();
    };
    client.on('state', on);
    client.connect();
  });
  t.after(() => {
    client.close();
    mock.close();
  });
  return client;
}

test('worlds.list names the places, who is in them, and how often', async (t) => {
  const c = await rig(t);
  const res = await c.request('worlds.list');
  assert.ok(res.worlds.length >= 2, 'the fixture ships more than one world');
  for (const w of res.worlds) {
    assert.match(w.world_id, /^wrld_/, 'a world is identified by its id');
    assert.ok(w.visits > 0, `${w.name} claims ${w.visits} visits`);
    assert.ok(w.last_ms > 0, 'a world nobody has been in would not be listed');
    assert.ok(Array.isArray(w.people), 'people is a list, present or empty');
    assert.ok(Array.isArray(w.topics), 'topics is a list, never null');
  }
  // Newest visit first: that is the order a person looks for.
  const times = res.worlds.map((w) => w.last_ms);
  assert.deepEqual(times, [...times].sort((a, b) => b - a));
});

test('a world facet narrows, and an unknown one narrows to nothing', async (t) => {
  const c = await rig(t);
  const { worlds } = await c.request('worlds.list');
  const all = await c.request('search', { q: 'the', limit: 500 });
  const one = await c.request('search', { q: 'the', limit: 500, world: worlds[0].world_id });
  assert.ok(one.total > 0, 'the first world has something in it');
  assert.ok(one.total < all.total, `${one.total} is not fewer than ${all.total}`);

  // By name, case-insensitively.
  const byName = await c.request('search', {
    q: 'the',
    limit: 500,
    world: worlds[0].name.slice(-4).toUpperCase(),
  });
  assert.equal(byName.total, one.total, 'a name substring selects the same world as its id');

  // The failure mode that would be a lie: a facet nobody has a world for must
  // select NOTHING. Falling back to "everywhere" would answer a question that
  // was not asked.
  const nowhere = await c.request('search', { q: 'the', limit: 500, world: 'atlantis' });
  assert.equal(nowhere.total, 0);
});

test('a question can name a world, and hands the id back to be un-picked', async (t) => {
  const c = await rig(t);
  const { worlds } = await c.request('worlds.list');
  const name = worlds[0].name;
  for (const q of [`what was said in ${name}`, `was wurde in der ${name.replace(/^The /, '')} Welt gesagt`]) {
    const res = await c.request('search.ask', { q });
    assert.equal(res.interpretation.world_id, worlds[0].world_id, q);
    assert.equal(res.interpretation.world_label, name, q);
    // The world's own words do not survive into the search terms, and neither
    // does the "Welt"/"world" that closed the phrase.
    assert.ok(!/welt|world/i.test(res.interpretation.query ?? ''), `${q}: ${res.interpretation.query}`);
  }
  // Without the preposition it is a phrase somebody may well have SAID, and
  // swallowing it would turn a search into a filter behind the user's back.
  const plain = await c.request('search.ask', { q: `who mentioned ${name}` });
  assert.equal(plain.interpretation.world_id, undefined);
});

test('person.stats adds up, and its caveats travel with it', async (t) => {
  const c = await rig(t);
  const { speakers } = await c.request('speakers.list');
  const who = speakers.find((s) => s.segments > 3) ?? speakers[0];
  const s = await c.request('person.stats', { id: who.id });

  assert.ok(s.turns > 0);
  assert.ok(s.share > 0 && s.share <= 1, `share ${s.share} is not a fraction`);
  assert.equal(
    Math.round((s.speech_ms / s.conversation_speech_ms) * 1e6),
    Math.round(s.share * 1e6),
    'the share is the two speech totals and nothing else'
  );
  assert.equal(s.mean_turn_ms, Math.trunc(s.speech_ms / s.turns));
  assert.ok(
    s.longest_monologue_ms >= s.mean_turn_ms,
    'a longest run shorter than a mean turn is arithmetic nobody can defend'
  );
  // Null is a real state and NOT a zero: zero would say they always answered
  // instantly, which is a claim about a person.
  assert.ok(s.median_latency_ms === null || s.median_latency_ms >= 0);
  assert.ok(s.median_latency_ms === null || s.median_latency_ms <= 5000, 'the cap is five seconds');

  for (const key of ['interruption', 'latency', 'share']) {
    assert.equal(typeof s.definitions[key], 'string', `no definition for ${key}`);
    assert.ok(s.definitions[key].length > 40, `the ${key} definition is too short to be one`);
  }
  assert.ok(s.by_conversation.length <= 10);
  for (const row of s.by_conversation) {
    assert.ok(row.share >= 0 && row.share <= 1);
    assert.ok(row.turns > 0, 'a conversation they took no turns in is not one of theirs');
  }

  // `days` must be a real window.
  await assert.rejects(() => c.request('person.stats', { id: who.id, days: 0 }));
  await assert.rejects(() => c.request('person.stats', { id: 99_999 }));
});

test('a conversation says where it was and who did the talking', async (t) => {
  const c = await rig(t);
  const { speakers } = await c.request('speakers.list');
  const page = await c.request('person.get', { id: speakers[0].id });
  const thread = page.recent_threads[0];
  const got = await c.request('thread.get', { id: thread.thread_id });

  assert.ok(got.world === null || /^wrld_/.test(got.world.world_id));
  const shares = got.stats.shares;
  assert.ok(shares.length > 0);
  // Shares are of the CONVERSATION, so they add to one.
  const total = shares.reduce((n, s) => n + s.share, 0);
  assert.ok(Math.abs(total - 1) < 1e-9, `the shares add to ${total}`);
  // Most talkative first, by speech time and not by turn count.
  const ms = shares.map((s) => s.speech_ms);
  assert.deepEqual(ms, [...ms].sort((a, b) => b - a));
});

test('the person page says where you meet', async (t) => {
  const c = await rig(t);
  const { speakers } = await c.request('speakers.list');
  const page = await c.request('person.get', { id: speakers[0].id });
  assert.ok(Array.isArray(page.worlds));
  assert.ok(page.worlds.length > 0, 'the fixture puts every conversation somewhere');
  assert.ok(page.worlds.length <= 8, 'the page shows eight at most');
  for (const w of page.worlds) {
    assert.match(w.world_id, /^wrld_/);
    assert.ok(w.minutes_together >= 0);
    assert.equal(Math.round(w.minutes_together * 60000), w.together_ms);
  }
  // Most time first — the order the question "where do we meet" is answered in.
  const t0 = page.worlds.map((w) => w.together_ms);
  assert.deepEqual(t0, [...t0].sort((a, b) => b - a));
});
