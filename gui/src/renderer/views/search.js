// Search — over the transcript, with speaker / source / date facets, in three
// modes: the words (FTS), the meaning (vectors), or both fused. The mode
// control and the per-hit `via` marker live in ./semantic.js.
// A hit is not a destination: clicking one loads the transcript around that
// moment and highlights it, because "what did she say about that world?" is
// answered by the conversation, not by the matching line on its own.

import { h, clear, fmtClock, fmtDay, fmtDayLabel, fmtDate, speakerColor } from '../lib/dom.js';
import { store, speakerLabel, segmentSpeakerLabel, isUncertain, isShaky, ask } from '../lib/store.js';
import { shakyMark, translationCell } from '../lib/marks.js';
import { toast } from '../lib/sheets.js';
import { defaultMode, modeControl, modeById, requestFor, resultSummary, semanticState, viaBadge } from './semantic.js';

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
    placeholder: 'ask it — “what did Kira say yesterday about the portal”',
    value: facetState.q,
    onkeydown: (e) => {
      if (e.key === 'Enter') runAsk();
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
      title: 'The speaker, source and date facets, set by hand',
      onclick: () => {
        facetState.advanced = !facetState.advanced;
        paintAdvanced();
      },
    },
    'Advanced'
  );

  // The front door: one box, one button, and a drawer.
  const askRow = h(
    'div',
    { class: 'facets ask-row' },
    h('div', { class: 'facet grow' }, h('label', { for: 'search-q', text: 'Ask' }), qInput),
    h('div', { class: 'facet' }, h('label', { text: ' ' }), h('button', { class: 'btn primary', id: 'search-ask', onclick: () => runAsk() }, 'Ask')),
    h('div', { class: 'facet' }, h('label', { text: ' ' }), advancedBtn)
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
    h('div', { class: 'facet' }, h('label', { for: 'search-speaker', text: 'Speaker' }), speakerSel),
    h('div', { class: 'facet' }, h('label', { for: 'search-source', text: 'Source' }), sourceSel),
    h('div', { class: 'facet' }, h('label', { for: 'search-from', text: 'From' }), fromInput),
    h('div', { class: 'facet' }, h('label', { for: 'search-to', text: 'To' }), toInput),
    h('div', { class: 'facet' }, h('label', { text: ' ' }), h('button', { class: 'btn', id: 'search-go', onclick: () => run() }, 'Search'))
  );

  const body = h('div', { class: 'view-body view-enter' }, h('div', { class: 'card' }, askRow, pills, facets, modes.el), resultCard);
  root.append(
    h('div', { class: 'view-head' }, h('div', {}, h('h1', { text: 'Search' }), sub), h('div', { class: 'spacer' })),
    body
  );

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
  async function runAsk() {
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
      const res = await ask(question ? 'search.answer' : 'search.ask', { q: facetState.q, limit: 100 });
      facetState.asked = res.interpretation ?? null;
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
      if (e.code === 'unknown_method') {
        // A daemon too old for 0.11.0 can still read the question — try the
        // one method back before giving up on the whole path. Only then does
        // this fall through to a plain keyword search.
        if (question) {
          try {
            const res = await ask('search.ask', { q: facetState.q, limit: 100 });
            facetState.asked = res.interpretation ?? null;
            lastHits = res.hits ?? [];
            renderPills();
            renderHits(res);
            return undefined;
          } catch (again) {
            if (again.code !== 'unknown_method') throw again;
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
      clear(results);
      results.append(
        h('div', { class: 'empty' }, h('b', { text: 'That question could not be asked' }), h('p', { text: `${e.message}. The daemon may be restarting — try again in a moment.` }))
      );
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
    const it = facetState.asked;
    // Taking a pill off changes which turns were searched, so whatever
    // sentence was above them was read off a different set of rows. It goes.
    facetState.answer = null;
    facetState.refused = null;
    if (!it) return run();
    const params = { q: it.query ?? '', limit: 100 };
    if (it.speaker_id != null) params.speaker = it.speaker_id;
    if (it.world_id) params.world = it.world_id;
    if (it.from_ns) params.from = new Date(nsToMs(it.from_ns)).toISOString();
    if (it.to_ns) params.to = new Date(nsToMs(it.to_ns)).toISOString();
    // An empty query with facets is a browse, and only the keyword leg can
    // answer one — `search.semantic` has nothing to embed.
    const modeId = params.q.trim() ? modeIdOf(it.mode) : 'keyword';
    try {
      const [method, p] = requestFor(modeId, params);
      const res = await ask(method, p);
      lastHits = res.hits ?? [];
      renderPills();
      renderHits(res, modeId);
    } catch (e) {
      clear(results);
      results.append(
        h('div', { class: 'empty' }, h('b', { text: 'Search failed' }), h('p', { text: e.message }))
      );
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
    clear(pills);
    const it = facetState.asked;
    // The world pill shows on BOTH paths: a facet the daemon read out of a
    // question, and one a world chip set by hand. Without the second, arriving
    // from "Where you meet" would filter the results with nothing on screen
    // saying so — a filter you cannot see is a filter you cannot take off.
    const handWorld = !it && facetState.world;
    pills.hidden = !it && !handWorld;
    if (!it && !handWorld) return;

    const pill = (key, label, title, drop) =>
      h(
        'span',
        { class: 'ask-pill', dataset: { facet: key }, title },
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
      pills.append(
        pill('speaker', it.speaker_label || speakerLabel(it.speaker_id), 'Only this voice — press ✕ to search everybody', () => {
          delete facetState.asked.speaker_id;
          delete facetState.asked.speaker_label;
        })
      );
    }
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
    if (!facetState.q.trim() && !facetState.world && !facetState.speaker && !facetState.source) {
      lastHits = [];
      clear(results);
      sub.textContent = 'Everything captured, by word or by meaning.';
      results.append(
        h(
          'div',
          { class: 'empty' },
          h('b', { text: 'Search everything you have said and heard' }),
          h('p', {
            text: 'Type a few words, or narrow by speaker, source and date. Results open in the transcript where they were said.',
          })
        )
      );
      return;
    }

    const params = { q: facetState.q, limit: 100 };
    if (facetState.speaker) params.speaker = Number(facetState.speaker);
    if (facetState.source) params.source = facetState.source;
    if (facetState.from) params.from = `${facetState.from}T00:00:00Z`;
    if (facetState.to) params.to = `${facetState.to}T23:59:59Z`;
    if (facetState.world) params.world = facetState.world;

    try {
      // A world with no words is a BROWSE, not a search for nothing: arriving
      // from a world chip, the facet is the whole question and there is no
      // query to rank by. `transcript` takes the same facet and answers it.
      if (!facetState.q.trim() && facetState.world) {
        const res = await ask('transcript', { world: facetState.world, limit: 100 });
        lastHits = [...(res.segments ?? [])].reverse();
        renderHits({ total: lastHits.length, hits: lastHits }, 'keyword');
        return;
      }
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
    /^(was|wer|wen|wem|wessen|wann|wo|wohin|woher|wie|warum|wieso|weshalb|welche[rsn]?|what|who|whom|whose|when|where|why|how|which)\b/i;

  function looksLikeAQuestion(q) {
    const s = String(q ?? '').trim();
    return s.endsWith('?') || INTERROGATIVES.test(s);
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
    return 'The transcript does not say.';
  }

  function renderHits(res, modeId = facetState.mode) {
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
            : null
        )
      );
      return;
    }
    for (const seg of lastHits) results.append(hitRow(seg, modeId));
  }

  function hitRow(seg, modeId = facetState.mode) {
    const color = speakerColor(seg.speaker);
    const row = h('div', {
      class: `seg${isUncertain(seg) ? ' uncertain' : ''}${isShaky(seg) ? ' shaky' : ''}`,
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
      // 0.10.2: a hit renders its translation exactly as the transcript does,
      // including which of the two lines leads. A result that looked different
      // from the row it takes you to is a result you have to re-read on
      // arrival — and the highlighting still lands on the ORIGINAL, because
      // the words you searched for are the words that were said.
      translationCell(seg, (t) => highlight(t, facetState.q)),
      h(
        'span',
        { class: 'meta' },
        // The same mark the transcript uses, for the same reason: a result you
        // are about to trust is entitled to say a second decoder did not.
        isShaky(seg) ? shakyMark() : null,
        // Only in Both: in the single-leg modes every row arrived the same way
        // and a badge on all of them says nothing.
        modeId === 'both' ? viaBadge(seg.via) : null,
        h('span', { class: 'chip', text: seg.source ?? 'unknown' }),
        // A hit is one line out of a conversation, and the question behind
        // clicking it is usually "what was going on there". This plays that
        // conversation FROM THIS LINE — not from the top, because the line is
        // what you searched for. Only where the daemon threaded the turn: a row
        // older than threading has no conversation to play.
        seg.thread != null
          ? h(
              'span',
              {
                class: 'btn small replay-start hit-replay',
                role: 'button',
                tabindex: '0',
                dataset: { replay: String(seg.thread), from: String(seg.id) },
                title: 'Play this conversation back from this line',
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
    return mount(root, ctx, arg);
  }

  fillFacets();
  paintAdvanced();
  renderPills();
  if (facetState.q || facetState.speaker || facetState.source || facetState.world) run();
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
