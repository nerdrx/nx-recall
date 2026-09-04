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

import { h, clear, fmtDate, fmtClock, fmtDayLabel, fmtDur } from '../lib/dom.js';
import { store, speakerLabel, ask, applyAssist, MOOD_MODES } from '../lib/store.js';
// 0.12.0 — per-person highlights. Every graph row here (digest participants,
// a commitment's two sides, a world's people) carries `colour`/`icon`, which is
// what `lookOn` reads; `lookOf` is for the accuracy table, whose rows carry a
// speaker id and nothing else.
import { lookOn, lookOf, iconSpan } from './highlight.js';
import { toast } from '../lib/sheets.js';

export const id = 'memory';

/**
 * 0.8.0 put three more cards on this tab, and it is worth saying why here
 * rather than in a commit message, because the obvious other home was Sources.
 *
 * Sources answers "what is this program ALLOWED to hear" — an allowlist, a
 * microphone switch, and what all of it costs on disk. Every control on it is a
 * consent decision. Notes, the vocabulary and the accuracy figures are not
 * consent decisions at all: they are what the app MADE of what it was allowed
 * to hear, which is the question this tab already exists to answer. A glossary
 * that changes how words are heard sitting under a disk-usage table would be
 * filed by the machinery it touches rather than by the thing it is for.
 *
 * The vocabulary and the accuracy card are also next to each other on purpose,
 * and directly under the notes: they are one loop. You fix a transcript, the
 * fix moves the accuracy figures and lands in the corrections vocabulary, and
 * the vocabulary is what the next transcript is biased toward. Three cards, one
 * sentence, in the order it happens.
 */

/// How long a snooze puts a reminder off for, and what each one is for.
///
/// Three, and no free-text field. A snooze is a thing you press while you are
/// doing something else — most of the time inside a headset, with a controller
/// — and a minute picker is not pressable in that state. The daemon accepts
/// anything up to a week (`snooze_min`); these are the three a person actually
/// wants.
const SNOOZES = [
  [10, '10 min', 'Bring this back in ten minutes.'],
  [60, '1 hour', 'Bring this back in an hour.'],
  [60 * 12, 'Tonight', 'Bring this back in twelve hours.'],
];

/// The states a note can be put in, and what each one means. Same shape as the
/// commitments above: nothing but a click moves one, at either end of the socket.
const NOTE_ACTIONS = [
  ['open', 'Reopen', 'Put it back on the list.'],
  ['done', 'Done', 'It happened. It stays, greyed, and nothing is deleted.'],
  ['dismissed', 'Dismiss', 'It was not a note. Nothing is deleted — the turn stays in the transcript.'],
];

/// A rate as a person reads one. Two significant figures, because the third is
/// noise on a sample of a dozen corrections and printing it would claim a
/// precision the estimate does not have.
function pct(x) {
  const n = Number(x);
  if (!Number.isFinite(n)) return '—';
  return `${(n * 100).toFixed(1)}%`;
}

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

/**
 * How a reminder's own date reads (0.9.0).
 *
 * Deliberately NOT `due()` above. A commitment's date is a claim about
 * something somebody else said and is rendered as a guess; a note's date is a
 * thing you said out loud about yourself, so it is rendered as a fact — and the
 * near ones are rendered in the units a person thinks in ("in 20 min"), because
 * a reminder twenty minutes away is not a date, it is a countdown.
 */
