// Synthetic conversation-label lookup only: no DOM, renderer layout, or audio.
// Run: node gui/scripts/transcript_bench.mjs
import assert from 'node:assert/strict';
import { performance } from 'node:perf_hooks';
import { threadNameResolver } from '../src/renderer/views/transcript.js';

const label = (id) => `Speaker ${id}`;
function previousNames(segments, thread) {
  const seen = [];
  for (const segment of segments) {
    if (segment.thread !== thread || segment.speaker == null) continue;
    if (!seen.includes(segment.speaker)) seen.push(segment.speaker);
  }
  return seen.map(label);
}
function median(samples) {
  samples.sort((a, b) => a - b);
  return samples[Math.floor(samples.length / 2)];
}
for (const [rowCount, threadCount] of [[600, 60], [10_000, 1_000], [10_000, 10_000]]) {
  const segments = Array.from({ length: rowCount }, (_, i) => ({
    thread: Math.floor(i * threadCount / rowCount), speaker: i % 7,
  }));
  const threads = [...new Set(segments.map((row) => row.thread))];
  const names = threadNameResolver(segments, label);
  for (const thread of threads) assert.deepEqual(names(thread), previousNames(segments, thread));
  const timings = {};
  for (const variant of ['previous', 'indexed']) {
    const samples = [];
    for (let sample = 0; sample < 7; sample++) {
      let checksum = 0;
      const started = performance.now();
      const resolve = variant === 'indexed'
        ? threadNameResolver(segments, label)
        : (thread) => previousNames(segments, thread);
      for (const thread of threads) checksum += resolve(thread).length;
      const elapsed = performance.now() - started;
      assert.ok(checksum > 0);
      if (sample >= 2) samples.push(elapsed);
    }
    timings[`${variant}_median_ms`] = Number(median(samples).toFixed(3));
  }
  console.log(JSON.stringify({ rows: rowCount, conversations: threadCount,
    previous_row_visits: rowCount * threadCount, indexed_row_visits: rowCount,
    ...timings, scope: 'synthetic conversation-label lookup; excludes DOM/layout and whole-app work' }));
}
