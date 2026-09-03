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
  applyConnState,
  applyMic,
  appSources,
  allowedAppCount,
  cancelResyncRetry,
  isCatchingUp,
  liveStatus,
  reloadAll,
  isUncertain,
  uncertainReason,
  hasMark,
  languageNote,
  isShaky,
  textViaNote,
  speakerByDisplayName,
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
  applyRoom,
  roomChip,
  applyAssist,
  translationLeads,
} from '../src/renderer/lib/store.js';
import { splitOutcome } from '../src/renderer/views/speakers.js';

function reset() {
  store.speakers = new Map();
  store.segments = [];
  store.segById = new Map();
  store.sources = [];
  store.ops = new Map();
  store.status = null;
  store.statusLive = false;
  store.appended = 0;
  store.window = { following: true, anchor: null, capped: false, beginning: false, firstMs: null };
  cancelResyncRetry();
  store.resync = { stale: [], attempt: 0, retrying: false, since: null };
  store.mic = { enabled: false, mode: 'follow', active: false, state: 'off', device: null, you_speaker: null };
  store.graph = { counts: null, enrichment: { phase: 'off' }, config: null };
  store.assist = { translate_to: '', read_languages: ['de', 'en'], translation_display: 'main', languages: [] };
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
  // A roster JOIN is the one shape this client acts on (0.8.0); everything else
  // on that topic is still nothing to draw.
  assert.equal(applyEvent({ seq: 2, ev: 'roster', data: { ev: 'leave', who: 'x' } }), null);
  assert.equal(applyEvent({ seq: 3, ev: 'roster', data: { ev: 'world', world_id: 'wrld_1' } }), null);
  applyEvent({ seq: 4, ev: 'segment', data: { ...seg(1), future_field: 'ignored' } });
  assert.equal(store.segments.length, 1);
});

// -- 0.8.0, the accuracy round ----------------------------------------------

test('a roster join is handed to the controller so it can raise a brief', () => {
  reset();
  const change = applyEvent({ seq: 1, ev: 'roster', data: { ev: 'join', who: 'Kira', t: '1' } });
  assert.equal(change.rosterJoin.who, 'Kira');
});

test('a display name links to a NAMED voice, case-insensitively, and never to auto', () => {
  reset();
  store.speakers.set(1, { id: 1, name: 'Kira', auto: 'Speaker_03' });
  store.speakers.set(2, { id: 2, name: null, auto: 'Speaker_07' });
  assert.equal(speakerByDisplayName('kira')?.id, 1);
  assert.equal(speakerByDisplayName('  KIRA  ')?.id, 1);
  // The generated label is not a name and must never win a brief: a person who
  // really is called Speaker_07 would otherwise get somebody else's account.
  assert.equal(speakerByDisplayName('Speaker_07'), null);
  assert.equal(speakerByDisplayName(''), null);
  assert.equal(speakerByDisplayName(null), null);
});

test('the cross-check flag is its own doubt, not the speaker one', () => {
  reset();
  const shaky = { ...seg(1), asr_confidence: 'shaky', match_score: 0.9, overlap_frac: 0.01 };
  assert.equal(isShaky(shaky), true);
  // A confident name with shaky words is NOT "uncertain" — that mark is about
  // who spoke, and saying "weak voice match" here would be a lie.
  assert.equal(isUncertain(shaky), false);
  assert.equal(isShaky({ ...seg(1), asr_confidence: 'solid' }), false);
  assert.equal(isShaky(seg(1)), false); // no cross-check ran
});

test('text provenance is one word, and silent for a first-pass reading', () => {
  reset();
  assert.match(textViaNote({ text_via: 'context' }), /surrounding audio/);
  assert.match(textViaNote({ text_via: 'arbiter' }), /language arbiter/);
  assert.equal(textViaNote({ text_via: 'live' }), '');
  assert.equal(textViaNote({}), '');
});

