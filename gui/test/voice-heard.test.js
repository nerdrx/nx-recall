import test from 'node:test';
import assert from 'node:assert/strict';
import {createServer} from 'node:net';
import {mkdtempSync,rmSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {readVoiceHeard,correctVoiceHeard} from '../src/main/voice.js';

async function fixture(response, run, expected={type:'heard'}) {
 const dir=mkdtempSync(join(tmpdir(),'recall-heard-')), path=join(dir,'socket');
 const server=createServer(socket=>socket.once('data',data=>{
  assert.deepEqual(JSON.parse(data),expected);
  socket.end(JSON.stringify(response)+'\n');
 }));
 await new Promise(resolve=>server.listen(path,resolve));
 try {await run(path);} finally {await new Promise(resolve=>server.close(resolve));rmSync(dir,{recursive:true,force:true});}
}
test('heard words cross a dedicated bounded channel, preserving Unicode and stripping extras', async()=>{
 const row={text:'<script>not markup</script> la nalu 🐾',timestamp:123,source:'local',wake_detected:false,decision:'wake_name_missing'};
 await fixture({ok:true,heard:[{...row,private:'discard'}]},async path=>assert.deepEqual(await readVoiceHeard(path),{ok:true,heard:[row]}));
 const long={...row,text:'\0'.repeat(2000)};
 await fixture({ok:true,heard:Array(6).fill(long)},async path=>assert.equal((await readVoiceHeard(path)).heard.length,6));
});
test('heard words reject oversized history and malformed metadata',async()=>{
 const row={text:'hello',timestamp:123,source:'local',wake_detected:false,decision:'reply'};
 for(const heard of [Array(7).fill(row),[{...row,text:'x'.repeat(2001)}],[{...row,source:'cloud'}],[{...row,wake_detected:'yes'}]]) {
  await fixture({ok:true,heard},async path=>assert.rejects(readVoiceHeard(path),/Invalid heard-words/));
 }
});

test('corrections use exact turn IDs, preserve original words, and surface failed saves',async()=>{
 const id='a'.repeat(32), request={type:'correct_heard',id,text:'Lanalu'};
 await fixture({ok:true,saved:true},async path=>assert.deepEqual(await correctVoiceHeard(path,id,'Lanalu'),{ok:true,saved:true}),request);
 await fixture({ok:false,error:'heard_expired'},async path=>assert.rejects(correctVoiceHeard(path,id,'Lanalu'),/expired/),request);
 await assert.rejects(correctVoiceHeard('/nonexistent','bad','Lanalu'),/1–2000/);
 const row={id,text:'la nalu',corrected_text:'Lanalu',timestamp:123,source:'local',wake_detected:false,decision:'wake_name_missing'};
 await fixture({ok:true,heard:[row]},async path=>assert.deepEqual((await readVoiceHeard(path)).heard,[row]));
 const long={...row,text:'\0'.repeat(2000),corrected_text:'\0'.repeat(2000)};
 await fixture({ok:true,heard:Array(6).fill(long)},async path=>assert.equal((await readVoiceHeard(path)).heard.length,6));
});
