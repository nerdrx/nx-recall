// Live captions — the rules, with no DOM and no Electron in them.
//
// Both halves of the feature import this file: the main process, which owns the
// settings file and has no renderer to ask, and the captions window, which owns
// the rows and has no disk. Keeping the arithmetic here is what lets the unit
// suite check the two things that are easy to get wrong and impossible to see
// in a screenshot — that a setting arriving from a file cannot put the window
// into a state no control can produce, and that a row's fade is a function of
// the clock rather than of whichever repaint happened to run.

/**
 * What the captions window is, out of the box.
 *
 * `clickThrough` defaults to ON because the window is always on top of whatever
 * you are actually doing: a caption bar that eats a click into the game is a
 * worse bug than one you have to turn off before you can drag it.
 *
 * `showYou` defaults to ON but dimmed (see `YOU_DIM`): your own turns are the
 * ones you already know, and the reason to keep them is rhythm — a caption
 * stack that skips half the conversation reads as dropped words.
 */
export const CAPTION_DEFAULTS = Object.freeze({
  turns: 5,
  size: 26,
  hold_s: 12,
  opacity: 0.6,
  showYou: true,
  clickThrough: true,
  bounds: null,
});

/**
 * The bounds every numeric setting is clamped to, and the bounds the controls
 * are built from — one source, so a slider can never offer a value the loader
 * would quietly rewrite.
 */
export const CAPTION_RANGES = Object.freeze({
  turns: { min: 3, max: 8, step: 1 },
  size: { min: 18, max: 40, step: 1 },
  hold_s: { min: 4, max: 60, step: 1 },
  opacity: { min: 0.3, max: 0.9, step: 0.05 },
});

/** How much dimmer a "You" row is than everybody else's. */
export const YOU_DIM = 0.55;

const clamp = (v, { min, max, step }, dflt) => {
  // `null` and `''` are ABSENT, not zero. Number() reads both as 0, which is a
  // finite number inside no range this file has — so a field an older build
  // wrote as null would silently become the minimum rather than the default.
  if (v == null || v === '') return dflt;
  const n = Number(v);
  if (!Number.isFinite(n)) return dflt;
  const stepped = Math.round(n / step) * step;
  // Rounded back to the step's own precision: 0.30000000000000004 is the same
  // opacity as 0.3 and a different string in every place that renders it.
  const places = String(step).includes('.') ? String(step).split('.')[1].length : 0;
  return Number(Math.min(max, Math.max(min, stepped)).toFixed(places));
};

/**
 * A settings block from anywhere — a JSON file written by an older build, a
 * renderer that sent one field, a hand-edited file — folded onto the defaults.
 *
 * Nothing here trusts its input: the file lives in the user's profile and a
 * `size` of 4000 in it would produce a window with one unreadable word in it
 * and no control able to get back out.
 */
export function normalizeCaptionSettings(raw) {
  const src = raw && typeof raw === 'object' ? raw : {};
  return {
    turns: clamp(src.turns, CAPTION_RANGES.turns, CAPTION_DEFAULTS.turns),
    size: clamp(src.size, CAPTION_RANGES.size, CAPTION_DEFAULTS.size),
    hold_s: clamp(src.hold_s, CAPTION_RANGES.hold_s, CAPTION_DEFAULTS.hold_s),
    opacity: clamp(src.opacity, CAPTION_RANGES.opacity, CAPTION_DEFAULTS.opacity),
    showYou: src.showYou === undefined ? CAPTION_DEFAULTS.showYou : !!src.showYou,
    clickThrough: src.clickThrough === undefined ? CAPTION_DEFAULTS.clickThrough : !!src.clickThrough,
    bounds: normalizeBounds(src.bounds),
  };
}

/**
 * A remembered window position, or null.
 *
 * Deliberately not validated against the current display layout: a monitor that
 * is unplugged today may be back tomorrow, and Electron already refuses to
 * place a window entirely off every screen. What IS refused is a size no
 * caption bar could use, because that is a corrupt file rather than a monitor.
 */
function normalizeBounds(b) {
  if (!b || typeof b !== 'object') return null;
  const n = (v) => (Number.isFinite(Number(v)) ? Math.round(Number(v)) : null);
  const x = n(b.x);
  const y = n(b.y);
  const width = n(b.width);
  const height = n(b.height);
  if (x == null || y == null || width == null || height == null) return null;
  if (width < 240 || height < 90) return null;
  return { x, y, width, height };
}

/**
 * The `translation` sibling track (contract below), or null.
 *
 * Absent is the ordinary case and means exactly nothing — a turn in a language
 * the user reads needs no second line, and inventing an empty one would put a
 * blank row under half the captions. A block that is present but has no `text`
 * is treated as absent for the same reason.
 *
 * Contract, so the sibling build and this one cannot drift:
 *
 *   segment.translation = { lang: "en", text: "…", via: "…" } | absent
 *
 * `lang` is a BCP-47-ish short tag, the language the TEXT is in (the target,
 * not the source — the source is the segment's own `lang`). `via` names what
 * produced it and is never rendered here; it is provenance for the sheet.
 */
export function translationOf(seg) {
  const t = seg?.translation;
  if (!t || typeof t !== 'object') return null;
  const text = typeof t.text === 'string' ? t.text.trim() : '';
  if (!text) return null;
  return { lang: typeof t.lang === 'string' ? t.lang : null, text, via: typeof t.via === 'string' ? t.via : null };
}

/**
 * Which rows the captions window is showing right now, and how faded each one
 * is — the whole render model, as one pure function of (rows, settings, clock).
 *
 * The hold is per-STACK, not per-row: the whole caption bar is one thing you
 * glance at, and a stack where the top line has already vanished while the one
 * under it is still solid reads as a rendering bug. So the fade is measured
 * from the newest row's arrival, which is what "no new text for hold_s" means.
 *
 * `showYou: false` drops your own turns BEFORE the last-N window is taken, so
 * turning it off gives you five of THEIR turns rather than five turns of which
 * three are yours.
 */
export function visibleCaptions(rows, settings, now = Date.now(), isYou = () => false) {
  const s = normalizeCaptionSettings(settings);
  const kept = (rows ?? []).filter((r) => s.showYou || !isYou(r.seg));
  const last = kept.slice(-s.turns);
  if (!last.length) return { rows: [], fade: 0, hidden: kept.length === 0 };
  const newest = last[last.length - 1].at ?? 0;
  const age = Math.max(0, now - newest) / 1000;
  // One second of actual fading at the end of the hold, so a stack leaves
  // rather than blinks. Past hold_s + 1 it is gone and the window is empty.
  const fade = age <= s.hold_s ? 1 : Math.max(0, 1 - (age - s.hold_s));
  return {
    rows: last.map((r) => ({ ...r, dim: isYou(r.seg) ? YOU_DIM : 1 })),
    fade,
    hidden: fade <= 0,
  };
}
