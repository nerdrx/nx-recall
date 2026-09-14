import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, rmSync, mkdirSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { EventEmitter } from 'node:events';
import { normalizeVoiceConfig, voiceDefaults, voiceToml, createVoiceController } from '../src/main/voice.js';
import { voiceStateLabel } from '../src/renderer/views/voice.js';

test('voice accepts only fixed local settings and encodes TOML safely', () => {
  const base = voiceDefaults('/example');
  assert.equal(base.backend, 'local');
  assert.equal(base.autostart, false);
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
  assert.equal(voiceStateLabel({ running: true }), 'Starting Local Voice…');
  assert.equal(voiceStateLabel({ running: true, status: { event: 'local_ready' } }), 'Listening locally');
  assert.equal(voiceStateLabel({ running: false, available: false }), 'Local Voice needs setup');
});

function touch(path) { mkdirSync(join(path, '..'), { recursive: true }); writeFileSync(path, 'fixture'); }
function installFixture(home) {
  const base = voiceDefaults(home), models = join(home, '.local/share/nx-recall/models');
  for (const path of [join(home, '.local/share/nx-recall/voice/venv/bin/python'), join(models, 'llama-voice/llama-server'), join(models, 'qwen2.5-3b-instruct-q4_k_m.gguf'), base.tts_model_path, base.tts_model_path + '.json', ...['encoder.int8.onnx', 'decoder.int8.onnx', 'joiner.int8.onnx', 'tokens.txt'].map(name=>join(base.stt_model_dir,name))]) touch(path);
}

test('setup requires packaged script, reports all missing components, and rejects voice start', t => {
  const dir = mkdtempSync(join(tmpdir(), 'nx-voice-missing-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const voice = createVoiceController({userData: join(dir,'data'), home:dir, runtime:dir, setupScriptPath:join(dir,'missing.py')});
  assert.equal(voice.state().setupAvailable,false);
  assert.equal(voice.state().available,false);
  assert.ok(Object.values(voice.state().components).every(v=>!v));
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
    ++launches; assert.equal(binary,'python3'); assert.deepEqual(args,[script]); assert.equal(opts.env.OPENAI_API_KEY,undefined); return worker;
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
