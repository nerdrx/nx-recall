// Per-person highlights, renderer side (0.12.0).
//
// A person may pin one palette TOKEN and one short emoji to a voice, and both
// then follow that voice everywhere a name is drawn — the transcript, search,
// the person page, the digests, the captions bar, the overlay. The daemon owns
// the rules (crates/recalld/src/palette.rs: ten tokens, at most two grapheme
// clusters, no whitespace) and lib/palette.js owns the arithmetic. What lives
// here is the two things a VIEW needs and must not implement twice:
//
//   1. `look()` / `lookOf()` — what colour and what icon this row wears.
//   2. `highlightPicker()` — the control that changes them.
//
// Two mounting points ask for the picker (the person page's identity block and
// the transcript's segment sheet) and there is exactly one implementation,
// because a picker that existed twice would eventually send `{colour}` from one
// place and `{colour, icon}` from the other — and `speakers.set`'s whole
// contract is that an OMITTED key leaves that half alone while an explicit
// `null` clears it. Sending the half that did not change is how a person loses
// an emoji by picking a colour.
//
// The rule that runs through all of it: **an unhighlighted voice must look
// exactly as it did before this feature existed.** `speakerNameColor` already
// guarantees the colour is byte-identical to `speakerColor(id)` for a voice
// with no token; everything else here is guarded on "is there actually a
// highlight" so that no extra element, no extra colour and no extra pixel of
// accent appears on the 99% of voices nobody has marked.

import { h, clear, speakerColor } from '../lib/dom.js';
import { PALETTE, accentColor, iconOf, swatchColor } from '../lib/palette.js';
import { store, ask, speakerColour, speakerIcon, speakerLabel, speakerNameColor } from '../lib/store.js';
import { toast } from '../lib/sheets.js';

/**
 * The daemon's cap, mirrored (palette.rs MAX_ICON_CLUSTERS). Two, not one: a
 * flag is one cluster and so is a skin-toned wave, but "🌙✨" is a reasonable
 * mark for a person. Enforced here as well as there so the field simply cannot
 * be over-filled — an err:params round trip to say "that is three characters"
 * is a worse control than a field that stops accepting the third.
 */
export const MAX_ICON_CLUSTERS = 2;

/**
 * `maxlength` is in UTF-16 code units and a grapheme cluster is not: 👩‍🚀 is
 * three code points and six units, and a subdivision flag is far worse. So the
 * attribute is only a backstop against a paste of a paragraph, and the real cap
 * is `clampIcon` below. Twenty-four units comfortably holds two of anything the
 * daemon will accept.
 */
const ICON_MAXLENGTH = 24;

/** One segmenter for the app, because constructing one per keystroke is real work. */
const segmenter =
  typeof Intl !== 'undefined' && typeof Intl.Segmenter === 'function'
    ? new Intl.Segmenter(undefined, { granularity: 'grapheme' })
    : null;

/**
 * Trim a typed icon to at most `MAX_ICON_CLUSTERS` grapheme clusters.
 *
 * `Intl.Segmenter` is the correct tool and Chromium has had it since 87, so the
 * fallback exists only so this module cannot throw in a stripped test
 * environment: it counts code POINTS, which over-counts a ZWJ emoji and would
 * therefore refuse a legitimate one. That is why it is a fallback and not the
 * implementation — the daemon's own counter errs the other way on purpose.
 */
export function clampIcon(raw) {
  const s = String(raw ?? '').trim();
  if (!s) return '';
  if (!segmenter) return [...s].slice(0, MAX_ICON_CLUSTERS).join('');
  const clusters = [...segmenter.segment(s)].map((g) => g.segment);
  return clusters.slice(0, MAX_ICON_CLUSTERS).join('');
}

// ---------------------------------------------------------------------------
// what a row wears
// ---------------------------------------------------------------------------

/**
 * The three facts a name surface needs about a voice.
 *
 * - `color` — what to paint a name that is ALREADY coloured today (the
 *   transcript's `.nm`, a search hit, the captions bar). Byte-identical to
 *   `speakerColor(id)` when there is no highlight, which is the compatibility
 *   claim of the whole feature.
 * - `hl` — the highlight colour, or `null`. What to paint a name that is
 *   plain ink today (the speakers list, the person heading, a commitment).
 *   Applying `color` to those would put the hashed identity hue on surfaces
 *   that never had it, which is a redesign and not this feature.
 * - `icon` — the emoji, or `''`. Never rendered as an empty element: a voice
 *   with no icon must produce the same DOM it produced yesterday.
 */
export function lookOf(id) {
  return { color: speakerNameColor(id), hl: accentColor(speakerColour(id)), icon: speakerIcon(id) };
}

