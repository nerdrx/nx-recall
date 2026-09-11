import { searchableSpeakerSelect } from '../lib/searchable-select.js';
// Live transcript — the view the app is for. Segments stream in as they are
// transcribed; anything the pipeline itself refused to identify is visibly
// muted and carries a "?" that says why; clicking any segment opens the
// reassign/correct sheet (PROTOCOL segments.reassign / segments.correct).
//
// 0.7.4 made it the whole archive rather than the last 600 rows. Scrolling
// near the top loads the page before, prepended without moving what you are
// reading, until a marker says you have reached the first thing ever captured;
// a date picker jumps to a day directly. The window model behind both lives in
// lib/store.js, and its one rule is that trimming never happens in the
// direction you are looking.

import { h, clear, fmtClock, fmtDay, fmtDayLabel } from '../lib/dom.js';
import { mountConversationFind } from '../lib/conversation-find.js';
import { createSpeakerPicker } from '../lib/speaker-picker.js';
import { saveMomentSheet } from '../lib/saved.js';
import {
  store,
  speakerLabel,
  segmentSpeakerLabel,
  isUncertain,
  uncertainReason,
  // 0.7.7: the "?" now also marks a turn whose WORDS came from somewhere other
  // than the primary model, which is a different doubt from a doubted name.
  hasMark,
  languageNote,
  // 0.8.0: a third doubt, and it is about neither the name nor which model
  // wrote the words down — it is two decoders reading the same seconds and
  // disagreeing. Its own mark, because it is its own claim.
  isShaky,
  SHAKY_NOTE,
  textViaNote,
  isYou,
  ask,
  // 0.11.0: the turn somebody is still saying. `livePartial` applies the
  // staleness rule, so this view and the captions bar can never disagree about
  // whether anybody is talking.
  livePartial,
  PARTIAL_STALE_MS,
  // 0.12.4: the same rule for a row that is GROWING rather than being replaced.
  SLICE_STALE_MS,
  setFollowing,
  loadOlderPage,
  replaceSegments,
  followTail,
  HARD_MAX,
} from '../lib/store.js';
// 0.12.0 — per-person highlights. `look()` answers "what colour and what icon
// does this row wear", preferring the store and falling back to the row's own
// `speaker_colour`/`speaker_icon` for a voice this client has not listed yet;
// `markRow` is the quiet accent on a highlighted row. All three live in one
// module so the transcript and search cannot disagree about them.
import { look, lookOf, iconSpan, markRow, highlightPicker } from './highlight.js';
import { separatorWalker } from '../lib/seams.js';
import { moodChips, shakyMark, translationCell } from '../lib/marks.js';
import { openSheet, toast } from '../lib/sheets.js';
import { play, stop as stopPreview, isActive, onPlayback, noAudioHint } from '../lib/preview.js';
// Conversation replay (0.9.2). The engine is in lib/replay.js and holds no DOM;
// this view owns the bar it draws and the row it lights.
import * as replay from '../lib/replay.js';

export const id = 'transcript';

/** How close to the top of the scroller starts loading the page before. */
const LOAD_MARGIN = 320;
/** How close to the bottom counts as "back on the tail". */
const TAIL_MARGIN = 8;

/**
 * One render pass's conversation labels. Index participants lazily, only when
 * that pass actually draws a conversation seam. Scanning the entire window
 * for every seam makes a history repaint quadratic in its row count.
 * Speaker labels remain live; only membership/order is shared within a pass.
 */
export function threadNameResolver(segments, label) {
  let participants = null;
  return (threadId) => {
    if (!participants) {
      participants = new Map();
      for (const segment of segments) {
        if (segment.thread == null || segment.speaker == null) continue;
        let speakers = participants.get(segment.thread);
        if (!speakers) participants.set(segment.thread, speakers = new Set());
        speakers.add(segment.speaker);
      }
    }
    return Array.from(participants.get(threadId) ?? [], label);
  };
}

