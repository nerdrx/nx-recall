import test from 'node:test';
import assert from 'node:assert/strict';
import { HeightIndex } from '../src/renderer/lib/windowed-history.js';

test('measured variable heights locate viewport boundaries without clipping long words',()=>{
  const index=new HeightIndex(100);index.append([{id:1},{id:2},{id:3}]);
  index.measure(1,3500);index.rebuild();
  assert.deepEqual(index.offsets,[0,100,3600,3700]);
  assert.equal(index.at(99),0);assert.equal(index.at(100),1);assert.equal(index.at(3599),1);assert.equal(index.at(3600),2);
  assert.equal(index.at(-100),0);assert.equal(index.at(9000),2);
});
test('purge preserves surviving measurements and identity across appended pages',()=>{
  const index=new HeightIndex();index.append([{id:1},{id:2}]);index.measure(1,650);index.rebuild();index.append([{id:3}]);
  index.remove(new Set([1]));assert.deepEqual(index.rows.map(row=>row.id),[2,3]);assert.deepEqual(index.heights,[650,156]);assert.equal(index.positions.get(2),0);assert.equal(index.positions.has(1),false);
  index.remove(new Set([2,3]));assert.equal(index.total,0);assert.equal(index.at(0),0);
});
test('large history binary lookup matches full scan at changing measurements',()=>{
  const index=new HeightIndex();index.append(Array.from({length:50000},(_,id)=>({id})));
  for(let i=0;i<50000;i+=7)index.measure(i,30+(i%1500));index.rebuild();
  for(let y=0;y<index.total;y+=23457)assert.equal(index.at(y),index.offsets.findIndex((start,i)=>i<index.rows.length && start<=y && index.offsets[i+1]>y));
});
