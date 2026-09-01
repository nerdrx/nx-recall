// Speakers — the identity surface, and where onboarding actually happens.
//
// DESIGN §5: "Onboarding is naming." Two named voices covered 38% of all speech
// in the field recording, so the first-run flow is not a settings page: it is a
// banner over this list saying "these voices are most of your conversations —
// who are they?" with the names right there to type.

import { h, clear, fmtDur, fmtFirstSeen, speakerColor } from '../lib/dom.js';
import { store, speakerLabel, isNamed, onboardingCandidates, ask, reloadSpeakers } from '../lib/store.js';
import { confirmSheet, openSheet, toast } from '../lib/sheets.js';
import { playSpeaker, stop as stopPreview, isActive, onPlayback, noAudioHint } from '../lib/preview.js';

export const id = 'speakers';

/** A voice's preview is keyed by its id, so any surface can drive the same one. */
const previewKey = (spId) => `speaker:${spId}`;

export function mount(root, ctx) {
  const bannerSlot = h('div', { id: 'onboarding-slot' });
  const list = h('div', { id: 'speaker-list' });
  const card = h('div', { class: 'card' }, h('div', { class: 'card-title', text: 'Voices' }), list);
  const body = h('div', { class: 'view-body view-enter' }, bannerSlot, card);
  const sub = h('span', { class: 'sub', id: 'speakers-sub' });

  root.append(
    h('div', { class: 'view-head' }, h('div', {}, h('h1', { text: 'Speakers' }), sub), h('div', { class: 'spacer' })),
    body
  );

  // -- onboarding banner ----------------------------------------------------

  function renderBanner() {
    clear(bannerSlot);
    const ob = onboardingCandidates();
    if (!ob.show) return;
    const pct = Math.round(ob.share * 100);
    const row = h('div', { class: 'who-row' });
    for (const sp of ob.speakers) {
      row.append(
        h(
          'span',
          { class: 'who-pair' },
          // Listening comes first, literally: you cannot answer "who is this?"
          // from a name field alone. Naming autoplays the sample too, so either
          // door leads to the same one motion.
          previewButton(sp.id),
          h(
            'button',
            { class: 'btn primary', dataset: { onboard: String(sp.id) }, onclick: () => startRename(sp.id, { listen: true }) },
            h('span', { class: 'dot', style: `color:${speakerColor(sp.id)}` }),
            ` Name ${speakerLabel(sp.id)} · ${fmtDur(sp.total_ms)}`
          )
        )
      );
    }
    bannerSlot.append(
      h(
        'div',
        { class: 'banner', id: 'onboarding-banner' },
        h('h2', { text: 'These voices are most of your conversations — who are they?' }),
        h('p', {
          text: `${pct}% of everything captured so far came from voices that do not have a name yet. Play a voice to hear who it is, then name it — naming relabels every past segment it matched and every future one too, and it is the only setup this needs.`,
        }),
        row,
        h('p', { class: 'banner-hint', id: 'onboarding-hint' })
      )
    );
    paintPlayState();
  }

  // -- preview --------------------------------------------------------------

  /** ▶ / ■ for one voice. Every surface uses this, so they cannot disagree. */
  function previewButton(spId) {
    const btn = h('button', {
      class: 'btn small preview',
      dataset: { preview: String(spId) },
      title: 'Listen to this voice',
      onclick: (e) => {
        e.stopPropagation();
        void togglePreview(spId);
      },
    });
    paintButton(btn, spId);
    return btn;
  }

  function paintButton(btn, spId) {
    const on = isActive(previewKey(spId));
    btn.classList.toggle('on', on);
    btn.textContent = on ? '■' : '▶';
    btn.setAttribute('aria-pressed', String(on));
    btn.setAttribute('aria-label', on ? `Stop ${speakerLabel(spId)}` : `Play a sample of ${speakerLabel(spId)}`);
  }

  /** Repaint every play control and row indicator from the shared state. */
  function paintPlayState() {
    for (const btn of root.querySelectorAll('[data-preview]')) paintButton(btn, Number(btn.dataset.preview));
    for (const row of list.querySelectorAll('.sp-row')) {
      row.classList.toggle('playing', isActive(previewKey(Number(row.dataset.speaker))));
    }
  }

  /**
   * The hint belongs next to the thing that failed, not in a toast that has
   * scrolled away by the time you look: "why is there no sound?" is answered
   * on the row you pressed.
   */
  function setHint(spId, text) {
    const show = (el) => {
      if (!el) return;
      el.textContent = text;
      el.classList.toggle('shown', !!text);
    };
    show(list.querySelector(`.sp-hint[data-hint="${spId}"]`));
    const banner = document.getElementById('onboarding-hint');
    if (banner && Number(banner.dataset.for) === spId) show(banner);
  }

  async function togglePreview(spId) {
    const key = previewKey(spId);
    if (isActive(key)) {
      stopPreview();
      return null;
    }
    const banner = document.getElementById('onboarding-hint');
    if (banner) banner.dataset.for = String(spId);
    setHint(spId, '');
    const res = await playSpeaker(key, spId);
    // An interrupted preview is the user's own doing; it explains itself.
    if (!res.stopped && !res.played) setHint(spId, noAudioHint(res.error));
    return res;
  }

  // -- row overflow menu ----------------------------------------------------
  //
  // The row answers "who is this and how much do they talk": ▶, the name, the
  // counts. Merge/Split/Delete are not that question — they are three verbs of
  // wildly different weight sitting a stray click apart, with Delete on the
  // end. They move behind one ⋯ per row, where Delete can still look
  // destructive without being adjacent to anything routine. Dragging one voice
  // onto another still merges, untouched.

  let openMenu = null; // {menu, btn, onDoc, onKey}
  let pendingRender = false;

  function closeMenu({ restoreFocus = false } = {}) {
    if (!openMenu) return;
    const { menu, btn, onDoc, onKey } = openMenu;
    openMenu = null;
    document.removeEventListener('mousedown', onDoc, true);
    document.removeEventListener('keydown', onKey, true);
    menu.remove();
    btn.setAttribute('aria-expanded', 'false');
    if (restoreFocus && btn.isConnected) btn.focus();
    if (pendingRender) {
      pendingRender = false;
      renderList();
    }
  }

  function openRowMenu(spId, btn, wrap) {
    const item = (label, run, cls = '') =>
      h(
        'button',
        {
          class: `menu-item${cls ? ` ${cls}` : ''}`,
          role: 'menuitem',
          type: 'button',
          onclick: () => {
            closeMenu();
            run();
          },
        },
        label
      );

    const menu = h(
      'div',
      { class: 'row-menu', role: 'menu', 'aria-label': `Actions for ${speakerLabel(spId)}` },
      item('Show in transcript', () => showInTranscript(spId)),
      item('Merge…', () => pickMerge(spId), 'menu-merge'),
      item('Split', () => doSplit(spId)),
      // Still visibly destructive — just no longer one slip away from Merge.
      item('Delete', () => doDelete(spId), 'danger')
    );
    menu.dataset.menu = String(spId);

    // Both handlers sit on the document, so both check that this view is still
    // mounted: switching views takes the menu's DOM away and these have to go
    // with it rather than hold a dead row alive.
    const onDoc = (e) => {
      if (!list.isConnected) {
        closeMenu();
        return;
      }
      if (menu.contains(e.target) || btn.contains(e.target)) return;
      closeMenu();
    };
    const onKey = (e) => {
      if (!list.isConnected) {
        closeMenu();
        return;
      }
      if (e.key === 'Escape') {
        e.stopPropagation();
        closeMenu({ restoreFocus: true });
        return;
      }
      if (e.key !== 'ArrowDown' && e.key !== 'ArrowUp') return;
      const items = [...menu.querySelectorAll('.menu-item')];
      const at = items.indexOf(document.activeElement);
      e.preventDefault();
      const next = e.key === 'ArrowDown' ? at + 1 : at - 1;
      items[(next + items.length) % items.length].focus();
    };

    wrap.append(menu);
    btn.setAttribute('aria-expanded', 'true');
    openMenu = { menu, btn, onDoc, onKey };
    // A menu on the last row would open into the bottom edge of the card.
    const box = menu.getBoundingClientRect();
    if (box.bottom > window.innerHeight - 8) menu.classList.add('up');
    document.addEventListener('mousedown', onDoc, true);
    document.addEventListener('keydown', onKey, true);
    menu.querySelector('.menu-item')?.focus();
  }

  function moreButton(spId) {
    const wrap = h('span', { class: 'sp-menu' });
    const btn = h(
      'button',
      {
        class: 'btn small more',
        dataset: { more: String(spId) },
        'aria-haspopup': 'menu',
        'aria-expanded': 'false',
        'aria-label': `More actions for ${speakerLabel(spId)}`,
        title: 'Merge, split or delete this voice',
        onclick: (e) => {
          e.stopPropagation();
          const mine = openMenu?.btn === btn;
          closeMenu();
          if (!mine) openRowMenu(spId, btn, wrap);
        },
      },
      '⋯'
    );
    wrap.append(btn);
    return wrap;
  }

  function showInTranscript(spId) {
    ctx.showSpeakerInTranscript?.(spId);
  }

  // -- list -----------------------------------------------------------------

  function renderList() {
    // The live feed repaints this list every couple of seconds (the counts
    // move). A repaint while a row menu is open would tear the menu out from
    // under the pointer, so the repaint waits for the menu to close instead.
    if (openMenu) {
      pendingRender = true;
      return;
    }
    clear(list);
    const rows = [...store.speakers.values()].sort((a, b) => (b.total_ms ?? 0) - (a.total_ms ?? 0));
    sub.textContent = `${rows.length} voice${rows.length === 1 ? '' : 's'} · ${rows.filter(isNamed).length} named`;
    const badge = document.getElementById('badge-speakers');
    if (badge) badge.textContent = String(rows.length);

    if (!rows.length) {
      list.append(
        h(
          'div',
          { class: 'empty' },
          h('b', { text: 'No voices yet' }),
          h('p', { text: 'Voices appear here the first time they are heard on an allowed source.' })
        )
      );
      return;
    }
    for (const sp of rows) list.append(speakerRow(sp));
    // A repaint must not lose the "this one is sounding right now" mark.
    paintPlayState();
  }

  function speakerRow(sp) {
    const color = speakerColor(sp.id);
    const named = isNamed(sp);
    const name = h('span', {
      class: `sp-name${named ? '' : ' unnamed'}`,
      text: speakerLabel(sp.id),
      title: 'Click to rename — renames are retroactive',
      tabindex: '0',
      role: 'button',
    });
    const row = h('div', {
      class: 'sp-row',
      dataset: { speaker: String(sp.id) },
      draggable: 'true',
    });

    name.addEventListener('click', () => startRename(sp.id));
    name.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') startRename(sp.id);
    });

    // Merge by dragging one voice onto another. The picker below does the same
    // thing for anyone who cannot or does not want to drag.
    row.addEventListener('dragstart', (e) => {
      e.dataTransfer.setData('text/plain', String(sp.id));
      e.dataTransfer.effectAllowed = 'move';
      row.classList.add('dragging');
    });
    row.addEventListener('dragend', () => row.classList.remove('dragging'));
    row.addEventListener('dragover', (e) => {
      e.preventDefault();
      e.dataTransfer.dropEffect = 'move';
      row.classList.add('drag-over');
    });
    row.addEventListener('dragleave', () => row.classList.remove('drag-over'));
    row.addEventListener('drop', (e) => {
      e.preventDefault();
      row.classList.remove('drag-over');
      const from = Number(e.dataTransfer.getData('text/plain'));
      if (Number.isFinite(from) && from !== sp.id) doMerge(from, sp.id);
    });

    row.append(
      previewButton(sp.id),
      h(
        'span',
        { class: 'sp-id', dataset: { sp: String(sp.id) } },
        h('span', { class: 'dot', style: `color:${color}` }),
        h(
          'span',
          {},
          name,
          h('span', { class: 'sp-sub', text: ` first heard ${fmtFirstSeen(sp.first_seen)}` }),
          h('span', { class: 'sp-hint', dataset: { hint: String(sp.id) } })
        )
      ),
      h('span', { class: 'sp-num' }, String(sp.segments ?? 0), h('small', { text: 'segments' })),
      h('span', { class: 'sp-num' }, fmtDur(sp.total_ms), h('small', { text: 'total speech' })),
      h('span', { class: 'sp-actions' }, moreButton(sp.id))
    );
    return row;
  }

  // -- actions --------------------------------------------------------------

  /**
   * `listen: true` starts the voice playing as the field opens. It is not
   * ambient autoplay — the person pressed "Name this voice", and the only way
   * to answer that question is to hear it. Listening and typing become one
   * motion instead of two round trips through the transcript.
   */
  function startRename(spId, { listen = false } = {}) {
    const row = list.querySelector(`.sp-row[data-speaker="${spId}"]`);
    const target = row?.querySelector('.sp-name');
    if (!target) {
      // The banner can ask to rename a voice while the list is elsewhere.
      renderList();
      return startRename(spId, { listen });
    }
    if (listen && !isActive(previewKey(spId))) void togglePreview(spId);
    const sp = store.speakers.get(spId);
    const input = h('input', {
      class: 'input',
      id: 'rename-input',
      value: sp?.name ?? '',
      placeholder: sp?.auto ?? 'Name this voice',
      style: 'width:180px',
    });
    // Enter commits and then blurs, so without this guard the same rename runs
    // twice and the second replaceWith throws on an already-detached input.
    let settled = false;
    const restore = () => {
      if (input.isConnected) input.replaceWith(target);
    };
    const commit = async () => {
      if (settled) return;
      settled = true;
      const next = input.value.trim();
      restore();
      if (next === (sp?.name ?? '')) return;
      try {
        await ask('speakers.name', { id: spId, name: next });
        // Nothing is patched here on purpose: the daemon broadcasts `relabel`
        // and the whole UI updates from that one path, so a rename made in
        // another client looks identical to one made here.
      } catch (e) {
        toast(`Could not rename — ${e.message}`, 'error');
      }
    };
    input.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') commit();
      if (e.key === 'Escape') {
        settled = true;
        restore();
      }
    });
    input.addEventListener('blur', commit);
    target.replaceWith(input);
    input.focus();
    input.select();
  }

  function pickMerge(fromId) {
    openSheet((close) => {
      const others = [...store.speakers.values()].filter((s) => s.id !== fromId);
      const pick = h('div', { class: 'sp-pick' });
      for (const sp of others) {
        pick.append(
          h(
            'button',
            {
              dataset: { mergeInto: String(sp.id) },
              onclick: () => {
                close();
                doMerge(fromId, sp.id);
              },
            },
            h('span', { class: 'dot', style: `color:${speakerColor(sp.id)}` }),
            speakerLabel(sp.id)
          )
        );
      }
      return [
        h('h2', { text: `Merge ${speakerLabel(fromId)} into…` }),
        h('p', {
          class: 'sub',
          text: 'Both voices become one identity, retroactively. Merges are recorded and can be undone from the operations log.',
        }),
        pick,
        h('div', { class: 'actions' }, h('button', { class: 'btn', onclick: () => close() }, 'Cancel')),
      ];
    });
  }

  async function doMerge(fromId, intoId) {
    const ok = await confirmSheet({
      title: `Merge ${speakerLabel(fromId)} into ${speakerLabel(intoId)}?`,
      body: 'Every segment matched to the first voice is relabelled as the second, in every view, including past ones. The first voice disappears from this list.',
      confirmLabel: 'Merge',
    });
    if (!ok) return;
    try {
      await ask('speakers.merge', { from: fromId, into: intoId });
      toast(`Merged into ${speakerLabel(intoId)}.`, 'ok');
    } catch (e) {
      toast(`Could not merge — ${e.message}`, 'error');
    }
  }

  async function doSplit(spId) {
    const ok = await confirmSheet({
      title: `Split ${speakerLabel(spId)}?`,
      body: 'The recordings matched to this voice are re-clustered, which can take a while. Progress shows in the status bar and the rest of the app keeps working.',
      confirmLabel: 'Split',
    });
    if (!ok) return;
    try {
      const res = await ask('speakers.split', { id: spId });
      toast(`Re-clustering started (${res.op}).`, 'ok');
    } catch (e) {
      toast(`Could not split — ${e.message}`, 'error');
    }
  }

  async function doDelete(spId) {
    let preview;
    try {
      preview = await ask('delete.preview', { speaker: spId });
    } catch (e) {
      toast(`Could not check what that would delete — ${e.message}`, 'error');
      return;
    }
    const ok = await confirmSheet({
      title: `Delete everything from ${speakerLabel(spId)}?`,
      body: 'This is a real purge: the segments, their text and their audio are removed and the space is reclaimed. It cannot be undone after the undo window closes.',
      detail: `${preview.segments} segments · ${Math.round((preview.bytes ?? 0) / 1024)} KB of audio`,
      confirmLabel: `Delete ${preview.segments} segments`,
      danger: true,
    });
    if (!ok) return;
    try {
      const res = await ask('delete.run', { speaker: spId });
      toast(`Deleting (${res.op}) — progress is in the status bar.`, 'ok');
    } catch (e) {
      toast(`Could not delete — ${e.message}`, 'error');
    }
  }

  // -- updates --------------------------------------------------------------

  function update(change) {
    if (!change) return;
    if (change.speakers) {
      // The store just re-pulled speakers.list (a voice was minted mid-view).
      renderList();
      renderBanner();
    }
    if (change.relabel || change.merged) {
      renderBanner();
      renderList();
    }
    if (change.added || change.purged) {
      // Counts moved; the rows show counts, so repaint them (cheap: tens of rows).
      renderList();
      renderBanner();
    }
    if (change.opFinished) {
      reloadSpeakers().then(() => {
        renderList();
        renderBanner();
      });
    }
  }

  // Playback state lives outside the view (one <audio> for the whole app), so
  // the rows follow it rather than owning it. The view has no unmount hook, so
  // the subscription retires itself once its DOM is gone.
  const off = onPlayback(() => {
    if (!list.isConnected) {
      off();
      return;
    }
    paintPlayState();
  });

  renderBanner();
  renderList();
  paintPlayState();
  return { update, renderList, renderBanner, startRename, closeMenu };
}
