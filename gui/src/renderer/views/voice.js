import { h } from '../lib/dom.js';

export function voiceStateLabel(state) {
  if (state.stopping) return 'Stopping…';
  if (state.preparing) {
    const stage = ({ language_model: 'Downloading reply model', speech_model: 'Preparing recognition model', voice_model: 'Preparing Amy voice', dependencies: 'Installing speech tools', runtime: 'Preparing voice runtime', llama: 'Preparing local inference', llm: 'Downloading reply model', stt: 'Preparing recognition model', tts: 'Preparing Amy voice', verifying: 'Checking local models' })[state.setupProgress?.stage] || 'Preparing Local Voice';
    return stage + (Number.isFinite(state.setupProgress?.percent) ? ` · ${Math.round(state.setupProgress.percent)}%` : '…');
  }
  if (!state.running) return state.error || (state.available ? 'Stopped' : 'Local Voice needs setup');
  const event = state.status?.event;
  return ({ local_listening: 'Listening locally', local_thinking: 'Thinking locally', local_speaking: 'Speaking', loading_local_models: 'Loading local models', waiting_for_vesktop_streams: 'Waiting for Lanalu to join voice', retrying: 'Retrying audio · check selected devices or Vesktop', recall_unavailable: 'Recall memory unavailable; continuing conversation', recall_identity_unavailable: 'Speaker identity unavailable; continuing conversation', local_ready: 'Listening locally', listening: 'Listening locally', speech_started: 'Hearing speech',
    speech_stopped: 'Understanding speech', transcribing: 'Understanding speech', thinking: 'Thinking locally',
    speaking: 'Speaking', interrupted: 'Listening after interruption', routing: 'Connecting audio',
    reconnecting: 'Reconnecting audio', local_llm_starting: 'Loading local model',
    wake_word_not_detected: 'Listening locally · waiting for a wake name', wake_word_detected: 'Wake name heard',
    turn_complete: 'Listening locally', local_turn_failed: 'A reply failed; listening again',
  })[event] || (state.status?.error ? 'Voice needs attention' : 'Starting Local Voice…');
}
export function mountVoice() {
  const api = window.recall.voice;
  let active = false, destroyed = false, timer = null, busy = false, initialized = false, generation = 0;
  let current = { running: false, available: false, config: {} };
  const status = h('p', { role: 'status', 'aria-live': 'polite', class: 'sub', text: 'Open Local Voice to load its status.' });
  const autostart = h('input', { id: 'voice-autostart', type: 'checkbox' });
  const error = h('p', { role: 'alert', class: 'sub' });
  const audioMode = h('select', { id: 'voice-audio-mode', class: 'input', onchange: () => { local.hidden = audioMode.value !== 'local'; vesktop.hidden = audioMode.value !== 'vesktop'; } },
    h('option', { value: 'vesktop', text: 'Lanalu in Vesktop' }), h('option', { value: 'local', text: 'This computer · microphone & speakers' }));
  const mode = h('select', { id: 'voice-listening-mode', class: 'input', onchange: () => { wake.disabled = busy || current.running || mode.value !== 'wakeword'; } },
    h('option', { value: 'wakeword', text: 'Reply when a wake name is heard' }), h('option', { value: 'always', text: 'Reply to each spoken turn' }));
  const wake = h('input', { id: 'voice-wake-words', class: 'input', type: 'text', maxlength: 640, placeholder: 'Lanalu, Chat GPT', 'aria-describedby': 'voice-wake-help' });
  const source = h('select', { id: 'voice-source', class: 'input' });
  const sink = h('select', { id: 'voice-sink', class: 'input' });
  const row = (label, control) => h('div', { class: 'voice-field' }, h('label', { for: control.id, text: label }), control);
  const refreshDevices = h('button', { class: 'btn', text: 'Refresh audio devices', onclick: () => void loadDevices() });
  const local = h('div', {}, row('Microphone', source), row('Voice output', sink), refreshDevices,
    h('p', { class: 'sub', text: 'Headphones keep generated speech out of your microphone. Local speaker mode pauses listening while speaking to prevent feedback.' }));
  const vesktop = h('p', { class: 'sub', text: 'Uses only the dedicated Lanalu Vesktop profile. Join a voice channel yourself; other Discord clients keep their audio routing. Memory answers can include saved Recall information and are audible to everyone in the call.' });
  const save = h('button', { class: 'btn', text: 'Save settings', onclick: () => void action(async () => { current = await api.save(patch()); error.textContent = 'Settings saved.'; }) });
  const setup = h('button', { id: 'voice-setup', class: 'btn primary', text: 'Set up Local Voice', onclick: () => void action(async () => { current = await api.setup(); }) });
  const missing = h('p', { class: 'sub', id: 'voice-missing' });
  const setupHelp = h('p', { class: 'sub', text: 'One-time setup downloads about 2 GB of models and speech tools. After setup, recognition, replies and voice run locally with no API fees. Setup does not start listening.' });
  const start = h('button', { class: 'btn primary', text: 'Start Local Voice', onclick: () => void action(async () => { await api.save(patch()); current = await api.start(); }) });
  const stop = h('button', { class: 'btn', text: 'Stop Local Voice', onclick: () => void action(async () => { current = await api.stop(); }) });
  const panel = h('section', { id: 'settings-voice', class: 'settings-panel', role: 'tabpanel', 'aria-labelledby': 'settings-tab-voice', dataset: { settingsPanel: 'voice' }, hidden: true },
    h('header', { class: 'settings-panel-head' }, h('h2', { class: 'settings-group-title', text: 'Local Voice' }),
      h('p', { class: 'sub', text: 'Talk with your local assistant using Recall memory. Recognition, replies and generated speech stay on this computer.' })),
    h('section', { class: 'card' }, h('h3', { class: 'card-title', text: 'Voice conversation' }), status, error,
      h('div', { class: 'voice-actions' }, setup, start, stop), setupHelp, missing,
      h('label', { for: 'voice-autostart' }, autostart, ' Start with Recall'),
      h('p', { class: 'sub', text: 'Runs while NX Recall is open, including in the tray. Quitting Recall stops the conversation.' })),
    h('section', { class: 'card' }, h('h3', { class: 'card-title', text: 'Audio connection' }), row('Where to talk', audioMode), local, vesktop),
    h('section', { class: 'card' }, h('h3', { class: 'card-title', text: 'When to reply' }), row('Listening mode', mode), row('Wake names, separated by commas', wake),
      h('p', { id: 'voice-wake-help', class: 'sub', text: 'Each request must include a wake name in wake-name mode. Names are recognized locally after a spoken turn. They trigger replies; they do not identify who is speaking.' }),
      h('p', { class: 'sub', text: 'In Vesktop, recognizes only people with an assigned name and a confident voice match in Recall. Generic speaker labels and uncertain matches stay unknown. Recognition uses Recall’s recent transcript and can arrive after the reply. A voice match is recognition, not permission to access private information.' }),
      h('p', { class: 'sub', text: 'The included speech model and Amy voice use English. Stop Local Voice before changing settings.' }), save));
  function patch() { return { autostart: autostart.checked, audio_mode: audioMode.value, mode: mode.value, wake_words: wake.value.split(',').map(v => v.trim()).filter(Boolean), local_source: source.value, local_sink: sink.value }; }
  function render() {
    status.textContent = voiceStateLabel(current);
    for (const control of [autostart, audioMode, mode, wake, source, sink, refreshDevices, save]) control.disabled = busy || current.running || current.preparing;
    wake.disabled ||= mode.value !== 'wakeword';
    setup.hidden = current.available && !current.preparing;
    setup.disabled = busy || current.running || current.preparing || !current.setupAvailable;
    setupHelp.hidden = current.available && !current.preparing;
    const labels = { runtime: 'voice runtime', llama: 'inference engine', llm: 'reply model', stt: 'recognition model', tts: 'Amy voice' };
    const absent = Object.entries(current.components || {}).filter(([,value]) => !value).map(([key]) => labels[key]).filter(Boolean);
    missing.textContent = absent.length ? `Still needed: ${absent.join(', ')}.` : '';
    missing.hidden = !absent.length;
    start.className = current.available ? 'btn primary' : 'btn';
    start.disabled = busy || current.running || current.preparing || !current.available;
    stop.textContent = current.preparing ? 'Cancel setup' : 'Stop Local Voice';
    stop.disabled = busy || (!current.running && !current.preparing);
  }
  function options(control, devices, selected) {
    control.replaceChildren(h('option', { value: '', text: 'Choose a device' }), ...devices.map(device => h('option', { value: device.name, text: device.label })));
    if (selected && !devices.some(device => device.name === selected)) control.append(h('option', { value: selected, text: 'Saved device · currently unavailable' }));
    control.value = selected || '';
  }
  async function loadDevices() {
    try {
      const devices = await api.devices();
      if (destroyed) return;
      options(source, devices.sources, source.value || current.config.local_source);
      options(sink, devices.sinks, sink.value || current.config.local_sink);
    } catch { if (!destroyed) error.textContent = 'Audio devices could not be listed. Check that PipeWire is running.'; }
  }
  async function action(fn) {
    if (busy) return;
    busy = true; error.textContent = ''; render();
    try { await fn(); } catch (e) { if (!destroyed) error.textContent = e.message; }
    finally { busy = false; if (!destroyed) render(); }
  }
  async function refresh() {
    if (!active || destroyed) return;
    const ticket = generation;
    try {
      current = await api.state();
      if (destroyed || !active || ticket !== generation) return;
      if (!initialized) {
        initialized = true;
        autostart.checked = current.config.autostart;
        audioMode.value = current.config.audio_mode; mode.value = current.config.mode; wake.value = current.config.wake_words.join(', ');
        local.hidden = audioMode.value !== 'local'; vesktop.hidden = audioMode.value !== 'vesktop';
        await loadDevices();
      }
      render();
    } catch { if (!destroyed) error.textContent = 'Local Voice status is unavailable.'; }
    finally { if (!destroyed && active && ticket === generation) timer = setTimeout(refresh, 1500); }
  }
  render();
  return { panel, setActive(value) { ++generation; active = value; clearTimeout(timer); if (active) void refresh(); }, destroy() { destroyed = true; active = false; clearTimeout(timer); } };
}
