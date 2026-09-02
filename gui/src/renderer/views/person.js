// The person page — the memory graph's first surface (docs/GRAPH.md, Tier 1).
//
// The Speakers view answers "which voices are there". This answers "who is
// this, and who do they talk to", which is a different question and the one
// that makes a transcript feel like a memory rather than a log.
//
// It is PUSHED state, not a fifth rail item: you always arrive here from a
// voice, and Back takes you where you came from. The rail stays four items
// because the app still has four places, and this is a detail of one of them.
//
// Everything on it comes from one `person.get` (PROTOCOL, schema 6). One round
// trip, because the page is one question and half a person on screen while the
// other half is still in flight is worse than a moment of nothing.

import { h, clear, fmtDur, fmtDate, speakerColor } from '../lib/dom.js';
import { store, speakerLabel, isYou, ask } from '../lib/store.js';
import { toast } from '../lib/sheets.js';
import { playSpeaker, stop as stopPreview, isActive, onPlayback, noAudioHint } from '../lib/preview.js';

export const id = 'person';

/**
 * How long the page waits before re-asking after the feed moved (finding #18).
 *
 * Long enough that a burst of live segments costs ONE `person.get` rather than
 * one each, short enough that a page left open follows the conversation it is
 * about. Everything here is derived from live segments (docs/GRAPH.md), so
 * "last heard" is only ever as true as the last query.
 */
const REFRESH_MS = 2000;

/** The same key the Speakers view uses, so the two can never sound at once. */
const previewKey = (spId) => `speaker:${spId}`;

/** What to call a person the daemon described, without needing them in `store`. */
function labelOf(row, fallbackId) {
  return row?.name || row?.auto || speakerLabel(fallbackId ?? row?.speaker_id);
}

