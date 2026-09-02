// The small marks a row wears, where more than one view wears the same one.
//
// There is exactly one of these today and it exists because it has to be
// IDENTICAL in two places: a turn the second decoder disagreed with looks the
// same in the transcript and in a search result, or the mark stops being a
// vocabulary and becomes two coincidences. The transcript's "?" is not here
// because only the transcript has one, and the day search grows a "?" is the
// day it moves.

import { h } from './dom.js';
import { SHAKY_NOTE, translationLeads } from './store.js';

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

/**
 * The words cell of a row, translated or not (0.9.0, reworked in 0.10.2).
 *
 * 0.9.0 put the translation under the words and argued the original must lead
 * because the transcript is a record. That argument is right about the DATA and
 * wrong about the reader: somebody who cannot read the original is not reading
 * a record, they are reading a wall of text with a hint under each line. So the
 * order is now a setting (`[assist] translation_display`), and `main` — the
 * translation on the line, the original as subtext — is the default.
 *
 * What does NOT change with the setting, in either mode:
 *
 * - both lines are always there. The original never leaves the row;
 * - the translation always says which language it is in and which model wrote
 *   it, because a paraphrase presented as a quotation is what this whole
 *   feature is bounded against;
 * - and in `main` the original carries its own language code, so the line the
 *   reader is being shown *instead of* the record is one keystroke from the
 *   record itself.
 *
 * `renderOriginal` builds the original's nodes — plain text in the transcript,
 * highlighted spans in a search hit — so the two surfaces cannot drift.
 */
export function translationCell(seg, renderOriginal = (t) => [t]) {
  const said = seg?.text || '…';
  const tr = seg?.translation;
  if (!tr?.text) return h('span', { class: 'txt' }, ...renderOriginal(said));

  const leads = translationLeads();
  const translated = h(
    'span',
    {
      class: 'txt-translated',
      lang: tr.lang || undefined,
      dataset: { translation: tr.lang || '', via: tr.via || '' },
      title: leads
        ? `Translated into ${tr.lang || 'your language'} by ${tr.via || 'the local model'}. The line below is what was actually said.`
        : `Translated into ${tr.lang || 'your language'} by ${tr.via || 'the local model'}. The line above is what was actually said.`,
    },
    // The arrow points DOWN the row at the line it is a reading of, so it only
    // makes sense on the second line. In `main` the translation is the first.
    leads ? null : h('span', { class: 'tr-mark', 'aria-hidden': 'true', text: '↳' }),
    tr.text
  );
  const original = h(
    'span',
    {
      class: 'txt-said',
      lang: seg?.lang || undefined,
      dataset: { said: seg?.lang || '' },
      title: leads ? 'What was actually said. The line above is a translation of it.' : undefined,
    },
    // The language code, and only in `main`: it is the difference between "a
    // quieter second line" and "the original, in Polish". In `under` the
    // original is already the line you are reading and needs no label.
    leads && seg?.lang
      ? h('span', { class: 'said-lang', 'aria-label': `said in ${seg.lang}`, text: seg.lang })
      : null,
    ...renderOriginal(said)
  );
  return h(
    'span',
    { class: `txt has-translation${leads ? ' translation-main' : ''}` },
    ...(leads ? [translated, original] : [original, translated])
  );
}
