import { h, clear } from './dom.js';
import { speakerMatches } from './speaker-match.js';
export function speakerPage(speakers, query, label, limit=40, excludeIds=[]) {
  const excluded=new Set(excludeIds);
  const matches=speakers.filter(sp=>!excluded.has(sp.id) && speakerMatches(sp,query,label(sp.id)));
  return {rows:matches.slice(0,limit),total:matches.length};
}
// Native buttons keep Enter/Space behavior. Arrow keys move within rendered choices.
export function createSpeakerPicker({speakers,label,render,onSelect,selected=null,allowUnassigned=false,excludeIds=[],id='speaker-find'}) {
  let limit=40;
  const list=h('div',{class:'sp-pick',id:`${id}-choices`});
  const status=h('p',{class:'sub',id:`${id}-status`,role:'status'});
  const input=h('input',{class:'input',id,placeholder:'Find a person or voice','aria-label':'Find a person or voice','aria-controls':list.id,oninput:()=>{limit=40;refresh();}});
  const more=h('button',{class:'btn small',type:'button',text:'Show more voices',onclick:()=>{const firstNew=list.children.length;limit+=40;refresh();list.children[firstNew]?.focus();}});
  function choice(sp) {
    const key=sp?.id??null;
    const node=render(sp,()=>{selected=key; for(const button of list.children)button.setAttribute('aria-pressed',String(button.dataset.speakerChoice===String(key)));onSelect(key);});
    node.dataset.speakerChoice=String(key);node.setAttribute('aria-pressed',String(selected===key));return node;
  }
  function refresh() {
    const focused=list.contains(document.activeElement)?document.activeElement?.dataset.speakerChoice:null;
    clear(list);
    if(allowUnassigned)list.append(choice(null));
    const page=speakerPage(speakers,input.value,label,limit,excludeIds);
    page.rows.forEach(sp=>list.append(choice(sp)));
    status.textContent=page.total?`${page.rows.length} of ${page.total} voices`:'No matching voices. Try a name or voice number.';
    more.hidden=page.rows.length>=page.total;
    if(focused!=null)[...list.children].find(node=>node.dataset.speakerChoice===focused)?.focus();
  }
  input.addEventListener('keydown',event=>{if(event.key==='ArrowDown'){event.preventDefault();(input.value.trim() ? [...list.children].find(node=>node.dataset.speakerChoice!=='null') : list.firstElementChild)?.focus();}});
  list.addEventListener('keydown',event=>{
    if(!['ArrowUp','ArrowDown','Home','End'].includes(event.key))return;
    const buttons=[...list.children],at=buttons.indexOf(document.activeElement);if(at<0)return;
    event.preventDefault();
    if(event.key==='ArrowUp'&&at===0){input.focus();return;}
    const next=event.key==='Home'?0:event.key==='End'?buttons.length-1:Math.max(0,Math.min(buttons.length-1,at+(event.key==='ArrowDown'?1:-1)));
    buttons[next]?.focus();
  });
  refresh();
  return {input,list,status,more,nodes:[input,list,status,more],refresh};
}
