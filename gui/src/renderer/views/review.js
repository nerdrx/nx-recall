import { h, clear } from '../lib/dom.js';
import { ask } from '../lib/store.js';
import { play, stop, isActive, onPlayback } from '../lib/preview.js';
import { archiveAttribution } from './memory-archive.js';

export function reviewReasons(item) {
  const descriptions = { decoder_disagreement: 'The second decoder disagreed with these words.',
    language_mismatch: 'The detected language conflicted with the declared language.' };
  return (item.review_reasons ?? []).map(reason => descriptions[reason]).filter(Boolean);
}
export function correctionEvidence(summary) {
  const count = Number(summary?.corrections);
  const rate = summary?.edit_rate;
  if (!Number.isFinite(count) || count <= 0 || !Number.isFinite(rate)) return 'No scored word corrections yet.';
  return `${(rate * 100).toFixed(1)}% observed word-edit rate across ${count} corrected ${count === 1 ? 'segment' : 'segments'}.`;
}
export async function correctAndReview(item, text, rpc = ask) {
  if (!text.trim()) throw new Error('Enter the words you heard.');
  if (text !== item.text) {
    await rpc('segments.correct', { segment_id: item.id, text, expected_text: item.text });
    const { item: current } = await rpc('review.get', { segment_id: item.id });
    if (!current || current.text !== text) throw new Error('Correction saved, but the transcript changed again. Refresh before reviewing.');
    await rpc('review.mark', { segment_id: item.id, revision: current.review_revision });
  } else {
    await rpc('review.mark', { segment_id: item.id, revision: item.review_revision });
  }
}

