// The renderer's model, tested without a renderer. applyEvent and the
// onboarding heuristic are pure functions over the store, so the rules that
// matter — a merge never chains, an unknown event is ignored, the onboarding
// banner appears only when unnamed voices really dominate — are checked here
// rather than inferred from a screenshot.

import test from 'node:test';
import assert from 'node:assert/strict';
import {
  store,
  applyEvent,
  applyMic,
  isUncertain,
  uncertainReason,
  isYou,
  micChip,
  onboardingCandidates,
  mergeSegments,
  speakerLabel,
  segmentSpeakerLabel,
  LANGUAGE_CHOICES,
  languageValue,
  languageLabel,
} from '../src/renderer/lib/store.js';

function reset() {
  store.speakers = new Map();
  store.segments = [];
  store.segById = new Map();
  store.sources = [];
  store.ops = new Map();
  store.status = null;
  store.mic = { enabled: false, mode: 'follow', active: false, state: 'off', device: null, you_speaker: null };
  store.graph = { counts: null, enrichment: { phase: 'off' }, config: null };
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

test('a nameless segment says which kind of nameless it is', () => {
  reset();
  store.speakers.set(1, { id: 1, name: 'Kira', total_ms: 0 });
  assert.equal(segmentSpeakerLabel(seg(1)), 'Kira');
  // Refused by the overlap gate: nobody can be named here, ever.
  assert.equal(segmentSpeakerLabel(seg(1, { speaker: null, overlap_frac: 0.61 })), 'several voices');
  // One person spoke and matched nothing: naming this one is worth doing.
  assert.equal(segmentSpeakerLabel(seg(1, { speaker: null, overlap_frac: 0.02, match_score: null })), 'unknown voice');
  // The label and the prose behind the "?" have to be telling one story.
  assert.match(uncertainReason(seg(1, { speaker: null, overlap_frac: 0.61 })), /several voices/i);
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

test('a mic event moves the switch and never erases the pin behind it', () => {
  reset();
  applyMic({ enabled: true, mode: 'follow', active: false, state: 'following:idle', you_speaker: 8 });
  assert.equal(store.mic.you_speaker, 8);

  // The capture thread's event carries the switch but not the pin — folding it
  // in must not blank the one field it does not talk about.
  const change = applyEvent({
    seq: 1,
    ev: 'mic',
    data: { enabled: true, mode: 'follow', active: true, state: 'following:active', device: null },
  });
  assert.deepEqual(change, { mic: true });
  assert.equal(store.mic.state, 'following:active');
  assert.equal(store.mic.you_speaker, 8, 'the pin survived an event that never mentioned it');

  // A status push carries the same block and converges on the same answer.
  applyEvent({ seq: 2, ev: 'status', data: { paused: false, mic: { state: 'off', enabled: false } } });
  assert.equal(store.mic.state, 'off');
  assert.equal(store.mic.you_speaker, 8);
});

test('the mic chip names the three states a person can act on', () => {
  assert.equal(micChip('off').text, 'off');
  // The one that matters: enabled but not recording, because nothing allowed
  // is running. If this read as "capturing" the whole follow model would be a
  // lie on screen.
  assert.equal(micChip('following:idle').text, 'waiting for an allowed app');
  assert.equal(micChip('following:active').text, 'capturing');
  assert.equal(micChip('always:active').text, 'capturing');
  assert.ok(micChip('following:active').live);
  assert.ok(!micChip('following:idle').live);
  // A microphone that will not open is a problem to look at, not "recording".
  assert.equal(micChip('always:idle').text, 'no input device');
  assert.match(micChip('always:idle').cls, /warn/);
  // An unknown state from a newer daemon degrades to "off", never to "on".
  assert.equal(micChip('something:new').text, 'off');
});

test('your own voice is recognised from the pin or from the speaker row', () => {
  reset();
  store.speakers.set(8, { id: 8, name: null, auto: 'You', you: true, total_ms: 1000 });
  store.speakers.set(1, { id: 1, name: 'Kira', total_ms: 1000 });
  // Before mic.get answers, `speakers.list` already carries the fact.
  assert.equal(isYou(8), true);
  assert.equal(isYou(1), false);
  assert.equal(isYou(null), false);

  // Once the pin is known it wins, which is what makes a merge take effect in
  // the UI without a speakers re-query.
  applyMic({ you_speaker: 1 });
  assert.equal(isYou(1), true);
  assert.equal(isYou(8), false);
});

test('a mic segment says its speaker is certain and its audio may not be', () => {
  reset();
  store.speakers.set(8, { id: 8, name: null, auto: 'You', you: true, total_ms: 0 });
  applyMic({ you_speaker: 8 });

  // No score, because nothing was compared. That must not read as uncertain.
  const clean = seg(1, { speaker: 8, match_score: null, overlap_frac: 0.02 });
  assert.equal(isUncertain(clean), false);

  // Speakers-bleed: the room is in the recording. The NAME is still certain —
  // it came from the device — and the copy has to say which half is in doubt.
  const bleed = seg(2, { speaker: 8, match_score: null, overlap_frac: 0.55 });
  assert.equal(isUncertain(bleed), true);
  const why = uncertainReason(bleed);
  assert.match(why, /your microphone/i);
  assert.match(why, /certain/i);
  assert.doesNotMatch(why, /not trustworthy/i, 'the label is not what is in doubt here');
});

test('onboarding never asks the user who they are', () => {
  reset();
  store.speakers.set(8, { id: 8, name: null, auto: 'You', you: true, total_ms: 200_000 });
  store.speakers.set(2, { id: 2, name: null, auto: 'Speaker_07', total_ms: 60_000 });
  applyMic({ you_speaker: 8 });

  const ob = onboardingCandidates();
  assert.ok(ob.show, 'there is still an unnamed voice worth naming');
  assert.deepEqual(
    ob.speakers.map((s) => s.id),
    [2],
    'the pinned voice is the one identity that was never guessed at'
  );
  // …and a user who talks more than everybody else must not be able to
  // suppress the question about them just by being loud.
  assert.equal(ob.share, 1, 'your own speech is not in the denominator either');
});

test('merging history into the live window never duplicates or reorders', () => {
  reset();
  applyEvent({ seq: 1, ev: 'segment', data: seg(10) });
  const added = mergeSegments([seg(5), seg(10), seg(7)]);
  assert.equal(added, 2);
  assert.deepEqual(store.segments.map((s) => s.id), [5, 7, 10]);
});

// ---- 0.6.1 ---------------------------------------------------------------

test('an inherited label reads as uncertain and says where it came from', () => {
  reset();
  store.speakers.set(1, { id: 1, name: 'Kira', total_ms: 1000 });

  // Proximity inheritance: a name, no score, and a reason. It must not read as
  // a measurement — nothing was measured — but it is not nameless either.
  const inherited = seg(1, { speaker: 1, match_score: null, label_via: 'proximity', overlap_frac: 0.02 });
  assert.equal(isUncertain(inherited), true);
  const why = uncertainReason(inherited);
  assert.match(why, /surrounding turn/i);
  assert.match(why, /Kira/);

  // A matched row with the same shape minus the marker is not uncertain: this
  // is about provenance, not about the missing score.
  const matched = seg(2, { speaker: 1, match_score: 0.82, label_via: 'match', overlap_frac: 0.02 });
  assert.equal(isUncertain(matched), false);
});

test('a speaker language is a closed set with one label per state', () => {
  assert.equal(languageValue({ languages: null }), '');
  assert.equal(languageValue({}), '');
  assert.equal(languageValue({ languages: [] }), '');
  assert.equal(languageValue({ languages: ['en'] }), 'en');
  // Order is normalised, so one setting has one representation in the UI too.
  assert.equal(languageValue({ languages: ['en', 'de'] }), 'de,en');

  assert.equal(languageLabel({ languages: null }), 'Any');
  assert.equal(languageLabel({ languages: ['de'] }), 'German');
  assert.equal(languageLabel({ languages: ['en'] }), 'English');
  assert.equal(languageLabel({ languages: ['en', 'de'] }), 'German + English');
  // Every choice the control offers has a value the daemon accepts and a line
  // saying what it does — "German" alone does not explain a changed transcript.
  assert.equal(LANGUAGE_CHOICES.length, 4);
  for (const c of LANGUAGE_CHOICES) {
    assert.ok(c.title.length > 20, `${c.label} does not explain itself`);
    for (const code of c.value ? c.value.split(',') : []) {
      assert.ok(['de', 'en'].includes(code), `${code} is not a language the daemon classifies`);
    }
  }
});

test('a relabel carries languages without clobbering the name, and vice versa', () => {
  reset();
  store.speakers.set(1, { id: 1, name: 'Kira', auto: 'Speaker_03', languages: null, segments: 0, total_ms: 0 });

  applyEvent({ seq: 1, ev: 'relabel', data: { speaker: 1, name: 'Kira', languages: ['de'] } });
  assert.deepEqual(store.speakers.get(1).languages, ['de']);
  assert.equal(store.speakers.get(1).name, 'Kira');

  // A plain rename says nothing about languages and must not erase them.
  applyEvent({ seq: 2, ev: 'relabel', data: { speaker: 1, name: 'Kira Vex' } });
  assert.equal(store.speakers.get(1).name, 'Kira Vex');
  assert.deepEqual(store.speakers.get(1).languages, ['de']);
});

test('a swept voice disappears and its rows go back to nameless', () => {
  reset();
  store.speakers.set(9, { id: 9, name: null, auto: 'Speaker_52', segments: 1, total_ms: 900 });
  applyEvent({ seq: 1, ev: 'segment', data: seg(1, { speaker: 9 }) });

  // Not a merge: nothing was moved anywhere, the identity simply stops
  // existing, so its rows must not keep pointing at an id no view knows.
  const change = applyEvent({ seq: 2, ev: 'relabel', data: { speaker: 9, name: null, pruned: true } });
  assert.equal(store.speakers.has(9), false);
  assert.equal(store.segments[0].speaker, null);
  assert.ok(change.speakers, 'the speakers view has to repaint');
});

test('a voice whose conversations were deleted stays in the bank at zero', () => {
  // DESIGN §8's other half (0.6.4): "keep the bank entry (still labeled going
  // forward)". The rows go as a `purge`, the identity is announced as a plain
  // relabel — NOT a prune — and what is left is a voice at zero, which the
  // speakers view has to be able to render as empty rather than as broken.
  reset();
  store.speakers.set(1, { id: 1, name: 'Kira', auto: 'Speaker_03', segments: 0, total_ms: 0 });
  applyEvent({ seq: 1, ev: 'segment', data: seg(1) });
  applyEvent({ seq: 2, ev: 'segment', data: seg(2) });

  applyEvent({ seq: 3, ev: 'purge', data: { ids: [1, 2] } });
  const change = applyEvent({ seq: 4, ev: 'relabel', data: { speaker: 1, name: 'Kira', languages: null } });

  assert.ok(store.speakers.has(1), 'keeping the voiceprint must keep the voice');
  assert.equal(store.speakers.get(1).segments, 0);
  assert.equal(store.speakers.get(1).total_ms, 0);
  assert.equal(store.segments.length, 0);
  assert.deepEqual(change.relabel, [1]);
});

// ---- the memory graph, Tiers 2 and 3 (0.7.0, docs/GRAPH.md) --------------

test('the graph worker state arrives on its own event and on the status block', () => {
  reset();
  // Its own event, which is what makes a running batch visible without polling.
  const change = applyEvent({
    seq: 1,
    ev: 'graph',
    data: { phase: 'running', batch_done: 1, batch_total: 4, walked: 3 },
  });
  assert.equal(change.graph.phase, 'running');
  assert.equal(store.graph.enrichment.batch_total, 4);

  // …and on the status block, so a client that missed the event converges.
  applyEvent({ seq: 2, ev: 'status', data: { queue_depth: 0, graph: { phase: 'idle' } } });
  assert.equal(store.graph.enrichment.phase, 'idle');

  // A status block from a daemon older than 0.7.0 carries no graph at all, and
  // must not erase what this client already knows.
  applyEvent({ seq: 3, ev: 'status', data: { queue_depth: 0 } });
  assert.equal(store.graph.enrichment.phase, 'idle');
});

test('a commitment change is a broadcast the client never does arithmetic on', () => {
  reset();
  store.graph = { counts: { open: 3 }, enrichment: { phase: 'off' }, config: null };
  const change = applyEvent({
    seq: 1,
    ev: 'commitment',
    data: { id: 900, state: 'done', source: 'llm', what: 'send the link' },
  });
  assert.equal(change.commitment.id, 900);
  assert.equal(change.commitment.state, 'done');
  // The counts behind the rail badge are the daemon's arithmetic, not ours:
  // the controller re-asks rather than guessing, so nothing moved here.
  assert.equal(store.graph.counts.open, 3);

  assert.equal(applyEvent({ seq: 2, ev: 'commitment', data: {} }), null);
  assert.equal(applyEvent({ seq: 3, ev: 'commitment' }), null);
});

test('an unknown event is still ignored rather than being an error', () => {
  reset();
  assert.equal(applyEvent({ seq: 1, ev: 'something-from-0.8', data: { x: 1 } }), null);
  assert.equal(applyEvent({ seq: 2, ev: 'graph' }), null);
});
