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
