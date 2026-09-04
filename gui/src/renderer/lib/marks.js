// The small marks a row wears, where more than one view wears the same one.
//
// Everything here exists because it has to be IDENTICAL in two places: a turn
// the second decoder disagreed with looks the same in the transcript and in a
// search result, or the mark stops being a vocabulary and becomes two
// coincidences. Same for a translated turn's two lines, and same again for
// 0.12.4's mood tint and event chips. The transcript's "?" is not here because
// only the transcript has one, and the day search grows a "?" is the day it
// moves.

import { h } from './dom.js';
import {
  SHAKY_NOTE,
  translationLeads,
  moodShown,
  moodTags,
  moodTint,
  moodRendered,
} from './store.js';
import { EVENT_LABELS, eventsOf, moodColor } from './palette.js';

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

// ---- 0.12.4, how a turn sounded ---------------------------------------------

/**
 * The mood a row may be tinted and tagged by, or null.
 *
 * Two gates, and they are different kinds of thing:
 *
 * - `moodRendered()` is the DAEMON's — a measurement, not a setting. The mood
 *   tag is stored on every machine that runs the pass and drawn only where the
 *   number earned it (FINDINGS §42). A client that guessed would be telling
 *   somebody how their friend felt on evidence nobody checked.
 * - `neutral` has no colour and no chip. It is what a transcript already looks
 *   like, and a mark on three rows in four is not a mark.
 */
function moodOf(seg) {
  if (!moodRendered()) return null;
  const m = seg?.mood;
  return m && m !== 'neutral' ? m : null;
}

/**
 * The chips at the end of a row: what was on the clip besides the words, and
 * — where the measurement allows it — how it sounded.
 *
 * Returns null when there is nothing to say, which is most rows: an empty
 * container on every line would put a gap in the grid for no reason.
 *
 * The EVENTS are not gated on `moodRendered()`. They were measured separately
 * and they passed; the mood is the half that did not.
 */
export function moodChips(seg) {
  if (!moodShown()) return null;
  // The EVENTS are gated on `moodShown()` and the MOOD on `moodTags()`, and
  // that is the whole difference between the two settings: a mood can be a
  // chip or a colour, an event can only be a chip. In `tint` the mood moves to
  // the words and laughter keeps its chip, because there is no such thing as
  // the colour of laughter — and because a mode that drew nothing at all would
  // be a setting that does not act.
  const events = eventsOf(seg);
  const mood = moodTags() ? moodOf(seg) : null;
  if (!events.length && !mood) return null;
  return h(
    'span',
    { class: 'mood-chips', dataset: { mood: mood ?? '', events: events.join(',') } },
    // Events first: they are a fact about the recording, and the mood — where
    // it is shown at all — is a reading of it.
    ...events.map((e) =>
      h('span', {
        class: `mood-chip event ${e}`,
        dataset: { event: e },
        text: EVENT_LABELS[e],
        title: `The decoder heard ${EVENT_LABELS[e]} on this turn. It is a tag on the audio, not a word anybody said.`,
      })
    ),
    mood
      ? h('span', {
          class: `mood-chip mood ${mood}`,
          dataset: { moodChip: mood },
          text: mood,
          style: `color:${moodColor(mood)}`,
          title: `The decoder read this turn as ${mood}. A guess from the sound of a voice, not from the words.`,
        })
      : null
  );
}

/**
 * The inline style that tints a row's words by mood, or null.
 *
 * A string rather than a class, for the reason every speaker colour in this app
 * is a string: the colour is `hsl(<hue> var(--mood-s) var(--mood-l))`, so it
 * repaints on a theme flip without re-rendering a row. The CLASS beside it
 * (`has-mood`) is what a test and a stylesheet select on — a colour cannot be
 * queried and a class can.
 */
export function moodStyle(seg) {
  if (!moodTint()) return null;
  const c = moodColor(moodOf(seg));
  // null and not '': `h()` drops a null attribute and would otherwise put a
  // bare `style=""` on every untinted row in the app.
  return c ? `color:${c}` : null;
}

/** The class a tinted row's words cell wears, or ''. */
export function moodClass(seg) {
  const mood = moodTint() ? moodOf(seg) : null;
  return mood ? ` has-mood mood-${mood}` : '';
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
  // 0.12.4: the mood tint goes on the WORDS CELL, which is why it is applied
  // here and not in the two views. Both of them draw their words through this
  // function, so a tint that lived in the transcript would be a transcript-only
  // feature and a search hit for the same turn would read differently.
  const tint = moodStyle(seg);
  const tinted = moodClass(seg);
  if (!tr?.text) {
    return h('span', { class: `txt${tinted}`, style: tint }, ...renderOriginal(said));
  }

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
    {
      // The tint colours the cell and both lines inherit it, translation
      // included: the mood is a fact about how the turn SOUNDED, and the
      // reading of it is the same turn.
      class: `txt has-translation${leads ? ' translation-main' : ''}${tinted}`,
      style: tint,
    },
    ...(leads ? [translated, original] : [original, translated])
  );
}
