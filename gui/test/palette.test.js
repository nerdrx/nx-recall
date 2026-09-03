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
  accent,
  accentColor,
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

test("the daemon's palette and this one are the same list", () => {
  const rs = readFileSync(join(repo, 'crates/recalld/src/palette.rs'), 'utf8');
  // Accent { token: "violet", hue: 268, hex: "#7700ff" },
  const found = [...rs.matchAll(
    /token:\s*"([a-z]+)",\s*hue:\s*(\d+),\s*hex:\s*"(#[0-9a-f]{6})"/g,
  )].map((m) => ({ token: m[1], hue: Number(m[2]), hex: m[3] }));
  assert.ok(found.length > 0, 'found no palette entries in the daemon source');
  assert.deepEqual(found, PALETTE);
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
