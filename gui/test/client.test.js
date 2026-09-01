// The reconnect/resync state machine, against the mock daemon over a real
// unix socket. These are the cases a GUI cannot be trusted on by inspection:
// a replay that arrives after live events, a daemon whose sequence counter went
// backwards, a gap wider than the replay buffer.

import test from 'node:test';
import assert from 'node:assert/strict';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { RecallClient } from '../src/main/client.js';
import { startMock } from '../mock/mockd.js';

let n = 0;
const sockPath = () => join(tmpdir(), `nx-recall-test-${process.pid}-${++n}.sock`);

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function withMock(opts = {}) {
  const path = sockPath();
  const mock = startMock({ sockPath: path, quiet: true, feedMs: 250, ...opts });
  return { mock, path };
}

function waitFor(client, ev, timeout = 5000) {
  return new Promise((resolve, reject) => {
    const t = setTimeout(() => reject(new Error(`timeout waiting for ${ev}`)), timeout);
    client.once(ev, (payload) => {
      clearTimeout(t);
      resolve(payload);
    });
  });
}

async function connected(client) {
  await new Promise((resolve, reject) => {
    const t = setTimeout(() => reject(new Error('never connected')), 5000);
    const on = (st) => {
      if (st.status === 'connected') {
        clearTimeout(t);
        client.off('state', on);
        resolve();
      }
    };
    client.on('state', on);
    client.connect();
  });
}

test('handshake, subscribe, and a live event stream', async () => {
  const { mock, path } = withMock();
  const client = new RecallClient({ socketPath: path });
  try {
    const resync = waitFor(client, 'resync');
    await connected(client);
    assert.equal((await resync).reason, 'first-connect');
    assert.match(client.daemon, /recalld-mock/);

    const evt = await waitFor(client, 'event');
    assert.equal(evt.ev, 'segment');
    assert.ok(evt.seq > 0);
    assert.equal(client.lastSeq, evt.seq);
  } finally {
    client.close();
    mock.close();
  }
});

test('requests get exactly one terminal reply, errors carry their code', async () => {
  const { mock, path } = withMock({ feedMs: 100000 });
  const client = new RecallClient({ socketPath: path });
  try {
    await connected(client);
    const list = await client.request('speakers.list');
    assert.ok(Array.isArray(list.speakers));
    assert.ok(list.speakers.length > 0);

    await assert.rejects(() => client.request('speakers.name', { id: 9999, name: 'X' }), (e) => e.code === 'not_found');
    await assert.rejects(() => client.request('no.such.method'), (e) => e.code === 'unknown_method');
  } finally {
    client.close();
    mock.close();
  }
});

test('a relabel is broadcast to every subscribed client', async () => {
  const { mock, path } = withMock({ feedMs: 100000 });
  const a = new RecallClient({ socketPath: path });
  const b = new RecallClient({ socketPath: path });
  try {
    await connected(a);
    await connected(b);
    const heard = waitFor(b, 'event');
    await a.request('speakers.name', { id: 2, name: 'Mara' });
    const evt = await heard;
    assert.equal(evt.ev, 'relabel');
    assert.deepEqual(evt.data, { speaker: 2, name: 'Mara' });
  } finally {
    a.close();
    b.close();
    mock.close();
  }
});

test('reconnect inside the replay buffer catches up without a resync', async () => {
  const { mock, path } = withMock({ feedMs: 100000 });
  const client = new RecallClient({ socketPath: path });
  try {
    await connected(client);
    await waitFor(client, 'resync'); // the first-connect one
    const seen = [];
    client.on('event', (e) => seen.push(e.seq));

    mock.emit('segments', 'segment', { id: 1, text: 'before the drop' });
    await sleep(60);
    const before = client.lastSeq;

    // Drop the connection under the client, then produce events it missed.
    client.sock.destroy();
    await sleep(80);
    for (let i = 0; i < 5; i++) mock.emit('segments', 'segment', { id: 100 + i, text: `missed ${i}` });

    const caught = await waitFor(client, 'caughtup', 8000);
    assert.equal(caught.from, before);
    assert.equal(caught.replayed, 5);
    assert.equal(client.lastSeq, before + 5);
  } finally {
    client.close();
    mock.close();
  }
});

