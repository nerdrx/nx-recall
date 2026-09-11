import test from 'node:test';
import assert from 'node:assert/strict';
import { hasSearchIntent, resolveSearchDate, searchDateParams, withoutSearchDates, savedSearchFilters, restoreSavedSearch } from '../src/renderer/views/search.js';

test('all-history drops dates without changing query, speaker, world, source or mode', () => {
  const input = { query: 'portal', speaker_id: 7, source: 'VRChat.exe', world_id: 'wrld_x', mode: 'hybrid', from_ns: '12', to_ns: '34', from_ms: 0, to_ms: 1 };
  assert.deepEqual(withoutSearchDates(input), { query: 'portal', speaker_id: 7, source: 'VRChat.exe', world_id: 'wrld_x', mode: 'hybrid' });
  assert.equal(input.from_ns, '12', 'does not mutate a saved record');
});
test('rolling saved searches resolve at reopen time', () => {
  const record = { query: 'portal', filters: { date: { kind: 'rolling', days: 7 }, speaker: '7', source: 'mic', mode: 'both' } };
  const restored = restoreSavedSearch(record, new Date(2026, 9, 12, 12));
  assert.equal(restored.from, '2026-10-05'); assert.equal(restored.to, '2026-10-12');
  assert.equal(restored.speaker, '7'); assert.equal(restored.source, 'mic'); assert.equal(restored.mode, 'both');
});
test('fixed saved searches retain their exact calendar dates', () => {
  const date = { kind: 'fixed', from: '2025-02-01', to: '2025-02-02' };
  assert.deepEqual(resolveSearchDate(date, new Date(2030, 1, 1)), { from: '2025-02-01', to: '2025-02-02' });
  assert.deepEqual(resolveSearchDate({ kind: 'all' }), { from: '', to: '' });
});
test('calendar end is exclusive next local midnight, including milliseconds in the last second', () => {
  const p = searchDateParams({ from: '2026-09-11', to: '2026-09-11' });
  assert.equal(Date.parse(p.from), new Date(2026, 8, 11).getTime());
  assert.equal(Date.parse(p.to), new Date(2026, 8, 12).getTime());
  assert.deepEqual(searchDateParams({}), {});
});
test('rolling save strips default parsed bounds but retains explicitly asked dates', () => {
  const state = { date: { kind: 'rolling', days: 7 }, asked: { query: 'portal', date_explicit: false, from_ns: '123', to_ns: '456', source: 'mic' } };
  const filters = savedSearchFilters(state);
  assert.equal(filters.asked.from_ns, undefined); assert.equal(filters.asked.source, 'mic');
  state.asked.date_explicit = true;
  assert.equal(savedSearchFilters(state).asked.from_ns, '123');
});
test('saved search restores an independent interpretation without a stale answer', () => {
  const record = { query: 'what yesterday', filters: { date: { kind: 'all' }, asked: { query: '', from_ns: '100', date_explicit: true } } };
  const restored = restoreSavedSearch(record);
  restored.asked.query = 'changed';
  assert.equal(record.filters.asked.query, '');
  assert.equal(restored.answer, null); assert.equal(restored.refused, null);
});

test('removing the last world facet returns to the prompt despite default dates', () => {
  const state = { q: '', world: 'wrld_pug', from: '2026-09-04', to: '2026-09-11', date: { kind: 'rolling', days: 7 } };
  assert.equal(hasSearchIntent(state), true);
  state.world = '';
  assert.equal(hasSearchIntent(state), false);
  assert.equal(hasSearchIntent({ ...state, speaker: '7' }), true, 'an explicit person still makes a browse');
});
test('a chosen archive day and a saved rolling browse work without keywords', () => {
  const state = { q: '', from: '2026-09-11', to: '2026-09-11', date: { kind: 'fixed' } };
  assert.equal(hasSearchIntent(state), true);
  assert.equal(hasSearchIntent({ ...state, date: { kind: 'rolling' } }, { saved: true }), true);
  assert.equal(hasSearchIntent({ q: '  ', date: { kind: 'all' } }), false, 'an empty all-history query does not scan everything');
});
