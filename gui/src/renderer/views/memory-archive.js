import { h, clear, fmtDate, fmtClock } from '../lib/dom.js';
import { ask, segmentSpeakerLabel } from '../lib/store.js';
import { toast, openSheet, confirmSheet } from '../lib/sheets.js';

// Local calendar boundaries deliberately use setDate, including DST days.
export function dayRange(day) {
  if (!/^\d{4}-\d{2}-\d{2}$/.test(day)) return null;
  const start = new Date(`${day}T00:00:00`);
  if (!Number.isFinite(start.getTime()) || localDay(start) !== day) return null;
  const end = new Date(start); end.setDate(end.getDate() + 1);
  return { from: start.toISOString(), to: end.toISOString() };
}
export function localDay(date = new Date()) {
  return `${date.getFullYear()}-${String(date.getMonth() + 1).padStart(2, '0')}-${String(date.getDate()).padStart(2, '0')}`;
}

const SAVED_PAGE = 100;
export function savedPageInfo(reply, kind, offset = 0) {
  const count = reply[kind]?.length ?? 0;
  const total = Number.isFinite(reply.total) ? reply.total : null;
  return { previous: Math.max(0, offset - SAVED_PAGE),
    next: offset + count, hasMore: count > 0 && (total == null ? count === SAVED_PAGE : offset + count < total),
    label: count ? `${offset + 1}–${offset + count}${total == null ? '' : ` of ${total}`}` : 'No saved items' };
}
export function archiveAttribution(segment) {
  const when = Number.isFinite(segment.t_ms) ? `${fmtDate(new Date(segment.t_ms).toISOString())} · ${fmtClock(segment.t_ms)}` : 'Time unavailable';
  return `${when} · ${segmentSpeakerLabel(segment)} · ${segment.source || 'Unknown source'}`;
}

