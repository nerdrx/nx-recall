import { h, clear, fmtDate, fmtClock } from '../lib/dom.js';
import { ask, segmentSpeakerLabel } from '../lib/store.js';
import { toast, openSheet, confirmSheet } from '../lib/sheets.js';
import { windowedHistory } from '../lib/windowed-history.js';

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

export function uniqueHistoryRows(rows, seen) {
  return rows.filter(row => { if (seen.has(row.id)) return false; seen.add(row.id); return true; });
}

export function mountArchive(root, ctx, digestRow) {
  let generation = 0;
  let selectedDay = localDay();
  let mode = null;
  let dead = false;
  let collectionFilter = 'all';
  let collections = [];
  const sourceRows = new Map();
  const purged = new Set();
  let revision = 0;
  let history = null;
  let relatedRefresh = null;
  const offsets = { moments: 0, searches: 0 };
  const button = (label, action, attrs = {}) => h('button', { class: 'btn small', onclick: action, ...attrs }, label);
  function error(box, message, retry) {
    clear(box); box.append(h('p', { role: 'alert', text: message }), button('Try again', retry));
  }
  function sourceRow(segment, attrs = {}, open = true) {
    if (purged.has(segment.id)) return null;
    segment = { ...segment };
    const row = h('article', { class: 'card archive-segment', ...attrs, dataset: { ...attrs.dataset, sourceSegment: segment.id } },
      h('p', { class: 'sub', dataset: { sourceAttribution: '' }, text: archiveAttribution(segment) }),
      h('p', { dataset: { sourceText: '' }, text: segment.text ?? '' }),
      open ? button('Open in transcript', () => ctx.jumpToSegment(segment)) : null);
    if (!sourceRows.has(segment.id)) sourceRows.set(segment.id,new Set());
    sourceRows.get(segment.id).add({node:row,segment});
    return row;
  }
  function update(change) {
    if (change?.purged?.length || change?.updated?.length) revision++;
    const referenced = change?.purged?.length ? new Set([...sourceRows.keys(), ...[...root.querySelectorAll('[data-source-ids]')].flatMap(row=>JSON.parse(row.dataset.sourceIds))]) : null;
    relatedRefresh?.(change);
    history?.update(change);
    for (const id of change?.purged ?? []) {
      if (referenced.has(id)) purged.add(id);
      for (const item of sourceRows.get(id) ?? []) item.node.remove();
      sourceRows.delete(id);
    }
    for (const segment of change?.updated ?? []) {
      purged.delete(segment.id);
      for (const item of sourceRows.get(segment.id) ?? []) {
        if (!item.node.isConnected) continue;
        Object.assign(item.segment,segment);
        item.node.querySelector('[data-source-text]').textContent = item.segment.text ?? '';
        item.node.querySelector('[data-source-attribution]').textContent = archiveAttribution(item.segment);
      }
    }
    if (change?.relabel) for (const entries of sourceRows.values()) for (const item of entries) {
      if (item.node.isConnected) item.node.querySelector('[data-source-attribution]').textContent = archiveAttribution(item.segment);
    }
    if (change?.purged?.length) {
      for (const row of document.querySelectorAll('[data-related-moment]')) {
        if (JSON.parse(row.dataset.sourceIds || '[]').some(id => change.purged.includes(id))) row.remove();
      }
      for (const row of root.querySelectorAll('[data-saved-kind="moments"]')) {
        if (JSON.parse(row.dataset.sourceIds || '[]').every(id=>purged.has(id))) row.remove();
      }
      const status = root.querySelector('#archive-history-status');
      const count = root.querySelectorAll('[data-history-id]').length;
      if (status) status.textContent = status.textContent.replace(/^\d+ turns? shown/,`${count} turn${count===1?'':'s'} shown`);
      const detail = document.getElementById('saved-moment-detail');
      if (detail) {
        const count = detail.querySelectorAll('[data-moment-segment]').length;
        document.getElementById('saved-moment-count').textContent = `${count} retained turns · deleted sources are removed immediately`;
        document.getElementById('saved-moment-play').disabled = count === 0;
      }
    }
    const historyStatus = root.querySelector('#archive-history-status');
    if (historyStatus && history) historyStatus.textContent = historyStatus.textContent.replace(/^\d+ turns? shown/, `${history.count} turn${history.count===1?'':'s'} shown`);
    pruneRows();
  }
  function pruneRows() {
    // Keep only rendered rows; cleared days and closed dialogs release their nodes.
    for (const [id,entries] of sourceRows) {
      for (const item of entries) if (!item.node.isConnected) entries.delete(item);
      if (!entries.size) sourceRows.delete(id);
    }
  }
  async function show(next) {
    mode = next;
    const ticket = ++generation;
    history?.destroy(); history = null;
    clear(root);
    pruneRows(); purged.clear();
    const content = h('div', { class: 'archive-content', 'aria-live': 'polite' });
    if (mode === 'day') {
      const input = h('input', { type: 'date', id: 'archive-day', value: selectedDay, 'aria-label': 'Archive day', onchange: () => {
        if (dayRange(input.value)) { selectedDay = input.value; void show('day'); }
      } });
      const move = (days) => { const date = new Date(`${selectedDay}T12:00:00`); date.setDate(date.getDate() + days); selectedDay = localDay(date); void show('day'); };
      root.append(h('div', { class: 'archive-toolbar' }, button('Previous day', () => move(-1)), input, button('Next day', () => move(1)), button('Today', () => { selectedDay = localDay(); void show('day'); })), content);
      content.append(h('p', { class: 'sub', text: 'Loading this day…' }));
      clear(content);
      const summaryBox = h('section', { class: 'archive-summaries' }, h('h2', { text: 'Conversations on this day' }), h('p', { class: 'sub', text: 'Loading summaries…' }));
      const historyList = h('div', { id: 'archive-history' });
      const historyStatus = h('p', { class: 'sub', id: 'archive-history-status', role: 'status' });
      const historyError = h('p', { role: 'alert', id: 'archive-history-error' });
      let cursor = null, loading = false, loaded = 0, finished = false;
      const seen = new Set();
      const range = dayRange(selectedDay);
      const more = button('Load more turns', () => void loadPage(), { id: 'archive-load-more' });
      const refresh = button('Refresh day', () => void show('day'), { id: 'archive-refresh' });
      content.append(summaryBox, h('h2', { text: 'Recorded words' }), h('p', { class: 'sub', text: 'Read this day in order. Use arrow keys on a turn to move between loaded turns; Home and End reach the first and last. Refresh the day to include new recordings.' }), historyList, historyStatus, historyError, more, refresh);
      history = windowedHistory(historyList, root.closest('.view-body') ?? root, segment => sourceRow(segment,{dataset:{historyId:segment.id}}), pruneRows);
      async function loadPage() {
        if (loading || finished || dead || ticket !== generation) return;
        loading = true; more.disabled = true; more.textContent = 'Loading…'; historyError.textContent = '';
        try {
          let reply, began;
          do { began = revision; reply = await ask('history.page', { ...range, limit: 100, ...(cursor ? { cursor } : {}) }); }
          while (began !== revision && !dead && ticket === generation);
          if (dead || ticket !== generation) return;
          const scroller = root.closest('.view-body');
          const scroll = scroller?.scrollTop;
          history.append(uniqueHistoryRows(reply.segments ?? [], seen).filter(segment => !purged.has(segment.id)));
          loaded = history.count;
          cursor = reply.next_cursor ?? null; finished = cursor == null;
          historyStatus.textContent = loaded ? `${loaded} turn${loaded === 1 ? '' : 's'} shown${finished ? ' · end of this day' : ' · more available'}` : 'No retained transcript for this day.';
          more.hidden = finished;
          if (scroller && scroll != null) scroller.scrollTop = scroll;
        } catch (e) {
          if (dead || ticket !== generation) return;
          historyError.textContent = `Recorded words could not be loaded — ${e.message}`;
          more.textContent = 'Retry recorded words';
        } finally {
          loading = false;
          if (!dead && ticket === generation) { more.disabled = false; if (!historyError.textContent) more.textContent = 'Load more turns'; }
        }
      }
      void ask('digest.list', { day: selectedDay, limit: 100 }).then(reply => {
        if (dead || ticket !== generation) return;
        clear(summaryBox); summaryBox.append(h('h2', { text: 'Conversations on this day' }));
        const digests = reply.digests ?? [];
        if (digests.length) summaryBox.append(h('div', { class: 'card digest-list' }, ...digests.map(digestRow)));
        else summaryBox.append(h('p', { class: 'sub', text: 'No summaries for this day. Recorded words remain available below.' }));
        if (digests.length >= 100) summaryBox.append(h('p', { class: 'sub', text: 'Showing the first 100 summaries; all retained turns can be browsed below.' }));
      }).catch(e => { if (!dead && ticket === generation) error(summaryBox, `Summaries could not be loaded — ${e.message}`, () => void show('day')); });
      await loadPage();
      return;
    }
    root.append(h('p', { class: 'sub', text: 'Saved locally. Removing a bookmark keeps the original transcript.' }), content);
    content.append(h('p', { class: 'sub', text: 'Loading saved items…' }));
    const began = revision;
    const results = await Promise.allSettled([
      ask('saved.moments.list', { limit: SAVED_PAGE, offset: offsets.moments, ...(collectionFilter === 'all' ? {} : { collection_id: collectionFilter === 'unfiled' ? null : Number(collectionFilter) }) }),
      ask('saved.searches.list', { limit: SAVED_PAGE, offset: offsets.searches }),
      ask('saved.collections.list'),
    ]);
    if (dead || ticket !== generation) return;
    if (began !== revision) { void show('saved'); return; }
    clear(content);
    if (results[2].status === 'fulfilled') {
      collections = results[2].value.collections ?? [];
      if (!['all','unfiled'].includes(collectionFilter) && !collections.some(c => String(c.id) === collectionFilter)) {
        collectionFilter = 'all'; offsets.moments = 0; void show('saved'); return;
      }
      const filter = h('select', { id: 'memory-collection', class: 'input', 'aria-label': 'Moment collection', onchange: () => { collectionFilter = filter.value; offsets.moments = 0; void show('saved'); } },
        h('option', { value: 'all', text: 'All moments' }), h('option', { value: 'unfiled', text: 'Unfiled' }),
        ...collections.map(c => h('option', { value: c.id, text: `${c.name} (${c.count})` })));
      filter.value = collectionFilter;
      const selected = collections.find(c => String(c.id) === collectionFilter);
      content.append(h('div', { class: 'archive-toolbar collection-toolbar' }, filter,
        button('New collection', () => editCollection(), { id: 'collection-new' }),
        button('Rename collection', () => editCollection(selected), { id: 'collection-rename', disabled: !selected }),
        button('Delete collection', async () => {
          if (!selected || !await confirmSheet({ title: `Delete ${selected.name}?`, body: 'Saved moments move to Unfiled. Their source turns and notes stay.', confirmLabel: 'Delete collection' })) return;
          try { await ask('saved.collections.delete', { id: selected.id }); collectionFilter = 'unfiled'; offsets.moments = 0; if (!dead && mode === 'saved') void show('saved'); }
          catch (e) { toast(`Could not delete the collection — ${e.message}`, 'error'); }
        }, { id: 'collection-delete', disabled: !selected })));
    } else content.append(h('p', { role: 'alert', text: `Collections could not be loaded — ${results[2].reason.message}` }));
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
        const row = h('article', { class: 'card saved-item', dataset: { savedId: record.id, savedKind: kind, ...(kind === 'moments' ? { sourceIds: JSON.stringify(record.segment_ids) } : {}) } });
        const title = kind === 'moments' ? record.title || 'Saved moment' : record.name;
        row.append(h('h3', { text: title }));
        if (kind === 'moments') {
          if (record.note) row.append(h('span', { class: 'sub', text: 'Personal note' }), h('p', { text: record.note }));
          const first = record.segments?.[0];
          if (first) row.append(h('span', { class: 'sub', text: 'Original transcript' }), sourceRow(first,{class:'saved-source-preview'},false));
          else row.append(h('p', { class: 'sub', text: 'The original words are no longer retained.' }));
        } else row.append(h('p', { text: record.query }));
        if (kind === 'moments' && record.unavailable_count > 0) row.append(h('p', { class: 'sub', text: `${record.unavailable_count} original turn(s) are no longer retained.` }));
        row.append(h('div', { class: 'archive-toolbar' },
          button(kind === 'moments' ? 'Open moment' : 'Run search', () => {
            if (kind === 'searches') ctx.go('search', { savedSearch: record });
            else if (record.segments?.length) void openMoment(record.id);
            else toast('The original words are no longer retained. Your saved title and note remain.');
          }, { disabled: kind === 'moments' && !record.segments?.length }),
          button('Edit', () => edit(kind, record)), button('Remove', async () => {
            if (!await confirmSheet({ title: `Remove ${title}?`, body: 'Only this saved item is removed. The original transcript stays.', confirmLabel: 'Remove' })) return;
            try { await ask(`saved.${kind}.delete`, { id: record.id }); if (!dead && mode === 'saved') void show('saved'); }
            catch (e) { toast(`Could not remove it — ${e.message}`, 'error'); }
          })));
        if (kind === 'moments') row.append(button('Move to collection', () => moveMoment(record), { dataset: { moveMoment: record.id } }));
        box.append(row);
      }
    }
  }
  async function openMoment(id) {
    const ticket = generation;
    let reply, began;
    try { do { began = revision; reply = await ask('saved.moments.get', { id }); } while (began !== revision && !dead && ticket === generation); }
    catch (e) { toast(`Could not open this moment — ${e.message}`, 'error'); return; }
    if (dead || mode !== 'saved' || ticket !== generation) return;
    const record = reply.moment;
    let relatedAlive = true, relatedRequest = 0;
    const relatedSourceIds = new Set(record.segment_ids);
    const related = h('section', { id: 'saved-moment-related', 'aria-label': 'Related saved moments' });
    let closeCurrent;
    openSheet(close => { closeCurrent=close; return [h('h2', { text: record.title || 'Saved moment' }),
      record.note ? h('div', {}, h('span', { class: 'sub', text: 'Personal note' }), h('p', { text: record.note })) : null,
      h('p', { class: 'sub', id: 'saved-moment-count', text: `${record.segments.filter(s=>!purged.has(s.id)).length} retained turns · playback includes only this saved range` }),
      record.unavailable_count ? h('p', { class: 'sub', text: `${record.unavailable_count} original turn(s) no longer retained.` }) : null,
      h('div', { id: 'saved-moment-detail' }, ...record.segments.map(segment => { const row=sourceRow(segment,{dataset:{momentSegment:segment.id}},false); if(row) row.append(button('Open in transcript',()=>{close();ctx.jumpToSegment(segment);})); return row; })),
      related,
      h('div', { class: 'sheet-actions' }, button('Close', () => close()),
        button('Play saved range', () => { close(); void ctx.replayMoment?.(record.id); }, { id: 'saved-moment-play' }))]; }, { onClose: () => { relatedAlive=false; relatedRefresh=null; pruneRows(); } });
    async function loadRelated() {
      const request=++relatedRequest;
      clear(related); related.append(h('h3',{text:'Related saved moments'}), h('p',{class:'sub',text:'Finding shared words in original transcripts…'}));
      try {
        let reply, began;
        do { began=revision; reply=await ask('saved.moments.related',{id,limit:5}); } while(began!==revision && relatedAlive && !dead && ticket===generation);
        if (!relatedAlive || dead || ticket!==generation || request!==relatedRequest) return;
        clear(related); related.append(h('h3',{text:'Related saved moments'}),h('p',{class:'sub',text:'Lightweight suggestions from shared original words, without a model or personal notes. Up to 24 query words and 200 matching candidates are considered; results are not exhaustive.'}));
        if (!reply.available) related.append(h('p',{class:'sub',text:'Related moments are unavailable.'}));
        else if (!reply.moments?.length) related.append(h('p',{class:'sub',text:'No related saved moments found.'}));
        relatedSourceIds.clear(); record.segment_ids.forEach(id=>relatedSourceIds.add(id));
        for (const moment of reply.moments ?? []) {
          moment.segments?.forEach(segment=>relatedSourceIds.add(segment.id));
          const row=h('article',{class:'card saved-item',dataset:{relatedMoment:moment.id,sourceIds:JSON.stringify(moment.segments?.map(s=>s.id)??[])}},h('h4',{text:moment.title || 'Saved moment'}),h('p',{class:'sub',text:moment.reason || 'Shared original words'}));
          const first=moment.segments?.find(segment=>!purged.has(segment.id));
          if (!first) continue;
          row.append(sourceRow(first,{},false),button('Open related moment',()=>{closeCurrent();void openMoment(moment.id);}));related.append(row);
        }
      } catch(e) { if(relatedAlive && !dead && ticket===generation && request===relatedRequest)error(related,`Related moments could not be loaded — ${e.message}`,()=>void loadRelated()); }
    }
    relatedRefresh = change => { if ([...(change?.purged??[]),...(change?.updated??[]).map(row=>row.id)].some(id=>relatedSourceIds.has(id))) void loadRelated(); };
    void loadRelated();
  }
  function editCollection(record = null) {
    openSheet(close => {
      const name = h('input', { id: 'collection-name', class: 'input', value: record?.name ?? '', maxlength: '120' });
      const problem = h('p', { role: 'alert' });
      const save = button('Save collection', async () => {
        if (!name.value.trim()) { problem.textContent = 'Give the collection a name.'; return; }
        save.disabled = true;
        try {
          const reply = await ask('saved.collections.save', { ...(record ? { id: record.id } : {}), name: name.value.trim() });
          close(); collectionFilter = String(reply.collection.id); offsets.moments = 0; if (!dead && mode === 'saved') void show('saved');
        } catch (e) { problem.textContent = e.message; save.disabled = false; }
      }, { id: 'collection-save' });
      return [h('h2', { text: record ? 'Rename collection' : 'New collection' }), h('label', { for: 'collection-name', text: 'Name' }), name, problem,
        h('div', { class: 'sheet-actions' }, button('Cancel', () => close()), save)];
    });
  }
  async function moveMoment(record) {
    let available;
    try { available = (await ask('saved.collections.list')).collections ?? []; }
    catch (e) { toast(e.message, 'error'); return; }
    if (dead || mode !== 'saved') return;
    openSheet(close => {
      const select = h('select', { class: 'input', id: 'moment-collection' }, h('option', { value: '', text: 'Unfiled' }), ...available.map(c => h('option', { value: c.id, text: c.name })));
      select.value = record.collection_id == null ? '' : String(record.collection_id);
      const problem = h('p', { role: 'alert' });
      const move = button('Move moment', async () => {
        move.disabled = true;
        try { await ask('saved.moments.move', { id: record.id, collection_id: select.value ? Number(select.value) : null }); close(); if (!dead && mode === 'saved') void show('saved'); }
        catch (e) { problem.textContent = e.message; move.disabled = false; }
      }, { id: 'moment-move' });
      return [h('h2', { text: 'Move saved moment' }), h('label', { for: 'moment-collection', text: 'Collection' }), select,
        h('p', { class: 'sub', text: 'Create a collection from Memory if you need a new destination.' }), problem,
        h('div', { class: 'sheet-actions' }, button('Cancel', () => close()), move)];
    });
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
  return { show, update, hide() { mode = null; generation++; history?.destroy(); history=null; }, destroy() { dead = true; generation++; history?.destroy(); history=null; sourceRows.clear(); purged.clear(); } };
}
