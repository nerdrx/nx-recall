// Search's Keyword / Smart / Both control, and the marker that says which leg
// found a hit.
//
// Kept out of search.js because it is one idea with three parts — a mode, a
// request shape and a badge — and all three have to agree. The `via` field is
// not decoration: "these words are in there" and "this seemed to mean the same
// thing" are different claims, and a person deciding whether to trust a result
// is entitled to know which one they are reading.

import { h } from '../lib/dom.js';

/** The three modes, in the order they appear. Sentence case (NX Clear §14). */
export const MODES = [
  {
    id: 'keyword',
    label: 'Keyword',
    method: 'search',
    hint: 'Turns containing these words.',
  },
  {
    id: 'smart',
    label: 'Smart',
    method: 'search.semantic',
    mode: 'semantic',
    hint: 'Turns that mean this, in either language, whatever words they used.',
  },
  {
    id: 'both',
    label: 'Both',
    method: 'search.semantic',
    mode: 'hybrid',
    hint: 'Words and meaning, merged — each hit says which one found it.',
  },
];

export function modeById(id) {
  return MODES.find((m) => m.id === id) ?? MODES[0];
}

/**
 * Whether the daemon can answer a Smart search, from its `status` block.
 * A daemon older than 0.6.5 has no `semantic` key at all, which is the same
 * answer as "not installed" and must not throw.
 */
export function semanticState(status) {
  const s = status?.semantic;
  if (!s || !s.available) {
    return {
      available: false,
      how: s?.how ?? 'Semantic search is not installed on this machine.',
    };
  }
  return {
    available: true,
    model: s.model ?? null,
    indexed: Number(s.indexed ?? 0),
    eligible: Number(s.eligible ?? 0),
    pending: Number(s.pending ?? 0),
  };
}

/**
 * Which mode to start in. `Both` when the model is there — the merged list is
 * strictly better than either half — and `Keyword` when it is not, because
 * that is the only one that can answer.
 */
export function defaultMode(state) {
  return state.available ? 'both' : 'keyword';
}

/**
 * The segmented control plus the line under it.
 *
 * When the model is absent the control does NOT disappear. A control that
 * vanishes teaches nobody anything; a disabled one that says how to turn it on
 * is the difference between a missing feature and an unclaimed one.
 */
export function modeControl({ selected, available, how, onSelect }) {
  const note = h('p', { class: 'sub seg-note' });
  const buttons = MODES.map((m) => {
    const off = !available && m.id !== 'keyword';
    const b = h('button', {
      type: 'button',
      class: 'seg-opt',
      id: `search-mode-${m.id}`,
      'aria-pressed': String(m.id === selected),
      ...(off ? { disabled: true, title: how } : {}),
      onclick: () => onSelect(m.id),
    });
    b.textContent = m.label;
    return b;
  });
  const row = h('div', { class: 'seg-ctl', role: 'group', 'aria-label': 'Search mode' }, ...buttons);

  function paint(next) {
    for (const [i, b] of buttons.entries()) b.setAttribute('aria-pressed', String(MODES[i].id === next));
    note.textContent = available
      ? modeById(next).hint
      : `${how} Until then, Keyword search covers everything captured.`;
  }
  paint(selected);
  return { el: h('div', { class: 'search-mode-control' }, row, note), paint };
}

/**
 * The badge on a hit. Only ever rendered in Both — in the single-leg modes
 * every row came the same way and a badge on all of them says nothing.
 */
export function viaBadge(via) {
  const label = { keyword: 'words', semantic: 'meaning', both: 'words + meaning' }[via];
  if (!label) return null;
  return h('span', {
    class: `chip via via-${via}`,
    text: label,
    title:
      via === 'keyword'
        ? 'Contains the words you typed.'
        : via === 'semantic'
          ? 'Means something close to what you typed — the words may be different, or in the other language.'
          : 'Both: it contains the words and it means the same thing.',
  });
}

/** Params for one search, given the mode and the facets. */
export function requestFor(modeId, params) {
  const m = modeById(modeId);
  return [m.method, m.mode ? { ...params, mode: m.mode } : { ...params }];
}

/**
 * The sentence under the result count. Says what was searched and how, because
 * "4 matches" means something different in each mode.
 */
export function resultSummary(modeId, res, q) {
  const total = res?.total ?? 0;
  const plural = total === 1 ? 'match' : 'matches';
  const forQ = q ? ` for “${q}”` : '';
  if (modeId === 'keyword') return `${total} ${plural}${forQ}`;
  const took = Number.isFinite(res?.took_ms) ? ` · ${res.took_ms.toFixed(0)} ms` : '';
  const how = modeId === 'smart' ? 'by meaning' : 'by words and meaning';
  return `${total} ${plural}${forQ}, ${how}${took}`;
}
