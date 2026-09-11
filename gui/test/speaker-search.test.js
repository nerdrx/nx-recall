import test from 'node:test';
import assert from 'node:assert/strict';
import { filterSpeakers, speakerSearchCount } from '../src/renderer/views/speakers.js';
const speakers = [
  {id:1,name:'Zoë Martín',auto:'Speaker_01',total_ms:10},
  {id:2,name:null,auto:'Guest_voice_02',total_ms:100},
  {id:200,name:'İpek',auto:'Speaker_200',total_ms:1},
];
test('speaker search matches accent-insensitive names, labels, IDs and all query words', () => {
  assert.deepEqual(filterSpeakers(speakers,'MARTIN zoe').map(s=>s.id),[1]);
  assert.deepEqual(filterSpeakers(speakers,'zoe\u0308').map(s=>s.id),[1]);
  assert.deepEqual(filterSpeakers(speakers,'ipek').map(s=>s.id),[200]);
  assert.deepEqual(filterSpeakers(speakers,'Guest_VOICE').map(s=>s.id),[2]);
  assert.deepEqual(filterSpeakers(speakers,'200').map(s=>s.id),[200]);
  assert.deepEqual(filterSpeakers(speakers,'Speaker_01').map(s=>s.id),[1]);
  assert.deepEqual(filterSpeakers(speakers,'zoe absent'),[]);
});
test('named filters combine with query and leave original ordering and data intact', () => {
  assert.deepEqual(filterSpeakers(speakers,'','named').map(s=>s.id),[1,200]);
  assert.deepEqual(filterSpeakers(speakers,'voice','unnamed').map(s=>s.id),[2]);
  assert.deepEqual(filterSpeakers(speakers,'Zoe','unnamed'),[]);
  assert.deepEqual(filterSpeakers(speakers,'  ').map(s=>s.id),[1,2,200]);
  assert.deepEqual(speakers.map(s=>s.id),[1,2,200]);
});
test('live changes can reapply the same query without cached names or counts', () => {
  const before = Array.from({length:200},(_,i)=>({id:i+1,name:null,auto:`Speaker_${i+1}`}));
  assert.equal(filterSpeakers(before,'Alice').length,0);
  const after = before.map(s=>s.id===159?{...s,name:'Alice'}:s);
  assert.deepEqual(filterSpeakers(after,'Alice','named').map(s=>s.id),[159]);
  assert.equal(filterSpeakers(after.filter(s=>s.id!==159),'Alice').length,0);
  assert.equal(speakerSearchCount(1,200),'1 of 200 voices');
  assert.equal(speakerSearchCount(0,1),'0 of 1 voice');
});