/**
 * The same, for a row that CARRIES its own `speaker_colour` / `speaker_icon`.
 *
 * Segment rows (transcript, search hits, replay turns) ship the highlight next
 * to `speaker_name` so a client can paint a voice it has never listed. The
 * store still wins where it knows the voice: it is the live copy, kept current
 * by `relabel`, while the row is a snapshot of whenever the query ran — a
 * search result fetched before a colour was picked would otherwise repaint the
 * transcript's rows and not its own.
 */
export function look(seg) {
  const id = seg?.speaker ?? null;
  if (id != null && store.speakers.has(id)) return lookOf(id);
  const token = seg?.speaker_colour ?? null;
  const hl = accentColor(token);
  return { color: hl || speakerColor(id), hl, icon: iconOf({ icon: seg?.speaker_icon }) };
}

/**
 * The same again, for a GRAPH row that carries `colour`/`icon` directly.
 *
 * The person page's edges and thread participants, a digest's participants, a
 * world's people, a commitment's `who` and `to` and the prune preview all ship
 * the pair under those two plain names (PROTOCOL v15) rather than under the
 * `speaker_*` prefix a segment uses, because on those rows the speaker is the
 * subject and not a column. Same precedence as `look`: the store wins where it
 * knows the voice, the row answers where it does not.
 */
export function lookOn(row, id = row?.speaker_id) {
  if (id != null && store.speakers.has(id)) return lookOf(id);
  const hl = accentColor(row?.colour ?? null);
  return { color: hl || speakerColor(id ?? null), hl, icon: iconOf(row) };
}

/**
 * The icon element, or `null` when there is none.
 *
 * `aria-hidden`, because it is a decoration on a name that is right next to it:
 * a screen reader announcing "crescent moon Kira" is reading out a colour-code,
 * not a name. Returning `null` rather than an empty span is load-bearing —
 * `h()` skips null children, so an unhighlighted voice gets exactly the DOM and
 * exactly the flex gaps it had before this feature.
 */
export function iconSpan(icon, cls = 'sp-icon') {
  return icon ? h('span', { class: cls, 'aria-hidden': 'true', text: icon }) : null;
}

/**
 * Mark a ROW (a transcript row, a search hit) with its speaker's highlight, as
 * a class plus an inline custom property.
 *
 * Two reasons it is a variable and not a colour in a rule. A theme flip has to
 * repaint it without re-rendering a single row, which is the same argument
 * `speakerColor` makes in lib/dom.js — `--person-hl` holds an `hsl()` whose
 * saturation and lightness are still `var(--sp-s)`/`var(--sp-l)`. And the
 * accent is drawn by ONE stylesheet rule, so its weight is decided in
 * styles.css rather than being spread across two views that would drift.
 *
 * The name is `person-hl` and not `hl` on purpose: styles.css already spends
 * `.seg .txt .hl` on the SEARCH-TERM match inside a row's words, one element
 * away from this, and two unrelated meanings of "hl" on the same row is a
 * merge waiting to happen.
 *
 * `uncertain` rows are deliberately left alone: styles.css already overrides
 * the inline speaker colour on those, because a row whose speaker is a guess
 * must not read as a confident anything — least of all as a person's chosen
 * mark. See the comment on `.seg.uncertain` in styles.css.
 */
export function markRow(row, hl, { uncertain = false } = {}) {
  if (!hl || uncertain) return row;
  row.classList.add('person-hl');
  row.style.setProperty('--person-hl', hl);
  return row;
}

// ---------------------------------------------------------------------------
// repainting on `relabel`
// ---------------------------------------------------------------------------

/**
 * Every picker currently on screen, so a highlight set in another client (or in
 * the CLI) moves the swatch under your cursor. Pruned on every pass rather than
 * on unmount: sheets and views are torn out of the DOM without telling anybody,
 * exactly as the playback subscriptions in speakers.js and transcript.js
 * discovered, and `isConnected` is the only honest test.
 */
const pickers = new Set();

/**
 * The half of a `relabel` that lib/labels.js does not know about.
 *
 * `patchSpeakerLabels` has repainted names and dots in place since 0.4 and it
 * paints `speakerColor(id)` — the hashed hue. Left alone, picking a colour
 * would light up every name and then have the very next broadcast wash the dots
 * back to the old hue, which is the "set but not shown" failure this feature
 * cannot afford. So the controller calls this straight after it, and it repaints
 * the three things a highlight owns: the dot, the name, and the icon.
 *
 * It works off `[data-sp="<id>"]`, the same tag `patchSpeakerLabels` uses, which
 * is why every surface that draws a name carries it.
 */
