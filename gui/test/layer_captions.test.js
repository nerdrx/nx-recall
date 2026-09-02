// Which surface the captions land on, and how the binary behind it is found.
//
// The two decisions in src/main/captions.js that are not about a window and are
// not visible in a screenshot:
//
//   1. `wantsLayerCaptions` — on Wayland the caption bar is a layer-shell
//      client, because Electron's `setIgnoreMouseEvents` is a measured no-op
//      there. The trap this guards is the e2e harness: it runs Electron inside a
//      headless gamescope that has no wlr-layer-shell AND inherits the
//      developer's own WAYLAND_DISPLAY, so without the exception a test run
//      would put a caption bar on the real desktop instead of in the compositor
//      under test.
//   2. `findOverlayBinary` — the install layout puts the binary beside the app
//      (`lib/nx-recall/nx-recall-overlay`, with the gui in `lib/nx-recall/gui`),
//      a checkout puts it in `target/`, and a `~/.local/bin` install puts it on
//      PATH. Getting the first wrong means a packaged app quietly falling back
//      to a window that cannot ignore a click.
//
// `src/main/captions.js` imports `electron`, which does not exist outside an
// Electron process, so the module is loaded behind a stub — see `withElectron`.

import test from 'node:test';
import assert from 'node:assert/strict';
import { registerHooks } from 'node:module';
import { mkdtempSync, mkdirSync, writeFileSync, chmodSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

// A stub `electron` with the two names main/captions.js imports. Nothing in
// these tests reaches either of them; they exist so the module can be loaded.
const STUB = `
export class BrowserWindow {}
export const screen = { getPrimaryDisplay: () => ({ workAreaSize: { width: 1920, height: 1080 } }) };
export default { BrowserWindow, screen };
`;
registerHooks({
  resolve(specifier, context, next) {
    if (specifier === 'electron') return { url: 'stub-electron:///', shortCircuit: true };
    return next(specifier, context);
  },
  load(url, context, next) {
    if (url === 'stub-electron:///') {
      return { format: 'module', source: STUB, shortCircuit: true };
    }
    return next(url, context);
  },
});

const { wantsLayerCaptions, findOverlayBinary } = await import('../src/main/captions.js');
const { normalizeCaptionSettings } = await import('../src/renderer/lib/captions.js');

/** Run `fn` with a patched `process.env`, restored afterwards. */
function withEnv(patch, fn) {
  const before = { ...process.env };
  for (const [k, v] of Object.entries(patch)) {
    if (v === undefined) delete process.env[k];
    else process.env[k] = v;
  }
  try {
    return fn();
  } finally {
    for (const k of Object.keys(process.env)) if (!(k in before)) delete process.env[k];
    for (const [k, v] of Object.entries(before)) process.env[k] = v;
  }
}

const clean = {
  NX_RECALL_E2E: undefined,
  NX_RECALL_NO_LAYER: undefined,
  XDG_SESSION_TYPE: undefined,
  WAYLAND_DISPLAY: undefined,
};

test('a Wayland session gets the layer surface, either way it can be spelt', (t) => {
  if (process.platform !== 'linux') return t.skip('the layer path is Linux only');
  withEnv({ ...clean, XDG_SESSION_TYPE: 'wayland' }, () => {
    assert.equal(wantsLayerCaptions(), true);
  });
  // A session where XDG_SESSION_TYPE was never exported but the socket is there.
  withEnv({ ...clean, WAYLAND_DISPLAY: 'wayland-0' }, () => {
    assert.equal(wantsLayerCaptions(), true);
  });
});

test('an X11 session keeps the window, where ignoring the mouse actually works', (t) => {
  if (process.platform !== 'linux') return t.skip('the layer path is Linux only');
  withEnv({ ...clean, XDG_SESSION_TYPE: 'x11' }, () => {
    assert.equal(wantsLayerCaptions(), false);
  });
});

// The headless suite runs Electron inside gamescope, which offers no
// wlr-layer-shell — and inherits the developer's own WAYLAND_DISPLAY. Without
// this exception a test run would open a caption bar on the real desktop.
test('the e2e harness never takes the layer path, whatever it inherited', (t) => {
  if (process.platform !== 'linux') return t.skip('the layer path is Linux only');
  for (const flag of ['1', '0']) {
    withEnv({ ...clean, XDG_SESSION_TYPE: 'wayland', WAYLAND_DISPLAY: 'wayland-0', NX_RECALL_E2E: flag }, () => {
      assert.equal(wantsLayerCaptions(), false, `NX_RECALL_E2E=${flag} still reached the layer path`);
    });
  }
});

test('NX_RECALL_NO_LAYER is the way back to the window', (t) => {
  if (process.platform !== 'linux') return t.skip('the layer path is Linux only');
  withEnv({ ...clean, XDG_SESSION_TYPE: 'wayland', NX_RECALL_NO_LAYER: '1' }, () => {
    assert.equal(wantsLayerCaptions(), false);
  });
});

// -- finding the binary -----------------------------------------------------

function executable(dir, name = 'nx-recall-overlay') {
  mkdirSync(dir, { recursive: true });
  const path = join(dir, name);
  writeFileSync(path, '#!/bin/sh\nexit 0\n');
  chmodSync(path, 0o755);
  return path;
}

test('the install layout is looked at first: beside the app, not on PATH', () => {
  const root = mkdtempSync(join(tmpdir(), 'nx-recall-layout-'));
  // packaging/build-release.sh: lib/nx-recall/gui is the app, lib/nx-recall/ has
  // the binary. ROOT is the gui directory.
  const app = join(root, 'lib', 'nx-recall', 'gui');
  mkdirSync(app, { recursive: true });
  const beside = executable(join(root, 'lib', 'nx-recall'));
  const onPath = join(root, 'bin');
  executable(onPath);
  assert.equal(findOverlayBinary({ PATH: onPath }, app), beside);
});

test('a checkout finds the one cargo built, release before debug', () => {
  const root = mkdtempSync(join(tmpdir(), 'nx-recall-checkout-'));
  const gui = join(root, 'gui');
  mkdirSync(gui, { recursive: true });
  const debug = executable(join(root, 'target', 'debug'));
  assert.equal(findOverlayBinary({ PATH: '' }, gui), debug);
  const release = executable(join(root, 'target', 'release'));
  assert.equal(findOverlayBinary({ PATH: '' }, gui), release, 'debug won over release');
});

test('PATH is the last answer, and no answer is null rather than a throw', () => {
  const root = mkdtempSync(join(tmpdir(), 'nx-recall-path-'));
  const gui = join(root, 'gui');
  mkdirSync(gui, { recursive: true });
  assert.equal(findOverlayBinary({ PATH: '' }, gui), null);
  const bin = join(root, 'elsewhere');
  const exe = executable(bin);
  assert.equal(findOverlayBinary({ PATH: `/nonexistent:${bin}` }, gui), exe);
});

test('a file that is there but not executable is not the binary', () => {
  const root = mkdtempSync(join(tmpdir(), 'nx-recall-notexec-'));
  const gui = join(root, 'gui');
  mkdirSync(gui, { recursive: true });
  const dud = join(root, 'nx-recall-overlay');
  writeFileSync(dud, 'not a program');
  chmodSync(dud, 0o644);
  assert.equal(findOverlayBinary({ PATH: '' }, gui), null);
});

// -- two writers, one file --------------------------------------------------
//
// Since 0.10.1 the caption bar writes captions.json too: `bounds` when it is
// dragged, `size` when it is scrolled over. This side has to take those changes
// rather than write its stale copy back over them, and has to recognise its own
// write so a drag does not fight the watch that is meant to follow it.
//
// The Rust side's half of this contract is unit-tested there; what is checked
// here is the half that has to MATCH it — the exact text, so each writer can
// tell its own bytes from the other's.

test('the file this side writes is the text the overlay compares against', () => {
  // `save()` writes `JSON.stringify(settings, null, 2)`. The Rust writer
  // reproduces that byte for byte, including key order and 26 rather than 26.0,
  // and `crates/nx-recall-overlay/src/settings.rs` pins these same two literals.
  const settings = normalizeCaptionSettings({ opacity: 0.65 });
  assert.equal(
    JSON.stringify(settings, null, 2),
    '{\n  "turns": 5,\n  "size": 26,\n  "hold_s": 12,\n  "opacity": 0.65,\n  "showYou": true,\n  "clickThrough": true,\n  "bounds": null,\n  "output": null\n}'
  );
  const placed = normalizeCaptionSettings({ opacity: 0.65, bounds: { x: 12, y: 34, width: 1100, height: 340 }, output: 'DP-2' });
  assert.equal(
    JSON.stringify(placed, null, 2),
    '{\n  "turns": 5,\n  "size": 26,\n  "hold_s": 12,\n  "opacity": 0.65,\n  "showYou": true,\n  "clickThrough": true,\n  "bounds": {\n    "x": 12,\n    "y": 34,\n    "width": 1100,\n    "height": 340\n  },\n  "output": "DP-2"\n}'
  );
});

test('a position the bar wrote survives being read back here', () => {
  // What the overlay writes after a drag: same schema, a real rectangle. It has
  // to come back through the normalizer unchanged, or the next slider nudge
  // would write the old position over the new one.
  const fromTheBar = {
    turns: 5, size: 31, hold_s: 12, opacity: 0.6, showYou: true, clickThrough: false,
    bounds: { x: 1460, y: 1004, width: 1100, height: 340 },
    output: 'DP-1',
  };
  const read = normalizeCaptionSettings(fromTheBar);
  assert.deepEqual(read.bounds, fromTheBar.bounds);
  // 0.10.3: the screen the bar was carried to survives the trip as well. A
  // field the normalizer dropped would be a field the next write erased, and
  // the bar would be back on the other monitor at the next launch.
  assert.equal(read.output, 'DP-1');
  assert.equal(read.size, 31);
  assert.equal(read.clickThrough, false);
  // …and writing it straight back out is the same bytes, so neither side sees
  // a change that is not one.
  assert.equal(JSON.stringify(read, null, 2), JSON.stringify(normalizeCaptionSettings(read), null, 2));
});

test('click-through is still the default, and only an explicit false turns it off', () => {
  // The regression this whole change is about was the opposite mistake: the bar
  // could not be clicked AT ALL. Making it movable must not make it grabby by
  // default.
  assert.equal(normalizeCaptionSettings(null).clickThrough, true);
  assert.equal(normalizeCaptionSettings({}).clickThrough, true);
  assert.equal(normalizeCaptionSettings({ size: 30 }).clickThrough, true);
  assert.equal(normalizeCaptionSettings({ clickThrough: false }).clickThrough, false);
});
