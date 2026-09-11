import test from 'node:test';
import assert from 'node:assert/strict';
import { dayRange, localDay } from '../src/renderer/views/memory-archive.js';

test('archive local calendar day rejects invalid values instead of widening a query', () => {
  for (const value of ['', 'today', '2026-02-30', '2026-13-01', '2026-9-1']) assert.equal(dayRange(value), null);
  const range = dayRange('2026-09-11');
  assert.equal(localDay(new Date(range.from)), '2026-09-11');
  assert.equal(localDay(new Date(range.to)), '2026-09-12');
});

test('archive day boundaries follow local DST rather than assuming 24 hours', () => {
  const before = process.env.TZ;
  process.env.TZ = 'Europe/Berlin';
  try {
    const spring = dayRange('2026-03-29');
    const autumn = dayRange('2026-10-25');
    assert.equal(Date.parse(spring.to) - Date.parse(spring.from), 23 * 3600000);
    assert.equal(Date.parse(autumn.to) - Date.parse(autumn.from), 25 * 3600000);
    assert.equal(localDay(new Date('2026-09-10T22:30:00Z')), '2026-09-11');
  } finally { if (before === undefined) delete process.env.TZ; else process.env.TZ = before; }
});

test('saved archive paging reaches rows beyond the default page and stops at the end', async () => {
  const { savedPageInfo } = await import('../src/renderer/views/memory-archive.js');
  const first = savedPageInfo({ searches: Array(100), total: 205 }, 'searches');
  assert.equal(first.next, 100);
  assert.equal(first.hasMore, true);
  const second = savedPageInfo({ searches: Array(100), total: 205 }, 'searches', first.next);
  assert.equal(second.next, 200);
  assert.equal(second.previous, 0);
  assert.equal(second.hasMore, true);
  const last = savedPageInfo({ searches: Array(5), total: 205 }, 'searches', second.next);
  assert.equal(last.label, '201–205 of 205');
  assert.equal(last.previous, 100);
  assert.equal(last.hasMore, false);
  assert.equal(savedPageInfo({ moments: [], total: 200 }, 'moments', 200).hasMore, false);
});

test('archive attribution includes a local clock, speaker and capture source', async () => {
  const { archiveAttribution } = await import('../src/renderer/views/memory-archive.js');
  const ms = new Date(2026, 8, 11, 13, 14, 15).getTime();
  assert.match(archiveAttribution({ t_ms: ms, speaker: null, source: 'Discord' }), /13:14:15 · unknown voice · Discord$/);
  assert.match(archiveAttribution({ t_ms: null, source: '' }), /^Time unavailable · unknown voice · Unknown source$/);
});

test('append history deduplicates repeated pages without dropping tied timestamps', async () => {
  const { uniqueHistoryRows } = await import('../src/renderer/views/memory-archive.js');
  const seen = new Set();
  assert.deepEqual(uniqueHistoryRows([{id:1,t_ms:10},{id:2,t_ms:10}],seen).map(r=>r.id),[1,2]);
  assert.deepEqual(uniqueHistoryRows([{id:2,t_ms:10},{id:3,t_ms:10},{id:3,t_ms:10}],seen).map(r=>r.id),[3]);
});