function dueChip(n) {
  if (n.due_ms == null) return null;
  const left = n.due_ms - Date.now();
  const mins = Math.round(left / 60000);
  if (n.fired && left <= 0) {
    return { text: 'reminded', cls: 'fired', title: `This came round at ${fmtDate(new Date(n.due_ms).toISOString())}.` };
  }
  if (left <= 0) {
    return { text: 'due now', cls: 'soon', title: 'This is due; the reminder is on its way.' };
  }
  if (mins < 60) return { text: `in ${mins} min`, cls: 'soon', title: `Due at ${fmtClock(n.due_ms).slice(0, 5)}.` };
  const sameDay = new Date(n.due_ms).toDateString() === new Date().toDateString();
  const day = fmtDayLabel(n.due_ms).split(' · ')[0].toLowerCase();
  return {
    text: sameDay ? `today ${fmtClock(n.due_ms).slice(0, 5)}` : `${day} ${fmtClock(n.due_ms).slice(0, 5)}`,
    cls: '',
    title: `Due ${fmtDate(new Date(n.due_ms).toISOString())}.`,
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
  /// 0.10.2: an `assist.set` is in flight, so the three controls are dead until
  /// it answers — the same visible optimistic window the thread stepper has.
  let translatePending = false;

  let notes = [];
  let noteBusy = new Set();
  let digests = [];
  /// A note somebody asked to be taken to, which may not be on screen yet.
  let pendingFocus = null;
  let accuracy = null;
  let vocab = store.vocab;
  /// 0.10.0 — every place a conversation has happened.
  let worlds = [];

  const sub = h('span', { class: 'sub', id: 'memory-sub' });
  const digestCard = h('div', { class: 'card', id: 'digest-card' });
  const openCard = h('div', { class: 'card', id: 'commitments-card' });
  const notesCard = h('div', { class: 'card', id: 'notes-card' });
  const accuracyCard = h('div', { class: 'card', id: 'accuracy-card' });
  const vocabCard = h('div', { class: 'card', id: 'vocab-card' });
  const topicsCard = h('div', { class: 'card', id: 'topics-card' });
  // 0.10.0. Above Topics, below Notes: a world is WHERE, a topic is WHAT, and
  // the where is the one people remember first.
  const worldsCard = h('div', { class: 'card', id: 'worlds-card' });
  const enrichCard = h('div', { class: 'card', id: 'enrich-card' });
  // 0.10.2. Directly under the model's own card, because it IS the model's
  // other job: the same 1.9 GB of weights, the same pinned cores, the same
  // queue. A person who has just read what the local model costs is the person
  // deciding whether to give it a second thing to do.
  const translateCard = h('div', { class: 'card', id: 'translate-card' });
  // 0.12.4. Directly under Translation, because it is the other question of the
  // same kind — "what else goes on a transcript row, and how loudly" — and
  // because both of its controls are `assist.set` calls on the same live block.
  // Its own card rather than a fourth block inside that one: a mood is not a
  // translation, and a card whose title lied about half its contents would be a
  // worse economy than one more heading.
  const moodCard = h('div', { class: 'card', id: 'mood-card' });
  const body = h(
    'div',
    { class: 'view-body view-enter' },
    // 0.9.0. Above the commitments on purpose: what happened is the thing you
    // came here to be reminded of, and what is still owed is what you do about
    // it. The card is absent entirely until there is something in it.
    digestCard,
    openCard,
    notesCard,
    // The correction loop, in the order it happens (see the note at the top).
    accuracyCard,
    vocabCard,
    worldsCard,
    topicsCard,
    enrichCard,
    translateCard,
    moodCard
  );

  root.append(
    h(
      'div',
      { class: 'view-head' },
      h('div', {}, h('h1', { text: 'Memory' }), sub),
      h('div', { class: 'spacer' })
    ),
    body
  );

  // -- yesterday (0.9.0) ----------------------------------------------------
  //
  // One paragraph per conversation that has settled, written by the local
  // model. Two groups and no more: "Yesterday", because that is the question
  // this card answers, and "Earlier today" for the ones from this morning that
  // are already over. Anything older is `recalld digest <day>` — a card is a
  // card, and an archive of every evening is a different surface.
  //
  // The card renders NOTHING when there is nothing, rather than an empty state.
  // An empty state here would be a permanent advertisement for a feature that
  // is off by default and needs a 1.9 GB download; the enrichment card below
  // already says all of that, once, where the switch is.

  function renderDigests() {
    clear(digestCard);
    if (!digests.length) {
      digestCard.hidden = true;
      return;
    }
    digestCard.hidden = false;
    const start = new Date();
    start.setHours(0, 0, 0, 0);
    const today = digests.filter((d) => (d.started_ms ?? 0) >= start.getTime());
    const before = digests.filter((d) => (d.started_ms ?? 0) < start.getTime());
    // "Yesterday" only when it really is. `digest.list` with no day returns the
    // newest conversations whichever day they happened on, so a machine that
    // was off for a week would otherwise have a card headed "Yesterday" over
    // last Tuesday — a small lie, and the kind that makes a person doubt the
    // paragraph under it.
    const yesterday = new Date(start.getTime() - 86_400_000);
    const leadIsYesterday =
      before.length > 0 && new Date(before[0].started_ms ?? 0) >= yesterday;

    digestCard.append(
      h(
        'div',
        { class: 'sheet-head' },
        h('div', {
          class: 'card-title',
          text: before.length ? (leadIsYesterday ? 'Yesterday' : 'Recently') : 'Earlier today',
        }),
        h('span', {
          class: 'sub',
          id: 'digest-sub',
          text: `${digests.length} conversation${digests.length === 1 ? '' : 's'} summarised`,
        })
      )
    );
    // The heading above says which group leads, so a sub-heading is only
    // written when there are two groups to tell apart.
    for (const [key, label, group] of [
      ['before', null, before],
      ['today', 'Earlier today', today],
    ]) {
      if (!group.length) continue;
      if (label && before.length) {
        digestCard.append(
          h('div', { class: 'acc-group-title', dataset: { group: key }, text: label })
        );
      }
      const list = h('div', { class: 'digest-list', dataset: { digests: key } });
      for (const d of group) list.append(digestRow(d));
      digestCard.append(list);
    }
    digestCard.append(
      h('p', {
        class: 'rail-hint',
        id: 'digest-note',
        style: 'padding:12px 0 0;max-width:70ch',
        text: 'Written by the local model on this machine, once per conversation, after it has finished. Conversations it read and found not worth a paragraph are not here — which is most short ones.',
      })
    );
  }

  function digestRow(d) {
    const people = d.participants ?? [];
    return h(
      'button',
      {
        class: 'digest-row',
        dataset: { digest: String(d.thread_id), day: d.day ?? '' },
        title: 'Read this conversation from its first turn',
        onclick: () => void openDigest(d),
      },
      h(
        'span',
        { class: 'digest-head' },
        h('span', {
          class: 'digest-when',
          text: d.started_ms ? fmtDate(new Date(d.started_ms).toISOString()) : '—',
        }),
        h(
          'span',
          { class: 'digest-people' },
          ...people.map((p) => {
            // A digest participant carries its own `colour`/`icon` (PROTOCOL
            // v15), which is what lets a conversation from July name people
            // this client has never listed in their own colours.
            const pl = lookOn(p, p.speaker_id);
            return h(
              'span',
              {
                class: 'chip person',
                dataset: {
                  sp: String(p.speaker_id),
                  ...(p.share == null ? {} : { share: p.share.toFixed(3) }),
                },
                // 0.10.0: the bar is speech TIME, not turn count — two people
                // take the same number of turns and one of them talks four
                // times as long. The tooltip says which, because a bar with no
                // units is a bar that gets read as the other thing.
                title:
                  p.share == null
                    ? ''
                    : `${Math.round(p.share * 100)}% of the speech in this conversation, over ${
                        p.turns ?? 0
                      } turn${p.turns === 1 ? '' : 's'}`,
              },
              h('span', { class: 'dot', style: `color:${pl.color}` }),
              iconSpan(pl.icon),
              p.label || speakerLabel(p.speaker_id),
              p.share == null
                ? null
                : h(
                    'span',
                    { class: 'share-bar' },
                    h('span', {
                      class: 'share-bar-fill',
                      // The bar is the same colour as the dot above it and has
                      // been since 0.10.0 — it is that person's share, so a
                      // highlight has to reach it or the chip would carry two
                      // different colours for one person.
                      style: `width:${Math.min(100, Math.max(0, p.share * 100))}%;background:${pl.color}`,
                    })
                  )
            );
          })
        ),
        d.turns ? h('span', { class: 'digest-turns', text: `${d.turns} turns` }) : null
      ),
      h('span', { class: 'digest-text', text: d.summary ?? '' }),
      // What the model thought was left hanging. Shown as text and not as
      // something to tick: a commitment is the row you act on, and this is a
      // sentence about the conversation.
      (d.open ?? []).length
        ? h(
            'span',
            { class: 'digest-open' },
            ...(d.open ?? []).map((o) => h('span', { class: 'digest-open-item', text: o }))
          )
        : null,
      // A digest is the model's paragraph ABOUT an evening; this plays the
      // evening. The card is itself a button, so this one stops the click.
      h(
        'span',
        {
          class: 'btn small replay-start digest-replay',
          role: 'button',
          tabindex: '0',
          dataset: { replay: String(d.thread_id) },
          title: 'Play this conversation back, turn by turn',
          'aria-label': 'Replay this conversation',
          onclick: (e) => {
            e.stopPropagation();
            void ctx.replayThread?.(d.thread_id);
          },
          onkeydown: (e) => {
            if (e.key !== 'Enter' && e.key !== ' ') return;
            e.preventDefault();
            e.stopPropagation();
            void ctx.replayThread?.(d.thread_id);
          },
        },
        'Replay'
      )
    );
  }

  /** A digest → the conversation it is about, in the transcript. */
  async function openDigest(d) {
    const landed = await ctx.showThreadInTranscript?.(d.thread_id);
    if (landed === null) toast('That conversation is no longer in the transcript.', '');
  }

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
    // A commitment's `who` and `to` carry `colour`/`icon` of their own (v15),
    // so a row about somebody the live window has forgotten still wears their
    // mark. `to` may be absent entirely — a promise made to the room.
    const whoLook = lookOn(c.who, c.who?.speaker_id);
    const toLook = lookOn(c.to, c.to?.speaker_id);

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
          h('span', { class: 'dot', style: `color:${whoLook.color}` }),
          iconSpan(whoLook.icon),
          // Both sides of the arrow, because "X → Y" is the one line in this app
          // where two people are named in the same breath and telling them
          // apart at a glance is the whole job of a highlight. Plain ink unless
          // there is one: neither name has ever worn the identity hue.
          h('b', {
            class: 'commit-name',
            text: who(c.who),
            ...(whoLook.hl ? { style: `color:${whoLook.hl}` } : {}),
          }),
          h('span', { class: 'commit-arrow', text: '→' }),
          h(
            'span',
            {
              class: 'commit-to',
              ...(c.to && toLook.hl ? { style: `color:${toLook.hl}` } : {}),
            },
            c.to ? iconSpan(toLook.icon) : null,
            c.to ? who(c.to) : 'the conversation'
          ),
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

  // -- notes to self (0.8.0) -------------------------------------------------
  //
  // A note is not a new kind of data: it is one MIC turn that began with a wake
  // phrase, and the turn is still in the transcript where it was said. That is
  // why every row here is a door back to it — the note is a handle on a moment,
  // and the moment is the thing.

  function renderNotes() {
    clear(notesCard);
    notesCard.append(
      h(
        'div',
        { class: 'sheet-head' },
        h('div', { class: 'card-title', text: 'Notes to self' }),
        h('span', {
          class: 'sub',
          id: 'notes-sub',
          text: notesSub(),
        })
      )
    );
    if (!notes.length) {
      notesCard.append(
        h(
          'div',
          { class: 'empty', id: 'notes-empty' },
          h('b', { text: 'No notes yet' }),
          h('p', {
            text: 'Say “recall, remember…” or “recall, merk dir…” into your microphone and the rest of the sentence lands here. The turn itself stays in the transcript — this is a second reading of it, not a copy.',
          })
        )
      );
      return;
    }
    const list = h('div', { class: 'note-list', id: 'note-list' });
    for (const n of notes) list.append(noteRow(n));
    notesCard.append(list);
    // A reminder may have asked for a row that did not exist yet.
    applyFocus();
  }

  /** "3 open · 1 with a reminder" — the second half only when it is true. */
  function notesSub() {
    if (!notes.length) return '';
    const open = notes.filter((n) => n.state === 'open');
    const timed = open.filter((n) => n.due_ms != null && !n.fired);
    return timed.length
      ? `${open.length} open · ${timed.length} will remind you`
      : `${open.length} open`;
  }

  function noteRow(n) {
    const pending = noteBusy.has(n.id);
    // 0.9.0: a note that carries a time is a reminder, and it says so in the
    // row. Everything else about the row is unchanged — a reminder is not a
    // second kind of note, it is a note with a date on it.
    const d = dueChip(n);
    const row = h('div', {
      class: `note-row state-${n.state}${pending ? ' pending' : ''}${d ? ' timed' : ''}${
        n.fired ? ' fired' : ''
      }`,
      dataset: {
        note: String(n.id),
        state: n.state,
        segment: String(n.segment_id),
        ...(n.due_ms != null ? { due: String(n.due_ms), fired: String(!!n.fired) } : {}),
      },
    });
    row.append(
      h(
        'button',
        {
          class: 'note-body',
          title: 'Read this in the transcript, where it was said',
          onclick: () => ctx.jumpToSegment?.({ id: n.segment_id, t_ms: n.t_ms ?? Date.now() }),
        },
        h('span', { class: 'note-text', text: n.text || '(nothing after the wake phrase)' }),
        h(
          'span',
          { class: 'note-meta' },
          d ? h('span', { class: `chip due ${d.cls}`.trim(), dataset: { due: 'chip' }, title: d.title, text: d.text }) : null,
          h('span', { class: 'note-when', text: n.t_ms ? fmtDate(new Date(n.t_ms).toISOString()) : '—' })
        )
      ),
      h(
        'span',
        { class: 'note-actions' },
        // Snooze is offered on an OPEN note only, and it is the one control
        // here that changes when a note comes back rather than what it is.
        ...(n.state === 'open'
          ? SNOOZES.map(([mins, label, title]) =>
              h(
                'button',
                {
                  class: 'chip note-act snooze',
                  dataset: { snooze: String(mins), note: String(n.id) },
                  title,
                  disabled: pending,
                  onclick: () => void snoozeNote(n, mins),
                },
                label
              )
            )
          : []),
        ...NOTE_ACTIONS.filter(([state]) => state !== n.state).map(([state, label, title]) =>
          h(
            'button',
            {
              class: `chip note-act${state === 'dismissed' ? ' quiet' : ''}`,
              dataset: { noteAct: state, note: String(n.id) },
              title,
              disabled: pending,
              onclick: () => void setNoteState(n, state),
            },
            label
          )
        ),
        h('span', { class: `chip state ${n.state}`, dataset: { noteState: n.state }, text: n.state })
      )
    );
    return row;
  }

  /**
   * "Not now" (0.9.0).
   *
   * `notes.set_state` with `snooze_min` rather than a method of its own: a
   * snooze IS a state change — the note goes back to open and its date moves —
   * and two ways to reach one row is two things a client has to keep in sync.
   *
   * On a note with no date at all this GIVES it one, which is the only way to
   * ask to be reminded of something you said without a time in it.
   */
  async function snoozeNote(n, minutes) {
    noteBusy.add(n.id);
    renderNotes();
    try {
      Object.assign(n, await ask('notes.set_state', { id: n.id, state: 'open', snooze_min: minutes }));
      const hours = Math.round(minutes / 60);
      toast(
        `Back in ${
          minutes < 60 ? `${minutes} minutes` : `${hours} hour${hours === 1 ? '' : 's'}`
        }.`,
        'ok'
      );
    } catch (e) {
      toast(`Could not snooze that — ${e.message}`, 'error');
    } finally {
      noteBusy.delete(n.id);
      renderNotes();
    }
  }

  async function setNoteState(n, state) {
    const before = n.state;
    // Optimistic with a rollback, like every other switch in this app.
    n.state = state;
    noteBusy.add(n.id);
    renderNotes();
    try {
      Object.assign(n, await ask('notes.set_state', { id: n.id, state }));
    } catch (e) {
      n.state = before;
      toast(`Could not change that note — ${e.message}`, 'error');
    } finally {
      noteBusy.delete(n.id);
      renderNotes();
    }
  }

  // -- the accuracy dashboard (0.8.0) ----------------------------------------
  //
  // Everything on this card is derived from corrections a PERSON made. That is
  // the only ground truth the daemon has about how wrong it was, and it is why
  // the empty state says "correct a few transcripts" rather than "no data":
  // there is nothing to measure until somebody disagrees with it, and no amount
  // of waiting produces any.

  function renderAccuracy() {
    clear(accuracyCard);
    accuracyCard.append(h('div', { class: 'card-title', text: 'How well it is hearing you' }));
    const a = accuracy;
    if (!a || !a.corrections) {
      accuracyCard.append(
        h(
          'div',
          { class: 'empty', id: 'accuracy-empty' },
          h('b', { text: 'Nothing to measure yet' }),
          h('p', { text: 'Correct a few transcripts and this fills in. Every fix is one more line of ground truth, and the estimate is only ever as good as how much of it there is.' })
        )
      );
      return;
    }
    accuracyCard.append(
      h(
        'div',
        { class: 'person-strip', id: 'accuracy-strip' },
        h(
          'div',
          { class: 'person-stat', dataset: { stat: 'corrections' } },
          h('b', { id: 'accuracy-corrections', text: String(a.corrections) }),
          h('small', { text: 'corrections' }),
          h('em', { text: 'lines somebody retyped' })
        ),
        h(
          'div',
          { class: 'person-stat', dataset: { stat: 'wer' } },
          h('b', { id: 'accuracy-wer', text: pct(a.edit_rate ?? a.estimated_wer) }),
          h('small', { text: 'of their words changed' }),
          h('em', { text: 'in the lines you fixed — a share, never above 100%' })
        ),
        // 0.10.1: the one figure here that covers EVERY row. The second decoder
        // reads each turn too, so its disagreement rate is not biased toward
        // the lines somebody bothered to fix.
        a.cross_check?.checked
          ? h(
              'div',
              { class: 'person-stat', dataset: { stat: 'shaky' } },
              h('b', { id: 'accuracy-shaky', text: pct(a.cross_check.shaky_share) }),
              h('small', { text: 'second decoder disagreed' }),
              h('em', { text: `of ${a.cross_check.checked} checked rows, fixed or not` })
            )
          : null
      ),
      byRows('by-source', 'By source', (a.by_source ?? []).map((r) => [r.source, r])),
      byRows(
        'by-speaker',
        'By voice',
        (a.by_speaker ?? []).slice(0, 5).map((r) => [speakerLabel(r.speaker_id), r]),
        (r) => r.speaker_id
      ),
      h('p', {
        class: 'rail-hint',
        id: 'accuracy-note',
        style: 'padding:10px 0 0;max-width:70ch',
        text: 'Two different estimates. "Changed" counts only the lines somebody retyped, so it reads high where you have been careful and says nothing where you have not. "Shaky" is the share of all checked rows a second decoder read differently — unbiased, but a disagreement is not always an error.',
      }),
      learnedLine(a.learned)
    );
  }

  /**
   * What the corrections have TAUGHT it (0.12.4).
   *
   * Every other number on this card looks backwards. This one is the reason to
   * keep correcting: each fix is one row of word-level ground truth, and at
   * thirty of them in one cell — one voice, one kind of source, one turn length
   * — the daemon can start measuring which of its decoders to believe there.
   * So the line says the count, and, until the bar is met, exactly how many
   * more are wanted. "Nothing learned yet" on its own would be a dead end.
   */
  function learnedLine(l) {
    if (!l) return null;
    const n = l.corrections ?? 0;
    const rules = l.rules ?? 0;
    const text = rules
      ? `${rules} decoder rule${rules === 1 ? '' : 's'} learned from ${n} correction${n === 1 ? '' : 's'}.`
      : `Learned from ${n} correction${n === 1 ? '' : 's'} — ${l.needed ?? 0} more in one voice, source and turn length and it can start choosing between its decoders.`;
    return h('p', {
      class: 'rail-hint',
      id: 'accuracy-learned',
      style: 'padding:6px 0 0;max-width:70ch',
      text,
    });
  }

  /**
   * `idOf` is how a table says its rows are about PEOPLE. "By source" rows are
   * about programs and have no voice behind them, so they pass nothing and get
   * exactly the plain row they have always had. "By voice" rows do, and this is
   * the one name surface in the app with no dot next to it — a table of
   * numbers, where a second column of colour would be noise. So the highlight
   * lands on the name itself, and only when there is one.
   */
  function byRows(key, title, rows, idOf = () => null) {
    if (!rows.length) return null;
    const list = h('div', { class: 'acc-rows', dataset: { acc: key } });
    for (const [label, r] of rows) {
      const spId = idOf(r);
      const accLook = spId == null ? null : lookOf(spId);
      list.append(
        h(
          'div',
          { class: 'acc-row', dataset: { accRow: label } },
          h(
            'span',
            { class: 'acc-name', ...(accLook?.hl ? { style: `color:${accLook.hl}` } : {}) },
            accLook ? iconSpan(accLook.icon) : null,
            label
          ),
          h('span', { class: 'acc-num' }, String(r.corrections ?? 0), h('small', { text: 'fixed' })),
          h('span', { class: 'acc-num' }, pct(r.edit_rate ?? r.estimated_wer), h('small', { text: 'changed' })),
          h(
            'span',
            { class: 'acc-num' },
            r.cross_check?.checked ? pct(r.cross_check.shaky_share) : '—',
            h('small', { text: 'shaky' })
          )
        )
      );
    }
    return h('div', { class: 'acc-group' }, h('div', { class: 'acc-group-title', text: title }), list);
  }

  // -- the vocabulary (0.8.0) ------------------------------------------------
  //
  // Two halves that must never be confused. The user glossary is a decision —
  // editable, removable, and the only thing on this card anybody can change.
  // The auto groups are OBSERVATIONS: who has been in the instance, which
  // worlds have been named, which words people keep correcting. Showing them as
  // chips that cannot be pressed is the point — you cannot argue with what was
  // heard, and pretending otherwise would be a button that does nothing.

  let vocabPending = false;

  function renderVocab() {
    clear(vocabCard);
    vocabCard.append(
      h(
        'div',
        { class: 'sheet-head' },
        h('div', { class: 'card-title', text: 'Vocabulary' }),
        h('span', {
          class: 'sub',
          id: 'vocab-effective',
          text: vocab ? `${(vocab.effective ?? []).length} terms biasing the transcriber` : '',
        })
      ),
      h('p', {
        class: 'rail-hint',
        style: 'padding:0 0 10px;max-width:70ch',
        text: 'Names, worlds and jargon the transcriber is nudged toward. Add the words it keeps getting wrong — this changes what future turns are heard as, and never rewrites one that already exists.',
      })
    );
    if (!vocab) {
      vocabCard.append(h('p', { class: 'rail-hint', id: 'vocab-loading', style: 'padding:0', text: 'Loading.' }));
      return;
    }

    const input = h('input', {
      class: 'input',
      id: 'vocab-add',
      type: 'text',
      placeholder: 'add a word or a name',
      disabled: vocabPending,
      onkeydown: (e) => {
        if (e.key !== 'Enter') return;
        e.preventDefault();
        void addTerm(input.value);
      },
    });

    const chips = h('div', { class: 'vocab-chips', id: 'vocab-user' });
    for (const term of vocab.user ?? []) {
      chips.append(
        h(
          'span',
          { class: 'vocab-chip', dataset: { term } },
          h('span', { class: 'vocab-chip-text', text: term }),
          h(
            'button',
            {
              class: 'vocab-chip-x',
              dataset: { removeTerm: term },
              'aria-label': `Remove ${term} from the glossary`,
              title: `Remove ${term}`,
              disabled: vocabPending,
              onclick: () => void setTerms((vocab.user ?? []).filter((t) => t !== term)),
            },
            '✕'
          )
        )
      );
    }
    if (!(vocab.user ?? []).length) {
      chips.append(h('span', { class: 'sub', id: 'vocab-user-empty', text: 'Nothing added yet.' }));
    }

    vocabCard.append(
      h('div', { class: 'vocab-group' }, h('div', { class: 'acc-group-title', text: 'Yours' }), chips),
      h('div', { class: 'vocab-add-row' }, input, h('button', {
        class: 'btn',
        id: 'vocab-add-go',
        disabled: vocabPending,
        onclick: () => void addTerm(input.value),
      }, 'Add'))
    );

    const auto = vocab.auto ?? {};
    const groups = [
      ['roster', 'From the roster', 'Display names of people who have been in the instance with you.'],
      ['worlds', 'From worlds', 'Names of worlds that have been named out loud or joined.'],
      ['corrections', 'From your corrections', 'Words you have retyped, which is the strongest signal there is.'],
    ];
    for (const [key, title, hint] of groups) {
      const terms = auto[key] ?? [];
      const row = h('div', { class: 'vocab-chips auto', dataset: { autoGroup: key } });
      for (const term of terms) row.append(h('span', { class: 'vocab-chip auto', dataset: { term } }, h('span', { class: 'vocab-chip-text', text: term })));
      vocabCard.append(
        h(
          'div',
          { class: 'vocab-group', dataset: { group: key } },
          h(
            'div',
            { class: 'acc-group-title' },
            title,
            h('span', { class: 'vocab-count', dataset: { count: key }, text: String(terms.length) })
          ),
          h('p', { class: 'rail-hint', style: 'padding:0 0 6px;max-width:70ch', text: hint }),
          terms.length ? row : h('span', { class: 'sub', text: 'Nothing here yet.' })
        )
      );
    }
  }

  async function addTerm(raw) {
    const term = String(raw ?? '').trim();
    if (!term) return;
    const have = vocab?.user ?? [];
    if (have.some((t) => t.toLowerCase() === term.toLowerCase())) {
      toast(`“${term}” is already in your glossary.`, '');
      return;
    }
    await setTerms([...have, term]);
  }

  /**
   * `vocab.set` REPLACES the glossary (PROTOCOL), so both add and remove go
   * through here with the whole list. Optimistic with a rollback, like every
   * other write in this app: the daemon confirms with a `vocab` broadcast and a
   * failure puts the old list back.
   */
  async function setTerms(terms) {
    const before = vocab;
    vocab = { ...vocab, user: terms };
    vocabPending = true;
    renderVocab();
    try {
      const out = await ask('vocab.set', { terms });
      vocab = out;
      store.vocab = out;
    } catch (e) {
      vocab = before;
      toast(`Could not change the vocabulary — ${e.message}`, 'error');
    } finally {
      vocabPending = false;
      renderVocab();
    }
  }

  // -- topics ---------------------------------------------------------------

  // -- worlds (0.10.0) ------------------------------------------------------
  //
  // Every place a conversation has happened, newest visit first. It renders
  // NOTHING when there is nothing — a machine with no VRChat on it has no
  // worlds and never will, and an empty state would be a permanent
  // advertisement for a game.
  //
  // A row leads to Search, filtered to that world. There is deliberately no
  // world page: what a person wants from "The Great Pug" is what was said
  // there, which is a search — a surface that already exists and can be
  // narrowed further.

  function renderWorlds() {
    clear(worldsCard);
    worldsCard.hidden = !worlds.length;
    if (!worlds.length) return;
    worldsCard.append(
      h(
        'div',
        { class: 'sheet-head' },
        h('div', { class: 'card-title', text: 'Worlds' }),
        h('span', {
          class: 'sub',
          id: 'worlds-sub',
          text: `${worlds.length} place${worlds.length === 1 ? '' : 's'} · from VRChat’s own log`,
        })
      )
    );
    const list = h('div', { class: 'world-list', id: 'world-list' });
    for (const w of worlds) {
      const named = !!w.name;
      const people = w.people ?? [];
      list.append(
        h(
          'button',
          {
            class: 'world-row',
            dataset: { world: w.world_id },
            title: named ? `${w.world_id} — search what was said here` : 'Search what was said here',
            onclick: () => ctx.searchWorld?.(w.world_id, w.name),
          },
          h(
            'span',
            { class: 'world-row-main' },
            h('span', { class: `world-name${named ? '' : ' unnamed'}`, text: named ? w.name : w.world_id }),
            h(
              'span',
              { class: 'world-people' },
              ...people.map((p) => {
                // `worlds.list` ships each person's highlight with them (v15),
                // which matters more here than almost anywhere: a world's
                // roster is a wall of chips and the colour is what makes one
                // of them findable.
                const pl = lookOn(p, p.speaker_id);
                return h(
                  'span',
                  { class: 'chip person', dataset: { sp: String(p.speaker_id) } },
                  h('span', { class: 'dot', style: `color:${pl.color}` }),
                  iconSpan(pl.icon),
                  p.label || speakerLabel(p.speaker_id)
                );
              }),
              people.length ? null : h('span', { class: 'sub', text: 'nobody identified here yet' })
            ),
            // Tier 2 output, and absent on a machine that has never run
            // enrichment — which is most of them, and is not an error.
            (w.topics ?? []).length
              ? h('span', { class: 'world-topics', text: (w.topics ?? []).join(' · ') })
              : null
          ),
          h(
            'span',
            { class: 'world-num' },
            String(w.visits ?? 0),
            h('small', { text: w.visits === 1 ? 'visit' : 'visits' })
          ),
          h(
            'span',
            { class: 'world-num' },
            w.last_ms ? fmtDate(new Date(w.last_ms).toISOString()) : '—',
            h('small', { text: 'last there' })
          )
        )
      );
    }
    worldsCard.append(list);
  }

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
            text: 'Conversations get a short label from the local model. It is off until you turn it on below — and once it is on it keeps working while you play, on as much of the machine as you give it.',
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
  let threadsPending = false;

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
      'aria-label': cfg.enabled ? 'Stop the local model' : 'Let the local model read, all the time',
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
            text: st.batch_total > 0 ? `reading conversation ${st.batch_done + 1} of ${st.batch_total}` : 'looking for the next conversation',
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

    // The honest copy, as flat statements. Not a paragraph anybody skims: each
    // line is a fact somebody might object to, answerable on its own.
    //
    // 0.7.2 rewrote the third line. It used to say "never while you are
    // gaming", which was true and was the wrong promise: a pass that waits for
    // an idle machine surfaces a promise hours after the evening it was made
    // in. What keeps it affordable is the jail — pinned cores, lowest priority
    // — and that is what the copy says now, next to the setting that sizes it.
    const threads = cfg.llm_threads ?? 4;
    enrichCard.append(
      h(
        'ul',
        { class: 'enrich-facts', id: 'enrich-facts' },
        fact(`A ${fmtGb(cfg.download_bytes)} language model, running on this machine.`),
        fact(
          `${threads} CPU core${threads === 1 ? '' : 's'} at the lowest priority — never the GPU unless you say so.`
        ),
        fact('It keeps working while you play. Pinned to those cores and running last in line, it cannot win a scheduling contest against your game.'),
        fact('Paused means paused. While capture is paused nothing is written down, and that includes this.'),
        fact('Off until you turn it on, and everything it finds is a suggestion you can dismiss.'),
        fact('Nothing leaves this machine. There is no network in this program except the one-time model download.')
      ),
      threadStepper(cfg),
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

  // -- translation (0.10.2) -------------------------------------------------
  //
  // Three controls, and they are three because the person asked three separate
  // questions in one breath: *everything but German and English should be
  // translated*, *the original should be subtext and the translation the main
  // thing*, and *translate it to English*. Each is a setting on its own here,
  // and none of them is inferred from another — a client that guessed "you
  // read German, so translate into German" would be answering a question
  // nobody asked.
  //
  // What this card deliberately does NOT have is a switch. Translation being
  // "on" is not a fourth fact: it is what having a target means, and `Off` is
  // the first option in the selector that sets it. Two controls for one state
  // is how a switch and a dropdown end up disagreeing.

  function renderTranslation() {
    clear(translateCard);
    const a = store.assist;
    const langs = a.languages.length ? a.languages : null;
    const live = store.conn.status === 'connected' && !translatePending;
    // A daemon that has never answered `assist.get` has given us no list, and
    // a selector built out of a list this file invented would offer languages
    // the daemon may refuse. Say so instead.
    const canSet = live && !!langs;

    translateCard.append(
      h(
        'div',
        { class: 'sheet-head' },
        h('div', { class: 'card-title', text: 'Translation' }),
        h('span', {
          class: 'sub',
          id: 'translate-sub',
          text: a.translate_to
            ? `into ${nameOf(a.translate_to)} · everything you do not read`
            : 'off — nothing is translated',
        })
      )
    );

    // 1. the target.
    const targetSelect = h(
      'select',
      {
        class: 'input',
        id: 'translate-target',
        disabled: !canSet,
        'aria-label': 'Translate turns into',
        onchange: (e) => void setAssist({ translate_to: e.target.value }),
      },
      h('option', { value: '', selected: !a.translate_to || undefined }, 'Off — do not translate'),
      ...(langs ?? []).map((l) =>
        h('option', { value: l.code, selected: l.code === a.translate_to || undefined }, l.name)
      )
    );

    // 2. the languages you read. Chips, because this is a set and not a
    //    choice, and a set of nineteen checkboxes is a form.
    const chips = h('div', { class: 'lang-chips', id: 'translate-read', dataset: { pending: String(translatePending) } });
    for (const l of langs ?? []) {
      const on = a.read_languages.includes(l.code);
      const locked = !!a.translate_to && l.code === a.translate_to;
      chips.append(
        h(
          'button',
          {
            class: `chip toggle-chip${on ? ' on' : ''}${locked ? ' locked' : ''}`,
            dataset: { lang: l.code, on: String(on) },
            role: 'switch',
            'aria-checked': String(on),
            disabled: !canSet || locked,
            title: locked
              ? `${l.name} is what you are translating INTO, so you read it by definition.`
              : on
                ? `${l.name} is not translated. Press to have it translated.`
                : `${l.name} is translated. Press to leave it alone.`,
            onclick: () => void toggleRead(l.code),
          },
          l.name
        )
      );
    }

    // 3. where the translation goes.
    const mode = (value, label, hint) =>
      h(
        'label',
        { class: `radio-row${a.translation_display === value ? ' on' : ''}` },
        h('input', {
          type: 'radio',
          name: 'translation-display',
          value,
          id: `translate-display-${value}`,
          checked: a.translation_display === value || undefined,
          disabled: !live,
          onchange: () => void setAssist({ translation_display: value }),
        }),
        h('span', {}, h('b', { text: label }), h('small', { text: hint }))
      );

    translateCard.append(
      h(
        'div',
        { class: 'enrich-tune', id: 'translate-target-row', dataset: { pending: String(translatePending) } },
        h(
          'span',
          { class: 'tune-label' },
          h('b', { text: 'Translate into' }),
          h('small', {
            text: 'The language you want to read. Turns already in it are never sent to the model.',
          })
        ),
        h('span', { class: 'spacer' }),
        targetSelect
      ),
      h(
        'div',
        { class: 'tune-block', id: 'translate-read-row' },
        h(
          'span',
          { class: 'tune-label' },
          h('b', { text: 'Languages you read' }),
          h('small', {
            text: 'Anything NOT on this list gets translated. The language you translate into is always on it.',
          })
        ),
        chips
      ),
      h(
        'div',
        { class: 'tune-block', id: 'translate-display-row' },
        h(
          'span',
          { class: 'tune-label' },
          h('b', { text: 'On a transcript row' }),
          h('small', { text: 'Both lines are always there. This is which one leads.' })
        ),
        h(
          'div',
          { class: 'radio-set', role: 'radiogroup', 'aria-label': 'Where the translation goes' },
          mode('main', 'Translation first', 'the original underneath, with its language code'),
          mode('under', 'Original first', 'the translation underneath — how 0.9.0 drew it')
        )
      ),
      h('p', {
        class: 'rail-hint',
        id: 'translate-note',
        style: 'padding:10px 0 0;max-width:70ch',
        text: 'The same local model that reads your conversations, one call per turn, after the enrichment queue has had its share. A turn it cannot translate honestly — it echoed the input, or answered in the wrong language — is left untranslated rather than guessed at, and the original is never replaced.',
      })
    );

    if (!langs) {
      translateCard.append(
        h('p', {
          class: 'mic-warn',
          id: 'translate-absent',
          text: 'This daemon is older than 0.10.2 and cannot be told what to translate from here. `translate_to` in config.toml still works.',
        })
      );
    }
  }

  // -- how it sounded (0.12.4) ----------------------------------------------
  //
  // Two things on one card, and they are deliberately not the same kind of
  // thing:
  //
  //   1. `mood_display` — YOUR choice about the page. Four states, and every
  //      one of them acts: `tags` puts a chip at the end of the row, `tint`
  //      colours the words, `both` does both, `off` does neither. A setting
  //      that is read and ignored is a blocker, not a footnote.
  //   2. What the DAEMON is willing to stand behind. That is not a choice and
  //      it is not on a switch: `status.mood.rendered` is a measurement
  //      (FINDINGS §42), and where it is false the card says so in the
  //      daemon's own words rather than quietly dropping half the feature.
  //
  // The card is honest about the third thing too: the pass has to be ON for
  // any of this to have anything to draw, and a person looking at four radio
  // buttons with an unmarked transcript behind them deserves to be told which
  // of the switches is the one that is off.

  function renderMood() {
    clear(moodCard);
    const st = store.status?.mood ?? null;
    const live = store.conn.status === 'connected' && !translatePending;
    const mode = MOOD_MODES.includes(store.assist.mood_display)
      ? store.assist.mood_display
      : 'tags';
    const rendered = st?.rendered === true;
    const on = st?.enabled === true;
    const available = st?.available === true;

    moodCard.append(
      h(
        'div',
        { class: 'sheet-head' },
        h('div', { class: 'card-title', text: 'How it sounded' }),
        h('span', {
          class: 'sub',
          id: 'mood-sub',
          text: !st
            ? 'this daemon is older than 0.12.4'
            : !on
              ? 'off — nothing is listened to'
              : !available
                ? 'the decoder is not installed'
                : `${st.read_total ?? 0} turns read · ${st.backlog ?? 0} to go`,
        })
      )
    );

    const option = (value, label, hint) =>
      h(
        'label',
        { class: `radio-row${mode === value ? ' on' : ''}` },
        h('input', {
          type: 'radio',
          name: 'mood-display',
          value,
          id: `mood-display-${value}`,
          checked: mode === value || undefined,
          disabled: !live,
          onchange: () => void setAssist({ mood_display: value }),
        }),
        h('span', {}, h('b', { text: label }), h('small', { text: hint }))
      );

    moodCard.append(
      h(
        'div',
        {
          class: 'tune-block',
          id: 'mood-display-row',
          dataset: { pending: String(translatePending) },
        },
        h(
          'span',
          { class: 'tune-label' },
          h('b', { text: 'On a transcript row' }),
          h('small', {
            text: 'Laughter and music are marks on the AUDIO, not words anybody said. Nothing here changes the transcript.',
          })
        ),
        h(
          'div',
          { class: 'radio-set', role: 'radiogroup', 'aria-label': 'How a mood or an event shows' },
          option('tags', 'A tag at the end', 'a small chip saying what was heard'),
          option(
            'tint',
            'Colour the words',
            'the mood becomes the row’s colour — laughter and music stay chips, because a sound has no colour'
          ),
          option('both', 'Both', 'the mood as a chip AND as the row’s colour'),
          option('off', 'Neither', 'the tags are still read and stored, just not drawn')
        )
      )
    );

    // The measurement, in the daemon's own sentence. Present whenever the mood
    // half is being withheld, in EVERY display mode — including `off`, where
    // nothing is drawn anyway: a person who turns the setting on tomorrow
    // should not have to discover this then.
    if (st && !rendered) {
      moodCard.append(
        h('p', {
          class: 'rail-hint',
          id: 'mood-why',
          style: 'padding:10px 0 0;max-width:70ch',
          text: st.why || 'The mood tag is stored but not shown on this daemon.',
        })
      );
    }

    // …and what is missing, if anything is. One line, and it names the switch
    // rather than describing the feeling of it being off.
    if (st && !on) {
      moodCard.append(
        h('p', {
          class: 'mic-warn',
          id: 'mood-off',
          text: 'Nothing is being listened to: set `enabled = true` under `[mood]` in config.toml. It runs in the same overnight window as the night shift, on the same niced cores, and never touches the GPU.',
        })
      );
    } else if (st && !available) {
      moodCard.append(
        h('p', {
          class: 'mic-warn',
          id: 'mood-absent',
          text: st.how || 'The decoder this needs is not installed.',
        })
      );
    }
  }

  function nameOf(code) {
    return store.assist.languages.find((l) => l.code === code)?.name ?? code;
  }

  /** Add or remove one language from the read set. */
  function toggleRead(code) {
    const now = store.assist.read_languages;
    const next = now.includes(code) ? now.filter((c) => c !== code) : [...now, code];
    return setAssist({ read_languages: next });
  }

  /**
   * The one write. Optimistic with rollback, like every other control here: the
   * value moves the instant a person presses and goes back if the daemon
   * refuses, rather than sitting still for a round trip.
   *
   * The daemon's reply is applied over the optimistic guess rather than trusted
   * to match it — `read_languages` comes back with the target folded in, and a
   * client that kept its own guess would draw a chip the daemon has switched on.
   */
  async function setAssist(patch) {
    const before = { ...store.assist };
    applyAssist(patch);
    translatePending = true;
    renderTranslation();
    renderMood();
    try {
      applyAssist(await ask('assist.set', patch));
    } catch (e) {
      store.assist = before;
      toast(`Could not change that — ${e.message}`, 'error');
    } finally {
      translatePending = false;
      renderTranslation();
      renderMood();
      // The transcript is not repainted from here. The daemon publishes an
      // `assist` event for a change made anywhere, this window is subscribed
      // to it like any other, and that is the one path — a second path would
      // be a second chance to disagree with the daemon.
    }
  }

  /// How much of the machine the model may use — the setting that replaced
  /// standing down while you played.
  ///
  /// A stepper rather than a slider: the useful range is small integers and
  /// every one of them is a whole core, so there is nothing to interpolate. The
  /// bounds come from the daemon (`llm_threads_min`/`max`), not from a number
  /// in this file that could drift away from what `graph.set` will accept.
  function threadStepper(cfg) {
    const min = cfg.llm_threads_min ?? 1;
    const max = cfg.llm_threads_max ?? 32;
    const value = Math.min(max, Math.max(min, cfg.llm_threads ?? 4));
    const step = (delta, label, title) =>
      h(
        'button',
        {
          class: 'btn small step',
          dataset: { step: String(delta) },
          'aria-label': title,
          title,
          disabled:
            threadsPending ||
            store.conn.status !== 'connected' ||
            value + delta < min ||
            value + delta > max,
          onclick: () => void setThreads(value + delta),
        },
        label
      );
    return h(
      'div',
      {
        class: 'enrich-tune',
        id: 'enrich-threads',
        // The optimistic window, made visible. The value moves the instant a
        // person presses, and the buttons are dead until the daemon answers —
        // a driver that pressed into that gap would be pressing nothing, so it
        // has to be able to see it, exactly as a commitment row exposes its own.
        dataset: { pending: String(threadsPending) },
      },
      h(
        'span',
        { class: 'tune-label' },
        h('b', { text: 'Model threads' }),
        h('small', {
          id: 'enrich-threads-hint',
          text: `${min}–${max} of this machine's cores. Applies to the next conversation it reads.`,
        })
      ),
      h('span', { class: 'spacer' }),
      h(
        'span',
        {
          class: 'tune-stepper',
          role: 'group',
          'aria-label': 'How many CPU cores the local model may use',
        },
        step(-1, '−', 'One core fewer'),
        h('output', {
          class: 'tune-value',
          id: 'enrich-threads-value',
          'aria-live': 'polite',
          text: String(value),
        }),
        step(1, '+', 'One core more')
      )
    );
  }

  async function setThreads(next) {
    const before = summary?.config?.llm_threads;
    threadsPending = true;
    // Optimistic, like every other control in this app: the daemon answers
    // with what is now true and a failure puts the old number back.
    if (summary?.config) summary.config.llm_threads = next;
    renderEnrichment();
    try {
      const out = await ask('graph.set', { llm_threads: next });
      summary = { ...(summary ?? {}), config: out.config, enrichment: out.enrichment };
      // The shared store carries the config for anything outside this view;
      // leaving it stale would make the app disagree with itself about a
      // number the daemon has already accepted.
      store.graph = { ...store.graph, config: out.config, enrichment: out.enrichment };
      // No toast on success, unlike the switch. The number is right under the
      // cursor and the line above it moves with it, so a notification would
      // only be a second copy of what a person is already looking at — and
      // stepping from four to eight would raise four of them.
    } catch (e) {
      if (summary?.config) summary.config.llm_threads = before;
      toast(`Could not change that — ${e.message}`, 'error');
    } finally {
      threadsPending = false;
      renderEnrichment();
    }
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

  /**
   * The 0.8.0 slices, each failing on its own.
   *
   * Deliberately not in the `Promise.all` below: a daemon older than 0.8.0
   * answers `unknown_method` to all three, and folding them into the same await
   * would make one missing method blank the commitments too. Each card decides
   * what to say about its own absence, which is the same rule the resync in
   * store.js follows for the same reason.
   */
  async function loadAccuracyRound() {
    await Promise.all([
      ask('notes.list', { limit: 100 })
        .then((r) => {
          notes = r.notes ?? [];
        })
        .catch(() => {
          notes = [];
        })
        .then(renderNotes),
      ask('accuracy.summary')
        .then((r) => {
          accuracy = r;
        })
        .catch(() => {
          accuracy = null;
        })
        .then(renderAccuracy),
      ask('vocab.get')
        .then((r) => {
          vocab = r;
          store.vocab = r;
        })
        .catch(() => {
          vocab = null;
        })
        .then(renderVocab),
      // 0.9.0. Its own slice for the same reason the three above have theirs:
      // a daemon older than 0.9.0 answers `unknown_method`, and one missing
      // method must not blank the cards next to it.
      ask('digest.list', { limit: 40 })
        .then((r) => {
          digests = r.digests ?? [];
        })
        .catch(() => {
          digests = [];
        })
        .then(renderDigests),
      // 0.10.2. Its own slice for the same reason every one above it has one:
      // a daemon older than 0.10.2 answers `unknown_method` here, and the card
      // says so rather than blanking the enrichment card beside it. This is
      // also what puts the LANGUAGE LIST in the model — the resync fetches it
      // too, but a view mounted between resyncs would otherwise draw a
      // selector with nothing in it.
      ask('assist.get')
        .then((r) => {
          applyAssist(r);
        })
        .catch(() => {})
        .then(renderTranslation),
      // 0.10.0. Its own slice for the same reason every one above it has one.
      ask('worlds.list', { limit: 12 })
        .then((r) => {
          worlds = r.worlds ?? [];
        })
        .catch(() => {
          worlds = [];
        })
        .then(renderWorlds),
    ]);
  }

  async function load() {
    void loadAccuracyRound();
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
  renderDigests();
  renderCommitments();
  renderNotes();
  renderAccuracy();
  renderVocab();
  renderWorlds();
  renderTopics();
  renderEnrichment();
  renderTranslation();
  renderMood();
  void load();

  return {
    update(change) {
      // A note arriving live goes to the top of the list — it is the newest
      // thing you said to yourself, and it is the reason you are looking.
      if (change?.note) {
        const known = notes.find((n) => n.id === change.note.id);
        if (known) Object.assign(known, change.note);
        else notes.unshift(change.note);
        renderNotes();
      }
      // 0.9.0: a conversation the model has just read. Only ever new — one
      // digest per conversation — so this unshifts rather than reconciling.
      if (change?.digest) {
        if (!digests.some((d) => d.thread_id === change.digest.thread_id)) {
          digests.unshift(change.digest);
        }
        renderDigests();
      }
      // A reminder came round. The note itself arrives beside it as a `note`
      // event carrying `fired`, so the row repaints from that; this is only
      // here so the card scrolls to it when the view is already open.
      if (change?.reminder) focusNote(change.reminder.note_id);
      // The glossary changed somewhere — here, the CLI, another window. It is a
      // broadcast, so this repaints rather than re-queries.
      if (change?.vocab) {
        vocab = change.vocab;
        renderVocab();
      }
      // A correction anywhere moves the accuracy figures, and a corrected
      // segment arrives as an ordinary `segment` update. Re-asking is one small
      // query and is always right; guessing at the arithmetic would not be.
      if (change?.updated?.some((s) => s.corrected)) {
        ask('accuracy.summary')
          .then((a) => {
            accuracy = a;
            renderAccuracy();
          })
          .catch(() => {});
      }
      // The worker's state arrives on the status topic, so the card follows a
      // batch without this view polling anything.
      if (change?.graph || change?.status) {
        if (change.graph) summary = { ...(summary ?? {}), enrichment: change.graph };
        renderEnrichment();
      }
      // The translation settings moved — here, in another window, or in the
      // config file. `status` carries them too, so a window that missed the
      // event converges on the next poll (0.10.2).
      if (change?.assist || change?.status) {
        renderTranslation();
        // `status` as well as `assist`: the mood card prints whether the pass
        // is on, whether the model is installed and whether the measurement
        // lets the mood half be drawn, and all three live on `status.mood`.
        renderMood();
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
    // 0.9.0: a reminder, from a toast or from an OS notification, lands on its
    // row. Returns null when the note is not in the list, which is how the
    // controller knows to say so rather than scrolling to nothing.
    focusNote,
  };

  /**
   * Land on one note's row.
   *
   * `pendingFocus` is what makes this work when the view has only just been
   * mounted — a reminder clicked from the transcript switches to Memory, and
   * the notes are still one query away when this is called. Remembering the id
   * and applying it after the next paint is the difference between a
   * notification that lands on the row and one that lands on an empty card.
   */
  function focusNote(noteId) {
    pendingFocus = noteId;
    return applyFocus();
  }

  function applyFocus() {
    if (pendingFocus == null) return null;
    const row = notesCard.querySelector(`[data-note="${CSS.escape(String(pendingFocus))}"]`);
    if (!row) return null;
    const id = pendingFocus;
    pendingFocus = null;
    row.scrollIntoView({ block: 'center', behavior: 'smooth' });
    // Restarted rather than merely added: a second reminder for the same row
    // while the first pulse is still running must be visible.
    row.classList.remove('flash');
    void row.offsetWidth;
    row.classList.add('flash');
    setTimeout(() => row.classList.remove('flash'), 1600);
    return id;
  }
}

/// GB as the rest of this project says GB — decimal, the unit a download size
/// is quoted in and the unit `models fetch` and GRAPH.md both use. Dividing by
/// 2^30 instead made this card say "1.8 GB" for a 1,929,903,264-byte file that
/// every other surface calls 1.9 GB, which is the kind of small lie that makes
/// a person doubt the large truths next to it.
function fmtGb(bytes) {
  const gb = (Number(bytes) || 1_946_604_700) / 1_000_000_000;
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
