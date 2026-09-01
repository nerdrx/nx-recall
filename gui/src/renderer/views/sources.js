// Sources — the allowlist. DESIGN §0: capture is DEFAULT-DENY, apps are opted
// in and never out, so this view has to read as "nothing is listened to unless
// you said so": denied rows are the quiet default state, allowed rows are the
// ones that stand out.
//
// The seen-history matters as much as the toggle: an app that has been silent
// can still be pre-denied here, which is the only way to deny something before
// it ever makes a sound (DESIGN §3).

import { h, svg, clear, fmtDate, speakerHue } from '../lib/dom.js';
import { store, ask, applyMic, micChip } from '../lib/store.js';
import { toast } from '../lib/sheets.js';

export const id = 'sources';

export function mount(root, ctx) {
  const list = h('div', { id: 'source-list' });
  const sub = h('span', { class: 'sub', id: 'sources-sub' });
  const micCard = h('div', { class: 'card mic-card', id: 'mic-card' });
  const body = h(
    'div',
    { class: 'view-body view-enter' },
    // The microphone sits ABOVE the app list, because it is the one source
    // whose consent question is different in kind: an app rule is about a
    // program's output, this is about the room.
    micCard,
    h(
      'div',
      { class: 'card' },
      h('div', { class: 'card-title', text: 'Applications' }),
      h('p', {
        class: 'rail-hint',
        style: 'padding:0 0 12px;max-width:64ch',
        text: 'Nothing is captured until you allow it here, and allowing takes effect immediately — no restart, no queue. Apps that have been seen but never allowed can be denied ahead of time.',
      }),
      list
    )
  );

  root.append(
    h('div', { class: 'view-head' }, h('div', {}, h('h1', { text: 'Sources' }), sub), h('div', { class: 'spacer' })),
    body
  );

  // -- the microphone -------------------------------------------------------

  let micPending = false;

  function renderMic() {
    const mic = store.mic;
    const chip = micChip(mic.state);
    clear(micCard);

    const toggle = h('button', {
      class: 'toggle',
      role: 'switch',
      id: 'mic-toggle',
      'aria-pressed': String(!!mic.enabled),
      'aria-label': mic.enabled ? 'Turn the microphone off' : 'Turn the microphone on',
      disabled: micPending || store.conn.status !== 'connected',
      onclick: () => setMic({ enabled: !mic.enabled }),
    });

    const mode = (value, label, hint) =>
      h(
        'button',
        {
          class: 'mode-opt',
          dataset: { mode: value },
          'aria-pressed': String(mic.mode === value),
          disabled: micPending || !mic.enabled,
          onclick: () => setMic({ mode: value }),
        },
        h('b', { text: label }),
        h('small', { text: hint })
      );

    micCard.append(
      h(
        'div',
        { class: 'mic-head' },
        h(
          'span',
          { class: 'mic-ico', 'aria-hidden': 'true' },
          svg('M12 3a3 3 0 0 1 3 3v6a3 3 0 0 1-6 0V6a3 3 0 0 1 3-3M5 11a7 7 0 0 0 14 0M12 18v3', 18)
        ),
        h(
          'span',
          { class: 'mic-title' },
          h('span', { class: 'name', text: 'Microphone' }),
          h('span', { class: 'key', text: mic.device ?? 'system default input' })
        ),
        h('span', { class: 'spacer' }),
        h('span', { class: chip.cls, id: 'mic-chip' }, h('span', { class: `dot${chip.live ? ' pulse' : ''}` }), chip.text),
        toggle
      ),
      // The one sentence that has to be unmissable. It is not a caveat about
      // the feature — it IS the feature's shape.
      h('p', {
        class: 'mic-warn',
        id: 'mic-warning',
        text: 'Your microphone hears the room, not the game. Anyone speaking near you is recorded and transcribed, whether or not they are in the instance.',
      }),
      h(
        'div',
        { class: 'mic-modes', id: 'mic-modes', role: 'group', 'aria-label': 'When the microphone records' },
        mode('follow', 'Follow allowed apps', 'Only while something in the list below is being captured.'),
        mode('always', 'Always', 'Whenever NX Recall is running.')
      ),
      h('p', {
        class: 'rail-hint',
        style: 'padding:8px 0 0;max-width:64ch',
        text: 'Your own voice is labelled from where it came, not from a guess — it needs no naming and it enrols itself.',
      })
    );
  }

  async function setMic(change) {
    const before = { ...store.mic };
    // Optimistic, like the source toggles: the daemon confirms with a `mic`
    // event and a `status` push, and a failure puts it back.
    applyMic(change);
    micPending = true;
    renderMic();
    try {
      applyMic(await ask('mic.set', change));
      toast(
        store.mic.enabled
          ? `Microphone on — ${store.mic.mode === 'always' ? 'recording whenever NX Recall runs' : 'recording only while an allowed app is captured'}.`
          : 'Microphone off. Nothing from the room is recorded.',
        'ok'
      );
    } catch (e) {
      store.mic = before;
      toast(`Could not change the microphone — ${e.message}`, 'error');
    } finally {
      micPending = false;
      renderMic();
    }
  }

  // -- the applications -----------------------------------------------------

  function render() {
    renderMic();
    clear(list);
    // The microphone has its own card above; it must not also appear as a row
    // in a list whose every other entry is opted in through the allowlist.
    const rows = [...store.sources]
      .filter((s) => s.kind !== 'mic')
      .sort((a, b) => Number(b.allowed) - Number(a.allowed) || String(a.display ?? a.match_key).localeCompare(String(b.display ?? b.match_key)));
    const allowed = rows.filter((s) => s.allowed).length;
    sub.textContent = `${allowed} allowed · ${rows.length - allowed} denied`;
    const badge = document.getElementById('badge-sources');
    if (badge) badge.textContent = String(allowed);

    if (!rows.length) {
      list.append(
        h(
          'div',
          { class: 'empty' },
          h('b', { text: 'No applications seen yet' }),
          h('p', { text: 'Anything that plays or captures audio shows up here the first time it does, denied by default.' })
        )
      );
      return;
    }
    for (const s of rows) list.append(sourceRow(s));
  }

  function sourceRow(s) {
    const hue = speakerHue(hash(s.match_key));
    const row = h('div', {
      class: `src-row${s.allowed ? '' : ' denied'}`,
      dataset: { source: s.match_key },
    });
    const toggle = h('button', {
      class: 'toggle',
      role: 'switch',
      'aria-pressed': String(!!s.allowed),
      'aria-label': `${s.allowed ? 'Deny' : 'Allow'} capture from ${s.display ?? s.match_key}`,
      dataset: { toggle: s.match_key },
      onclick: () => setAllowed(s, !s.allowed),
    });

    row.append(
      h('span', {
        class: 'src-mono',
        text: String(s.display ?? s.match_key).slice(0, 2).toUpperCase(),
        style: `--mono-a:hsl(${hue} 80% 72%);--mono-b:hsl(${hue} 70% 42%)`,
      }),
      h(
        'span',
        {},
        h('span', { class: 'name', text: s.display ?? s.match_key }),
        h('span', { class: 'key', text: ` ${s.match_key}${s.binary && s.binary !== s.match_key ? ` · ${s.binary}` : ''}` })
      ),
      h(
        'span',
        { class: 'seen' },
        h('div', { text: `first seen ${fmtDate(s.first_seen)}` }),
        h('div', { text: `last seen ${fmtDate(s.last_seen)}` })
      ),
      h(
        'span',
        { class: 'sp-actions' },
        s.allowed
          ? h('span', { class: `chip ${s.streams > 0 ? 'live' : ''}`.trim() }, h('span', { class: 'dot' }), s.streams > 0 ? 'capturing' : 'allowed')
          : h('span', { class: 'chip', text: 'denied' }),
        toggle
      )
    );
    return row;
  }

  async function setAllowed(s, next) {
    // Optimistic: the toggle is a live switch and the daemon confirms with a
    // `source` event. On failure the event never arrives and we put it back.
    const before = s.allowed;
    s.allowed = next;
    render();
    try {
      await ask('sources.set', { match_key: s.match_key, allowed: next });
      toast(next ? `Capturing from ${s.display ?? s.match_key}.` : `${s.display ?? s.match_key} denied — capture stopped.`, 'ok');
    } catch (e) {
      s.allowed = before;
      render();
      toast(`Could not change ${s.display ?? s.match_key} — ${e.message}`, 'error');
    }
  }

  render();
  void ctx;
  return {
    update(change) {
      if (change?.mic || change?.conn) renderMic();
      if (change?.sources || change?.status) render();
    },
    render,
  };
}

function hash(str) {
  let x = 0;
  for (const ch of String(str)) x = (x * 31 + ch.charCodeAt(0)) % 1000003;
  return x;
}
