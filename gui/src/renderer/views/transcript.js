// Live transcript — the view the app is for. Segments stream in as they are
// transcribed; anything the pipeline itself refused to identify is visibly
// muted and carries a "?" that says why; clicking any segment opens the
// reassign/correct sheet (PROTOCOL segments.reassign / segments.correct).

import { h, clear, fmtClock, fmtDay, fmtDayLabel, speakerColor } from '../lib/dom.js';
import { store, speakerLabel, segmentSpeakerLabel, isUncertain, uncertainReason, isYou, ask } from '../lib/store.js';
import { openSheet, toast } from '../lib/sheets.js';
import { play, stop as stopPreview, isActive, onPlayback, noAudioHint } from '../lib/preview.js';

export const id = 'transcript';

export function mount(root, ctx) {
  let follow = true;
  let filterSpeaker = null;
  let lastYou = store.mic.you_speaker;
  // The conversation the view was sent to, if any. Subtle by design: it marks a
  // span rather than hiding everything else, because the point of arriving at a
  // conversation is to see it in the evening it happened in.
  let highlightThread = null;

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
      onclick: () => {
        follow = !follow;
        followBtn.setAttribute('aria-pressed', String(follow));
        followBtn.textContent = follow ? 'Following' : 'Follow';
        if (follow) scrollToEnd();
      },
    },
    'Following'
  );

  const speakerFilter = h(
    'select',
    {
      class: 'input',
      id: 'transcript-filter',
      onchange: (e) => {
        const v = e.target.value;
        filterSpeaker = v === '' ? null : Number(v);
        renderAll();
      },
    },
    h('option', { value: '' }, 'Everyone')
  );

  /** Drive the existing Everyone/speaker filter from anywhere else in the UI. */
  function setFilter(spId) {
    filterSpeaker = spId ?? null;
    speakerFilter.value = spId == null ? '' : String(spId);
    renderAll();
  }

  const head = h(
    'div',
    { class: 'view-head' },
    h('div', {}, h('h1', { text: 'Live transcript' }), countSub),
    h('div', { class: 'spacer' }),
    liveChip,
    speakerFilter,
    followBtn
  );

  root.append(head, body);

  // -- rendering ------------------------------------------------------------

  function visible() {
    return filterSpeaker == null ? store.segments : store.segments.filter((s) => s.speaker === filterSpeaker);
  }

  function renderAll() {
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
    let day = null;
    let thread = null;
    for (const seg of rows) {
      const d = fmtDay(seg.t_ms);
      if (d !== day) {
        day = d;
        thread = null; // a new day is a new conversation whatever the ids say
        list.append(h('div', { class: 'day-sep', text: fmtDayLabel(seg.t_ms) }));
      }
      const sep = threadSep(seg, thread);
      if (sep) list.append(sep);
      if (seg.thread != null) thread = seg.thread;
      list.append(segRow(seg));
    }
    updateCount();
    scrollToEnd(true);
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
  function threadSep(seg, previous) {
    if (seg.thread == null || seg.thread === previous) return null;
    // The first thread of a page has nothing to be a boundary from.
    if (previous == null && !list.querySelector('.seg')) return null;
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
        uncertain ? h('span', { class: 'qmark', text: '?', title: uncertainReason(seg) }) : null
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
    countSub.textContent = `${n} segment${n === 1 ? '' : 's'} in view${filterSpeaker != null ? ` · filtered to ${speakerLabel(filterSpeaker)}` : ''}`;
    const badge = document.getElementById('badge-transcript');
    if (badge) badge.textContent = String(store.segments.length);
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
    if (!follow && !force) return;
    body.scrollTop = body.scrollHeight;
  }

  // -- incremental updates --------------------------------------------------

  function update(change) {
    if (!change) return;
    if (change.added) {
      const stick = follow && nearBottom();
      for (const seg of change.added) {
        if (filterSpeaker != null && seg.speaker !== filterSpeaker) continue;
        const lastRow = list.querySelector('.seg:last-of-type');
        const lastSeg = lastRow ? store.segById.get(Number(lastRow.dataset.seg)) : null;
        const newDay = !lastSeg || fmtDay(lastSeg.t_ms) !== fmtDay(seg.t_ms);
        if (newDay) {
          if (list.querySelector('.empty')) clear(list);
          list.append(h('div', { class: 'day-sep', text: fmtDayLabel(seg.t_ms) }));
        }
        // The live feed crosses conversation boundaries too, and a separator
        // that only ever appeared on a full repaint would be a boundary you
        // could only see by leaving the view and coming back.
        const sep = newDay ? null : threadSep(seg, lastSeg?.thread ?? null);
        if (sep) list.append(sep);
        list.append(segRow(seg, true));
      }
      // The window is bounded (store.MAX_SEGMENTS); drop rows the model dropped.
      while (list.querySelectorAll('.seg').length > store.segments.length) {
        const first = list.querySelector('.seg');
        if (!first) break;
        first.remove();
      }
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
    if (change.merged) renderAll(); // ids moved wholesale; a repaint is honest and rare
    if (change.relabel) refreshFilterOptions();
    // The pin arriving (or moving, after a merge) changes which rows are
    // marked as the user's. Guarded on the id itself, because `mic` and
    // `status` events also fire for things that change nothing here.
    if (change.mic && store.mic.you_speaker !== lastYou) {
      lastYou = store.mic.you_speaker;
      renderAll();
    }
    if (change.status || change.conn) refreshLiveChip();
  }

  refreshLiveChip();
  refreshFilterOptions();
  renderAll();

  return {
    update,
    /** Search results jump here: scroll the segment into view and mark it. */
    focusSegment(segId) {
      follow = false;
      followBtn.setAttribute('aria-pressed', 'false');
      followBtn.textContent = 'Follow';
      const row = list.querySelector(`.seg[data-seg="${segId}"]`);
      if (!row) {
        toast('That segment has scrolled out of the live window — search still finds it.', '');
        return false;
      }
      for (const el of list.querySelectorAll('.seg.hit')) el.classList.remove('hit');
      row.classList.add('hit');
      row.scrollIntoView({ block: 'center', behavior: 'smooth' });
      return true;
    },
    /**
     * "Show in transcript", from the speakers view or a segment sheet: the
     * existing filter is set to that voice and the view lands on the last thing
     * they said, because the question behind the click is almost always "what
     * have they been saying?".
     */
    focusSpeaker(spId) {
      follow = false;
      followBtn.setAttribute('aria-pressed', 'false');
      followBtn.textContent = 'Follow';
      setFilter(spId);
      const rows = list.querySelectorAll('.seg');
      const last = rows[rows.length - 1];
      if (last) {
        for (const el of list.querySelectorAll('.seg.hit')) el.classList.remove('hit');
        last.classList.add('hit');
        last.scrollIntoView({ block: 'center', behavior: 'smooth' });
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
      follow = false;
      followBtn.setAttribute('aria-pressed', 'false');
      followBtn.textContent = 'Follow';
      highlightThread = threadId;
      setFilter(null);
      const rows = [...list.querySelectorAll(`.seg[data-thread="${threadId}"]`)];
      for (const el of list.querySelectorAll('.seg.hit')) el.classList.remove('hit');
      rows[0]?.scrollIntoView({ block: 'center', behavior: 'smooth' });
      // Nothing on screen means the conversation is older than the window the
      // merge could reach; say so rather than leaving a still transcript.
      if (!rows.length) toast('That conversation is outside the loaded window.', '');
      return { thread: threadId, rows: rows.length };
    },
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
        text: isUncertain(seg)
          ? uncertainReason(seg)
          : isYou(seg.speaker)
            ? // No score, and there should not be one: nothing was compared.
              // Saying "matched at 0.00" here would be a lie about a fact the
              // daemon is more sure of than anything else in the transcript.
              `Recorded on your own microphone, so the speaker is not a guess. ${Math.round((seg.overlap_frac ?? 0) * 100)}% overlapped.`
            : `Matched at ${(seg.match_score ?? 0).toFixed(2)} confidence, ${Math.round((seg.overlap_frac ?? 0) * 100)}% overlapped.`,
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
