// The captions window's renderer.
//
// It is a second, much smaller client of exactly the same machinery as the main
// window: the same preload bridge, the same `applyEvent`, the same window rules
// in lib/store.js. That is the whole design. The rule this feature would
// otherwise get wrong is 0.8.2's — the re-decode and cross-check workers walk
// the archive at idle priority and re-publish every row they stamp, and a
// caption bar that filed those as arrivals would spend its evening flashing up
// sentences from July at forty pixels tall. `applyEvent` already refuses them,
// but only once the model knows where its head is, so this window seeds itself
// from the live tail before it renders anything.
//
// What it does NOT share is the transcript's job. This shows the LIVE feed and
// only the live feed: rows appear because a `segment` event arrived while the
// window was open, never because a query answered. There is no history here and
// no way to scroll into any.

import { h, clear, speakerColor } from './lib/dom.js';
// 0.12.0 — per-person highlights. This window deliberately does NOT import
// views/highlight.js: that module owns the picker, which pulls in lib/sheets.js
// and a sheet root this document does not have. All it needs is the same two
// answers, and both are plain store selectors.
import { accentColor, iconOf } from './lib/palette.js';
import {
  store,
  applyConnState,
  applyEvent,
  applyMic,
  ask,
  clearPartial,
  followTail,
  isShaky,
  isUncertain,
  isYou,
  livePartial,
  reloadSpeakers,
  segmentSpeakerLabel,
  SHAKY_NOTE,
} from './lib/store.js';
import { normalizeCaptionSettings, translationOf, visibleCaptions, YOU_DIM } from './lib/captions.js';

const stack = document.getElementById('cap-stack');
const idle = document.getElementById('cap-idle');
const root = document.documentElement;

/** Every live turn this window has seen, newest last: {seg, at}. */
let rows = [];
let settings = normalizeCaptionSettings(null);
/** True once the tail has been fetched — before that, nothing is an arrival. */
let seeded = false;

/**
 * How much of the feed to keep. Comfortably more than the largest `turns`
 * setting, so raising the slider shows the turns that already happened rather
 * than an empty bar, and small enough that an all-night session is not a leak.
 */
const KEEP = 40;

// ---------------------------------------------------------------------------
// rendering
// ---------------------------------------------------------------------------

function applySettingsToStyle() {
  root.style.setProperty('--cap-opacity', String(settings.opacity));
  root.style.setProperty('--cap-size', `${settings.size}px`);
}

function row({ seg, dim }) {
  const shaky = isShaky(seg);
  const mine = isYou(seg.speaker);
  const uncertain = isUncertain(seg);
  // The store first, the row second — the same precedence views/highlight.js
  // uses, and it matters here because a caption arrives as an event and this
  // window seeds its speaker list separately. Byte-identical to
  // `speakerColor(seg.speaker)` for a voice nobody has highlighted.
  const known = seg.speaker != null && store.speakers.has(seg.speaker);
  const token = known ? store.speakers.get(seg.speaker)?.colour : seg.speaker_colour;
  const color = accentColor(token) || speakerColor(seg.speaker);
  const icon = known ? iconOf(store.speakers.get(seg.speaker)) : iconOf({ icon: seg.speaker_icon });
  const tr = translationOf(seg);

  const el = h('div', {
    class: `cap-row${mine ? ' you' : ''}${shaky ? ' shaky' : ''}${uncertain ? ' uncertain' : ''}`,
    dataset: { seg: String(seg.id) },
    style: `--cap-dim:${dim}`,
  });
  el.append(
    h(
      'span',
      { class: 'cap-who', style: seg.speaker == null ? '' : `color:${color}` },
      h('span', { class: 'cap-dot' }),
      // `.cap-dot` takes its colour from `.cap-who`'s `currentColor`, so the
      // highlight reaches it for free. The icon is a sibling of the name rather
      // than part of it, so the always-dark caption CSS can size it on its own.
      icon ? h('span', { class: 'sp-icon', 'aria-hidden': 'true', text: icon }) : null,
      segmentSpeakerLabel(seg)
    ),
    h(
      'span',
      { class: 'cap-text' },
      // The same "≈" the transcript and the search results wear. `text_via` is
      // deliberately absent here: which pass produced the words is a fact for
      // somebody deciding whether to retype them, and nobody is doing that at a
      // glance over a game.
      shaky ? h('span', { class: 'cap-mark', text: '≈', title: SHAKY_NOTE, 'aria-label': SHAKY_NOTE }) : null,
      seg.text || '…'
    )
  );
  if (tr) {
    el.append(
      h(
        'span',
        { class: 'cap-tr' },
        tr.lang ? h('span', { class: 'cap-tr-lang', text: tr.lang }) : null,
        tr.text
      )
    );
  }
  return el;
}

