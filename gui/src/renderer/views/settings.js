import { h } from '../lib/dom.js';
import { mount as mountMemory } from './memory.js';
import { mountSections } from '../lib/section-nav.js';

export const id = 'settings';
export const SETTINGS_CATEGORIES = [
  { key: 'appearance', title: 'Appearance', hint: 'Spacing & shortcuts' },
  { key: 'processing', title: 'Processing', hint: 'Local model & gaming' },
  { key: 'language', title: 'Language & sound', hint: 'Translation & interpretation' },
  { key: 'quality', title: 'Recognition quality', hint: 'Vocabulary & corrections' },
];
let lastCategory = 'appearance';

export function mount(root, ctx, arg = {}) {
  const controller = mountMemory(root, ctx, { settings: true });
  const body = root.querySelector('.view-body');
  const density = h('select', { id: 'settings-density', onchange: () => {
    document.documentElement.dataset.density = density.value;
    try { localStorage.setItem('nx-recall-density', density.value); } catch { /* A session preference still works without storage. */ }
  } }, h('option', { value: 'comfortable', text: 'Comfortable' }), h('option', { value: 'compact', text: 'Compact' }));
  density.value = document.documentElement.dataset.density || 'comfortable';
  const appearance = h('section', {
    id: 'settings-appearance', class: 'settings-panel', role: 'tabpanel',
    'aria-labelledby': 'settings-tab-appearance', dataset: { settingsPanel: 'appearance' }, hidden: true,
  }, h('header', { class: 'settings-panel-head' },
    h('h2', { class: 'settings-group-title', text: 'Appearance' }),
    h('p', { class: 'sub', text: 'Make Recall comfortable to read and quick to navigate.' })),
    h('section', { class: 'card' }, h('h3', { class: 'card-title', text: 'Content spacing' }),
      h('p', { class: 'sub', text: 'Comfortable gives each turn room. Compact fits more conversations on screen.' }),
      h('label', { for: 'settings-density', text: 'Spacing' }), density),
    h('section', { class: 'card' }, h('h3', { class: 'card-title', text: 'Keyboard shortcuts' }),
      h('dl', { class: 'settings-shortcuts' },
        h('dt', {}, h('kbd', { text: 'Ctrl / ⌘ + K' })), h('dd', { text: 'Quick switch to a view or search' }),
        h('dt', {}, h('kbd', { text: 'Ctrl / ⌘ + F' })), h('dd', { text: 'Open Search from any view' }),
        h('dt', {}, h('kbd', { text: 'Ctrl / Alt + 1–6' })), h('dd', { text: 'Switch between the six main views' }),
        h('dt', {}, h('kbd', { text: 'Arrow keys' })), h('dd', { text: 'Move between Settings categories or Memory tabs' }),
        h('dt', {}, h('kbd', { text: 'Esc' })), h('dd', { text: 'Close a dialog or cancel an inline edit' }))),
  );
  const navigation = mountSections(body, {
    id: 'settings', label: 'Settings categories', initial: arg?.section ?? arg?.category ?? lastCategory,
    sections: SETTINGS_CATEGORIES.map(category => ({
      id: category.key, title: category.title, hint: category.hint,
      panel: category.key === 'appearance' ? appearance : body.querySelector(`[data-settings-panel="${category.key}"]`),
    })),
    related: h('div', { class: 'section-related' }, h('p', { class: 'sub', text: 'Looking for recording controls?' }),
      h('button', { class: 'btn', id: 'settings-open-sources', onclick: () => ctx.go('sources') }, 'Capture, captions & backups')),
    onSelect(key) { lastCategory = key; body.dataset.settingsCategory = key; },
  });
  return { ...controller, selectCategory: navigation.select };
}
