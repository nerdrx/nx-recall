// A relabel is retroactive and broadcast (PROTOCOL "Events"): every view
// showing that speaker updates IN PLACE and no client re-queries. That promise
// is kept here, in one place, rather than in each view: anything that renders a
// speaker tags itself `data-sp="<id>"` and this rewrites the name and the
// colour wherever it is on screen — including rows the user is looking at right
// now, mid-scroll, while the live feed keeps appending.

import { speakerColor } from './dom.js';
import { speakerLabel } from './store.js';

export function patchSpeakerLabels(ids) {
  for (const id of ids) {
    if (id == null) continue;
    const label = speakerLabel(id);
    const color = speakerColor(id);
    for (const el of document.querySelectorAll(`[data-sp="${id}"]`)) {
      const nm = el.matches('.nm') ? el : el.querySelector('.nm, .sp-name');
      if (nm) {
        nm.textContent = label;
        nm.classList.toggle('unnamed', label.startsWith('Speaker'));
      }
      const dot = el.matches('.dot') ? el : el.querySelector('.dot');
      if (dot) dot.style.color = color;
    }
  }
}