/**
 * The turn somebody is still saying (0.11.0).
 *
 * The same row, built by the same function, with two differences a person can
 * see at a glance: the ink is lighter and there is a trailing "…". That is the
 * whole design — a provisional row that looked different STRUCTURALLY would
 * jump when the final replaced it, and the jump is what makes live captions
 * unreadable. `partial` on the wrapper is what carries the ink.
 *
 * The speaker is whatever the daemon could honestly say, which on a partial is
 * either a proximity guess or nobody at all (PROTOCOL 0.11.0: no embedding is
 * computed for a partial). `speaker_hint === "proximity"` renders exactly like
 * a `label_via: "proximity"` segment, because it is the same claim.
 */
function provisionalRow(p) {
  const seg = {
    id: null,
    speaker: p.speaker,
    text: p.text,
    label_via: p.speaker_hint === 'proximity' ? 'proximity' : null,
  };
  const el = row({ seg, dim: isYou(p.speaker) ? YOU_DIM : 1 });
  el.classList.add('provisional');
  // 0.12.4: a growing row is provisional in a weaker sense than a partial. Its
  // words will not be taken back — they are already the words the finished row
  // will carry — so it is drawn in settled ink and keeps only the "…", which is
  // true of it: the sentence is not over. `growing` is what carries that, and
  // `provisional` stays so a stylesheet or a test that knows only about 0.11.0
  // still finds the row.
  if (p.growing) el.classList.add('growing');
  el.removeAttribute('data-seg');
  el.dataset.partial = String(p.seq_in_turn);
  el.querySelector('.cap-text')?.append(h('span', { class: 'cap-ell', text: '…' }));
  return el;
}

function render() {
  const view = visibleCaptions(rows, settings, Date.now(), (s) => isYou(s.speaker));
  root.style.setProperty('--cap-fade', String(view.fade));
  clear(stack);
  for (const r of view.rows) stack.append(row(r));
  // Under the settled rows, always, and outside the last-N window: it is the
  // turn happening NOW, so it is at the bottom, and it is not one of the five
  // turns you asked to keep — it is the one that has not happened yet.
  const p = livePartial();
  if (p && (settings.showYou || !isYou(p.speaker))) stack.append(provisionalRow(p));
  // The idle label is for a window that has never had anything in it, not for
  // one whose last turn has faded — a bar that says "waiting for speech" every
  // twelve seconds of quiet is a bar nobody keeps open.
  idle.hidden = rows.length > 0 || stack.childElementCount > 0;
}

// The fade is a function of the clock, so something has to tick. Four times a
// second is far below anything a person can see stepping and far above nothing.
setInterval(render, 250);

// ---------------------------------------------------------------------------
// the feed
// ---------------------------------------------------------------------------

function append(seg) {
  rows.push({ seg, at: Date.now() });
  if (rows.length > KEEP) rows = rows.slice(-KEEP);
}

window.recall.onEvent((evt) => {
  const change = applyEvent(evt, { onSpeakersChanged: render });
  if (!change) return;
  // `added` is the ONLY thing that puts a caption on screen. A row the model
  // filed as history (`outside`) or one that belongs to a window somewhere else
  // (`detached`) is not something that was just said.
  if (change.added && seeded) {
    for (const seg of change.added) append(seg);
  }
  // A correction — typed here, in the CLI, or by the re-decode worker — rewrites
  // a caption in place if it is still on screen. It is the same words, read
  // again; putting it back at the bottom would say it was said twice.
  if (change.updated) {
    for (const seg of change.updated) {
      const at = rows.findIndex((r) => r.seg.id === seg.id);
      if (at >= 0) rows[at] = { ...rows[at], seg };
    }
  }
  render();
});

