// Search — over the transcript, with speaker / source / date facets, in three
// modes: the words (FTS), the meaning (vectors), or both fused. The mode
// control and the per-hit `via` marker live in ./semantic.js.
// A hit is not a destination: clicking one loads the transcript around that
// moment and highlights it, because "what did she say about that world?" is
// answered by the conversation, not by the matching line on its own.

import { h, clear, fmtClock, fmtDay, fmtDate, speakerColor } from '../lib/dom.js';
import { store, speakerLabel, segmentSpeakerLabel, isUncertain, ask } from '../lib/store.js';
import { toast } from '../lib/sheets.js';
import { defaultMode, modeControl, requestFor, resultSummary, semanticState, viaBadge } from './semantic.js';

export const id = 'search';

// Facet state outlives the view so switching away and back keeps the query.
// Default window: the last week through today. Memory refresh is almost always
// "recently" — an open-ended range made every first search scan all of history.
function isoDay(d) {
  const y = d.getFullYear();
  const m = String(d.getMonth() + 1).padStart(2, '0');
  const day = String(d.getDate()).padStart(2, '0');
  return `${y}-${m}-${day}`;
}
const facetState = {
  q: '',
  speaker: '',
  source: '',
  from: isoDay(new Date(Date.now() - 7 * 86400e3)),
  to: isoDay(new Date()),
  // `null` until the first mount, then whichever mode the daemon can serve.
  // Sticky like the rest of the facets: a person who chose Keyword meant it.
  mode: null,
};
let lastHits = [];

