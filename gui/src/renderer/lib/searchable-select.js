import { h, clear } from './dom.js';
import { speakerMatches } from './speaker-match.js';

// Keep the native select's keyboard and form behavior. Searching narrows choices;
// it never changes or submits the current selection.
export function searchableSpeakerSelect(select, { label = 'Find a speaker', threshold = 12 } = {}) {
  let options = [...select.options];
  const input = h('input', { class: 'input', type: 'search', id: select.id ? `${select.id}-find` : undefined,
    placeholder: 'Find speaker…', 'aria-label': label, autocomplete: 'off' });
  const status = h('span', { class: 'sub speaker-select-status', role: 'status', 'aria-live': 'polite', hidden: true });
  const element = h('div', { class: 'searchable-speaker-select' }, input, select, status);
  select.setAttribute('aria-label', select.getAttribute('aria-label') || 'Speaker');
  function paint() {
    const value = select.value;
    const query = input.value.trim();
    const matches = options.filter(option => option.value && speakerMatches({ id: option.value, auto: option.dataset.speakerAuto }, query, option.textContent));
    const keep = new Set(matches);
    clear(select);
    for (const option of options) if (!query || !option.value || option.value === value || keep.has(option)) select.append(option);
    select.value = value;
    input.hidden = options.filter(option => option.value).length < threshold && !query;
    input.disabled = select.disabled;
    status.hidden = !query;
    const retained = value && !matches.some(option => option.value === value);
    status.textContent = `${matches.length} matching speaker${matches.length === 1 ? '' : 's'}${retained ? ' · current selection kept' : ''}`;
  }
  input.addEventListener('input', paint);
  input.addEventListener('keydown', event => {
    if (event.key === 'ArrowDown') { event.preventDefault(); select.focus(); }
    if (event.key === 'Escape' && input.value) { event.preventDefault(); event.stopPropagation(); input.value = ''; paint(); }
    // Enter does not submit a surrounding search form or change an identity.
    if (event.key === 'Enter') event.preventDefault();
  });
  select.addEventListener('change', paint);
  paint();
  return { element, input,
    // Call after the owner replaces all options, preserving the typed query.
    refresh() { options = [...select.options]; paint(); },
    reset() { input.value = ''; paint(); },
  };
}
