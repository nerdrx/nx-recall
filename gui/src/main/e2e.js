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
      return { uncertain: n, qmarks: q, why: why.slice(0, 60) };
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
      assert(/paused/i.test(chip), `transcript still claims to be live (chip: "${chip}")`);
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
