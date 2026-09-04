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

// ---------------------------------------------------------------------------
// 0.12.2 — which Discord client is muted
// ---------------------------------------------------------------------------
//
// The Sources card renders `truth.status.audio.mute` verbatim, so what this
// guards is the mock agreeing with the daemon's rule rather than with itself:
// a role of `other` is never muted however sure the measurement is, a role of
// `bridge` is muted whenever streams are live, and `auto` mutes exactly one
// instance — the one the streams explain.

test('only one Discord client is muted, and it is the one the streams explain', async (t) => {
  const c = await rig(t);
  const mute = (await c.request('truth.status')).audio.mute;
  assert.equal(mute.streams_live, true, 'the fixture has a call running');
  assert.equal(mute.instances.length, 2, 'two clients, which is the whole point');

  const by = (src) => mute.instances.find((i) => i.source === src);
  assert.equal(by('vesktop').muted, true, 'the plugin’s own client');
  assert.equal(by('Discord').muted, false, 'the other call keeps recording');
  assert.ok(by('vesktop').share >= mute.share_bar);
  assert.ok(by('Discord').share < mute.share_bar);
  assert.deepEqual(mute.muted_sessions, [by('vesktop').session_id]);
  assert.ok(by('Discord').why.includes('different call'));
});

test('a manual role acts in both directions and auto is not a stored state', async (t) => {
  const c = await rig(t);
  const read = async (src) =>
    (await c.request('truth.status')).audio.mute.instances.find((i) => i.source === src);

  // The user says the rule has it backwards. Both states must act.
  await c.request('sources.instance_role', { source: 'vesktop', role: 'other' });
  await c.request('sources.instance_role', { source: 'Discord', role: 'bridge' });
  assert.equal((await read('vesktop')).muted, false, '`other` is never muted');
  assert.equal((await read('Discord')).muted, true, '`bridge` needs no evidence');
  assert.ok((await read('vesktop')).why.includes('not the bridge'));

  const roles = (await c.request('truth.status')).audio.mute.roles;
  assert.deepEqual(roles, { vesktop: 'other', Discord: 'bridge' });

  // Back to auto: the override is removed, not stored as a third value.
  await c.request('sources.instance_role', { source: 'vesktop', role: 'auto' });
  await c.request('sources.instance_role', { source: 'Discord', role: 'auto' });
  assert.deepEqual((await c.request('truth.status')).audio.mute.roles, {});
  assert.equal((await read('vesktop')).muted, true, 'the measurement is back');
  assert.equal((await read('Discord')).muted, false);
});

test('the microphones are never bridge roles', async (t) => {
  const c = await rig(t);
  for (const key of ['mic', 'room', 'discord:12345']) {
    await assert.rejects(
      () => c.request('sources.instance_role', { source: key, role: 'bridge' }),
      /not clients/,
      key
    );
  }
  await assert.rejects(
    () => c.request('sources.instance_role', { source: 'vesktop', role: 'brdige' }),
    /auto, bridge, other/
  );
});
