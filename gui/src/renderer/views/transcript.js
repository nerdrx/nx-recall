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

import { h, clear, fmtClock, fmtDay, fmtDayLabel, speakerColor } from '../lib/dom.js';
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
  isYou,
  ask,
  setFollowing,
  loadOlderPage,
  replaceSegments,
  followTail,
  HARD_MAX,
} from '../lib/store.js';
import { separatorWalker } from '../lib/seams.js';
import { openSheet, toast } from '../lib/sheets.js';
import { play, stop as stopPreview, isActive, onPlayback, noAudioHint } from '../lib/preview.js';

export const id = 'transcript';

/** How close to the top of the scroller starts loading the page before. */
const LOAD_MARGIN = 320;
/** How close to the bottom counts as "back on the tail". */
const TAIL_MARGIN = 8;

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

  const following = () => store.window.following;

  const list = h('div', { class: 'seg-list', id: 'seg-list' });
  const card = h('div', { class: 'card' }, list);
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

  /** Drive the existing Everyone/speaker filter from anywhere else in the UI. */
  function setFilter(spId) {
    filterSpeaker = spId ?? null;
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
    speakerFilter,
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

  root.append(head, body);
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
      updateCount();
      return;
    }
    const walk = nodeWalker();
    for (const seg of rows) {
      for (const sep of walk(seg)) list.append(sep);
      list.append(segRow(seg));
    }
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
  function nodeWalker(state) {
    const walk = separatorWalker(state);
    return (seg) => {
      const at = walk(seg);
      const out = [];
      if (at.day) out.push(h('div', { class: 'day-sep', text: fmtDayLabel(seg.t_ms) }));
      if (at.thread) out.push(threadSep(seg));
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
  function threadSep(seg) {
    const names = threadNames(seg.thread);
    return h(
      'div',
      { class: 'thread-sep', dataset: { thread: String(seg.thread) } },
      h('span', { class: 'thread-sep-label', text: names.length ? names.join(' · ') : 'another conversation' })
    );
  }

  /** Who is in a thread, as far as the loaded window can see. */
  function threadNames(threadId) {
    const seen = [];
    for (const s of store.segments) {
      if (s.thread !== threadId || s.speaker == null) continue;
      if (!seen.includes(s.speaker)) seen.push(s.speaker);
    }
    return seen.map((id) => speakerLabel(id));
  }

  function segRow(seg, isNew = false) {
    const uncertain = isUncertain(seg);
    // The user's own voice, off their own microphone. The label did not come
    // from a match, it came from where the audio arrived — so it is the one
    // name in the transcript that is never a guess. Marked, not shouted: a
    // filled dot and an underline on the name, no layout change, no colour of
    // its own beyond the accent.
    const mine = isYou(seg.speaker);
    const color = speakerColor(seg.speaker);
    const row = h('div', {
      class: `seg${uncertain ? ' uncertain' : ''}${mine ? ' you' : ''}${isNew ? ' new' : ''}${seg.corrected ? ' corrected' : ''}${
        highlightThread != null && seg.thread === highlightThread ? ' in-thread' : ''
      }`,
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
      nm
    );
    row.append(
      h('span', { class: 't', text: fmtClock(seg.t_ms) }),
      who,
      h('span', { class: 'txt', text: seg.text || '…' }),
      h(
        'span',
        { class: 'meta' },
        seg.source === 'Discord' ? h('span', { class: 'chip', text: 'discord' }) : null,
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
      speakerFilter.append(h('option', { value: String(sp.id) }, speakerLabel(sp.id)));
    }
    speakerFilter.value = cur;
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
        });
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
    if (change.relabel) refreshFilterOptions();
    // The pin arriving (or moving, after a merge) changes which rows are
    // marked as the user's. Guarded on the id itself, because `mic` and
    // `status` events also fire for things that change nothing here.
    if (change.mic && store.mic.you_speaker !== lastYou) {
      lastYou = store.mic.you_speaker;
      repaint();
    }
    if (change.status || change.conn) refreshLiveChip();
  }

  refreshLiveChip();
  refreshFilterOptions();
  paintFollowBtn();
  // A mount that follows lands on the tail. A mount that does not is a jump
  // that has already loaded its page and is about to scroll to it itself.
  renderAll({ scroll: following() ? 'end' : 'none' });

  /** Every jump leaves the tail. One place, so the button can never lag it. */
  function leaveTheTail() {
    if (following()) setFollow(false, { repaint: false });
    paintFollowBtn();
  }

  return {
    update,
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
    const pick = h('div', { class: 'sp-pick' });
    const rebuild = () => {
      clear(pick);
      const mk = (spId, label) =>
        h(
          'button',
          {
            'aria-pressed': String(picked === spId),
            onclick: () => {
              picked = spId;
              rebuild();
            },
          },
          h('span', { class: 'dot', style: `color:${speakerColor(spId)}` }),
          label
        );
      pick.append(mk(null, 'Unassigned'));
      for (const sp of [...store.speakers.values()].sort((a, b) => (b.total_ms ?? 0) - (a.total_ms ?? 0))) {
        pick.append(mk(sp.id, speakerLabel(sp.id)));
      }
    };
    rebuild();

    const text = h('textarea', { class: 'input', id: 'correct-text', spellcheck: 'false' });
    text.value = seg.text ?? '';

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
        // No optimistic write: the daemon broadcasts the corrected segment and
        // the model updates from that, so every client agrees (DESIGN §8).
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
      h('h2', { text: 'Reassign or correct' }),
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
      pick,
      h('div', { class: 'card-title', text: 'What they said' }),
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
