// The renderer's model, tested without a renderer. applyEvent and the
// onboarding heuristic are pure functions over the store, so the rules that
// matter — a merge never chains, an unknown event is ignored, the onboarding
// banner appears only when unnamed voices really dominate — are checked here
// rather than inferred from a screenshot.

import test from 'node:test';
import assert from 'node:assert/strict';
import { store, applyEvent, isUncertain, uncertainReason, onboardingCandidates, mergeSegments, speakerLabel } from '../src/renderer/lib/store.js';

function reset() {
  store.speakers = new Map();
  store.segments = [];
  store.segById = new Map();
  store.sources = [];
  store.ops = new Map();
  store.status = null;
}

const seg = (id, over = { }) => ({
  id,
  t_ms: 1_756_000_000_000 + id * 1000,
  dur_ms: 2000,
  text: `line ${id}`,
  speaker: 1,
  overlap_frac: 0.02,
  match_score: 0.7,
  ...over,
});

test('a segment event appends and keeps the list in time order', () => {
  reset();
  store.speakers.set(1, { id: 1, name: 'Kira', segments: 0, total_ms: 0 });
  applyEvent({ seq: 1, ev: 'segment', data: seg(2) });
  applyEvent({ seq: 2, ev: 'segment', data: seg(4) });
  applyEvent({ seq: 3, ev: 'segment', data: seg(3) }); // late arrival
  assert.deepEqual(store.segments.map((s) => s.id), [2, 3, 4]);
  assert.equal(store.speakers.get(1).segments, 3);
  assert.equal(store.speakers.get(1).total_ms, 6000);
});

test('a repeated segment id updates in place rather than duplicating', () => {
  reset();
  applyEvent({ seq: 1, ev: 'segment', data: seg(7) });
  const change = applyEvent({ seq: 2, ev: 'segment', data: seg(7, { text: 'corrected', corrected: true }) });
  assert.equal(store.segments.length, 1);
  assert.equal(store.segments[0].text, 'corrected');
  assert.ok(change.updated);
});

test('a rename relabels in the model and a merge repoints every segment', () => {
  reset();
  store.speakers.set(1, { id: 1, name: null, auto: 'Speaker_07', segments: 2, total_ms: 4000 });
  store.speakers.set(2, { id: 2, name: 'Ash', segments: 1, total_ms: 3000 });
  applyEvent({ seq: 1, ev: 'segment', data: seg(1, { speaker: 1 }) });
  applyEvent({ seq: 2, ev: 'segment', data: seg(2, { speaker: 2 }) });

  assert.equal(speakerLabel(1), 'Speaker_07');
  applyEvent({ seq: 3, ev: 'relabel', data: { speaker: 1, name: 'Mara' } });
  assert.equal(speakerLabel(1), 'Mara');

  const change = applyEvent({ seq: 4, ev: 'relabel', data: { speaker: 1, merged_into: 2, name: 'Ash' } });
  assert.deepEqual(change.merged, { from: 1, into: 2 });
  assert.equal(store.speakers.has(1), false);
  assert.deepEqual(store.segments.map((s) => s.speaker), [2, 2]);
  // Counts follow the identity, not the tombstone: 2's own 2 plus 1's 3.
  assert.equal(store.speakers.get(2).segments, 5);
});

test('a purge removes segments and gives their time back', () => {
  reset();
  store.speakers.set(1, { id: 1, name: 'Kira', segments: 0, total_ms: 0 });
  applyEvent({ seq: 1, ev: 'segment', data: seg(1) });
  applyEvent({ seq: 2, ev: 'segment', data: seg(2) });
  applyEvent({ seq: 3, ev: 'purge', data: { ids: [1] } });
  assert.deepEqual(store.segments.map((s) => s.id), [2]);
  assert.equal(store.speakers.get(1).segments, 1);
  assert.equal(store.speakers.get(1).total_ms, 2000);
});

test('unknown events and unknown fields are ignored, never fatal', () => {
  reset();
  assert.equal(applyEvent({ seq: 1, ev: 'weather', data: { sunny: true } }), null);
  assert.equal(applyEvent({ seq: 2, ev: 'roster', data: { ev: 'join', who: 'x' } }), null);
  applyEvent({ seq: 3, ev: 'segment', data: { ...seg(1), future_field: 'ignored' } });
  assert.equal(store.segments.length, 1);
});

test('op progress is tracked and cleared on completion', () => {
  reset();
  applyEvent({ seq: 1, ev: 'op.progress', data: { op: 'op_1', kind: 'delete.run', frac: 0.5 } });
  assert.equal(store.ops.get('op_1').frac, 0.5);
  const change = applyEvent({ seq: 2, ev: 'op.done', data: { op: 'op_1', kind: 'delete.run', removed: 12 } });
  assert.equal(store.ops.size, 0);
  assert.equal(change.opFinished.removed, 12);
});

test('uncertainty tracks the pipeline, not a guess', () => {
  assert.equal(isUncertain(seg(1)), false);
  assert.equal(isUncertain(seg(1, { overlap_frac: 0.61, speaker: null })), true);
  assert.equal(isUncertain(seg(1, { match_score: 0.2 })), true);
  assert.equal(isUncertain(seg(1, { speaker: null })), true);
  // The "?" has to name the reason, not just shrug.
  assert.match(uncertainReason(seg(1, { overlap_frac: 0.61, speaker: null })), /overlap/i);
  assert.match(uncertainReason(seg(1, { match_score: 0.2 })), /weak/i);
});

test('onboarding asks only when unnamed voices really dominate', () => {
  reset();
  store.speakers.set(1, { id: 1, name: 'Kira', total_ms: 90_000 });
  store.speakers.set(2, { id: 2, name: null, auto: 'Speaker_07', total_ms: 5_000 });
  assert.equal(onboardingCandidates().show, false, 'a sliver of unnamed speech is not an onboarding moment');

  store.speakers.set(3, { id: 3, name: null, auto: 'Speaker_12', total_ms: 60_000 });
  const ob = onboardingCandidates();
  assert.equal(ob.show, true);
  assert.ok(ob.share > 0.4);
  assert.deepEqual(ob.speakers.map((s) => s.id), [3, 2], 'the loudest unnamed voices come first');
});

test('merging history into the live window never duplicates or reorders', () => {
  reset();
  applyEvent({ seq: 1, ev: 'segment', data: seg(10) });
  const added = mergeSegments([seg(5), seg(10), seg(7)]);
  assert.equal(added, 2);
  assert.deepEqual(store.segments.map((s) => s.id), [5, 7, 10]);
});