export function mountReview(root, ctx) {
  let dead = false, active = false, generation = 0, evidenceGeneration = 0, next = null, loading = false;
  const cards = new Map();
  const button = (text, onclick, attrs = {}) => h('button', { class: 'btn small', onclick, ...attrs }, text);
  const status = h('p', { class: 'sub', role: 'status', 'aria-live': 'polite' });
  const evidence = h('p', { class: 'sub', text: 'Correction evidence is loading…' });
  const list = h('div', { class: 'review-list' });
  const refresh = button('Refresh inbox', () => load(false), { id: 'review-refresh' });
  const more = button('Next page', () => load(true), { id: 'review-more', hidden: true });
  root.classList.add('recognition-review');
  root.append(h('div', { class: 'review-heading' }, h('div', {}, h('h2', { text: 'Review words' }),
    h('p', { class: 'sub', text: 'Listen to uncertain words, correct them, or mark this version reviewed. Voice uncertainty is separate.' })), refresh),
    evidence, h('p', { class: 'sub', text: 'These are selected corrections—not a measure of accuracy on unseen audio. Disagreement is a reason to check, not proof of an error.' }), status, list, more);
  const offPlayback = onPlayback(() => {
    for (const { item, listen } of cards.values()) {
      const active = isActive(`review:${item.id}`);
      listen.textContent = active ? 'Stop listening' : 'Listen';
      listen.setAttribute('aria-pressed', String(active));
    }
  });
  function stopOwned() { for (const id of cards.keys()) if (isActive(`review:${id}`)) stop(); }
  function card(item) {
    const problem = h('p', { class: 'review-problem', role: 'alert' });
    const words = h('textarea', { class: 'input', id: `review-words-${item.id}`, rows: '3', text: item.text ?? '' });
    const listen = button('Listen', async () => {
      const key = `review:${item.id}`;
      if (isActive(key)) { stop(); return; }
      const result = await play(key, [item.id]);
      if (!dead && !result.played && !result.stopped) problem.textContent = 'Audio is unavailable. It may have expired or been removed.';
    }, { 'aria-pressed': 'false' });
    const mark = button('Mark reviewed', () => save(false));
    const correct = button('Save correction', () => save(true));
    const node = h('article', { class: 'card review-card', dataset: { reviewId: item.id } },
      h('p', { class: 'sub', text: archiveAttribution(item) }),
      ...reviewReasons(item).map(text => h('p', { class: 'review-reason', text })),
      h('label', { for: words.id, text: 'Transcript words' }), words,
      h('div', { class: 'review-actions' }, listen, correct, mark,
        button('Open transcript', () => ctx.jumpToSegment(item))), problem);
    async function save(edit) {
      const owner = cards.get(item.id);
      if (dead || owner?.node !== node || owner.stale) return;
      if (!edit && words.value !== item.text) { problem.textContent = 'Save your correction first, or restore the original words.'; return; }
      correct.disabled = mark.disabled = true; problem.textContent = '';
      try {
        await correctAndReview(item, edit ? words.value : item.text);
        if (dead || cards.get(item.id)?.node !== node) return;
        if (isActive(`review:${item.id}`)) stop();
        cards.delete(item.id); node.remove();
        status.textContent = cards.size ? 'Reviewed. The inbox will revisit changed words or new evidence.' : 'This page is reviewed. Check the next page or refresh for newer items.';
        refresh.focus();
        void loadEvidence();
      } catch (error) {
        if (dead || cards.get(item.id)?.node !== node) return;
        if (!owner.stale) { problem.textContent = error.message; correct.disabled = mark.disabled = false; }
      }
    }
    cards.set(item.id, { item, node, listen, correct, mark, problem, words, stale: false });
    return node;
  }
  async function loadEvidence() {
    if (dead || !active) return;
    const ticket = ++evidenceGeneration;
    try { const summary = await ask('accuracy.summary'); if (!dead && active && ticket === evidenceGeneration) evidence.textContent = correctionEvidence(summary); }
    catch { if (!dead && active && ticket === evidenceGeneration) evidence.textContent = 'Correction evidence is unavailable.'; }
  }
  async function load(advance) {
    if (loading || dead || !active) return;
    loading = true; const ticket = ++generation;
    refresh.disabled = more.disabled = true;
    status.textContent = 'Loading uncertain words…';
    try {
      const reply = await ask('review.list', { limit: 30, ...(advance && next ? { before_id: next } : {}) });
      if (dead || !active || ticket !== generation) return;
      stopOwned(); cards.clear(); clear(list);
      next = reply.next_before_id;
      for (const item of reply.items ?? []) list.append(card(item));
      more.hidden = next == null;
      status.textContent = cards.size ? `${cards.size} items on this page, newest first.` : 'No uncertain words to review here. Unchecked audio is not automatically error-free.';
      void loadEvidence();
    } catch (error) { if (!dead && ticket === generation) status.textContent = `Could not load the inbox: ${error.message}`; }
    finally { if (!dead && ticket === generation) { loading = false; refresh.disabled = more.disabled = false; } }
  }
  return {
    show() { if (dead || active) return; active = true; void load(false); },
    hide() { active = false; generation++; evidenceGeneration++; loading = false; stopOwned(); refresh.disabled = more.disabled = false; },
    update(change) {
      if (dead) return;
      if (change?.purged?.length || change?.updated?.length) {
        generation++; loading = false; refresh.disabled = more.disabled = false;
        status.textContent = 'The archive changed. Refresh to see the current review inbox.';
      }
      for (const id of change?.purged ?? []) {
        if (isActive(`review:${id}`)) stop();
        cards.get(id)?.node.remove(); cards.delete(id);
      }
      for (const item of change?.updated ?? []) {
        const current = cards.get(item.id);
        if (current) {
          current.stale = true;
          const draft = current.words.value !== current.item.text;
          if (!draft) current.words.value = item.text ?? '';
          current.correct.disabled = current.mark.disabled = true;
          current.problem.textContent = draft ? 'The source changed. Your draft is kept here; copy it before refreshing.' : 'This transcript changed. Refresh before reviewing it.';
        }
      }
    },
    destroy() { dead = true; active = false; generation++; evidenceGeneration++; stopOwned(); offPlayback(); cards.clear(); },
  };
}
