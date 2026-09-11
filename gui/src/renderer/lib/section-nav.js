import { h } from './dom.js';

export function sectionKeyboardTarget(keys, current, key) {
  if (!keys.length) return null;
  const index = Math.max(0, keys.indexOf(current));
  if (key === 'Home') return keys[0];
  if (key === 'End') return keys.at(-1);
  if (key === 'ArrowDown' || key === 'ArrowRight') return keys[(index + 1) % keys.length];
  if (key === 'ArrowUp' || key === 'ArrowLeft') return keys[(index + keys.length - 1) % keys.length];
  return null;
}

/** Accessible category navigation. Move existing panels; never remount their controls. */
export function mountSections(container, { id, label, sections, initial, related = null, onSelect } = {}) {
  const keys = sections.map(section => section.id);
  if (!keys.length || new Set(keys).size !== keys.length) throw new Error('Sections need distinct identifiers.');
  container.classList.add('section-layout');
  const nav = h('div', { class: 'section-nav', role: 'tablist', 'aria-label': label, 'aria-orientation': 'vertical' });
  const panels = h('div', { class: 'section-panels' });
  const sidebar = h('aside', { class: 'section-sidebar' }, nav, related);
  function select(key, { focus = false } = {}) {
    if (!keys.includes(key)) key = keys[0];
    container.dataset.section = key;
    for (const panel of panels.children) panel.hidden = panel.dataset.section !== key;
    for (const tab of nav.children) {
      const selected = tab.dataset.section === key;
      tab.setAttribute('aria-selected', String(selected));
      tab.tabIndex = selected ? 0 : -1;
      if (selected && focus) tab.focus();
    }
    panels.scrollTop = 0;
    onSelect?.(key);
    return key;
  }
  for (const section of sections) {
    const panel = section.panel;
    panel.id = `${id}-${section.id}`;
    panel.classList.add('section-panel');
    panel.setAttribute('role', 'tabpanel');
    panel.setAttribute('aria-labelledby', `${id}-tab-${section.id}`);
    panel.dataset.section = section.id;
    panels.append(panel);
    nav.append(h('button', {
      id: `${id}-tab-${section.id}`, class: 'section-category btn', role: 'tab',
      dataset: { section: section.id, [`${id}Category`]: section.id }, 'aria-controls': panel.id,
      onclick: () => select(section.id), onkeydown: event => {
        const target = sectionKeyboardTarget(keys, section.id, event.key);
        if (!target || event.altKey || event.ctrlKey || event.metaKey) return;
        event.preventDefault(); select(target, { focus: true });
      },
    }, h('span', { class: 'section-category-title', text: section.title }),
      section.hint ? h('small', { class: 'section-category-hint', text: section.hint }) : null));
  }
  container.append(sidebar, panels);
  select(initial);
  return { select, nav, panels };
}
