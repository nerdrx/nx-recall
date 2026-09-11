import test from 'node:test';
import assert from 'node:assert/strict';
import { selectedMomentIds } from '../src/renderer/lib/saved.js';

test('moment ranges preserve chronological source ids, not numeric id order', () => {
  const rows = [{id: 9}, {id: 2}, {id: 8}];
  assert.deepEqual(selectedMomentIds(rows, '9', '2'), [9, 2]);
  assert.deepEqual(selectedMomentIds(rows, '8', '8'), [8]);
  assert.throws(() => selectedMomentIds(rows, 8, 9));
  assert.throws(() => selectedMomentIds(rows, 404, 9));
});
test('moment ranges accept twenty turns and reject twenty-one', () => {
  const rows = Array.from({length: 21}, (_, i) => ({id: i + 1}));
  assert.equal(selectedMomentIds(rows, 1, 20).length, 20);
  assert.throws(() => selectedMomentIds(rows, 1, 21));
});
