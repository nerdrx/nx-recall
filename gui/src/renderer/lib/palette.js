// The highlight palette, desktop side.
//
// A mirror of crates/recalld/src/palette.rs — the same ten tokens, the same ten
// hues — and the third copy of that list (the second is the overlay's
// crates/nx-recall-overlay/src/palette.rs). gui/test/palette.test.js reads the
// daemon's file and fails if the three ever disagree, which is the only reason
// three copies are tolerable.
//
// Only the HUE lives here. Saturation and lightness come from --sp-s / --sp-l
// in tokens.css, exactly as speakerColor() in dom.js spends them: 72%/28% on
// the light ground and 72%/74% on the dark one, both measured to clear WCAG AA
// against every surface a name is painted on. That is the whole legibility
// argument for tokens over hex — a highlight changes WHICH hue a name wears and
// never how readable it is, and it repaints on a theme flip without re-
// rendering a row, like every other speaker colour in this app.

export const PALETTE = [
  { token: 'violet', hue: 268, hex: '#7700ff' },
  { token: 'indigo', hue: 232, hex: '#3355ee' },
  { token: 'cyan', hue: 192, hex: '#00a5c4' },
  { token: 'teal', hue: 168, hex: '#00a487' },
  { token: 'green', hue: 140, hex: '#1fa14e' },
  { token: 'lime', hue: 92, hex: '#5f9c1a' },
  { token: 'amber', hue: 44, hex: '#b8820a' },
  { token: 'orange', hue: 22, hex: '#cc6516' },
  { token: 'rose', hue: 350, hex: '#d6396b' },
  { token: 'magenta', hue: 312, hex: '#b83bc4' },
];

// The mood palette (0.12.4), mirroring MOOD_PALETTE in the daemon's
// palette.rs. Three hues, and they are three of the ten above: the suite turns
// one wheel rather than two.
//
// What differs is where the saturation and lightness come from. A highlight
// paints a NAME at --sp-s / --sp-l; a mood paints a SENTENCE, which is body
// text and cannot wear a 72% accent. So this spends --mood-s / --mood-l
// (46%/30% light, 46%/79% dark — tokens.css), measured against every ground a
// transcript row is painted on: worst case 5.61:1 light, 8.41:1 dark.
//
// `neutral` is a mood and is deliberately NOT here. It is what a transcript
// already looks like, and a colour for it would repaint the whole archive to
// say nothing. moodColor() returns null for it and the row keeps its ink,
// which is the same fall-through an unknown token gets.
export const MOOD_PALETTE = [
  { token: 'happy', hue: 44, hex: '#705d29' },
  { token: 'sad', hue: 232, hex: '#293370' },
  { token: 'angry', hue: 350, hex: '#702935' },
];

// The events a row may carry, as the daemon spells them, and the one word each
// gets on a chip. A closed set here as well as there: an event a newer daemon
// invents is dropped rather than rendered as a raw identifier.
export const EVENT_LABELS = {
  laughter: 'laughter',
  music: 'music',
  applause: 'applause',
  cry: 'crying',
};

const BY_MOOD = new Map(MOOD_PALETTE.map((a) => [a.token, a]));

// The tint for a mood, as a CSS colour in the ground's own saturation and
// lightness — or null for `neutral`, for a mood this build cannot paint, and
// for no mood at all. A null means "leave the ink alone", which is what an
// untinted row already is.
export function moodColor(token) {
  const a = token ? BY_MOOD.get(token) : null;
  return a ? `hsl(${a.hue} var(--mood-s) var(--mood-l))` : null;
}

// The events on a segment, filtered to the ones this build has a word for and
// in the palette's own order so two rows with the same events read the same.
export function eventsOf(seg) {
  const raw = Array.isArray(seg?.events) ? seg.events : [];
  return Object.keys(EVENT_LABELS).filter((e) => raw.includes(e));
}

const BY_TOKEN = new Map(PALETTE.map((a) => [a.token, a]));

// The entry for a token, or null. Unknown is not an error: a daemon newer than
// this GUI may name an eleventh colour, and the right answer then is to fall
// back to the voice's hashed hue rather than to paint nothing.
export function accent(token) {
  if (!token) return null;
  return BY_TOKEN.get(token) || null;
}

export function isAccent(token) {
  return accent(token) != null;
}

// A highlight token as a CSS colour, in the ground's own saturation and
// lightness. Returns null when there is no highlight, so callers can fall
// through to speakerColor(id) with a `??` and an unhighlighted voice keeps
// exactly the colour it has today.
export function accentColor(token) {
  const a = accent(token);
  return a ? `hsl(${a.hue} var(--sp-s) var(--sp-l))` : null;
}

// The colour for a swatch in the picker. The same hsl() as accentColor, because
// a swatch that showed the canonical hex would be promising a colour the row
// will not actually wear on this ground.
export function swatchColor(token) {
  return accentColor(token);
}

// The name colour for a speaker: their highlight if they have one, otherwise
// the hashed colour they have always had. One function, so no surface can
// accidentally implement half the rule.
//
// `speaker` is any object carrying `colour` — a speakers.list row, a transcript
// row's resolved speaker, a search hit's participant. `fallback` is the
// speakerColor(id) the caller would have used.
export function nameColor(speaker, fallback) {
  return accentColor(speaker && speaker.colour) || fallback;
}

// The icon, or ''. Trimmed, because the daemon stores NULL for an empty one and
// a client that rendered ' ' would open a gap before a name for no reason.
export function iconOf(speaker) {
  const icon = speaker && speaker.icon;
  return typeof icon === 'string' ? icon.trim() : '';
}
