import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, rmSync, mkdirSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createServer } from 'node:net';
import { EventEmitter } from 'node:events';
import { normalizeVoiceConfig, voiceDefaults, voiceToml, createVoiceController, sendVoiceText, voiceModelLabels } from '../src/main/voice.js';
import { voiceStateLabel } from '../src/renderer/views/voice.js';

test('voice accepts only fixed local settings and encodes TOML safely', () => {
  const base = voiceDefaults('/example');
  assert.equal(base.backend, 'local');
  assert.equal(base.autostart, false);
  assert.equal(base.recognition_source, 'recall');
  assert.throws(() => normalizeVoiceConfig({ recognition_source: 'remote' }, base));
  assert.throws(() => normalizeVoiceConfig({ autostart: 'yes' }, base));
  assert.throws(() => normalizeVoiceConfig({ backend: 'openai' }, base));
  assert.throws(() => normalizeVoiceConfig({ llm_url: 'https://remote.test' }, base));
  assert.throws(() => normalizeVoiceConfig({ wake_words: [] }, base));
  assert.throws(() => normalizeVoiceConfig({ audio_mode: 'anything' }, base));
  assert.throws(() => normalizeVoiceConfig({ local_source: 'x\nbackend="cloud"' }, base));
  const result = normalizeVoiceConfig({ wake_words: [' Lanalu ', 'Lanalu'], local_source: 'microphone "desk"' }, base);
  assert.deepEqual(result.wake_words, ['Lanalu']);
  assert.ok(voiceToml(result).includes('local_source = "microphone \\"desk\\""'));
});