test('a gap wider than the replay buffer forces a full resync', async () => {
  const { mock, path } = withMock({ feedMs: 100000 });
  const client = new RecallClient({ socketPath: path });
  try {
    await connected(client);
    await waitFor(client, 'resync');
    mock.emit('segments', 'segment', { id: 1 });
    await sleep(60);

    client.sock.destroy();
    await sleep(80);
    // 400 events with a 200-deep buffer: the client's position falls out of it.
    for (let i = 0; i < 400; i++) mock.emit('segments', 'segment', { id: 500 + i });

    const res = await waitFor(client, 'resync', 8000);
    assert.equal(res.reason, 'replay-buffer-overrun');
  } finally {
    client.close();
    mock.close();
  }
});

test('a daemon whose sequence went backwards is treated as a restart', async () => {
  const { mock, path } = withMock({ feedMs: 100000 });
  const client = new RecallClient({ socketPath: path });
  try {
    await connected(client);
    await waitFor(client, 'resync');
    mock.emit('segments', 'segment', { id: 1 });
    await sleep(60);
    assert.ok(client.lastSeq > 1000);

    mock.restart(1); // fresh counter, every client dropped
    const res = await waitFor(client, 'resync', 8000);
    assert.equal(res.reason, 'daemon-restart');

    // And the stream actually resumes. Without rebasing lastSeq onto the
    // daemon's new position, every low-numbered event after the restart looks
    // like a replay duplicate and is dropped — the client stays connected and
    // silently shows nothing, which is the worst possible failure here.
    assert.ok(client.lastSeq <= 1, `lastSeq was not rebased (${client.lastSeq})`);
    const after = [];
    client.on('event', (e) => after.push(e.seq));
    mock.emit('segments', 'segment', { id: 42, text: 'after the restart' });
    await sleep(120);
    assert.deepEqual(after, [2]);
  } finally {
    client.close();
    mock.close();
  }
});

test('the client refuses a daemon that speaks another proto', async () => {
  const { mock, path } = withMock({ feedMs: 100000 });
  const client = new RecallClient({ socketPath: path, autoReconnect: false });
  client.on('error', () => {});
  try {
    const states = [];
    client.on('state', (s) => states.push(s));
    // Pretend to be a proto-2 client against a proto-1 daemon.
    client._send = ((orig) => (obj) => orig.call(client, obj.hello ? { hello: { ...obj.hello, proto: 2 } } : obj))(
      client._send
    );
    client.connect();
    await sleep(600);
    assert.ok(states.some((s) => s.status === 'offline' && /proto/.test(s.error ?? '')), JSON.stringify(states));
  } finally {
    client.close();
    mock.close();
  }
});

test('pause stops the feed and resume starts it again', async () => {
  const { mock, path } = withMock({ feedMs: 150 });
  const client = new RecallClient({ socketPath: path });
  try {
    await connected(client);
    let segments = 0;
    client.on('event', (e) => {
      if (e.ev === 'segment') segments += 1;
    });
    await sleep(500);
    assert.ok(segments > 0, 'the feed never started');

    const res = await client.request('pause');
    assert.equal(res.paused, true);
    const atPause = segments;
    await sleep(700);
    assert.equal(segments, atPause, 'segments kept arriving while paused');

    await client.request('resume');
    await sleep(500);
    assert.ok(segments > atPause, 'the feed did not restart');
  } finally {
    client.close();
    mock.close();
  }
});

test('an async op reports progress and completes without blocking requests', async () => {
  const { mock, path } = withMock({ feedMs: 100000 });
  const client = new RecallClient({ socketPath: path });
  try {
    await connected(client);
    const progress = [];
    let done = null;
    client.on('event', (e) => {
      if (e.ev === 'op.progress') progress.push(e.data.frac);
      if (e.ev === 'op.done') done = e.data;
    });

    const preview = await client.request('delete.preview', { speaker: 1 });
    assert.ok(preview.segments > 0);
    const { op } = await client.request('delete.run', { speaker: 1 });
    assert.match(op, /^op_/);

    // Other requests keep flowing while the op runs (PROTOCOL "Async ops").
    const status = await client.request('status');
    assert.ok(status.uptime_s >= 0);

    for (let i = 0; i < 60 && !done; i++) await sleep(100);
    assert.ok(done, 'the op never finished');
    assert.equal(done.op, op);
    assert.ok(progress.length >= 3, `expected progress events, got ${progress.length}`);
    assert.ok(done.removed > 0);
  } finally {
    client.close();
    mock.close();
  }
});

