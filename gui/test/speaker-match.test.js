import test from 'node:test';
import assert from 'node:assert/strict';
import { speakerMatches } from '../src/renderer/lib/speaker-match.js';
import { speakerPage } from '../src/renderer/lib/speaker-picker.js';

test('speaker lookup matches accents, case, generated names, IDs and all query terms',()=>{
  const sp={id:42,name:'Zoë García',auto:'Velvet Otter'};
  for(const query of [' zoe  garcia ','GARCÍA','otter zoe','voice 42','speaker 42','42'])assert.equal(speakerMatches(sp,query,'Zoë García'),true,query);
  assert.equal(speakerMatches(sp,'zoe missing'),false);
  assert.equal(speakerMatches({id:7},'speaker 07','Speaker_07'),true);
  assert.equal(speakerMatches({id:8},' \t '),true);
  assert.equal(speakerMatches(null,'unassigned','Unassigned'),true);
});
test('speaker chooser bounds large rosters without losing search matches or excluded identities',()=>{
  const speakers=Array.from({length:205},(_,id)=>({id,name:`Person ${id}`}));
  const label=id=>speakers[id].name;
  assert.equal(speakerPage(speakers,'',label).rows.length,40);
  assert.equal(speakerPage(speakers,'',label,80).rows.length,80);
  assert.deepEqual(speakerPage(speakers,'person 204',label).rows.map(sp=>sp.id),[204]);
  assert.equal(speakerPage(speakers,'204',label,40,[204]).total,0);
  const all=speakerPage(speakers,'',label,240,[2]);
  assert.equal(all.total,204);assert.equal(all.rows.length,204);assert.equal(all.rows.some(sp=>sp.id===2),false);
});
