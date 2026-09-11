// Sheets, confirms and toasts — the only floating layers in the app. The scrim
// behind a sheet is the one place real backdrop-filter is spent: NX Clear
// reserves it for the single layer that overlaps live content (§14 axis 3), and
// everything else here is a flat surface with a hairline and a soft shadow.

import { h, clear } from './dom.js';

const root = () => document.getElementById('sheet-root');

let openCount = 0;
let nextSheetId = 0;

/** openSheet(build) — build(close) returns the sheet's children. */
export function openSheet(build, { onClose } = {}) {
  const previousFocus = document.activeElement;
  let closed = false;
  const scrim = h('div', { class: 'scrim', role: 'presentation' });
  const sheet = h('div', { class: 'sheet', role: 'dialog', 'aria-modal': 'true' });

  const close = (result) => {
    if (closed) return;
    closed = true;
    scrim.remove();
    openCount = Math.max(0, openCount - 1);
    document.removeEventListener('keydown', onKey, true);
    if (previousFocus?.isConnected) previousFocus.focus({ preventScroll: true });
    if (onClose) onClose(result);
  };
  const onKey = (e) => {
    if (root().lastElementChild !== scrim) return;
    if (e.key === 'Tab') {
      const controls = [...sheet.querySelectorAll('button:not(:disabled),input:not(:disabled),textarea:not(:disabled),select:not(:disabled),a[href]')].filter(el => !el.hidden && el.getClientRects().length);
      const first = controls[0]; const last = controls.at(-1);
      if (first && ((!e.shiftKey && (e.target === last || !sheet.contains(e.target))) || (e.shiftKey && (e.target === first || !sheet.contains(e.target))))) {
        e.preventDefault(); (e.shiftKey ? last : first).focus();
      }
      return;
    }
    if (e.key !== 'Escape') return;
    // This listener is on `document` in the CAPTURE phase, so it runs before
    // anything inside the sheet ever sees the key — which is right for a sheet
    // and wrong for a control inside one that has its own idea of Escape. The
    // inline transcript fix is the first: Escape there abandons the edit and
    // keeps the sheet, and a `stopPropagation` in its own handler can never
    // reach a listener that has already fired. So the opt-out is declared on
    // the element instead, and the nearer meaning wins.
    if (e.target?.closest?.('[data-keep-escape]')) return;
    e.stopPropagation();
    close(null);
  };

  sheet.append(...[build(close)].flat().filter(Boolean));
  const heading = sheet.querySelector('h2,h3');
  if (heading) { heading.id ||= `sheet-title-${++nextSheetId}`; sheet.setAttribute('aria-labelledby', heading.id); }
  scrim.append(sheet);
  scrim.addEventListener('mousedown', (e) => {
    if (e.target === scrim) close(null);
  });
  document.addEventListener('keydown', onKey, true);
  root().append(scrim);
  openCount += 1;

  // Focus the first control so the sheet is keyboard-usable immediately.
  const first = sheet.querySelector('input, textarea, button, select');
  if (first) first.focus();
  return close;
}

export function sheetsOpen() {
  return openCount > 0;
}

/** A confirm that states the consequence plainly (DESIGN §9). */
export function confirmSheet({ title, body, confirmLabel = 'Confirm', danger = false, detail = null }) {
  return new Promise((resolve) => {
    let settled = false;
    const finish = (v) => {
      if (settled) return;
      settled = true;
      resolve(v);
    };
    const close = openSheet(
      () => [
        h('h2', { text: title }),
        h('p', { class: 'sub', text: body }),
        detail ? h('div', { class: 'quote', text: detail }) : null,
        h(
          'div',
          { class: 'actions' },
          h('button', { class: 'btn', onclick: () => { finish(false); close(false); } }, 'Cancel'),
          h(
            'button',
            { class: `btn ${danger ? 'danger' : 'primary'}`, onclick: () => { finish(true); close(true); } },
            confirmLabel
          )
        ),
      ],
      { onClose: () => finish(false) }
    );
  });
}

/**
 * A confirm with more than one way to say yes (DESIGN §8).
 *
 * `confirmSheet` asks a yes/no question. Some destructive acts are not one:
 * deleting a voice is "the words, or the words and the voice", and offering
 * only the second would delete more than anybody asked for while offering only
 * the first is the bug this exists to fix. So the choices are the buttons —
 * each with its own label and its own weight — and Cancel is always last and
 * always plain.
 *
 * Resolves with the chosen `value`, or `null` for Cancel / Escape / a click on
 * the scrim. A choice can be `danger: true`; more than one should not be, or
 * the styling stops meaning anything.
 */
export function chooseSheet({ title, body, detail = null, choices = [], cancelLabel = 'Cancel' }) {
  return new Promise((resolve) => {
    let settled = false;
    const finish = (v) => {
      if (settled) return;
      settled = true;
      resolve(v);
    };
    const close = openSheet(
      () => [
        h('h2', { text: title }),
        h('p', { class: 'sub', text: body }),
        detail ? h('div', { class: 'quote', text: detail }) : null,
        h(
          'div',
          { class: 'actions stacked' },
          ...choices.map((c) =>
            h(
              'button',
              {
                class: `btn ${c.danger ? 'danger' : 'primary'}`,
                dataset: c.key ? { choice: c.key } : {},
                onclick: () => {
                  finish(c.value);
                  close(c.value);
                },
              },
              c.label
            )
          ),
          h(
            'button',
            {
              class: 'btn',
              dataset: { choice: 'cancel' },
              onclick: () => {
                finish(null);
                close(null);
              },
            },
            cancelLabel
          )
        ),
      ],
      { onClose: () => finish(null) }
    );
    // `openSheet` focuses the first control, which here is the most emphasised
    // action — and when the only action is destructive that would put Enter one
    // keystroke from a delete. The keyboard lands on the first thing that
    // destroys nothing instead, which is the safe choice when there is one and
    // Cancel when there is not.
    const scrims = document.querySelectorAll('#sheet-root .scrim');
    scrims[scrims.length - 1]?.querySelector('.actions .btn:not(.danger)')?.focus();
  });
}

export function toast(text, kind = '') {
  const box = document.getElementById('toasts');
  const el = h('div', { class: `toast ${kind}`.trim(), text });
  box.append(el);
  // Errors stay until they are pushed out; everything else auto-dismisses.
  const ttl = kind === 'error' ? 9000 : 4200;
  setTimeout(() => {
    el.style.transition = 'opacity var(--dur) var(--ease-soft)';
    el.style.opacity = '0';
    setTimeout(() => el.remove(), 260);
  }, ttl);
  while (box.children.length > 4) box.firstChild.remove();
  return el;
}

export function clearToasts() {
  clear(document.getElementById('toasts'));
}
