import { h, clear, fmtClock } from './dom.js';

// Keep original grapheme boundaries: case folding can change string length.
export function literalMatchParts(text, query) {
  text = String(text ?? '');
  const fold = value => value.normalize('NFD').replace(/\p{M}/gu, '').toLowerCase();
  const needle = fold(String(query ?? '').trim());
  if (!needle) return [{ text, matched: false }];
  let normalized = ''; const ranges = [];
  for (const item of new Intl.Segmenter(undefined, { granularity: 'grapheme' }).segment(text)) {
    const word = fold(item.segment);
    for (let i = 0; i < word.length; i++) ranges.push([item.index, item.index + item.segment.length]);
    normalized += word;
  }
  const parts = []; let cursor = 0, at = 0;
  while ((at = normalized.indexOf(needle, at)) !== -1) {
    const start = ranges[at][0], end = ranges[at + needle.length - 1][1];
    if (start >= cursor) {
      if (start > cursor) parts.push({ text: text.slice(cursor, start), matched: false });
      parts.push({ text: text.slice(start, end), matched: true }); cursor = end;
    }
    at += needle.length;
  }
  if (cursor < text.length || !parts.length) parts.push({ text: text.slice(cursor), matched: false });
  return parts;
}
export function conversationMatches(rows, query) {
  return [...rows].sort((a,b) => a.t_ms-b.t_ms || a.id-b.id).map(row => ({ row, parts: literalMatchParts(row.text, query) }))
    .filter(item => item.parts.some(part => part.matched));
}
export function stepConversationMatch(index, direction, count) {
  return count ? ((index + direction) % count + count) % count : -1;
}
let resume = null;
export function mountConversationFind({ request, reveal, jump }) {
  let state = resume; resume = null;
  let generation = 0, dead = false, matches = [], index = -1, trigger = null;
  const deleted = new Set(), corrected = new Map();
  const latestRows = rows => rows.filter(row => !deleted.has(row.id)).map(row => ({ ...row, ...corrected.get(row.id) }));
  const panel = h('section', { class: 'conversation-find', hidden: true, 'aria-label': 'Find in conversation' });
  const input = h('input', { class: 'input', id: 'conversation-find-query', type: 'search', placeholder: 'Find words in this conversation', 'aria-label': 'Find words in this conversation', oninput: render });
  const status = h('span', { role: 'status', class: 'sub' });
  const timeline = h('nav', { class: 'conversation-find-timeline', 'aria-label': 'Matching moments' });
  const preview = h('p', { class: 'conversation-find-preview' });
  const previous = h('button', { class: 'btn small', text: 'Previous match', onclick: () => select(-1) });
  const next = h('button', { class: 'btn small', text: 'Next match', onclick: () => select(1) });
  const retry = h('button', { class: 'btn small', text: 'Try again', hidden: true, onclick: () => open(state.thread, trigger, input.value) });
  function close() { ++generation; panel.hidden = true; state = null; clear(preview); clear(timeline); trigger?.focus({ preventScroll: true }); }
  panel.append(h('div', { class: 'conversation-find-controls' }, input, previous, next,
    h('button', { class: 'btn small', text: 'Close find', onclick: close })), status, retry, timeline, preview);
  panel.onkeydown = e => {
    if (e.key === 'Escape') { e.preventDefault(); close(); }
    else if (e.key === 'Enter' && e.target === input && !e.isComposing) { e.preventDefault(); select(e.shiftKey ? -1 : 1); }
  };
  function paint() {
    const current = matches[index]; clear(preview);
    if (current) preview.append(...current.parts.map(part => part.matched ? h('mark', { text: part.text }) : part.text));
    status.textContent = !input.value.trim() ? `Search all ${state.rows.length} turns in this conversation.` : matches.length ? `${index+1} of ${matches.length} matching turns · Enter / Shift+Enter to move` : 'No matching turns in this conversation.';
    previous.disabled = next.disabled = !matches.length;
    for (const [i, button] of [...timeline.children].entries()) button.setAttribute('aria-current', String(i === index));
  }
  function render() {
    if (!state?.rows) return;
    state.query = input.value; matches = conversationMatches(state.rows, input.value); index = matches.length ? 0 : -1;
    clear(timeline);
    matches.forEach((item, i) => timeline.append(h('button', { class: 'btn small', text: fmtClock(item.row.t_ms), 'aria-label': `Match ${i+1} at ${fmtClock(item.row.t_ms)}`, onclick: () => activate(i) })));
    paint();
  }
  function activate(value) {
    if (!matches[value]) return; index = value; state.selected = matches[index].row.id; paint();
    if (!reveal(matches[index].row.id)) {
      const transfer = resume = { ...state };
      void Promise.resolve(jump(matches[index].row)).finally(() => { if (resume === transfer) resume = null; });
    }
  }
  function select(direction) { activate(stepConversationMatch(index, direction, matches.length)); }
  async function open(thread, source, query = '') {
    trigger = source; deleted.clear(); corrected.clear(); state = { thread, query, rows: null }; input.value = query; panel.hidden = false;
    clear(preview); clear(timeline); retry.hidden = true; previous.disabled = next.disabled = true;
    status.textContent = 'Loading the complete conversation…'; input.focus();
    const ticket = ++generation;
    try {
      const result = await request('thread.get', { id: thread });
      if (dead || ticket !== generation) return;
      state.rows = latestRows(result.segments ?? []); render();
    } catch (error) {
      if (dead || ticket !== generation) return;
      status.textContent = `Could not load this conversation. ${error.message}`; retry.hidden = false;
    }
  }
  if (state) { panel.hidden = false; input.value = state.query; render(); const selected = matches.findIndex(item => item.row.id === state.selected); if(selected >= 0) { index = selected; paint(); } queueMicrotask(() => { if (!dead && panel.isConnected) input.focus({ preventScroll: true }); }); }
  return { panel, open, destroy() { dead = true; ++generation; }, update(change) {
    if (!state) return;
    const relevant = id => !state.rows || state.rows.some(row => row.id === id);
    for (const id of change?.purged ?? []) { if (relevant(id)) deleted.add(id); corrected.delete(id); }
    for (const row of change?.updated ?? []) if (relevant(row.id)) corrected.set(row.id, row);
    if (!state?.rows) return;
    const purged = new Set(change?.purged ?? []);
    const updated = new Map((change?.updated ?? []).map(row => [row.id,row]));
    if (!state.rows.some(row => purged.has(row.id) || updated.has(row.id))) return;
    state.rows = latestRows(state.rows); render();
  } };
}