export function mount(root, ctx, arg) {
  const spId = Number(arg?.id);
  let page = null;

  const title = h('h1', { text: speakerLabel(spId) });
  const sub = h('span', { class: 'sub', id: 'person-sub' });
  const back = h(
    'button',
    {
      class: 'btn small',
      id: 'person-back',
      title: 'Back to the voices',
      onclick: () => ctx.back?.(),
    },
    '← Back'
  );

  const header = h('div', { class: 'card', id: 'person-header' });
  const stats = h('div', { class: 'card', id: 'person-stats' });
  // 0.10.0. Two cards, in the order the questions come: WHERE you meet, then
  // HOW you talk. Both sit above "people they talk with" because both are
  // about this person, and the edges are about everybody else.
  const worlds = h('div', { class: 'card', id: 'person-worlds' });
  const talk = h('div', { class: 'card', id: 'person-talk' });
  const edges = h('div', { class: 'card', id: 'person-edges' });
  const threads = h('div', { class: 'card', id: 'person-threads' });
  const body = h('div', { class: 'view-body view-enter' }, header, stats, worlds, talk, edges, threads);

  root.append(
    h('div', { class: 'view-head' }, back, h('div', {}, title, sub), h('div', { class: 'spacer' })),
    body
  );

  // -- header ---------------------------------------------------------------

  const hint = h('span', { class: 'sp-hint', id: 'person-hint' });

  function previewButton() {
    const btn = h('button', {
      class: 'btn small preview',
      id: 'person-play',
      dataset: { preview: String(spId) },
      title: 'Listen to this voice',
      onclick: () => void togglePreview(),
    });
    paintButton(btn);
    return btn;
  }

  function paintButton(btn = document.getElementById('person-play')) {
    if (!btn) return;
    const on = isActive(previewKey(spId));
    btn.classList.toggle('on', on);
    btn.textContent = on ? '■' : '▶';
    btn.setAttribute('aria-pressed', String(on));
    btn.setAttribute('aria-label', on ? `Stop ${speakerLabel(spId)}` : `Play a sample of ${speakerLabel(spId)}`);
  }

  async function togglePreview() {
    if (isActive(previewKey(spId))) {
      stopPreview();
      return;
    }
    hint.textContent = '';
    hint.classList.remove('shown');
    const res = await playSpeaker(previewKey(spId), spId);
    // A preview that fails explains itself where it was pressed, never in a
    // toast that has scrolled away by the time you look.
    if (!res.stopped && !res.played) {
      hint.textContent = noAudioHint(res.error);
      hint.classList.add('shown');
    }
  }

  function renderHeader() {
    clear(header);
    const sp = page?.speaker ?? store.speakers.get(spId) ?? null;
    const name = labelOf(sp, spId);
    title.textContent = name;
    const languages = page?.languages ?? sp?.languages ?? null;
    header.append(
      h(
        'div',
        { class: 'person-head' },
        h('span', { class: 'dot big', style: `color:${speakerColor(spId)}` }),
        h(
          'div',
          { class: 'person-who' },
          h('b', { class: sp?.name ? 'person-name' : 'person-name unnamed', text: name, id: 'person-name' }),
          h(
            'div',
            { class: 'person-tags' },
            // "You" is provenance, not a rank: the label came from the
            // microphone rather than from a match, and saying so is the point.
            isYou(spId) ? h('span', { class: 'chip', text: 'your own voice' }) : null,
            h('span', {
              class: `chip lang${languages?.length ? ' set' : ''}`,
              id: 'person-lang',
              title: 'Which languages this voice speaks — set it from the Speakers list',
              text: languages?.length ? languages.map((l) => l.toUpperCase()).join(' + ') : 'Any language',
            }),
            sp?.name ? null : h('span', { class: 'chip', text: 'not named yet' })
          ),
          hint
        ),
        h('div', { class: 'spacer' }),
        // The same one-line account the roster raises when this person walks
        // in (0.8.0), asked for deliberately. It is on the header rather than
        // in a card because it is a SUMMARY of everything below it: a card
        // would be the page's contents restated above the page's contents.
        h(
          'button',
          {
            class: 'btn small',
            id: 'person-brief',
            title: `What is open between you and ${name}, in one line`,
            onclick: () => void ctx.showBrief?.(spId),
          },
          'Brief'
        ),
        previewButton()
      )
    );
  }

  // -- stat strip -----------------------------------------------------------

  // `ms` is the raw instant behind a rendered date. The visible text is only
  // minute-resolution, so it is not something a test — or a person watching a
  // conversation happen — can read a change out of.
  function stat(label, value, note, ms = null) {
    const dataset = { stat: label.toLowerCase().replace(/\s+/g, '-') };
    if (ms != null) dataset.ms = String(ms);
    return h(
      'div',
      { class: 'person-stat', dataset },
      h('b', { text: value }),
      h('small', { text: label }),
      note ? h('em', { text: note }) : null
    );
  }

  function renderStats() {
    clear(stats);
    const t = page?.totals;
    stats.append(h('div', { class: 'card-title', text: 'In total' }));
    if (!t) {
      stats.append(h('div', { class: 'empty' }, h('p', { text: 'Still loading.' })));
      return;
    }
    stats.append(
      h(
        'div',
        { class: 'person-strip', id: 'person-strip' },
        stat('total speech', fmtDur(t.speech_ms)),
        stat('segments', String(t.segments ?? 0)),
        stat('sessions', String(t.sessions ?? 0)),
        stat('conversations', String(t.threads ?? 0)),
        stat('first heard', t.first_heard_ms ? fmtDate(new Date(t.first_heard_ms).toISOString()) : '—', null, t.first_heard_ms ?? 0),
        stat('last heard', t.last_heard_ms ? fmtDate(new Date(t.last_heard_ms).toISOString()) : '—', null, t.last_heard_ms ?? 0)
      )
    );
  }

  // -- where you meet (0.10.0) ----------------------------------------------
  //
  // Chips rather than rows: a world is a NAME and a couple of numbers, and
  // five of them read at a glance where five table rows do not. The card is
  // absent entirely when the daemon has no worlds for this person — which is
  // every Discord-only voice, and every conversation older than the visits
  // table. An empty state here would be an advertisement for VRChat.

  function renderWorlds() {
    clear(worlds);
    const list = page?.worlds ?? [];
    worlds.hidden = !list.length;
    if (!list.length) return;
    worlds.append(
      h(
        'div',
        { class: 'sheet-head' },
        h('div', { class: 'card-title', text: 'Where you meet' }),
        h('span', {
          class: 'sub',
          id: 'person-worlds-sub',
          text: 'From VRChat\u2019s own log. Time in conversation there — not time in the world.',
        })
      )
    );
    const row = h('div', { class: 'world-chips', id: 'person-world-chips' });
    for (const w of list) {
      // A world with no name renders as its id. The name arrives on a separate
      // log line and sometimes never does, and an id is still a place.
      const named = !!w.name;
      row.append(
        h(
          'button',
          {
            class: `world-chip${named ? '' : ' unnamed'}`,
            dataset: { world: w.world_id },
            title: named ? w.world_id : 'This world has no name in the log',
            onclick: () => ctx.searchWorld?.(w.world_id, w.name),
          },
          h('span', { class: 'world-name', text: named ? w.name : w.world_id }),
          h(
            'span',
            { class: 'world-meta' },
            `${w.visits} visit${w.visits === 1 ? '' : 's'}`,
            ' \u00b7 ',
            fmtDur(w.together_ms ?? 0),
            ' \u00b7 ',
            w.last_ms ? fmtDate(new Date(w.last_ms).toISOString()) : '\u2014'
          )
        )
      );
    }
    worlds.append(row);
  }

  // -- how you talk (0.10.0) -------------------------------------------------
  //
  // Its own request (`person.stats`), not part of `person.get`: it is a
  // different question, it is the more expensive one, and a page that already
  // renders should not wait on it.
  //
  // Two of these numbers are approximations and BOTH carry their definition in
  // a tooltip, taken from the daemon rather than written here — one wording,
  // one meaning. A statistic whose caveat lives in a different file is a
  // statistic that will eventually be shown without one.

  let talkStats = null;

  function pct(v) {
    return `${Math.round((v ?? 0) * 100)}%`;
  }

  function renderTalk() {
    clear(talk);
    const t = talkStats;
    // No turns means nothing to say about how they talk — and a row of zeroes
    // would be a claim that they never interrupt anybody.
    talk.hidden = !t || !t.turns;
    if (!t || !t.turns) return;
    const defs = t.definitions ?? {};

    talk.append(
      h(
        'div',
        { class: 'sheet-head' },
        h('div', { class: 'card-title', text: 'How you talk' }),
        h('span', {
          class: 'sub',
          id: 'talk-sub',
          text: `${t.turns} turn${t.turns === 1 ? '' : 's'} across ${
            (t.by_conversation ?? []).length
          } recent conversation${(t.by_conversation ?? []).length === 1 ? '' : 's'}`,
        })
      )
    );

    // The share bar. A number and a bar, because "34%" is a fact and the bar
    // is what makes it a comparison.
    talk.append(
      h(
        'div',
        { class: 'talk-share', id: 'talk-share', dataset: { share: String(t.share ?? 0) } },
        h(
          'div',
          { class: 'talk-share-head' },
          h('b', { id: 'talk-share-pct', text: pct(t.share) }),
          h('small', {
            title: defs.share ?? '',
            text: 'of the speech in the conversations they were in',
          })
        ),
        h(
          'div',
          { class: 'talk-bar' },
          h('span', {
            class: 'talk-bar-fill',
            style: `width:${Math.min(100, Math.max(0, (t.share ?? 0) * 100))}%`,
          })
        )
      )
    );

    const cell = (key, value, label, title) =>
      h(
        'div',
        { class: 'person-stat', dataset: { talk: key }, title: title ?? '' },
        h('b', { text: value }),
        h('small', { text: label }),
        title ? h('em', { class: 'talk-why', text: 'hover for what this counts' }) : null
      );

    talk.append(
      h(
        'div',
        { class: 'person-strip', id: 'talk-strip' },
        cell('mean-turn', fmtDur(t.mean_turn_ms ?? 0), 'mean turn'),
        cell('monologue', fmtDur(t.longest_monologue_ms ?? 0), 'longest run'),
        cell(
          'interruptions',
          `${t.interruptions_given ?? 0} / ${t.interruptions_received ?? 0}`,
          'interruptions given / received',
          defs.interruption ??
            'A turn that starts while somebody else is still talking, with overlapped speech in it. An approximation.'
        ),
        cell(
          'latency',
          // Null is not zero. Zero would say they always answered instantly;
          // the dash says nobody measured a reply.
          t.median_latency_ms == null ? '\u2014' : fmtDur(t.median_latency_ms),
          'usual reply gap',
          defs.latency ??
            'Median gap from the previous speaker finishing to them starting, over gaps of at most five seconds.'
        ),
        cell('tpm', (t.turns_per_minute ?? 0).toFixed(1), 'turns per minute')
      )
    );

    if (t.median_latency_ms == null) {
      talk.append(
        h('p', {
          class: 'rail-hint',
          id: 'talk-latency-note',
          style: 'padding:10px 0 0;max-width:70ch',
          text: 'No reply gap yet: nothing they said followed somebody else inside five seconds. Longer gaps are dropped rather than squashed to five, because past that it is a lull and not an answer.',
        })
      );
    }
  }

  // -- who they talk with ---------------------------------------------------

  function renderEdges() {
    clear(edges);
    edges.append(h('div', { class: 'card-title', text: 'People they talk with' }));
    const list = page?.edges ?? [];
    if (!list.length) {
      edges.append(
        h(
          'div',
          { class: 'empty' },
          h('b', { text: 'Nobody yet' }),
          h('p', {
            text: 'This voice has not shared a conversation with another named voice. Being in the same instance is not the same thing — these are the people they actually take turns with.',
          })
        )
      );
      return;
    }
    // The roster column is a property of the list, not of a row: reserved for
    // everyone when anyone has it, absent when nobody does.
    const anyRoster = list.some((e) => e.roster_seconds != null);
    const rows = h('div', { class: `edge-list${anyRoster ? ' has-roster' : ''}`, id: 'edge-list' });
    for (const e of list) {
      rows.append(
        h(
          'button',
          {
            class: 'edge-row',
            dataset: { edge: String(e.speaker_id) },
            title: `Open ${labelOf(e, e.speaker_id)}'s page`,
            onclick: () => ctx.openPerson?.(e.speaker_id),
          },
          h('span', { class: 'dot', style: `color:${speakerColor(e.speaker_id)}` }),
          h('span', { class: 'edge-name', text: labelOf(e, e.speaker_id) }),
          h(
            'span',
            { class: 'edge-num' },
            String(e.threads ?? 0),
            h('small', { text: e.threads === 1 ? 'conversation' : 'conversations' })
          ),
          h('span', { class: 'edge-num' }, fmtDur((e.seconds ?? 0) * 1000), h('small', { text: 'they spoke' })),
          h(
            'span',
            { class: 'edge-num' },
            e.last_ms ? fmtDate(new Date(e.last_ms).toISOString()) : '—',
            h('small', { text: 'last together' })
          ),
          // The roster can only vouch for two voices the user has named. For
          // everyone else the honest answer is nothing at all — a zero would
          // be a claim that they were never in a room together, which the
          // daemon did not say and cannot know.
          anyRoster
            ? h(
                'span',
                {
                  class: 'edge-num roster',
                  title:
                    e.roster_seconds != null
                      ? 'Time both names were in the same VRChat instance, from the roster'
                      : 'The roster cannot vouch for this pair — it knows names, and one of these voices has none',
                },
                e.roster_seconds != null ? fmtDur(e.roster_seconds * 1000) : '',
                h('small', { text: e.roster_seconds != null ? 'in the same instance' : '' })
              )
            : null
        )
      );
    }
    edges.append(rows);
  }

  // -- recent conversations -------------------------------------------------

  function renderThreads() {
    clear(threads);
    threads.append(h('div', { class: 'card-title', text: 'Recent conversations' }));
    const list = page?.recent_threads ?? [];
    if (!list.length) {
      threads.append(
        h(
          'div',
          { class: 'empty' },
          h('b', { text: 'No conversations yet' }),
          h('p', { text: 'Conversations appear once this voice has been heard alongside the turns around it.' })
        )
      );
      return;
    }
    const rows = h('div', { class: 'thread-list', id: 'thread-list' });
    for (const t of list) {
      const names = (t.participants ?? []).map((p) => labelOf(p, p.speaker_id));
      rows.append(
        h(
          'button',
          {
            class: 'thread-row',
            dataset: { thread: String(t.thread_id) },
            title: 'Read this conversation in the transcript',
            onclick: () => ctx.showThreadInTranscript?.(t.thread_id),
          },
          h(
            'span',
            { class: 'thread-when' },
            t.started_ms ? fmtDate(new Date(t.started_ms).toISOString()) : '—',
            h('small', { text: `${t.segments ?? 0} segment${t.segments === 1 ? '' : 's'}` })
          ),
          h(
            'span',
            { class: 'thread-body' },
            h(
              'span',
              { class: 'thread-who' },
              ...(t.participants ?? []).map((p) =>
                h('span', { class: 'dot', style: `color:${speakerColor(p.speaker_id)}`, title: labelOf(p, p.speaker_id) })
              ),
              h('span', { class: 'thread-names', text: names.join(' · ') })
            ),
            // A preview is a handle, not a summary: the first thing anybody
            // said, verbatim. Tier 1 does not summarise.
            h('span', { class: 'thread-preview', text: t.preview || 'no transcript for this conversation' })
          ),
          // "Read this conversation" is the row; this is "hear it". Nested in a
          // button, so it is a span with a button's role and stops the click.
          h(
            'span',
            {
              class: 'btn small replay-start thread-replay',
              role: 'button',
              tabindex: '0',
              dataset: { replay: String(t.thread_id) },
              title: 'Play this conversation back, turn by turn',
              'aria-label': 'Replay this conversation',
              onclick: (e) => {
                e.stopPropagation();
                void ctx.replayThread?.(t.thread_id);
              },
              onkeydown: (e) => {
                if (e.key !== 'Enter' && e.key !== ' ') return;
                e.preventDefault();
                e.stopPropagation();
                void ctx.replayThread?.(t.thread_id);
              },
            },
            'Replay'
          )
        )
      );
    }
    threads.append(rows);
  }

  // -- loading --------------------------------------------------------------

  // The last answer, serialised. A refresh that brings back exactly what is
  // already on screen must not rebuild the DOM: the page re-asks while the
  // conversation is happening, and rebuilding it under a reader's cursor for no
  // reason is its own kind of wrong.
  let lastPayload = null;

  async function load() {
    let fetched;
    try {
      fetched = await ask('person.get', { id: spId });
    } catch (e) {
      page = null;
      lastPayload = null;
      sub.textContent = 'could not be loaded';
      clear(header);
      header.append(
        h(
          'div',
          { class: 'empty' },
          h('b', { text: 'This page could not be loaded' }),
          h('p', { text: e.message }),
          h('p', {
            class: 'sub',
            text: 'A daemon older than 0.6.2 has no memory graph; the rest of the app works exactly as before.',
          })
        )
      );
      clear(stats);
      clear(edges);
      clear(threads);
      // 0.10.0: both cards are absent-by-default, so a failed load hides them
      // rather than leaving last answer's worlds under an error message.
      talkStats = null;
      clear(worlds);
      worlds.hidden = true;
      clear(talk);
      talk.hidden = true;
      return;
    }
    const payload = JSON.stringify(fetched);
    if (page && payload === lastPayload) return;
    page = fetched;
    lastPayload = payload;
    const t = page.totals ?? {};
    sub.textContent = `${t.threads ?? 0} conversation${t.threads === 1 ? '' : 's'} · ${(page.edges ?? []).length} ${
      (page.edges ?? []).length === 1 ? 'person' : 'people'
    } · ${fmtDur(t.speech_ms)} of speech`;
    renderHeader();
    renderStats();
    renderWorlds();
    renderEdges();
    renderThreads();
  }

  /**
   * `person.stats` (0.10.0), on its own. A daemon too old to know the method
   * is not an error worth a red box on a page that is otherwise complete: the
   * card simply is not there.
   */
  async function loadTalk() {
    try {
      talkStats = await ask('person.stats', { id: spId });
    } catch {
      talkStats = null;
    }
    if (!body.isConnected) return;
    renderTalk();
  }

  /**
   * The page was frozen at mount (audit finding #18): it loaded once and then
   * only ever reacted to a rename or a merge. Everything ELSE on it moves —
   * total speech, segments, sessions, conversations, "last heard", the edges,
   * the recent-conversation list — because every one of those is a query over
   * live segments. A person page left open while the person is talking sat
   * there claiming they were last heard when the page happened to be opened.
   *
   * Debounced rather than per-event: a burst of segments is one re-fetch, and
   * the timer dies with the page rather than outliving it.
   */
  let refreshTimer = null;
  function scheduleRefresh() {
    if (refreshTimer) return;
    refreshTimer = setTimeout(() => {
      refreshTimer = null;
      // Mounted-only. `go()` clears the root, so a page that has been left
      // behind must not keep querying on a DOM nobody can see.
      if (!body.isConnected) return;
      void load();
      void loadTalk();
    }, REFRESH_MS);
  }

  // Playback lives outside the view (one <audio> for the whole app), so the
  // button follows it rather than owning it. No unmount hook, so the
  // subscription retires itself once its DOM is gone.
  const off = onPlayback(() => {
    if (!body.isConnected) {
      off();
      return;
    }
    paintButton();
  });

  renderHeader();
  renderStats();
  renderWorlds();
  renderTalk();
  renderEdges();
  renderThreads();
  void load();
  void loadTalk();

  return {
    update(change) {
      if (!change) return;
      // A rename anywhere — here, the CLI, another client — arrives as one
      // broadcast, and this page shows names in four places.
      if (change.relabel || change.merged) {
        if (change.merged?.from === spId) {
          // The voice this page is about was merged away. Following the merge
          // is the only honest thing to do: the id no longer exists.
          toast(`${speakerLabel(spId)} was merged — showing ${speakerLabel(change.merged.into)}.`, '');
          ctx.openPerson?.(change.merged.into);
          return;
        }
        void load();
        return;
      }
      // Everything else on this page is a query over live segments, so it
      // moves on exactly these: a turn arriving, rows being deleted, and an
      // operation (a merge, a split, a purge) finishing. `detached` is a live
      // turn that arrived while the TRANSCRIPT window is parked on another day
      // — this page is not, so it counts here even though that view did not
      // file the row.
      if (change.added || change.detached || change.purged || change.opFinished) scheduleRefresh();
    },
    reload: () => Promise.all([load(), loadTalk()]),
  };
}
