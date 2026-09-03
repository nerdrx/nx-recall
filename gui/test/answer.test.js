// Grounded answers (0.11.0), over the socket and without a renderer.
//
// The renderer's job — the card above the hits, the chips that scroll and
// flash — is the e2e suite's, because it needs a screen. What is checked here
// is the shape of the contract the card is drawn from, and the one invariant
// that makes the card safe to draw at all: **every citation is an id that is in
// the hit list of the same reply.** A chip that points at a turn the page does
// not contain is a chip that scrolls nowhere, and no amount of CSS fixes it.

import test from 'node:test';
import assert from 'node:assert/strict';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { startMock } from '../mock/mockd.js';
import { RecallClient } from '../src/main/client.js';

let n = 0;
const sockPath = () => join(tmpdir(), `nx-recall-ans-${process.pid}-${++n}.sock`);

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

test('an answer cites turns that are in its own hit list', async () => {
  const { mock, client } = await connected();
  try {
    const res = await client.request('search.answer', { q: 'which portal was it?' });

    // Everything `search.ask` returns is still here — that is the contract, and
    // it is what lets the view render the hits whichever way the answer went.
    assert.ok(res.interpretation, 'no interpretation');
    assert.ok(Array.isArray(res.hits) && res.hits.length > 0, 'no hits');
    assert.equal(res.interpretation.is_question, true);

    // Exactly one of the two. Never both, never neither.
    assert.ok(res.answer, `refused instead: ${JSON.stringify(res.refused)}`);
    assert.equal(res.refused, null);

    assert.ok(res.answer.text.length > 0);
    assert.ok(['de', 'en'].includes(res.answer.lang));
    assert.ok(res.answer.via, 'an answer must say which model wrote it');
    assert.equal(typeof res.answer.took_ms, 'number');

    // The invariant.
    assert.ok(res.answer.citations.length > 0, 'an answer with no citations');
    const ids = new Set(res.hits.map((h) => h.id));
    for (const id of res.answer.citations) {
      assert.ok(ids.has(id), `cited ${id}, which is not in the hits`);
    }
  } finally {
    client.close();
    mock.close();
  }
});

test('a question the archive cannot answer is refused, and the hits still come back', async () => {
  const { mock, client } = await connected();
  try {
    // The mock answers two questions and refuses everything else, which is
    // also the honest default for an archive that mostly does not contain what
    // you asked about.
    const res = await client.request('search.answer', { q: 'what did anybody say about kryptonite?' });
    assert.equal(res.answer, null);
    assert.ok(res.refused?.reason, 'a refusal with no reason');
    // A refusal is not an error: whatever the search found is still returned,
    // because that is what the user would have got anyway.
    assert.ok(Array.isArray(res.hits));
    assert.ok(res.interpretation, 'a refusal dropped the interpretation');
  } finally {
    client.close();
    mock.close();
  }
});

test('the daemon says whether it read the query as a question', async () => {
  const { mock, client } = await connected();
  try {
    for (const [q, want] of [
      ['what did Kira say yesterday about the portal', true],
      ['which portal was it?', true],
      ['was hat Kira gestern gesagt', true],
      ['portal', false],
      ['shader compile error', false],
      // An interrogative in the MIDDLE is not a question: this is a phrase
      // somebody is searching for.
      ['the world where we met', false],
    ]) {
      const res = await client.request('search.ask', { q });
      assert.equal(res.interpretation.is_question, want, `is_question of ${JSON.stringify(q)}`);
    }
  } finally {
    client.close();
    mock.close();
  }
});

// --- 0.11.x: the three copies of one interrogative list ---------------------
//
// The client picks the METHOD — `search.answer` or `search.ask` — before the
// round trip, off its own copy of the daemon's interrogative list. So a word
// the daemon calls interrogative and the client does not is not a wrong
// answer, it is no answer and no line saying why: the client called
// `search.ask`, which has nothing to refuse with. There are three copies (the
// daemon's, the renderer's and this fixture's) and the only thing that can
// hold them together is a test that reads all three.
//
// The regex is read out of the source and run, rather than compared as a
// string, because `welche[rsn]?` is four interrogatives spelled as one.

test('the client and the daemon agree on what an interrogative is', async () => {
  const { readFile } = await import('node:fs/promises');
  const { fileURLToPath } = await import('node:url');
  const { dirname, resolve: resolvePath } = await import('node:path');
  const here = dirname(fileURLToPath(import.meta.url));
  const root = resolvePath(here, '..', '..');

  // The daemon's list, out of the daemon's source.
  const ask = await readFile(resolvePath(root, 'crates/recalld/src/ask.rs'), 'utf8');
  const block = ask.slice(ask.indexOf('const INTERROGATIVES'));
  const words = [...block.slice(0, block.indexOf('];')).matchAll(/"([a-z]+)"/g)].map((m) => m[1]);
  assert.ok(words.length >= 25, `found only ${words.length} interrogatives in ask.rs`);
  assert.ok(words.includes('worum') && words.includes('wovon'));

  // …and each client copy's regex, out of its own source.
  for (const file of ['gui/src/renderer/views/search.js', 'gui/mock/mockd.js']) {
    const src = await readFile(resolvePath(root, file), 'utf8');
    const at = src.indexOf('const INTERROGATIVES');
    assert.notEqual(at, -1, `${file} has no interrogative list`);
    const decl = src.slice(at, src.indexOf(';', at));
    const body = decl.slice(decl.indexOf('/') + 1, decl.lastIndexOf('/'));
    const flags = decl.slice(decl.lastIndexOf('/') + 1).trim();
    const re = new RegExp(body, flags);
    for (const word of words) {
      // `worueber` is the FOLDED spelling the daemon compares against — ü
      // becomes ue there. The client sees what was typed.
      const typed = word === 'worueber' ? 'worüber' : word;
      assert.ok(re.test(`${typed} war das`), `${file} does not know ${JSON.stringify(typed)}`);
    }
    // An interrogative in the middle is still not a question.
    assert.equal(re.test('the world where we met'), false, file);
    // …and an apostrophe is not a word boundary: `wie's gelaufen ist` is one
    // token to the daemon and is not a question, so it must not be one here.
    assert.equal(re.test("wie's gelaufen ist"), false, file);
  }
});

test('the mock reads a question the same way the daemon would', async () => {
  const { mock, client } = await connected();
  try {
    for (const [q, want] of [
      // The `wor-` compounds, which no client copy knew.
      ['worum ging es gestern Abend', true],
      ['wovon hat Aspen geredet', true],
      ['worüber habt ihr gesprochen', true],
      // Opening punctuation is walked past, as the daemon's tokeniser does.
      ['„was hat Aspen gesagt', true],
      ['- wann war das', true],
      // …and the apostrophe case, which the old `\b` made a question.
      ["wie's gelaufen ist", false],
    ]) {
      const res = await client.request('search.ask', { q });
      assert.equal(res.interpretation.is_question, want, `is_question of ${JSON.stringify(q)}`);
    }
  } finally {
    client.close();
    mock.close();
  }
});
