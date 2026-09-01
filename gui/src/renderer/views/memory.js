// Memory — the memory graph's own place in the app (docs/GRAPH.md, Tiers 2
// and 3).
//
// The other four views answer questions about the recording: what was said,
// who said it, where it came from. This one answers questions about the
// *conversations*: who owes what to whom, what you keep talking about, and
// whether the local model is allowed to look.
//
// It is a rail item rather than pushed state — unlike the person page — because
// it is not a detail of anything. You do not arrive here from a voice; you come
// here because you are wondering what you were supposed to do by Friday.
//
// Three rules run through every line of this file:
//
//   1. NOTHING AUTO-ACTS. A commitment is a suggestion until a person clicks.
//      There is no reminder, no notification, no badge that nags.
//   2. A GUESS LOOKS LIKE A GUESS. Rule-sourced rows say "pattern match" in the
//      row itself, not in a tooltip; model-sourced rows say so too. They are
//      not the same claim and they must not read as one.
//   3. THE COPY IS HONEST ABOUT THE COST. The enrichment card states the model
//      size, the core count, when it runs, that it is off by default, and that
//      nothing leaves the machine — in plain words, next to the switch.

import { h, clear, fmtDate, speakerColor } from '../lib/dom.js';
import { store, speakerLabel, ask } from '../lib/store.js';
import { toast } from '../lib/sheets.js';

export const id = 'memory';

/// The states a person can put a commitment into, and what each one means. The
/// order is the order the buttons appear in: the affirming one first.
const ACTIONS = [
  ['confirmed', 'Confirm', 'Yes, this was really promised. It stays on the list.'],
  ['done', 'Mark done', 'It happened. It leaves the open list and is kept.'],
  ['dismissed', 'Dismiss', 'This was never a promise. It leaves the list and nothing is deleted.'],
];

/// What each source is, said plainly enough to act on. This is the difference
/// between "a regular expression matched" and "a language model read it", and
/// a person deciding whether to trust a row needs to know which.
const SOURCES = {
  rules: {
    label: 'pattern match',
    cls: 'guess',
    title:
      'Found by a text rule — a phrase like "I\'ll…" or "ich schick dir…" next to somebody to owe it to. Cheap, and wrong sometimes. Nothing acted on it.',
  },
  llm: {
    label: 'local model',
    cls: 'model',
    title:
      'Read by the local model on this machine, under a grammar that makes it decide yes or no before it can name anything. Still a suggestion; nothing acted on it.',
  },
};

/** How a due date reads. `null` is not "overdue" — it is "no date was said". */
function due(c) {
  if (c.due_ms == null) return { text: 'no date', cls: 'undated', title: 'Nobody said when.' };
  const days = Math.round((c.due_ms - Date.now()) / 86_400_000);
  const when = fmtDate(new Date(c.due_ms).toISOString());
  const raw = c.due_raw ? `"${c.due_raw}"` : 'a date in the transcript';
  if (c.due_ms < Date.now()) {
    return { text: when, cls: 'past', title: `${raw}, which has passed. Nothing was done about it.` };
  }
  return {
    text: when,
    cls: days <= 1 ? 'soon' : '',
    title: `From ${raw}, resolved against when it was said.`,
  };
}

/** What to call a person the daemon described, without needing them in `store`. */
function who(p) {
  if (!p) return 'somebody';
  return p.name || p.auto || speakerLabel(p.speaker_id);
}