export function mountArchive(root, ctx, digestRow) {
  let generation = 0;
  let selectedDay = localDay();
  let mode = null;
  let dead = false;
  const offsets = { moments: 0, searches: 0 };
  const button = (label, action, attrs = {}) => h('button', { class: 'btn small', onclick: action, ...attrs }, label);
  function error(box, message, retry) {
    clear(box); box.append(h('p', { role: 'alert', text: message }), button('Try again', retry));
  }
  async function show(next) {
    mode = next;
    const ticket = ++generation;
    clear(root);
    const content = h('div', { class: 'archive-content', 'aria-live': 'polite' });
    if (mode === 'day') {
      const input = h('input', { type: 'date', id: 'archive-day', value: selectedDay, 'aria-label': 'Archive day', onchange: () => {
        if (dayRange(input.value)) { selectedDay = input.value; void show('day'); }
      } });
      const move = (days) => { const date = new Date(`${selectedDay}T12:00:00`); date.setDate(date.getDate() + days); selectedDay = localDay(date); void show('day'); };
      root.append(h('div', { class: 'archive-toolbar' }, button('Previous day', () => move(-1)), input, button('Next day', () => move(1)), button('Today', () => { selectedDay = localDay(); void show('day'); })), content);
      content.append(h('p', { class: 'sub', text: 'Loading this day…' }));
      try {
        const range = dayRange(selectedDay);
        const [summary, transcript] = await Promise.allSettled([
          ask('digest.list', { day: selectedDay, limit: 100 }),
          ask('transcript', { ...range, limit: 200 }),
        ]);
        if (dead || ticket !== generation) return;
        clear(content);
        const digests = summary.status === 'fulfilled' ? summary.value.digests ?? [] : [];
        const segments = transcript.status === 'fulfilled' ? transcript.value.segments ?? [] : [];
        if (summary.status === 'rejected' && transcript.status === 'rejected') throw transcript.reason;
        content.append(h('h2', { text: 'Conversations on this day' }));
        if (digests.length) content.append(h('div', { class: 'card digest-list' }, ...digests.map(digestRow)));
        else if (summary.status === 'rejected') content.append(h('p', { role: 'alert', text: `Summaries could not be loaded — ${summary.reason.message}` }), button('Retry summaries', () => void show('day')));
        else content.append(h('p', { class: 'sub', text: 'No summaries for this day. Recorded words are available below when retained.' }));
        content.append(h('h2', { text: 'Recorded words' }));
        if (transcript.status === 'rejected') content.append(h('p', { role: 'alert', text: `Recorded words could not be loaded — ${transcript.reason.message}` }), button('Retry recorded words', () => void show('day')));
        else if (!segments.length) content.append(h('p', { class: 'empty', text: 'No retained transcript for this day.' }));
        for (const segment of segments) content.append(h('div', { class: 'card archive-segment' },
          h('span', { class: 'sub', text: archiveAttribution(segment) }),
          h('p', { text: segment.text ?? '' }), button('Open in transcript', () => ctx.jumpToSegment(segment))));
        if (segments.length >= 200 || digests.length >= 100) content.append(h('p', { class: 'sub', text: 'Showing a preview of this day. Open Search to explore all retained words.' }),
          button('Search this day', () => ctx.go('search', { savedSearch: { query: '', filters: { date: { kind: 'fixed', from: selectedDay, to: selectedDay } } } })));
      } catch (e) { if (!dead && ticket === generation) error(content, `Could not load this day — ${e.message}`, () => void show('day')); }
      return;
    }
    root.append(h('p', { class: 'sub', text: 'Saved locally. Removing a bookmark keeps the original transcript.' }), content);
    content.append(h('p', { class: 'sub', text: 'Loading saved items…' }));
    const results = await Promise.allSettled([
      ask('saved.moments.list', { limit: SAVED_PAGE, offset: offsets.moments }),
      ask('saved.searches.list', { limit: SAVED_PAGE, offset: offsets.searches }),
    ]);
    if (dead || ticket !== generation) return;
    clear(content);
    for (const [index, kind, label] of [[0, 'moments', 'Saved moments'], [1, 'searches', 'Saved searches']]) {
      const box = h('section', { class: 'saved-group', dataset: { savedGroup: kind } }, h('h2', { text: label }));
      content.append(box);
      const result = results[index];
      if (result.status === 'rejected') { error(box, `${label} could not be loaded — ${result.reason.message}`, () => void show('saved')); continue; }
      const rows = result.value[kind] ?? [];
      // A delete on the last page returns to the preceding page, so an
      // otherwise populated archive never strands the reader on an empty page.
      if (!rows.length && offsets[kind] > 0) { offsets[kind] = Math.max(0, offsets[kind] - SAVED_PAGE); void show('saved'); return; }
      const paging = savedPageInfo(result.value, kind, offsets[kind]);
      if (rows.length) box.append(h('div', { class: 'archive-toolbar', dataset: { savedPaging: kind } },
        button('Previous page', () => { offsets[kind] = paging.previous; void show('saved'); }, { disabled: offsets[kind] === 0 }),
        h('span', { class: 'sub', text: paging.label }),
        button('Next page', () => { offsets[kind] = paging.next; void show('saved'); }, { disabled: !paging.hasMore })));
      if (!rows.length) box.append(h('p', { class: 'empty', text: kind === 'moments' ? 'Save a moment from a transcript turn or search result to keep it here.' : 'Save a search from Search to return to its query and filters.' }));
      for (const record of rows) {
        const row = h('article', { class: 'card saved-item', dataset: { savedId: record.id, savedKind: kind } });
        const title = kind === 'moments' ? record.title || 'Saved moment' : record.name;
        row.append(h('h3', { text: title }));
        if (kind === 'moments') {
          if (record.note) row.append(h('span', { class: 'sub', text: 'Personal note' }), h('p', { text: record.note }));
          const first = record.segments?.[0];
          if (first) row.append(h('span', { class: 'sub', text: `Original transcript · ${archiveAttribution(first)}` }), h('p', { text: first.text ?? '' }));
          else row.append(h('p', { class: 'sub', text: 'The original words are no longer retained.' }));
        } else row.append(h('p', { text: record.query }));
        if (kind === 'moments' && record.unavailable_count > 0) row.append(h('p', { class: 'sub', text: `${record.unavailable_count} original turn(s) are no longer retained.` }));
        row.append(h('div', { class: 'archive-toolbar' },
          button(kind === 'moments' ? 'Open moment' : 'Run search', () => {
            if (kind === 'searches') ctx.go('search', { savedSearch: record });
            else if (record.segments?.length) ctx.jumpToSegment(record.segments[0]);
            else toast('The original words are no longer retained. Your saved title and note remain.');
          }, { disabled: kind === 'moments' && !record.segments?.length }),
          button('Edit', () => edit(kind, record)), button('Remove', async () => {
            if (!await confirmSheet({ title: `Remove ${title}?`, body: 'Only this saved item is removed. The original transcript stays.', confirmLabel: 'Remove' })) return;
            try { await ask(`saved.${kind}.delete`, { id: record.id }); if (!dead && mode === 'saved') void show('saved'); }
            catch (e) { toast(`Could not remove it — ${e.message}`, 'error'); }
          })));
        box.append(row);
      }
    }
  }
  function edit(kind, record) {
    openSheet((close) => {
      const title = h('input', { id: 'saved-edit-title', value: kind === 'moments' ? record.title ?? '' : record.name, maxlength: kind === 'moments' ? '180' : '120' });
      const note = h('textarea', { id: 'saved-edit-note', rows: '4', maxlength: '4096', text: record.note ?? '' });
      const status = h('p', { class: 'sub', role: 'alert' });
      const save = button('Save', async () => {
        if (kind === 'searches' && !title.value.trim()) { status.textContent = 'Give this search a name.'; title.focus(); return; }
        save.disabled = true;
        try {
          await ask(`saved.${kind}.save`, kind === 'moments' ? { id: record.id, segment_ids: record.segment_ids, title: title.value.trim(), note: note.value } : { id: record.id, name: title.value.trim(), query: record.query, filters: record.filters });
          close(); if (!dead && mode === 'saved') void show('saved');
        } catch (e) { status.textContent = `Could not save — ${e.message}`; save.disabled = false; }
      });
      return [h('h2', { text: kind === 'moments' ? 'Edit saved moment' : 'Rename saved search' }), h('label', { for: 'saved-edit-title', text: 'Title' }), title,
        kind === 'moments' ? h('label', { for: 'saved-edit-note', text: 'Note' }) : null, kind === 'moments' ? note : null, status,
        h('div', { class: 'sheet-actions' }, button('Cancel', () => close()), save)];
    });
  }
  return { show, destroy() { dead = true; generation++; } };
}