export function mount(root, ctx) {
  const results = h('div', { id: 'search-results' });
  const resultCard = h('div', { class: 'card' }, results);
  const sub = h('span', { class: 'sub', id: 'search-sub', text: 'Everything captured, by word or by meaning.' });

  const sem = semanticState(store.status);
  if (facetState.mode == null) facetState.mode = defaultMode(sem);
  // A daemon that lost its model between visits must not leave the view stuck
  // in a mode it can no longer answer.
  if (!sem.available) facetState.mode = 'keyword';
  const modes = modeControl({
    selected: facetState.mode,
    available: sem.available,
    how: sem.how,
    onSelect: (id) => {
      facetState.mode = id;
      modes.paint(id);
      if (facetState.q) run();
    },
  });

  const qInput = h('input', {
    class: 'input',
    id: 'search-q',
    type: 'search',
    placeholder: 'e.g. portal world',
    value: facetState.q,
    onkeydown: (e) => {
      if (e.key === 'Enter') run();
    },
  });

  const speakerSel = h('select', { class: 'input', id: 'search-speaker' });
  const sourceSel = h('select', { class: 'input', id: 'search-source' });
  const fromInput = h('input', { class: 'input', id: 'search-from', type: 'date', value: facetState.from });
  const toInput = h('input', { class: 'input', id: 'search-to', type: 'date', value: facetState.to });

  function fillFacets() {
    clear(speakerSel);
    speakerSel.append(h('option', { value: '' }, 'Anyone'));
    for (const sp of [...store.speakers.values()].sort((a, b) => (b.total_ms ?? 0) - (a.total_ms ?? 0))) {
      speakerSel.append(h('option', { value: String(sp.id) }, speakerLabel(sp.id)));
    }
    speakerSel.value = facetState.speaker;

    clear(sourceSel);
    sourceSel.append(h('option', { value: '' }, 'Any source'));
    for (const s of store.sources) sourceSel.append(h('option', { value: s.match_key }, s.display ?? s.match_key));
    sourceSel.value = facetState.source;
  }

  const facets = h(
    'div',
    { class: 'facets' },
    h('div', { class: 'facet grow' }, h('label', { for: 'search-q', text: 'Query' }), qInput),
    h('div', { class: 'facet' }, h('label', { for: 'search-speaker', text: 'Speaker' }), speakerSel),
    h('div', { class: 'facet' }, h('label', { for: 'search-source', text: 'Source' }), sourceSel),
    h('div', { class: 'facet' }, h('label', { for: 'search-from', text: 'From' }), fromInput),
    h('div', { class: 'facet' }, h('label', { for: 'search-to', text: 'To' }), toInput),
    h('div', { class: 'facet' }, h('label', { text: ' ' }), h('button', { class: 'btn primary', id: 'search-go', onclick: () => run() }, 'Search'))
  );

  const body = h('div', { class: 'view-body view-enter' }, h('div', { class: 'card' }, facets, modes.el), resultCard);
  root.append(
    h('div', { class: 'view-head' }, h('div', {}, h('h1', { text: 'Search' }), sub), h('div', { class: 'spacer' })),
    body
  );

  // -- run ------------------------------------------------------------------

  async function run() {
    facetState.q = qInput.value;
    facetState.speaker = speakerSel.value;
    facetState.source = sourceSel.value;
    facetState.from = fromInput.value;
    facetState.to = toInput.value;

    const params = { q: facetState.q, limit: 100 };
    if (facetState.speaker) params.speaker = Number(facetState.speaker);
    if (facetState.source) params.source = facetState.source;
    if (facetState.from) params.from = `${facetState.from}T00:00:00Z`;
    if (facetState.to) params.to = `${facetState.to}T23:59:59Z`;

    try {
      const [method, p] = requestFor(facetState.mode, params);
      const res = await ask(method, p);
      lastHits = res.hits ?? [];
      renderHits(res);
    } catch (e) {
      // The one error worth demoting a mode over: the daemon lost (or never
      // had) the model. Fall back rather than showing the user a red box for
      // a search that keyword can still answer.
      if (e.code === 'unavailable' && facetState.mode !== 'keyword') {
        facetState.mode = 'keyword';
        modes.paint('keyword');
        toast('Smart search needs a model this daemon does not have. Searching by words.', '');
        return run();
      }
      clear(results);
      results.append(
        h('div', { class: 'empty' }, h('b', { text: 'Search failed' }), h('p', { text: `${e.message}. The daemon may be restarting — try again in a moment.` }))
      );
    }
  }

  function renderHits(res) {
    clear(results);
    sub.textContent = resultSummary(facetState.mode, res, facetState.q);
    if (!lastHits.length) {
      results.append(
        h(
          'div',
          { class: 'empty' },
          h('b', { text: 'Nothing matched' }),
          h('p', {
            text:
              facetState.mode === 'keyword'
                ? 'Try fewer words, or Smart search if you cannot remember them — search only covers what has been captured on allowed sources.'
                : 'Try describing it differently, or widen the speaker and date facets — search only covers what has been captured on allowed sources.',
          })
        )
      );
      return;
    }
    for (const seg of lastHits) results.append(hitRow(seg));
  }

  function hitRow(seg) {
    const color = speakerColor(seg.speaker);
    const row = h('div', {
      class: `seg${isUncertain(seg) ? ' uncertain' : ''}`,
      dataset: { hit: String(seg.id) },
      role: 'button',
      tabindex: '0',
      title: 'Open this moment in the transcript',
    });
    row.append(
      h('span', { class: 't', text: `${fmtDay(seg.t_ms).slice(5)} ${fmtClock(seg.t_ms).slice(0, 5)}` }),
      h(
        'span',
        { class: 'who', ...(seg.speaker != null ? { dataset: { sp: String(seg.speaker) } } : {}) },
        h('span', { class: 'dot', style: `color:${color}` }),
        // Same vocabulary as the transcript: a hit with no voice says which
        // kind of nameless it is, not just that a field is empty.
        h('span', {
          class: `nm${seg.speaker == null ? ' reasoned' : ''}`,
          text: segmentSpeakerLabel(seg),
          style: seg.speaker == null ? '' : `color:${color}`,
        })
      ),
      h('span', { class: 'txt' }, ...highlight(seg.text ?? '', facetState.q)),
      h(
        'span',
        { class: 'meta' },
        // Only in Both: in the single-leg modes every row arrived the same way
        // and a badge on all of them says nothing.
        facetState.mode === 'both' ? viaBadge(seg.via) : null,
        h('span', { class: 'chip', text: seg.source ?? 'unknown' })
      )
    );
    const jump = () => ctx.jumpToSegment(seg);
    row.addEventListener('click', jump);
    row.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') jump();
    });
    return row;
  }

  // Marks the query inside the hit without ever building HTML from daemon text.
  function highlight(text, q) {
    const needle = (q ?? '').trim().toLowerCase();
    if (!needle) return [text];
    const out = [];
    let i = 0;
    const hay = text.toLowerCase();
    for (;;) {
      const at = hay.indexOf(needle, i);
      if (at < 0) break;
      if (at > i) out.push(text.slice(i, at));
      // On the dark ground the mark was bold-and-brighter; on a light one that
      // vanishes, so it carries the accent wash a chip uses (styles.css .hl).
      out.push(h('b', { class: 'hl', text: text.slice(at, at + needle.length) }));
      i = at + needle.length;
    }
    out.push(text.slice(i));
    return out;
  }

  // Re-run mount() in place: the mode control's availability is baked into
  // its buttons, and rebuilding is cheaper to reason about than mutating them.
  function remount() {
    clear(root);
    return mount(root, ctx);
  }

  fillFacets();
  if (facetState.q || facetState.speaker || facetState.source) run();
  else
    results.append(
      h(
        'div',
        { class: 'empty' },
        h('b', { text: 'Search everything you have said and heard' }),
        h('p', { text: 'Type a few words, or narrow by speaker, source and date. Results open in the transcript where they were said.' })
      )
    );

  return {
    update(change) {
      if (change?.relabel || change?.merged) fillFacets();
      if (change?.sources) fillFacets();
      // The model can arrive while the app is open — a fetch and a daemon
      // restart — so the toggle follows `status` rather than the first paint.
      if (change?.status) {
        const now = semanticState(store.status);
        if (now.available !== sem.available) return remount();
      }
    },
    focusQuery() {
      qInput.focus();
      qInput.select();
    },
  };
}

export function lastSearchDate() {
  return facetState.from ? fmtDate(`${facetState.from}T00:00:00Z`) : null;
}

export function noteJump() {
  toast('Opened in the transcript.', '');
}
