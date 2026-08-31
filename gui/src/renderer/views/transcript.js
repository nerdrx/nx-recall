// Live transcript — the view the app is for. Segments stream in as they are
// transcribed; anything the pipeline itself refused to identify is visibly
// muted and carries a "?" that says why; clicking any segment opens the
// reassign/correct sheet (PROTOCOL segments.reassign / segments.correct).

import { h, clear, fmtClock, fmtDay, fmtDayLabel, speakerColor } from '../lib/dom.js';
import { store, speakerLabel, isUncertain, uncertainReason, ask } from '../lib/store.js';
import { openSheet, toast } from '../lib/sheets.js';

export const id = 'transcript';

export function mount(root, ctx) {
  let follow = true;
  let filterSpeaker = null;

  const list = h('div', { class: 'seg-list', id: 'seg-list' });
  const card = h('div', { class: 'card' }, list);
  const body = h('div', { class: 'view-body view-enter', id: 'transcript-body' }, card);

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
    for (const seg of rows) {
      const d = fmtDay(seg.t_ms);
      if (d !== day) {
        day = d;
        list.append(h('div', { class: 'day-sep', text: fmtDayLabel(seg.t_ms) }));
      }
      list.append(segRow(seg));
    }
    updateCount();
    scrollToEnd(true);
  }

  function segRow(seg, isNew = false) {
    const uncertain = isUncertain(seg);
    const color = speakerColor(seg.speaker);
    const row = h('div', {
      class: `seg${uncertain ? ' uncertain' : ''}${isNew ? ' new' : ''}${seg.corrected ? ' corrected' : ''}`,
      dataset: { seg: String(seg.id) },
      role: 'button',
      tabindex: '0',
      title: 'Click to reassign or correct this segment',
    });
    const who = h(
      'span',
      { class: 'who', ...(seg.speaker != null ? { dataset: { sp: String(seg.speaker) } } : {}) },
      h('span', { class: 'dot', style: `color:${color}` }),
      h('span', { class: 'nm', text: speakerLabel(seg.speaker), style: seg.speaker == null ? '' : `color:${color}` })
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
      liveChip.className = 'chip warn';
      liveChip.append(h('span', { class: 'dot' }), 'paused');
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
        if (!lastSeg || fmtDay(lastSeg.t_ms) !== fmtDay(seg.t_ms)) {
          if (list.querySelector('.empty')) clear(list);
          list.append(h('div', { class: 'day-sep', text: fmtDayLabel(seg.t_ms) }));
        }
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
    refreshLiveChip,
  };
}

// ---------------------------------------------------------------------------
// the reassign / correct sheet
// ---------------------------------------------------------------------------

export function openSegmentSheet(seg, ctx) {
  let picked = seg.speaker ?? null;

  openSheet((close) => {
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

    return [
      h('h2', { text: 'Reassign or correct' }),
      h('p', {
        class: 'sub',
        text: isUncertain(seg)
          ? uncertainReason(seg)
          : `Matched at ${(seg.match_score ?? 0).toFixed(2)} confidence, ${Math.round((seg.overlap_frac ?? 0) * 100)}% overlapped.`,
      }),
      h('div', { class: 'quote', text: `${fmtClock(seg.t_ms)} · ${seg.source ?? 'unknown source'}` }),
      h('div', { class: 'card-title', text: 'Who said this' }),
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
  });
  void ctx;
}
