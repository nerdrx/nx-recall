// Headless driver — only ever loaded when NX_RECALL_E2E=1.
//
// It drives the REAL DOM (clicking the real rail buttons, typing into the real
// rename input) and reads back the real rendered rows, so a green report means
// the UI works, not that a mock agreed with itself. It binds no port and opens
// no server: the whole conversation is executeJavaScript into the one window,
// and the result is a JSON report plus PNGs in gui/test-artifacts/.
//
// It runs inside headless gamescope (scripts/headless_test.sh) so a UI check
// never opens a window on the developer's desktop.

import { app } from 'electron';
import { mkdirSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const __dirname = dirname(fileURLToPath(import.meta.url));
const OUT = process.env.NX_RECALL_E2E_OUT || join(__dirname, '..', '..', 'test-artifacts');

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

export function runE2E(deps) {
  const results = [];
  let shots = 0;

  const win = () => {
    const w = deps.getWindow();
    if (!w || w.isDestroyed()) throw new Error('no window');
    return w;
  };
  const js = (code) => win().webContents.executeJavaScript(code, true);

  async function shot(name) {
    // Software GL inside headless gamescope presents lazily: without waiting
    // for two real frames, capturePage hands back the PREVIOUS view and the
    // screenshots quietly document the wrong screen.
    await js('new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r)))').catch(() => {});
    win().webContents.invalidate?.();
    await sleep(500);
    const img = await win().webContents.capturePage();
    const file = join(OUT, `${String(++shots).padStart(2, '0')}-${name}.png`);
    writeFileSync(file, img.toPNG());
    return file;
  }

  async function step(name, fn) {
    const started = Date.now();
    try {
      const detail = await fn();
      results.push({ name, ok: true, ms: Date.now() - started, detail: detail ?? null });
      console.log(`[e2e] PASS ${name}${detail ? ` — ${JSON.stringify(detail)}` : ''}`);
    } catch (e) {
      results.push({ name, ok: false, ms: Date.now() - started, error: e.message });
      console.log(`[e2e] FAIL ${name} — ${e.message}`);
      try {
        await shot(`FAIL-${name}`);
      } catch {
        /* the failure report matters more than its picture */
      }
    }
  }

  async function waitFor(label, fn, { timeout = 15000, every = 200 } = {}) {
    const until = Date.now() + timeout;
    let last;
    for (;;) {
      last = await fn();
      if (last) return last;
      if (Date.now() > until) throw new Error(`timed out waiting for ${label} (last: ${JSON.stringify(last)})`);
      await sleep(every);
    }
  }

  const assert = (cond, msg) => {
    if (!cond) throw new Error(msg);
  };

  async function main() {
    mkdirSync(OUT, { recursive: true });
    const w = win();
    if (w.webContents.isLoading()) await new Promise((r) => w.webContents.once('did-finish-load', r));
    await sleep(1200); // first paint + boot() queries

    // 1 — the socket handshake actually completed
    await step('connect', async () => {
      await waitFor('connected', async () => deps.getUi().conn.status === 'connected');
      return { daemon: deps.getUi().conn.daemon, socket: deps.getUi().conn.socketPath };
    });

    // 2 — history rendered from the transcript query
    await step('segments-render', async () => {
      const c = await waitFor('segment rows', async () => {
        const c = await js('window.__recallDebug ? window.__recallDebug.counts() : null');
        return c && c.rows > 0 ? c : null;
      });
      assert(c.rows > 0, 'no segment rows in the DOM');
      assert(c.speakers > 0, 'no speakers loaded');
      return c;
    });

    // 3 — the live feed appends without a re-query
    await step('live-feed-appends', async () => {
      const before = (await js('window.__recallDebug.counts()')).rows;
      const after = await waitFor(
        'a new segment',
        async () => {
          const n = (await js('window.__recallDebug.counts()')).rows;
          return n > before ? n : null;
        },
        { timeout: 12000 }
      );
      return { before, after };
    });

    // 4 — uncertain segments are visibly muted and carry the "?" affordance
    await step('uncertain-segments-muted', async () => {
      const n = await js('document.querySelectorAll("#seg-list .seg.uncertain").length');
      const q = await js('document.querySelectorAll("#seg-list .seg.uncertain .qmark").length');
      const why = await js('(document.querySelector("#seg-list .qmark")||{}).title || ""');
      assert(n > 0, 'no uncertain segments rendered — the overlap fixtures should produce some');
      assert(q > 0, 'uncertain segments carry no "?" affordance');
      assert(why.length > 20, 'the "?" does not explain itself');

      // 0.5.5: a nameless segment says WHICH kind of nameless it is. Refused by
      // the overlap gate and matched against nothing are different problems
      // with different fixes, and "Unassigned" said neither.
      const labels = await js(
        '[...new Set([...document.querySelectorAll("#seg-list .seg .who .nm.reasoned")].map(e => e.textContent))]'
      );
      assert(labels.includes('several voices'), `no overlap-refused label rendered: ${JSON.stringify(labels)}`);
      assert(labels.includes('unknown voice'), `no unmatched-voice label rendered: ${JSON.stringify(labels)}`);
      const generic = await js('document.getElementById("seg-list").textContent.includes("Unassigned")');
      assert(!generic, 'a transcript row still says "Unassigned"');
      return { uncertain: n, qmarks: q, labels, why: why.slice(0, 60) };
    });

    await step('shot-transcript', async () => ({ file: await shot('transcript') }));

    // 5 — the segment sheet opens, and reassigning writes through the daemon
    await step('segment-sheet-reassign', async () => {
      await js('document.querySelector("#seg-list .seg.uncertain").click()');
      await waitFor('the sheet', async () => js('!!document.querySelector(".sheet #segment-save")'));
      const file = await shot('segment-sheet');
      // Pick the second identity offered (the first is "Unassigned").
      await js('document.querySelectorAll(".sheet .sp-pick button")[1].click()');
      await js('document.querySelector(".sheet #segment-save").click()');
      await waitFor('the sheet to close', async () => js('!document.querySelector(".sheet")'));
      return { file };
    });

    // 5b — a segment's own ▶: the same shared player, one clip.
    await step('segment-sheet-plays-its-clip', async () => {
      await js('document.querySelector("#seg-list .seg").click()');
      await waitFor('the sheet', async () => js('!!document.querySelector(".sheet #segment-play")'));
      await js('document.querySelector(".sheet #segment-play").click()');
      const heard = await waitFor(
        'the audio element to play the segment',
        async () => {
          const a = await js('window.__recallDebug.audio()');
          return a.exists && (a.ended || a.currentTime > 0) ? a : null;
        },
        { timeout: 15000, every: 100 }
      );
      assert(heard.error == null, `the media element reported error ${heard.error}`);
      await js('document.querySelector(".scrim").dispatchEvent(new MouseEvent("mousedown", {bubbles: true}))');
      await waitFor('the sheet to close', async () => js('!document.querySelector(".sheet")'));
      // Closing the sheet takes the stop button away, so it takes the sound.
      const after = await js('window.__recallDebug.audio()');
      assert(after.phase === 'idle', `preview kept running after the sheet closed (${after.phase})`);
      return { currentTime: heard.currentTime, readyState: heard.readyState };
    });

    // 6 — speakers view + the onboarding banner (DESIGN §5)
    await step('speakers-and-onboarding', async () => {
      await js('document.querySelector(\'.rail-item[data-view="speakers"]\').click()');
      await waitFor('the speaker list', async () => js('document.querySelectorAll("#speaker-list .sp-row").length > 0'));
      const banner = await js('!!document.getElementById("onboarding-banner")');
      const headline = await js('(document.querySelector("#onboarding-banner h2")||{}).textContent || ""');
      const rows = await js('document.querySelectorAll("#speaker-list .sp-row").length');
      assert(banner, 'the onboarding banner did not appear even though unnamed voices dominate');
      assert(/who are they/i.test(headline), `unexpected onboarding copy: ${headline}`);
      return { rows, headline };
    });

    // A voice minted after connect must appear without a view remount — the
    // mock mints Speaker_77 on its 4th feed tick, which has fired by now.
    await step('minted-voice-appears', async () => {
      const found = await waitFor(
        'the minted voice in the list',
        async () =>
          js(
            '[...document.querySelectorAll("#speaker-list .sp-row")].some(r => r.textContent.includes("Speaker_77"))'
          ),
        { timeout: 15000 }
      );
      assert(found, 'Speaker_77 was minted mid-feed but never appeared in the speakers view');
      return { found };
    });

    // 6b — the feature this release exists for: you can HEAR a voice before
    // you are asked to name it. The assertion is on the real <audio> element,
    // not on the UI's opinion of it.
    await step('voice-preview-plays-and-stops', async () => {
      const target = await js(`(() => {
        const b = document.querySelector('[data-onboard]');
        return b ? Number(b.dataset.onboard) : null;
      })()`);
      assert(target != null, 'no unnamed voice to preview');

      await js(`document.querySelector('.sp-row[data-speaker="${target}"] [data-preview]').click()`);
      const marked = await waitFor(
        'the row to show it is playing',
        async () => js(`!!document.querySelector('.sp-row[data-speaker="${target}"].playing')`),
        { timeout: 10000, every: 100 }
      );
      const heard = await waitFor(
        'the audio element to play',
        async () => {
          const a = await js('window.__recallDebug.audio()');
          return a.exists && (a.ended || a.currentTime > 0) ? a : null;
        },
        { timeout: 20000, every: 100 }
      );
      assert(marked, 'the row never showed a playing indicator');
      assert(heard.error == null, `the media element reported error ${heard.error}`);
      const label = await js(`document.querySelector('.sp-row[data-speaker="${target}"] [data-preview]').textContent`);
      assert(label === '■', `the play button did not become a stop button (got ${JSON.stringify(label)})`);

      const file = await shot('speakers-playing');
      // Pressing it again stops, and nothing else was ever sounding at once.
      await js(`document.querySelector('.sp-row[data-speaker="${target}"] [data-preview]').click()`);
      await waitFor('the preview to stop', async () => js('window.__recallDebug.audio().phase === "idle"'), {
        timeout: 8000,
        every: 100,
      });
      const stopped = await js(`document.querySelectorAll('.sp-row.playing').length`);
      assert(stopped === 0, 'a row still claims to be playing after stop');
      return { speaker: target, currentTime: heard.currentTime, samples: heard.total, file };
    });

    // 6c — a voice whose audio retention already took. This must read as a
    // setting, in place, not as a failed click.
    await step('preview-with-no-audio-explains-retention', async () => {
      const rows = await js(`[...document.querySelectorAll('#speaker-list .sp-row')].map(r => Number(r.dataset.speaker))`);
      assert(rows.includes(5), `the aged-out mock voice is not listed: ${rows}`);
      await js(`document.querySelector('.sp-row[data-speaker="5"] [data-preview]').click()`);
      const hint = await waitFor(
        'the retention hint',
        async () => js(`(document.querySelector('.sp-row[data-speaker="5"] .sp-hint.shown')||{}).textContent || ''`),
        { timeout: 10000, every: 100 }
      );
      assert(/retention/i.test(hint), `the hint does not explain itself: ${JSON.stringify(hint)}`);
      const toastOnly = await js('[...document.querySelectorAll(".toast")].some(t => /retention/i.test(t.textContent))');
      assert(!toastOnly, 'the retention hint was a toast — it has to sit on the row');
      return { hint, file: await shot('speakers-no-audio') };
    });

    // 6d — DESIGN §5's onboarding, made answerable: pressing "Name this voice"
    // plays it, so listening and typing are one motion.
    await step('naming-a-voice-plays-it', async () => {
      const target = await js(`(() => {
        const b = document.querySelector('[data-onboard]');
        return b ? Number(b.dataset.onboard) : null;
      })()`);
      assert(target != null, 'no unnamed voice in the banner');
      await js('document.querySelector("[data-onboard]").click()');
      await waitFor('the rename input', async () => js('!!document.getElementById("rename-input")'));
      const playing = await waitFor(
        'the voice to start playing with the field',
        async () => js(`window.__recallDebug.audio().key === "speaker:${target}"`),
        { timeout: 10000, every: 100 }
      );
      assert(playing, 'naming a voice did not play it');
      // Leave the UI as the later steps expect to find it.
      await js(`(() => {
        const i = document.getElementById('rename-input');
        i.dispatchEvent(new KeyboardEvent('keydown', {key: 'Escape', bubbles: true}));
        return true;
      })()`);
      await js('document.querySelector(\'.rail-item[data-view="transcript"]\').click()');
      await js('document.querySelector(\'.rail-item[data-view="speakers"]\').click()');
      await waitFor('the speaker list again', async () => js('document.querySelectorAll("#speaker-list .sp-row").length > 0'));
      const quiet = await js('window.__recallDebug.audio().phase');
      assert(quiet === 'idle', `leaving the view left audio running (${quiet})`);
      return { speaker: target };
    });

    // 6e — the row is for reading; the verbs live behind one ⋯. Delete is still
    // red, it just no longer sits a slip away from Merge (0.5.5).
    await step('voice-row-overflow-menu', async () => {
      const target = await js('Number(document.querySelector("#speaker-list .sp-row").dataset.speaker)');
      const actions = await js(`document.querySelectorAll('.sp-row[data-speaker="${target}"] .sp-actions .btn').length`);
      assert(actions === 1, `the row still carries ${actions} action buttons — they belong behind ⋯`);

      const more = `document.querySelector('.sp-row[data-speaker="${target}"] [data-more]')`;
      const haspopup = await js(`${more}.getAttribute('aria-haspopup')`);
      assert(haspopup === 'menu', `⋯ does not announce a menu (aria-haspopup=${haspopup})`);

      await js(`${more}.click()`);
      await waitFor('the row menu', async () => js('!!document.querySelector(".row-menu")'));
      const items = await js('[...document.querySelectorAll(".row-menu .menu-item")].map(b => b.textContent)');
      for (const want of ['Show in transcript', 'Merge…', 'Split', 'Delete']) {
        assert(items.includes(want), `"${want}" is not in the row menu: ${JSON.stringify(items)}`);
      }
      const danger = await js('(document.querySelector(".row-menu .menu-item.danger")||{}).textContent || ""');
      assert(danger === 'Delete', `Delete lost its destructive styling inside the menu (danger item: "${danger}")`);
      assert((await js(`${more}.getAttribute('aria-expanded')`)) === 'true', 'the ⋯ button never reports itself as expanded');
      const file = await shot('speakers-row-menu');

      // Escape closes it…
      await js('document.dispatchEvent(new KeyboardEvent("keydown", {key: "Escape", bubbles: true}))');
      await waitFor('the menu to close on Escape', async () => js('!document.querySelector(".row-menu")'));
      // …and so does a press anywhere else.
      await js(`${more}.click()`);
      await waitFor('the menu again', async () => js('!!document.querySelector(".row-menu")'));
      await js('document.getElementById("main").dispatchEvent(new MouseEvent("mousedown", {bubbles: true}))');
      await waitFor('the menu to close on click-away', async () => js('!document.querySelector(".row-menu")'));
      assert((await js(`${more}.getAttribute('aria-expanded')`)) === 'false', 'the ⋯ button still claims to be expanded');
      return { speaker: target, items, file };
    });

    // 6f — "show in transcript": a voice is really a question about what they
    // said, and the answer is the existing filter, already set (0.5.5).
    await step('show-voice-in-transcript', async () => {
      const target = await js('Number(document.querySelector("#speaker-list .sp-row").dataset.speaker)');
      await js(`document.querySelector('.sp-row[data-speaker="${target}"] [data-more]').click()`);
      await waitFor('the row menu', async () => js('!!document.querySelector(".row-menu")'));
      await js(
        '[...document.querySelectorAll(".row-menu .menu-item")].find(b => b.textContent === "Show in transcript").click()'
      );
      await waitFor('the transcript to mount', async () => js('window.__recallDebug.view() === "transcript"'));
      const filter = await waitFor('the speaker filter to be set', async () =>
        js('document.getElementById("transcript-filter").value')
      );
      assert(Number(filter) === target, `the filter reads "${filter}", not the voice that was asked for (${target})`);
      const rows = await js('document.querySelectorAll("#seg-list .seg").length');
      assert(rows > 0, 'the filtered transcript rendered no rows');
      const strays = await js(
        `[...document.querySelectorAll('#seg-list .seg .who')].filter(w => w.dataset.sp !== '${target}').length`
      );
      assert(strays === 0, `${strays} rows in the filtered transcript belong to another voice`);
      const file = await shot('show-in-transcript');
      // Leave the UI where the later steps expect to find it.
      await js('document.querySelector(\'.rail-item[data-view="speakers"]\').click()');
      await waitFor('the speaker list again', async () => js('document.querySelectorAll("#speaker-list .sp-row").length > 0'));
      return { speaker: target, rows, file };
    });

    await step('shot-speakers', async () => ({ file: await shot('speakers') }));

    // 7 — inline rename through the real UI, then the retroactive broadcast
    await step('rename-propagates-in-place', async () => {
      // The voice the banner is asking about.
      const target = await js(`(() => {
        const b = document.querySelector('[data-onboard]');
        return b ? Number(b.dataset.onboard) : null;
      })()`);
      assert(target != null, 'no unnamed voice to rename');

      await js(`document.querySelector('.sp-row[data-speaker="${target}"] .sp-name').click()`);
      await waitFor('the rename input', async () => js('!!document.getElementById("rename-input")'));
      await js(`(() => {
        const i = document.getElementById('rename-input');
        i.value = 'Mara';
        i.dispatchEvent(new KeyboardEvent('keydown', {key: 'Enter', bubbles: true}));
        return true;
      })()`);
      await waitFor('the relabel broadcast', async () =>
        js(`window.__recallDebug.store.speakers.get(${target})?.name === 'Mara'`)
      );

      // Now the part that matters: a view that is ALREADY on screen must
      // relabel in place, without being remounted and without re-querying.
      await js('document.querySelector(\'.rail-item[data-view="transcript"]\').click()');
      await waitFor('transcript rows', async () => js('document.querySelectorAll("#seg-list .seg").length > 0'));
      const before = await js(`document.querySelectorAll('#seg-list [data-sp="${target}"] .nm').length`);
      assert(before > 0, 'the renamed voice has no rows in the transcript to check');
      const named = await js(
        `[...document.querySelectorAll('#seg-list [data-sp="${target}"] .nm')].every(e => e.textContent === 'Mara')`
      );
      assert(named, 'transcript rows still show the old label after a rename');

      // And again, live, from "another client": rename via the protocol while
      // the transcript is mounted and confirm the mounted DOM changes itself.
      await js(`window.recall.request('speakers.name', {id: ${target}, name: 'Mara Vex'})`);
      await waitFor('in-place relabel', async () =>
        js(`[...document.querySelectorAll('#seg-list [data-sp="${target}"] .nm')].every(e => e.textContent === 'Mara Vex')`)
      );
      return { speaker: target, rowsRelabelled: before };
    });

    await step('shot-transcript-renamed', async () => ({ file: await shot('transcript-renamed') }));

    // 8 — pause from the TRAY path stops the feed (DESIGN §8, the marquee case)
    await step('tray-pause-stops-feed', async () => {
      const before = (await js('window.__recallDebug.counts()')).rows;
      await deps.setPaused(true); // exactly what the tray menu item calls
      await waitFor('the UI to show paused', async () =>
        js('document.getElementById("pause-btn").dataset.paused === "true"')
      );
      const label = await js('document.getElementById("pause-label").textContent');
      const chip = await js('(document.getElementById("live-chip")||{}).textContent || ""');
      await sleep(6000); // three feed intervals
      const after = (await js('window.__recallDebug.counts()')).rows;
      assert(after === before, `feed kept running while paused (${before} → ${after})`);
      assert(label === 'Resume capture', `pause button did not flip its label (got "${label}")`);
      // The chip sits in the transcript header, where you are actually reading
      // when you wonder why nothing new is arriving — and it says which thing
      // is paused, in amber (0.5.5).
      assert(/capture paused/i.test(chip), `transcript still claims to be live (chip: "${chip}")`);
      const amber = await js('document.getElementById("live-chip").className');
      assert(/\bwarn\b/.test(amber), `the paused chip is not the amber one (class: "${amber}")`);
      return { before, after, label, chip, trayLine: deps.statusLine() };
    });

    await step('shot-paused', async () => ({ file: await shot('paused') }));

    // 9 — resume, and the feed comes back
    await step('resume-restarts-feed', async () => {
      const before = (await js('window.__recallDebug.counts()')).rows;
      await deps.setPaused(false);
      const after = await waitFor(
        'the feed to restart',
        async () => {
          const n = (await js('window.__recallDebug.counts()')).rows;
          return n > before ? n : null;
        },
        { timeout: 12000 }
      );
      return { before, after, trayLine: deps.statusLine() };
    });

    // 10 — the tray menu itself: it must exist and reflect state
    await step('tray-menu', async () => {
      const menu = deps.buildTrayMenu();
      const labels = menu.items.map((i) => i.label).filter(Boolean);
      assert(labels.some((l) => /Pause capture|Resume capture/.test(l)), `no pause item in the tray menu: ${labels}`);
      assert(labels.some((l) => /Open NX Recall/.test(l)), 'no "open" item in the tray menu');
      assert(labels.some((l) => /Quit NX Recall/.test(l)), 'no quit item in the tray menu');
      assert(labels.some((l) => /Capturing|Paused|offline/i.test(l)), 'the tray has no live status line');
      return { labels };
    });

    // 11 — search, facets, and jumping into transcript context
    await step('search-and-jump', async () => {
      await js('document.querySelector(\'.rail-item[data-view="search"]\').click()');
      await waitFor('the search box', async () => js('!!document.getElementById("search-q")'));
      await js(`(() => {
        document.getElementById('search-q').value = 'portal';
        document.getElementById('search-go').click();
        return true;
      })()`);
      const hits = await waitFor('search hits', async () => {
        const n = await js('document.querySelectorAll("#search-results .seg").length');
        return n > 0 ? n : null;
      });
      const facets = await js('document.querySelectorAll(".facets .facet").length');
      const file = await shot('search');
      await js('document.querySelector("#search-results .seg").click()');
      await waitFor('the transcript to take focus', async () => js('window.__recallDebug.view() === "transcript"'));
      const marked = await waitFor('the hit to be marked', async () => js('!!document.querySelector("#seg-list .seg.hit")'));
      return { hits, facets, marked, file };
    });

    await step('shot-search-jump', async () => ({ file: await shot('search-jump') }));

    // 12 — sources: default-deny visuals and a live toggle
    await step('sources-toggle', async () => {
      await js('document.querySelector(\'.rail-item[data-view="sources"]\').click()');
      await waitFor('the source list', async () => js('document.querySelectorAll("#source-list .src-row").length > 0'));
      const denied = await js('document.querySelectorAll("#source-list .src-row.denied").length');
      assert(denied > 0, 'no source renders as denied — default-deny has no visual');
      const key = await js('document.querySelector("#source-list .src-row.denied").dataset.source');
      await js(`document.querySelector('[data-toggle="${key}"]').click()`);
      await waitFor('the toggle to flip', async () =>
        js(`document.querySelector('[data-toggle="${key}"]').getAttribute('aria-pressed') === 'true'`)
      );
      const file = await shot('sources');
      await js(`document.querySelector('[data-toggle="${key}"]').click()`); // put it back
      return { deniedBefore: denied, toggled: key, file };
    });

    // 12b — the microphone. Off by default and NOT in the application list;
    // enabling it in follow mode has to read as "waiting", not as "recording",
    // because that difference is the whole privacy model (0.6.0).
    await step('mic-card-is-off-and-separate', async () => {
      await js('document.querySelector(\'.rail-item[data-view="sources"]\').click()');
      await waitFor('the microphone card', async () => js('!!document.getElementById("mic-card")'));

      // It sits above the application list, not in it.
      const order = await js(`(() => {
        const body = document.querySelector('.view-body');
        const cards = [...body.querySelectorAll('.card')];
        return cards.indexOf(document.getElementById('mic-card'));
      })()`);
      assert(order === 0, `the microphone card is not first in the view (index ${order})`);
      const inList = await js(`[...document.querySelectorAll('#source-list .src-row')].some(r => r.dataset.source === 'mic')`);
      assert(!inList, 'the microphone is listed as an application — it is not one');

      const mic = await js('window.__recallDebug.mic()');
      assert(mic.enabled === false, 'the microphone is not off by default');
      assert(mic.mode === 'follow', `the default mode is not follow (${mic.mode})`);
      assert(mic.chip === 'off', `the state chip reads "${mic.chip}"`);
      assert(mic.pressed === 'false', 'the toggle claims to be on');

      // The copy that has to be unmissable: it hears the ROOM.
      assert(/room/i.test(mic.warning), `the warning does not say what it hears: ${mic.warning}`);
      assert(/not the game/i.test(mic.warning), `the warning does not draw the contrast: ${mic.warning}`);
      return { warning: mic.warning.slice(0, 70), chip: mic.chip };
    });

    await step('mic-follow-mode-waits-then-captures', async () => {
      // Deny every application first, so "follow" has nothing to follow and
      // the waiting state is reachable rather than theoretical.
      const denied = await js(`(async () => {
        const keys = [...document.querySelectorAll('#source-list .src-row:not(.denied)')].map(r => r.dataset.source);
        for (const k of keys) await window.recall.request('sources.set', {match_key: k, allowed: false});
        return keys;
      })()`);

      await js('document.getElementById("mic-toggle").click()');
      const waiting = await waitFor(
        'the microphone to report waiting',
        async () => {
          const m = await js('window.__recallDebug.mic()');
          return m.enabled && m.state === 'following:idle' ? m : null;
        },
        { timeout: 10000, every: 150 }
      );
      assert(
        /waiting/i.test(waiting.chip),
        `enabled-but-idle must not read as recording (chip: "${waiting.chip}")`
      );
      assert(
        waiting.modePressed.find(([m]) => m === 'follow')?.[1] === 'true',
        `the mode picker does not show follow selected: ${JSON.stringify(waiting.modePressed)}`
      );
      const file = await shot('sources-mic-waiting');

      // Allowing an application is what starts the microphone. Nothing else.
      await js(`window.recall.request('sources.set', {match_key: ${JSON.stringify(denied[0])}, allowed: true})`);
      const capturing = await waitFor(
        'the microphone to start capturing',
        async () => {
          const m = await js('window.__recallDebug.mic()');
          return m.state === 'following:active' ? m : null;
        },
        { timeout: 12000, every: 150 }
      );
      assert(/capturing/i.test(capturing.chip), `the chip did not follow the state: "${capturing.chip}"`);
      return { denied, waiting: waiting.chip, capturing: capturing.chip, file };
    });

    // 12c — "You" renders distinctly, and it is a DIFFERENT treatment from
    // everyone else rather than merely a present one.
    await step('your-own-voice-renders-distinctly', async () => {
      await js('document.querySelector(\'.rail-item[data-view="transcript"]\').click()');
      const seen = await waitFor(
        'You rows in the transcript',
        async () => {
          const m = await js('window.__recallDebug.mic()');
          return m.youRows > 0 ? m : null;
        },
        { timeout: 20000, every: 250 }
      );
      assert(seen.otherRows > 0, 'every row is marked as the user — that is not a distinction');

      // The mark itself: a ring on the dot and a standing underline on the
      // name, neither of which an ordinary row carries.
      const style = await js(`(() => {
        const you = document.querySelector('#seg-list .seg.you .who .dot');
        const other = document.querySelector('#seg-list .seg:not(.you) .who .dot');
        const youName = document.querySelector('#seg-list .seg.you .who .nm');
        const otherName = document.querySelector('#seg-list .seg:not(.you) .who .nm');
        const cs = (e) => e ? getComputedStyle(e) : null;
        return {
          youDot: cs(you)?.boxShadow ?? '',
          otherDot: cs(other)?.boxShadow ?? '',
          youUnderline: cs(youName)?.borderBottomColor ?? '',
          otherUnderline: cs(otherName)?.borderBottomColor ?? '',
        };
      })()`);
      assert(style.youDot !== 'none' && style.youDot !== '', `the You dot carries no ring: ${JSON.stringify(style)}`);
      assert(style.youDot !== style.otherDot, 'the You dot looks exactly like everyone else');
      assert(style.youUnderline !== style.otherUnderline, 'the You name looks exactly like everyone else');

      // …and the layout did not move: same grid, same columns, no extra badge.
      const cols = await js(`(() => {
        const you = getComputedStyle(document.querySelector('#seg-list .seg.you')).gridTemplateColumns;
        const other = getComputedStyle(document.querySelector('#seg-list .seg:not(.you)')).gridTemplateColumns;
        return [you, other];
      })()`);
      assert(cols[0] === cols[1], `the You row uses a different layout: ${cols}`);

      const file = await shot('transcript-you');
      return { youRows: seen.youRows, otherRows: seen.otherRows, file };
    });

    await step('mic-off-stops-it', async () => {
      await js('document.querySelector(\'.rail-item[data-view="sources"]\').click()');
      await waitFor('the microphone card', async () => js('!!document.getElementById("mic-card")'));
      const before = (await js('window.__recallDebug.mic()')).state;
      assert(before === 'following:active', `expected an active microphone, saw ${before}`);

      await js('document.getElementById("mic-toggle").click()');
      const off = await waitFor(
        'the microphone to go off',
        async () => {
          const m = await js('window.__recallDebug.mic()');
          return m.state === 'off' ? m : null;
        },
        { timeout: 10000, every: 150 }
      );
      assert(off.chip === 'off', `the chip did not follow the switch: "${off.chip}"`);
      assert(off.pressed === 'false', 'the toggle still claims to be on');

      // No new rows of the user's own voice arrive after the switch.
      await js('document.querySelector(\'.rail-item[data-view="transcript"]\').click()');
      const stopped = (await js('window.__recallDebug.mic()')).youRows;
      await sleep(6000); // three feed intervals
      const after = (await js('window.__recallDebug.mic()')).youRows;
      assert(after === stopped, `the microphone kept producing rows while off (${stopped} → ${after})`);
      return { stopped, after };
    });

    // 12d — native widgets Chromium draws outside the page (a <select> option
    // popup above all) take their colours from `color-scheme` and from nothing
    // we can style. Without it the Search view's speaker dropdown is
    // light-on-light and unreadable.
    await step('native-widgets-render-dark', async () => {
      const scheme = await js('window.__recallDebug.colorScheme()');
      assert(/dark/.test(scheme), `the document declares color-scheme "${scheme}", so native popups render light`);

      await js('document.querySelector(\'.rail-item[data-view="search"]\').click()');
      await waitFor('the search facets', async () => js('!!document.getElementById("search-q")'));
      const sel = await js(`(() => {
        const s = document.getElementById('search-speaker');
        if (!s) return null;
        const cs = getComputedStyle(s);
        // The popup itself is drawn by the browser and cannot be inspected, so
        // check the rule that governs it instead of its pixels.
        let rule = '';
        for (const sheet of document.styleSheets) {
          let rules;
          try { rules = sheet.cssRules; } catch { continue; }
          for (const r of rules) {
            if (r.selectorText && /select\\.input option/.test(r.selectorText)) rule = r.cssText;
          }
        }
        return {
          scheme: cs.colorScheme,
          options: s.options.length,
          rule,
        };
      })()`);
      assert(sel, 'the search view has no speaker <select> to check');
      assert(/dark/.test(sel.scheme), `the select itself inherits "${sel.scheme}"`);
      assert(sel.options > 1, `the speaker dropdown has nothing in it (${sel.options})`);
      // Belt and braces behind the scheme, for platforms whose popup ignores it.
      assert(sel.rule, 'no explicit option colours are declared');
      assert(/background/.test(sel.rule) && /color/.test(sel.rule), `the option rule is incomplete: ${sel.rule}`);
      const file = await shot('search-select');
      await js('document.querySelector(\'.rail-item[data-view="transcript"]\').click()');
      return { ...sel, file };
    });

    // 13 — the footer is the status surface the design asks for
    await step('status-footer', async () => {
      const text = await js('document.getElementById("footer").textContent');
      for (const want of ['seq', 'queue', 'drops', 'connected']) {
        assert(text.includes(want), `footer is missing "${want}" (got: ${text})`);
      }
      return { text: text.slice(0, 120) };
    });

    // 14 — the daemon dies and comes back with a reset counter. This is the
    // case DESIGN §2 is written for: the client must notice, re-run its
    // queries, and end up showing the truth again — without being restarted.
    if (process.env.NX_RECALL_MOCK_PID) {
      await step('survives-daemon-restart', async () => {
        await js('document.querySelector(\'.rail-item[data-view="transcript"]\').click()');
        await waitFor('transcript rows', async () => js('document.querySelectorAll("#seg-list .seg").length > 0'));
        const before = (await js('window.__recallDebug.counts()')).rows;
        const seqBefore = deps.getUi().conn.seq;

        process.kill(Number(process.env.NX_RECALL_MOCK_PID), 'SIGUSR1');
        await waitFor('the connection to drop', async () => deps.getUi().conn.status !== 'connected', { timeout: 8000 });
        await waitFor('the reconnect', async () => deps.getUi().conn.status === 'connected', { timeout: 20000 });

        // The user is told, and the views are rebuilt from queries rather than
        // patched from a gap that no longer exists.
        const told = await waitFor(
          'the restart notice',
          async () => js('[...document.querySelectorAll(".toast")].some(t => /restarted/i.test(t.textContent))'),
          { timeout: 6000 }
        );
        const after = await waitFor(
          'the views to reload',
          async () => {
            const c = await js('window.__recallDebug.counts()');
            return c.rows > 0 && c.speakers > 0 ? c : null;
          },
          { timeout: 15000 }
        );
        assert(told, 'the user was never told the daemon restarted');

        // The live feed has to actually resume. This is the assertion that
        // caught the client holding its pre-restart sequence number and
        // discarding the restarted daemon's whole stream as duplicates.
        const resumed = await waitFor(
          'the feed after the restart',
          async () => {
            const n = (await js('window.__recallDebug.counts()')).rows;
            return n > after.rows ? n : null;
          },
          { timeout: 15000 }
        );
        return { before, after: after.rows, resumed, seqBefore, seqAfter: deps.getUi().conn.seq };
      });
      await step('shot-after-restart', async () => ({ file: await shot('after-restart') }));

      // 14b — the daemon that came back is a NEWER one (the hub replaced its
      // binary under it, 0.5.3). This window is now the old half of the app and
      // has to say so — once, above every view, with the one action that ends
      // the mismatch (0.5.5).
      await step('update-banner-and-dismiss', async () => {
        const bar = await waitFor(
          'the update banner',
          async () => {
            const u = await js('window.__recallDebug.update()');
            return u.shown ? u : null;
          },
          { timeout: 15000 }
        );
        const daemon = deps.getUi().conn.daemon;
        assert(bar.version === daemon, `the banner names ${bar.version}, the daemon says ${daemon}`);
        assert(bar.text.includes(String(daemon).split('/').pop()), `the banner does not name the new version: ${bar.text}`);
        assert(/restart/i.test(bar.text), `the banner does not say what to do about it: ${bar.text}`);

        // The button is NOT pressed: app.relaunch() would take the window out
        // from under this driver mid-run. Its wiring is read instead — the
        // handler on the button, and the channel that handler calls.
        assert(bar.restart.present, 'the banner offers no Restart button');
        assert(bar.restart.wired, 'the Restart button carries no handler');
        assert(bar.restart.ipc, 'there is no relaunch channel for it to call');
        const file = await shot('update-banner');

        // It is a notice, not a modal: it can be waved away.
        await js('document.getElementById("update-dismiss").click()');
        const gone = await waitFor('the banner to go away', async () => js('!window.__recallDebug.update().shown'));
        assert(gone, 'the update banner could not be dismissed');
        return { from: bar.text.slice(0, 60), version: bar.version, file };
      });
    }

    // 15 — the app lives in the tray: closing the window hides it, does not
    // quit, and does not take the pause switch away with it (DESIGN §8).
    await step('tray-works-with-window-closed', async () => {
      const w = win();
      w.close();
      await sleep(600);
      assert(!w.isDestroyed(), 'closing the window destroyed it — the app would have quit');
      assert(!w.isVisible(), 'the window is still visible after close');

      const labels = deps.buildTrayMenu().items.map((i) => i.label).filter(Boolean);
      assert(labels.some((l) => /Pause capture/.test(l)), 'the tray lost its pause item with the window closed');

      const paused = await deps.setPaused(true);
      assert(paused === true, 'pause from the tray failed with no window open');
      assert(/Paused/.test(deps.statusLine()), `tray status did not follow: ${deps.statusLine()}`);
      await deps.setPaused(false);

      deps.showWindow();
      await sleep(500);
      assert(win().isVisible(), 'the tray could not bring the window back');
      return { labels, statusLine: deps.statusLine() };
    });

    const passed = results.filter((r) => r.ok).length;
    const report = {
      when: new Date().toISOString(),
      socket: deps.getUi().conn.socketPath,
      passed,
      failed: results.length - passed,
      results,
    };
    mkdirSync(OUT, { recursive: true });
    writeFileSync(join(OUT, 'e2e-report.json'), JSON.stringify(report, null, 2));
    console.log(`[e2e] ${passed}/${results.length} steps passed`);
  }

  app.whenReady().then(() =>
    main()
      .catch((e) => console.log('[e2e] harness crashed —', e.stack || e.message))
      .finally(async () => {
        if (process.env.NX_RECALL_E2E_HOLD === '1') return; // keep the window up for a screenshot
        await sleep(400);
        deps.quit();
      })
  );
}
