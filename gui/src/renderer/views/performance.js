import { h } from '../lib/dom.js';
import { ask } from '../lib/store.js';

export function formatLatency(value) {
  if (typeof value !== 'number' || !Number.isFinite(value) || value < 0) return '—';
  return value >= 1000 ? `${(value / 1000).toFixed(2)} s` : `${Math.round(value)} ms`;
}
export function mountPerformance() {
  const status = h('p', { class: 'sub', role: 'status', text: 'Open Performance to load measurements.' });
  const content = h('div', { class: 'performance-grid' });
  let active = false, destroyed = false, timer = null, generation = 0;
  const retry = h('button', { class: 'btn', text: 'Refresh', onclick: () => { clearTimeout(timer); void refresh(); } });
  const panel = h('section', {
    id: 'settings-performance', class: 'settings-panel', role: 'tabpanel',
    'aria-labelledby': 'settings-tab-performance', dataset: { settingsPanel: 'performance' }, hidden: true,
  }, h('header', { class: 'settings-panel-head' },
    h('h2', { class: 'settings-group-title', text: 'Performance' }),
    h('p', { class: 'sub', text: 'Measured locally. Counters reset when the recording service restarts.' })), status, retry, content);
  function latency(title, data, description) {
    const count = data?.samples ?? 0;
    return h('section', { class: 'card' }, h('h3', { class: 'card-title', text: title }),
      h('p', { class: 'performance-value', text: formatLatency(data?.p50_ms) }),
      h('p', { class: 'sub', text: count ? `Median · ${count} recent samples (up to ${data.window ?? 256})` : 'No measurements yet' }),
      h('dl', { class: 'settings-shortcuts' },
        h('dt', { text: '95th percentile' }), h('dd', { text: formatLatency(data?.p95_ms) }),
        h('dt', { text: 'Latest' }), h('dd', { text: formatLatency(data?.latest_ms) })),
      h('p', { class: 'sub', text: description }));
  }
  async function refresh() {
    if (!active || destroyed) return;
    const ticket = ++generation;
    retry.disabled = true;
    if (!content.children.length) status.textContent = 'Loading measurements…';
    try {
      const data = await ask('performance.get', {});
      if (destroyed || !active || ticket !== generation) return;
      const q = data.queue;
      const repair = data.repair;
      const states = { idle: 'Up to date', running: 'Repairing', paused: 'Paused', waiting_capture: 'Giving recording priority', waiting_foreground: 'Waiting for active work', waiting_store: 'Waiting for database access', backoff: 'Will retry shortly', stopped: 'Stopped' };
      content.replaceChildren(
        latency('Audio to transcript', data.capture, 'From the end of a recorded turn to its stored transcript. Includes waiting and recognition; excludes screen rendering.'),
        latency('Search response', data.search, 'Time spent serving search requests, including answers and failed requests. Excludes network and screen rendering.'),
        h('section', { class: 'card' }, h('h3', { class: 'card-title', text: 'Recording queue' }),
          h('p', { class: 'performance-value', text: q ? `${Number(q.seconds).toFixed(2)} s` : '—' }),
          h('p', { class: 'sub', text: q ? `${Number(q.capacity_seconds).toFixed(1)} s capacity · ${q.dropped_chunks} dropped buffers (${Number(q.dropped_seconds).toFixed(2)} s)` : 'Recording queue is not attached.' }),
          h('p', { class: 'sub', text: 'Dropped audio was lost before transcription. These counters cover this service session.' })),
        h('section', { class: 'card' }, h('h3', { class: 'card-title', text: 'Recording service memory' }),
          h('p', { class: 'performance-value', text: Number.isFinite(data.resident_bytes) ? `${Math.round(data.resident_bytes / 1048576)} MiB` : '—' }),
          h('p', { class: 'sub', text: 'Resident memory of the recording service. The desktop window and separately running models are excluded.' })),
        h('section', { class: 'card' }, h('h3', { class: 'card-title', text: 'Semantic search index' }),
          h('p', { class: 'performance-value performance-state', text: repair ? (states[repair.state] ?? 'Waiting') : 'Model not loaded' }),
          h('p', { class: 'sub', text: repair ? `${repair.pending ?? 'Unknown'} pending at last check · ${repair.completed ?? 0} repaired this session · ${repair.failed_attempts ?? 0} failed attempts` : 'Semantic search needs its optional local model.' }),
          h('p', { class: 'sub', text: 'Repairs resume in the background and give recording priority.' })),
      );
      status.textContent = `Updated ${new Date().toLocaleTimeString()}. Refreshes every 3 seconds while open.`;
    } catch (error) {
      if (!destroyed && active && ticket === generation) status.textContent = `Measurements unavailable. Previous values may be stale. ${error.message}`;
    } finally {
      if (!destroyed && active && ticket === generation) {
        retry.disabled = false;
        timer = setTimeout(refresh, 3000);
      }
    }
  }
  return { panel, setActive(value) { active = value; ++generation; clearTimeout(timer); retry.disabled = false; if (active) void refresh(); },
    destroy() { destroyed = true; active = false; ++generation; clearTimeout(timer); } };
}