test('voice controller persists settings, owns one worker and waits for graceful stop', async t => {
  const dir = mkdtempSync(join(tmpdir(), 'nx-voice-test-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const home = join(dir, 'home'), userData = join(dir, 'data'), runtime = join(dir, 'run');
  const python = join(home, '.local/share/nx-recall/voice/venv/bin/python');
  installFixture(home);
  let launches = 0, argumentsSeen;
  class Worker extends EventEmitter { kill(signal) { assert.equal(signal, 'SIGTERM'); setImmediate(() => this.emit('exit', 0, null)); } }
  const voice = createVoiceController({ userData, home, runtime, spawnWorker: (binary, args, options) => {
    ++launches; assert.equal(binary, python); assert.equal(options.detached, true); assert.equal(options.env.OPENAI_API_KEY, undefined); argumentsSeen = args;
    return new Worker();
  } });
  voice.save({ wake_words: ['Chat GPT'], audio_mode: 'local', local_source: 'mic', local_sink: 'headphones' });
  assert.equal(JSON.parse(readFileSync(join(userData, 'voice.json'))).mode, 'wakeword');
  assert.equal(voice.start().running, true);
  voice.start(); assert.equal(launches, 1);
  assert.deepEqual(argumentsSeen, ['-m', 'nx_recall_voice.daemon', 'run', '--config', join(userData, 'voice.toml')]);
  assert.throws(() => voice.save({ mode: 'always' }));
  const stopped = await voice.stop(); assert.equal(stopped.running, false);
  assert.equal(voiceStateLabel(stopped), 'Stopped');
  const restored = createVoiceController({ userData, home, runtime });
  assert.deepEqual(restored.state().config.wake_words, ['Chat GPT']);
});

test('voice status does not claim listening before worker readiness', () => {
  assert.match(voiceStateLabel({ running: true, status:{event:'recognition_unavailable',recognition_error:'paused'} }), /Resume capture/);
  assert.equal(voiceStateLabel({ running: true }), 'Starting Local Voice…');
  assert.equal(voiceStateLabel({ running: true, status: { event: 'local_ready' } }), 'Listening locally');
  assert.equal(voiceStateLabel({ running: false, available: false }), 'Local Voice needs setup');
});

function touch(path) { mkdirSync(join(path, '..'), { recursive: true }); writeFileSync(path, 'fixture'); }
function installFixture(home) {
  const base = voiceDefaults(home), models = join(home, '.local/share/nx-recall/models');
  for (const path of [join(home, '.local/share/nx-recall/voice/venv/bin/python'), join(models, 'llama-voice/llama-server'), join(models, 'qwen3.5-4b-q4_k_m.gguf'), base.tts_model_path, base.tts_model_path + '.json', ...['encoder.int8.onnx', 'decoder.int8.onnx', 'joiner.int8.onnx', 'tokens.txt'].map(name=>join(base.stt_model_dir,name))]) touch(path);
}

test('setup requires packaged script, reports all missing components, and rejects voice start', t => {
  const dir = mkdtempSync(join(tmpdir(), 'nx-voice-missing-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const voice = createVoiceController({userData: join(dir,'data'), home:dir, runtime:dir, setupScriptPath:join(dir,'missing.py')});
  assert.equal(voice.state().setupAvailable,false);
  assert.equal(voice.state().available,false);
  assert.equal(voice.state().components.stt,true);
  assert.ok(Object.entries(voice.state().components).filter(([key])=>key!=='stt').every(([,v])=>!v));
  assert.throws(()=>voice.start(), /Set up Local Voice/);
  assert.throws(()=>voice.setup(), /setup is missing/);
});

test('setup has one owned process, bounded sanitized progress, and never starts listening', async t => {
  const dir = mkdtempSync(join(tmpdir(), 'nx-voice-setup-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const script=join(dir,'setup-local.py'); touch(script);
  let launches=0;
  const worker=new EventEmitter(); worker.stdout=new EventEmitter();
  worker.kill=()=>{setImmediate(()=>worker.emit('exit',0));return true;};
  const voice=createVoiceController({userData:join(dir,'data'),home:dir,runtime:dir,setupScriptPath:script,spawnWorker:(binary,args,opts)=>{
    ++launches; assert.equal(binary,'python3'); assert.deepEqual(args,[script,'--tts-backend','piper']); assert.equal(opts.env.OPENAI_API_KEY,undefined); return worker;
  }});
  voice.save({autostart:true});
  assert.equal(voice.setup().preparing,true); voice.setup(); assert.equal(launches,1);
  assert.throws(()=>voice.start(),/Wait/);
  worker.stdout.emit('data',Buffer.from(JSON.stringify({event:'setup_progress',stage:'llm',percent:40,message:'private text'})+'\n'));
  assert.deepEqual(voice.state().setupProgress,{stage:'llm',percent:40});
  assert.ok(!JSON.stringify(voice.state()).includes('private text'));
  installFixture(dir); worker.emit('exit',0);
  assert.equal(voice.state().available,true); assert.equal(voice.state().running,false); assert.equal(voice.state().preparing,false);
  voice.setup(); await voice.stop(); assert.equal(voice.state().preparing,false);
});


test('typed requests use bounded local socket protocol and return a visible reply', async t=>{
  const dir=mkdtempSync(join(tmpdir(),'nx-voice-text-'));t.after(()=>rmSync(dir,{recursive:true,force:true}));
  const socketPath=join(dir,'control.sock');
  const server=createServer(socket=>{let input='';socket.on('data',chunk=>{input+=chunk;if(input.includes('\n')){assert.deepEqual(JSON.parse(input),{type:'text',text:'Hello Lanalu'});socket.end(JSON.stringify({ok:true})+'\n'+JSON.stringify({type:'reply',text:'Hello, locally.'})+'\n');}});});
  await new Promise(resolve=>server.listen(socketPath,resolve));t.after(()=>server.close());
  assert.deepEqual(await sendVoiceText(socketPath,'Hello Lanalu'),{ok:true,text:'Hello, locally.'});
  await assert.rejects(sendVoiceText(socketPath,''),/1–2000/);
  await assert.rejects(sendVoiceText(socketPath,'x'.repeat(2001)),/1–2000/);
});


test('debug retains stopped worker state, routes and error without transcript fields',async t=>{
  const dir=mkdtempSync(join(tmpdir(),'nx-voice-debug-'));t.after(()=>rmSync(dir,{recursive:true,force:true}));installFixture(dir);
  const worker=new EventEmitter();worker.pid=2147483647;worker.stderr=new EventEmitter();worker.kill=()=>{setImmediate(()=>worker.emit('exit',0));return true;};
  const voice=createVoiceController({userData:join(dir,'data'),home:dir,runtime:dir,spawnWorker:()=>worker});voice.start();
  const statusFile=join(dir,'nx-recall-voice/status.json');mkdirSync(join(statusFile,'..'),{recursive:true});
  writeFileSync(statusFile,JSON.stringify({pid:worker.pid,event:'retrying',error:'BrokenPipeError',retry_seconds:2,routes:{ready:false,playback:1,capture:0},transcript:'PRIVATE SENTENCE',input_kind:'text',error_stage:'playback',updated_at:1}));
  await voice.stop();const debug=voice.debug();
  assert.equal(debug.state,'retrying');assert.equal(debug.running,false);assert.equal(debug.routes.playback,1);assert.equal(debug.retry_seconds,2);assert.match(debug.lastError,/BrokenPipeError/);assert.ok(!JSON.stringify(debug).includes('PRIVATE SENTENCE'));
});

 test('model labels describe configured models without exposing custom paths', () => {
  const base=voiceDefaults('/example');
  assert.deepEqual(voiceModelLabels(base), {llm:'Qwen3.5 4B · Q4_K_M',stt:'Recall’s configured recognition model',tts:'Piper Amy · English'});
  assert.deepEqual(voiceModelLabels({...base,recognition_source:'local',stt_model_dir:'/private/model',tts_model_path:'/private/voice.onnx'}), {llm:'Qwen3.5 4B · Q4_K_M',stt:'Custom recognition model',tts:'Custom Piper voice'});
});

test('shared recognition does not require a separate speech model',t=>{
  const dir=mkdtempSync(join(tmpdir(),'nx-voice-shared-'));t.after(()=>rmSync(dir,{recursive:true,force:true}));installFixture(dir);
  const base=voiceDefaults(dir);rmSync(base.stt_model_dir,{recursive:true});
  const controller=createVoiceController({userData:join(dir,'data'),home:dir,runtime:dir});
  assert.equal(controller.state().available,true);
  controller.save({recognition_source:'local'});assert.equal(controller.state().available,false);
  controller.save({recognition_source:'recall'});assert.equal(controller.state().available,true);
});

test('voice preferences are bounded and Kokoro requires its own complete model',t=>{
 const base=voiceDefaults('/example');
 for(const patch of [{tts_speed:.59},{tts_speed:1.51},{tts_speed:NaN},{tts_speed:'1'},{piper_noise_scale:-.1},{piper_noise_scale:1.1},{tts_backend:'remote'},{kokoro_voice:'unknown'}])assert.throws(()=>normalizeVoiceConfig(patch,base));
 const next=normalizeVoiceConfig({tts_backend:'kokoro',kokoro_voice:'af_bella',tts_speed:1.2,piper_noise_scale:.5},base);
 assert.equal(voiceModelLabels(next).tts,'Kokoro Bella · English');assert.ok(voiceToml(next).includes('tts_speed = 1.2'));
 const dir=mkdtempSync(join(tmpdir(),'nx-voice-kokoro-'));t.after(()=>rmSync(dir,{recursive:true,force:true}));installFixture(dir);
 const controller=createVoiceController({userData:join(dir,'data'),home:dir,runtime:dir});
 controller.save({tts_backend:'kokoro'});assert.equal(controller.state().available,false);assert.equal(controller.state().voiceModels.piper,true);
 for(const name of ['model.onnx','voices.bin','tokens.txt','lexicon-us-en.txt','espeak-ng-data/phontab'])touch(join(voiceDefaults(dir).kokoro_model_dir,name));
 assert.equal(controller.state().available,true);assert.equal(controller.state().voiceModels.kokoro,true);
});
