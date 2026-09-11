import { searchableSpeakerSelect } from '../lib/searchable-select.js';
// Search — over the transcript, with speaker / source / date facets, in three
// modes: the words (FTS), the meaning (vectors), or both fused. The mode
// control and the per-hit `via` marker live in ./semantic.js.
// Open a hit beside the results, with the adjacent turns and a transcript
// link. The conversation supplies context without losing the result list.

import { h, clear, fmtClock, fmtDay, fmtDayLabel, fmtDate } from '../lib/dom.js';
import { store, speakerLabel, segmentSpeakerLabel, isUncertain, isShaky, ask } from '../lib/store.js';
import { moodChips, shakyMark, translationCell } from '../lib/marks.js';
// 0.12.0 — per-person highlights. The same three helpers the transcript uses,
// because a hit and the row it takes you to must not disagree about a colour.
import { look, lookOf, iconSpan, markRow } from './highlight.js';
import { openSheet, toast } from '../lib/sheets.js';
import { defaultMode, modeControl, modeById, requestFor, resultSummary, semanticState, viaBadge } from './semantic.js';

import { saveMomentSheet } from '../lib/saved.js';

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
  date: { kind: 'rolling', days: 7 },
  q: '',
  speaker: '',
  source: '',
  ...resolveSearchDate({ kind: 'rolling', days: 7 }),
  // `null` until the first mount, then whichever mode the daemon can serve.
  // Sticky like the rest of the facets: a person who chose Keyword meant it.
  mode: null,
  // 0.8.0. The manual facets did not go away — they went BEHIND something. The
  // front door is one box you ask a question in; this is whether the drawer of
  // dropdowns under it is open, and it is sticky because a person who opened it
  // is working that way for the afternoon.
  advanced: false,
  // The last `search.ask` interpretation, or null when the query that produced
  // the results on screen was an explicit one. It is what the pills are drawn
  // from, and clearing a pill edits it and re-runs.
  asked: null,
  // 0.10.0. A world facet set by HAND — from a world chip on a person page or
  // the Memory card — as opposed to one the daemon read out of a question,
  // which lives on `asked`. Sticky like the rest of the facets.
  world: '',
  worldLabel: '',
  // 0.11.0. The last `search.answer` reply's `answer`, or its `refused`, or
  // null when the query that produced what is on screen was not a question.
  // Deliberately NOT sticky across mounts in any meaningful way: an answer is a
  // reading of the archive at the moment it was asked, and one left on screen
  // after the question scrolled away is a sentence with nothing under it.
  answer: null,
  refused: null,
};
let lastHits = [];
// Which search is current. Every path that talks to the daemon takes a ticket
// on the way in and checks it on the way out: a reply that arrives after a
// newer search started paints nothing. Without this, "flickering" asked and
// then "portal" searched left the pills for "flickering" on screen — the
// explicit search cleared them and the older reply put them back (0.11.8, seen
// only under load, which is exactly when replies arrive out of order).
let generation = 0;
const ticket = () => ++generation;
const stale = (my) => my !== generation;

