import test from 'node:test';
import assert from 'node:assert/strict';
import { reviewReasons, correctionEvidence, correctAndReview } from '../src/renderer/views/review.js';

test('review evidence distinguishes decoder disagreement and language from speaker uncertainty', () => {
  assert.deepEqual(reviewReasons({ review_reasons: ['decoder_disagreement', 'language_mismatch', 'uncertain_speaker'] }), [
    'The second decoder disagreed with these words.', 'The detected language conflicted with the declared language.',
  ]);
  assert.deepEqual(reviewReasons({ speaker_id: null }), []);
});
test('correction evidence never substitutes zero accuracy for missing samples', () => {
  for (const summary of [null, {}, { corrections: 0, edit_rate: 0 }, { corrections: 3, edit_rate: null }]) assert.equal(correctionEvidence(summary), 'No scored word corrections yet.');
  assert.equal(correctionEvidence({ corrections: 2, edit_rate: 0.25 }), '25.0% observed word-edit rate across 2 corrected segments.');
});
test('review correction uses existing audit workflow then marks only refreshed current words', async () => {
  const calls = [];
  await correctAndReview({ id: 4, text: 'old', review_revision: 1 }, 'new', async (method, params) => {
    calls.push([method, params]);
    return method === 'review.get' ? { item: { id: 4, text: 'new', review_revision: 7 } } : {};
  });
  assert.deepEqual(calls, [
    ['segments.correct', { segment_id: 4, text: 'new', expected_text: 'old' }],
    ['review.get', { segment_id: 4 }], ['review.mark', { segment_id: 4, revision: 7 }],
  ]);
});
test('a concurrent post-correction change is never silently marked reviewed', async () => {
  const calls = [];
  await assert.rejects(correctAndReview({ id: 4, text: 'old', review_revision: 1 }, 'new', async method => {
    calls.push(method); return { item: { text: 'someone else', review_revision: 8 } };
  }), /changed again/);
  assert.deepEqual(calls, ['segments.correct', 'review.get']);
});
test('marking unchanged words does not create a fake correction in accuracy evidence', async () => {
  const calls = [];
  await correctAndReview({ id: 4, text: 'same', review_revision: 2 }, 'same', async (...args) => calls.push(args));
  assert.deepEqual(calls, [['review.mark', { segment_id: 4, revision: 2 }]]);
});
test('failed correction and blank input never mark reviewed', async () => {
  const calls = [];
  const rpc = async method => { calls.push(method); throw new Error('conflict'); };
  await assert.rejects(correctAndReview({ id: 4, text: 'old' }, 'new', rpc), /conflict/);
  await assert.rejects(correctAndReview({ id: 4, text: 'old' }, '  ', rpc), /Enter the words/);
  assert.deepEqual(calls, ['segments.correct']);
});

// Minimal DOM for exercising the actual mounted async controller, not a copy
// of its ownership rules. No model/audio/browser dependencies are needed.
class ReviewNode {
  constructor(tag = '') { this.tag = tag; this.nodeType = 1; this.children = []; this.listeners = {}; this.dataset = {}; this.classList = { add() {} }; this.disabled = false; }
  set textContent(text) { this.text = String(text); if (this.tag === 'textarea') this.value = String(text); }
  get textContent() { return this.text ?? this.children.map(node => node.textContent).join(''); }
  setAttribute(key, value) { this[key] = value; }
  addEventListener(name, callback) { this.listeners[name] = callback; }
  append(...nodes) { for (const node of nodes) { node.parent = this; this.children.push(node); } }
  get firstChild() { return this.children[0]; }
  removeChild(node) { this.children.splice(this.children.indexOf(node), 1); node.parent = null; }
  remove() { this.parent?.removeChild(this); }
  focus() {}
  click() { return this.listeners.click?.(); }
}
const descendants = node => [node, ...node.children.flatMap(descendants)];
const deferred = () => { let resolve, reject; const promise = new Promise((a, b) => { resolve = a; reject = b; }); return { promise, resolve, reject }; };
const flush = () => new Promise(resolve => setImmediate(resolve));
async function mountedReview(rpc, run) {
  const oldDocument = globalThis.document, oldWindow = globalThis.window;
  globalThis.document = { createElement: tag => new ReviewNode(tag), createTextNode: text => { const node = new ReviewNode(); node.textContent = text; return node; } };
  globalThis.window = { recall: { request: async (method, params) => ({ ok: true, data: await rpc(method, params) }) } };
  const { mountReview } = await import('../src/renderer/views/review.js');
  const root = new ReviewNode('root');
  const controller = mountReview(root, { jumpToSegment() {} });
  try { controller.show(); await flush(); await run(root, controller); }
  finally { controller.destroy(); globalThis.document = oldDocument; globalThis.window = oldWindow; }
}
const fixture = { id: 4, text: 'same words', review_revision: 1, review_reasons: ['decoder_disagreement'] };
const findButton = (root, label) => descendants(root).find(node => node.tag === 'button' && node.textContent === label);
const findCard = root => descendants(root).find(node => node.tag === 'article');

test('late save completion cannot unregister a replacement card after refresh', async () => {
  const save = deferred();
  await mountedReview(method => method === 'review.list' ? { items: [{ ...fixture }], next_before_id: null } : method === 'review.mark' ? save.promise : {}, async (root, controller) => {
    const oldCard = findCard(root);
    const saving = findButton(oldCard, 'Mark reviewed').click();
    await findButton(root, 'Refresh inbox').click(); await flush();
    const newCard = findCard(root);
    assert.notEqual(newCard, oldCard);
    save.resolve({ reviewed: true }); await saving; await flush();
    assert.equal(findCard(root), newCard, 'old completion must not remove refreshed DOM');
    controller.update({ purged: [4] });
    assert.equal(findCard(root), undefined, 'replacement must remain registered for privacy removal');
  });
});

test('a failed in-flight save cannot re-enable a card invalidated by newer source text', async () => {
  const save = deferred();
  await mountedReview(method => method === 'review.list' ? { items: [{ ...fixture }], next_before_id: null } : method === 'review.mark' ? save.promise : {}, async (root, controller) => {
    const card = findCard(root);
    const saving = findButton(card, 'Mark reviewed').click();
    controller.update({ updated: [{ ...fixture, text: 'new source words' }] });
    save.reject(new Error('late failure')); await saving;
    assert.equal(findButton(card, 'Mark reviewed').disabled, true);
    assert.equal(findButton(card, 'Save correction').disabled, true);
    assert.equal(descendants(card).find(node => node.tag === 'textarea').value, 'new source words');
    assert.match(descendants(card).find(node => node.class === 'review-problem' || node.className === 'review-problem').textContent, /Refresh before reviewing/);
  });
});
