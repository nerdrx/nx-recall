// Tiny DOM + formatting helpers. No framework: the whole UI is four views and
// an event stream, and a dependency would cost more than it saves.

export function h(tag, attrs = {}, ...kids) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v == null || v === false) continue;
    if (k === 'class') el.className = v;
    else if (k === 'text') el.textContent = v;
    else if (k === 'html') el.innerHTML = v; // only ever called with our own markup
    else if (k.startsWith('on') && typeof v === 'function') el.addEventListener(k.slice(2), v);
    else if (k === 'dataset') for (const [dk, dv] of Object.entries(v)) el.dataset[dk] = dv;
    else if (k === 'style') el.setAttribute('style', v);
    else el.setAttribute(k, v === true ? '' : String(v));
  }
  for (const kid of kids.flat()) {
    if (kid == null || kid === false) continue;
    el.append(kid.nodeType ? kid : document.createTextNode(String(kid)));
  }
  return el;
}

export function svg(paths, size = 16) {
  const ns = 'http://www.w3.org/2000/svg';
  const el = document.createElementNS(ns, 'svg');
  el.setAttribute('viewBox', '0 0 24 24');
  el.setAttribute('width', size);
  el.setAttribute('height', size);
  el.setAttribute('fill', 'none');
  el.setAttribute('stroke', 'currentColor');
  el.setAttribute('stroke-width', '1.7');
  el.setAttribute('aria-hidden', 'true');
  const p = document.createElementNS(ns, 'path');
  p.setAttribute('d', paths);
  p.setAttribute('stroke-linecap', 'square');
  el.append(p);
  return el;
}

export function clear(el) {
  while (el.firstChild) el.removeChild(el.firstChild);
  return el;
}

// -- identity colour ---------------------------------------------------------

// A speaker's colour is derived from their id and clamped to the cyan→violet
// band (DESIGN §8's monogram rule), so identities are instantly separable
// without ever introducing a colour that competes with violet or means
// "attention". Unassigned speech gets no hue at all — it gets muted grey.
export function speakerHue(id) {
  const n = Number(id) || 0;
  let x = (n * 2654435761) % 4294967296;
  x = Math.abs(x);
  return 187 + (x % 104); // 187..290
}

export function speakerColor(id) {
  if (id == null) return 'var(--muted)';
  return `hsl(${speakerHue(id)} 82% 70%)`;
}

// -- formatting (locale-independent by hand — DESIGN §7) ---------------------

const p2 = (n) => String(n).padStart(2, '0');

export function fmtClock(ms) {
  const d = new Date(ms);
  return `${p2(d.getHours())}:${p2(d.getMinutes())}:${p2(d.getSeconds())}`;
}

export function fmtDay(ms) {
  const d = new Date(ms);
  return `${d.getFullYear()}-${p2(d.getMonth() + 1)}-${p2(d.getDate())}`;
}

export function fmtDayLabel(ms) {
  const day = fmtDay(ms);
  const today = fmtDay(Date.now());
  const yesterday = fmtDay(Date.now() - 86400000);
  if (day === today) return `Today · ${day}`;
  if (day === yesterday) return `Yesterday · ${day}`;
  return day;
}

export function fmtDur(ms) {
  const s = Math.round((ms || 0) / 1000);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${p2(s % 60)}s`;
  return `${Math.floor(m / 60)}h ${p2(m % 60)}m`;
}

export function fmtBytes(n) {
  const u = ['B', 'KB', 'MB', 'GB'];
  let i = 0;
  let v = Number(n) || 0;
  while (v >= 1024 && i < u.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v < 10 && i > 0 ? v.toFixed(1) : Math.round(v)} ${u[i]}`;
}

export function fmtDate(iso) {
  if (!iso) return '—';
  const t = Date.parse(iso);
  return Number.isFinite(t) ? `${fmtDay(t)} ${fmtClock(t).slice(0, 5)}` : '—';
}

// A voice minted a minute ago has no stored first_seen yet, and a zeroed
// timestamp is not a date either. "first heard —" reads like a bug in the app;
// "first heard today" is both true and the answer to the question being asked.
export function fmtFirstSeen(iso) {
  const t = iso ? Date.parse(iso) : NaN;
  if (!Number.isFinite(t) || t <= 0) return 'today';
  return fmtDate(iso);
}
