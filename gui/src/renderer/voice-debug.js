import { h } from './lib/dom.js';
import { voiceStateLabel, recognitionProblem } from './views/voice.js';
const status=document.getElementById('debug-status');
const data=document.getElementById('debug-data');
const summary=document.getElementById('debug-summary');
const events=document.getElementById('debug-events');
let timer;
async function refresh(){
  clearTimeout(timer);
  try{
    const snapshot=await window.recall.voice.debug();
    const time=snapshot.lastUpdated?new Date(snapshot.lastUpdated*1000).toLocaleTimeString():'No worker update yet';
    const audio=snapshot.running && (snapshot.audio_ready || snapshot.routes?.ready);
    const route=snapshot.routes;
    const audioText=audio?`Attached · ${route?.playback ?? 0} incoming stream(s), ${route?.capture ?? 0} microphone stream(s)`:
      (snapshot.running?'Waiting for audio. Check the selected devices or join voice in the selected client. Typed requests can still work.':'Stopped. Start Lanalu to connect audio.');
    const rows=[['Models',snapshot.available?'Ready on this computer':'Setup required'],['Audio',audioText],
      ...Object.entries(snapshot.models || {}).map(([key,value])=>[({llm:'Reply model',stt:'Recognition model',tts:'Voice model'})[key] || key,value]),
      ['Latest state',voiceStateLabel({running:snapshot.running,available:snapshot.available,error:snapshot.lastError,status:{event:snapshot.state,recognition_error:snapshot.recognition_error}})],
      ['Recognition', recognitionProblem(snapshot.recognition_error) || (snapshot.recognition_source==='recall'?'Uses Recall’s captured transcript':'Uses the separate recognizer')],
      ['Last failure',snapshot.lastError?(snapshot.lastError+(snapshot.error_stage?` · during ${snapshot.error_stage}`:'')):'None reported'],['Last update',time]];
    summary.replaceChildren(h('h2',{text:'Voice health'}),h('dl',{class:'settings-shortcuts'},...rows.flatMap(([label,value])=>[h('dt',{text:label}),h('dd',{text:value})])));
    events.replaceChildren(...snapshot.events.slice(-12).reverse().map(event=>h('li',{text:`${new Date(event.at).toLocaleTimeString()} · ${event.event.replaceAll('vesktop','voice client').replaceAll('_',' ')}`})));
    if(!snapshot.events.length)events.append(h('li',{text:'No activity observed yet.'}));
    data.textContent=JSON.stringify(snapshot,(_key,value)=>typeof value==='string'?value.replaceAll('vesktop','virtual_io'):value,2);status.textContent=`Updated ${new Date().toLocaleTimeString()}`;
  }
  catch{status.textContent='Diagnostics unavailable. Try refreshing.';}
  timer=setTimeout(refresh,2000);
}
document.getElementById('debug-refresh').addEventListener('click',()=>void refresh());
window.addEventListener('beforeunload',()=>clearTimeout(timer));
void refresh();
