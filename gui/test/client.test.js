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