export function paintHighlights(ids) {
  for (const picker of [...pickers]) {
    if (!picker.el.isConnected) pickers.delete(picker);
    else if (ids == null || ids.includes(picker.spId)) picker.paint();
  }
  if (!ids) return;
  for (const id of ids) {
    if (id == null) continue;
    const { color, hl, icon } = lookOf(id);
    for (const el of document.querySelectorAll(`[data-sp="${id}"]`)) {
      const dot = el.matches('.dot') ? el : el.querySelector('.dot');
      if (dot) dot.style.color = color;
      // `.nm` is a surface that is coloured whatever happens (transcript,
      // search); `.sp-name` is one that is plain ink until somebody picks a
      // colour. Painting the second unconditionally would put the identity hue
      // on the Speakers list, which is a look this app has never had.
      const nm = el.matches('.nm') ? el : el.querySelector('.nm');
      if (nm && !nm.classList.contains('reasoned')) nm.style.color = color;
      const spName = el.matches('.sp-name') ? el : el.querySelector('.sp-name');
      if (spName) spName.style.color = hl || '';
      syncIcon(el, icon);
      const row = el.closest('.seg');
      if (row) {
        // A guess is still a guess. Same rule as `markRow`.
        const on = !!hl && !row.classList.contains('uncertain');
        row.classList.toggle('person-hl', on);
        if (on) row.style.setProperty('--person-hl', hl);
        else row.style.removeProperty('--person-hl');
      }
    }
  }
}

/**
 * Add, update or remove the icon inside one name container, in place.
 *
 * Inserted before the name rather than appended, because that is where every
 * view renders it and a relabel must not be able to move it to the other side.
 */
function syncIcon(el, icon) {
  const have = el.querySelector(':scope > .sp-icon');
  if (!icon) {
    have?.remove();
    return;
  }
  if (have) {
    have.textContent = icon;
    return;
  }
  const span = iconSpan(icon);
  const name = el.querySelector(':scope > .nm, :scope > .sp-name');
  // A container whose name is nested (the Speakers row wraps its name in an
  // unclassed span) gets the icon in front of whatever comes first, which is
  // still in front of the name.
  el.insertBefore(span, name ?? el.firstChild?.nextSibling ?? null);
}

// ---------------------------------------------------------------------------
// the picker
// ---------------------------------------------------------------------------

/** "violet" → "Violet". The token is an identifier; a label is for a person. */
const colourName = (token) => token.charAt(0).toUpperCase() + token.slice(1);

/**
 * The one highlight control: ten swatches, a "none", and a short emoji field.
 *
 * Mounted on the person page (views/person.js `renderHeader`) and in the
 * transcript's segment sheet (views/transcript.js `openSegmentSheet`). In the
 * sheet it is deliberately NOT tied to the naming input's own hidden rule: that
 * field only appears for a voice with no name, and a highlight is settable for
 * anybody — the whole point of one is to find a person you already know.
 *
 * It writes NOTHING to the store. `speakers.set` is answered by a `relabel`
 * broadcast and the entire UI, this control included, repaints off that one
 * path — the same discipline `startRename` in views/speakers.js keeps, and the
 * reason a highlight set from the CLI looks identical to one set here.
 *
 * The palette comes from the local `PALETTE` constant rather than from
 * `speakers.palette`, so the swatches are on screen in the same frame the sheet
 * is. The RPC exists and a newer daemon may know an eleventh colour; the honest
 * cost of not asking is that this build cannot offer a colour it also could not
 * paint (lib/palette.js `accent` returns null for an unknown token), so asking
 * would buy nothing but a flash of empty row.
 */
