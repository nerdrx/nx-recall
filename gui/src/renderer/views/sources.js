// Sources — the allowlist. DESIGN §0: capture is DEFAULT-DENY, apps are opted
// in and never out, so this view has to read as "nothing is listened to unless
// you said so": denied rows are the quiet default state, allowed rows are the
// ones that stand out.
//
// The seen-history matters as much as the toggle: an app that has been silent
// can still be pre-denied here, which is the only way to deny something before
// it ever makes a sound (DESIGN §3).

import { h, svg, clear, fmtDate, fmtBytes, speakerHue } from '../lib/dom.js';
import { store, ask, applyMic, micChip, appSources, allowedAppCount } from '../lib/store.js';
import { toast } from '../lib/sheets.js';
import { CAPTION_RANGES, normalizeCaptionSettings } from '../lib/captions.js';

export const id = 'sources';

export function mount(root, ctx) {
  const list = h('div', { id: 'source-list' });
  const sub = h('span', { class: 'sub', id: 'sources-sub' });
  const micCard = h('div', { class: 'card mic-card', id: 'mic-card' });
  const storageCard = h('div', { class: 'card', id: 'storage-card' });
  const captionsCard = h('div', { class: 'card', id: 'captions-card' });
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
    ),
    // Live captions (0.8.3). On this page rather than a settings page of its
    // own because this page is already where the app's shape is decided — what
    // it listens to, what it keeps — and a second window that floats over
    // everything is the same kind of decision.
    captionsCard,
    // What all of that costs on disk. It belongs on this page because this is
    // where the decisions that grow it are made.
    storageCard
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

  // -- storage --------------------------------------------------------------
  //
  // Four rows rather than one number, because the four behave differently and
  // only two of them are anybody's decision: audio is capped by the retention
  // window and self-limiting, the transcript grows forever and is the point,
  // the models are a fixed one-off, and kept clips are deliberately exempt.

  function renderStorage() {
    clear(storageCard);
    const s = store.status?.storage;
    storageCard.append(h('div', { class: 'card-title', text: 'Storage' }));
    if (!s) {
      storageCard.append(
        h('p', {
          class: 'rail-hint',
          id: 'storage-pending',
          style: 'padding:0;max-width:64ch',
          text: 'Not measured yet — the retention sweeper measures it once per pass, so the status poll never has to walk the disk.',
        })
      );
      return;
    }
    const rows = [
      ['db', 'Transcripts and voices', s.db_bytes, 'The memory: text, identities and the search index. It grows, and it is meant to.'],
      ['audio', 'Recordings', s.audio_bytes, `${s.audio_files ?? 0} segment file${s.audio_files === 1 ? '' : 's'}, capped by the audio retention window.`],
      ['goldens', 'Kept voice samples', s.goldens_bytes, 'Exempt from retention on purpose — a future model is re-enrolled from these.'],
      ['models', 'Speech models', s.models_bytes, 'Fixed, downloaded once. No setting shrinks these.'],
    ];
    const table = h('div', { class: 'storage-rows', id: 'storage-rows' });
    for (const [key, label, bytes, hint] of rows) {
      table.append(
        h(
          'div',
          { class: 'storage-row', dataset: { storage: key } },
          h('span', { class: 'storage-name' }, h('b', { text: label }), h('small', { text: hint })),
          h('span', { class: 'storage-bytes', text: fmtBytes(bytes ?? 0) })
        )
      );
    }
    table.append(
      h(
        'div',
        { class: 'storage-row total', dataset: { storage: 'total' } },
        h('span', { class: 'storage-name' }, h('b', { text: 'Total' })),
        h('span', { class: 'storage-bytes', text: fmtBytes(s.total_bytes ?? 0) })
      )
    );
    storageCard.append(
      table,
      h('p', {
        class: 'rail-hint',
        id: 'storage-note',
        style: 'padding:10px 0 0;max-width:64ch',
        text: 'Recordings age out on the retention window and are the only part that shrinks on its own; the speech models are a fixed download. Nothing here leaves the machine.',
      })
    );
  }

  // -- live captions --------------------------------------------------------
  //
  // Six controls and one button. Every one of them is a preference about a
  // window that sits over somebody's game, and not one of them reaches the
  // daemon: the captions are drawn from the same live feed this window is
  // already reading, so all the settings decide is how it looks.
  //
  // The values are pushed live rather than on release. A person setting the
  // type size of a caption bar is looking at the caption bar, not at this card,
  // and a slider that only takes effect when you let go makes that a guessing
  // game. The main process debounces the disk write behind it.

  let caps = normalizeCaptionSettings(null);
  let capsOpen = false;

  async function pushCaptions(patch) {
    caps = normalizeCaptionSettings(await window.recall.captions.set(patch));
    renderCaptions();
  }

  function capSlider(key, label, hint, format) {
    const range = CAPTION_RANGES[key];
    const value = h('span', { class: 'cap-value', dataset: { capValue: key }, text: format(caps[key]) });
    const input = h('input', {
      class: 'cap-range',
      type: 'range',
      dataset: { cap: key },
      min: String(range.min),
      max: String(range.max),
      step: String(range.step),
      value: String(caps[key]),
      'aria-label': label,
      oninput: (e) => {
        // Paint the number from the INPUT, not from the round trip: the round
        // trip is a millisecond away and a label that lags the thumb by one
        // frame is the thing that makes a slider feel broken.
        value.textContent = format(Number(e.target.value));
        void pushCaptions({ [key]: Number(e.target.value) });
      },
    });
    return h(
      'div',
      { class: 'cap-row-ctl' },
      h('span', { class: 'cap-label' }, h('b', { text: label }), h('small', { text: hint })),
      input,
      value
    );
  }

  function capToggle(key, label, hint) {
    return h(
      'div',
      { class: 'cap-row-ctl' },
      h('span', { class: 'cap-label' }, h('b', { text: label }), h('small', { text: hint })),
      h('span', { class: 'spacer' }),
      h('button', {
        class: 'toggle',
        role: 'switch',
        dataset: { cap: key },
        'aria-pressed': String(!!caps[key]),
        'aria-label': label,
        onclick: () => pushCaptions({ [key]: !caps[key] }),
      })
    );
  }

  function renderCaptions() {
    clear(captionsCard);
    captionsCard.append(
      h(
        'div',
        { class: 'sheet-head' },
        h('span', { class: 'card-title', text: 'Captions' }),
        h(
          'button',
          {
            class: 'btn small',
            id: 'captions-open',
            'aria-pressed': String(capsOpen),
            onclick: async () => {
              capsOpen = await window.recall.captions.toggle();
              renderCaptions();
            },
          },
          capsOpen ? 'Hide captions' : 'Show captions'
        )
      ),
      h('p', {
        class: 'rail-hint',
        id: 'captions-hint',
        style: 'padding:0 0 12px;max-width:64ch',
        text: 'The last few turns, in large type, in a window that floats over everything else and ignores the mouse. It shows what is being said now — never history — and it is the same live feed this transcript is reading. There is no keyboard shortcut on purpose: a global one would take a key away from whatever you are playing.',
      }),
      capSlider('turns', 'Turns on screen', 'How many of the most recent turns the bar holds.', (v) => String(v)),
      capSlider('size', 'Text size', 'The words themselves; names and translations scale with them.', (v) => `${v} px`),
      capSlider('hold_s', 'Hold', 'How long the bar stays up after the last thing anybody said.', (v) => `${v} s`),
      capSlider('opacity', 'Ground', 'How much of what is underneath the captions cover.', (v) => `${Math.round(v * 100)}%`),
      capToggle('showYou', 'Show your own turns', 'Kept dimmer than everybody else’s, because you already know what you said.'),
      capToggle('clickThrough', 'Ignore the mouse', 'On, clicks land in whatever is underneath. Off, the bar can be dragged and resized.')
    );
  }

  // -- the applications -----------------------------------------------------

  function render() {
    renderMic();
    renderStorage();
    renderCaptions();
    clear(list);
    // The microphone has its own card above; it must not also appear as a row
    // in a list whose every other entry is opted in through the allowlist —
    // nor in the count the rail badge draws from (audit finding #25a), which is
    // why both come from the same rule in store.js.
    const rows = appSources().sort(
      (a, b) => Number(b.allowed) - Number(a.allowed) || String(a.display ?? a.match_key).localeCompare(String(b.display ?? b.match_key))
    );
    const allowed = allowedAppCount();
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
      // Only the hue travels: the plate, its hairline and the ink are all
      // composed from it in styles.css out of theme tokens, so the same app
      // keeps the same identity colour on both grounds and stays readable on
      // each (see .src-mono).
      h('span', {
        class: 'src-mono',
        text: String(s.display ?? s.match_key).slice(0, 2).toUpperCase(),
        style: `--sp-h:${hue}`,
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
  // The captions card's truth lives in the main process — the window can be
  // opened from the tray or from a command line while this view is not
  // mounted — so a mount ASKS rather than assuming the defaults it just drew.
  void (async () => {
    try {
      const st = await window.recall.captions.state();
      caps = normalizeCaptionSettings(st?.settings);
      capsOpen = !!st?.open;
      renderCaptions();
    } catch {
      /* an older main process with no captions channel: the defaults stand */
    }
  })();

  void ctx;
  return {
    update(change) {
      if (change?.mic || change?.conn) renderMic();
      if (change?.status) renderStorage();
      if (change?.sources || change?.status) render();
      // A settings broadcast carries the whole block; the rail button's own
      // click carries only `true`, and then only the open/closed half moved.
      if (change?.captions) {
        if (typeof change.captions === 'object') caps = normalizeCaptionSettings(change.captions);
        void window.recall.captions
          .state()
          .then((st) => {
            capsOpen = !!st?.open;
            renderCaptions();
          })
          .catch(() => renderCaptions());
      }
    },
    render,
  };
}

function hash(str) {
  let x = 0;
  for (const ch of String(str)) x = (x * 31 + ch.charCodeAt(0)) % 1000003;
  return x;
}
