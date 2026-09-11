// Measured heights preserve complete text while only viewport rows live in the DOM.
export class HeightIndex {
  constructor(estimate = 156) { this.estimate = estimate; this.rows = []; this.heights = []; this.offsets = [0]; this.positions=new Map(); }
  rebuild() { this.offsets = [0]; this.positions.clear(); for (let i=0;i<this.heights.length;i++) { this.offsets.push(this.offsets.at(-1) + this.heights[i]); this.positions.set(this.rows[i].id,i); } }
  append(rows) { this.rows.push(...rows); this.heights.push(...rows.map(() => this.estimate)); this.rebuild(); }
  measure(index, height) { if (!(height > 0) || Math.abs(this.heights[index] - height) < .5) return false; this.heights[index] = height; return true; }
  remove(ids) { const keep = this.rows.map((row, i) => ({row, height:this.heights[i]})).filter(item => !ids.has(item.row.id)); this.rows = keep.map(item=>item.row); this.heights = keep.map(item=>item.height); this.rebuild(); }
  at(offset) { let lo=0, hi=this.rows.length; while(lo<hi) { const mid=(lo+hi)>>1; if(this.offsets[mid+1]<=offset) lo=mid+1; else hi=mid; } return Math.min(lo,Math.max(0,this.rows.length-1)); }
  get total() { return this.offsets.at(-1); }
}

export function windowedHistory(host, scroller, renderRow, onPrune = () => {}) {
  const index = new HeightIndex();
  const mounted = new Map();
  let frame=0, dead=false;
  host.style.position='relative'; host.style.overflowAnchor='none';
  host.setAttribute('aria-live','off'); host.setAttribute('role','list'); host.setAttribute('aria-label','Recorded turns');
  const origin = () => host.getBoundingClientRect().top - scroller.getBoundingClientRect().top + scroller.scrollTop;
  const anchor = () => { const y=scroller.scrollTop-origin(); const i=index.at(y); return {id:index.rows[i]?.id, offset:y-index.offsets[i], before:y<0}; };
  function restore(saved) { if(saved.before) return; const i=index.positions.get(saved.id) ?? -1; if(i>=0) scroller.scrollTop=origin()+index.offsets[i]+saved.offset; }
  function schedule() { if(!dead && !frame) frame=requestAnimationFrame(()=>{frame=0;paint();}); }
  function paint() {
    if(dead) return;
    const y=scroller.scrollTop-origin(), padding=600;
    const first=index.at(Math.max(0,y-padding)), last=index.at(y+scroller.clientHeight+padding);
    const wanted=new Set(index.rows.slice(first,last+1).map(row=>row.id));
    for(const [id,node] of mounted) if(node.contains(document.activeElement)) wanted.add(id);
    for(const [id,node] of mounted) if(!wanted.has(id)) { observer.unobserve(node); node.remove(); mounted.delete(id); }
    for(const id of wanted) {
      const i=index.positions.get(id); if(i==null)continue; const row=index.rows[i];
      let node=mounted.get(row.id);
      if(!node) { node=renderRow(row); if(!node)continue; mounted.set(row.id,node); const following=[...host.children].find(child=>index.positions.get(Number(child.dataset.historyId))>i); host.insertBefore(node,following??null); observer.observe(node); }
      Object.assign(node.style,{position:'absolute',top:`${index.offsets[i]}px`,left:'0',right:'0',margin:'0',boxSizing:'border-box'});
      node.setAttribute('role','listitem'); node.setAttribute('aria-posinset',i+1); node.setAttribute('aria-setsize',index.rows.length);
    }
    host.style.height=`${index.total}px`; host.dataset.loadedCount=index.rows.length;
    onPrune();
  }
  const observer=new ResizeObserver(entries=>{
    const saved=anchor(); let changed=false;
    for(const entry of entries) { const i=index.positions.get(Number(entry.target.dataset.historyId)) ?? -1; if(i>=0)changed=index.measure(i,entry.target.getBoundingClientRect().height+12)||changed; }
    if(changed) {index.rebuild();host.style.height=`${index.total}px`;restore(saved);schedule();}
  });
  const resize=new ResizeObserver(schedule); resize.observe(scroller);
  scroller.addEventListener('scroll',schedule,{passive:true}); host.addEventListener('focusout',schedule);
  // Explicit row navigation crosses virtual boundaries without losing focus.
  host.addEventListener('keydown',keydown);
  function keydown(event) {
    if(!['ArrowDown','ArrowUp','Home','End'].includes(event.key)||event.altKey||event.ctrlKey||event.metaKey)return;
    const current=event.target.closest('[data-history-id]'); if(!current)return;
    const i=index.positions.get(Number(current.dataset.historyId)) ?? -1;
    const next=event.key==='Home'?0:event.key==='End'?index.rows.length-1:Math.max(0,Math.min(index.rows.length-1,i+(event.key==='ArrowDown'?1:-1)));
    event.preventDefault(); scroller.scrollTop=origin()+index.offsets[next]; paint(); mounted.get(index.rows[next]?.id)?.querySelector('button')?.focus({preventScroll:true}); schedule();
  }
  host.historyState=()=>({ids:index.rows.map(row=>row.id),rendered:[...mounted.keys()],heights:[...index.heights]});
  return {
    append(rows){index.append(rows);paint();},
    update(change){
      const saved=anchor(); const deleted=new Set(change?.purged??[]);
      const focused=document.activeElement?.closest('[data-history-id]');
      const focusIndex=focused && host.contains(focused) && deleted.has(Number(focused.dataset.historyId)) ? index.positions.get(Number(focused.dataset.historyId)) : null;
      if(deleted.size) {index.remove(deleted);for(const id of deleted){const node=mounted.get(id);if(node){observer.unobserve(node);node.remove();mounted.delete(id);}}}
      for(const row of change?.updated??[]) {const existing=index.rows[index.positions.get(row.id)];if(existing)Object.assign(existing,row);}
      host.style.height=`${index.total}px`;restore(saved);paint();
      if(focusIndex!=null && index.rows.length) {const i=Math.min(focusIndex,index.rows.length-1);scroller.scrollTop=origin()+index.offsets[i];paint();mounted.get(index.rows[i].id)?.querySelector('button')?.focus({preventScroll:true});}
    },
    get count(){return index.rows.length;},
    destroy(){dead=true;cancelAnimationFrame(frame);observer.disconnect();resize.disconnect();scroller.removeEventListener('scroll',schedule);host.removeEventListener('focusout',schedule);host.removeEventListener('keydown',keydown);delete host.historyState;mounted.clear();}
  };
}
