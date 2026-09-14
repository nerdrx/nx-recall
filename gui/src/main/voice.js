// Local Voice belongs to the desktop app. Only this controller starts/stops its
// worker; the renderer receives a small settings/status surface, never a shell.
import { spawn, execFile } from 'node:child_process';
import { existsSync, readFileSync, mkdirSync, writeFileSync, renameSync, statSync } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { promisify } from 'node:util';
const exec = promisify(execFile);
const SETUP_STAGES = new Set(['preparing', 'dependencies', 'runtime', 'llama', 'llm', 'stt', 'tts', 'verifying', 'complete', 'language_model', 'speech_model', 'voice_model']);
function workerEnv() { return { ...Object.fromEntries(Object.entries(process.env).filter(([key]) => /^(PATH|HOME|LANG|LC_.*|XDG_.*|TMPDIR|PULSE_SERVER|DBUS_SESSION_BUS_ADDRESS|LD_LIBRARY_PATH|VK_ICD_FILENAMES|AMD_VULKAN_ICD)$/.test(key))), PYTHONUNBUFFERED: '1' }; }
function present(path) { try { const s = statSync(path); return s.isFile() && s.size > 0; } catch { return false; } }


export function voiceDefaults(home = homedir()) {
  return { backend: 'local', autostart: false, audio_mode: 'vesktop', mode: 'wakeword',
    wake_words: ['lanalu', 'chat gpt'], local_source: '', local_sink: '',
    vesktop_profile: join(home, '.config/vesktop'),
    stt_model_dir: join(home, '.local/share/nx-recall/models/sherpa-onnx-nemo-parakeet_tdt_transducer_110m-en-36000-int8'),
    tts_model_path: join(home, '.local/share/nx-recall/models/voices/en_US-amy-medium.onnx') };
}
export function normalizeVoiceConfig(patch, previous = voiceDefaults()) {
  if (!patch || typeof patch !== 'object' || Array.isArray(patch)) throw new Error('Invalid voice settings');
  const result = { ...previous, backend: 'local' };
  for (const [key, value] of Object.entries(patch)) {
    if (!Object.hasOwn(previous, key)) throw new Error('Unknown voice setting');
    if (key === 'backend') { if (value !== 'local') throw new Error('Local Voice requires the local backend'); continue; }
    if (key === 'autostart') {
      if (typeof value !== 'boolean') throw new Error('Start with Recall must be on or off');
      result[key] = value;
    } else if (key === 'wake_words') {
      if (!Array.isArray(value) || value.length > 8 || value.some(v => typeof v !== 'string' || v.length > 80 || /[\x00-\x1f]/u.test(v))) throw new Error('Use up to eight short wake names');
      result[key] = [...new Set(value.map(v => v.trim()).filter(Boolean))];
    } else {
      if (typeof value !== 'string' || value.length > 2048 || /[\x00-\x1f]/u.test(value)) throw new Error('Invalid voice setting');
      result[key] = value;
    }
  }
  if (!['local', 'vesktop'].includes(result.audio_mode)) throw new Error('Choose local audio or Vesktop');
  if (!['always', 'wakeword'].includes(result.mode)) throw new Error('Choose wake names or always listening');
  if (result.mode === 'wakeword' && !result.wake_words.some(word => /[\p{L}\p{N}]/u.test(word))) throw new Error('Enter at least one wake name');
  return result;
}
export function voiceToml(config) {
  return '# Managed by NX Recall Local Voice. No cloud credentials.\n' + Object.entries(config).map(([key, value]) => `${key} = ${JSON.stringify(value)}`).join('\n') + '\n';
}
export function createVoiceController({ userData, home = homedir(), runtime = process.env.XDG_RUNTIME_DIR || `/run/user/${process.getuid()}`, spawnWorker = spawn, setupScriptPath } = {}) {
  const defaults = voiceDefaults(home);
  const file = join(userData, 'voice.json'), toml = join(userData, 'voice.toml');
  const python = join(home, '.local/share/nx-recall/voice/venv/bin/python');
  const statusFile = join(runtime, 'nx-recall-voice/status.json');
  const installedSetup = join(home, '.local/lib/nx-recall/voice/setup-local.py');
  const setupScript = setupScriptPath || (existsSync(installedSetup) ? installedSetup : fileURLToPath(new URL('../../../voice/setup-local.py', import.meta.url)));
  let config = defaults, child = null, setupChild = null, setupProgress = null, lastError = null, stopping = null;
  function components() {
    const models = join(home, '.local/share/nx-recall/models');
    return { runtime: present(python), llama: present(join(models, 'llama-voice/llama-server')),
      llm: present(join(models, 'qwen2.5-3b-instruct-q4_k_m.gguf')),
      stt: ['encoder.int8.onnx', 'decoder.int8.onnx', 'joiner.int8.onnx', 'tokens.txt'].every(name => present(join(config.stt_model_dir, name))),
      tts: present(config.tts_model_path) && present(config.tts_model_path + '.json') };
  }

  try { config = normalizeVoiceConfig(JSON.parse(readFileSync(file, 'utf8')), defaults); } catch { /* New profile or invalid settings: safe local defaults. */ }
  function state() {
    let status = null;
    if (child) {
      try {
        const data = JSON.parse(readFileSync(statusFile, 'utf8'));
        if (data.pid === child.pid) status = {
          event: String(data.event || 'starting').slice(0, 100),
          updated_at: data.updated_at, connected: !!data.connected,
          error: typeof data.error === 'string' ? data.error.slice(0, 160) : null,
        };
      } catch { /* Worker may not have written its first atomic snapshot yet. */ }
    }
    return { running: !!child, preparing: !!setupChild, setupProgress, setupAvailable: present(setupScript), components: components(), stopping: !!stopping, available: Object.values(components()).every(Boolean), config: { ...config, wake_words: [...config.wake_words] }, status, error: lastError };
  }
  function save(patch) {
    if (child || setupChild) throw new Error('Stop Local Voice or wait for setup before changing settings');
    const next = normalizeVoiceConfig(patch, config);
    mkdirSync(userData, { recursive: true, mode: 0o700 });
    for (const [path, contents] of [[file, JSON.stringify(next, null, 2) + '\n'], [toml, voiceToml(next)]]) {
      writeFileSync(path + '.tmp', contents, { mode: 0o600 });
      renameSync(path + '.tmp', path);
    }
    config = next;
    return state();
  }
  async function devices() {
    const [sources, sinks] = await Promise.all(['sources', 'sinks'].map(kind => exec('pactl', ['-f', 'json', 'list', kind], { timeout: 5000, maxBuffer: 1024 * 1024 })));
    const clean = (text, source) => JSON.parse(text).filter(d => !source || (!d.name.endsWith('.monitor') && (d.monitor_of_sink === undefined || d.monitor_of_sink === null || d.monitor_of_sink === 4294967295)))
      .filter(d => !/^(lanalu|nx_recall_voice)_/.test(d.name))
      .map(d => ({ name: String(d.name), label: String(d.description || d.name) }));
    return { sources: clean(sources.stdout, true), sinks: clean(sinks.stdout, false) };
  }
  function start() {
    if (child) return state();
    if (setupChild) throw new Error('Wait for Local Voice setup to finish');
    if (!Object.values(components()).every(Boolean)) throw new Error('Set up Local Voice before starting a conversation');
    if (config.audio_mode === 'local' && (!config.local_source || !config.local_sink)) throw new Error('Choose a microphone and an output device');
    save({});
    lastError = null;
    const worker = spawnWorker(python, ['-m', 'nx_recall_voice.daemon', 'run', '--config', toml], { stdio: 'ignore', detached: true, env: workerEnv() });
    child = worker;
    worker.once('error', () => { if (child === worker) { child = null; lastError = 'Local Voice could not start'; } });
    worker.once('exit', (code, signal) => {
      if (child === worker) {
        child = null;
        if (!stopping && (code !== 0 || signal)) lastError = `Local Voice stopped unexpectedly (${signal || code})`;
      }
      // A failed worker must not leave its private inference children running.
      try { process.kill(-worker.pid, 'SIGTERM'); } catch { /* Process group is already gone. */ }
    });
    return state();
  }
  function setup() {
    if (setupChild) return state();
    if (child) throw new Error('Stop Local Voice before running setup');
    if (!present(setupScript)) throw new Error('Local Voice setup is missing. Reinstall the current NX Recall release.');
    lastError = null;
    setupProgress = { stage: 'preparing', percent: null };
    const worker = spawnWorker('python3', [setupScript], { stdio: ['ignore', 'pipe', 'ignore'], detached: true, env: workerEnv() });
    setupChild = worker;
    let buffer = '';
    worker.stdout?.on('data', chunk => {
      buffer += chunk.toString();
      if (buffer.length > 8192) { buffer = ''; return; }
      const lines = buffer.split('\n'); buffer = lines.pop();
      for (const line of lines) {
        try {
          const item = JSON.parse(line);
          if (item.event === 'setup_progress' && SETUP_STAGES.has(item.stage)) setupProgress = {
            stage: item.stage, percent: Number.isFinite(item.percent) ? Math.min(100, Math.max(0, item.percent)) : null,
          };
        } catch { /* Ignore dependency logs; never pass their raw text to the UI. */ }
      }
    });
    worker.once('error', () => { if (setupChild === worker) { setupChild = null; lastError = 'Setup could not start. Check that Python 3 is installed.'; } });
    worker.once('exit', code => {
      if (setupChild === worker) {
        setupChild = null;
        if (!stopping) {
          if (code !== 0) lastError = 'Setup did not finish. Check your connection and free disk space, then try again.';
          else if (!Object.values(components()).every(Boolean)) lastError = 'Setup finished, but required files are still missing. Run setup again or check custom model paths.';
          else setupProgress = { stage: 'complete', percent: 100 };
        }
      }
      try { process.kill(-worker.pid, 'SIGTERM'); } catch { /* Owned process group already gone. */ }
    });
    return state();
  }
  async function stop() {
    if (stopping) return stopping;
    if (!child && !setupChild) return state();
    const worker = child || setupChild;
    stopping = new Promise(resolve => {
      const timer = setTimeout(() => { try { process.kill(-worker.pid, 'SIGKILL'); } catch {} finish(); }, 30000);
      const finish = () => { clearTimeout(timer); if (child === worker) child = null; if (setupChild === worker) setupChild = null; resolve(); };
      worker.once('exit', finish);
      worker.once('error', finish);
      worker.kill('SIGTERM');
    });
    await stopping;
    stopping = null;
    return state();
  }
  return { state, save, devices, start, setup, stop };
}
