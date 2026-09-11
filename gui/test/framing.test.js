import test from 'node:test';
import assert from 'node:assert/strict';
import { RecallClient } from '../src/main/client.js';

const LIMIT = 16 * 1024 * 1024;
function harness() {
  const client = new RecallClient({ autoReconnect: false });
  const messages = [];
  const warnings = [];
  let destroyed = false;
  client.sock = { destroy() { destroyed = true; } };
  client._dispatch = (msg) => messages.push(msg);
  client.on('warn', (warning) => warnings.push(warning));
  return { client, messages, warnings, get destroyed() { return destroyed; } };
}

for (const fragmented of [false, true]) {
  test(`oversized complete ASCII frame is rejected (${fragmented ? 'fragmented' : 'one read'})`, () => {
    const h = harness();
    const frame = JSON.stringify({ payload: 'a'.repeat(LIMIT) }) + '\n';
    if (fragmented) {
      h.client._onData(frame.slice(0, LIMIT));
      assert.equal(h.destroyed, false);
      h.client._onData(frame.slice(LIMIT));
    } else h.client._onData(frame);
    h.client._onData('{"later":true}\n');
    assert.equal(h.destroyed, true);
    assert.equal(h.messages.length, 0, 'must not dispatch oversized or subsequent frames');
    assert.equal(h.warnings.length, 1);
  });
}

test('UTF-8 byte limit rejects an unterminated multilingual frame below the character limit', () => {
  const h = harness();
  h.client._onData('界'.repeat(Math.floor(LIMIT / 3) + 1));
  assert.equal(h.destroyed, true);
  assert.equal(h.messages.length, 0);
});

test('whitespace counts toward the frame budget before trimming', () => {
  const h = harness();
  h.client._onData(' '.repeat(LIMIT + 1) + '\n');
  assert.equal(h.destroyed, true);
});

test('exact byte limit survives fragmentation and coalesced subsequent frames', () => {
  const h = harness();
  const overhead = Buffer.byteLength(JSON.stringify({ payload: '' }));
  const frame = JSON.stringify({ payload: 'a'.repeat(LIMIT - overhead) });
  for (let start = 0; start < frame.length; start += 65536) {
    h.client._onData(frame.slice(start, start + 65536));
  }
  h.client._onData('\n\n{"next":"こんにちは 🐾"}\n');
  assert.equal(h.destroyed, false);
  assert.equal(h.messages.length, 2);
  assert.equal(h.messages[0].payload.length, LIMIT - overhead);
  assert.equal(h.messages[1].next, 'こんにちは 🐾');
});

test('coalesced frames have independent byte budgets', () => {
  const h = harness();
  const frame = JSON.stringify({ payload: 'a'.repeat(LIMIT / 2) }) + '\n';
  h.client._onData(frame + frame);
  assert.equal(h.destroyed, false);
  assert.equal(h.messages.length, 2);
});
