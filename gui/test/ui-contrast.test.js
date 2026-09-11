// Numeric checks against shipped CSS tokens and the controls that consume them.
// This complements headless screenshots; it is not a browser/AT accessibility audit.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

const css = (path) => readFileSync(new URL(path, import.meta.url), 'utf8').replace(/\/\*[\s\S]*?\*\//g, '');
const tokensCSS = css('../src/renderer/tokens.css');
const stylesCSS = css('../src/renderer/styles.css');
const declarations = (body) => Object.fromEntries(
  [...body.matchAll(/([\w-]+)\s*:\s*([^;]+);/g)].map((m) => [m[1], m[2].trim()])
);
function block(source, selector) {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const match = source.match(new RegExp(`${escaped}\\s*\\{([^{}]*)\\}`));
  assert.ok(match, `missing CSS block ${selector}`);
  return declarations(match[1]);
}
const base = block(tokensCSS, ':root');
const themes = {
  light: base,
  dark: { ...base, ...block(tokensCSS, ':root[data-theme="dark"]') },
};
function resolve(value, tokens, seen = []) {
  assert.equal(typeof value, 'string', 'CSS value must exist');
  return value.replace(/var\(\s*(--[\w-]+)(?:\s*,\s*([^()]*))?\s*\)/g, (_, name, fallback) => {
    assert.ok(!seen.includes(name), `circular alias ${name}`);
    return resolve(tokens[name] ?? fallback, tokens, [...seen, name]);
  });
}
function rulesFor(selector) {
  const values = {};
  // Only flat, exact selectors used below; pseudo states are checked explicitly.
  for (const match of stylesCSS.matchAll(/([^{}]+)\{([^{}]*)\}/g)) {
    if (match[1].split(',').map((s) => s.trim()).includes(selector)) {
      Object.assign(values, declarations(match[2]));
    }
  }
  return values;
}
function color(value, tokens) {
  const match = resolve(value, tokens).match(/#[\da-f]{6}\b/i);
  assert.ok(match, `expected shipped opaque hex color in ${value}`);
  return match[0];
}
function luminance(hex) {
  const channels = [1, 3, 5].map((at) => parseInt(hex.slice(at, at + 2), 16) / 255)
    .map((x) => x <= 0.04045 ? x / 12.92 : ((x + 0.055) / 1.055) ** 2.4);
  return channels.reduce((sum, x, i) => sum + x * [0.2126, 0.7152, 0.0722][i], 0);
}
function check(fg, bg, threshold, label) {
  const [low, high] = [luminance(fg), luminance(bg)].sort((a, b) => a - b);
  const ratio = (high + 0.05) / (low + 0.05);
  assert.ok(ratio >= threshold, `${label}: ${fg} on ${bg} = ${ratio.toFixed(2)}:1, requires ${threshold}:1`);
}
const grounds = ['--clear-bg', '--clear-surface', '--clear-tile', '--violet-soft'];
for (const [name, tokens] of Object.entries(themes)) {
  test(`${name}: body and secondary text clear 4.5:1 on reading surfaces`, () => {
    for (const foreground of ['--text', '--muted']) {
      for (const background of grounds) {
        check(color(`var(${foreground})`, tokens), color(`var(${background})`, tokens), 4.5,
          `${name} ${foreground}/${background}`);
      }
    }
  });
  test(`${name}: actual primary button normal/hover colors clear 4.5:1`, () => {
    const primary = rulesFor('.btn.primary');
    const hover = rulesFor('.btn.primary:hover:not(:disabled)');
    for (const background of [primary.background, hover.background]) {
      check(color(primary.color, tokens), color(background, tokens), 4.5, `${name} primary button`);
    }
  });
  test(`${name}: input/select/textarea and off-switch boundaries clear 3:1`, () => {
    for (const selector of ['.input', 'select.input', 'textarea.input', '.toggle']) {
      const control = rulesFor(selector);
      const line = color(control['border-color'] ?? control.border, tokens);
      for (const ground of ['--clear-bg', '--clear-surface', '--clear-tile']) {
        check(line, color(`var(${ground})`, tokens), 3, `${name} ${selector} against ${ground}`);
      }
      check(line, color(control.background, tokens), 3, `${name} ${selector} inner boundary`);
    }
  });
  test(`${name}: final button/input focus outline clears 3:1 on reading surfaces`, () => {
    for (const selector of ['.btn:focus-visible', 'button:focus-visible', 'input:focus-visible', 'select:focus-visible', 'textarea:focus-visible']) {
      const outline = rulesFor(selector).outline;
      assert.ok(outline && outline !== 'none', `${selector} needs a visible outline`);
      for (const background of grounds) {
        check(color(outline, tokens), color(`var(${background})`, tokens), 3, `${name} ${selector}/${background}`);
      }
    }
  });
}
test('OS-followed dark colors and native-widget scheme match explicit dark theme', () => {
  const system = { ...base, ...block(tokensCSS, ':root:not([data-theme="light"])') };
  for (const key of Object.keys(themes.dark)) {
    assert.equal(resolve(system[key], system), resolve(themes.dark[key], themes.dark), `dark parity: ${key}`);
  }
  assert.equal(base['color-scheme'], 'light');
  assert.equal(system['color-scheme'], 'dark');
  for (const selector of ['select.input option', 'select.input optgroup']) {
    const option = rulesFor(selector);
    for (const [name, tokens] of Object.entries(themes)) {
      check(color(option.color, tokens), color(option.background, tokens), 4.5, `${name} native ${selector}`);
    }
  }
});
