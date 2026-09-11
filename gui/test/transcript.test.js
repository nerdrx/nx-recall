import test from 'node:test';
import assert from 'node:assert/strict';
import { threadNameResolver } from '../src/renderer/views/transcript.js';

function previousNames(segments, thread, label) {
  const seen = [];
  for (const segment of segments) {
    if (segment.thread !== thread || segment.speaker == null) continue;
    if (!seen.includes(segment.speaker)) seen.push(segment.speaker);
  }
  return seen.map(label);
}

test('conversation labels preserve first-speaking order, duplicates, and unnamed turns', () => {
  const segments = [
    { thread: 2, speaker: 7 },
    { thread: 1, speaker: null },
    { thread: 1, speaker: 3 },
    { thread: 2, speaker: 8 },
    { thread: 1, speaker: 4 },
    { thread: 1, speaker: 3 },
    { thread: null, speaker: 5 },
  ];
  const label = (id) => `Speaker ${id}`;
  const names = threadNameResolver(segments, label);
  for (const thread of [1, 2, 3]) {
    assert.deepEqual(names(thread), previousNames(segments, thread, label));
  }
  assert.deepEqual(names(1), ['Speaker 3', 'Speaker 4']);
});

test('one render pass scans a large window once, and a pass without seams does no work', () => {
  let visited = 0;
  const rows = Array.from({ length: 10_000 }, (_, id) => ({ thread: id, speaker: id }));
  const segments = {
    *[Symbol.iterator]() {
      for (const row of rows) { visited++; yield row; }
    },
  };
  const names = threadNameResolver(segments, String);
  assert.equal(visited, 0, 'ordinary live append without a seam does not build an index');
  for (const row of rows) assert.deepEqual(names(row.thread), [String(row.speaker)]);
  assert.equal(visited, rows.length, '10,000 seams still make only one 10,000-row scan');
});

test('fresh passes reflect added/reassigned turns, and label changes are never cached', () => {
  const segments = [{ thread: 1, speaker: 2 }];
  let renamed = false;
  const label = (id) => renamed && id === 2 ? 'Renamed' : `Speaker ${id}`;
  const first = threadNameResolver(segments, label);
  assert.deepEqual(first(1), ['Speaker 2']);
  renamed = true;
  assert.deepEqual(first(1), ['Renamed']);
  segments[0] = { thread: 1, speaker: 3 };
  segments.push({ thread: 1, speaker: 4 });
  const next = threadNameResolver(segments, label);
  assert.deepEqual(next(1), ['Speaker 3', 'Speaker 4']);
});