export function mount(root, ctx) {
  let summary = null;
  let commitments = [];
  let topics = [];
  let busy = new Set();
  let showAll = false;

  const sub = h('span', { class: 'sub', id: 'memory-sub' });
  const openCard = h('div', { class: 'card', id: 'commitments-card' });
  const topicsCard = h('div', { class: 'card', id: 'topics-card' });
  const enrichCard = h('div', { class: 'card', id: 'enrich-card' });
  const body = h('div', { class: 'view-body view-enter' }, openCard, topicsCard, enrichCard);

  root.append(
    h(
      'div',
      { class: 'view-head' },
      h('div', {}, h('h1', { text: 'Memory' }), sub),
      h('div', { class: 'spacer' })
    ),
    body
  );

  // -- open commitments -----------------------------------------------------

  function renderCommitments() {
    clear(openCard);
    const shown = showAll
      ? commitments
      : commitments.filter((c) => c.state === 'candidate' || c.state === 'confirmed');
    const settled = commitments.length - shown.length;

    openCard.append(
      h(
        'div',
        { class: 'sheet-head' },
        h('div', { class: 'card-title', text: showAll ? 'Every commitment' : 'Open commitments' }),
        settled > 0 || showAll
          ? h(
              'button',
              {
                class: 'btn small',
                id: 'commitments-toggle',
                title: showAll
                  ? 'Show only what is still open'
                  : 'Also show what has been done or dismissed',
                onclick: () => {
                  showAll = !showAll;
                  renderCommitments();
                },
              },
              showAll ? 'Open only' : `Show settled (${settled})`
            )
          : null
      )
    );

    if (!shown.length) {
      openCard.append(
        h(
          'div',
          { class: 'empty', id: 'commitments-empty' },
          h('b', { text: showAll ? 'Nothing noticed yet' : 'Nothing open' }),
          h('p', {
            text: commitments.length
              ? 'Everything that was noticed has been dealt with.'
              : 'Promises show up here when somebody says they will do something and there is somebody in the conversation to owe it to. Nothing was noticed yet — which is not the same as nothing having been promised.',
          })
        )
      );
      return;
    }

    const list = h('div', { class: 'commit-list', id: 'commit-list' });
    for (const c of shown) list.append(commitmentRow(c));
    openCard.append(
      list,
      h('p', {
        class: 'rail-hint',
        id: 'commitments-note',
        style: 'padding:12px 0 0;max-width:70ch',
        text: 'These are suggestions. Nothing here reminds you, notifies you, or acts on its own — the list is only ever as long as what you leave on it.',
      })
    );
  }

  function commitmentRow(c) {
    const d = due(c);
    const src = SOURCES[c.source] ?? SOURCES.rules;
    const pending = busy.has(c.id);
    const row = h('div', {
      class: `commit-row state-${c.state}${pending ? ' pending' : ''}`,
      dataset: { commitment: String(c.id), state: c.state, source: c.source },
    });

    row.append(
      h(
        'span',
        { class: `commit-due ${d.cls}`.trim(), title: d.title },
        d.text,
        h('small', { text: c.due_raw ?? 'no date said' })
      ),
      h(
        'span',
        { class: 'commit-body' },
        h(
          'span',
          { class: 'commit-who' },
          h('span', { class: 'dot', style: `color:${speakerColor(c.who?.speaker_id)}` }),
          h('b', { class: 'commit-name', text: who(c.who) }),
          h('span', { class: 'commit-arrow', text: '→' }),
          h('span', { class: 'commit-to', text: c.to ? who(c.to) : 'the conversation' }),
          // Rule 2: the claim's provenance sits in the row, not in a tooltip.
          h('span', { class: `chip src ${src.cls}`, title: src.title, text: src.label }),
          c.state !== 'candidate'
            ? h('span', { class: `chip state ${c.state}`, text: c.state })
            : null
        ),
        h('span', { class: 'commit-what', text: c.what }),
        // The line it was found in, so you can disagree with it here rather
        // than having to go and check.
        h(
          'button',
          {
            class: 'commit-said',
            dataset: { segment: String(c.segment) },
            title: 'Read this in the transcript',
            onclick: () => open(c),
          },
          h('span', { class: 'quote-mark', text: '“' }),
          c.said ?? 'the transcript for this turn is gone',
          h('span', { class: 'quote-mark', text: '”' })
        )
      ),
      h(
        'span',
        { class: 'commit-actions' },
        ...ACTIONS.filter(([state]) => state !== c.state).map(([state, label, title]) =>
          h(
            'button',
            {
              class: `btn small${state === 'dismissed' ? ' quiet' : ''}`,
              dataset: { act: state, commitment: String(c.id) },
              title,
              disabled: pending,
              onclick: () => void setState(c, state),
            },
            label
          )
        )
      )
    );
    return row;
  }

  /** A commitment → the turn it was found in, in the transcript. */
  async function open(c) {
    if (c.thread != null) {
      const landed = await ctx.showThreadInTranscript?.(c.thread);
      if (landed !== null) return;
    }
    ctx.jumpToSegment?.({ id: c.segment, t_ms: c.t_ms ?? Date.now() });
  }

  async function setState(c, state) {
    const before = c.state;
    // Optimistic, like every other switch in this app: the daemon confirms
    // with a `commitment` broadcast and a failure puts it back.
    c.state = state;
    busy.add(c.id);
    renderCommitments();
    try {
      Object.assign(c, await ask('commitments.set_state', { id: c.id, state }));
      await refreshSummary();
      toast(
        state === 'dismissed'
          ? 'Dismissed. Nothing was deleted — the conversation is untouched.'
          : state === 'done'
            ? 'Marked done.'
            : 'Confirmed.',
        'ok'
      );
    } catch (e) {
      c.state = before;
      toast(`Could not change that — ${e.message}`, 'error');
    } finally {
      busy.delete(c.id);
      renderCommitments();
    }
  }

  // -- topics ---------------------------------------------------------------

  function renderTopics() {
    clear(topicsCard);
    topicsCard.append(h('div', { class: 'card-title', text: 'Topics' }));
    if (!topics.length) {
      topicsCard.append(
        h(
          'div',
          { class: 'empty', id: 'topics-empty' },
          h('b', { text: 'No topics yet' }),
          h('p', {
            text: 'Conversations get a short label from the local model. It is off until you turn it on below, and it only runs while you are not gaming.',
          })
        )
      );
      return;
    }
    const list = h('div', { class: 'topic-list', id: 'topic-list' });
    for (const t of topics) {
      list.append(
        h(
          'button',
          {
            class: 'topic-row',
            dataset: { topic: t.topic, threads: String(t.threads) },
            title: `Open the most recent of ${t.threads} conversation${t.threads === 1 ? '' : 's'} about this`,
            onclick: () => void openTopic(t),
          },
          h('span', { class: 'topic-name', text: t.topic }),
          h(
            'span',
            { class: 'topic-num' },
            String(t.threads ?? 0),
            h('small', { text: t.threads === 1 ? 'conversation' : 'conversations' })
          ),
          h('span', { class: 'topic-num' }, String(t.segments ?? 0), h('small', { text: 'turns' })),
          h(
            'span',
            { class: 'topic-num' },
            t.last_ms ? fmtDate(new Date(t.last_ms).toISOString()) : '—',
            h('small', { text: 'last heard' })
          )
        )
      );
    }
    topicsCard.append(list);
  }

  async function openTopic(t) {
    const thread = (t.thread_ids ?? [])[0];
    if (thread == null) {
      toast('That topic has no conversation left to open.', '');
      return;
    }
    await ctx.showThreadInTranscript?.(thread);
  }

  // -- enrichment status ----------------------------------------------------
  //
  // Rule 3 lives here. The switch is one line; everything around it is what
  // turning it on actually does, in words rather than in a settings reference.

  let switchPending = false;

  function renderEnrichment() {
    clear(enrichCard);
    const cfg = summary?.config ?? {};
    const st = summary?.enrichment ?? { phase: 'off' };
    const phase = cfg.enabled ? st.phase ?? 'idle' : 'off';
    const installed = cfg.installed !== false;

    const toggle = h('button', {
      class: 'toggle',
      role: 'switch',
      id: 'enrich-toggle',
      'aria-pressed': String(!!cfg.enabled),
      'aria-label': cfg.enabled ? 'Stop the local model' : 'Let the local model run when idle',
      disabled: switchPending || store.conn.status !== 'connected',
      onclick: () => void setEnabled(!cfg.enabled),
    });

    const chip = chipFor(phase, st);
    enrichCard.append(
      h(
        'div',
        { class: 'mic-head' },
        h('span', { class: 'mic-ico', 'aria-hidden': 'true' }, brainIcon()),
        h(
          'span',
          { class: 'mic-title' },
          h('span', { class: 'name', text: 'Reading your conversations, locally' }),
          h('span', { class: 'key', text: cfg.llm_model ?? 'qwen2.5-3b-instruct-q4_k_m.gguf' })
        ),
        h('span', { class: 'spacer' }),
        h('span', { class: chip.cls, id: 'enrich-chip' }, h('span', { class: `dot${chip.live ? ' pulse' : ''}` }), chip.text),
        toggle
      )
    );

    // The progress line. Only while something is actually running — a bar at
    // 0% for an hour is not information.
    if (phase === 'running') {
      const frac = st.batch_total ? st.batch_done / st.batch_total : 0;
      enrichCard.append(
        h(
          'div',
          { class: 'enrich-progress', id: 'enrich-progress' },
          h('span', { class: 'op-bar' }, h('i', { style: `transform:scaleX(${Math.max(0.02, frac)})` })),
          h('span', {
            class: 'enrich-progress-text',
            id: 'enrich-progress-text',
            text: `reading conversation ${st.batch_done + 1} of ${st.batch_total}`,
          })
        )
      );
    }

    if (phase === 'blocked' && st.reason) {
      enrichCard.append(
        h('p', { class: 'enrich-reason', id: 'enrich-reason', text: `Waiting — ${st.reason}` })
      );
    }
    if (!installed) {
      enrichCard.append(
        h('p', {
          class: 'mic-warn',
          id: 'enrich-missing',
          text: `The model is not downloaded yet. Run \`recalld models fetch --graph\` in a terminal (about ${fmtGb(cfg.download_bytes)}), then come back — nothing here works until it is on disk.`,
        })
      );
    }

    // The honest copy, as four flat statements. Not a paragraph anybody skims:
    // four lines, each of which is a fact somebody might object to.
    enrichCard.append(
      h(
        'ul',
        { class: 'enrich-facts', id: 'enrich-facts' },
        fact(`A ${fmtGb(cfg.download_bytes)} language model, running on this machine.`),
        fact(`${cfg.llm_threads ?? 4} CPU cores at the lowest priority — never the GPU unless you say so.`),
        fact('Only while nothing is being captured and capture is not paused. Never while you are gaming.'),
        fact('Off until you turn it on, and everything it finds is a suggestion you can dismiss.'),
        fact('Nothing leaves this machine. There is no network in this program except the one-time model download.')
      ),
      h('p', {
        class: 'rail-hint',
        id: 'enrich-note',
        style: 'padding:10px 0 0;max-width:70ch',
        text: 'It reads conversations nobody has looked at yet, newest first, and writes two things: who promised what, and a short label for what the conversation was about. It never changes a transcript.',
      })
    );

    if (st.last_error) {
      enrichCard.append(
        h('p', { class: 'enrich-error', id: 'enrich-error', text: `Last error: ${st.last_error}` })
      );
    }
    if (summary?.counts) {
      const c = summary.counts;
      enrichCard.append(
        h('p', {
          class: 'rail-hint',
          id: 'enrich-counts',
          style: 'padding:8px 0 0',
          text: `${c.threads_enriched} of ${c.threads} conversations read · ${c.threads_pending} waiting · ${c.from_llm} commitments from the model, ${c.from_rules} from pattern matching`,
        })
      );
    }
  }

  function fact(text) {
    return h('li', {}, h('span', { class: 'tick', 'aria-hidden': 'true', text: '·' }), text);
  }

  /** Three states a person acts on, out of the daemon's five. */
  function chipFor(phase, st) {
    switch (phase) {
      case 'running':
        return { text: 'reading', cls: 'chip live', live: true };
      case 'blocked':
        return { text: 'waiting', cls: 'chip warn' };
      case 'unavailable':
        return { text: 'not installed', cls: 'chip warn' };
      case 'idle':
        return {
          text: st?.walked ? 'idle — nothing left to read' : 'idle',
          cls: 'chip',
        };
      default:
        return { text: 'off', cls: 'chip' };
    }
  }

  async function setEnabled(next) {
    switchPending = true;
    renderEnrichment();
    try {
      const out = await ask('graph.enrich', { action: next ? 'start' : 'stop' });
      summary = { ...(summary ?? {}), config: out.config, enrichment: out.enrichment };
      toast(
        next
          ? 'The local model will read your conversations while the machine is idle.'
          : 'Stopped. Everything it already found stays; nothing new is written.',
        'ok'
      );
    } catch (e) {
      toast(`Could not change that — ${e.message}`, 'error');
    } finally {
      switchPending = false;
      await refreshSummary().catch(() => {});
      renderEnrichment();
    }
  }

  // -- loading --------------------------------------------------------------

  async function refreshSummary() {
    summary = await ask('graph.summary');
    renderSub();
    return summary;
  }

  function renderSub() {
    const c = summary?.counts;
    if (!c) {
      sub.textContent = 'loading';
      return;
    }
    sub.textContent = `${c.open} open · ${c.commitments} noticed · ${c.topics} topic${c.topics === 1 ? '' : 's'}`;
  }

  async function load() {
    try {
      const [s, list, tops] = await Promise.all([
        ask('graph.summary'),
        ask('commitments.list'),
        ask('topics.list'),
      ]);
      summary = s;
      commitments = list.commitments ?? [];
      topics = tops.topics ?? [];
    } catch (e) {
      summary = null;
      sub.textContent = 'could not be loaded';
      clear(openCard);
      openCard.append(
        h(
          'div',
          { class: 'empty' },
          h('b', { text: 'The memory graph could not be loaded' }),
          h('p', { text: e.message }),
          h('p', {
            class: 'sub',
            text: 'A daemon older than 0.7.0 has no commitments or topics; the rest of the app works exactly as before.',
          })
        )
      );
      clear(topicsCard);
      clear(enrichCard);
      return;
    }
    renderSub();
    renderCommitments();
    renderTopics();
    renderEnrichment();
  }

  renderSub();
  renderCommitments();
  renderTopics();
  renderEnrichment();
  void load();

  return {
    update(change) {
      // The worker's state arrives on the status topic, so the card follows a
      // batch without this view polling anything.
      if (change?.graph || change?.status) {
        if (change.graph) summary = { ...(summary ?? {}), enrichment: change.graph };
        renderEnrichment();
      }
      if (change?.commitment) {
        const row = commitments.find((c) => c.id === change.commitment.id);
        if (row) Object.assign(row, change.commitment);
        renderCommitments();
      }
      if (change?.opFinished?.kind === 'graph.enrich') void load();
      if (change?.relabel) renderCommitments();
    },
    reload: load,
  };
}

function fmtGb(bytes) {
  const gb = (Number(bytes) || 1_946_604_700) / 1_073_741_824;
  return `${gb.toFixed(1)} GB`;
}

/// §14.2's icon well wants a stroked glyph, and this one has to read as
/// "thinking about what was said" rather than as a brand mark: a speech bubble
/// with a thread running through it.
function brainIcon() {
  const ns = 'http://www.w3.org/2000/svg';
  const el = document.createElementNS(ns, 'svg');
  el.setAttribute('viewBox', '0 0 24 24');
  el.setAttribute('width', '18');
  el.setAttribute('height', '18');
  el.setAttribute('fill', 'none');
  el.setAttribute('stroke', 'currentColor');
  el.setAttribute('stroke-width', '1.7');
  el.setAttribute('aria-hidden', 'true');
  for (const d of ['M4 5h16v11H9l-5 4z', 'M8 9h8M8 12.5h5']) {
    const p = document.createElementNS(ns, 'path');
    p.setAttribute('d', d);
    p.setAttribute('stroke-linecap', 'square');
    el.append(p);
  }
  return el;
}
