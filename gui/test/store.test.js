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
  prependSegments,
  replaceSegments,
  setFollowing,
  collapseToTail,
  trimWindow,
  capWindow,
  MAX_SEGMENTS,
  HARD_MAX,
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
  store.appended = 0;
  store.window = { following: true, anchor: null, capped: false, beginning: false, firstMs: null };
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

// ---- 0.7.4: the window model ---------------------------------------------
//
// One rule, asserted from every side below: TRIMMING NEVER HAPPENS IN THE
// DIRECTION THE READER IS LOOKING. Following the tail, that direction is the
// newest rows and the oldest are dropped above MAX_SEGMENTS exactly as they
// always were. Browsing, it is the older rows and nothing is dropped at all.

/** Fill the window with `n` rows in time order, ids 1..n. */
function fill(n, from = 1) {
  const rows = [];
  for (let i = from; i < from + n; i += 1) rows.push(seg(i));
  store.segments = rows;
  store.segById = new Map(rows.map((s) => [s.id, s]));
  return rows;
}

test('following the tail, the window still trims the oldest above 600', () => {
  reset();
  fill(MAX_SEGMENTS);
  applyEvent({ seq: 1, ev: 'segment', data: seg(MAX_SEGMENTS + 1) });
  assert.equal(store.segments.length, MAX_SEGMENTS, 'the live window is still bounded');
  assert.equal(store.segments[0].id, 2, 'the OLDEST row went, which is the unchanged behaviour');
  assert.equal(store.segById.has(1), false, 'the index followed the array');
  assert.equal(store.segments.at(-1).id, MAX_SEGMENTS + 1);
  assert.equal(store.appended, 1, 'a bounded window still counts what arrived');
});

test('browsing history, a live segment trims nothing at all', () => {
  reset();
  fill(MAX_SEGMENTS);
  setFollowing(false);
  for (let i = 0; i < 50; i += 1) applyEvent({ seq: i, ev: 'segment', data: seg(MAX_SEGMENTS + 1 + i) });
  assert.equal(store.segments.length, MAX_SEGMENTS + 50, 'trimming is suspended while reading history');
  assert.equal(store.segments[0].id, 1, 'the row at the top of the reader’s screen is still there');
  assert.equal(store.appended, 50);
});

test('a page of older rows prepends without duplicating or reordering', () => {
  reset();
  fill(10, 101); // ids 101..110
  setFollowing(false);
  // The page overlaps what is already held — the daemon's `to` is exclusive so
  // it should not, but a client that trusted that would corrupt itself the one
  // time it was wrong.
  const added = prependSegments([seg(97), seg(99), seg(101), seg(98)]);
  assert.equal(added, 3, 'the row already held was not added twice');
  assert.deepEqual(store.segments.slice(0, 4).map((s) => s.id), [97, 98, 99, 101]);
  assert.equal(store.segments.length, 13);
  assert.equal(new Set(store.segments.map((s) => s.id)).size, 13, 'no duplicate ids');
  assert.deepEqual(
    store.segments.map((s) => s.t_ms),
    [...store.segments.map((s) => s.t_ms)].sort((a, b) => a - b),
    'the window stayed in time order'
  );
});

test('a prepend leaves the tail, so no live row can undo it', () => {
  reset();
  fill(MAX_SEGMENTS, 1000);
  assert.equal(store.window.following, true);
  prependSegments([seg(1), seg(2)]);
  assert.equal(store.window.following, false, 'asking for older rows IS browsing');
  // …and the proof: the very next live segment used to be enough to throw the
  // fetched page away, because the window was already full.
  applyEvent({ seq: 1, ev: 'segment', data: seg(9999) });
  assert.equal(store.segById.has(1), true, 'the page that was just fetched survived a live row');
  assert.equal(store.segments[0].id, 1);
});

test('audit finding #12: history merged in for a jump survives a full window', () => {
  reset();
  // The exact shape of the bug. The window is FULL (which is when it bit) and
  // a search hit two hours older is merged in for context. The old code sorted
  // the page in and then shift()ed the oldest rows off to get back to 600 —
  // dropping precisely the rows it had just been asked to show.
  fill(MAX_SEGMENTS, 1000);
  const context = [seg(1), seg(2), seg(3), seg(4), seg(5)];
  const added = mergeSegments(context);

  assert.equal(added, 5);
  for (const c of context) {
    assert.ok(store.segById.has(c.id), `merged row ${c.id} was discarded — this is finding #12`);
  }
  assert.deepEqual(store.segments.slice(0, 5).map((s) => s.id), [1, 2, 3, 4, 5]);
  assert.equal(store.segments.length, MAX_SEGMENTS + 5, 'nothing was trimmed to make room');
  assert.equal(store.window.following, false, 'a jump into the past is not the live tail');
});

test('merging rows that are NOT older leaves the follow state alone', () => {
  reset();
  fill(10, 100);
  // Backfilling a gap inside the window, or re-merging what is already there,
  // is not a jump — the tail must not be dropped for it.
  mergeSegments([seg(105), seg(106)]);
  assert.equal(store.window.following, true);
  assert.equal(store.segments.length, 10);
});

test('returning to Follow collapses the window to the newest 600 in one step', () => {
  reset();
  fill(2400);
  setFollowing(false);
  assert.equal(store.segments.length, 2400, 'browsing keeps everything that was paged in');

  const dropped = setFollowing(true);
  assert.equal(dropped, true);
  assert.equal(store.segments.length, MAX_SEGMENTS, 'one step, not a slow bleed');
  assert.equal(store.segments[0].id, 2400 - MAX_SEGMENTS + 1, 'the NEWEST 600 are what is left');
  assert.equal(store.segments.at(-1).id, 2400);
  assert.equal(store.segById.size, MAX_SEGMENTS, 'the index collapsed with the array');
  assert.equal(store.window.capped, false);
  // …and the beginning marker cannot still be true: the beginning just went.
  assert.equal(store.window.beginning, false);
});