test('NDJSON framing survives split and coalesced chunks', async () => {
  const { mock, path } = withMock({ feedMs: 100000 });
  const client = new RecallClient({ socketPath: path });
  try {
    await connected(client);
    await waitFor(client, 'resync'); // catch-up must finish, or frames get queued
    const got = [];
    client.on('event', (e) => got.push(e.data.id));

    // Two frames in one write, and one frame split across two writes.
    const one = JSON.stringify({ seq: 999001, ev: 'segment', data: { id: 11 } });
    const two = JSON.stringify({ seq: 999002, ev: 'segment', data: { id: 12 } });
    client._onData(`${one}\n${two}\n`);
    const three = JSON.stringify({ seq: 999003, ev: 'segment', data: { id: 13 } });
    client._onData(three.slice(0, 10));
    client._onData(`${three.slice(10)}\n`);
    client._onData('not json at all\n'); // must be dropped, not fatal

    assert.deepEqual(got, [11, 12, 13]);
  } finally {
    client.close();
    mock.close();
  }
});

test('the client reconnects on its own after the daemon goes away', async () => {
  const path = sockPath();
  let mock = startMock({ sockPath: path, quiet: true, feedMs: 100000 });
  const client = new RecallClient({ socketPath: path });
  try {
    await connected(client);
    mock.close();
    await sleep(300);
    assert.equal(client.status, 'offline');

    mock = startMock({ sockPath: path, quiet: true, feedMs: 100000, seqStart: 99999 });
    for (let i = 0; i < 60 && client.status !== 'connected'; i++) await sleep(100);
    assert.equal(client.status, 'connected');
  } finally {
    client.close();
    mock.close();
  }
});

test('a voice preview arrives as a playable WAV, and an aged-out one says so', async () => {
  const { mock, path } = withMock({ feedMs: 100000 });
  const client = new RecallClient({ socketPath: path });
  try {
    await connected(client);

    // The naming query: the clips worth hearing, longest first.
    const sample = await client.request('speakers.sample', { id: 1, limit: 2 });
    assert.equal(sample.samples.length, 2);
    assert.ok(
      Math.floor(sample.samples[0].duration_ms / 1000) >= Math.floor(sample.samples[1].duration_ms / 1000),
      'samples are not longest-first'
    );

    // And one of them, decoded: a real RIFF/WAVE at the rate segments are stored at.
    const clip = await client.request('segments.audio', { id: sample.samples[0].segment_id });
    const wav = Buffer.from(clip.wav_b64, 'base64');
    assert.equal(wav.subarray(0, 4).toString(), 'RIFF');
    assert.equal(wav.subarray(8, 12).toString(), 'WAVE');
    assert.equal(wav.readUInt32LE(24), 16000, 'segments are 16 kHz');
    assert.equal(clip.bytes, wav.length);
    assert.equal(clip.sample_rate, 16000);

    // Retention outlives the recording: the voice is listed, the audio is not.
    const aged = await client.request('speakers.sample', { id: 5 });
    assert.deepEqual(aged.samples, []);
    const seg = mock.state.segments.find((s) => s.speaker === 5);
    const code = await client.request('segments.audio', { id: seg.id }).then(
      () => null,
      (e) => e.code
    );
    assert.equal(code, 'gone', 'aged-out audio must be `gone`, not a generic failure');
  } finally {
    client.close();
    mock.close();
  }
});

