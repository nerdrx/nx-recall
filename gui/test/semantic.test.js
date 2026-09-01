// Search's three modes, tested without a renderer.
//
// The mode control's *decisions* are pure functions over the daemon's `status`
// block — which mode to start in, which request to send, what the badge says —
// so they are checked here rather than inferred from a screenshot. What needs
// a screen (the pill's ground, the disabled state's contrast) is the headless
// suite's job.

import test from 'node:test';
import assert from 'node:assert/strict';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import {
  MODES,
  defaultMode,
  modeById,
  requestFor,
  resultSummary,
  semanticState,
} from '../src/renderer/views/semantic.js';
import { rrf, expand, startMock } from '../mock/mockd.js';
import { RecallClient } from '../src/main/client.js';

test('the model being absent is a state, not a crash', () => {
  // A daemon older than 0.6.5 has no `semantic` key at all. That is the same
  // answer as "not installed", and it must not throw on the way to saying so.
  for (const status of [null, undefined, {}, { semantic: null }]) {
    const s = semanticState(status);
    assert.equal(s.available, false);
    assert.ok(s.how.length > 0, 'there is always something to tell the user');
    assert.equal(defaultMode(s), 'keyword');
  }
});

test('the daemon"s own sentence is the one the UI shows', () => {
  const how = 'semantic search is not installed. `recalld models fetch --semantic` installs ...';
  const s = semanticState({ semantic: { available: false, how } });
  assert.equal(s.how, how, 'the CLI and the app must not invent separate advice');
});

test('Both is the default when the model is there', () => {
  const s = semanticState({
    semantic: { available: true, model: 'multilingual-e5-small-int8@1', indexed: 41822, eligible: 41830, pending: 8 },
  });
  assert.equal(s.available, true);
  assert.equal(s.pending, 8);
  assert.equal(defaultMode(s), 'both');
});

test('each mode maps to the method that can answer it', () => {
  assert.deepEqual(requestFor('keyword', { q: 'portal' }), ['search', { q: 'portal' }]);
  assert.deepEqual(requestFor('smart', { q: 'portal' }), ['search.semantic', { q: 'portal', mode: 'semantic' }]);
  assert.deepEqual(requestFor('both', { q: 'portal' }), ['search.semantic', { q: 'portal', mode: 'hybrid' }]);
  // An unknown mode falls back to the one that always works rather than
  // sending a method the daemon may not have.
  assert.deepEqual(requestFor('nonsense', { q: 'x' }), ['search', { q: 'x' }]);
});

test('the facets travel unchanged into every mode', () => {
  const facets = { q: 'wale', limit: 100, speaker: 7, source: 'VRChat.exe', from: 'a', to: 'b' };
  for (const m of MODES) {
    const [, p] = requestFor(m.id, facets);
    for (const k of ['q', 'limit', 'speaker', 'source', 'from', 'to']) assert.equal(p[k], facets[k], `${m.id}.${k}`);
  }
});

test('every mode has a hint and they are all different', () => {
  const hints = MODES.map((m) => modeById(m.id).hint);
  assert.equal(new Set(hints).size, MODES.length);
  assert.ok(hints.every((h) => h.endsWith('.')));
  // Sentence case, per the design language: no shouting in a control label.
  assert.deepEqual(MODES.map((m) => m.label), ['Keyword', 'Smart', 'Both']);
});

test('the result line says how the answer was found', () => {
  const res = { total: 4, took_ms: 31.4 };
  assert.equal(resultSummary('keyword', res, 'wale'), '4 matches for “wale”');
  assert.match(resultSummary('smart', res, 'wale'), /by meaning · 31 ms$/);
  assert.match(resultSummary('both', res, 'wale'), /by words and meaning · 31 ms$/);
  assert.equal(resultSummary('keyword', { total: 1 }, ''), '1 match');
  // A daemon that did not report a duration must not produce "NaN ms".
  assert.doesNotMatch(resultSummary('smart', { total: 0 }, 'x'), /NaN/);
});

// -- fusion, against the daemon's own rule ------------------------------------

test('fusion prefers what both legs found, deterministically', () => {
  // Same case as the daemon's own unit test: 2 tops neither list and wins.
  const fused = rrf([1, 2], [3, 2]);
  assert.equal(fused[0].id, 2);
  assert.equal(fused[0].via, 'both');
  assert.deepEqual(fused.map((f) => f.id), rrf([1, 2], [3, 2]).map((f) => f.id));
  assert.deepEqual(
    fused.slice(1).map((f) => f.via),
    ['keyword', 'semantic']
  );
});

test('one leg alone still fuses', () => {
  const fused = rrf([7, 8], []);
  assert.deepEqual(fused.map((f) => f.id), [7, 8]);
  assert.ok(fused.every((f) => f.via === 'keyword'));
});

test('the mock reaches across languages the way the model does', () => {
  assert.ok(expand('die Welt mit den Walen').includes(' whale'));
  assert.ok(expand('the train was running late').includes(' verspätung'));
  // ...without reaching across nothing at all: "den" is not "dentist".
  assert.ok(!expand('die Welt mit den Walen').includes(' dentist'));
});

// -- over the socket ----------------------------------------------------------

let n = 0;
const sockPath = () => join(tmpdir(), `nx-recall-sem-${process.pid}-${++n}.sock`);

async function connected(opts) {
  const path = sockPath();
  const mock = startMock({ sockPath: path, quiet: true, feedMs: 100000, ...opts });
  const client = new RecallClient({ socketPath: path });
  await new Promise((resolve, reject) => {
    const t = setTimeout(() => reject(new Error('timeout connecting')), 5000);
    const on = (st) => {
      if (st.status !== 'connected') return;
      clearTimeout(t);
      client.off('state', on);
      resolve();
    };
    client.on('state', on);
    client.connect();
  });
  return { mock, client };
}

test('a hybrid search comes back with a via on every hit', async () => {
  const { mock, client } = await connected({ semantic: true });
  try {
    const res = await client.request('search.semantic', { q: 'world', mode: 'hybrid', limit: 20 });
    assert.equal(res.mode, 'hybrid');
    assert.ok(res.model);
    assert.ok(res.hits.length > 0, 'the canned history talks about worlds');
    for (const hit of res.hits) {
      assert.ok(['keyword', 'semantic', 'both'].includes(hit.via), `bad via: ${hit.via}`);
      assert.equal(typeof hit.rrf, 'number');
      // A cosine is only reported when the vector leg actually scored the row.
      if (hit.via === 'keyword') assert.equal(hit.score, undefined);
      else assert.equal(typeof hit.score, 'number');
    }
    // Ordered by the fusion score, descending.
    const scores = res.hits.map((h) => h.rrf);
    assert.deepEqual(scores, [...scores].sort((a, b) => b - a));
  } finally {
    client.close();
    mock.close();
  }
});

test('a daemon without the model refuses with the fix, not with emptiness', async () => {
  const { mock, client } = await connected({ semantic: false });
  try {
    const status = await client.request('status', {});
    assert.equal(status.semantic.available, false);
    assert.match(status.semantic.how, /models fetch --semantic/);

    await assert.rejects(
      () => client.request('search.semantic', { q: 'world', mode: 'hybrid' }),
      (e) => {
        assert.equal(e.code, 'unavailable');
        assert.match(e.message, /models fetch --semantic/);
        return true;
      },
      'an empty result set would tell the user she never said it'
    );

    // ...and keyword search is exactly what it always was.
    const kw = await client.request('search', { q: 'world', limit: 20 });
    assert.ok(kw.hits.length > 0);
  } finally {
    client.close();
    mock.close();
  }
});