test('collapsing a window that never grew keeps the beginning marker', () => {
  reset();
  fill(40);
  store.window.beginning = true;
  setFollowing(false);
  setFollowing(true);
  assert.equal(store.segments.length, 40);
  assert.equal(store.window.beginning, true, 'nothing was dropped, so nothing stopped being true');
});

test('the 20k ceiling trims from whichever end is furthest from the reader', () => {
  reset();
  fill(HARD_MAX);
  setFollowing(false);

  // Reading the OLDEST rows: the anchor is at the top, so the newest go.
  store.window.anchor = store.segments[0].id;
  prependSegments([seg(0, { t_ms: 1 }), seg(-1, { t_ms: 0 })]);
  assert.equal(store.segments.length, HARD_MAX);
  assert.equal(store.segments[0].id, -1, 'the rows being read stayed');
  assert.equal(store.segById.has(HARD_MAX), false, 'the far end was trimmed instead');
  assert.equal(store.window.capped, true, 'hitting the ceiling is said out loud');

  // Reading the NEWEST rows: the anchor is at the bottom, so the oldest go —
  // the same rule, pointing the other way.
  reset();
  fill(HARD_MAX);
  store.window.anchor = store.segments.at(-1).id;
  store.window.following = false;
  const first = store.segments[0].id;
  mergeSegments([seg(HARD_MAX + 1), seg(HARD_MAX + 2)]);
  assert.equal(store.segments.length, HARD_MAX);
  assert.equal(store.segById.has(first), false, 'the far end — the old one — was trimmed');
  assert.equal(store.segments.at(-1).id, HARD_MAX + 2);
});

test('with no anchor at all the ceiling degrades to the tail behaviour', () => {
  reset();
  fill(HARD_MAX + 5);
  store.window.anchor = null;
  store.window.following = false;
  capWindow();
  assert.equal(store.segments.length, HARD_MAX);
  assert.equal(store.segments.at(-1).id, HARD_MAX + 5, 'the newest rows are the safe default to keep');
  assert.equal(store.segments[0].id, 6);
});

test('trimWindow is the only trim, and it does nothing while browsing', () => {
  reset();
  fill(5000);
  setFollowing(false);
  assert.equal(trimWindow(), 0, 'nothing to trim: under the ceiling, and not following');
  assert.equal(store.segments.length, 5000);

  store.window.following = true;
  assert.equal(trimWindow(), 5000 - MAX_SEGMENTS);
  assert.equal(store.segments.length, MAX_SEGMENTS);
});

test('the date picker replaces the window rather than merging into it', () => {
  reset();
  fill(600, 5000);
  const day = [seg(11), seg(12), seg(13)];
  assert.equal(replaceSegments(day), 3);
  assert.deepEqual(store.segments.map((s) => s.id), [11, 12, 13]);
  assert.equal(store.segById.size, 3, 'the index was rebuilt, not appended to');
  assert.equal(store.window.following, false, 'you asked to be somewhere else');
  assert.equal(store.window.beginning, false, 'a day in the middle is not the beginning');
  assert.equal(store.window.anchor, 11, 'the reader lands at the START of the day');
  // …and from there the scrollback still works: paging back is anchored on the
  // first row of whatever is resident, whichever way you got there.
  assert.equal(store.segments[0].id, 11);
});

test('a live row arriving in a detached window is counted, not filed', () => {
  reset();
  store.speakers.set(1, { id: 1, name: 'Kira', segments: 0, total_ms: 0 });
  // The date picker took the window to another day. Tonight's rows must not
  // pile up underneath July's — the tail is one query away when Follow is
  // pressed, and until then this window is somewhere else.
  replaceSegments([seg(11), seg(12)]);
  assert.equal(store.window.detached, true);

  const change = applyEvent({ seq: 1, ev: 'segment', data: seg(9000) });
  assert.deepEqual(change, { detached: true }, 'no `added` — nothing was rendered');
  assert.equal(store.segments.length, 2, 'the day you are reading is untouched');
  assert.equal(store.segById.has(9000), false);
  // …but it happened, and the speakers view is entitled to know.
  assert.equal(store.appended, 1);
  assert.equal(store.speakers.get(1).segments, 1);

  // A correction to a row that IS resident still lands, detached or not.
  const upd = applyEvent({ seq: 2, ev: 'segment', data: seg(11, { text: 'fixed' }) });
  assert.ok(upd.updated);
  assert.equal(store.segById.get(11).text, 'fixed');
});

test('a purge still works while browsing, and takes only what it names', () => {
  reset();
  store.speakers.set(1, { id: 1, name: 'Kira', segments: 0, total_ms: 0 });
  fill(800);
  setFollowing(false);
  applyEvent({ seq: 1, ev: 'purge', data: { ids: [1, 2, 800] } });
  assert.equal(store.segments.length, 797, 'the purge trimmed, the WINDOW did not');
  assert.equal(store.segments[0].id, 3);
});

test('collapseToTail is idempotent and safe on an empty window', () => {
  reset();
  assert.equal(collapseToTail(), 0);
  fill(MAX_SEGMENTS);
  assert.equal(collapseToTail(), 0, 'exactly at the window is not over it');
  assert.equal(store.segments.length, MAX_SEGMENTS);
});