test('a frame far bigger than the old guard still arrives whole', async () => {
  // A segment's WAV travels inside one JSON frame, so the client's
  // oversized-frame guard sits above what the daemon may produce (16 MB, the
  // 10 MB audio cap once base64'd). At the old 4 MB it would have hung up on
  // exactly the reply that carried the audio.
  const { mock, path } = withMock({ feedMs: 100000 });
  const client = new RecallClient({ socketPath: path });
  try {
    await connected(client);
    await sleep(300); // the subscribe lands just after the connected state
    const big = 'x'.repeat(5 * 1024 * 1024);
    const got = waitFor(client, 'event', 20000);
    mock.emit('segments', 'segment', { id: 4242, text: big });
    const evt = await got;
    assert.equal(evt.data.text.length, big.length);
    assert.equal(client.status, 'connected', 'the big frame dropped the connection');
  } finally {
    client.close();
    mock.close();
  }
});

test('deleting a voice offers both halves of the choice, and the empty one still goes', async () => {
  // The 0.6.4 bug, at the protocol seam the GUI actually calls: a voice with no
  // segments left could not be deleted at all, because `delete.run` is scoped
  // by SEGMENTS and there were none — and no delete ever touched the voiceprint
  // behind it, so the ghost went on matching future audio.
  const { mock, path } = withMock({ feedMs: 100000 });
  const client = new RecallClient({ socketPath: path });
  try {
    await connected(client);
    const listed = async () => (await client.request('speakers.list')).speakers;

    // The ghost: in the bank, with nothing under it.
    const ghost = (await listed()).find((s) => s.segments === 0);
    assert.ok(ghost, 'the mock has no 0-segment voice to reproduce the bug with');
    const gone = await client.request('speakers.delete', { id: ghost.id });
    assert.equal(gone.segments, 0);
    assert.equal(gone.removed_speaker, true);
    assert.ok(!(await listed()).some((s) => s.id === ghost.id), 'the empty voice survived its delete');

    // Keeping the voiceprint: the words go, the voice does not — and the reply
    // says so, because a delete that leaves something behind has to admit it.
    const kira = (await listed()).find((s) => s.name === 'Kira');
    const kept = await client.request('speakers.delete', { id: kira.id, keep_voiceprint: true });
    assert.ok(kept.segments > 0);
    assert.equal(kept.removed_speaker, false);
    assert.match(kept.msg, /voiceprint was kept/i);
    const after = (await listed()).find((s) => s.id === kira.id);
    assert.ok(after, 'the kept voice left the bank anyway');
    assert.equal(after.segments, 0);

    // And your own pinned voice is refused, with the switch that works named.
    const you = (await listed()).find((s) => s.you);
    const refusal = await client.request('speakers.delete', { id: you.id }).then(
      () => null,
      (e) => e
    );
    assert.equal(refusal?.code, 'refused');
    assert.match(refusal.message, /microphone/i);
  } finally {
    client.close();
    mock.close();
  }
});

// ---- the memory graph over the wire (0.7.0, docs/GRAPH.md) --------------

test('the graph answers its summary, its list, and its state machine', async () => {
  const { mock, path } = withMock({ feedMs: 100000 });
  const client = new RecallClient({ socketPath: path });
  try {
    await connected(client);

    const summary = await client.request('graph.summary', {});
    assert.ok(summary.counts.commitments > 0, 'the fixture has promises in it');
    assert.equal(summary.counts.open, summary.counts.commitments);
    // Both tiers are represented, because the GUI renders them differently.
    assert.ok(summary.counts.from_rules > 0);
    assert.ok(summary.counts.from_llm > 0);
    // Tier 3 ships off, exactly as the daemon does.
    assert.equal(summary.config.enabled, false);
    assert.equal(summary.enrichment.phase, 'off');

    const { commitments } = await client.request('commitments.list', {});
    assert.equal(commitments.length, summary.counts.commitments);
    // Undated last, never first: a promise with no date is not overdue.
    const undated = commitments.findIndex((c) => c.due_ms == null);
    assert.equal(undated, commitments.length - 1, 'an undated promise sorted above a dated one');
    for (const c of commitments) {
      assert.ok(c.who.name || c.who.auto, 'a commitment with nobody to owe it');
      assert.ok(c.said, 'the evidence has to travel with the claim');
      assert.ok(['rules', 'llm'].includes(c.source));
      assert.equal(c.state, 'candidate', 'nothing has been acted on');
    }

    // The state machine, and its broadcast.
    const id = commitments[0].id;
    const done = await client.request('commitments.set_state', { id, state: 'done' });
    assert.equal(done.state, 'done');
    const after = await client.request('graph.summary', {});
    assert.equal(after.counts.done, 1);
    assert.equal(after.counts.open, summary.counts.open - 1);

    await assert.rejects(() => client.request('commitments.set_state', { id, state: 'nope' }));
    await assert.rejects(() => client.request('commitments.set_state', { id: 1, state: 'done' }));
  } finally {
    client.close();
    mock.close();
  }
});