test('a vocab event replaces the cached glossary; a note event is handed on', () => {
  reset();
  const v = { user: ['PhysBones'], auto: { roster: [], worlds: [], corrections: [] }, effective: ['PhysBones'] };
  assert.deepEqual(applyEvent({ seq: 1, ev: 'vocab', data: v }).vocab, v);
  assert.deepEqual(store.vocab, v);
  const note = { id: 7, segment_id: 90, text: 'the link', t_ms: 1, t_ns: '1', state: 'open' };
  assert.deepEqual(applyEvent({ seq: 2, ev: 'note', data: note }).note, note);
  assert.equal(applyEvent({ seq: 3, ev: 'note', data: { text: 'no id' } }), null);
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

// ---- 0.7.7: the conversational language prior ----------------------------

test('a re-read transcript is marked even when its speaker is certain', () => {
  reset();
  store.speakers.set(1, { id: 1, name: 'Kira', total_ms: 1000 });

  // The name is not in doubt — a confident match, no overlap — but the WORDS
  // on screen came out of the arbiter rather than out of the primary model,
  // and that is a fact about the transcript the reader is entitled to.
  const reread = seg(1, {
    speaker: 1,
    match_score: 0.81,
    label_via: 'match',
    overlap_frac: 0.02,
    lang: 'de',
    lang_via: 're-decode',
  });
  assert.equal(isUncertain(reread), false, 'the speaker is not what is in doubt');
  assert.equal(hasMark(reread), true, 'but there is still something to say');
  const why = uncertainReason(reread);
  assert.match(why, /re-read/i);
  assert.match(why, /arbiter/i);
  // …and it must NOT invent a doubt about the name it was not asked about.
  assert.doesNotMatch(why, /voice match/i);
});

test('a flip nothing could settle says the words were kept', () => {
  reset();
  const flagged = seg(2, {
    speaker: 1,
    match_score: 0.72,
    label_via: 'match',
    overlap_frac: 0.02,
    lang: null,
    lang_via: 'mismatch',
  });
  assert.equal(hasMark(flagged), true);
  assert.match(uncertainReason(flagged), /kept as they are/i);
});

test('the ordinary language provenances say nothing at all', () => {
  // `model`, `classified` and `context` never changed a word, so there is
  // nothing for the "?" to explain — and a mark on every row is no mark.
  for (const via of ['model', 'classified', 'context', null, undefined]) {
    const ordinary = seg(3, {
      speaker: 1,
      match_score: 0.9,
      label_via: 'match',
      overlap_frac: 0.02,
      lang: 'de',
      lang_via: via,
    });
    assert.equal(languageNote(ordinary), '', String(via));
    assert.equal(hasMark(ordinary), false, String(via));
  }
});

test('both doubts show together when both are real', () => {
  reset();
  store.speakers.set(1, { id: 1, name: 'Kira', total_ms: 1000 });
  const both = seg(4, {
    speaker: 1,
    match_score: null,
    label_via: 'proximity',
    overlap_frac: 0.02,
    lang: 'de',
    lang_via: 're-decode',
  });
  const why = uncertainReason(both);
  assert.match(why, /surrounding turn/i, 'who');
  assert.match(why, /arbiter/i, 'and what');
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
  // 0.11.0: Japanese is routed by the audio identifier, not the text classifier.
  assert.equal(languageLabel({ languages: ['ja'] }), 'Japanese');
  // 0.11.6: and the two the same route learned to decode.
  assert.equal(languageLabel({ languages: ['ko'] }), 'Korean');
  assert.equal(languageLabel({ languages: ['zh'] }), 'Chinese');
  // Every choice the control offers has a value the daemon accepts and a line
  // saying what it does — "German" alone does not explain a changed transcript.
  assert.equal(LANGUAGE_CHOICES.length, 7);
  for (const c of LANGUAGE_CHOICES) {
    assert.ok(c.title.length > 20, `${c.label} does not explain itself`);
    for (const code of c.value ? c.value.split(',') : []) {
      // The closed set is `lang::KNOWN` in the daemon: a tag exists only if a
      // decoder for it is catalogued, which is why `yue` is not here even
      // though SenseVoice writes it.
      assert.ok(
        ['de', 'en', 'ja', 'ko', 'zh'].includes(code),
        `${code} is not a language the daemon knows`,
      );
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

// ---------------------------------------------------------------------------
// the resync, and what a half-answered one is allowed to do (audit finding #11)
// ---------------------------------------------------------------------------

/**
 * A daemon made of five answers, any of which may throw. `ask` reads
 * `window.recall.request`, so this is the whole seam — no Electron, no socket.
 */
function fakeDaemon(answers) {
  const calls = [];
  globalThis.window = {
    recall: {
      request: async (method, params) => {
        calls.push(method);
        const fn = answers[method];
        if (!fn) return { ok: false, err: { code: 'unknown_method', msg: method } };
        try {
          return { ok: true, data: fn(params) };
        } catch (e) {
          return { ok: false, err: { code: e.code ?? 'failed', msg: e.message } };
        }
      },
    },
  };
  return calls;
}

const boom = (code = 'io') => () => {
  throw Object.assign(new Error('the socket flapped'), { code });
};

test('a resync whose voicebank query fails keeps the voices it had — and retries', async () => {
  reset();
  store.speakers.set(12, { id: 12, name: 'Kira', segments: 4, total_ms: 8000 });
  store.sources = [{ match_key: 'VRChat.exe', kind: 'app', allowed: true }];

  let speakersUp = false;
  const calls = fakeDaemon({
    'speakers.list': () => {
      if (!speakersUp) throw Object.assign(new Error('the socket flapped'), { code: 'io' });
      return { speakers: [{ id: 12, name: 'Kira', segments: 9, total_ms: 9000 }] };
    },
    transcript: () => ({ segments: [seg(1), seg(2)] }),
    'sources.list': () => ({ sources: [{ match_key: 'VRChat.exe', kind: 'app', allowed: true }] }),
    'mic.get': () => ({ enabled: false, mode: 'follow', state: 'off' }),
    'graph.summary': () => ({ counts: { open: 2 }, enrichment: { phase: 'off' } }),
  });

  await reloadAll({ retryMin: 5 });

  // The old code assigned `{speakers: []}` over the live model here: the
  // voicebank read as deleted and every transcript row fell back to "Speaker 12".
  assert.equal(store.speakers.size, 1, 'a failed query emptied the voicebank');
  assert.equal(speakerLabel(12), 'Kira');
  assert.deepEqual(store.resync.stale, ['speakers']);
  assert.equal(isCatchingUp(), true, 'the footer would still be green');
  assert.equal(store.loaded, false, 'a half-answered resync must not claim to have loaded');
  assert.equal(store.resync.retrying, true, 'nothing was scheduled to try again');
  // The slices that DID answer are fresh.
  assert.deepEqual(store.segments.map((s) => s.id), [1, 2]);
  assert.equal(store.graph.counts.open, 2);

  speakersUp = true;
  await new Promise((r) => setTimeout(r, 80));
  assert.equal(isCatchingUp(), false, 'the retry never landed');
  assert.equal(store.speakers.get(12).segments, 9, 'the retry did not refresh the stale slice');
  assert.equal(store.loaded, true);
  assert.ok(calls.filter((m) => m === 'speakers.list').length >= 2, 'the failed query was never re-asked');
  cancelResyncRetry();
});

test('a resync that fails outright changes nothing and says so', async () => {
  reset();
  store.speakers.set(12, { id: 12, name: 'Kira', segments: 4, total_ms: 8000 });
  store.segments = [seg(1), seg(2), seg(3)];
  store.segById = new Map(store.segments.map((s) => [s.id, s]));
  store.sources = [{ match_key: 'VRChat.exe', kind: 'app', allowed: true }];
  store.graph = { counts: { open: 5 }, enrichment: { phase: 'off' }, config: null };

  fakeDaemon({
    'speakers.list': boom(),
    transcript: boom(),
    'sources.list': boom(),
    'mic.get': boom(),
    'graph.summary': boom(),
  });
  await reloadAll({ retry: false });

  // This is the daemon-restart case: the socket flaps once more as the client
  // re-asks, all five reject, and the old code declared the world empty.
  assert.equal(store.speakers.size, 1, 'the voicebank was wiped');
  assert.equal(store.segments.length, 3, 'the transcript was wiped');
  assert.equal(store.sources.length, 1, 'the sources were wiped');
  assert.equal(store.graph.counts.open, 5, 'the graph counts were wiped');
  assert.deepEqual(store.resync.stale, ['speakers', 'transcript', 'sources', 'mic', 'graph']);
  assert.equal(isCatchingUp(), true);
});

test('a daemon too old for a method is not a stale slice', async () => {
  reset();
  fakeDaemon({
    'speakers.list': () => ({ speakers: [{ id: 1, name: 'Ash' }] }),
    transcript: () => ({ segments: [] }),
    'sources.list': () => ({ sources: [] }),
    // no mic.get, no graph.summary → unknown_method, which is an ANSWER: this
    // daemon predates those halves of the app (0.6.0 / 0.7.0).
  });
  await reloadAll({ retry: false });
  assert.deepEqual(store.resync.stale, []);
  assert.equal(isCatchingUp(), false, 'an older daemon must not read as a failed resync');
  assert.equal(store.loaded, true);
  assert.equal(store.graph.counts, null);
});

// ---------------------------------------------------------------------------
// the two footer/badge rules the views used to disagree about (finding #25)
// ---------------------------------------------------------------------------

test('the sources badge counts applications, never the microphone', () => {
  reset();
  store.sources = [
    { match_key: 'VRChat.exe', kind: 'app', allowed: true },
    { match_key: 'Discord', kind: 'app', allowed: true },
    { match_key: 'firefox', kind: 'app', allowed: false },
    // Allowed, and still not one of these: the mic has its own card, its own
    // method and its own default. The rail used to count it and the view did
    // not, so the same daemon read 3 or 2 depending on which painted last.
    { match_key: 'mic', kind: 'mic', allowed: true },
  ];
  assert.equal(allowedAppCount(), 2);
  assert.deepEqual(appSources().map((s) => s.match_key), ['VRChat.exe', 'Discord', 'firefox']);
});

// ---------------------------------------------------------------------------
// the room microphone (0.10.0)
// ---------------------------------------------------------------------------

test('the room microphone is not an application either', () => {
  reset();
  store.sources = [
    { match_key: 'VRChat.exe', kind: 'app', allowed: true },
    { match_key: 'mic', kind: 'mic', allowed: true },
    // A second device with its own switch, its own card and its own default.
    // Counting it as an allowed application would put the rail badge back into
    // exactly the disagreement finding #25 was about.
    { match_key: 'room', kind: 'room', allowed: true },
  ];
  assert.equal(allowedAppCount(), 1);
  assert.deepEqual(appSources().map((s) => s.match_key), ['VRChat.exe']);
});

test('a room block folds in from the method, the event and the status push', () => {
  reset();
  assert.equal(store.room.enabled, false);
  assert.equal(store.room.state, 'off');

  applyRoom({ enabled: true, mode: 'always', device: 'alsa_input.desk', state: 'always:idle', active: false });
  assert.equal(store.room.device, 'alsa_input.desk');
  assert.equal(store.room.state, 'always:idle');

  // The `room` event, on the status topic like the mic's.
  const change = applyEvent({ seq: 1, ev: 'room', data: { state: 'always:active', active: true } });
  assert.deepEqual(change, { room: true });
  assert.equal(store.room.state, 'always:active');
  // A partial block must not erase the device it did not mention.
  assert.equal(store.room.device, 'alsa_input.desk');

  // And a status push converges a client that missed the event.
  applyEvent({ seq: 2, ev: 'status', data: { room: { state: 'off', enabled: false } } });
  assert.equal(store.room.state, 'off');
});

test('the room chip separates "no device" from "waiting" from "unplugged"', () => {
  // The state the headset cannot be in is the one worth having a word for: a
  // room mic with nothing chosen is unconfigured, not waiting.
  assert.equal(roomChip('off').text, 'off');
  assert.equal(roomChip('needs-device').text, 'no device chosen');
  assert.equal(roomChip('needs-device').cls, 'chip warn');
  assert.equal(roomChip('following:idle').text, 'waiting for an allowed app');
  assert.equal(roomChip('always:idle').text, 'device not connected');
  assert.equal(roomChip('always:active').live, true);
  assert.equal(roomChip('following:active').live, true);
});

test('a disconnect stops the footer quoting the daemon it lost', () => {
  reset();
  const status = { queue_depth: 3, drops: 0, storage: { db_bytes: 1 }, semantic: { available: true } };
  applyConnState({ conn: { status: 'connected' }, status });
  assert.equal(liveStatus().queue_depth, 3);

  // Main nulls its status the moment the connection drops. The renderer used
  // to keep the last one and go on quoting "queue 3 · db 1.2 GB" next to the
  // words "daemon offline". The counters are no longer live…
  applyConnState({ conn: { status: 'offline' }, status: null });
  assert.equal(liveStatus(), null, 'the footer would still be quoting a dead daemon');
  assert.equal(store.statusLive, false);
  // …but the STRUCTURAL facts in the block survive, because "the semantic model
  // is not installed on this machine" would be a different wrong claim.
  assert.equal(store.status.semantic.available, true);

  applyConnState({ conn: { status: 'connected' }, status: { ...status, queue_depth: 7 } });
  assert.equal(liveStatus().queue_depth, 7);
});

// ---------------------------------------------------------------------------
// what a split reply means (audit finding #17)
// ---------------------------------------------------------------------------

test('a split past the event cap asks for a re-query, and says what moved', () => {
  // The daemon's reply shape, verbatim from PROTOCOL and service.rs.
  const big = splitOutcome(
    { op: 'op_7', kept: 1, minted: 9, auto: 'Speaker_09', moved_segments: 412, resync: true },
    'Kira'
  );
  assert.equal(big.resync, true, 'resync:true was discarded — those rows keep the old name for ever');
  assert.match(big.text, /412 segments/);
  assert.match(big.text, /Speaker_09/);
  // The old toast said "Re-clustering started (op_7)" over an operation that
  // had already finished, and the sheet promised progress in the status bar.
  assert.doesNotMatch(big.text, /status bar/i);
  assert.doesNotMatch(big.text, /started/i);

  const small = splitOutcome(
    { op: 'op_8', kept: 1, minted: 9, auto: 'Speaker_09', moved_segments: 1, resync: false },
    'Kira'
  );
  assert.equal(small.resync, false, 'below the cap the per-row events already did the work');
  assert.match(small.text, /1 segment moved/);
});

test('a re-published row older than the window is not news', () => {
  // The re-decode and cross-check workers announce every archive row they
  // stamp. A client holding the tail must not file yesterday under now, and
  // must not count the turn against its speaker a second time.
  reset();
  store.speakers.set(1, { id: 1, name: 'Kira', segments: 3, total_ms: 6000 });
  for (const id of [10, 11, 12]) applyEvent({ seq: id, ev: 'segment', data: seg(id) });
  const before = store.segments.map((s) => s.id);
  const change = applyEvent({ seq: 99, ev: 'segment', data: seg(3, { text_via: 'context' }) });
  assert.deepEqual(store.segments.map((s) => s.id), before, 'the window is unchanged');
  assert.ok(!store.segById.has(3), 'the old row is not held');
  assert.ok(!change?.added, 'nothing was added');
  assert.equal(store.speakers.get(1).segments, 6, 'no double count: three arrivals, one re-publish');
  assert.equal(store.appended, 3);
  // …but a row that belongs INSIDE the window (a late turn) is still filed in order.
  applyEvent({ seq: 100, ev: 'segment', data: seg(11, { text: 'late twin' }) }); // known → update
  const late = applyEvent({ seq: 101, ev: 'segment', data: { ...seg(11), id: 111, t_ms: seg(11).t_ms + 1 } });
  assert.ok(late.added);
  assert.deepEqual(store.segments.map((s) => s.id), [10, 11, 111, 12]);
});

// ---- 0.9.0: the assistant --------------------------------------------------

test('a reminder is the alarm and the note beside it is the row', () => {
  // Two events, and they do different jobs. The `reminder` is the one thing in
  // this protocol a client is expected to interrupt somebody with; the `note`
  // that follows carries `fired` so a list already on screen repaints without
  // re-querying.
  reset();
  const r = applyEvent({
    seq: 1,
    ev: 'reminder',
    data: { note_id: 702, text: 'morgen um zehn an den Link', due_ms: 1_700_000_000_000 },
  });
  assert.equal(r.reminder.note_id, 702);
  assert.equal(r.reminder.text, 'morgen um zehn an den Link');

  const n = applyEvent({
    seq: 2,
    ev: 'note',
    data: { id: 702, segment_id: 9, text: 'morgen um zehn an den Link', state: 'open', fired: true },
  });
  assert.equal(n.note.fired, true, 'fired is not done — it is still an open note');
  assert.equal(n.note.state, 'open');

  // A reminder with no note id is not a reminder. An older daemon cannot send
  // one, but a malformed frame must not become an undefined notification.
  assert.equal(applyEvent({ seq: 3, ev: 'reminder', data: {} }), null);
  assert.equal(applyEvent({ seq: 4, ev: 'reminder', data: null }), null);
});

test('a digest arrives once per conversation and is never an update', () => {
  reset();
  const d = applyEvent({
    seq: 1,
    ev: 'digest',
    data: { thread_id: 3, day: '2026-09-02', summary: 'Es ging um den Shader.', open: [] },
  });
  assert.equal(d.digest.thread_id, 3);
  assert.equal(d.digest.summary, 'Es ging um den Shader.');
  assert.equal(applyEvent({ seq: 2, ev: 'digest', data: { day: 'x' } }), null, 'no thread, no digest');
});

test('a translation rides on the segment and null is the ordinary answer', () => {
  // The client stores segment rows as the daemon sends them, so this is really
  // a check that the field survives the trip — and that a row without one is
  // not distinguishable from a row from a daemon that never had the column.
  reset();
  applyEvent({ seq: 1, ev: 'segment', data: seg(10, { translation: null }) });
  assert.equal(store.segById.get(10).translation, null);

  const tr = { lang: 'de', text: 'welches Portal war es', via: 'qwen2.5-3b@1' };
  applyEvent({ seq: 2, ev: 'segment', data: seg(11, { translation: tr }) });
  assert.deepEqual(store.segById.get(11).translation, tr);

  // A re-decode that replaced the words arrives as an update to the same row,
  // and the translation it carries replaces the old one rather than merging.
  applyEvent({ seq: 3, ev: 'segment', data: { ...seg(11), translation: null } });
  assert.equal(store.segById.get(11).translation, null, 'a cleared translation clears');
});

test('the translation settings fold in from four places and never lose the list', () => {
  reset();
  // The shipped state: nothing translated, both native languages read, and the
  // translation on the line when there is one.
  assert.equal(store.assist.translate_to, '');
  assert.equal(translationLeads(), true);

  // `assist.get` — the only source that carries the selector's options.
  applyAssist({
    translate_to: 'en',
    read_languages: ['de', 'en'],
    translation_display: 'under',
    languages: [{ code: 'en', name: 'English' }, { code: 'de', name: 'German' }],
  });
  assert.equal(translationLeads(), false, 'under means the original leads');
  assert.equal(store.assist.languages.length, 2);

  // `status`, three seconds later, carries the three values and NOT the list.
  // A replace would empty the selector on every poll.
  applyEvent({
    seq: 1,
    ev: 'status',
    data: { assist: { translate_to: 'en', read_languages: ['en'], translation_display: 'main' } },
  });
  assert.equal(store.assist.languages.length, 2, 'the poll must not empty the selector');
  assert.deepEqual(store.assist.read_languages, ['en']);
  assert.equal(translationLeads(), true);

  // The event another window's change arrives as. It repaints the transcript,
  // which is why it says so.
  const change = applyEvent({ seq: 2, ev: 'assist', data: { translation_display: 'under' } });
  assert.deepEqual(change, { assist: true });
  assert.equal(translationLeads(), false);

  // Junk is ignored rather than stored: a mode nothing can render would make
  // every row fall back on a different guess in every view.
  applyAssist({ translation_display: 'sideways' });
  assert.equal(store.assist.translation_display, 'under');
  assert.equal(applyEvent({ seq: 3, ev: 'assist', data: null }), null);
});