window.recall.onState((st) => {
  applyConnState(st);
});

// A resync means the daemon restarted under us. The captions are live text and
// there is no live text during an outage, so the stack is dropped rather than
// left showing a conversation that stopped happening — and the tail is fetched
// again so the archive rule has a head to measure against.
window.recall.onResync(() => {
  rows = [];
  clearPartial();
  seeded = false;
  render();
  void seed();
});

async function seed() {
  await reloadSpeakers().catch(() => {});
  await ask('mic.get')
    .then(applyMic)
    .catch(() => {});
  // The point of this call is not the rows — none of them will be rendered. It
  // is `store.segments[0]`: without a head, `applyEvent` has nothing to compare
  // a re-published archive row against and would file July under now.
  await followTail().catch(() => {});
  seeded = true;
  render();
}

// ---------------------------------------------------------------------------
// settings
// ---------------------------------------------------------------------------

function applySettings(next) {
  settings = normalizeCaptionSettings(next);
  applySettingsToStyle();
  render();
}

window.recall.onCaptionSettings(applySettings);

(async function boot() {
  applySettings(await window.recall.captions.get());
  applyConnState(await window.recall.getState());
  await seed();

  // The headless driver's handle onto this window. Same discipline as the main
  // window's: it reads what is actually rendered, not what the model believes,
  // so a green assertion means a person would have seen it.
  window.__captionsDebug = {
    settings: () => ({ ...settings }),
    seeded: () => seeded,
    // What the model behind the captions knows, so the driver can prove the
    // archive rule with the same arithmetic the transcript's own step uses.
    headMs: () => store.segments[0]?.t_ms ?? null,
    rows: () =>
      [...document.querySelectorAll('.cap-row')].map((el) => {
        const id = Number(el.dataset.seg);
        const r = rows.find((x) => x.seg.id === id);
        return {
          id,
          at: r?.at ?? null,
          t_ms: r?.seg.t_ms ?? null,
          text: el.querySelector('.cap-text')?.textContent ?? '',
          who: el.querySelector('.cap-who')?.textContent ?? '',
          you: el.classList.contains('you'),
          shaky: el.classList.contains('shaky'),
          translation: el.querySelector('.cap-tr')?.textContent ?? null,
          size: getComputedStyle(el.querySelector('.cap-text')).fontSize,
          ground: getComputedStyle(el).backgroundColor,
        };
      }),
    // The provisional row, read off the DOM like everything else here: the
    // driver has to prove what a person would have seen, not what the model
    // believes it published.
    partial: () => {
      const el = document.querySelector('.cap-row.provisional');
      if (!el) return null;
      return {
        seq: Number(el.dataset.partial),
        text: el.querySelector('.cap-text')?.textContent ?? '',
        who: el.querySelector('.cap-who')?.textContent ?? '',
        ellipsis: !!el.querySelector('.cap-ell'),
        // 0.12.4: a growing row (a sliced turn) against a provisional one (a
        // partial). The difference is a claim about the words, so the test has
        // to be able to see both it and the ink that follows from it.
        growing: el.classList.contains('growing'),
        dim: getComputedStyle(el.querySelector('.cap-text')).color,
      };
    },
    /** What the MODEL holds, so "no partial afterwards" can be asserted. */
    partialModel: () => (store.partial ? { ...store.partial } : null),
    // Everything the feed has handed this window, whether or not the last-N
    // window is currently showing it.
    fed: () => rows.map((r) => ({ id: r.seg.id, t_ms: r.seg.t_ms, at: r.at })),
    fade: () => Number(getComputedStyle(root).getPropertyValue('--cap-fade')),
    idle: () => !idle.hidden,
    // The one thing this window must get right on both of NX Clear's grounds.
    ground: () => ({
      theme: root.getAttribute('data-theme'),
      scheme: getComputedStyle(root).colorScheme,
      body: getComputedStyle(document.body).backgroundColor,
      row: getComputedStyle(document.querySelector('.cap-row') ?? document.getElementById('cap-idle')).backgroundColor,
    }),
  };
})();
