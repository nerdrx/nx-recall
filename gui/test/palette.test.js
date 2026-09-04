// The palette exists three times — once in the daemon, once in the headset
// overlay, once here — because neither Rust binary can import this file and
// this file cannot import either of them. Three copies of ten numbers is fine;
// three copies that have DRIFTED is a person whose highlight is one colour in
// the app and another in the headset, which is two identities.
//
// So the copies are not trusted, they are checked: this reads both Rust files
// and asserts the list matches, token for token and hue for hue.

import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

import {
  PALETTE,
  MOOD_PALETTE,
  accent,
  accentColor,
  eventsOf,
  moodColor,
  nameColor,
  iconOf,
  isAccent,
} from '../src/renderer/lib/palette.js';

const here = dirname(fileURLToPath(import.meta.url));
const repo = join(here, '..', '..');

test('the palette is ten distinct tokens and carries the brand colour', () => {
  assert.equal(PALETTE.length, 10);
  assert.equal(new Set(PALETTE.map((a) => a.token)).size, 10);
  assert.equal(accent('violet').hex, '#7700ff');
  for (const a of PALETTE) {
    assert.ok(a.hue >= 0 && a.hue < 360, `${a.token} is not a hue`);
    assert.match(a.hex, /^#[0-9a-f]{6}$/, `${a.token} needs a swatch`);
  }
});

// Both palettes are written as `Accent { … }` tables, so one regex over the
// whole file finds thirteen entries and matches neither list. Cut the source at
// the second table's name first — a reader tracing a drift has to be told WHICH
// list drifted, and a combined assertion cannot say.
function accentsIn(rs) {
  // Accent { token: "violet", hue: 268, hex: "#7700ff" },
  return [...rs.matchAll(
    /token:\s*"([a-z]+)",\s*hue:\s*(\d+),\s*hex:\s*"(#[0-9a-f]{6})"/g,
  )].map((m) => ({ token: m[1], hue: Number(m[2]), hex: m[3] }));
}

function daemonPalettes() {
  const rs = readFileSync(join(repo, 'crates/recalld/src/palette.rs'), 'utf8');
  const at = rs.indexOf('MOOD_PALETTE: &[Accent]');
  assert.ok(at > 0, 'the daemon source has no MOOD_PALETTE table');
  return { people: accentsIn(rs.slice(0, at)), moods: accentsIn(rs.slice(at)) };
}

test("the daemon's palette and this one are the same list", () => {
  const found = daemonPalettes().people;
  assert.ok(found.length > 0, 'found no palette entries in the daemon source');
  assert.deepEqual(found, PALETTE);
});

// 0.12.4. The same drift check for the mood tint, and one more assertion the
// person palette does not need: the three mood hues must BE three of the ten,
// because the whole design argument for them is that the suite turns one wheel.
test("the mood palette is the daemon's, and its hues are three of the ten", () => {
  const found = daemonPalettes().moods;
  assert.equal(found.length, 3);
  assert.deepEqual(found, MOOD_PALETTE);
  for (const m of MOOD_PALETTE) {
    assert.ok(
      PALETTE.some((a) => a.hue === m.hue),
      `${m.token} is a hue the person palette does not have`,
    );
  }
});

test('a mood is painted through the ground, and neutral is not painted at all', () => {
  // The same legibility argument as a highlight, at the tint's own saturation
  // and lightness — which is what makes a whole SENTENCE in it readable where
  // --sp-s would be a highlighter pen.
  assert.equal(moodColor('happy'), 'hsl(44 var(--mood-s) var(--mood-l))');
  assert.equal(moodColor('sad'), 'hsl(232 var(--mood-s) var(--mood-l))');
  assert.equal(moodColor('angry'), 'hsl(350 var(--mood-s) var(--mood-l))');
  // A mood the daemon stores and no surface tints. Not an oversight — a
  // transcript already looks like neutral, and painting it would repaint the
  // whole archive to say nothing.
  assert.equal(moodColor('neutral'), null);
  // …and the ordinary absences, which must all fall through to the row's ink.
  assert.equal(moodColor(null), null);
  assert.equal(moodColor(undefined), null);
  assert.equal(moodColor(''), null);
  // A fifth mood from a newer daemon is not painted wrong, it is not painted.
  assert.equal(moodColor('ecstatic'), null);
});

test('the events on a row are the closed set, in one order', () => {
  assert.deepEqual(eventsOf({ events: ['laughter'] }), ['laughter']);
  // Sorted into the palette's own order, so two rows with the same events read
  // the same however the daemon happened to list them.
  assert.deepEqual(eventsOf({ events: ['music', 'laughter'] }), ['laughter', 'music']);
  assert.deepEqual(eventsOf({ events: ['cry', 'applause'] }), ['applause', 'cry']);
  // An event this build has no word for is dropped, and the rest still render.
  assert.deepEqual(eventsOf({ events: ['laughter', 'sneeze'] }), ['laughter']);
  assert.deepEqual(eventsOf({ events: ['sneeze'] }), []);
  // The ordinary row, and every shape of absence a daemon or a mock can send.
  assert.deepEqual(eventsOf({ events: [] }), []);
  assert.deepEqual(eventsOf({}), []);
  assert.deepEqual(eventsOf(null), []);
  assert.deepEqual(eventsOf({ events: null }), []);
});

test("the overlay's palette and this one are the same hues, in order", () => {
  const rs = readFileSync(
    join(repo, 'crates/nx-recall-overlay/src/palette.rs'),
    'utf8',
  );
  // ("violet", 268.0),
  const found = [...rs.matchAll(/\("([a-z]+)",\s*(\d+)\.0\)/g)].map((m) => ({
    token: m[1],
    hue: Number(m[2]),
  }));
  assert.ok(found.length > 0, 'found no palette entries in the overlay source');
  assert.deepEqual(found, PALETTE.map((a) => ({ token: a.token, hue: a.hue })));
});

test('a highlight is painted through the ground, never as a literal', () => {
  // The whole legibility argument: the colour a row wears is the token's hue at
  // the THEME's saturation and lightness, so a highlight cannot make a name
  // less readable than the automatic colour it replaced.
  assert.equal(accentColor('violet'), 'hsl(268 var(--sp-s) var(--sp-l))');
  assert.equal(accentColor('rose'), 'hsl(350 var(--sp-s) var(--sp-l))');
  assert.equal(accentColor(null), null);
  assert.equal(accentColor(undefined), null);
  // A colour from a newer daemon is not painted wrong, it is not painted.
  assert.equal(accentColor('chartreuse'), null);
  assert.equal(isAccent('chartreuse'), false);
  assert.equal(isAccent('teal'), true);
});

test('an unhighlighted voice keeps exactly the colour it has today', () => {
  const fallback = 'hsl(211 var(--sp-s) var(--sp-l))';
  assert.equal(nameColor({ id: 3 }, fallback), fallback);
  assert.equal(nameColor({ id: 3, colour: null }, fallback), fallback);
  assert.equal(nameColor(null, fallback), fallback);
  // An unknown token also falls back — a name in its old colour beats a name
  // in no colour.
  assert.equal(nameColor({ colour: 'chartreuse' }, fallback), fallback);
  // And a highlight wins.
  assert.equal(
    nameColor({ colour: 'amber' }, fallback),
    'hsl(44 var(--sp-s) var(--sp-l))',
  );
});

test('the icon is a trimmed string or nothing at all', () => {
  assert.equal(iconOf({ icon: '\u{1F319}' }), '\u{1F319}');
  assert.equal(iconOf({ icon: '  \u{1F319} ' }), '\u{1F319}');
  assert.equal(iconOf({ icon: null }), '');
  assert.equal(iconOf({ icon: '   ' }), '');
  assert.equal(iconOf({}), '');
  assert.equal(iconOf(null), '');
});
