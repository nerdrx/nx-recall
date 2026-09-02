// The small marks a row wears, where more than one view wears the same one.
//
// There is exactly one of these today and it exists because it has to be
// IDENTICAL in two places: a turn the second decoder disagreed with looks the
// same in the transcript and in a search result, or the mark stops being a
// vocabulary and becomes two coincidences. The transcript's "?" is not here
// because only the transcript has one, and the day search grows a "?" is the
// day it moves.

import { h } from './dom.js';
import { SHAKY_NOTE } from './store.js';

/**
 * The cross-check disagreed (0.8.0). A wave rather than a letter: the "?" next
 * to it is already a glyph you read, and two readable glyphs on one row is a
 * sentence nobody asked for. It is `cursor: help` and it says one plain thing
 * on hover — there is no action behind it, because there is nothing the app can
 * do about a disagreement that the person reading cannot do better.
 */
export function shakyMark() {
  return h('span', {
    class: 'shaky-mark',
    text: '≈',
    title: SHAKY_NOTE,
    'aria-label': SHAKY_NOTE,
  });
}