test('turning the local model on walks a batch and comes back idle', async () => {
  const { mock, path } = withMock({ feedMs: 100000 });
  const client = new RecallClient({ socketPath: path });
  try {
    await connected(client);
    assert.equal((await client.request('graph.get', {})).config.enabled, false);

    const on = await client.request('graph.enrich', { action: 'start' });
    assert.equal(on.config.enabled, true);
    assert.equal(on.enrichment.phase, 'running');

    // It gets there on its own, reporting progress the way a delete does.
    const deadline = Date.now() + 8000;
    let phase = 'running';
    while (Date.now() < deadline && phase !== 'idle') {
      await sleep(200);
      phase = (await client.request('graph.get', {})).enrichment.phase;
    }
    assert.equal(phase, 'idle', 'the batch never finished');

    const off = await client.request('graph.enrich', { action: 'stop' });
    assert.equal(off.config.enabled, false);
    assert.equal(off.enrichment.phase, 'off');
    // Asking again for what is already true is not an error.
    assert.equal((await client.request('graph.enrich', { action: 'stop' })).changed, false);
    await assert.rejects(() => client.request('graph.enrich', { action: 'sideways' }));
  } finally {
    client.close();
    mock.close();
  }
});

// 0.7.2 — how much of the machine the model may use is a live setting, and the
// range it is clamped to travels with it so the Memory tab's stepper is built
// out of what the daemon will accept rather than out of a literal in the view.
test('the model thread count is live and clamped to the range it advertises', async () => {
  const { mock, path } = withMock({ feedMs: 100000 });
  const client = new RecallClient({ socketPath: path });
  try {
    await connected(client);
    const { config } = await client.request('graph.get', {});
    assert.equal(config.llm_threads_min, 1);
    assert.equal(config.llm_threads_max, 32);
    assert.ok(
      config.llm_threads >= config.llm_threads_min && config.llm_threads <= config.llm_threads_max,
      'the shipped default sits outside the range a client may offer'
    );

    const up = await client.request('graph.set', { llm_threads: 8 });
    assert.equal(up.config.llm_threads, 8);
    assert.equal(up.config.persisted, true, 'a setting that forgets is worse than none');
    assert.equal((await client.request('graph.get', {})).config.llm_threads, 8);

    // Clamped, not refused, at both ends — the same contract as every other
    // tuning number: the reply says what is now true.
    assert.equal((await client.request('graph.set', { llm_threads: 0 })).config.llm_threads, 1);
    assert.equal((await client.request('graph.set', { llm_threads: 999 })).config.llm_threads, 32);

    // Threads alone is a complete request; nothing at all is not.
    assert.equal((await client.request('graph.get', {})).config.enabled, false);
    await assert.rejects(() => client.request('graph.set', {}));
  } finally {
    client.close();
    mock.close();
  }
});

test('topics group conversations and name the threads behind them', async () => {
  const { mock, path } = withMock({ feedMs: 100000 });
  const client = new RecallClient({ socketPath: path });
  try {
    await connected(client);
    const { topics } = await client.request('topics.list', {});
    assert.ok(topics.length > 0);
    for (const t of topics) {
      assert.ok(t.topic.length > 0);
      assert.ok(t.threads >= 1);
      assert.ok(t.segments >= 1);
      assert.ok(Array.isArray(t.thread_ids) && t.thread_ids.length > 0);
      // A topic is a way INTO conversations, so every id it names has to open.
      const thread = await client.request('thread.get', { id: t.thread_ids[0] });
      assert.ok(thread.segments.length > 0);
    }
    // Newest first, because the answer to "what have we been talking about" is
    // about now.
    const last = topics.map((t) => t.last_ms);
    assert.deepEqual(last, [...last].sort((a, b) => b - a));
  } finally {
    client.close();
    mock.close();
  }
});
