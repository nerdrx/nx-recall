// The captions rules, tested without a window.
//
// Two properties matter here and neither is visible in a screenshot. The first
// is that a settings block from ANYWHERE — a file written by an older build, a
// hand-edit, a renderer that sent one field — cannot put the window into a
// state no control can produce; the file lives in the user's profile and a
// `size` of 4000 in it is a window with one unreadable word and no way out.
// The second is that the stack's fade is a function of the clock, so it is
// checked against a clock rather than against whichever repaint happened to run.

import test from 'node:test';
import assert from 'node:assert/strict';
import {
  CAPTION_DEFAULTS,
  CAPTION_RANGES,
  YOU_DIM,
  normalizeCaptionSettings,
  translationOf,
  visibleCaptions,
} from '../src/renderer/lib/captions.js';

const seg = (id, over = {}) => ({ id, t_ms: 1_000_000 + id * 1000, text: `line ${id}`, speaker: 1, ...over });
const at = (n, t) => ({ seg: seg(n), at: t });

// -- settings ---------------------------------------------------------------

test('nothing at all is the documented defaults', () => {
  assert.deepEqual(normalizeCaptionSettings(null), { ...CAPTION_DEFAULTS });
  assert.deepEqual(normalizeCaptionSettings(undefined), { ...CAPTION_DEFAULTS });
  assert.deepEqual(normalizeCaptionSettings('nonsense'), { ...CAPTION_DEFAULTS });
});

test('the window is click-through and shows your turns out of the box', () => {
  // Both are load-bearing defaults rather than taste. A caption bar that eats a
  // click into the game is a worse bug than one you have to unlock first; and a
  // stack that silently skips half the conversation reads as dropped words.
  assert.equal(CAPTION_DEFAULTS.clickThrough, true);
  assert.equal(CAPTION_DEFAULTS.showYou, true);
  assert.equal(CAPTION_DEFAULTS.hold_s, 12);
});

test('every numeric setting is clamped to the range its control offers', () => {
  for (const [key, range] of Object.entries(CAPTION_RANGES)) {
    assert.equal(normalizeCaptionSettings({ [key]: -9999 })[key], range.min, `${key} floor`);
    assert.equal(normalizeCaptionSettings({ [key]: 9999 })[key], range.max, `${key} ceiling`);
    assert.equal(normalizeCaptionSettings({ [key]: 'x' })[key], CAPTION_DEFAULTS[key], `${key} nonsense`);
    assert.equal(normalizeCaptionSettings({ [key]: null })[key], CAPTION_DEFAULTS[key], `${key} null`);
  }
});

test('an opacity lands on its step, and reads as one number rather than seventeen', () => {
  assert.equal(normalizeCaptionSettings({ opacity: 0.62 }).opacity, 0.6);
  assert.equal(normalizeCaptionSettings({ opacity: 0.63 }).opacity, 0.65);
  // The bug this is here for: 0.30000000000000004 is the same opacity as 0.3
  // and a different string in every place that renders it.
  assert.equal(String(normalizeCaptionSettings({ opacity: 0.3001 }).opacity), '0.3');
});

test('a partial patch changes one thing and leaves the rest', () => {
  const next = normalizeCaptionSettings({ ...CAPTION_DEFAULTS, turns: 8 });
  assert.equal(next.turns, 8);
  assert.equal(next.size, CAPTION_DEFAULTS.size);
  assert.equal(next.clickThrough, CAPTION_DEFAULTS.clickThrough);
});

test('remembered bounds survive; a corrupt or unusable one does not', () => {
  const good = { x: 40, y: 900, width: 1100, height: 300 };
  assert.deepEqual(normalizeCaptionSettings({ bounds: good }).bounds, good);
  assert.equal(normalizeCaptionSettings({ bounds: { x: 1, y: 2 } }).bounds, null);
  assert.equal(normalizeCaptionSettings({ bounds: { x: 0, y: 0, width: 8, height: 4 } }).bounds, null);
  assert.equal(normalizeCaptionSettings({ bounds: 'somewhere' }).bounds, null);
});

// -- the translation contract -----------------------------------------------