export function mount(root, ctx, arg) {
  // Arriving from a world chip: the facet is the whole point of the trip, so
  // it is set before anything renders and the search runs with it on.
  //
  // Deliberately NOT sticky, unlike the speaker and date facets. "Filtered to
  // The Great Pug" is a trip you took, not a preference you set: leaving
  // Search and coming back later to a filter you no longer remember switching
  // on is how a search box starts lying to you. `remount()` passes `arg`
  // through, so a mode change inside the trip keeps it.
  facetState.world = arg?.world ? String(arg.world) : '';
  facetState.worldLabel = arg?.world ? (arg.worldLabel ?? '') : '';
  if (arg?.world) facetState.asked = null;
  if (arg?.savedSearch) Object.assign(facetState, restoreSavedSearch(arg.savedSearch));
  if (facetState.date.kind === 'rolling') Object.assign(facetState, resolveSearchDate(facetState.date));
  const results = h('div', { id: 'search-results', role: 'region', 'aria-label': 'Search results', 'aria-busy': 'false' });
  const resultCount = h('h2', { id: 'search-result-count', text: 'Your results', 'aria-live': 'polite', 'aria-atomic': 'true' });
  const resultStatus = h('span', { class: 'sub', id: 'search-result-status', role: 'status', 'aria-live': 'polite' });
  const resultCard = h('section', { class: 'card search-result-card', 'aria-label': 'Matching moments' },
    h('div', { class: 'search-results-head' }, resultCount, resultStatus), results);
  const context = h('aside', { class: 'card search-context', id: 'search-context', hidden: true, 'aria-label': 'Conversation context' });
  let contextGeneration = 0;
  let contextIds = [];
  let disposed = false;
  let pending = false;
  let focusedResult = 0;
  let retrySearch = () => run();
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
      // Whichever path put the results on screen is the one that re-runs: a
      // mode change must not silently throw away an interpretation.
      if (!facetState.q) return;
      if (facetState.asked) {
        facetState.asked.mode = id === 'both' ? 'hybrid' : id === 'smart' ? 'semantic' : 'keyword';
        renderPills();
        void rerunAsked();
      } else {
        void run();
      }
    },
  });

  // The front door (0.8.0). It used to be a field called Query with four
  // dropdowns beside it, which meant the first thing the app asked a person who
  // wanted to remember something was to decompose their question into facets.
  // Now they type the question and press Enter, the daemon says what it
  // understood, and the pills below are where they disagree with it.
  const qInput = h('input', {
    class: 'input',
    id: 'search-q',
    type: 'search',
    placeholder: 'Words, a person, or a question…',
    'aria-describedby': 'search-keyboard-hint',
    value: facetState.q,
    onkeydown: (e) => {
      if (e.key === 'Enter') runAsk();
      if (e.key === 'ArrowDown' && !pending && lastHits.length) { e.preventDefault(); focusResult(0); }
    },
  });

  const speakerSel = h('select', { class: 'input', id: 'search-speaker' });
  const speakerPicker = searchableSpeakerSelect(speakerSel, { label: 'Find a speaker for search results' });
  const sourceSel = h('select', { class: 'input', id: 'search-source' });
  const fromInput = h('input', { class: 'input', id: 'search-from', type: 'date', value: facetState.from });
  const toInput = h('input', { class: 'input', id: 'search-to', type: 'date', value: facetState.to });

  function fillFacets() {
    clear(speakerSel);
    speakerSel.append(h('option', { value: '' }, 'Anyone'));
    for (const sp of [...store.speakers.values()].sort((a, b) => (b.total_ms ?? 0) - (a.total_ms ?? 0))) {
      speakerSel.append(h('option', { value: String(sp.id), dataset: { speakerAuto: sp.auto ?? '' } }, speakerLabel(sp.id)));
    }
    speakerSel.value = facetState.speaker;
    speakerPicker.refresh();

    clear(sourceSel);
    sourceSel.append(h('option', { value: '' }, 'Any source'));
    for (const s of store.sources) sourceSel.append(h('option', { value: s.match_key }, s.display ?? s.match_key));
    sourceSel.value = facetState.source;
  }

  // What the daemon understood, as pills you can take off. It sits between the
  // box and the results because that is where the answer to "why these
  // results?" belongs — after the question, before the answer.
  const pills = h('div', { class: 'ask-pills', id: 'ask-pills', hidden: true });

  const advancedBtn = h(
    'button',
    {
      class: 'btn',
      id: 'search-advanced',
      'aria-expanded': String(facetState.advanced),
      'aria-controls': 'search-facets',
      title: 'Choose a speaker or source',
      onclick: () => {
        facetState.advanced = !facetState.advanced;
        paintAdvanced();
      },
    },
    'Filters'
  );

  // The front door: one box, one button, and a drawer.
  const askRow = h(
    'div',
    { class: 'facets ask-row' },
    h('div', { class: 'facet grow' }, h('label', { for: 'search-q', text: 'Search your conversations' }), qInput),
    h('button', { class: 'btn primary', id: 'search-ask', onclick: () => runAsk() }, 'Ask'),
    advancedBtn,
    h('button', { class: 'btn', id: 'search-save', onclick: () => saveSearch() }, 'Save search')
  );

  function paintAdvanced() {
    facets.hidden = !facetState.advanced;
    advancedBtn.setAttribute('aria-expanded', String(facetState.advanced));
  }

  // Unchanged, and deliberately: the manual facets are still the whole
  // vocabulary of the old view, still sticky, still exactly where somebody who
  // learned them left them. They are merely no longer the first thing anybody
  // has to read.
  const facets = h(
    'div',
    { class: 'facets', id: 'search-facets', hidden: !facetState.advanced },
    h('div', { class: 'facet' }, h('label', { for: 'search-speaker', text: 'Speaker' }), speakerPicker.element),
    h('div', { class: 'facet' }, h('label', { for: 'search-source', text: 'Source' }), sourceSel),
    h('div', { class: 'facet' }, h('label', { text: ' ' }), h('button', { class: 'btn', id: 'search-go', onclick: () => run() }, 'Apply filters'))
  );

  const scopeLabel = h('span', { class: 'sub', id: 'search-scope-label', 'aria-live': 'polite' });
  const allHistory = () => {
    facetState.date = { kind: 'all' };
    facetState.from = fromInput.value = '';
    facetState.to = toInput.value = '';
    if (facetState.asked) {
      facetState.asked = { ...withoutSearchDates(facetState.asked), date_explicit: true };
      void rerunAsked();
    } else void run();
    paintScope();
  };
  function paintScope() {
    const it = facetState.asked;
    scopeLabel.textContent = it?.from_ns || it?.to_ns ? `Searching ${timeLabel(it)}`
      : facetState.from || facetState.to ? `Searching ${facetState.from || 'the beginning'} → ${facetState.to || 'today'}` : 'Searching all history';
  }
  for (const input of [fromInput, toInput]) input.addEventListener('change', () => {
    facetState.from = fromInput.value;
    facetState.to = toInput.value;
    facetState.date = { kind: 'fixed', from: facetState.from, to: facetState.to };
    if (facetState.asked) {
      facetState.asked = { ...withoutSearchDates(facetState.asked), date_explicit: true };
      const bounds = searchDateParams(facetState);
      if (bounds.from) facetState.asked.from_ns = String(BigInt(Date.parse(bounds.from)) * 1000000n);
      if (bounds.to) facetState.asked.to_ns = String(BigInt(Date.parse(bounds.to)) * 1000000n);
      void rerunAsked();
    } else void run();
    paintScope();
  });
  const dateRow = h('div', { class: 'facets search-date-scope', id: 'search-date-scope' },
    h('div', { class: 'facet' }, h('label', { for: 'search-from', text: 'From' }), fromInput),
    h('div', { class: 'facet' }, h('label', { for: 'search-to', text: 'To' }), toInput),
    h('button', { class: 'btn', id: 'search-all-history', onclick: allHistory }, 'Search all history'),
    h('button', { class: 'btn', onclick: () => {
      facetState.date = { kind: 'rolling', days: 7 };
      Object.assign(facetState, resolveSearchDate(facetState.date));
      fromInput.value = facetState.from; toInput.value = facetState.to;
      if (facetState.asked) {
        facetState.asked = { ...withoutSearchDates(facetState.asked), date_explicit: false };
        void rerunAsked();
      } else void run();
      paintScope();
    } }, 'Last 7 days'), scopeLabel);
  const saveSearch = () => openSheet((close) => {
    const name = h('input', { class: 'input', id: 'saved-search-name', value: qInput.value.trim().slice(0, 120), maxlength: 120 });
    const save = h('button', { class: 'btn primary', onclick: async () => {
      if (!name.value.trim()) { name.focus(); return; }
      save.disabled = true;
      try {
        const snapshot = { ...facetState, q: qInput.value, speaker: speakerSel.value, source: sourceSel.value,
          from: fromInput.value, to: toInput.value, asked: qInput.value === facetState.q ? facetState.asked : null };
        await ask('saved.searches.save', { name: name.value.trim(), query: snapshot.q, filters: savedSearchFilters(snapshot) });
        close(); toast('Search saved in Memory.', '');
      } catch (e) { toast(e.message, 'error'); save.disabled = false; }
    } }, 'Save search');
    return [h('h2', { text: 'Save this search' }), h('p', { class: 'sub', text: 'Keep this query and its filters in Memory. Last 7 days stays relative; chosen dates stay fixed.' }),
      h('label', { for: 'saved-search-name', text: 'Name' }), name,
      h('div', { class: 'actions' }, h('button', { class: 'btn', onclick: () => close() }, 'Cancel'), save)];
  });
  const contextEmpty = h('aside', { class: 'search-context-empty', 'aria-label': 'Conversation reader' },
    h('span', { class: 'search-reader-label', text: 'CONVERSATION' }),
    h('h2', { text: 'Read the conversation.' }),
    h('p', { text: 'Choose a result to read the turns around it. Your query and results stay here.' }),
    h('p', { class: 'sub', id: 'search-keyboard-hint', text: '↑ ↓ move through results · Enter opens context' }));
  const workspace = h('div', { class: 'search-workspace' }, resultCard, context, contextEmpty);
  const body = h('div', { class: 'view-body view-enter search014' },
    h('section', { class: 'card search-toolbar', 'aria-label': 'Search and filters' }, askRow,
      h('div', { class: 'search-refine' }, dateRow, modes.el), pills, facets),
    h('div', { class: 'search-summary' }, sub), workspace);
  root.append(
    h('div', { class: 'view-head' }, h('div', {}, h('h1', { text: 'Search' }),
      h('span', { class: 'sub', text: 'Find the words. Stay with the conversation.' }))), body
  );

  function hideContext() {
    context.hidden = true; contextGeneration++;
    contextIds = []; clear(context);
    contextEmpty.hidden = false;
    workspace.classList.remove('context-open');
    for (const row of results.querySelectorAll('.seg')) { row.classList.remove('context-selected'); row.removeAttribute('aria-current'); }
  }
  function beginSearch(retry) {
    hideContext(); pending = true; retrySearch = retry;
    results.setAttribute('aria-busy', 'true');
    resultStatus.textContent = 'Searching…';
    resultCount.textContent = 'Finding matches';
    clear(results);
    results.append(h('div', { class: 'search-loading', text: 'Looking through your captured conversations…' }));
  }
  function endSearch() {
    pending = false; results.setAttribute('aria-busy', 'false');
    resultStatus.textContent = '';
  }
  function searchError(title, message) {
    endSearch(); lastHits = []; clear(results);
    resultCount.textContent = 'Search unavailable';
    sub.textContent = 'Your query and filters are ready to try again.';
    results.append(h('div', { class: 'empty search-feedback', role: 'alert' },
      h('b', { text: title }), h('p', { text: message }),
      h('button', { class: 'btn', onclick: () => retrySearch() }, 'Try again')));
  }
  function focusResult(index) {
    const rows = [...results.querySelectorAll('.seg')];
    if (pending || !rows.length) return;
    focusedResult = Math.max(0, Math.min(rows.length - 1, index));
    rows.forEach((row, at) => { row.tabIndex = at === focusedResult ? 0 : -1; });
    rows[focusedResult].focus({ preventScroll: true });
    rows[focusedResult].scrollIntoView({ block: 'nearest' });
  }

  // -- run ------------------------------------------------------------------

  /**
   * The question path (0.8.0). One request: the daemon parses the sentence,
   * runs the search with what it found, and hands back BOTH — so this never
   * has to guess at what it understood, and never disagrees with it.
   *
   * A daemon too old to know the method is not an error worth a red box: the
   * words are still a perfectly good keyword query, so it falls through to the
   * explicit path and says so once.
   */
  function acceptInterpretation(interpretation) {
    facetState.asked = interpretation ?? null;
    if (interpretation?.date_explicit) {
      facetState.from = fromInput.value = interpretation.from_ns ? isoDay(new Date(nsToMs(interpretation.from_ns))) : '';
      facetState.to = toInput.value = interpretation.to_ns ? isoDay(new Date(nsToMs(interpretation.to_ns) - 1)) : '';
      facetState.date = { kind: 'fixed', from: facetState.from, to: facetState.to };
    }
  }

  function askParams() {
    facetState.speaker = speakerSel.value; facetState.source = sourceSel.value;
    facetState.from = fromInput.value; facetState.to = toInput.value;
    return { q: facetState.q, limit: 100, ...searchDateParams(facetState),
      ...(facetState.speaker ? { speaker: Number(facetState.speaker) } : {}),
      ...(facetState.source ? { source: facetState.source } : {}),
      ...(facetState.world ? { world: facetState.world } : {}) };
  }

  async function runAsk() {
    beginSearch(() => runAsk());
    const my = ticket();
    facetState.q = qInput.value;
    facetState.answer = null;
    facetState.refused = null;
    if (!facetState.q.trim()) {
      facetState.asked = null;
      renderPills();
      return run();
    }
    // 0.11.0. A question gets an answer; a keyword query gets what it always
    // got. The reading is the daemon's own (`interpretation.is_question`), but
    // the DECISION of which method to call has to happen before the round
    // trip, so it is made here with the same two rules and then confirmed:
    // `search.answer` returns everything `search.ask` does, so a query this
    // guessed wrong about costs a model call it did not need and nothing else.
    const question = looksLikeAQuestion(facetState.q);
    try {
      const res = await ask(question ? 'search.answer' : 'search.ask', askParams());
      if (stale(my)) return undefined;
      acceptInterpretation(res.interpretation);
      facetState.answer = res.answer ?? null;
      facetState.refused = res.refused ?? null;
      // The mode the daemon chose is the mode the toggle now shows: the control
      // must never claim one thing while the results came from another.
      if (facetState.asked?.mode) {
        facetState.mode = modeIdOf(facetState.asked.mode);
        modes.paint(facetState.mode);
      }
      lastHits = res.hits ?? [];
      renderPills();
      renderHits(res);
    } catch (e) {
      if (stale(my)) return undefined;
      if (e.code === 'unknown_method') {
        // A daemon too old for 0.11.0 can still read the question — try the
        // one method back before giving up on the whole path. Only then does
        // this fall through to a plain keyword search.
        if (question) {
          try {
            const res = await ask('search.ask', askParams());
            if (stale(my)) return undefined;
            acceptInterpretation(res.interpretation);
            lastHits = res.hits ?? [];
            renderPills();
            renderHits(res);
            return undefined;
          } catch (again) {
            if (stale(my)) return undefined;
            if (again.code !== 'unknown_method') { searchError('That question could not be asked', again.message); return undefined; }
          }
        }
        facetState.asked = null;
        renderPills();
        toast('This daemon cannot read a question yet — searching for those words instead.', '');
        return run();
      }
      facetState.asked = null;
      facetState.answer = null;
      facetState.refused = null;
      renderPills();
      searchError('That question could not be asked', e.message);
    }
    return undefined;
  }

  /**
   * Re-run with the interpretation as it now stands — what taking a pill off
   * does. Deliberately the EXPLICIT call rather than `search.ask` again: the
   * whole point of removing a facet is that the daemon's reading of the
   * sentence was wrong, so asking it to read the same sentence again would put
   * the facet straight back.
   */
  async function rerunAsked() {
    beginSearch(() => rerunAsked());
    const my = ticket();
    const it = facetState.asked;
    // Taking a pill off changes which turns were searched, so whatever
    // sentence was above them was read off a different set of rows. It goes.
    facetState.answer = null;
    facetState.refused = null;
    if (!it) return run();
    const params = { q: it.query ?? '', limit: 100, ...(!it.from_ns && !it.to_ns && it.date_explicit === false ? searchDateParams(facetState) : {}) };
    if (it.speaker_id != null) params.speaker = it.speaker_id;
    if (it.world_id) params.world = it.world_id;
    if (it.source) params.source = it.source;
    if (it.from_ns) params.from = new Date(nsToMs(it.from_ns)).toISOString();
    if (it.to_ns) params.to = new Date(nsToMs(it.to_ns)).toISOString();
    // An empty query with facets is a browse, and only the keyword leg can
    // answer one — `search.semantic` has nothing to embed.
    const modeId = params.q.trim() ? modeIdOf(it.mode) : 'keyword';
    try {
      const [method, p] = requestFor(modeId, params);
      const response = await ask(params.q.trim() ? method : 'transcript', p);
      const res = params.q.trim() ? response : { hits: [...(response.segments ?? [])].reverse(), total: response.segments?.length ?? 0 };
      if (stale(my)) return undefined;
      lastHits = res.hits ?? [];
      renderPills();
      renderHits(res, modeId);
    } catch (e) {
      if (stale(my)) return undefined;
      searchError('Search failed', e.message);
    }
    return undefined;
  }

  // -- the interpretation, as pills ------------------------------------------

  function nsToMs(ns) {
    return Math.round(Number(ns) / 1e6);
  }

  /** The wire's mode names, in this view's vocabulary. */
  function modeIdOf(mode) {
    return mode === 'hybrid' ? 'both' : mode === 'semantic' ? 'smart' : 'keyword';
  }

  /**
   * What to call the time facet. The contract carries instants, not the phrase
   * that produced them (PROTOCOL) — which is the right call, because "yesterday"
   * stops being true at midnight and an instant does not. So the label is
   * derived here: a range that is exactly one day gets that day's name, and
   * anything else says its ends.
   */
  function timeLabel(it) {
    const from = it.from_ns ? nsToMs(it.from_ns) : null;
    const to = it.to_ns ? nsToMs(it.to_ns) : null;
    if (from != null && to != null) {
      const oneDay = Math.abs(to - from - 86_400_000) < 60_000;
      if (oneDay) return fmtDayLabel(from).split(' · ')[0].toLowerCase();
      return `${fmtDay(from)} → ${fmtDay(to - 1)}`;
    }
    if (from != null) return `since ${fmtDay(from)}`;
    return `until ${fmtDay(to)}`;
  }

  /** What to call a world when only its id is known. */
  function worldLabel(id, label) {
    if (label) return label;
    return String(id ?? '').startsWith('wrld_') ? `${String(id).slice(0, 13)}…` : String(id ?? '');
  }

  function renderPills() {
    paintScope();
    clear(pills);
    const it = facetState.asked;
    // The world pill shows on BOTH paths: a facet the daemon read out of a
    // question, and one a world chip set by hand. Without the second, arriving
    // from "Where you meet" would filter the results with nothing on screen
    // saying so — a filter you cannot see is a filter you cannot take off.
    const handWorld = !it && facetState.world;
    pills.hidden = !it && !handWorld;
    if (!it && !handWorld) return;

    // `icon` is a separate child rather than part of `label` on purpose:
    // `.ask-pill-text` is READ as the name — the e2e compares it against the
    // `.nm` on every hit to prove the pill still describes the result set — so
    // folding an emoji into it would make the pill and the rows disagree about
    // who this is. Same shape as `.who`, where the icon is a sibling of `.nm`
    // and never inside it.
    const pill = (key, label, title, drop, icon) =>
      h(
        'span',
        { class: 'ask-pill', dataset: { facet: key }, title },
        iconSpan(icon),
        h('span', { class: 'ask-pill-text', text: label }),
        h(
          'button',
          {
            class: 'ask-pill-x',
            dataset: { drop: key },
            'aria-label': `Search again without ${label}`,
            title: `Search again without ${label}`,
            onclick: () => {
              drop();
              void rerunAsked();
            },
          },
          '✕'
        )
      );

    pills.append(
      h('span', {
        class: 'ask-pills-label',
        text: handWorld ? 'filtered to' : 'understood as',
      })
    );
    if (handWorld) {
      pills.append(
        pill(
          'world',
          `in ${worldLabel(facetState.world, facetState.worldLabel)}`,
          'Only what was said in this world — press ✕ to search everywhere',
          () => {
            facetState.world = '';
            facetState.worldLabel = '';
          }
        )
      );
      return;
    }
    if (it.query) {
      // Not removable: with the words gone there is no question left, only
      // facets — and the box above is where you change the words.
      pills.append(
        h('span', { class: 'ask-pill fixed', dataset: { facet: 'query' }, title: 'The words that were searched for' },
          h('span', { class: 'ask-pill-text', text: `“${it.query}”` }))
      );
    }
    if (it.speaker_id != null) {
      // The pill is the one place on this page that names the voice the whole
      // result set is about, so the icon goes in FRONT of the label rather than
      // beside it — it has to read as part of the person's name, the same way
      // it does on every row below. No colour: the pill is a facet chip with
      // its own ✕ and its own hairline, and tinting it would make a filter look
      // like a warning.
      const pillIcon = lookOf(it.speaker_id).icon;
      const pillLabel = it.speaker_label || speakerLabel(it.speaker_id);
      pills.append(
        pill(
          'speaker',
          pillLabel,
          'Only this voice — press ✕ to search everybody',
          () => {
            delete facetState.asked.speaker_id;
            delete facetState.asked.speaker_label;
          },
          pillIcon
        )
      );
    }
    if (it.source) pills.append(pill('source', it.source, 'Only this source — remove to search every source', () => {
      delete facetState.asked.source;
      facetState.source = sourceSel.value = '';
    }));
    if (it.world_id) {
      pills.append(
        pill(
          'world',
          `in ${worldLabel(it.world_id, it.world_label)}`,
          'Only what was said in this world — press ✕ to search everywhere',
          () => {
            delete facetState.asked.world_id;
            delete facetState.asked.world_label;
          }
        )
      );
    }
    if (it.from_ns || it.to_ns) {
      pills.append(
        pill('time', timeLabel(it), 'Only this stretch of time — press ✕ to search all of it', () => {
          delete facetState.asked.from_ns;
          delete facetState.asked.to_ns;
          delete facetState.asked.from_ms; delete facetState.asked.to_ms;
          facetState.asked.date_explicit = true;
          facetState.date = { kind: 'all' };
          facetState.from = fromInput.value = ''; facetState.to = toInput.value = '';
        })
      );
    }
    pills.append(
      pill('mode', modeById(modeIdOf(it.mode)).label, 'How it searched — press ✕ to fall back to the words alone', () => {
        facetState.asked.mode = 'keyword';
        facetState.mode = 'keyword';
        modes.paint('keyword');
      })
    );
  }

  async function run() {
    beginSearch(() => run());
    const my = ticket();
    // An explicit search is a different question from the one that was asked,
    // so the pills go: leaving them up would explain results they did not
    // produce, which is worse than explaining nothing. The answer goes with
    // them, and for the stronger version of the same reason.
    facetState.asked = null;
    facetState.answer = null;
    facetState.refused = null;
    renderPills();
    facetState.q = qInput.value;
    facetState.speaker = speakerSel.value;
    facetState.source = sourceSel.value;
    facetState.from = fromInput.value;
    facetState.to = toInput.value;

    // Nothing left to ask. Taking the last facet off a wordless browse is the
    // way here, and an empty query is a `params` error on the daemon — so this
    // says "ask me something" rather than showing a red box for a question
    // nobody asked.
    if (!hasSearchIntent(facetState, { saved: !!arg?.savedSearch })) {
      endSearch(); resultCount.textContent = 'Your results';
      lastHits = [];
      clear(results);
      sub.textContent = 'Everything captured, by word or by meaning.';
      results.append(
        h(
          'div',
          { class: 'empty' },
          h('b', { text: 'Search everything you have said and heard' }),
          h('p', {
            text: 'Type a few words, or narrow by speaker, source and date. Open a result to read the surrounding conversation.',
          })
        )
      );
      return;
    }

    const params = { q: facetState.q, limit: 100 };
    if (facetState.speaker) params.speaker = Number(facetState.speaker);
    if (facetState.source) params.source = facetState.source;
    Object.assign(params, searchDateParams(facetState));
    if (facetState.world) params.world = facetState.world;

    try {
      // A world with no words is a BROWSE, not a search for nothing: arriving
      // from a world chip, the facet is the whole question and there is no
      // query to rank by. `transcript` takes the same facet and answers it.
      if (!facetState.q.trim()) {
        const res = await ask('transcript', params);
        if (stale(my)) return;
        lastHits = [...(res.segments ?? [])].reverse();
        renderHits({ total: lastHits.length, hits: lastHits }, 'keyword');
        return;
      }
      const [method, p] = requestFor(facetState.mode, params);
      const res = await ask(method, p);
      if (stale(my)) return;
      lastHits = res.hits ?? [];
      renderHits(res);
    } catch (e) {
      if (stale(my)) return;
      // The one error worth demoting a mode over: the daemon lost (or never
      // had) the model. Fall back rather than showing the user a red box for
      // a search that keyword can still answer.
      if (e.code === 'unavailable' && facetState.mode !== 'keyword') {
        facetState.mode = 'keyword';
        modes.paint('keyword');
        toast('Smart search needs a model this daemon does not have. Searching by words.', '');
        return run();
      }
      searchError('Search failed', e.message);
    }
  }

  // -- 0.11.0: the answer card ------------------------------------------------

  /**
   * Was this typed as a question? Two readings, and a person means either: it
   * ends in a question mark, or it opens with an interrogative. The daemon has
   * the same two rules and reports its own reading as
   * `interpretation.is_question` — but the call has to be chosen BEFORE the
   * round trip, so this is the copy that picks the method. Deliberately not
   * "contains an interrogative anywhere": "the world where we met" is a phrase
   * somebody is searching for, and answering it in a sentence would be the app
   * talking over them.
   */
  const INTERROGATIVES =
    /^(was|wer|wen|wem|wessen|wann|wo|wohin|woher|wie|warum|wieso|weshalb|wor(?:ü|ue)ber|worum|wovon|welche[rsn]?|what|who|whom|whose|when|where|why|how|which)(?![\w'’])/i;

  /**
   * The Japanese half (0.11.x). Japanese does not front its interrogatives —
   * 誰 and 何 sit wherever the clause puts them — so the rule above finds
   * nothing in a Japanese question. What is positional is the final particle,
   * and this is the daemon's list of them (`ask.rs`, `JA_QUESTION_ENDINGS`),
   * longest first so `ですか` is not read as the bare `か` it ends with. The
   * script test is the guard: it is what keeps `no` and `kana` from being
   * questions.
   */
  const JA_ENDING = /(?:でしょうか|ですか|ますか|かな|か|の)$/;
  const JA_TRAILING = /[?？。.！!…、,」"']+$/;

  function isJapanese(s) {
    let kana = 0;
    let han = 0;
    let latin = 0;
    for (const ch of s) {
      if (/[\u3040-\u30FF\u31F0-\u31FF\uFF66-\uFF9D]/.test(ch)) kana += 1;
      else if (/[\u3400-\u4DBF\u4E00-\u9FFF\uF900-\uFAFF]/.test(ch)) han += 1;
      else if (/\p{L}/u.test(ch)) latin += 1;
    }
    return kana >= 2 || (kana >= 1 && han >= 1) || (han >= 1 && latin === 0 && kana === 0);
  }

  function looksLikeAQuestion(q) {
    const s = String(q ?? '').trim();
    // Both widths of the mark. `？` is what a Japanese IME produces, and a copy
    // that only knows the ASCII one picks `search.ask` for a typed question —
    // so the person gets no answer and no line saying why.
    if (s.endsWith('?') || s.endsWith('？')) return true;
    if (isJapanese(s) && JA_ENDING.test(s.replace(JA_TRAILING, ''))) return true;
    // The daemon tokenises, and tokenising walks past opening punctuation; an
    // anchored regex does not. `„was hat Aspen gesagt` was a question to one
    // copy and not to the other, and the copy that loses is always the one
    // that picked the method — so the round trip never happened and the person
    // got neither an answer nor a line saying why.
    const head = s.replace(/^[^\p{L}\p{N}]+/u, '');
    return INTERROGATIVES.test(head);
  }

  /** The hit row for one segment id, if it is on screen. */
  function hitById(id) {
    return results.querySelector(`.seg[data-hit="${id}"]`);
  }

  /**
   * One citation, as a chip that goes somewhere. It scrolls the hit into view
   * and flashes it — deliberately NOT a jump into the transcript, which is what
   * clicking the hit itself does: the question a chip answers is "which line
   * says that?", and the line is right here.
   */
  function citationChip(id) {
    const seg = lastHits.find((s) => s.id === id);
    // A citation for a hit that is not on the page cannot be rendered as a
    // chip that goes nowhere, so it is not rendered at all — and the card
    // below refuses to draw an answer with no chips left.
    if (!seg) return null;
    const label = `${fmtClock(seg.t_ms).slice(0, 5)} ${segmentSpeakerLabel(seg)}`;
    const go = () => {
      const row = hitById(id);
      if (!row) return;
      row.scrollIntoView({ block: 'center', behavior: 'smooth' });
      row.classList.remove('cited');
      // Reflow, so re-clicking the same chip restarts the flash instead of
      // doing nothing because the class never left.
      void row.offsetWidth;
      row.classList.add('cited');
      setTimeout(() => row.classList.remove('cited'), 1600);
    };
    return h(
      'button',
      {
        class: 'chip cite-chip',
        dataset: { cite: String(id) },
        title: 'Show the turn this came from',
        'aria-label': `Show the turn at ${label}`,
        onclick: go,
      },
      label
    );
  }

  /**
   * The sentence, above the hits, with the turns it was read off under it.
   *
   * Three rules, and all three are the feature:
   * - an answer is never rendered without its chips. A sentence with no
   *   evidence attached is the app asserting something, which is the one thing
   *   this whole path exists not to do;
   * - a refusal is one quiet line, not a red box. "The transcript does not say"
   *   is a correct answer, and the hits below it are still useful;
   * - a query that was not a question gets neither.
   */
  function answerCard() {
    if (facetState.refused) {
      return h(
        'div',
        { class: 'answer-card refused', id: 'answer-card' },
        h('p', { class: 'answer-refused', id: 'answer-refused', text: refusalLine(facetState.refused.reason) })
      );
    }
    const a = facetState.answer;
    if (!a?.text) return null;
    const chips = (a.citations ?? []).map(citationChip).filter(Boolean);
    if (!chips.length) return null;
    return h(
      'div',
      { class: 'answer-card', id: 'answer-card' },
      h('p', { class: 'answer-text', id: 'answer-text', text: a.text }),
      h(
        'div',
        { class: 'answer-cites', id: 'answer-cites' },
        h('span', { class: 'sub', text: 'from' }),
        ...chips
      )
    );
  }

  /**
   * What to say when it did not answer. The daemon's reasons are written for a
   * person already, so most of them are passed through — but the two that name
   * machinery are not something anybody asked about, and the one a user will
   * see most often gets the plainest sentence in the app.
   */
  function refusalLine(reason) {
    if (reason === 'there is nothing in the archive about that') return 'Nothing in the transcript is about that.';
    if (reason === 'the local model is switched off') return 'Answers need the local model, which is switched off.';
    if (String(reason ?? '').startsWith('answers need the local model')) return 'Answers need the local model, which is not installed.';
    // Two reasons that are about the MACHINE and not about the archive, and
    // both used to fall through to the sentence below. A model that timed out
    // or was killed learned nothing about this transcript, and telling a
    // person "The transcript does not say." on its behalf is the app making a
    // claim about their own recordings out of a crash. Same for the feature
    // being switched off at the build level.
    if (reason === 'the model did not answer') return 'The model did not answer. The results below are unfiltered.';
    if (reason === 'answers are off until the bench passes') return 'Answers are switched off in this build.';
    // A post-check failure is also not a statement about the archive: there
    // WAS a sentence and it was thrown away for not coming off the rows.
    if (reason === "the model's answer did not come from the cited turns")
      return 'The model wrote an answer that was not in the cited turns, so it was discarded.';
    return 'The transcript does not say.';
  }

  function renderHits(res, modeId = facetState.mode) {
    endSearch(); focusedResult = 0;
    resultCount.textContent = searchCountLabel(res.total, lastHits.length);
    resultStatus.textContent = lastHits.length ? '↑ ↓ to explore' : '';
    clear(results);
    sub.textContent = resultSummary(modeId, res, facetState.asked?.query ?? facetState.q);
    // The card is built AFTER the hits exist in `lastHits` and appended BEFORE
    // them, because a chip has to be able to find the row it points at.
    const card = answerCard();
    if (card) results.append(card);
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
          }),
          // The pills above are not decoration here: an empty answer to a
          // question is almost always one facet too many, and this says which
          // ones are on and where to take them off.
          facetState.asked
            ? h('p', { class: 'sub', text: 'Or take one of the pills above off — the daemon may have read more into the question than you meant.' })
            : null,
          (facetState.from || facetState.to || facetState.asked?.from_ns || facetState.asked?.to_ns)
            ? h('button', { class: 'btn', onclick: allHistory }, 'Search all history') : null,
          h('button', { class: 'btn', onclick: () => { facetState.advanced = true; paintAdvanced(); speakerSel.focus(); } }, 'Adjust filters')
        )
      );
      return;
    }
    // Group only neighboring ranks. Moving a lower-ranked hit beside its
    // conversation would silently change the search engine's ordering.
    for (const group of groupSearchHits(lastHits)) {
      const headingId = `search-conversation-${group.startRank}`;
      if (group.hits.length > 1) results.append(h('div', {
        class: 'search-conversation-heading', id: headingId, role: 'heading', 'aria-level': '3',
        dataset: { conversationGroup: String(group.startRank), matches: String(group.hits.length) },
      }, h('span', { text: `${fmtDayLabel(group.minMs)} · ${fmtClock(group.minMs).slice(0, 5)}–${fmtClock(group.maxMs).slice(0, 5)}` }),
      h('span', { class: 'sub', text: `${group.hits.length} matching turns in this conversation` })));
      for (const [offset, seg] of group.hits.entries()) {
        const row = hitRow(seg, modeId, group.startRank - 1 + offset);
        if (group.hits.length > 1) {
          row.setAttribute('aria-describedby', headingId);
          row.dataset.conversationGroup = String(group.startRank);
        }
        results.append(row);
      }
    }
  }

  function hitRow(seg, modeId = facetState.mode, index = 0) {
    // A hit is very often about a voice the live window has already trimmed, so
    // the row's own `speaker_colour`/`speaker_icon` matter more here than
    // anywhere else — `look` prefers the store where it has the voice and falls
    // back to what the query answered where it does not.
    const { color, hl, icon } = look(seg);
    const audio = searchAudioState(seg);
    const row = h('article', {
      class: `seg${isUncertain(seg) ? ' uncertain' : ''}${isShaky(seg) ? ' shaky' : ''}`,
      dataset: { hit: String(seg.id) },
      tabindex: index === 0 ? '0' : '-1',
      'aria-label': `Result ${index + 1}: ${segmentSpeakerLabel(seg)}, ${fmtDayLabel(seg.t_ms)}`,
      'aria-keyshortcuts': 'ArrowUp ArrowDown Home End Enter Space',
      title: 'Enter to read context; arrow keys move through results',
    });
    row.append(
      h('span', { class: 't', text: `${fmtDay(seg.t_ms).slice(5)} ${fmtClock(seg.t_ms).slice(0, 5)}` }),
      h(
        'span',
        { class: 'who', ...(seg.speaker != null ? { dataset: { sp: String(seg.speaker) } } : {}) },
        h('span', { class: 'dot', style: `color:${color}` }),
        iconSpan(icon),
        // Same vocabulary as the transcript: a hit with no voice says which
        // kind of nameless it is, not just that a field is empty.
        h('span', {
          class: `nm${seg.speaker == null ? ' reasoned' : ''}`,
          text: segmentSpeakerLabel(seg),
          style: seg.speaker == null ? '' : `color:${color}`,
        })
      ),
      // 0.10.2: a hit renders its translation exactly as the transcript does,
      // including which of the two lines leads. A result that looked different
      // from the row it takes you to is a result you have to re-read on
      // arrival — and the highlighting still lands on the ORIGINAL, because
      // the words you searched for are the words that were said.
      translationCell(seg, (t) => highlight(t, keywordHighlightQuery(seg, modeId, facetState.asked?.query ?? facetState.q))),
      h(
        'span',
        { class: 'meta' },
        // 0.12.4: the same chips the transcript draws, from the same function
        // and in the same place, because a hit that looked different from the
        // row it takes you to is a hit you have to re-read on arrival.
        moodChips(seg),
        // The same mark the transcript uses, for the same reason: a result you
        // are about to trust is entitled to say a second decoder did not.
        isShaky(seg) ? shakyMark() : null,
        // Only in Both: in the single-leg modes every row arrived the same way
        // and a badge on all of them says nothing.
        modeId === 'both' ? viaBadge(seg.via) : null,
        h('span', { class: 'search-source', text: seg.source ?? 'unknown' }),
        h('span', { class: `search-audio ${audio.kind}`, text: audio.label, title: audio.description }),
        h('button', { class: 'btn small search-read', 'aria-controls': 'search-context',
          onclick: (e) => { e.stopPropagation(); void openContext(seg, row); } }, 'Read context'),
        // A hit is one line out of a conversation, and the question behind
        // clicking it is usually "what was going on there". This plays that
        // conversation FROM THIS LINE — not from the top, because the line is
        // what you searched for. Only where the daemon threaded the turn: a row
        // older than threading has no conversation to play.
        seg.thread != null
          ? h(
              'button',
              {
                class: 'btn small replay-start hit-replay',
                tabindex: '0',
                dataset: { replay: String(seg.thread), from: String(seg.id) },
                title: 'Replay retained audio in this conversation from this line; unavailable turns are skipped',
                'aria-label': 'Replay this conversation from this line',
                onclick: (e) => {
                  e.stopPropagation();
                  void ctx.replayThread?.(seg.thread, { from: seg.id });
                },
                onkeydown: (e) => {
                  if (e.key !== 'Enter' && e.key !== ' ') return;
                  e.preventDefault();
                  e.stopPropagation();
                  void ctx.replayThread?.(seg.thread, { from: seg.id });
                },
              },
              'Replay'
            )
          : null
      )
    );
    // The same 2px inset the transcript row wears, and refused on the same
    // grounds: an `uncertain` hit is a guess about who spoke, and styles.css
    // deliberately overrides the speaker colour on those.
    markRow(row, hl, { uncertain: isUncertain(seg) });
    const jump = () => { if (!pending) void openContext(seg, row); };
    row.addEventListener('click', jump);
    row.addEventListener('focus', () => {
      focusedResult = index;
      for (const sibling of results.querySelectorAll('.seg')) sibling.tabIndex = sibling === row ? 0 : -1;
    });
    row.addEventListener('keydown', (e) => {
      if (e.target !== row || pending) return;
      const next = nextSearchResult(index, e.key, lastHits.length);
      if (next != null) { e.preventDefault(); focusResult(next); }
      else if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); jump(); }
    });
    return row;
  }

  async function openContext(seg, trigger) {
    if (pending) return;
    contextEmpty.hidden = true; workspace.classList.add('context-open');
    for (const row of results.querySelectorAll('.seg')) {
      row.classList.toggle('context-selected', row === trigger);
      if (row === trigger) row.setAttribute('aria-current', 'true'); else row.removeAttribute('aria-current');
    }
    const my = ++contextGeneration;
    contextIds = [seg.id];
    context.hidden = false;
    clear(context);
    const close = () => { hideContext(); trigger.focus({ preventScroll: true }); };
    context.onkeydown = (e) => { if (e.key === 'Escape') { e.stopPropagation(); close(); } };
    const closeButton = h('button', { class: 'btn small', 'aria-label': 'Close conversation context', onclick: close }, 'Close');
    const content = h('div', { class: 'search-context-turns', 'aria-live': 'polite', 'aria-busy': 'true' }, 'Loading conversation…');
    context.append(h('div', { class: 'search-context-head' }, h('h2', { text: 'Around this moment' }), closeButton),
      h('p', { class: 'sub', text: `${fmtDayLabel(seg.t_ms)} · ${searchAudioState(seg).label}`, title: searchAudioState(seg).description }),
      h('div', { class: 'actions' },
        h('button', { class: 'btn', onclick: () => ctx.jumpToSegment(seg) }, 'Open transcript'),
        h('button', { class: 'btn', onclick: () => saveMomentSheet([seg.id]) }, 'Save moment'),
        seg.thread != null ? h('button', { class: 'btn', onclick: () => ctx.replayThread?.(seg.thread, { from: seg.id }) }, 'Replay') : null), content);
    closeButton.focus({ preventScroll: true });
    try {
      const res = await ask('segments.context', { id: seg.id, before: 3, after: 3 });
      if (disposed || my !== contextGeneration || !context.isConnected) return;
      content.setAttribute('aria-busy', 'false');
      clear(content);
      const turns = (res.segments ?? []).sort((a, b) => a.t_ms - b.t_ms || a.id - b.id);
      if (!turns.some((turn) => turn.id === seg.id)) throw new Error('This moment is no longer available.');
      contextIds = turns.map(turn => turn.id);
      turns.sort((a, b) => a.t_ms - b.t_ms || a.id - b.id);
      for (const turn of turns) content.append(h('article', { class: `search-context-turn${turn.id === seg.id ? ' selected' : ''}` },
        h('div', { class: 'sub', text: `${fmtClock(turn.t_ms)} · ${segmentSpeakerLabel(turn)}` }), translationCell(turn)));
      const selected = content.querySelector('.selected');
      if (selected) content.scrollTop = Math.max(0, selected.offsetTop - content.offsetTop - 80);
    } catch (e) {
      if (disposed || my !== contextGeneration || !context.isConnected) return;
      content.setAttribute('aria-busy', 'false');
      clear(content); content.append(h('p', { role: 'alert', text: `Could not load context: ${e.message}` }),
        h('button', { class: 'btn', onclick: () => openContext(seg, trigger) }, 'Try again'));
    }
  }

  // Plain text and explicit mark nodes only. Snippets from the daemon are
  // never HTML, and query punctuation never becomes a regular expression.
  function highlight(text, query) {
    return keywordMatchParts(text, query).map(part => part.matched
      ? h('mark', { class: 'hl search-match', text: part.text }) : part.text);
  }

  // Re-run mount() in place: the mode control's availability is baked into
  // its buttons, and rebuilding is cheaper to reason about than mutating them.
  function remount() {
    disposed = true; contextGeneration++; ticket();
    clear(root);
    return mount(root, ctx, arg);
  }

  fillFacets();
  paintAdvanced();
  renderPills();
  if (arg?.savedSearch && facetState.asked) void rerunAsked();
  else if (arg?.savedSearch || facetState.q || facetState.speaker || facetState.source || facetState.world) run();
  else
    results.append(
      h(
        'div',
        { class: 'empty' },
        h('b', { text: 'Search everything you have said and heard' }),
        h('p', { text: 'Type a few words, or narrow by speaker, source and date. Open a result to read the surrounding conversation.' })
      )
    );

  return {
    destroy() { disposed = true; contextGeneration++; ticket(); },
    update(change) {
      if (searchChangeTouches(change, contextIds)) hideContext();
      if (!pending && searchChangeTouches(change, lastHits.map(hit => hit.id))) {
        // Refresh the completed search, preserving any unsent input draft.
        const inputs = [qInput, speakerSel, sourceSel, fromInput, toInput];
        const draft = inputs.map(input => input.value);
        const values = [facetState.q, facetState.speaker, facetState.source, facetState.from, facetState.to];
        inputs.forEach((input, index) => { input.value = values[index] ?? ''; });
        try { void (facetState.asked ? rerunAsked() : run()); }
        finally { inputs.forEach((input, index) => { input.value = draft[index]; }); }
      }
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

// Date intent is persisted separately from resolved bounds: reopening a rolling
// search next month must not silently search the week when it was saved.
export function resolveSearchDate(date, now = new Date()) {
  if (date?.kind === 'rolling') {
    // Today counts as one of the days. Calendar arithmetic also preserves
    // local midnight across daylight-saving changes. Malformed saved filters
    // fall back to a week rather than producing invalid or unbounded dates.
    const days = Number.isInteger(date.days) && date.days >= 1 && date.days <= 3650 ? date.days : 7;
    const start = new Date(now); start.setDate(start.getDate() - (days - 1));
    return { from: isoDay(start), to: isoDay(now) };
  }
  return date?.kind === 'fixed' ? { from: date.from || '', to: date.to || '' } : { from: '', to: '' };
}
export function searchDateParams(state) {
  const out = {};
  if (state.from) out.from = new Date(`${state.from}T00:00:00`).toISOString();
  if (state.to) { const end = new Date(`${state.to}T00:00:00`); end.setDate(end.getDate() + 1); out.to = end.toISOString(); }
  return out;
}
export function withoutSearchDates(interpretation) {
  const { from_ns, to_ns, from_ms, to_ms, ...rest } = interpretation;
  return rest;
}
export function savedSearchFilters(state) {
  const { speaker, source, world, worldLabel, mode, date, asked } = state;
  return { speaker, source, world, worldLabel, mode, date: { ...date }, asked: asked ? (date.kind === 'rolling' && asked.date_explicit === false ? withoutSearchDates(asked) : { ...asked }) : null };
}
export function restoreSavedSearch(record, now = new Date()) {
  const f = record.filters ?? {};
  const date = ['fixed', 'rolling', 'all'].includes(f.date?.kind) ? f.date : { kind: 'all' };
  return { q: String(record.query ?? ''), speaker: f.speaker ?? '', source: f.source ?? '', world: f.world ?? '',
    worldLabel: f.worldLabel ?? '', mode: f.mode ?? 'keyword', date: { ...date }, ...resolveSearchDate(date, now),
    asked: f.asked ? { ...f.asked } : null, answer: null, refused: null };
}

// Default dates limit a real query; they do not turn removing the last facet
// into an unsolicited archive scan. A chosen day or saved browse is explicit.
export function hasSearchIntent(state, { saved = false } = {}) {
  return !!(String(state.q ?? '').trim() || state.world || state.speaker || state.source
    || ((state.date?.kind === 'fixed' || saved) && (state.from || state.to)));
}

// Clamp at the ends instead of wrapping past the start of a reading list.
export function nextSearchResult(index, key, count) {
  if (!Number.isInteger(count) || count <= 0) return null;
  if (key === 'Home') return 0;
  if (key === 'End') return count - 1;
  if (key === 'ArrowDown') return Math.min(count - 1, index + 1);
  if (key === 'ArrowUp') return Math.max(0, index - 1);
  return null;
}
export function searchCountLabel(total, shown) {
  const count = Number.isFinite(total) ? Math.max(shown, total) : shown;
  if (!count) return 'No matches';
  if (count > shown) return `${shown} of ${count.toLocaleString()} matches`;
  return `${count.toLocaleString()} ${count === 1 ? 'match' : 'matches'}`;
}

// FTS treats the query as literal words (including words like AND), not raw
// query syntax. Match original token ranges, so case folding or decomposed
// accents cannot shift highlight offsets or drop any of the captured text.
const SEARCH_WORDS = /[\p{L}\p{N}\p{M}]+/gu;
const foldSearchWord = word => word.normalize('NFD').replace(/\p{M}/gu, '').toLowerCase();
export function keywordMatchParts(text, query) {
  const original = String(text ?? '');
  const terms = new Set((String(query ?? '').match(SEARCH_WORDS) ?? []).map(foldSearchWord));
  if (!terms.size) return [{ text: original, matched: false }];
  const parts = [];
  let end = 0;
  for (const match of original.matchAll(SEARCH_WORDS)) {
    if (!terms.has(foldSearchWord(match[0]))) continue;
    if (match.index > end) parts.push({ text: original.slice(end, match.index), matched: false });
    parts.push({ text: match[0], matched: true });
    end = match.index + match[0].length;
  }
  if (end < original.length || !parts.length) parts.push({ text: original.slice(end), matched: false });
  return parts;
}
export function keywordHighlightQuery(segment, mode, query) {
  return mode === 'keyword' || (mode === 'both' && ['keyword', 'both'].includes(segment.via)) ? query : '';
}
export function searchAudioState(segment) {
  if (segment?.has_audio === false) return { kind: 'unavailable', label: 'Text only',
    description: 'No retained recording is linked to this turn. Its words are still available.' };
  if (segment?.has_audio === true) return { kind: 'linked', label: 'Recording linked',
    description: 'A recording reference is stored. Playback checks whether the file is still available.' };
  return { kind: 'unknown', label: 'Audio not checked',
    description: 'This result does not report audio retention. Playback checks availability.' };
}
export function groupSearchHits(hits, { maxGapMs = 5 * 60 * 1000 } = {}) {
  const span = Number.isFinite(maxGapMs) && maxGapMs >= 0 ? maxGapMs : 5 * 60 * 1000;
  const groups = [];
  const identity = row => row.thread != null ? `thread:${row.thread}`
    : row.session != null && row.source ? `session:${JSON.stringify([row.session, row.source])}` : null;
  for (const [index, hit] of hits.entries()) {
    const key = identity(hit);
    const at = Number.isFinite(hit.t_ms) ? hit.t_ms : null;
    const previous = groups.at(-1);
    // Bound the entire group, not just adjacent gaps: a chain of five-minute
    // hops must not turn hours of separate moments into one conversation.
    if (key && at != null && previous?.identity === key && previous.minMs != null
      && Math.max(previous.maxMs, at) - Math.min(previous.minMs, at) <= span) {
      previous.hits.push(hit);
      previous.minMs = Math.min(previous.minMs, at);
      previous.maxMs = Math.max(previous.maxMs, at);
    } else groups.push({ identity: key, startRank: index + 1, minMs: at, maxMs: at, hits: [hit] });
  }
  return groups;
}

export function searchChangeTouches(change, ids) {
  const changed = new Set([...(change?.purged ?? []), ...(change?.updated ?? []).map(row => row.id)]);
  return ids.some(id => changed.has(id));
}
