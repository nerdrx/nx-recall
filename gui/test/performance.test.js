import test from 'node:test';
import assert from 'node:assert/strict';
import { formatLatency } from '../src/renderer/views/performance.js';
test('performance distinguishes missing samples from measured zero', () => {
  for (const value of [null, undefined, NaN, Infinity, -1, '10']) assert.equal(formatLatency(value), '—');
  assert.equal(formatLatency(0), '0 ms');
  assert.equal(formatLatency(12.7), '13 ms');
  assert.equal(formatLatency(1250), '1.25 s');
});
