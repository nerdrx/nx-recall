// Sources — the allowlist. DESIGN §0: capture is DEFAULT-DENY, apps are opted
// in and never out, so this view has to read as "nothing is listened to unless
// you said so": denied rows are the quiet default state, allowed rows are the
// ones that stand out.
//
// The seen-history matters as much as the toggle: an app that has been silent
// can still be pre-denied here, which is the only way to deny something before
// it ever makes a sound (DESIGN §3).

import { h, svg, clear, fmtDate, fmtBytes, speakerHue } from '../lib/dom.js';
// 0.12.0 — per-person highlights. Only `iconOf` is wanted in this view: the one
// place a speaker is named here is inside a native <option>, which cannot be
// coloured. See `truthRow`.
import { iconOf } from '../lib/palette.js';
import {
  store,
  ask,
  applyMic,
  micChip,
  appSources,
  allowedAppCount,
  // 0.10.0: the second microphone's switch, folded in exactly like the first.
  applyRoom,
  roomChip,
  isNamed,
} from '../lib/store.js';
import { toast } from '../lib/sheets.js';
import { CAPTION_RANGES, normalizeCaptionSettings } from '../lib/captions.js';

export const id = 'sources';

export function mount(root, ctx) {
  const list = h('div', { id: 'source-list' });
  const sub = h('span', { class: 'sub', id: 'sources-sub' });
  const micCard = h('div', { class: 'card mic-card', id: 'mic-card' });
  // 0.10.0. Three more cards, all of them on this page because this page is
  // where what the program listens to and what it does with it is decided.
  const roomCard = h('div', { class: 'card mic-card', id: 'room-card' });
  const truthCard = h('div', { class: 'card', id: 'truth-card' });
  const exportCard = h('div', { class: 'card', id: 'export-card' });
  const storageCard = h('div', { class: 'card', id: 'storage-card' });
  const captionsCard = h('div', { class: 'card', id: 'captions-card' });
  const body = h(
    'div',
    { class: 'view-body view-enter' },
    // The microphone sits ABOVE the app list, because it is the one source
    // whose consent question is different in kind: an app rule is about a
    // program's output, this is about the room.
    micCard,
    // The room mic sits directly beside the headset's: they are the two
    // devices in the flat, and the difference between them is the thing a
    // person has to be able to see at a glance.
    roomCard,
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
    // The Discord bridge (0.9.0's ground truth), which is a source of
    // *labels* rather than of audio — and belongs here because the question it
    // answers is the same one every other card on this page answers: what is
    // coming in, and from where.
    truthCard,
    // Writing it back out (0.10.0). Under everything that produces the
    // transcript, because it is the last thing you do with one.
    exportCard,
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

  // -- the room microphone (0.10.0) -----------------------------------------
  //
  // A second card beside the first, deliberately alike: the same head, the same
  // chip, the same two modes. Three things differ, and each is a fact about the
  // device rather than a UI choice — it needs a device chosen before it can do
  // anything, its warning is about other people rather than about the room
  // around you, and it says out loud that these voices are NOT you.

  let roomPending = false;
  let devices = [];
  let devicesError = null;

  async function loadDevices() {
    try {
      devices = (await ask('devices.list'))?.devices ?? [];
      devicesError = null;
    } catch (e) {
      devices = [];
      // An older daemon has no `devices.list`. Say so rather than rendering an
      // empty picker that looks like "you own no microphones".
      devicesError = e.message;
    }
    renderRoom();
  }

  function renderRoom() {
    const room = store.room;
    const chip = roomChip(room.state);
    clear(roomCard);

    const known = devices.some((d) => d.node_name === room.device);
    const toggle = h('button', {
      class: 'toggle',
      role: 'switch',
      id: 'room-toggle',
      'aria-pressed': String(!!room.enabled),
      'aria-label': room.enabled ? 'Turn the room microphone off' : 'Turn the room microphone on',
      // The one disabled state that is not a connection problem: there is
      // nothing for the switch to open, and the daemon would refuse it.
      disabled: roomPending || store.conn.status !== 'connected' || (!room.enabled && !room.device),
      onclick: () => setRoom({ enabled: !room.enabled }),
    });

    const picker = h(
      'select',
      {
        class: 'cap-select',
        id: 'room-device',
        'aria-label': 'Which microphone hears the room',
        disabled: roomPending || store.conn.status !== 'connected',
        onchange: (e) => setRoom({ device: e.target.value || null }),
      },
      h('option', { value: '', text: room.device ? 'No device (turns it off)' : 'Choose a device…' }),
      ...devices.map((d) =>
        h('option', {
          value: d.node_name,
          selected: d.node_name === room.device,
          // The default input is almost always the headset the card above is
          // already on, and pointing this one at it would record the user
          // twice under two identities. Say which one it is.
          text: `${d.description || d.node_name}${d.is_default ? ' — system default (your headset)' : ''}`,
        })
      ),
      // A pinned device that is not on the graph right now must still show as
      // the chosen one: unplugging a mic is not un-choosing it.
      ...(room.device && !known ? [h('option', { value: room.device, selected: true, text: `${room.device} — not connected` })] : [])
    );

    const mode = (value, label, hint) =>
      h(
        'button',
        {
          class: 'mode-opt',
          dataset: { roomMode: value },
          'aria-pressed': String(room.mode === value),
          disabled: roomPending || !room.enabled,
          onclick: () => setRoom({ mode: value }),
        },
        h('b', { text: label }),
        h('small', { text: hint })
      );

    roomCard.append(
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
          h('span', { class: 'name', text: 'Room microphone' }),
          h('span', { class: 'key', text: room.device ?? 'no device chosen' })
        ),
        h('span', { class: 'spacer' }),
        h('span', { class: chip.cls, id: 'room-chip' }, h('span', { class: `dot${chip.live ? ' pulse' : ''}` }), chip.text),
        toggle
      ),
      // As blunt as the microphone's, and about somebody else. This is the one
      // card in the program where the people affected are not in the room's
      // conversation by choice and are not in the instance at all.
      h('p', {
        class: 'mic-warn',
        id: 'room-warning',
        text: 'A second microphone for the people physically in the room with you. Everyone it hears is recorded and transcribed — a partner, a flatmate, a friend on the sofa — whether or not they are in the instance. Their voices are matched, named and remembered like anybody else’s; nothing on this device is marked as you.',
      }),
      h(
        'div',
        { class: 'cap-row-ctl' },
        h(
          'span',
          { class: 'cap-label' },
          h('b', { text: 'Device' }),
          h('small', {
            text: devicesError
              ? `Could not list the capture devices — ${devicesError}`
              : 'There is no default: pick the microphone that is in the room, not the one on your head.',
          })
        ),
        picker,
        h('button', {
          class: 'btn small',
          id: 'room-refresh',
          text: 'Refresh',
          onclick: () => void loadDevices(),
        })
      ),
      h(
        'div',
        { class: 'mic-modes', id: 'room-modes', role: 'group', 'aria-label': 'When the room microphone records' },
        mode('follow', 'Follow allowed apps', 'Only while something in the list below is being captured.'),
        mode('always', 'Always', 'Whenever NX Recall is running.')
      )
    );
  }

  async function setRoom(change) {
    const before = { ...store.room };
    applyRoom(change);
    roomPending = true;
    renderRoom();
    try {
      applyRoom(await ask('room.set', change));
      toast(
        store.room.enabled
          ? `Room microphone on — ${store.room.mode === 'always' ? 'recording whenever NX Recall runs' : 'recording only while an allowed app is captured'}.`
          : 'Room microphone off. Nothing from the room is recorded.',
        'ok'
      );
    } catch (e) {
      store.room = before;
      toast(`Could not change the room microphone — ${e.message}`, 'error');
    } finally {
      roomPending = false;
      renderRoom();
    }
  }

  // -- the Discord bridge (0.9.0's ground truth) ----------------------------
  //
  // Not a source of audio: a source of LABELS. Discord knows who was speaking
  // and when, which is the only yardstick this program has ever had for
  // speaker identity — and the card's job is to say what is arriving, from
  // whom, and how well the voicebank is doing against it.

  let truth = null;
  let truthUsers = [];
  let truthSummary = null;
  let truthPending = null;

  async function loadTruth() {
    try {
      const [status, users, summary] = await Promise.all([
        ask('truth.status').catch(() => null),
        ask('truth.users').catch(() => null),
        ask('truth.summary').catch(() => null),
      ]);
      truth = status;
      truthUsers = users?.users ?? [];
      truthSummary = summary;
    } catch {
      /* an older daemon has none of these; the card says "off" */
    }
    renderTruth();
  }

  /**
   * Has the plugin sent anything in the last minute?
   *
   * The newest of the two answers, not the first: `truth.status` was asked once
   * on mount and the status push keeps arriving, so preferring either one on
   * its own would leave a live bridge reading as silent within a minute.
   */
  function truthLive() {
    const last = Math.max(truth?.last_span_ms ?? 0, store.status?.truth?.last_event_ms ?? 0);
    return last > 0 && Date.now() - last < 60_000;
  }

  function truthChip() {
    if (!(truth?.enabled ?? store.status?.truth?.enabled)) return { text: 'off', cls: 'chip' };
    if (truthLive()) return { text: 'receiving', cls: 'chip live', live: true };
    return { text: 'waiting for Discord', cls: 'chip' };
  }

  /**
   * Per-user audio (0.12.1), in one sentence.
   *
   * Three states worth telling apart and one that must never be guessed at: a
   * daemon too old to have the feature says nothing rather than "off", because
   * "off" is a claim about a switch that does not exist there.
   */
  function audioLine() {
    const a = truth?.audio;
    if (!a) {
      return 'Per-user audio: not available on this daemon.';
    }
    if (!a.enabled) {
      return 'Per-user audio is off. With it on, Vesktop sends each person in the call as their own stream, and every turn is that person by construction — no voice matching, no overlap to un-mix. Set [truth] audio = true and switch it on in Vencord → RecallBridge as well; both sides are off by default, because this takes people’s voices out of the client.';
    }
    if (!a.live) {
      return 'Per-user audio is on and nothing is arriving. Join a voice call with RecallBridge’s audio option enabled; until a stream arrives, Discord is recorded off the speakers exactly as before.';
    }
    const names = a.streams
      .filter((s) => s.live)
      .map((s) => s.name || s.user_id)
      .join(', ');
    return `Per-user audio: ${a.live} live stream${a.live === 1 ? '' : 's'} — ${names}. Each is recorded and named as that person; the mixed Discord tap is muted while they are arriving, so nothing is transcribed twice.`;
  }

  /** `precision · recall · n`, or an honest sentence when there is nothing. */
  function scorecard() {
    const id = truthSummary?.identity;
    if (!id || !id.n) return { text: 'no clean turns scored yet', empty: true };
    const pct = (v) => (v == null ? '—' : `${Math.round(v * 100)}%`);
    return {
      text: `precision ${pct(id.precision)} · recall ${pct(id.recall)} · n ${id.n}`,
      empty: false,
    };
  }

  function renderTruth() {
    clear(truthCard);
    const brief = store.status?.truth ?? null;
    const enabled = truth?.enabled ?? brief?.enabled ?? false;
    const chip = truthChip();

    truthCard.append(
      h(
        'div',
        { class: 'sheet-head' },
        h('span', { class: 'card-title', text: 'Discord' }),
        h('span', { class: 'spacer' }),
        h('span', { class: chip.cls, id: 'truth-chip' }, h('span', { class: `dot${chip.live ? ' pulse' : ''}` }), chip.text)
      ),
      h('p', {
        class: 'rail-hint',
        id: 'truth-hint',
        style: 'padding:0 0 12px;max-width:64ch',
        text: 'Discord already knows who was speaking and when. The RecallBridge plugin sends that here — speaking edges, who is in the channel, and their nicknames. No audio, no messages, no text of any kind, and none of it leaves this machine: it is the only yardstick this program has for whether it is naming voices correctly.',
      })
    );

    if (!enabled) {
      truthCard.append(
        h(
          'div',
          { class: 'empty', id: 'truth-off' },
          h('b', { text: 'The bridge is off' }),
          h('p', {
            text: 'Run `recalld truth on`, restart the daemon, then paste the token from `recalld truth token` into Vencord → RecallBridge.',
          })
        )
      );
      return;
    }

    const score = scorecard();
    truthCard.append(
      h(
        'p',
        { class: 'rail-hint', id: 'truth-score', style: 'padding:0 0 10px;max-width:64ch' },
        score.empty
          ? 'Identity score: no clean turns scored yet — a turn counts only when Discord says one person spoke for most of it, it is at least a second long, and that account is linked to a voice.'
          : `Identity score: ${score.text}. Precision is how often it is right when it answers; recall is how often it answers at all.`
      ),
      // 0.12.1: per-user audio. A different claim from everything else on this
      // card — the rest of it is about measuring how well voices are being
      // recognised, and this is about not having to recognise them.
      h('p', { class: 'rail-hint', id: 'truth-audio', style: 'padding:0 0 12px;max-width:64ch' }, audioLine())
    );

    if (!truthUsers.length) {
      truthCard.append(
        h(
          'div',
          { class: 'empty', id: 'truth-empty' },
          h('b', { text: 'Nobody heard yet' }),
          h('p', { text: 'Accounts appear here the first time the plugin reports them speaking.' })
        )
      );
      return;
    }

    const named = [...store.speakers.values()].filter(isNamed);
    const list = h('div', { id: 'truth-users' });
    for (const u of truthUsers) {
      list.append(truthRow(u, named));
    }
    truthCard.append(list);
  }

  function truthRow(u, named) {
    const row = h('div', { class: 'src-row', dataset: { truthUser: u.user_id } });
    const select = h(
      'select',
      {
        class: 'cap-select',
        dataset: { truthLink: u.user_id },
        disabled: truthPending === u.user_id,
        onchange: (e) => setTruthLink(u, e.target.value),
      },
      h('option', { value: '', selected: u.speaker == null, text: 'Link…' }),
      // 0.12.0 — the icon, and only the icon. An `<option>` is drawn by the
      // platform, not by us: Chromium ignores every colour we could put on one
      // (tokens.css says as much about `color-scheme` and native popups), so a
      // highlight's colour has nowhere to go here. The emoji renders fine, and
      // on a long list of named voices it is the fastest way to find the person
      // you are linking a Discord account to.
      ...named.map((sp) => {
        const ic = iconOf(sp);
        return h('option', {
          value: String(sp.id),
          selected: sp.id === u.speaker,
          text: `${ic ? `${ic} ` : ''}${sp.name ?? sp.auto}`,
        });
      }),
      // A voice linked automatically may not be named yet, and the select must
      // still be able to show what it is linked TO.
      ...(u.speaker != null && !named.some((sp) => sp.id === u.speaker)
        ? [h('option', { value: String(u.speaker), selected: true, text: u.speaker_name ?? `voice ${u.speaker}` })]
        : [])
    );

    row.append(
      h('span', {
        class: 'src-mono',
        text: String(u.name ?? u.user_id).slice(0, 2).toUpperCase(),
        style: `--sp-h:${speakerHue(hash(u.user_id))}`,
      }),
      h(
        'span',
        {},
        // The Discord nickname, shown and never applied: it is per-guild and
        // somebody picked it for a joke last Tuesday. Naming a voice stays a
        // decision a person makes on the Speakers page.
        h('span', { class: 'name', text: u.name ?? u.user_id }),
        h('span', { class: 'key', text: ` ${u.segments ?? 0} turn${u.segments === 1 ? '' : 's'}${u.via === 'truth' ? ' · linked automatically' : u.via === 'manual' ? ' · linked by you' : ''}` })
      ),
      h('span', { class: 'spacer' }),
      h('span', { class: 'sp-actions' }, select)
    );
    return row;
  }

  async function setTruthLink(u, value) {
    const before = { ...u };
    const speaker = value ? Number(value) : null;
    u.speaker = speaker;
    u.via = speaker == null ? null : 'manual';
    truthPending = u.user_id;
    renderTruth();
    try {
      const out = speaker == null ? await ask('truth.unlink', { user_id: u.user_id }) : await ask('truth.link', { user_id: u.user_id, speaker_id: speaker });
      Object.assign(u, out ?? {});
      toast(speaker == null ? `${before.name ?? before.user_id} unlinked.` : `${before.name ?? before.user_id} is ${out?.speaker_name ?? 'that voice'}.`, 'ok');
    } catch (e) {
      Object.assign(u, before);
      toast(`Could not link ${before.name ?? before.user_id} — ${e.message}`, 'error');
    } finally {
      truthPending = null;
      renderTruth();
      // The score moves when a link does: a linked user's turns are the only
      // ones identity is scored on.
      void ask('truth.summary')
        .then((s) => {
          truthSummary = s;
          renderTruth();
        })
        .catch(() => {});
    }
  }

  // -- the Markdown export (0.10.0) -----------------------------------------
  //
  // DESIGN §12: this writes files to your disk and nothing else. The card says
  // that sentence, and the shape of the card is the same claim — a folder you
  // pick in a native dialog, a preview of exactly which files, and one button.
  // There is nowhere else for the output to go, and no control that suggests
  // there might be.

  let exportDir = null;
  let exportPlan = null;
  let exportBusy = false;
  let exportOp = null;
  let exportProgress = null;
  let exportDone = null;
  let exportError = null;
  const exportRange = { from: '', to: '', translations: false };

  function renderExport() {
    // A status poll arrives every few seconds and repaints this page. Repainting
    // a card somebody is typing a date into would take the cursor away from
    // them, so a focused card is left exactly as it is — everything it shows is
    // already in the closure and nothing has changed underneath it.
    if (exportCard.contains(document.activeElement) && !exportBusy) return;
    clear(exportCard);
    const canRun = !!exportDir && !exportBusy && store.conn.status === 'connected';

    exportCard.append(
      h(
        'div',
        { class: 'sheet-head' },
        h('span', { class: 'card-title', text: 'Export to Markdown' }),
        h('span', { class: 'spacer' }),
        h('button', {
          class: 'btn small',
          id: 'export-choose',
          text: exportDir ? 'Change folder…' : 'Choose folder…',
          disabled: exportBusy,
          onclick: () => void chooseFolder(),
        })
      ),
      // The sentence, verbatim, and unmissable.
      h('p', {
        class: 'mic-warn',
        id: 'export-note',
        text: 'This writes files to your disk and nothing else. One Markdown file per day, plus people.md, in the folder you pick — there is no upload, no sharing and no link. Moving them anywhere afterwards is your own doing.',
      }),
      h('p', {
        class: 'rail-hint',
        id: 'export-dir',
        style: 'padding:0 0 10px;max-width:64ch',
        text: exportDir ? exportDir : 'No folder chosen yet.',
      })
    );

    const dateInput = (key, label) =>
      h(
        'div',
        { class: 'cap-row-ctl' },
        h('span', { class: 'cap-label' }, h('b', { text: label })),
        h('input', {
          class: 'cap-range',
          type: 'date',
          id: `export-${key}`,
          value: exportRange[key],
          'aria-label': label,
          oninput: (e) => {
            exportRange[key] = e.target.value;
            // The plan is about a range; changing the range invalidates it
            // rather than silently describing a different export.
            exportPlan = null;
            renderExport();
          },
        })
      );

    exportCard.append(
      dateInput('from', 'From'),
      dateInput('to', 'To (up to, not including)'),
      h(
        'div',
        { class: 'cap-row-ctl' },
        h(
          'span',
          { class: 'cap-label' },
          h('b', { text: 'Include translations' }),
          h('small', { text: 'Written under each turn that has one, as a quote.' })
        ),
        h('span', { class: 'spacer' }),
        h('button', {
          class: 'toggle',
          role: 'switch',
          id: 'export-translations',
          'aria-pressed': String(exportRange.translations),
          'aria-label': 'Include translations',
          onclick: () => {
            exportRange.translations = !exportRange.translations;
            exportPlan = null;
            renderExport();
          },
        })
      ),
      h(
        'div',
        { class: 'sheet-head', style: 'padding-top:10px' },
        h('button', {
          class: 'btn small',
          id: 'export-preview',
          text: 'Preview',
          disabled: !canRun,
          onclick: () => void preview(),
        }),
        h('button', {
          class: 'btn small primary',
          id: 'export-run',
          text: exportBusy ? 'Exporting…' : 'Export',
          disabled: !canRun || !exportPlan || !!exportPlan.blocked?.length || !exportPlan.files?.length,
          onclick: () => void run(),
        }),
        h('span', { class: 'spacer' }),
        exportDone
          ? h('button', {
              class: 'btn small',
              id: 'export-open',
              text: 'Open folder',
              onclick: () => void window.recall.exportFolder.open(exportDone.dir),
            })
          : h('span', {})
      )
    );

    if (exportError) {
      exportCard.append(h('p', { class: 'mic-warn', id: 'export-error', text: exportError }));
    }
    if (exportProgress) {
      exportCard.append(
        h('p', {
          class: 'rail-hint',
          id: 'export-progress',
          style: 'padding:8px 0 0',
          text: `Writing ${exportProgress.done} of ${exportProgress.total}…`,
        })
      );
    }
    if (exportDone) {
      exportCard.append(
        h('p', {
          class: 'rail-hint',
          id: 'export-done',
          style: 'padding:8px 0 0',
          text: `Wrote ${exportDone.files} file${exportDone.files === 1 ? '' : 's'} (${fmtBytes(exportDone.bytes ?? 0)}) to ${exportDone.dir}.`,
        })
      );
    }
    if (exportPlan) {
      exportCard.append(
        h('p', {
          class: 'rail-hint',
          id: 'export-counts',
          style: 'padding:8px 0 4px;max-width:64ch',
          text: exportPlan.files.length
            ? `${exportPlan.days} day${exportPlan.days === 1 ? '' : 's'} · ${exportPlan.conversations} conversation${exportPlan.conversations === 1 ? '' : 's'} · ${exportPlan.turns} turn${exportPlan.turns === 1 ? '' : 's'} · ${fmtBytes(exportPlan.bytes)} over ${exportPlan.files.length} file${exportPlan.files.length === 1 ? '' : 's'}`
            : 'Nothing to export in that range.',
        })
      );
      const files = h('div', { class: 'storage-rows', id: 'export-files' });
      for (const f of exportPlan.files) {
        files.append(
          h(
            'div',
            { class: 'storage-row', dataset: { exportFile: f.name } },
            h(
              'span',
              { class: 'storage-name' },
              h('b', { text: f.name }),
              h('small', {
                text: f.blocked
                  ? 'Already there and not written by NX Recall — this file will not be touched, and the export refuses while it is in the way.'
                  : f.exists
                    ? 'Replaces an earlier export of the same day.'
                    : 'New file.',
              })
            ),
            h('span', { class: 'storage-bytes', text: fmtBytes(f.bytes) })
          )
        );
      }
      if (exportPlan.files.length) exportCard.append(files);
    }
  }

  /** ISO date (`2026-09-01`) → the instant the daemon reads as that local day. */
  function dayStart(value) {
    if (!value) return undefined;
    const d = new Date(`${value}T00:00:00`);
    return Number.isNaN(d.getTime()) ? undefined : d.toISOString();
  }

  function exportParams() {
    const params = { dir: exportDir, include_translations: exportRange.translations };
    const from = dayStart(exportRange.from);
    const to = dayStart(exportRange.to);
    if (from) params.from = from;
    if (to) params.to = to;
    return params;
  }

  async function chooseFolder() {
    const dir = await window.recall.exportFolder.choose();
    if (!dir) return;
    exportDir = dir;
    exportPlan = null;
    exportDone = null;
    exportError = null;
    renderExport();
    void preview();
  }

  async function preview() {
    exportBusy = true;
    exportError = null;
    renderExport();
    try {
      exportPlan = await ask('export.preview', exportParams());
      if (exportPlan.blocked?.length) {
        exportError = `${exportPlan.blocked.join(', ')} ${exportPlan.blocked.length === 1 ? 'is' : 'are'} already in that folder and ${exportPlan.blocked.length === 1 ? 'was' : 'were'} not written by NX Recall. Move ${exportPlan.blocked.length === 1 ? 'it' : 'them'}, or pick an empty folder — the export will not overwrite a file it did not write.`;
      }
    } catch (e) {
      exportPlan = null;
      exportError = e.message;
    } finally {
      exportBusy = false;
      renderExport();
    }
  }

  async function run() {
    exportBusy = true;
    exportError = null;
    exportDone = null;
    exportProgress = { done: 0, total: exportPlan?.files?.length ?? 0 };
    renderExport();
    try {
      const out = await ask('export.run', exportParams());
      exportOp = out.op;
      // The op finishes on the event stream; `update` below picks it up.
    } catch (e) {
      exportBusy = false;
      exportOp = null;
      exportProgress = null;
      exportError = e.message;
      toast(`Export refused — ${e.message}`, 'error');
      renderExport();
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

  // Not `withSurface(null)`: that reads `caps` for the fields it is keeping,
  // and `caps` is still in its temporal dead zone on this line.
  let caps = { ...normalizeCaptionSettings(null), surface: 'window', outputs: [] };

  /**
   * The settings, plus the two facts about them that are not settings: which
   * surface this desktop puts the captions on, and what screens there are.
   *
   * `normalizeCaptionSettings` drops what it does not know — which is exactly
   * what it is for, and exactly why these two have to be put back by hand. A
   * card that lost them on the first click would silently revert to describing
   * a caption bar this desktop is not running.
   */
  function withSurface(raw) {
    return {
      ...normalizeCaptionSettings(raw),
      surface: raw?.surface ?? caps?.surface ?? 'window',
      outputs: Array.isArray(raw?.outputs) ? raw.outputs : (caps?.outputs ?? []),
    };
  }
  let capsOpen = false;

  async function pushCaptions(patch) {
    caps = withSurface(await window.recall.captions.set(patch));
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

  /**
   * Which screen the bar is on. The value is the connector name the compositor
   * uses (`DP-2`), because that is what survives a reboot — an index into a
   * list does not.
   */
  function capScreen() {
    const select = h(
      'select',
      {
        // `.input` is this app's select, the one the translation card uses.
        class: 'input',
        dataset: { cap: 'output' },
        'aria-label': 'Which screen the captions are on',
        onchange: (e) => pushCaptions({ output: e.target.value || null }),
      },
      // "Wherever it lands" is not the same as any named screen: it is the
      // rules the overlay follows when nobody has said (the screen the
      // remembered position is on, then the one at the desk's origin).
      h('option', { value: '', text: 'Wherever it lands', selected: !caps.output || undefined }),
      ...caps.outputs.map((o) =>
        h('option', {
          value: o.name,
          text: `${o.name} — ${o.w}×${o.h}`,
          selected: caps.output === o.name || undefined,
        })
      )
    );
    return h(
      'div',
      { class: 'cap-row-ctl' },
      h(
        'span',
        { class: 'cap-label' },
        h('b', { text: 'Screen' }),
        h('small', { text: 'You can also just drag the bar across — it follows onto whichever screen you carry it to.' })
      ),
      h('span', { class: 'spacer' }),
      select
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
      // 0.10.3. A layer surface belongs to one screen and cannot change it —
      // dragging the bar across a seam re-makes it on the other side, and this
      // is the same move without the dragging. Offered only where there is a
      // choice to make: one screen, or a desktop drawing the bar as a window,
      // and a selector with one entry is furniture.
      caps.surface === 'layer' && caps.outputs.length > 1 ? capScreen() : null,
      // Three desktops, three honest sentences. On the layer path the toggle is
      // real and does two different things — it is the difference between
      // scenery and furniture — so it says both. On Wayland WITHOUT layer-shell
      // it is measurably a no-op and says so rather than pretending. On X11 it
      // is the switch it always was.
      capToggle(
        'clickThrough',
        'Ignore the mouse',
        {
          layer:
            'On, clicks pass straight through to your game. Off, the bar stays put and you can drag it somewhere else, scroll over it to resize the text, and right-click it to turn this back on.',
          'window-wayland':
            'Not available on this desktop — the window cannot refuse a click here, so the bar takes them either way.',
          window: 'On, clicks land in whatever is underneath. Off, the bar can be dragged and resized.',
        }[caps.surface] ?? 'On, clicks land in whatever is underneath. Off, the bar can be dragged and resized.'
      )
    );
  }

  // -- the applications -----------------------------------------------------

  function render() {
    renderMic();
    renderRoom();
    renderTruth();
    renderExport();
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
  // 0.10.0. Three things this view owns that the boot queries do not fetch,
  // because nothing outside this page needs them: the room switch, the capture
  // devices it picks from, and the Discord bridge's state. Asked on mount, so
  // an older daemon that has none of them leaves the cards in their off state
  // rather than breaking the page.
  void (async () => {
    try {
      applyRoom(await ask('room.get'));
    } catch {
      /* a daemon older than the room microphone: the card stays off */
    }
    renderRoom();
    await loadDevices();
  })();
  void loadTruth();

  void (async () => {
    try {
      const st = await window.recall.captions.state();
      caps = withSurface(st?.settings);
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
      if (change?.room || change?.conn) renderRoom();
      if (change?.status) renderStorage();
      // ---- 0.10.0 --------------------------------------------------------
      // The export's op finishes on the event stream, like every other op.
      if (change?.ops && exportOp) {
        const live = store.ops.get(exportOp);
        if (live) {
          exportProgress = {
            done: Math.round((live.frac ?? 0) * (exportProgress?.total ?? 1)),
            total: exportProgress?.total ?? 1,
          };
          renderExport();
        }
      }
      if (change?.opFinished?.kind === 'export.run' && change.opFinished.op === exportOp) {
        const d = change.opFinished;
        exportBusy = false;
        exportOp = null;
        exportProgress = null;
        if (d.failed) {
          exportError = d.msg ?? 'the export failed';
          toast(`Export failed — ${exportError}`, 'error');
        } else {
          exportDone = { files: d.files, bytes: d.bytes, dir: d.dir };
          // The files are on disk now, so the plan describes what IS there.
          void preview();
          toast(`Exported ${d.files} file${d.files === 1 ? '' : 's'} to your disk.`, 'ok');
        }
        renderExport();
      }
      // A link made here or anywhere else, and the periodic status poll that
      // carries whether the plugin is still sending.
      if (change?.truth) void loadTruth();
      if (change?.status) renderTruth();
      if (change?.speakers || change?.relabel) renderTruth();
      // ---- end 0.10.0 ----------------------------------------------------
      if (change?.sources || change?.status) render();
      // A settings broadcast carries the whole block; the rail button's own
      // click carries only `true`, and then only the open/closed half moved.
      if (change?.captions) {
        if (typeof change.captions === 'object') caps = withSurface(change.captions);
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