export function mount(root, ctx) {
  let filterSpeaker = null;
  let lastYou = store.mic.you_speaker;
  let loadingOlder = false;
  // Programmatic scrolls (scrollToEnd, scrollIntoView, the anchoring below)
  // fire the same scroll event a finger does. Acting on those would flip the
  // follow state under the user for reasons they did not cause, so the handler
  // stands down for a beat after the view moves the scroller itself.
  let ignoreScrollUntil = 0;
  // The conversation the view was sent to, if any. Subtle by design: it marks a
  // span rather than hiding everything else, because the point of arriving at a
  // conversation is to see it in the evening it happened in.
  let highlightThread = null;
  // The turn conversation replay is on, so a repaint of the rows under a
  // running replay does not lose the lit row.
  let replayingSeg = null;
  // Whether this replay has already put its first turn on screen. See
  // `paintReplay`: arriving scrolls differently from following.
  let replayArrived = false;

  const following = () => store.window.following;
  /** The one timer this view owns: see `renderPartial`. */
  let staleTimer = null;

  const list = h('div', { class: 'seg-list', id: 'seg-list' });
  // 0.11.0 — the live tail. Its own container, AFTER the list rather than
  // inside it, and that is not tidiness: `.seg:last-of-type`, the trim loop and
  // the separator walker all reason about the last child of `#seg-list`, and a
  // provisional row in there would quietly become "the last segment" to every
  // one of them. It is not a segment. It has no id, it is never counted, it
  // never reaches history, and it is gone the instant the real row lands.
  const tail = h('div', { class: 'seg-tail', id: 'seg-tail', 'aria-live': 'polite' });
  const card = h('div', { class: 'card' }, list, tail);
  const body = h('div', { class: 'view-body view-enter', id: 'transcript-body' }, card);

  // Capture state belongs where the words are, not only in the rail: this is
  // the surface you are reading when you wonder why nothing new has appeared.
  const liveChip = h('span', { class: 'chip live', id: 'live-chip' });
  const countSub = h('span', { class: 'sub', id: 'seg-count' });

  const followBtn = h(
    'button',
    {
      class: 'btn small',
      id: 'follow-btn',
      'aria-pressed': 'true',
      onclick: () => setFollow(!following()),
    },
    'Following'
  );

  /**
   * The one place the follow state changes.
   *
   * Turning it ON usually just collapses the window to the newest
   * MAX_SEGMENTS, which is local and instant. The exception is a DETACHED
   * window — the date picker rebuilt it on another day, so the tail is not in
   * it to collapse to and collapsing would leave you following July. That one
   * case costs a query, and it is the only correct answer.
   */
  function setFollow(on, { repaint = true } = {}) {
    const was = following();
    if (on && store.window.detached) {
      paintFollowBtn(true);
      followTail()
        .then(() => {
          paintFollowBtn();
          refreshFilterOptions();
          renderAll({ scroll: 'end' });
        })
        .catch((e) => {
          paintFollowBtn();
          toast(`Could not get back to the live transcript — ${e.message}`, 'error');
        });
      return true;
    }
    setFollowing(on);
    paintFollowBtn();
    if (on && repaint) renderAll({ scroll: 'end' });
    else if (was !== following() && repaint) updateCount();
    return true;
  }

  function paintFollowBtn(pending = false) {
    const on = pending || following();
    followBtn.setAttribute('aria-pressed', String(on));
    followBtn.textContent = on ? 'Following' : 'Follow';
    followBtn.title = on
      ? 'Following the live tail. The window keeps the newest 600 rows.'
      : 'Reading history. Nothing is trimmed while you are up here — press to go back to the live tail.';
  }

  // The far half of the story (design point 5). Scrollback answers "what were
  // we saying ten minutes ago"; this answers "what were we saying in July",
  // which no amount of scrolling is a reasonable way to reach. Native input:
  // `color-scheme` already draws its calendar glyph correctly on both grounds.
  const dateInput = h('input', {
    class: 'input',
    id: 'transcript-date',
    type: 'date',
    title: 'Jump to a day',
    'aria-label': 'Jump to a day',
    onchange: (e) => jumpToDay(e.target.value),
  });

  const speakerFilter = h(
    'select',
    {
      class: 'input',
      id: 'transcript-filter',
      onchange: (e) => {
        const v = e.target.value;
        filterSpeaker = v === '' ? null : Number(v);
        repaint();
      },
    },
    h('option', { value: '' }, 'Everyone')
  );

  const speakerPicker = searchableSpeakerSelect(speakerFilter, { label: 'Find a speaker to filter the transcript' });

  /** Drive the existing Everyone/speaker filter from anywhere else in the UI. */
  function setFilter(spId) {
    filterSpeaker = spId ?? null;
    speakerPicker.reset();
    speakerFilter.value = spId == null ? '' : String(spId);
    repaint();
  }

  const head = h(
    'div',
    { class: 'view-head' },
    h('div', {}, h('h1', { text: 'Live transcript' }), countSub),
    h('div', { class: 'spacer' }),
    liveChip,
    dateInput,
    speakerPicker.element,
    followBtn
  );

  // The quiet inline loader, and the two notices that sit above the first row.
  // All three live outside the row list so a prepend never has to step over
  // them, and all three are hidden unless they have something true to say.
  const loader = h(
    'div',
    { class: 'scroll-loader', id: 'older-loader', hidden: true },
    h('span', { class: 'spinner' }),
    h('span', { text: 'loading earlier…' })
  );
  const beginMark = h('div', { class: 'begin-sep', id: 'transcript-beginning', hidden: true });
  const capNote = h('div', { class: 'window-note', id: 'window-note', hidden: true });
  const top = h('div', { class: 'seg-top' }, capNote, loader, beginMark);
  card.prepend(top);

  // -------------------------------------------------------------------------
  // the replay bar (0.9.2)
  //
  // Pinned between the header and the rows rather than floating over them: the
  // whole point of replay is that you are READING along, and a bar over the
  // bottom of the transcript would cover the words it is playing. It exists
  // only while a conversation is playing and takes no space otherwise.
  // -------------------------------------------------------------------------

  const ICON_PLAY = '<path d="M7 4l12 8-12 8z"></path>';
  const ICON_PAUSE = '<rect x="6" y="5" width="4" height="14"></rect><rect x="14" y="5" width="4" height="14"></rect>';

  const rIco = h('span', { class: 'replay-ico', 'aria-hidden': 'true' });
  const rPlay = h(
    'button',
    { class: 'btn small primary replay-play', id: 'replay-play', onclick: () => replay.toggle() },
    rIco
  );
  const rPrev = h(
    'button',
    { class: 'btn small', id: 'replay-prev', title: 'The turn before', 'aria-label': 'Previous turn', onclick: () => replay.prev() },
    '‹'
  );
  const rNext = h(
    'button',
    { class: 'btn small', id: 'replay-next', title: 'The next turn', 'aria-label': 'Next turn', onclick: () => replay.next() },
    '›'
  );
  const rRate = h('button', {
    class: 'btn small replay-rate',
    id: 'replay-rate',
    title: 'Playback speed',
    onclick: () => replay.cycleRate(),
  });
  const rWho = h('span', { class: 'replay-who', id: 'replay-who' });
  const rClock = h('span', { class: 'replay-clock', id: 'replay-clock' });
  const rScrub = h('div', {
    class: 'replay-scrub',
    id: 'replay-scrub',
    role: 'group',
    'aria-label': 'The turns in this conversation',
  });
  // Rule 1, said once for the whole conversation rather than on every row it
  // is true of. Hidden entirely when every turn still has its audio.
  const rNote = h('div', { class: 'replay-note', id: 'replay-note', hidden: true });
  const rClose = h('button', {
    class: 'btn small',
    id: 'replay-close',
    title: 'Stop replaying',
    'aria-label': 'Stop replaying',
    onclick: () => replay.close(),
  });
  rClose.textContent = '✕';

  const replayBar = h(
    'div',
    {
      class: 'replay-bar',
      id: 'replay-bar',
      hidden: true,
      tabindex: '0',
      role: 'group',
      'aria-label': 'Conversation replay',
      // Space plays and pauses, the arrows step. Only while the bar itself has
      // focus — the transcript's rows are buttons and space means "open this
      // one" on them, and stealing that would be worse than no shortcut.
      onkeydown: (e) => {
        if (e.target !== replayBar) return;
        if (e.key === ' ' || e.key === 'Spacebar') {
          e.preventDefault();
          replay.toggle();
        } else if (e.key === 'ArrowRight' || e.key === 'ArrowDown') {
          e.preventDefault();
          replay.next();
        } else if (e.key === 'ArrowLeft' || e.key === 'ArrowUp') {
          e.preventDefault();
          replay.prev();
        } else if (e.key === 'Escape') {
          e.preventDefault();
          replay.close();
        }
      },
    },
    h('div', { class: 'replay-controls' }, rPrev, rPlay, rNext, h('span', { class: 'replay-label' }, rWho, rClock), h('span', { class: 'spacer' }), rRate, h('button', { class: 'btn small', id: 'replay-save-moment', onclick: () => { const rs = replay.replayState(); const turn = rs.turns?.[rs.index]; if (turn) void saveMomentSheet([turn.id]); } }, 'Save moment'), rClose),
    rScrub,
    rNote
  );

  const conversationFind = mountConversationFind({ request: ask, jump: ctx.jumpToSegment,
    reveal(id) {
      const row = list.querySelector(`.seg[data-seg="${id}"]`);
      if (!row) return false;
      leaveTheTail();
      for (const hit of list.querySelectorAll('.seg.hit')) hit.classList.remove('hit');
      row.classList.add('hit'); reveal(row); return true;
    },
  });
  root.append(head, conversationFind.panel, replayBar, body);
  body.addEventListener('scroll', onScroll, { passive: true });

  // -- rendering ------------------------------------------------------------

  function visible() {
    return filterSpeaker == null ? store.segments : store.segments.filter((s) => s.speaker === filterSpeaker);
  }

  function renderAll({ scroll = 'end' } = {}) {
    clear(list);
    const rows = visible();
    if (!rows.length) {
      list.append(
        h(
          'div',
          { class: 'empty' },
          h('b', { text: store.conn.status === 'connected' ? 'Nothing captured yet' : 'Waiting for the daemon' }),
          h('p', {
            text:
              store.conn.status === 'connected'
                ? 'Allowed sources are being listened to. Speech shows up here within a couple of seconds of being said.'
                : 'recalld is not answering on its socket. The transcript fills in as soon as it is back.',
          })
        )
      );
      renderPartial();
      updateCount();
      return;
    }
    const walk = nodeWalker();
    for (const seg of rows) {
      for (const sep of walk(seg)) list.append(sep);
      list.append(segRow(seg));
    }
    renderPartial();
    updateCount();
    if (scroll === 'end') scrollToEnd(true);
    else if (scroll === 'top') scrollTo(0);
  }

  /**
   * A full repaint that lands where the reader already is. Following means the
   * tail; browsing means "do not move" — a filter change or a relabel must not
   * throw somebody who is reading July back to tonight.
   */
  function repaint() {
    const at = body.scrollTop;
    renderAll({ scroll: following() ? 'end' : 'none' });
    if (!following()) body.scrollTop = Math.min(at, body.scrollHeight);
  }

  /**
   * The separator rules (lib/seams.js), rendered. One walker instance carries
   * the state down a pass, which is what lets a PREPENDED page be separated by
   * the same rules as a repaint and then hand its state to the seam.
   */
  function nodeWalker(state, names = threadNameResolver(store.segments, speakerLabel)) {
    const walk = separatorWalker(state);
    return (seg) => {
      const at = walk(seg);
      const out = [];
      if (at.day) out.push(h('div', { class: 'day-sep', text: fmtDayLabel(seg.t_ms) }));
      if (at.thread) out.push(threadSep(seg, names(seg.thread)));
      return out;
    };
  }

  /**
   * The hairline where one conversation becomes another (docs/GRAPH.md).
   *
   * Only where threads exist: a row the daemon never threaded — anything older
   * than 0.6.2 — carries no id and renders exactly as it always did, which is
   * what keeps this additive rather than a change to every transcript ever
   * captured. The names are the people in the new conversation, because "a
   * conversation started" is only useful if it says whose.
   */
  function threadSep(seg, names) {
    return h(
      'div',
      { class: 'thread-sep', dataset: { thread: String(seg.thread) } },
      h('span', { class: 'thread-sep-label', text: names.length ? names.join(' · ') : 'another conversation' }),
      // The boundary is the one place in the transcript that names a whole
      // conversation, so it is where "play it back" belongs. Quiet until the
      // hairline is hovered or focused: a row of buttons down the page would
      // be louder than the separators themselves.
      replayButton(seg.thread, { className: 'thread-sep-replay' }),
      h('button', { class: 'btn small conversation-find-open', text: 'Find', 'aria-label': 'Find in this conversation', onclick: e => conversationFind.open(seg.thread, e.currentTarget) })
    );
  }

  /**
   * The one affordance, wherever a conversation is named. Same element and same
   * sentence from a separator, a digest, a person page and a search hit — four
   * routes to one thing, which is what stops it reading as four features.
   */
  function replayButton(threadId, { className = '' } = {}) {
    return h(
      'button',
      {
        class: `btn small replay-start ${className}`.trim(),
        dataset: { replay: String(threadId) },
        title: 'Play this conversation back, turn by turn',
        'aria-label': 'Replay this conversation',
        onclick: (e) => {
          e.stopPropagation();
          void startReplay(threadId);
        },
      },
      'Replay'
    );
  }

  function segRow(seg, isNew = false) {
    const uncertain = isUncertain(seg);
    const shaky = isShaky(seg);
    // The user's own voice, off their own microphone. The label did not come
    // from a match, it came from where the audio arrived — so it is the one
    // name in the transcript that is never a guess. Marked, not shouted: a
    // filled dot and an underline on the name, no layout change, no colour of
    // its own beyond the accent.
    const mine = isYou(seg.speaker);
    // The colour is `speakerColor(seg.speaker)` byte for byte until somebody
    // highlights this voice — see lib/store.js `speakerNameColor`. The icon is
    // '' for everybody else, and an '' icon renders no element at all.
    const { color, hl, icon } = look(seg);
    const row = h('div', {
      class: `seg${uncertain ? ' uncertain' : ''}${shaky ? ' shaky' : ''}${mine ? ' you' : ''}${isNew ? ' new' : ''}${seg.corrected ? ' corrected' : ''}${
        highlightThread != null && seg.thread === highlightThread ? ' in-thread' : ''
      }${replayingSeg === seg.id ? ' replaying' : ''}`,
      dataset: { seg: String(seg.id), ...(seg.thread != null ? { thread: String(seg.thread) } : {}) },
      role: 'button',
      tabindex: '0',
      title: 'Click to reassign or correct this segment',
    });
    // A nameless segment says WHY it is nameless (store.segmentSpeakerLabel):
    // "several voices" and "unknown voice" are different problems with
    // different fixes, and neither of them is a name — hence the italic.
    const nm = h('span', {
      class: `nm${seg.speaker == null ? ' reasoned' : ''}`,
      text: segmentSpeakerLabel(seg),
      style: seg.speaker == null ? '' : `color:${color}`,
      ...(seg.speaker != null
        ? {
            title: mine
              ? `Show only ${speakerLabel(seg.speaker)} — your own voice, from your microphone`
              : `Show only ${speakerLabel(seg.speaker)}`,
          }
        : {}),
    });
    if (seg.speaker != null) {
      // The name filters; the rest of the row still opens the sheet. Reading a
      // transcript and wanting only one person's lines is one click, not a trip
      // to the dropdown.
      nm.addEventListener('click', (e) => {
        e.stopPropagation();
        setFilter(seg.speaker);
      });
    }
    const who = h(
      'span',
      { class: 'who', ...(seg.speaker != null ? { dataset: { sp: String(seg.speaker) } } : {}) },
      h('span', { class: 'dot', style: `color:${color}` }),
      // Between the dot and the name, and absent entirely without a highlight:
      // the name column is a fixed 148px (styles.css) and an icon that appeared
      // on every row would spend a fifth of it on nothing.
      iconSpan(icon),
      nm
    );
    // The row accent. Guarded on `uncertain` on purpose: styles.css already
    // overrides the inline speaker colour on a doubted row, and a row whose
    // speaker is a guess must not be painted as somebody's confident mark.
    markRow(row, hl, { uncertain });
    // 0.9.0: a turn in a language you do not read, in one you do. Both lines
    // are always on the row — the transcript is a record and the original never
    // leaves it — and which of the two LEADS is `[assist] translation_display`
    // (0.10.2). The cell itself is in lib/marks.js, because a search hit has to
    // draw the identical thing.
    row.append(
      h('span', { class: 't', text: fmtClock(seg.t_ms) }),
      who,
      translationCell(seg),
      h(
        'span',
        { class: 'meta' },
        // 0.12.4: what was on the clip besides the words, and — where the
        // measurement earned it — how it sounded. First in the meta column,
        // because it is about the turn itself rather than about where the turn
        // came from or how much the app trusts it. Absent on a row with
        // nothing to say, which is most of them.
        moodChips(seg),
        seg.source === 'Discord' ? h('span', { class: 'chip', text: 'discord' }) : null,
        // Two decoders, one disagreement (0.8.0). Deliberately NOT folded into
        // the "?": that mark answers "who said this and which model wrote it",
        // and this one answers "is this even what was said". Same size, own
        // glyph, own sentence.
        shaky ? shakyMark() : null,
        // The "?" is now about two things: a name in doubt, and words that
        // did not come from the primary model (0.7.7). Either earns the mark.
        hasMark(seg) ? h('span', { class: 'qmark', text: '?', title: uncertainReason(seg) }) : null
      )
    );
    const open = () => openSegmentSheet(seg, ctx);
    row.addEventListener('click', open);
    row.addEventListener('keydown', (e) => {
      if (e.key === 'Enter' || e.key === ' ') {
        e.preventDefault();
        open();
      }
    });
    return row;
  }

  // ---- 0.11.0, the live tail ----------------------------------------------

  /**
   * Draw, update or remove the provisional row for the turn being said now.
   *
   * Four reasons there is nothing to draw, and all four are the same reason —
   * this is the LIVE tail: the reader is up in history, the window is somewhere
   * else entirely, a speaker filter is on and the guess does not match it, or
   * nobody is talking. In every one of those the honest thing is an absent row
   * rather than a row somewhere it does not belong.
   */
  function renderPartial() {
    clearTimeout(staleTimer);
    staleTimer = null;
    const p = following() && !store.window.detached ? livePartial() : null;
    const show = p && (filterSpeaker == null || p.speaker === filterSpeaker);
    if (!show) {
      if (tail.firstChild) {
        const stick = following() && nearBottom();
        clear(tail);
        if (stick) scrollToEnd();
      }
      return;
    }
    // Nothing else in this view is on a clock, and this is the one thing that
    // has to come off the screen without an event: a pause, a discarded turn
    // after an audio gap, or a daemon that went away all end a turn silently.
    // One timer, only while a row is up, aimed at the exact moment it expires.
    // 0.12.5: a growing row is allowed to sit much longer between slices than
    // a partial is between decodes, so the timer is aimed at whichever rule
    // this row is under.
    const staleMs = p.growing ? SLICE_STALE_MS : PARTIAL_STALE_MS;
    staleTimer = setTimeout(renderPartial, Math.max(250, staleMs - (Date.now() - p.at) + 100));
    const stick = following() && nearBottom();
    // Rebuilt rather than patched: it is one row of four spans and it changes
    // once a second, and a patch would have to know which of them moved.
    clear(tail);
    tail.append(partialRow(p));
    if (stick) scrollToEnd();
  }

  /**
   * The provisional row itself. Deliberately the same shape as `segRow` — the
   * same four columns in the same order — so the final row replaces it without
   * the page shifting under whoever is reading it.
   *
   * It is NOT a button and it does NOT open the sheet: there is no segment to
   * reassign or correct yet, and offering it would be offering to edit
   * something that does not exist.
   */
  function partialRow(p) {
    const seg = { speaker: p.speaker, label_via: p.speaker_hint === 'proximity' ? 'proximity' : null };
    // A partial carries no highlight of its own (PROTOCOL 0.11.0 sends only the
    // proximity hint), so this is the store's answer or nothing — and it has to
    // be the same answer `segRow` gives, or the row would change colour at the
    // moment the final replaces it.
    const { color, icon } = lookOf(p.speaker);
    // 0.12.5: a GROWING row is a different claim from a provisional one, and
    // it is the stronger of the two. A partial's words may be replaced
    // wholesale by the next reading; a slice's words are final — they are
    // already the words the row will carry — and only the END of the sentence
    // is still missing. It gets its own class so it can be drawn in settled ink
    // rather than as a guess, and the ellipsis stays on both, because on both
    // the sentence is unfinished.
    const row = h('div', {
      class: `seg partial${p.growing ? ' growing' : ''}${isYou(p.speaker) ? ' you' : ''}`,
      dataset: { partial: String(p.seq_in_turn) },
      // Not a control and not a row anybody can act on. Announced politely by
      // the container, never focusable.
      'aria-label': p.growing ? 'still being said, words so far' : 'still being said',
    });
    row.append(
      h('span', { class: 't', text: p.t_start_ms ? fmtClock(p.t_start_ms) : '' }),
      h(
        'span',
        { class: 'who' },
        h('span', { class: 'dot', style: `color:${color}` }),
        iconSpan(icon),
        h('span', {
          class: `nm${p.speaker == null ? ' reasoned' : ''}`,
          text: segmentSpeakerLabel(seg),
          style: p.speaker == null ? '' : `color:${color}`,
        })
      ),
      h('span', { class: 'txt' }, p.text, h('span', { class: 'seg-ell', text: '…' })),
      h('span', { class: 'meta' })
    );
    // The same accent the final row will wear, so nothing moves or lights up
    // when the provisional is replaced. `isUncertain` refuses it for a
    // proximity guess, which is what most partials are.
    markRow(row, lookOf(p.speaker).hl, { uncertain: isUncertain(seg) });
    return row;
  }

  function updateCount() {
    const n = visible().length;
    const parts = [`${n} segment${n === 1 ? '' : 's'} in view`];
    if (filterSpeaker != null) parts.push(`filtered to ${speakerLabel(filterSpeaker)}`);
    if (!following()) parts.push(store.window.beginning ? 'reading from the beginning' : 'reading history');
    countSub.textContent = parts.join(' · ');
    const badge = document.getElementById('badge-transcript');
    if (badge) badge.textContent = String(store.segments.length);
    paintTopNotices();
  }

  /**
   * The two things above the first row that are only sometimes true: that you
   * have reached the first thing ever captured, and that you have not — because
   * the window hit its ceiling instead. The second one is the honest half: a
   * scrollback that silently stopped growing would read as a scrollback that
   * had reached the end, and it has not.
   */
  function paintTopNotices() {
    const atBeginning = store.window.beginning && !!store.segments.length;
    beginMark.hidden = !atBeginning;
    if (atBeginning) {
      const first = store.window.firstMs ?? store.segments[0]?.t_ms;
      beginMark.textContent = first
        ? `the beginning — first captured ${fmtDayLabel(first)}`
        : 'the beginning';
    }
    capNote.hidden = !store.window.capped;
    if (store.window.capped) {
      capNote.textContent = `Showing a ${HARD_MAX.toLocaleString()}-row window — jump older via search or the date picker.`;
    }
  }

  function refreshLiveChip() {
    clear(liveChip);
    if (store.conn.status !== 'connected') {
      liveChip.className = 'chip bad';
      liveChip.append(h('span', { class: 'dot' }), 'offline');
    } else if (store.paused) {
      // Amber, not red: this is attention, and red is spent on delete alone.
      liveChip.className = 'chip warn';
      liveChip.append(h('span', { class: 'dot' }), 'capture paused');
    } else {
      liveChip.className = 'chip live';
      liveChip.append(h('span', { class: 'dot pulse' }), 'live');
    }
  }

  function refreshFilterOptions() {
    const cur = speakerFilter.value;
    clear(speakerFilter);
    speakerFilter.append(h('option', { value: '' }, 'Everyone'));
    for (const sp of [...store.speakers.values()].sort((a, b) => (b.total_ms ?? 0) - (a.total_ms ?? 0))) {
      // The icon and not the colour, for the same reason the Discord truth
      // links in views/sources.js get only the icon: an `<option>` popup is
      // drawn by the platform and takes nothing we can style (see the
      // `color-scheme` note at the top of tokens.css). The emoji renders.
      const ic = lookOf(sp.id).icon;
      speakerFilter.append(
        h('option', { value: String(sp.id), dataset: { speakerAuto: sp.auto ?? '' } }, ic ? `${ic} ${speakerLabel(sp.id)}` : speakerLabel(sp.id))
      );
    }
    speakerFilter.value = cur;
    speakerPicker.refresh();
  }

  function nearBottom() {
    return body.scrollHeight - body.scrollTop - body.clientHeight < 120;
  }

  function scrollToEnd(force = false) {
    if (!following() && !force) return;
    scrollTo(body.scrollHeight);
  }

  /** Every deliberate move of the scroller goes through here, so onScroll knows. */
  function scrollTo(px) {
    ignoreScrollUntil = Date.now() + 400;
    body.scrollTop = px;
  }

  /** …and so does every scrollIntoView, for the same reason. */
  function reveal(el, opts = { block: 'center', behavior: 'smooth' }) {
    ignoreScrollUntil = Date.now() + 900; // smooth scrolling takes a while to land
    el?.scrollIntoView(opts);
  }

  // -- infinite scrollback --------------------------------------------------

  function onScroll() {
    if (Date.now() < ignoreScrollUntil) return;
    noteAnchor();
    // Reaching the very bottom is the other way of pressing Following, and it
    // is the one people actually use. It collapses the window, same as the
    // button, so an evening of scrollback is not kept alive by accident.
    //
    // Not when the window is DETACHED, though: the bottom of a day you jumped
    // to is the end of that day, and reading to the end of it must not teleport
    // you to tonight. Getting back is the button's job there, deliberately.
    if (body.scrollHeight - body.scrollTop - body.clientHeight <= TAIL_MARGIN) {
      if (!following() && !store.window.detached) setFollow(true);
      return;
    }
    // Scrolling up off the tail is browsing. Saying so in the button is what
    // makes the suspended trimming legible rather than mysterious.
    if (following() && !nearBottom()) setFollow(false);
    if (body.scrollTop < LOAD_MARGIN) loadOlder();
  }

  /**
   * Roughly which row the reader is on, for the 20k ceiling to trim away from.
   *
   * By scroll fraction rather than by hit-testing the rows: this runs on every
   * scroll event over a list that can be twenty thousand nodes long, and the
   * only question it has to answer is "which END is further away" — a rough
   * answer to that is exactly as good as an exact one, and O(1).
   */
  function noteAnchor() {
    const n = store.segments.length;
    if (!n) return;
    const span = Math.max(1, body.scrollHeight - body.clientHeight);
    const frac = Math.min(1, Math.max(0, body.scrollTop / span));
    store.window.anchor = store.segments[Math.round(frac * (n - 1))]?.id ?? null;
  }

  /**
   * One page further back, prepended without moving what is on screen.
   *
   * The anchoring is the whole trick and it is deliberately not `scrollIntoView`
   * on a remembered row: measure the scroller's height before the insert and
   * after it, and add the difference to scrollTop. Everything above the
   * viewport grew by exactly that much, so every pixel below it stays where it
   * was — no jump, no smooth-scroll animation to fight, and it is correct even
   * when the new rows are different heights from the old ones.
   */
  async function loadOlder() {
    if (loadingOlder || store.window.beginning || !store.segments.length) return null;
    loadingOlder = true;
    loader.hidden = false;
    const before = store.segments[0];
    // Pin the ceiling's idea of "where the reader is" to the top of the list
    // for the duration of the fetch: the page about to arrive goes above it,
    // and it must not be what gets trimmed to make room.
    store.window.anchor = before.id;
    try {
      const res = await loadOlderPage();
      const at = store.segments.indexOf(before);
      if (res.added && at < 0) {
        // The ceiling took the row we were anchored on. Vanishingly rare, and
        // a repaint is the honest answer rather than a guessed scroll offset.
        renderAll({ scroll: 'top' });
      } else if (res.added) {
        const older = store.segments.slice(0, at);
        const h0 = body.scrollHeight;
        const top0 = body.scrollTop;
        prependRows(older.filter((s) => filterSpeaker == null || s.speaker === filterSpeaker));
        body.scrollTop = top0 + (body.scrollHeight - h0);
        ignoreScrollUntil = Date.now() + 200;
        refreshFilterOptions();
      }
      updateCount();
      return res;
    } catch (e) {
      toast(`Could not load earlier segments — ${e.message}`, 'error');
      return null;
    } finally {
      loadingOlder = false;
      loader.hidden = true;
    }
  }

  /**
   * Put a page of older rows above the ones already rendered, and fix the seam.
   *
   * The seam is the part worth spelling out. The row that used to be first was
   * rendered with no predecessor, so it got a day separator it may no longer
   * deserve and no thread hairline it may now need. Its old separators are torn
   * off and recomputed from the walker's state after the last new row — which
   * is exactly the state a full repaint would have been in at that point.
   */
  function prependRows(rows) {
    const oldFirst = list.querySelector('.seg');
    if (oldFirst) {
      for (let n = oldFirst.previousElementSibling; n; ) {
        const prev = n.previousElementSibling;
        n.remove();
        n = prev;
      }
    }
    const frag = document.createDocumentFragment();
    const walk = nodeWalker();
    for (const seg of rows) {
      for (const sep of walk(seg)) frag.append(sep);
      frag.append(segRow(seg));
    }
    if (oldFirst) {
      const seam = store.segById.get(Number(oldFirst.dataset.seg));
      if (seam) for (const sep of walk(seam)) frag.append(sep);
    }
    list.prepend(frag);
  }

  /**
   * The other seam: the one the TAIL trim leaves behind. Dropping the oldest
   * rows strands the separators that led them, and can leave the new first row
   * with no day label at all — so whatever is above the first row is torn off
   * and replaced with what a repaint would have put there, which is a day
   * header and no thread hairline.
   */
  function fixLeadingSeparators() {
    const first = list.querySelector('.seg');
    if (!first) return;
    for (let n = first.previousElementSibling; n; ) {
      const prev = n.previousElementSibling;
      n.remove();
      n = prev;
    }
    const seg = store.segById.get(Number(first.dataset.seg));
    if (seg) list.prepend(h('div', { class: 'day-sep', text: fmtDayLabel(seg.t_ms) }));
  }

  /**
   * The date picker: clear the view and load that day, anchored `from`/`to`.
   * Anchored is the point — the daemon hands back the OLDEST rows in a bounded
   * range (PROTOCOL "Paging the transcript"), so you land at the start of the
   * day rather than at the end of it.
   */
  async function jumpToDay(value) {
    if (!value) return null;
    const start = new Date(`${value}T00:00:00`);
    if (Number.isNaN(start.getTime())) return null;
    const end = new Date(start.getTime() + 86_400_000);
    try {
      const res = await ask('transcript', {
        from: start.toISOString(),
        to: end.toISOString(),
        limit: 2000,
      });
      const rows = res.segments ?? [];
      if (!rows.length) {
        // The view is not thrown away for a day with nothing in it: you would
        // lose where you were to be told "no".
        toast(`Nothing was captured on ${value}.`, '');
        return { day: value, rows: 0 };
      }
      replaceSegments(rows);
      highlightThread = null;
      paintFollowBtn();
      refreshFilterOptions();
      renderAll({ scroll: 'top' });
      return { day: value, rows: rows.length };
    } catch (e) {
      toast(`Could not open ${value} — ${e.message}`, 'error');
      return null;
    }
  }

  // -- incremental updates --------------------------------------------------

  function update(change) {
    if (!change) return;
    if (change.added) {
      const stick = following() && nearBottom();
      const names = threadNameResolver(store.segments, speakerLabel);
      for (const seg of change.added) {
        if (filterSpeaker != null && seg.speaker !== filterSpeaker) continue;
        const lastRow = list.querySelector('.seg:last-of-type');
        const lastSeg = lastRow ? store.segById.get(Number(lastRow.dataset.seg)) : null;
        if (!lastSeg && list.querySelector('.empty')) clear(list);
        // The live feed crosses day and conversation boundaries too, by the
        // same rules — a separator that only ever appeared on a full repaint
        // would be a boundary you could only see by leaving and coming back.
        const walk = nodeWalker({
          day: lastSeg ? fmtDay(lastSeg.t_ms) : null,
          thread: lastSeg?.thread ?? null,
          seen: !!lastSeg,
        }, names);
        // Where the MODEL filed it decides where the row goes. A late turn — a
        // mic segment that closed after an app segment that started later, a
        // row the daemon re-read — lands before the rows it precedes, not at
        // the bottom as if it had just happened.
        const at = store.segments.indexOf(seg);
        const next = at >= 0 ? store.segments.slice(at + 1).find((s) => list.querySelector(`.seg[data-seg="${s.id}"]`)) : null;
        if (next) {
          list.insertBefore(segRow(seg, true), list.querySelector(`.seg[data-seg="${next.id}"]`));
          continue;
        }
        for (const sep of walk(seg)) list.append(sep);
        list.append(segRow(seg, true));
      }
      // Whatever the model dropped, the DOM drops — from the SAME end. While
      // following that is the oldest rows, as ever; while browsing the model
      // drops nothing at all, so neither does this.
      let trimmed = false;
      while (list.querySelectorAll('.seg').length > visible().length) {
        const first = list.querySelector('.seg');
        if (!first) break;
        first.remove();
        trimmed = true;
      }
      if (trimmed) fixLeadingSeparators();
      updateCount();
      if (stick) scrollToEnd();
    }
    if (change.updated) {
      for (const seg of change.updated) {
        const old = list.querySelector(`.seg[data-seg="${seg.id}"]`);
        if (old) old.replaceWith(segRow(seg));
      }
    }
    if (change.purged) {
      for (const segId of change.purged) list.querySelector(`.seg[data-seg="${segId}"]`)?.remove();
      updateCount();
    }
    if (change.merged) repaint(); // ids moved wholesale; a repaint is honest and rare
    // 0.10.2: `translation_display` decides which of a translated row's two
    // lines is the main one, so a change to it — from the card, from another
    // window, from the config file — is a repaint of every row on screen.
    if (change.assist) repaint();
    if (change.relabel) refreshFilterOptions();
    // The pin arriving (or moving, after a merge) changes which rows are
    // marked as the user's. Guarded on the id itself, because `mic` and
    // `status` events also fire for things that change nothing here.
    if (change.mic && store.mic.you_speaker !== lastYou) {
      lastYou = store.mic.you_speaker;
      repaint();
    }
    if (change.status || change.conn) refreshLiveChip();
    // 0.11.0. `change.partial` is set both by a partial arriving and by the
    // segment that replaces it, so one call covers "draw it", "update it" and
    // "it is gone". Pause and a dropped connection reach it through the same
    // door — `livePartial` refuses to answer in either state.
    if (change.partial || change.added || change.status || change.conn || change.mic) renderPartial();
  }

  // -------------------------------------------------------------------------
  // painting the replay
  // -------------------------------------------------------------------------

  /**
   * Start replaying a conversation. Called from the separator's own button and,
   * through the controller, from a digest, a person page and a search hit.
   *
   * The thread's span is marked the same way arriving at it from a person page
   * marks it — you are here to read this conversation either way — and the rows
   * are assumed to be resident, because every route into this merges them
   * first (app.js `showThreadInTranscript`).
   */
  async function startReplay(threadId, { from = null, segments = null } = {}) {
    leaveTheTail();
    highlightThread = Number(threadId);
    setFilter(null);
    const res = await replay.start(Number(threadId), { from, ...(segments ? { segments } : {}) });
    if (!res.ok) {
      toast(replay.startError(res.error), 'error');
      return null;
    }
    replayBar.focus({ preventScroll: true });
    return res;
  }

  function paintReplay(rs) {
    const on = !!rs?.active;
    replayBar.hidden = !on;
    // While a replay is running the thread's OWN mark steps back to a rail, so
    // the one tinted row in the list is the one being said. Two rows painted
    // the same colour for two different reasons is the same as no mark at all.
    list.classList.toggle('replaying', on);
    const wasSeg = replayingSeg;
    replayingSeg = on ? (rs.turns[rs.index]?.id ?? null) : null;
    if (wasSeg !== replayingSeg) {
      for (const el of list.querySelectorAll('.seg.replaying')) el.classList.remove('replaying');
      if (replayingSeg != null) {
        const row = list.querySelector(`.seg[data-seg="${replayingSeg}"]`);
        if (row) {
          row.classList.add('replaying');
          // Arriving is a JUMP and following is a nudge, and they want opposite
          // scrolls. The first turn of a replay can be a thousand rows from
          // where you were reading, so it lands instantly and in the middle —
          // an animated flight down an evening is a second of nothing. After
          // that, `nearest` and smooth: the transcript should move only when
          // the playing turn would otherwise leave the screen, because yanking
          // every row to the middle makes a conversation read as a slot machine.
          if (!replayArrived) reveal(row, { block: 'center', behavior: 'auto' });
          else reveal(row, { block: 'nearest', behavior: 'smooth' });
          replayArrived = true;
        }
      }
    }
    if (!on) {
      replayArrived = false;
      clear(rScrub);
      rNote.hidden = true;
      return;
    }

    const turn = rs.turns[rs.index] ?? null;
    const playing = rs.phase === 'playing' || rs.phase === 'silent' || rs.phase === 'loading';
    rIco.innerHTML = `<svg viewBox="0 0 24 24" width="14" height="14" fill="currentColor" aria-hidden="true">${playing ? ICON_PAUSE : ICON_PLAY}</svg>`;
    rPlay.setAttribute('aria-label', playing ? 'Pause' : 'Play');
    rPlay.title = playing ? 'Pause' : rs.phase === 'ended' ? 'Play it again from the start' : 'Play';
    rPlay.dataset.phase = rs.phase;
    // The replay turn carries its own `speaker_colour`/`speaker_icon` (PROTOCOL
    // `replay.turns`), so a conversation replayed out of an evening the store
    // has forgotten still says who is talking in their own colour.
    const turnLook = turn ? look(turn) : null;
    rWho.textContent = turn
      ? `${turnLook.icon ? `${turnLook.icon} ` : ''}${segmentSpeakerLabel({ speaker: turn.speaker, overlap_frac: 0 })}`
      : '';
    if (turn?.speaker != null) rWho.style.color = turnLook.color;
    else rWho.style.color = '';
    rClock.textContent = turn
      ? `${fmtClock(turn.t_ms)} · turn ${rs.index + 1} of ${rs.turns.length}`
      : '';
    rRate.textContent = `${rs.rate}×`;
    rRate.setAttribute('aria-label', `Playback speed ${rs.rate} times — press to change`);
    rPrev.disabled = rs.index === 0;
    rNext.disabled = rs.index >= rs.turns.length - 1;

    // The scrubber is over the TURNS, not over the seconds: a conversation is
    // a sequence of things people said, and the thing a listener wants to get
    // back to is one of them. A turn whose audio retention took carries a tick
    // that says so, which is the only per-row mention of it anywhere.
    if (rScrub.childElementCount !== rs.turns.length) {
      clear(rScrub);
      rs.turns.forEach((t, i) => {
        rScrub.append(
          h('button', {
            class: 'replay-tick',
            dataset: { turn: String(t.id), at: String(i) },
            style: `flex-grow:${Math.max(1, Math.round((t.dur_ms || 0) / 100))}`,
            onclick: () => replay.jump(i),
          })
        );
      });
    }
    [...rScrub.children].forEach((tick, i) => {
      const t = rs.turns[i];
      tick.classList.toggle('gone', !t.has_audio);
      tick.classList.toggle('at', i === rs.index);
      tick.classList.toggle('done', i < rs.index);
      // A tick is a bar three pixels wide; the icon can only go in the tooltip,
      // where it is the fastest way to spot your own turns in a long scrub.
      const tickIcon = look(t).icon;
      tick.title = `${fmtClock(t.t_ms)} — ${tickIcon ? `${tickIcon} ` : ''}${speakerLabel(t.speaker)}${
        t.has_audio ? '' : ' · no audio kept'
      }`;
      tick.setAttribute('aria-label', tick.title);
    });

    const note = replay.missingNote(rs);
    rNote.hidden = !note;
    rNote.textContent = note ?? '';
  }

  // The engine outlives any one paint, and the view has no unmount hook, so the
  // subscription retires itself once its DOM is gone (same as the speakers
  // view's playback subscription).
  const offReplay = replay.onReplay((rs) => {
    if (!replayBar.isConnected) {
      offReplay();
      return;
    }
    paintReplay(rs);
  });

  refreshLiveChip();
  refreshFilterOptions();
  paintFollowBtn();
  paintReplay(replay.replayState());
  // A mount that follows lands on the tail. A mount that does not is a jump
  // that has already loaded its page and is about to scroll to it itself.
  renderAll({ scroll: following() ? 'end' : 'none' });

  /** Every jump leaves the tail. One place, so the button can never lag it. */
  function leaveTheTail() {
    if (following()) setFollow(false, { repaint: false });
    paintFollowBtn();
  }

  return {
    update(change) { conversationFind.update(change); update(change); },
    destroy() { conversationFind.destroy(); },
    /** Search results jump here: scroll the segment into view and mark it. */
    focusSegment(segId) {
      leaveTheTail();
      const row = list.querySelector(`.seg[data-seg="${segId}"]`);
      // No toast for a miss any more, and no miss either: the controller has
      // already merged the minutes around the hit and the merge now SURVIVES
      // the trim (store.js, audit finding #12). A row that is still not here
      // is a bug, not a window boundary, and inventing an explanation for the
      // user was how the bug stayed invisible.
      if (!row) return false;
      for (const el of list.querySelectorAll('.seg.hit')) el.classList.remove('hit');
      row.classList.add('hit');
      reveal(row);
      return true;
    },
    /**
     * "Show in transcript", from the speakers view or a segment sheet: the
     * existing filter is set to that voice and the view lands on the last thing
     * they said, because the question behind the click is almost always "what
     * have they been saying?".
     */
    focusSpeaker(spId) {
      leaveTheTail();
      setFilter(spId);
      const rows = list.querySelectorAll('.seg');
      const last = rows[rows.length - 1];
      if (last) {
        for (const el of list.querySelectorAll('.seg.hit')) el.classList.remove('hit');
        last.classList.add('hit');
        reveal(last);
      }
      return { speaker: spId, rows: rows.length };
    },
    /**
     * "Read this conversation", from a person page: the transcript lands on
     * the thread with the speaker filter **cleared** and the thread's own span
     * marked. Clearing is the whole point — you came to read what everybody
     * said, and arriving filtered to one voice would answer a question nobody
     * asked. The marking is deliberately quiet: a tinted rail down the span,
     * not a colour that competes with the words.
     */
    focusThread(threadId) {
      leaveTheTail();
      highlightThread = threadId;
      setFilter(null);
      const rows = [...list.querySelectorAll(`.seg[data-thread="${threadId}"]`)];
      for (const el of list.querySelectorAll('.seg.hit')) el.classList.remove('hit');
      if (rows.length) reveal(rows[0]);
      return { thread: threadId, rows: rows.length };
    },
    /**
     * The live tail (0.11.0), read off the DOM: what a person would actually
     * see, not what the model believes it was handed.
     */
    partial() {
      const el = tail.querySelector('.seg.partial');
      if (!el) return null;
      return {
        seq: Number(el.dataset.partial),
        who: el.querySelector('.nm')?.textContent ?? '',
        text: el.querySelector('.txt')?.textContent ?? '',
        ellipsis: !!el.querySelector('.seg-ell'),
        // 0.12.5: whether this row is GROWING (a sliced turn, words being
        // added) rather than provisional (a partial, words being replaced).
        growing: el.classList.contains('growing'),
        ink: getComputedStyle(el.querySelector('.txt')).color,
        // The two rules that make it a tail and not a row.
        inList: !!list.querySelector('.seg.partial'),
        counted: list.querySelectorAll('.seg').length,
      };
    },
    /** A conversation, played back (0.9.2). Every route in lands here. */
    startReplay,
    /** What the bar is saying, for the headless driver. */
    replayUi() {
      const rs = replay.replayState();
      return {
        ...rs,
        shown: !replayBar.hidden,
        who: rWho.textContent,
        clock: rClock.textContent,
        rate: rs.rate,
        rateLabel: rRate.textContent,
        playLabel: rPlay.getAttribute('aria-label'),
        note: replayBar.querySelector('#replay-note')?.hidden ? '' : rNote.textContent,
        ticks: [...rScrub.children].map((t) => ({
          turn: Number(t.dataset.turn),
          gone: t.classList.contains('gone'),
          at: t.classList.contains('at'),
        })),
        row: list.querySelector('.seg.replaying')?.dataset.seg ?? null,
        rowsWithButton: list.querySelectorAll('.thread-sep .replay-start').length,
      };
    },
    /** For the headless driver and the keyboard: one page further back. */
    loadOlder,
    jumpToDay,
    refreshLiveChip,
  };
}