test('a translation block is read only when it actually says something', () => {
  assert.deepEqual(translationOf(seg(1, { translation: { lang: 'en', text: 'hello', via: 'nllb-200' } })), {
    lang: 'en',
    text: 'hello',
    via: 'nllb-200',
  });
  // Absent is the ordinary case and means nothing: a turn in a language the
  // user reads needs no second line.
  assert.equal(translationOf(seg(1)), null);
  assert.equal(translationOf(seg(1, { translation: {} })), null);
  assert.equal(translationOf(seg(1, { translation: { lang: 'en', text: '   ' } })), null);
  assert.equal(translationOf(null), null);
  // A block with no `lang` still renders — the words are the point and the tag
  // is the ornament.
  assert.deepEqual(translationOf(seg(1, { translation: { text: 'hi' } })), { lang: null, text: 'hi', via: null });
});

// -- the visible stack ------------------------------------------------------

test('only the last N turns are on screen, and N is the setting', () => {
  const now = 5_000_000;
  const rows = [1, 2, 3, 4, 5, 6, 7, 8, 9].map((n) => at(n, now - 100));
  const v = visibleCaptions(rows, { turns: 4 }, now);
  assert.deepEqual(v.rows.map((r) => r.seg.id), [6, 7, 8, 9]);
  assert.equal(v.fade, 1);
});

test('the stack fades as one thing, from the NEWEST arrival', () => {
  const now = 5_000_000;
  // An old row under a fresh one must not fade on its own: a bar whose top line
  // has vanished while the one under it is solid reads as a rendering fault.
  const rows = [at(1, now - 60_000), at(2, now - 1000)];
  assert.equal(visibleCaptions(rows, { hold_s: 12 }, now).fade, 1);
  assert.equal(visibleCaptions(rows, { hold_s: 12 }, now).rows.length, 2);
});

test('a stack goes when nothing new has been said for hold_s', () => {
  const now = 5_000_000;
  const rows = [at(1, now - 11_000)];
  assert.equal(visibleCaptions(rows, { hold_s: 12 }, now).fade, 1);
  // One second of actual fading at the end of the hold, so it leaves rather
  // than blinks.
  const half = visibleCaptions([at(1, now - 12_500)], { hold_s: 12 }, now);
  assert.ok(half.fade > 0 && half.fade < 1, `mid-fade, got ${half.fade}`);
  assert.equal(half.hidden, false);
  const gone = visibleCaptions([at(1, now - 20_000)], { hold_s: 12 }, now);
  assert.equal(gone.fade, 0);
  assert.equal(gone.hidden, true);
});

test('your own turns are dimmer when shown, and dropped before the window when not', () => {
  const now = 5_000_000;
  const mine = (n) => ({ seg: seg(n, { speaker: 9 }), at: now - 100 });
  const rows = [at(1, now - 100), mine(2), at(3, now - 100), mine(4), at(5, now - 100)];
  const isYou = (s) => s.speaker === 9;

  const shown = visibleCaptions(rows, { turns: 3, showYou: true }, now, isYou);
  assert.deepEqual(shown.rows.map((r) => r.seg.id), [3, 4, 5]);
  assert.equal(shown.rows.find((r) => r.seg.id === 4).dim, YOU_DIM);
  assert.equal(shown.rows.find((r) => r.seg.id === 5).dim, 1);

  // Hidden means you get three of THEIR turns, not three turns of which two
  // are yours — the filter is taken before the last-N window, not after.
  const hidden = visibleCaptions(rows, { turns: 3, showYou: false }, now, isYou);
  assert.deepEqual(hidden.rows.map((r) => r.seg.id), [1, 3, 5]);
});

test('an empty feed is empty rather than a stack of nothing', () => {
  const v = visibleCaptions([], {}, 5_000_000);
  assert.deepEqual(v.rows, []);
  assert.equal(v.fade, 0);
  assert.equal(v.hidden, true);
});

test('a settings block that has never been normalised is normalised on the way in', () => {
  const now = 5_000_000;
  const rows = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10].map((n) => at(n, now - 100));
  // 99 turns is not a state any control can reach, so the stack must not honour
  // it even if a file says so.
  assert.equal(visibleCaptions(rows, { turns: 99 }, now).rows.length, CAPTION_RANGES.turns.max);
});
