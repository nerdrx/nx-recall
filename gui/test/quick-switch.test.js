import test from 'node:test';
import assert from 'node:assert/strict';
import { matchingCommands } from '../src/renderer/lib/quick-switch.js';
const commands = [{label:'Memory',keywords:'saved bookmark archive'}, {label:'Search',keywords:'find transcript'}, {label:'Transcript',keywords:'live recent'}, {label:'Café',keywords:'place'}];
test('quick switch matches all query words across names and aliases', () => {
  assert.equal(matchingCommands(commands, '  SAVED archive ')[0].label, 'Memory');
  assert.deepEqual(matchingCommands(commands, 'saved transcript'), []);
  assert.equal(matchingCommands(commands, 'cafe')[0].label, 'Café');
});
test('named destinations rank before keyword matches without mutating catalog', () => {
  const before = commands.map(c => c.label);
  assert.deepEqual(matchingCommands(commands, 'transcript').map(c => c.label), ['Transcript','Search']);
  assert.deepEqual(commands.map(c => c.label), before);
  assert.deepEqual(matchingCommands(commands, '').map(c => c.label), before);
});
