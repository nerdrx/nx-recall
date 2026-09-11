import test from 'node:test';
import assert from 'node:assert/strict';
import { sectionKeyboardTarget, mountSections } from '../src/renderer/lib/section-nav.js';

test('section keyboard navigation wraps and ignores unrelated keys', () => {
  const keys = ['appearance', 'processing', 'language', 'quality'];
  assert.equal(sectionKeyboardTarget(keys, 'appearance', 'ArrowUp'), 'quality');
  assert.equal(sectionKeyboardTarget(keys, 'quality', 'ArrowRight'), 'appearance');
  assert.equal(sectionKeyboardTarget(keys, 'language', 'Home'), 'appearance');
  assert.equal(sectionKeyboardTarget(keys, 'processing', 'End'), 'quality');
  assert.equal(sectionKeyboardTarget(keys, 'language', 'Tab'), null);
  assert.equal(sectionKeyboardTarget([], '', 'ArrowDown'), null);
});

test('switching sections preserves controls and maintains a single keyboard tab stop', t => {
  class Node {
    constructor() { this.nodeType = 1; this.children = []; this.dataset = {}; this.attrs = {}; this.events = {}; this.classList = { add() {} }; }
    append(...nodes) { for (const node of nodes) { if (node.parent) node.parent.children.splice(node.parent.children.indexOf(node), 1); this.children.push(node); node.parent = this; } }
    setAttribute(key, value) { this.attrs[key] = value; }
    addEventListener(key, value) { this.events[key] = value; }
    focus() { globalThis.document.activeElement = this; }
  }
  const previous = globalThis.document;
  globalThis.document = { createElement: () => new Node(), createTextNode: text => ({ nodeType: 3, text }) };
  t.after(() => { if (previous === undefined) delete globalThis.document; else globalThis.document = previous; });
  const container = new Node();
  const a = new Node(), b = new Node(), control = new Node();
  control.value = 'unsaved preference'; a.append(control); container.append(a, b);
  const selected = [];
  const result = mountSections(container, { id: 'test', label: 'Sections', initial: 'a', sections: [{ id: 'a', title: 'A', panel: a }, { id: 'b', title: 'B', panel: b }], onSelect: key => selected.push(key) });
  assert.equal(a.hidden, false); assert.equal(b.hidden, true);
  assert.deepEqual(result.nav.children.map(n => n.tabIndex), [0, -1]);
  result.select('b', { focus: true });
  assert.equal(a.hidden, true); assert.equal(b.hidden, false);
  assert.deepEqual(result.nav.children.map(n => n.tabIndex), [-1, 0]);
  assert.equal(document.activeElement, result.nav.children[1]);
  assert.equal(result.nav.children[1].attrs['aria-controls'], b.id);
  assert.equal(b.attrs['aria-labelledby'], result.nav.children[1].attrs.id);
  assert.equal(a.children[0], control); assert.equal(control.value, 'unsaved preference');
  assert.equal(result.select('missing'), 'a');
  assert.deepEqual(selected, ['a', 'b', 'a']);
});
