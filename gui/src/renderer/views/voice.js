import { h } from '../lib/dom.js';
import { createMutationFeedback } from '../lib/mutation.js';

export const id = 'lanalu';
export function mount(root) {
  const controller=mountVoice();
  controller.panel.hidden=false;
  controller.panel.removeAttribute('role');
  controller.panel.removeAttribute('aria-labelledby');
  controller.panel.querySelector('.settings-panel-head')?.remove();
  root.append(h('div',{class:'view-head'},h('div',{},h('h1',{text:'Lanalu'}),h('p',{class:'sub',text:'Your local voice assistant, with Recall memory.'})),h('img',{class:'lanalu-header-art',src:'../../assets/lanalu-hero.png',width:280,height:140,alt:'Lanalu appears on the left of the NX Recall artwork.'})),h('div',{class:'view-body view-enter lanalu-body'},controller.panel));
  controller.setActive(true);
  return controller;
}

export function recognitionProblem(code) {
  return ({paused:'Resume capture in Recall.',source_unavailable:'Enable the matching input in Recall Sources.',input_mismatch:'Choose the same microphone in Recall and Lanalu.',ambiguous_source:'Choose one matching input in Recall Sources.',timeout:'Could not match this turn to Recall. Try again or use separate recognition.'})[code] || '';
}
export function voiceAudioLevels(state, now = Date.now() / 1000) {
  const status = state.status || {};
  const recent = stamp => Number.isFinite(stamp) && stamp > 0 && now >= stamp && now - stamp < 4;
  const valid = peak => Number.isFinite(peak) && peak >= 0 && peak <= 1;
  const fresh = state.running && !state.stopping && recent(status.audio_levels_at);
  const inputFresh = fresh && recent(status.audio_input_read_at) && valid(status.audio_input_peak);
  const outputFresh = fresh && valid(status.audio_output_peak);
  const input = inputFresh ? status.audio_input_peak : 0;
  const output = outputFresh ? status.audio_output_peak : 0;
  return {
    input, output,
    inputLabel: !state.running || state.stopping ? 'Input stopped' : !inputFresh ? 'No recent input · check audio connection' : input > 0 ? 'Receiving audio signal' : 'Input connected · silence',
    outputLabel: !state.running || state.stopping ? 'Output stopped' : !outputFresh ? 'No recent output measurement' : output > 0 ? 'Writing generated audio' : 'No generated audio',
  };
}
export function heardRows(heard) {
  if (!Array.isArray(heard)) return [];
  return heard.slice(-6).reverse().map(item => ({
    id: typeof item?.id === 'string' && /^[a-f0-9]{32}$/.test(item.id) ? item.id : null,
    text: typeof item?.text === 'string' ? item.text.slice(0,2000) : '',
    correctedText: typeof item?.corrected_text === 'string' ? item.corrected_text.slice(0,2000) : '',
    source: item?.source === 'recall' ? 'Recall recognition' : item?.source === 'local' ? 'Separate recognition' : 'Recognition',
    timestamp: Number.isFinite(item?.timestamp) && item.timestamp > 0 && item.timestamp < 8640000000000 ? item.timestamp : null,
    decision: ({reply:'Passed to Lanalu',wake_name_missing:'Wake name missed — no reply',no_words:'No words recognized'})[item?.decision] || 'Recognition received',
  }));
}
export function voiceStateLabel(state) {
  if (state.stopping) return 'Stopping…';
  if (state.preparing) {
    const stage = ({ language_model: 'Downloading reply model', speech_model: 'Preparing recognition model', voice_model: 'Preparing voice model', dependencies: 'Installing speech tools', runtime: 'Preparing voice runtime', llama: 'Preparing local inference', llm: 'Downloading reply model', stt: 'Preparing recognition model', tts: 'Preparing voice model', verifying: 'Checking local models' })[state.setupProgress?.stage] || 'Preparing Local Voice';
    return stage + (Number.isFinite(state.setupProgress?.percent) ? ` · ${Math.round(state.setupProgress.percent)}%` : '…');
  }
  if (!state.running) return state.error || (state.available ? 'Stopped' : 'Local Voice needs setup');
  const event = state.status?.event;
  if (event === 'recognition_unavailable') return 'Recognition needs attention · ' + (recognitionProblem(state.status?.recognition_error) || 'Check Recall capture.');
  return ({ waiting_for_recall_transcript: 'Waiting for Recall to recognize this turn', recall_transcript_received: 'Recall recognized speech', audio_ready: 'Listening locally', local_listening: 'Listening locally', local_thinking: 'Thinking locally', local_speaking: 'Speaking', loading_local_models: 'Loading local models', waiting_for_vesktop_streams: 'Waiting for the selected voice client to join voice', retrying: 'Retrying audio · check selected devices or the voice client', recall_unavailable: 'Recall memory unavailable; continuing conversation', recall_identity_unavailable: 'Speaker identity unavailable; continuing conversation', local_ready: 'Listening locally', listening: 'Listening locally', speech_started: 'Hearing speech',
    speech_stopped: 'Understanding speech', transcribing: 'Understanding speech', thinking: 'Thinking locally',
    speaking: 'Speaking', interrupted: 'Listening after interruption', routing: 'Connecting audio',
    reconnecting: 'Reconnecting audio', local_llm_starting: 'Loading local model',
    wake_word_not_detected: 'Listening locally · waiting for a wake name', wake_word_detected: 'Wake name heard',
    turn_complete: 'Listening locally', local_turn_failed: 'A reply failed; listening again',
  })[event] || (state.status?.error ? 'Voice needs attention' : 'Starting Local Voice…');
}
export function mountVoice(api = window.recall.voice) {
  let active = false, destroyed = false, timer = null, busy = false, initialized = false, generation = 0;
  let current = { running: false, available: false, config: {} };
  const models = h('dl', { class: 'settings-shortcuts', id: 'lanalu-models' });
  const status = h('p', { role: 'status', 'aria-live': 'polite', class: 'sub', text: 'Open Local Voice to load its status.' });
  const inputLevel = h('meter', {id:'lanalu-input-level',min:0,max:1,value:0,'aria-describedby':'lanalu-input-state'});
  const outputLevel = h('meter', {id:'lanalu-output-level',min:0,max:1,value:0,'aria-describedby':'lanalu-output-state'});
  const inputState = h('span', {id:'lanalu-input-state',class:'sub'});
  const outputState = h('span', {id:'lanalu-output-state',class:'sub'});
  const audioLevels = h('div', {class:'lanalu-audio-levels'},
    h('div', {}, h('label', {for:inputLevel.id,text:'Audio reaching Lanalu'}),inputLevel,inputState),
    h('div', {}, h('label', {for:outputLevel.id,text:'Lanalu output'}),outputLevel,outputState),
    h('p', {class:'sub',text:'Levels show audio, not recognized speech. Output shows audio written by Lanalu, not confirmation that another app receives it.'}));
  const heardList = h('div', {id:'lanalu-heard-list',class:'lanalu-heard-list'});
  const heardState = h('p', {id:'lanalu-heard-state',class:'sub',role:'status','aria-live':'polite'});
  const heardCard = h('section', {class:'card',id:'lanalu-heard'},h('h3',{class:'card-title',text:'What Lanalu heard'}),
    h('p',{class:'sub',text:'The last six recognized turns, newest first. This view clears when Local Voice stops. Saved corrections remain in Recall and can supply trusted name hints; they do not train model weights or resend a reply.'}),heardState,heardList);
  const heardEntries = new Map();
  let correctionPending = null;
  function updateCorrectionControls() {
    for (const entry of heardEntries.values()) {
      entry.edit.disabled = !!correctionPending || entry.expired;
      entry.save.disabled = !!correctionPending || entry.expired;
      entry.cancel.disabled = correctionPending === entry;
      entry.draft.disabled = correctionPending === entry;
    }
  }
  function clearHeard(text, retainDrafts = false) {
    for (const [key,entry] of heardEntries) {
      if (retainDrafts && !entry.editor.hidden) { entry.expired=true; entry.feedback.textContent='Connection unavailable. Your draft is kept here; wait for this turn to reconnect.'; }
      else { entry.article.remove(); heardEntries.delete(key); }
    }
    updateCorrectionControls(); heardState.textContent = text;
  }
  function createHeardEntry(row) {
    const entry = {row,expired:false};
    entry.meta=h('p',{class:'sub'}); entry.original=h('p',{class:'lanalu-heard-text'});
    entry.corrected=h('p',{class:'lanalu-heard-corrected',hidden:true});entry.decision=h('p',{class:'sub'});
    entry.feedback=h('p',{class:'sub',role:'status','aria-live':'polite'});
    entry.draft=h('textarea',{class:'input',rows:2,maxlength:2000,'aria-label':'Corrected words'});
    entry.cancel=h('button',{class:'btn',text:'Cancel',onclick:()=>{entry.editor.hidden=true;entry.edit.hidden=!entry.row.id;entry.feedback.textContent='';if(entry.expired){entry.article.remove();heardEntries.delete(entry.row.id);} }});
    entry.save=h('button',{class:'btn primary',text:'Save correction',onclick:async()=>{
      if (correctionPending || entry.expired) return;
      const text=entry.draft.value.trim();
      if (!text) {entry.feedback.textContent='Enter the corrected words first.';return;}
      correctionPending=entry;entry.feedback.textContent='Saving correction…';updateCorrectionControls();
      try {
        const result=await api.correctHeard(entry.row.id,text);
        if (result?.ok!==true) throw new Error('Correction could not be saved.');
        if (!destroyed && heardEntries.get(entry.row.id)===entry) {
          entry.row.correctedText=text;entry.corrected.textContent='Corrected spelling: '+text;entry.corrected.hidden=false;
          entry.editor.hidden=true;entry.edit.hidden=false;entry.feedback.textContent='Correction saved. Original words and wake decision are unchanged.';
        }
      } catch (failure) { if (!destroyed && heardEntries.get(entry.row.id)===entry) entry.feedback.textContent=entry.expired?'This turn expired. Your draft is kept here; it was not applied to another turn.':(failure?.message || 'Correction could not be saved. Your draft is kept here.'); }
      finally {if(correctionPending===entry)correctionPending=null;updateCorrectionControls();}
    }});
    entry.editor=h('div',{class:'lanalu-heard-editor',hidden:true},entry.draft,h('div',{class:'voice-actions'},entry.save,entry.cancel));
    entry.edit=h('button',{class:'btn',text:'Correct words',hidden:!row.id,onclick:()=>{entry.draft.value=entry.row.correctedText||entry.row.text;entry.editor.hidden=false;entry.edit.hidden=true;entry.feedback.textContent='';entry.draft.focus();}});
    entry.article=h('article',{class:'lanalu-heard-turn',dataset:{heardId:row.id||''}},entry.meta,entry.original,entry.corrected,entry.decision,entry.edit,entry.editor,entry.feedback);
    return entry;
  }
  function renderHeard(items) {
    const rows = heardRows(items), keys = new Set();
    heardState.textContent = rows.length ? 'Recent recognition · not an accuracy score' : 'Waiting for recognized words…';
    rows.forEach((row,index)=>{
      const key=row.id || JSON.stringify([row.timestamp,row.text,index]);keys.add(key);
      let entry=heardEntries.get(key);
      if (!entry) {entry=createHeardEntry(row);heardEntries.set(key,entry);}
      const expired=entry.expired;entry.expired=false;entry.row={...row,correctedText:row.correctedText||entry.row.correctedText};
      if(expired)entry.feedback.textContent='Turn reconnected. Your draft is ready to save.';
      entry.meta.textContent=row.source+(row.timestamp?' · '+new Date(row.timestamp*1000).toLocaleTimeString([], {hour:'2-digit',minute:'2-digit',second:'2-digit'}):'');
      entry.original.textContent=row.text||'No words recognized.';entry.decision.textContent=row.decision;
      entry.corrected.textContent='Corrected spelling: '+entry.row.correctedText;entry.corrected.hidden=!entry.row.correctedText;
      if (heardList.children[index]!==entry.article) heardList.insertBefore(entry.article,heardList.children[index]||null);
    });
    for(const [key,entry] of heardEntries) if(!keys.has(key)) {
      if(!entry.editor.hidden) {entry.expired=true;entry.feedback.textContent='This turn expired. Your draft is kept here; it cannot be saved to another turn.';}
      else {entry.article.remove();heardEntries.delete(key);}
    }
    updateCorrectionControls();
  }
  let sending=false;
  const reply=h('div',{id:'lanalu-reply',class:'lanalu-reply',role:'log','aria-live':'polite'});
  const messageStatus=h('p',{class:'sub',role:'status','aria-live':'polite'});
  const message=h('textarea',{id:'lanalu-message',class:'input',rows:3,maxlength:2000,placeholder:'Ask Lanalu about a conversation, or simply say hello…','aria-describedby':'lanalu-message-help'});
  const send=h('button',{id:'lanalu-send',class:'btn',text:'Send message',onclick:()=>void sendMessage()});
  message.addEventListener('keydown',event=>{if(event.key==='Enter'&&(event.ctrlKey||event.metaKey)){event.preventDefault();void sendMessage();}});
  const debug=h('button',{id:'lanalu-debug',class:'btn',text:'Open Debug',onclick:()=>void api.openDebug().catch(()=>{error.textContent='Debug window could not open.';})});
  const autostart = h('input', { id: 'voice-autostart', type: 'checkbox' });
  const error = h('p', { role: 'alert', class: 'sub' });
  const audioMode = h('select', { id: 'voice-audio-mode', class: 'input', onchange: () => { local.hidden = audioMode.value !== 'local'; vesktop.hidden = audioMode.value !== 'vesktop'; } },
    h('option', { value: 'vesktop', text: 'Virtual in/out' }), h('option', { value: 'local', text: 'This computer · microphone & speakers' }));
  const recognition = h('select', { id: 'voice-recognition-source', class: 'input', 'aria-describedby': 'voice-recognition-help' },
    h('option', { value: 'recall', text: 'Use Recall’s recognizer' }), h('option', { value: 'local', text: 'Separate recognizer · Parakeet' }));
  const voiceEngine = h('select', { id:'voice-tts-backend', class:'input', onchange:()=>paintVoiceChoice() },
    h('option',{value:'piper',text:'Amy · fast, small model'}), h('option',{value:'kokoro',text:'Kokoro · more voice choices'}));
  const kokoroVoice = h('select',{id:'voice-kokoro-voice',class:'input'},
    ...[['af_heart','Heart'],['af_bella','Bella'],['af_sarah','Sarah'],['af_nicole','Nicole']].map(([value,text])=>h('option',{value,text})));
  const speedLabel=h('output',{id:'voice-speed-value',for:'voice-tts-speed',text:'1.00×'});
  const speed=h('input',{id:'voice-tts-speed',type:'range',min:.6,max:1.5,step:.05,value:1,'aria-describedby':'voice-speed-value',oninput:()=>{speedLabel.textContent=Number(speed.value).toFixed(2)+'×';}});
  const variationLabel=h('output',{id:'voice-variation-value',for:'voice-piper-variation',text:'0.67'});
  const variation=h('input',{id:'voice-piper-variation',type:'range',min:0,max:1,step:.001,value:.667,'aria-describedby':'voice-variation-value',oninput:()=>{variationLabel.textContent=Number(variation.value).toFixed(2);}});
  const mode = h('select', { id: 'voice-listening-mode', class: 'input', onchange: () => { wake.disabled = busy || current.running || mode.value !== 'wakeword'; } },
    h('option', { value: 'wakeword', text: 'Reply when a wake name is heard' }), h('option', { value: 'always', text: 'Reply to each spoken turn' }));
  const wake = h('input', { id: 'voice-wake-words', class: 'input', type: 'text', maxlength: 640, placeholder: 'Lanalu, Chat GPT', 'aria-describedby': 'voice-wake-help' });
  const source = h('select', { id: 'voice-source', class: 'input' });
  const sink = h('select', { id: 'voice-sink', class: 'input' });
  const row = (label, control) => h('div', { class: 'voice-field' }, h('label', { for: control.id, text: label }), control);
  const refreshDevices = h('button', { class: 'btn', text: 'Refresh audio devices', onclick: () => void loadDevices() });
  const local = h('div', {}, row('Microphone', source), row('Voice output', sink), refreshDevices,
    h('p', { class: 'sub', text: 'Headphones keep generated speech out of your microphone. Local speaker mode pauses listening while speaking to prevent feedback.' }));
  const vesktop = h('div', {class:'sub'},
    h('p', {text:'In your voice client, select these devices after starting Local Voice:'}),
    h('ul', {},h('li', {},h('strong', {text:'Output: '}),'NX Recall - Call audio to Lanalu'),h('li', {},h('strong', {text:'Input: '}),'NX Recall - Lanalu microphone')),
    h('p', {text:'Do not select “NX Recall - Internal voice bus”; Recall manages it internally.'}),
    h('p', {text:'Join a voice channel yourself; other clients keep their audio routing. Memory answers can include saved Recall information and are audible to everyone in the call.'}));
  const saveStatus = h('p', { class: 'sub', id: 'voice-save-status' });
  const save = h('button', { class: 'btn', text: 'Save settings', onclick: () => saveSettings(saveFeedback) });
  const saveFeedback = createMutationFeedback({ status: saveStatus, button: save, success: 'Settings saved.' });
  const voiceSaveStatus=h('p',{class:'sub',id:'voice-style-save-status'});
  const voiceSave=h('button',{class:'btn',id:'voice-style-save',text:'Save voice settings',onclick:()=>saveSettings(voiceSaveFeedback)});
  const voiceSaveFeedback=createMutationFeedback({status:voiceSaveStatus,button:voiceSave,success:'Voice settings saved. Start Local Voice to use them.'});
  const voiceSetup=h('button',{class:'btn',id:'voice-style-setup',text:'Download Kokoro voice model',onclick:()=>void action(async()=>{await api.save(patch());current=await api.setup();})});
  const voiceSetupHint=h('p',{class:'sub',id:'voice-style-setup-help'});
  const kokoroRow=row('Kokoro voice',kokoroVoice);
  const variationRow=h('div',{},row('Delivery variation',variation),variationLabel,h('p',{class:'sub',text:'Higher values vary Amy’s delivery more between readings.'}));
  function paintVoiceChoice(){kokoroRow.hidden=voiceEngine.value!=='kokoro';variationRow.hidden=voiceEngine.value!=='piper';voiceSetup.hidden=voiceEngine.value!=='kokoro' || !!current.voiceModels?.kokoro;voiceSetupHint.hidden=voiceEngine.value!=='kokoro';voiceSetupHint.textContent=current.preparing?voiceStateLabel(current):current.voiceModels?.kokoro?'Kokoro is ready on this computer.':'The optional voice model adds about 350 MB to setup. Speech then runs locally.';}
  function saveSettings(feedback){if(saveFeedback.pending || voiceSaveFeedback.pending)return;const changes=patch();void feedback.run(async()=>{current=await api.save(changes);}).catch(()=>{}).finally(()=>{if(!destroyed)render();});render();}
  const setup = h('button', { id: 'voice-setup', class: 'btn primary', text: 'Set up Local Voice', onclick: () => void action(async () => { await api.save(patch()); current = await api.setup(); }) });
  const missing = h('p', { class: 'sub', id: 'voice-missing' });
  const setupHelp = h('p', { class: 'sub', text: 'One-time setup downloads about 3 GB of models and speech tools. After setup, recognition, replies and voice run locally with no API fees. Setup does not start listening.' });
  const start = h('button', { class: 'btn primary', text: 'Start Local Voice', onclick: () => void action(async () => { await api.save(patch()); current = await api.start(); }) });
  const stop = h('button', { class: 'btn', text: 'Stop Local Voice', onclick: () => void action(async () => { current = await api.stop(); }) });
  const panel = h('section', { id: 'settings-voice', class: 'settings-panel', role: 'tabpanel', 'aria-labelledby': 'settings-tab-voice', dataset: { settingsPanel: 'voice' }, hidden: true },
    h('header', { class: 'settings-panel-head' }, h('h2', { class: 'settings-group-title', text: 'Local Voice' }),
      h('p', { class: 'sub', text: 'Talk with your local assistant using Recall memory. Recognition, replies and generated speech stay on this computer.' })),
    h('section', { class: 'card' }, h('h3', { class: 'card-title', text: 'Voice conversation' }), status, error, audioLevels,
      h('div', { class: 'voice-actions' }, setup, start, stop, debug), setupHelp, missing,
      h('label', { for: 'voice-autostart' }, autostart, ' Start with Recall'),
      h('p', { class: 'sub', text: 'Runs while NX Recall is open, including in the tray. Quitting Recall stops the conversation.' })),
    heardCard,
    h('section',{class:'card lanalu-message-card'},h('h3',{class:'card-title',text:'Write to Lanalu'}),
      h('label',{for:'lanalu-message',text:'Your message'}),message,
      h('p',{id:'lanalu-message-help',class:'sub',text:'Typed requests bypass wake names. Lanalu replies here and through the selected voice output. In Virtual in/out mode, people in the call can hear the answer. Ctrl/Cmd+Enter to send.'}),
      h('div',{class:'voice-actions'},send),messageStatus,reply),
    h('section',{class:'card',id:'lanalu-voice-style'},h('h3',{class:'card-title',text:'Lanalu’s voice'}),row('Voice model',voiceEngine),kokoroRow,
      row('Speaking speed',speed),speedLabel,variationRow,h('p',{class:'sub',text:'Voice runs locally. Stop Local Voice, save your changes, then start it again.'}),voiceSave,voiceSaveStatus,voiceSetup,voiceSetupHint),
    h('section', { class: 'card' }, h('h3', { class: 'card-title', text: 'Local models' }), row('Speech recognition', recognition), h('p', { id: 'voice-recognition-help', class: 'sub', text: 'Using Recall shares its configured recognizer. The matching microphone or incoming audio from the selected voice client must be captured in Recall. The separate recognizer uses its own English Parakeet model.' }), models, h('p', { class: 'sub', text: 'Configured models run on this computer. Debug shows whether their files are ready.' })),
    h('section', { class: 'card' }, h('h3', { class: 'card-title', text: 'Audio connection' }), row('Where to talk', audioMode), local, vesktop),
    h('section', { class: 'card' }, h('h3', { class: 'card-title', text: 'When to reply' }), row('Listening mode', mode), row('Wake names, separated by commas', wake),
      h('p', { id: 'voice-wake-help', class: 'sub', text: 'Each request must include a wake name in wake-name mode. Names are recognized locally after a spoken turn. They trigger replies; they do not identify who is speaking.' }),
      h('p', { class: 'sub', text: 'In Virtual in/out mode, recognizes only people with an assigned name and a confident voice match in Recall. Generic speaker labels and uncertain matches stay unknown. Recognition uses Recall’s recent transcript and can arrive after the reply. A voice match is recognition, not permission to access private information.' }),
      h('p', { class: 'sub', text: 'The separate recognizer and included voices use English. Stop Local Voice before changing settings.' }), save, saveStatus));
  async function sendMessage() {
    if(sending || !current.running || !message.value.trim())return;
    sending=true;messageStatus.textContent='Sending locally…';render();
    try {const question=message.value; const result=await api.send(question); if(!destroyed){message.value='';messageStatus.textContent='Reply received. This exchange is shown only in this view.';reply.replaceChildren(h('p',{},h('strong',{text:'You: '}),question),h('p',{},h('strong',{text:'Lanalu: '}),result.text));}}
    catch(e){if(!destroyed)messageStatus.textContent=e.message;}
    finally{sending=false;if(!destroyed)render();}
  }
  function patch() { return { tts_backend:voiceEngine.value, kokoro_voice:kokoroVoice.value, tts_speed:Number(speed.value), piper_noise_scale:Number(variation.value), autostart: autostart.checked, audio_mode: audioMode.value, recognition_source: recognition.value, mode: mode.value, wake_words: wake.value.split(',').map(v => v.trim()).filter(Boolean), local_source: source.value, local_sink: sink.value }; }
  function render() {
    status.textContent = voiceStateLabel(current);
    const levels = voiceAudioLevels(current);
    inputLevel.value = levels.input; outputLevel.value = levels.output;
    inputState.textContent = levels.inputLabel; outputState.textContent = levels.outputLabel;
    if (!current.running || current.stopping) clearHeard('Start Local Voice to see recognized words.');
    paintVoiceChoice();
    voiceSetup.disabled=busy || saveFeedback.pending || voiceSaveFeedback.pending || current.running || current.preparing || !current.setupAvailable;
    models.replaceChildren(...Object.entries(current.models || {}).flatMap(([key, value]) => [h('dt', { text: ({llm:'Replies',stt:'Speech recognition',tts:'Voice'})[key] || key }), h('dd', { text: value })]));
    send.disabled=sending || !current.running || current.stopping;
    message.disabled=sending;
    if(!current.running && !messageStatus.textContent)messageStatus.textContent='Start Lanalu to send a message.';
    else if(current.running && messageStatus.textContent==='Start Lanalu to send a message.')messageStatus.textContent='Type a message; no wake name is required.';
    for (const control of [autostart, audioMode, recognition, voiceEngine, kokoroVoice, speed, variation, voiceSave, mode, wake, source, sink, refreshDevices, save]) control.disabled = busy || saveFeedback.pending || voiceSaveFeedback.pending || current.running || current.preparing;
    wake.disabled ||= mode.value !== 'wakeword';
    setup.hidden = current.available && !current.preparing;
    setup.disabled = busy || current.running || current.preparing || !current.setupAvailable;
    setupHelp.hidden = current.available && !current.preparing;
    const labels = { runtime: 'voice runtime', llama: 'inference engine', llm: 'reply model', stt: 'recognition model', tts: current.config.tts_backend==='kokoro'?'Kokoro voice model':'Amy voice' };
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
        voiceEngine.value=current.config.tts_backend || 'piper';kokoroVoice.value=current.config.kokoro_voice || 'af_heart';speed.value=current.config.tts_speed ?? 1;variation.value=current.config.piper_noise_scale ?? .667;
        speedLabel.textContent=Number(speed.value).toFixed(2)+'×';variationLabel.textContent=Number(variation.value).toFixed(2);paintVoiceChoice();
        autostart.checked = current.config.autostart;
        audioMode.value = current.config.audio_mode; recognition.value = current.config.recognition_source || 'recall'; mode.value = current.config.mode; wake.value = current.config.wake_words.join(', ');
        local.hidden = audioMode.value !== 'local'; vesktop.hidden = audioMode.value !== 'vesktop';
        await loadDevices();
      }
      render();
      if (current.running && !current.stopping) {
        try {
          const result = await api.heard();
          if (destroyed || !active || ticket !== generation || !current.running || current.stopping) return;
          if (result?.ok !== true || !Array.isArray(result.heard)) throw new Error('Unavailable');
          renderHeard(result.heard);
        } catch {
          if (!destroyed && active && ticket === generation && current.running && !current.stopping) clearHeard('Recognized words are unavailable. Open Debug to check Local Voice.',true);
        }
      }
    } catch { if (!destroyed) { error.textContent = 'Local Voice status is unavailable.'; clearHeard('Recognized words are unavailable.',true); render(); } }
    finally { if (!destroyed && active && ticket === generation) timer = setTimeout(refresh, 1500); }
  }
  render();
  return { panel, setActive(value) { ++generation; active = value; clearTimeout(timer); if (active) void refresh(); }, destroy() { voiceSaveFeedback.destroy(); saveFeedback.destroy(); destroyed = true; active = false; clearTimeout(timer); } };
}
