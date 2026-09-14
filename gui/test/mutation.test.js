import test from 'node:test';
import assert from 'node:assert/strict';
import { createMutationFeedback } from '../src/renderer/lib/mutation.js';
function node(textContent='') { return {textContent,dataset:{},disabled:false,hidden:false,setAttribute(){},removeAttribute(){}}; }
const delay=ms=>new Promise(resolve=>setTimeout(resolve,ms));
test('save feedback stays local, guards duplicates and confirms only after the write',async()=>{
  const status=node(),button=node('Save');
  let resolve,calls=0;
  const feedback=createMutationFeedback({status,button,slowAfter:5});
  const work=()=>{calls++;return new Promise(r=>{resolve=r;});};
  const first=feedback.run(work),second=feedback.run(work);
  assert.equal(first,second);assert.equal(button.disabled,true);assert.equal(status.textContent,'Saving…');
  await delay(15);assert.equal(calls,1);assert.match(status.textContent,/Still saving/);
  resolve(42);assert.equal(await first,42);assert.equal(status.textContent,'Saved');assert.equal(button.textContent,'Save');assert.equal(button.disabled,false);assert.equal(feedback.pending,false);
});
test('failed save preserves nearby error and permits a retry',async()=>{
  const status=node(),button=node('Save changes');const feedback=createMutationFeedback({status,button});
  await assert.rejects(feedback.run(()=>Promise.reject(new Error('timeout'))),/timeout/);
  assert.equal(status.dataset.state,'error');assert.match(status.textContent,/timeout/);assert.equal(button.disabled,false);
  await feedback.run(()=>true);assert.equal(status.textContent,'Saved');
});
test('destroy prevents late completion from repainting an abandoned editor',async()=>{
  const status=node();const feedback=createMutationFeedback({status,slowAfter:5});let resolve;
  const work=feedback.run(()=>new Promise(r=>{resolve=r;}));await Promise.resolve();feedback.destroy();
  resolve();await work;await delay(10);assert.equal(status.textContent,'Saving…');
});