export function highlightPicker(spId, { label = 'Highlight' } = {}) {
  const swatches = h('div', {
    class: 'hl-swatches',
    role: 'group',
    'aria-label': `Highlight colour for ${speakerLabel(spId)}`,
  });
  const icon = h('input', {
    class: 'input hl-icon',
    type: 'text',
    dataset: { hlIcon: String(spId) },
    placeholder: '🌙',
    'aria-label': `Emoji for ${speakerLabel(spId)}`,
    // Escape in here reverts the field and keeps the sheet open, the same
    // meaning it has in the naming input next to it (lib/sheets.js).
    'data-keep-escape': '',
    maxlength: String(ICON_MAXLENGTH),
    // 12 characters of room for two clusters, so a wide flag is not clipped.
    style: 'width:64px',
  });
  const iconClear = h(
    'button',
    {
      class: 'btn small hl-clear',
      type: 'button',
      dataset: { hlIconClear: String(spId) },
      title: 'Remove the emoji',
      'aria-label': `Remove ${speakerLabel(spId)}'s emoji`,
      onclick: () => void send({ icon: null }),
    },
    '✕'
  );

  const el = h(
    'div',
    { class: 'hl-picker', dataset: { highlight: String(spId) } },
    h('span', { class: 'hl-label', text: label }),
    swatches,
    icon,
    iconClear
  );

  /**
   * One request, one half — or both, when a person types an emoji into a voice
   * that has no colour and we would otherwise be guessing what they meant. The
   * caller passes only what it is changing, and the omitted key leaves the
   * other half exactly as it is (PROTOCOL `speakers.set`).
   */
  async function send(patch) {
    try {
      await ask('speakers.set', { id: spId, ...patch });
      // No optimistic write and no local paint: the daemon broadcasts
      // `relabel` carrying both halves, `paintHighlights` repaints every
      // surface, and this control is one of them.
    } catch (e) {
      // The daemon's own words. It is the thing that knows the palette and the
      // cluster count, and "colour must be one of the palette tokens (...)" is
      // a better sentence than any this file could invent.
      toast(`Could not set the highlight — ${e.message}`, 'error');
    }
  }

  function paint() {
    const token = speakerColour(spId);
    clear(swatches);
    swatches.append(
      h(
        'button',
        {
          class: `hl-sw hl-none${token ? '' : ' on'}`,
          type: 'button',
          dataset: { hlColour: 'none' },
          'aria-label': 'No colour',
          'aria-pressed': String(!token),
          title: 'No colour — this voice keeps the hue it was given automatically',
          onclick: () => {
            if (token) void send({ colour: null });
          },
        },
        '✕'
      )
    );
    for (const a of PALETTE) {
      const on = token === a.token;
      swatches.append(
        h('button', {
          class: `hl-sw${on ? ' on' : ''}`,
          type: 'button',
          dataset: { hlColour: a.token },
          'aria-label': colourName(a.token),
          'aria-pressed': String(on),
          title: colourName(a.token),
          // The swatch shows the colour the ROW will wear on this ground, not
          // the canonical hex — a swatch that promised #7700ff and then painted
          // a light-theme name at 28% lightness would be promising the wrong
          // thing (lib/palette.js `swatchColor`).
          style: `--person-hl:${swatchColor(a.token)}`,
          onclick: () => {
            if (!on) void send({ colour: a.token });
          },
        })
      );
    }
    // Only when the field is not being typed in: a repaint mid-keystroke that
    // put the stored value back would fight the person holding the keyboard.
    if (document.activeElement !== icon) icon.value = speakerIcon(spId);
    iconClear.disabled = !speakerIcon(spId);
  }

  /**
   * Enter and blur both commit, and Enter blurs — so without a guard the same
   * icon is sent twice, which is the bug `startRename` in views/speakers.js
   * carries its `settled` flag for. Here the flag is the value rather than a
   * boolean, because the field survives the round trip and a person may well
   * type a second emoji straight after the first. It clears itself the moment
   * the `relabel` lands and the store agrees with what is on screen.
   */
  let sent = null;
  const commit = () => {
    // A blur caused by the field being taken OUT of the document is not a
    // person finishing a thought. The person page re-renders its header every
    // couple of seconds while the feed moves (views/person.js, finding #18),
    // and without this a half-typed emoji would be committed by the refresh
    // that interrupted it. The element survives the re-render with its value
    // intact, so nothing is lost by declining here.
    if (!icon.isConnected) return;
    const next = clampIcon(icon.value);
    icon.value = next;
    if (next === speakerIcon(spId)) {
      sent = null;
      return;
    }
    if (next === sent) return;
    sent = next;
    void send({ icon: next === '' ? null : next });
  };
  icon.addEventListener('input', () => {
    // Clamped as it is typed rather than refused on save: the cap is two
    // clusters and the field is 64px wide, so the limit is already visible.
    const next = clampIcon(icon.value);
    if (next !== icon.value) icon.value = next;
  });
  icon.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') {
      e.preventDefault();
      e.stopPropagation();
      icon.value = speakerIcon(spId);
      return;
    }
    if (e.key === 'Enter') {
      e.preventDefault();
      commit();
    }
  });
  icon.addEventListener('blur', commit);

  paint();
  // Pruned on the way in as well as in `paintHighlights`: the person page
  // rebuilds its header on every `person.get` (every two seconds while the feed
  // moves) and a set that only ever grew would hold a morning of dead headers
  // alive. Nothing else in this app has an unmount hook to do it properly.
  for (const p of [...pickers]) if (!p.el.isConnected) pickers.delete(p);
  pickers.add({ spId, el, paint });
  el.refresh = paint;
  return el;
}
