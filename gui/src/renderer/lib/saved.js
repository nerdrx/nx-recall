// User-created moments point at captured turns; this dialog never copies audio.
import { h, fmtClock } from './dom.js';
import { ask, segmentSpeakerLabel } from './store.js';
import { openSheet, toast } from './sheets.js';

export function selectedMomentIds(rows, start, end) {
  const a = rows.findIndex(r => r.id === Number(start));
  const b = rows.findIndex(r => r.id === Number(end));
  if (a < 0 || b < a || b - a >= 20) throw new Error('Choose up to 20 consecutive turns, in order.');
  return rows.slice(a, b + 1).map(r => r.id);
}

export async function saveMomentSheet(segmentIds, { title = '', note = '', id = null } = {}) {
  let rows = [];
  const original = [...segmentIds];
  if (original.length === 1 && id == null) {
    try {
      const res = await ask('segments.context', { id: original[0], before: 10, after: 10 });
      rows = res.segments ?? [];
    } catch (e) { toast(e.message || 'This moment is no longer available.', 'error'); return null; }
  }
  return new Promise(resolve => {
    let settled = false;
    const finish = value => { if (!settled) { settled = true; resolve(value); } };
    let busy = false;
    const close = openSheet(close => {
      const name = h('input', { class: 'input', id: 'moment-title', maxlength: '180', value: title, placeholder: 'Optional title' });
      const annotation = h('textarea', { class: 'input', id: 'moment-note', maxlength: '4096', rows: '3', placeholder: 'Your own note — separate from the transcript' });
      annotation.value = note;
      const error = h('p', { class: 'sub', role: 'alert', id: 'moment-error' });
      const choice = (key, value) => h('select', { class: 'input', id: key }, ...rows.map(r => h('option', { value: String(r.id), selected: r.id === value }, `${fmtClock(r.t_ms)} · ${segmentSpeakerLabel(r)} · ${(r.text ?? '').slice(0, 55)}`)));
      const start = choice('moment-start', original[0]);
      const end = choice('moment-end', original.at(-1));
      const submit = h('button', { class: 'btn primary', id: 'moment-save', onclick: async () => {
        if (busy) return;
        try {
          const ids = rows.length ? selectedMomentIds(rows, start.value, end.value) : original;
          busy = true; submit.disabled = true; error.textContent = '';
          const res = await ask('saved.moments.save', { ...(id != null ? { id } : {}), segment_ids: ids, title: name.value.trim(), note: annotation.value.trim() });
          finish(res.moment); close(res.moment); toast('Moment saved in Memory.');
        } catch (e) { error.textContent = e.message || 'Could not save this moment.'; }
        finally { busy = false; submit.disabled = false; }
      } }, id == null ? 'Save moment' : 'Save changes');
      return [h('h2', { text: id == null ? 'Save a moment' : 'Edit saved moment' }),
        h('p', { class: 'sub', text: 'Keep a link to these turns. Audio follows your retention settings; deleted sources are not kept by saving.' }),
        h('label', { for: 'moment-title', text: 'Title' }), name,
        rows.length ? h('div', { class: 'moment-range' }, h('label', { for: 'moment-start' }, 'From', start), h('label', { for: 'moment-end' }, 'Through', end)) : h('p', { class: 'sub', text: `${original.length} source turn${original.length === 1 ? '' : 's'}` }),
        h('label', { for: 'moment-note', text: 'Personal note' }), annotation, error,
        h('div', { class: 'actions' }, h('button', { class: 'btn', onclick: () => close(null) }, 'Cancel'), submit)];
    }, { onClose: value => finish(value ?? null) });
  });
}