// ---------------------------------------------------------------------------
// the reassign / correct sheet
// ---------------------------------------------------------------------------

export function openSegmentSheet(seg, ctx) {
  let picked = seg.speaker ?? null;

  const build = (close) => {
    const recent = new Map();
    for (const row of store.segments) if (row.speaker != null) recent.set(row.speaker, Math.max(recent.get(row.speaker) ?? 0, row.t_ms ?? 0));
    const choices = [...store.speakers.values()].sort((a, b) => Number(!!b.name) - Number(!!a.name) || (recent.get(b.id) ?? 0) - (recent.get(a.id) ?? 0) || (b.total_ms ?? 0) - (a.total_ms ?? 0));
    const speakerPicker = createSpeakerPicker({
      speakers: choices, label: speakerLabel, selected: picked, allowUnassigned: true,
      onSelect: id => { picked = id; },
      render: (sp, choose) => {
        const id = sp?.id ?? null;
        const { color, icon } = lookOf(id);
        return h('button', { onclick: choose }, h('span', { class: 'dot', style: `color:${color}` }), iconSpan(icon), speakerLabel(id));
      },
    });

    // ------------------------------------------------------------------
    // "Fix this" (0.8.0)
    //
    // The correct path has been here since 0.4 and almost nobody used it,
    // because it was a textarea below a heading below a picker: three
    // decisions deep for an act that is one — you read a wrong word and you
    // want to type the right one. So the words themselves are the control.
    // Click them and you are editing them, in place, at the same size and in
    // the same position; Enter saves, Escape puts them back. The Save button
    // is still there and still saves both halves, because the picker above it
    // has to commit somehow, and because a person who typed and then reached
    // for the obvious button must not lose their edit.
    // ------------------------------------------------------------------

    // `data-keep-escape`: Escape in here abandons the edit and keeps the sheet
    // open (lib/sheets.js). The nearer meaning wins.
    const text = h('textarea', {
      class: 'input',
      id: 'correct-text',
      spellcheck: 'false',
      'data-keep-escape': '',
      hidden: true,
    });
    text.value = seg.text ?? '';

    const reader = h('button', {
      class: 'fix-text',
      id: 'segment-text',
      title: 'Click to fix these words — Enter saves, Escape cancels',
      text: seg.text || '…',
      onclick: () => beginEdit(),
    });
    const fixHint = h('span', {
      class: 'fix-hint',
      id: 'segment-fix-hint',
      text: 'click the words to fix them',
    });

    function beginEdit() {
      reader.hidden = true;
      text.hidden = false;
      fixHint.textContent = 'Enter saves · Escape cancels';
      text.focus();
      // The cursor lands at the end rather than selecting everything: a fix is
      // almost always a word, and select-all makes the first keystroke a delete.
      text.setSelectionRange(text.value.length, text.value.length);
    }

    function endEdit({ revert = false } = {}) {
      if (revert) text.value = seg.text ?? '';
      text.hidden = true;
      reader.hidden = false;
      reader.textContent = text.value || '…';
      fixHint.textContent = 'click the words to fix them';
    }

    text.addEventListener('keydown', (e) => {
      if (e.key === 'Escape') {
        // Stopped here so the sheet's own Escape does not also close the sheet:
        // one keystroke, one meaning, and the nearer thing wins.
        e.preventDefault();
        e.stopPropagation();
        endEdit({ revert: true });
        return;
      }
      // Shift+Enter still breaks a line — a transcript turn can be long, and an
      // editor where Enter is the only save has to leave a way to type one.
      if (e.key === 'Enter' && !e.shiftKey) {
        e.preventDefault();
        void saveText();
      }
    });

    /** The one motion: Enter in the words saves the words and nothing else. */
    async function saveText() {
      if (text.value === seg.text) {
        endEdit();
        return;
      }
      try {
        await ask('segments.correct', { segment_id: seg.id, text: text.value });
        // No optimistic write: the daemon broadcasts the corrected segment,
        // every client updates from that, and the row grows its "edited" mark
        // through the same path a correction made in the CLI would take.
        endEdit();
        toast('Fixed. It feeds the accuracy figures and the vocabulary.', 'ok');
      } catch (e) {
        toast(`Could not save that — ${e.message}`, 'error');
      }
    }

    // ------------------------------------------------------------------
    // Naming a NEW voice, here (0.10.0)
    //
    // The moment you know who a voice is, is the moment you are reading what
    // they said — not later, on the Speakers page, hunting for "Speaker_38"
    // among the rest. So a segment whose voice has no name yet offers the
    // field right under the picker. Enter names it; the daemon's `relabel`
    // broadcast repaints every row that voice ever spoke, exactly as a rename
    // from the Speakers page would. A voice that already has a name is not
    // renamed from here: that is a different, rarer act, and it stays where
    // its consequences (merges, languages) are visible.
    // ------------------------------------------------------------------
    const unnamed = () => {
      const id = picked ?? null;
      if (id == null) return null;
      const sp = store.speakers.get(id);
      return sp && !sp.name ? sp : null;
    };
    const nameInput = h('input', {
      class: 'input',
      id: 'name-voice',
      type: 'text',
      placeholder: 'Name this voice',
      'aria-label': 'Name this voice',
      'data-keep-escape': '',
      maxlength: '48',
    });
    const nameBtn = h('button', { class: 'btn small', id: 'name-voice-save', onclick: () => void saveName() }, 'Name');
    const nameRow = h(
      'div',
      { class: 'name-voice-row', id: 'name-voice-row', hidden: !unnamed() },
      h('span', { class: 'sp-hint', id: 'name-voice-hint' }),
      nameInput,
      nameBtn
    );
    function paintNameRow() {
      const sp = unnamed();
      nameRow.hidden = !sp;
      if (sp) nameRow.querySelector('#name-voice-hint').textContent = `${speakerLabel(sp.id)} has no name yet.`;
    }
    // ------------------------------------------------------------------
    // Highlighting the picked voice, here (0.12.0)
    //
    // The naming row above is hidden for a voice that already has a name,
    // because renaming is a rarer act with consequences that belong on the
    // Speakers page. A HIGHLIGHT is the opposite: you want to mark the person
    // you are reading, and you almost always already know their name. So this
    // row follows `picked` and nothing else — it is up for any voice, named or
    // not, and it is deliberately not folded into `paintNameRow`'s hidden rule.
    //
    // Rebuilt rather than re-targeted when the pick moves, because the picker
    // closes over one speaker id (its swatches' aria-labels name that person)
    // and a control that quietly started writing to somebody else would be the
    // worst possible bug in a feature about telling people apart.
    // ------------------------------------------------------------------
    const hlRow = h('div', { class: 'hl-row', id: 'highlight-row', hidden: picked == null });
    function paintHlRow() {
      clear(hlRow);
      hlRow.hidden = picked == null;
      if (picked == null) return;
      hlRow.append(highlightPicker(picked));
    }

    paintNameRow();
    paintHlRow();
    // The picker above can move this segment to another unnamed voice; the
    // offer follows the picked voice, not the row's original one.
    pick.addEventListener('click', () =>
      queueMicrotask(() => {
        paintNameRow();
        paintHlRow();
      })
    );
    nameInput.addEventListener('keydown', (e) => {
      if (e.key === 'Escape') {
        e.preventDefault();
        e.stopPropagation();
        nameInput.value = '';
        return;
      }
      if (e.key === 'Enter') {
        e.preventDefault();
        void saveName();
      }
    });
    async function saveName() {
      const sp = unnamed();
      const name = nameInput.value.trim();
      if (!sp || !name) return;
      nameBtn.disabled = true;
      try {
        await ask('speakers.name', { id: sp.id, name });
        // No optimistic write: the `relabel` event relabels this row and every
        // other one, and the hint below reflects the store once it has.
        toast(`Named ${name}. Every turn of that voice now says so.`, 'ok');
        nameInput.value = '';
        setTimeout(paintNameRow, 50);
      } catch (e) {
        toast(`Could not name that voice — ${e.message}`, 'error');
      } finally {
        nameBtn.disabled = false;
      }
    }

    const save = async () => {
      const jobs = [];
      if (picked !== (seg.speaker ?? null)) jobs.push(ask('segments.reassign', { segment_id: seg.id, speaker_id: picked }));
      if (text.value !== seg.text) jobs.push(ask('segments.correct', { segment_id: seg.id, text: text.value }));
      if (!jobs.length) {
        close();
        return;
      }
      try {
        await Promise.all(jobs);
        toast('Segment updated.', 'ok');
        close();
      } catch (e) {
        toast(`Could not save the correction — ${e.message}`, 'error');
      }
    };

    // "Who said this?" is a question about a sound, so the sound is one press
    // away — same shared player as the speakers view, so the two can never be
    // talking at once.
    const key = `segment:${seg.id}`;
    const hint = h('span', { class: 'sp-hint', id: 'segment-audio-hint' });
    const listen = h('button', {
      class: 'btn small preview',
      id: 'segment-play',
      title: 'Listen to this segment',
      onclick: async () => {
        if (isActive(key)) {
          stopPreview();
          return;
        }
        hint.textContent = '';
        hint.classList.remove('shown');
        const res = await play(key, [seg.id]);
        if (!res.stopped && !res.played) {
          hint.textContent = noAudioHint(res.error === 'empty' ? 'gone' : res.error);
          hint.classList.add('shown');
        }
      },
    });
    const paint = () => {
      const on = isActive(key);
      listen.classList.toggle('on', on);
      listen.textContent = on ? '■' : '▶';
      listen.setAttribute('aria-pressed', String(on));
      listen.setAttribute('aria-label', on ? 'Stop this segment' : 'Play this segment');
    };
    paint();
    // The sheet is torn out of the DOM on close, which is also when this
    // subscription stops being anyone's business. It only retires itself — the
    // sheet's onClose does the stopping. A closed sheet that reached for the
    // player here would kill the *next* preview on its very first event, which
    // is exactly the bug the headless suite caught.
    const off = onPlayback(() => {
      if (!listen.isConnected) {
        off();
        return;
      }
      paint();
    });

    return [
      h('div', { class: 'sheet-head' }, h('h2', { text: 'Reassign or correct' }), h('button', { class: 'btn small', id: 'segment-save-moment', onclick: () => { close(); void saveMomentSheet([seg.id]); } }, 'Save moment')),
      h('p', {
        class: 'sub',
        // The name's provenance, then — when there is one — the words'. A
        // certain speaker whose transcript was re-read still says so here.
        text: isUncertain(seg)
          ? uncertainReason(seg)
          : [
              isYou(seg.speaker)
                ? // No score, and there should not be one: nothing was
                  // compared. Saying "matched at 0.00" here would be a lie
                  // about a fact the daemon is more sure of than anything else
                  // in the transcript.
                  `Recorded on your own microphone, so the speaker is not a guess. ${Math.round((seg.overlap_frac ?? 0) * 100)}% overlapped.`
                : `Matched at ${(seg.match_score ?? 0).toFixed(2)} confidence, ${Math.round((seg.overlap_frac ?? 0) * 100)}% overlapped.`,
              languageNote(seg),
            ]
              .filter(Boolean)
              .join(' '),
      }),
      h(
        'div',
        { class: 'quote listen-row' },
        listen,
        h('span', {}, `${fmtClock(seg.t_ms)} · ${seg.source ?? 'unknown source'}`, hint)
      ),
      h(
        'div',
        { class: 'sheet-head' },
        h('span', { class: 'card-title', text: 'Who said this' }),
        // Only once the segment HAS a voice: "show me the rest of what they
        // said" is the natural next question after answering "who is this?".
        seg.speaker != null
          ? h(
              'button',
              {
                class: 'btn small',
                id: 'sheet-show-in-transcript',
                onclick: () => {
                  close();
                  ctx?.showSpeakerInTranscript?.(seg.speaker);
                },
              },
              'Show in transcript'
            )
          : null
      ),
      ...speakerPicker.nodes,
      nameRow,
      // Under the naming offer and not inside it: `nameRow` is hidden the
      // moment a voice has a name, and a highlight is for exactly the voice
      // you already know the name of. See `paintHlRow` above.
      hlRow,
      h(
        'div',
        { class: 'sheet-head' },
        h('span', { class: 'card-title', text: 'What they said' }),
        fixHint
      ),
      // Where the words came from, when it was not the first pass, and whether
      // a second decoder agreed with them. Both are one short line — a person
      // deciding whether to retype a sentence wants to know that a machine has
      // already been round twice, not to read a paragraph about how.
      provenanceLine(seg),
      reader,
      text,
      h(
        'div',
        { class: 'actions' },
        h('button', { class: 'btn', onclick: () => close() }, 'Cancel'),
        h('button', { class: 'btn primary', id: 'segment-save', onclick: save }, 'Save')
      ),
    ];
  };

  // Closing the sheet takes the sound with it — the stop button just left.
  openSheet(build, { onClose: () => stopPreview() });
}

/**
 * The one line under "What they said": how these words got here, and what a
 * second opinion made of them. Absent entirely when there is nothing to say —
 * a first-pass reading two decoders agreed on is the ordinary case and does not
 * need to announce itself.
 */
function provenanceLine(seg) {
  const via = textViaNote(seg);
  const shaky = isShaky(seg);
  if (!via && !shaky) return null;
  return h(
    'p',
    { class: 'fix-provenance', id: 'segment-provenance' },
    via ? h('span', { class: 'chip', id: 'segment-text-via', text: via }) : null,
    shaky ? h('span', { class: 'chip warn', id: 'segment-shaky', text: SHAKY_NOTE }) : null
  );
}
