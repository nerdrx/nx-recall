import { h } from '../lib/dom.js';
import { mount as mountMemory } from './memory.js';

export const id = 'settings';
export function mount(root, ctx) {
  const controller = mountMemory(root, ctx, { settings: true });
  const density = h('select', { id: 'settings-density', onchange: () => {
    document.documentElement.dataset.density = density.value;
    try { localStorage.setItem('nx-recall-density', density.value); } catch { /* unavailable storage still permits a session preference */ }
  } }, h('option', { value: 'comfortable', text: 'Comfortable' }), h('option', { value: 'compact', text: 'Compact' }));
  density.value = document.documentElement.dataset.density || 'comfortable';
  root.querySelector('.view-body').prepend(h('section', { class: 'card' }, h('h2', { text: 'Appearance and navigation' }),
    h('label', { for: 'settings-density', text: 'Content spacing' }), density,
    h('p', { class: 'sub', text: 'Keyboard: Ctrl/Cmd+F opens Search. Ctrl or Alt + 1–6 switches views. Arrow keys move between Memory tabs.' }),
    h('button', { class: 'btn', onclick: () => ctx.go('sources') }, 'Capture, captions, storage and backups')));
  return controller;
}
