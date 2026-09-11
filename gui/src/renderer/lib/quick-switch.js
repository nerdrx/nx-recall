import { h, clear } from './dom.js';
import { openSheet } from './sheets.js';

const normalized = value => String(value).normalize('NFKD').replace(/\p{M}/gu, '').toLocaleLowerCase().trim();
export function matchingCommands(commands, query) {
  const words = normalized(query).split(/\s+/).filter(Boolean);
  return commands.filter(command => words.every(word => normalized(`${command.label} ${command.keywords || ''}`).includes(word)))
    .sort((a, b) => Number(normalized(b.label).startsWith(normalized(query))) - Number(normalized(a.label).startsWith(normalized(query))));
}

// Uses native buttons so focus, Enter, and screen readers share one path.
export function openQuickSwitch(commands, search) {
  openSheet(close => {
    const resultList = h('div', { class: 'quick-results' });
    const summary = h('p', { class: 'sub quick-summary', role: 'status' });
    const input = h('input', { id: 'quick-switch-input', class: 'input', type: 'search', placeholder: 'Find a view, action, or conversation…', autocomplete: 'off', 'aria-label': 'Find a view, action, or conversation' });
    const run = action => { close(); action(); };
    const render = () => {
      clear(resultList);
      const matches = matchingCommands(commands, input.value).slice(0, 12);
      const buttons = matches.map(command => h('button', { class: 'quick-result', onclick: () => run(command.run) },
        h('span', {}, h('b', { text: command.label }), h('small', { text: command.description || 'Open view' })),
        h('span', { class: 'quick-result-hint', 'aria-hidden': true, text: command.shortcut || '↵' })));
      const query = input.value.trim();
      if (query) buttons.push(h('button', { class: 'quick-result quick-search', onclick: () => run(() => search(query)) },
        h('span', {}, h('b', { text: `Search for “${query}”` }), h('small', { text: 'Search all retained conversations' })), h('span', { 'aria-hidden': true, text: '↵' })));
      resultList.append(...buttons);
      summary.textContent = `${buttons.length} destination${buttons.length === 1 ? '' : 's'} · ↑↓ to move · Enter to open · Esc to close`;
    };
    const keys = e => {
      const buttons = [...resultList.querySelectorAll('button')];
      if (!buttons.length) return;
      const at = buttons.indexOf(document.activeElement);
      if (e.key === 'ArrowDown') { e.preventDefault(); buttons[(at + 1) % buttons.length].focus(); }
      else if (e.key === 'ArrowUp') { e.preventDefault(); if (at <= 0) input.focus(); else buttons[at - 1].focus(); }
      else if (e.target !== input && (e.key === 'Home' || e.key === 'End')) { e.preventDefault(); buttons[e.key === 'Home' ? 0 : buttons.length - 1].focus(); }
      else if (e.target === input && e.key === 'Enter' && !e.isComposing) { e.preventDefault(); buttons[0].click(); }
    };
    input.addEventListener('input', render);
    input.addEventListener('keydown', keys);
    resultList.addEventListener('keydown', keys);
    render();
    return [h('h2', { text: 'Quick switch' }), input, resultList, summary];
  });
}
