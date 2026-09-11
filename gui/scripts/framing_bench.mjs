// Synthetic audio-sized NDJSON frame; no daemon, models, or recordings needed.
// node gui/scripts/framing_bench.mjs [path/to/alternate/client.mjs]
import { performance } from 'node:perf_hooks';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import assert from 'node:assert/strict';
const moduleUrl = process.argv[2]
  ? pathToFileURL(resolve(process.argv[2]))
  : new URL('../src/main/client.js', import.meta.url);
const { RecallClient } = await import(moduleUrl.href);
const frame = JSON.stringify({ id: 1, ok: { wav: 'A'.repeat(12 * 1024 * 1024) } }) + '\n';
const samples = [];
for (let run = 0; run < 6; run++) {
  const client = new RecallClient({ autoReconnect: false });
  let received = 0;
  client._dispatch = (msg) => {
    assert.equal(msg.ok.wav.length, 12 * 1024 * 1024);
    received++;
  };
  const start = performance.now();
  for (let offset = 0; offset < frame.length; offset += 64 * 1024) {
    client._onData(frame.slice(offset, offset + 64 * 1024));
  }
  const elapsed = performance.now() - start;
  assert.equal(received, 1);
  if (run > 0) samples.push(elapsed);
}
samples.sort((a, b) => a - b);
console.log(JSON.stringify({ node: process.version, frameBytes: Buffer.byteLength(frame), chunkBytes: 65536,
  runs: samples.length, medianMs: +samples[2].toFixed(2), minMs: +samples[0].toFixed(2) }, null, 2));
