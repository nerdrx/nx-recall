import test from 'node:test';
import assert from 'node:assert/strict';
import { nextSearchResult, searchCountLabel, hasSearchIntent, resolveSearchDate, searchDateParams, withoutSearchDates, savedSearchFilters, restoreSavedSearch } from '../src/renderer/views/search.js';

test('all-history drops dates without changing query, speaker, world, source or mode', () => {
  const input = { query: 'portal', speaker_id: 7, source: 'VRChat.exe', world_id: 'wrld_x', mode: 'hybrid', from_ns: '12', to_ns: '34', from_ms: 0, to_ms: 1 };
  assert.deepEqual(withoutSearchDates(input), { query: 'portal', speaker_id: 7, source: 'VRChat.exe', world_id: 'wrld_x', mode: 'hybrid' });
  assert.equal(input.from_ns, '12', 'does not mutate a saved record');
});
test('rolling saved searches resolve at reopen time', () => {
  const record = { query: 'portal', filters: { date: { kind: 'rolling', days: 7 }, speaker: '7', source: 'mic', mode: 'both' } };
  const restored = restoreSavedSearch(record, new Date(2026, 9, 12, 12));
  assert.equal(restored.from, '2026-10-06'); assert.equal(restored.to, '2026-10-12');
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

test('rolling calendar days include today exactly once', () => {
  const now = new Date(2026, 8, 11, 12);
  assert.deepEqual(resolveSearchDate({ kind: 'rolling', days: 1 }, now), { from: '2026-09-11', to: '2026-09-11' });
  assert.deepEqual(resolveSearchDate({ kind: 'rolling', days: 7 }, now), { from: '2026-09-05', to: '2026-09-11' });
  for (const days of [undefined, 0, -1, 1.5, '7', NaN, Infinity, 3651]) {
    assert.deepEqual(resolveSearchDate({ kind: 'rolling', days }, now), { from: '2026-09-05', to: '2026-09-11' });
  }
});
test('rolling days cover complete local calendar days across both DST changes', () => {
  const old = process.env.TZ;
  process.env.TZ = 'Europe/Berlin';
  try {
    const spring = new Date(2026, 2, 29, 12);
    const oneDay = searchDateParams(resolveSearchDate({ kind: 'rolling', days: 1 }, spring));
    assert.deepEqual(oneDay, { from: '2026-03-28T23:00:00.000Z', to: '2026-03-29T22:00:00.000Z' });
    const springWeek = resolveSearchDate({ kind: 'rolling', days: 7 }, spring);
    assert.deepEqual(springWeek, { from: '2026-03-23', to: '2026-03-29' });
    const springBounds = searchDateParams(springWeek);
    assert.equal((Date.parse(springBounds.to) - Date.parse(springBounds.from)) / 3600000, 167);
    const autumnWeek = resolveSearchDate({ kind: 'rolling', days: 7 }, new Date(2026, 9, 25, 12));
    assert.deepEqual(autumnWeek, { from: '2026-10-19', to: '2026-10-25' });
    const autumnBounds = searchDateParams(autumnWeek);
    assert.equal((Date.parse(autumnBounds.to) - Date.parse(autumnBounds.from)) / 3600000, 169);
  } finally {
    if (old == null) delete process.env.TZ; else process.env.TZ = old;
  }
});

test('keyboard navigation traverses result boundaries without accidental wraparound', () => {
  assert.equal(nextSearchResult(0, 'ArrowUp', 3), 0);
  assert.equal(nextSearchResult(0, 'ArrowDown', 3), 1);
  assert.equal(nextSearchResult(2, 'ArrowDown', 3), 2);
  assert.equal(nextSearchResult(1, 'Home', 3), 0);
  assert.equal(nextSearchResult(1, 'End', 3), 2);
  assert.equal(nextSearchResult(0, 'ArrowDown', 0), null);
  assert.equal(nextSearchResult(0, 'Enter', 3), null, 'opening context does not move focus to another hit');
});
test('result count distinguishes a capped page from all matches', () => {
  assert.equal(searchCountLabel(240, 100), '100 of 240 matches');
  assert.equal(searchCountLabel(1, 1), '1 match');
  assert.equal(searchCountLabel(0, 0), 'No matches');
  assert.equal(searchCountLabel(undefined, 4), '4 matches');
  assert.equal(searchCountLabel(0, 4), '4 matches', 'a malformed total cannot hide rendered hits');
});
