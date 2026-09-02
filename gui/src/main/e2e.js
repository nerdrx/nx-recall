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
// NX Clear ships two grounds (DESIGN §14.1) and the suite photographs both, so
// one pass must not overwrite the other's artefacts. The harness sets this to
// "-light" / "-dark"; run by hand it is empty and nothing is renamed.
const SUFFIX = process.env.NX_RECALL_E2E_SUFFIX || '';

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
    const file = join(OUT, `${String(++shots).padStart(2, '0')}-${name}${SUFFIX}.png`);
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

    // 1b — which of NX Clear's two grounds this pass is photographing.
    //
    // The theme is driven through nativeTheme in the main process, which is the
    // same path an OS switch takes, so the first thing to establish is that the
    // renderer really followed it — otherwise a "dark" pass would quietly
    // photograph the light one. If it did not follow, the explicit [data-theme]
    // stamp §14.1 prescribes is applied, so the pass stays honest either way.
    await step('theme-pass', async () => {
      const asked = deps.theme?.().forced ?? null;
      const read = () =>
        js(`(() => {
          const root = getComputedStyle(document.documentElement);
          return {
            scheme: root.colorScheme,
            ground: getComputedStyle(document.body).backgroundColor,
            stamp: document.documentElement.getAttribute('data-theme'),
          };
        })()`);
      let got = await read();
      let stamped = false;
      if (asked && !got.scheme.includes(asked)) {
        await js(`document.documentElement.setAttribute('data-theme', ${JSON.stringify(asked)})`);
        got = await read();
        stamped = true;
      }
      if (asked) {
        assert(
          got.scheme.includes(asked),
          `this pass asked for the ${asked} ground; the document reports color-scheme "${got.scheme}"`
        );
      }
      return { asked, stamped, ...got, window: deps.theme?.().ground ?? null };
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
    //
    // Counted with `appended`, not with rows. 0.7.4's window is bounded while
    // following the tail and the mock now carries fourteen evenings behind the
    // canned rows, so the list opens FULL: it can be moving and still be 600
    // rows long. `appended` is the monotone count of live segments this client
    // has folded in, which is the thing this step is actually about.
    await step('live-feed-appends', async () => {
      const before = (await js('window.__recallDebug.counts()')).appended;
      const after = await waitFor(
        'a new segment',
        async () => {
          const c = await js('window.__recallDebug.counts()');
          return c.appended > before ? c.appended : null;
        },
        { timeout: 12000 }
      );
      const w = await js('window.__recallDebug.scrollback()');
      assert(w.rows > 0, 'the feed appended but nothing is on screen');
      return { before, after, rows: w.rows, following: w.following };
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

    // 4b — 0.6.1: a label inherited from the turns around it. It has a name and
    // no score, and it must read as a guess with a reason rather than as a
    // measurement — the "?" is what carries that.
    await step('inherited-labels-read-as-uncertain', async () => {
      const p = await waitFor('a proximity-labelled row', async () => {
        const p = await js('window.__recallDebug.proximity()');
        return p.rows > 0 ? p : null;
      });
      assert(
        p.uncertain === p.rows,
        `${p.rows - p.uncertain} inherited row(s) render as certain`
      );
      assert(/surrounding turn/i.test(p.why), `the "?" does not explain the inheritance: ${p.why}`);
      return p;
    });

    // 4d — 0.8.0: a second decoder read the same seconds and disagreed. That is
    // a different doubt from every other one on a row — the "?" is about who
    // spoke and which model wrote it down, both of which the pipeline will
    // defend — so it gets its own mark, and the mark has to say the one plain
    // thing there is to say about it.
    await step('shaky-rows-carry-the-cross-check-mark', async () => {
      const a = await waitFor('a shaky row', async () => {
        const a = await js('window.__recallDebug.accuracy()');
        return a.shakyRows > 0 ? a : null;
      });
      assert(a.shakyMarks === a.shakyRows, `${a.shakyRows} shaky rows wear ${a.shakyMarks} marks`);
      // A badge on every row is not a badge: rows the cross-check AGREED with
      // must carry nothing at all.
      assert(a.solidMarks === 0, `${a.solidMarks} rows the cross-check agreed with are wearing the mark`);
      assert(
        /second decoder disagreed/i.test(a.tip),
        `the mark does not explain itself: "${a.tip}"`
      );
      // …and the words themselves read as unsettled, the same way an uncertain
      // row's do. The class is not enough — the treatment has to be visible.
      const muted = await js(`(() => {
        const row = document.querySelector('#seg-list .seg.shaky');
        const other = document.querySelector('#seg-list .seg:not(.shaky):not(.uncertain)');
        const ink = (r) => r && getComputedStyle(r.querySelector('.txt')).color;
        return { shaky: ink(row), plain: ink(other), style: getComputedStyle(row.querySelector('.txt')).fontStyle };
      })()`);
      assert(muted.shaky !== muted.plain, `a shaky row's words are painted like a settled one (${muted.shaky})`);
      assert(muted.style === 'italic', `a shaky row's words are not set apart (font-style: ${muted.style})`);
      return { rows: a.shakyRows, tip: a.tip, ...muted };
    });

    await step('shot-transcript', async () => ({ file: await shot('transcript') }));

    // -----------------------------------------------------------------------
    // 4c — 0.7.4: the transcript is the whole archive, not the last 600 rows.
    //
    // The mock carries fourteen evenings behind its canned rows, so the window
    // opens FULL and every one of these steps is really paging: scroll up, get
    // the page before, keep going until the beginning, and never lose your
    // place doing it.
    // -----------------------------------------------------------------------

    await step('scrolling-up-loads-the-page-before', async () => {
      const before = await js('window.__recallDebug.scrollback()');
      assert(before.following, 'the transcript did not open on the live tail');
      assert(before.rows > 0, 'nothing rendered to scroll up from');
      assert(!before.beginning, 'the mock fixture is smaller than one window — nothing to page');
      assert(before.datePicker, 'the transcript header has no date picker');

      // Where the row at the top of the viewport sits right now. Anchoring is
      // the claim being tested, and the only honest way to check it is to look
      // at a real element's position on a real screen before and after.
      const topOf = (id) =>
        js(`(() => {
          const r = document.querySelector('#seg-list .seg[data-seg="${id}"]');
          return r ? Math.round(r.getBoundingClientRect().top) : null;
        })()`);

      // A real scroll, not a call into the view: the whole feature is that
      // this happens because you scrolled.
      await js('document.getElementById("transcript-body").scrollTop = 0');
      const after = await waitFor(
        'an older page',
        async () => {
          const w = await js('window.__recallDebug.scrollback()');
          return w.rows > before.rows ? w : null;
        },
        { timeout: 20000 }
      );

      assert(!after.following, 'scrolling up into history left the Follow button on');
      assert(after.pressed === 'false', `the button says "${after.pressed}" while reading history`);
      assert(after.rows === after.unique, `the prepended page duplicated rows (${after.rows} rows, ${after.unique} ids)`);
      assert(after.ordered, 'the window came back out of time order');
      assert(after.firstId !== before.firstId, 'the first row did not change — nothing was prepended');
      assert(after.firstMs < before.firstMs, 'the page that arrived was not OLDER');
      assert(after.segments === after.rows, `${after.segments} segments in the model, ${after.rows} rows on screen`);
      return {
        rows: `${before.rows} → ${after.rows}`,
        segments: after.segments,
        page: after.rows - before.rows,
        anchored: (await topOf(before.firstId)) != null,
      };
    });

    await step('a-prepend-does-not-move-what-you-are-reading', async () => {
      // Park a known row in the middle of the viewport, note where it is, load
      // another page above it, and assert it did not move. This is the whole
      // scroll-anchoring contract in one measurement.
      const mark = await js(`(() => {
        const rows = [...document.querySelectorAll('#seg-list .seg')];
        const mid = rows[Math.min(40, rows.length - 1)];
        mid.scrollIntoView({ block: 'center' });
        return { id: Number(mid.dataset.seg) };
      })()`);
      await sleep(600);
      const at = (id) =>
        js(`Math.round(document.querySelector('#seg-list .seg[data-seg="${id}"]').getBoundingClientRect().top)`);
      const before = await at(mark.id);
      const rowsBefore = (await js('window.__recallDebug.scrollback()')).rows;

      await js('window.__recallDebug.loadOlder()');
      const grew = await waitFor(
        'the page to land',
        async () => {
          const w = await js('window.__recallDebug.scrollback()');
          return w.rows > rowsBefore ? w : null;
        },
        { timeout: 20000 }
      );
      const after = await at(mark.id);
      assert(
        Math.abs(after - before) <= 4,
        `the row being read jumped ${after - before}px when a page was prepended above it`
      );
      return { id: mark.id, top: `${before} → ${after}`, rows: `${rowsBefore} → ${grew.rows}` };
    });

    await step('the-seams-between-pages-are-clean', async () => {
      const w = await js('window.__recallDebug.scrollback()');
      // Several evenings are loaded by now, so there is more than one seam and
      // more than one day boundary to have got wrong.
      assert(w.daySeps >= 2, `only ${w.daySeps} day separator(s) across ${w.rows} rows of several evenings`);
      assert(w.adjacentSeps === 0, 'two day separators ended up back to back — a seam was not recomputed');
      assert(
        new Set(w.dayLabels).size === w.dayLabels.length,
        `the same day is announced twice: ${JSON.stringify(w.dayLabels)}`
      );
      assert(w.threadSeps > 0, 'conversation boundaries stopped rendering once pages were prepended');
      // The one structural rule: nothing but a separator may sit above the
      // first row, and the first row must have a day header.
      const leading = await js(`(() => {
        const first = document.querySelector('#seg-list .seg');
        const out = [];
        for (let n = first.previousElementSibling; n; n = n.previousElementSibling) out.unshift(n.className);
        return out;
      })()`);
      assert(leading.length === 1 && /day-sep/.test(leading[0]), `above the first row: ${JSON.stringify(leading)}`);
      return { days: w.daySeps, threads: w.threadSeps, labels: w.dayLabels.slice(0, 4) };
    });

    await step('paging-back-reaches-the-beginning-and-says-so', async () => {
      let w = await js('window.__recallDebug.scrollback()');
      for (let i = 0; i < 12 && !w.beginning; i += 1) {
        await js('window.__recallDebug.loadOlder()');
        await sleep(400);
        w = await js('window.__recallDebug.scrollback()');
      }
      assert(w.beginning, `never reached the beginning (${w.segments} segments loaded)`);
      assert(w.beginMark.shown, 'the beginning was reached but nothing says so');
      assert(/beginning/i.test(w.beginMark.text), `the marker does not say what it is: "${w.beginMark.text}"`);
      assert(
        /\d{4}-\d{2}-\d{2}|today|yesterday/i.test(w.beginMark.text),
        `the marker does not say WHEN the first thing was captured: "${w.beginMark.text}"`
      );
      assert(w.rows === w.unique, 'the full archive contains duplicate rows');
      assert(w.ordered, 'the full archive came out of order');
      assert(!w.note.shown, 'the 20k ceiling notice is up on a fixture nowhere near it');

      // Paging again at the beginning is a no-op. Measured at the OLD end:
      // the live feed is still appending at the new one, which is correct and
      // has nothing to do with whether another page came back.
      const first = w.firstId;
      const firstMs = w.firstMs;
      await js('window.__recallDebug.loadOlder()');
      await sleep(500);
      const again = await js('window.__recallDebug.scrollback()');
      assert(again.firstId === first, `paging past the beginning changed the first row (${first} → ${again.firstId})`);
      assert(again.firstMs === firstMs, 'paging past the beginning reached further back than the beginning');
      assert(again.beginning, 'the beginning stopped being the beginning');

      await js('document.getElementById("transcript-body").scrollTop = 0');
      await sleep(300);
      return { segments: w.segments, marker: w.beginMark.text, days: w.daySeps };
    });

    await step('shot-scrollback-beginning', async () => ({ file: await shot('scrollback-beginning') }));

    await step('the-date-picker-jumps-to-a-day', async () => {
      // The day of the oldest thing in the archive — which is loaded right
      // now, so the expected answer is knowable rather than guessed.
      const day = await js(`(() => {
        const t = window.__recallDebug.store.segments[0].t_ms;
        const d = new Date(t);
        const p = (n) => String(n).padStart(2, '0');
        return \`\${d.getFullYear()}-\${p(d.getMonth() + 1)}-\${p(d.getDate())}\`;
      })()`);

      // Through the real control, with the real event a date input fires.
      await js(`(() => {
        const i = document.getElementById('transcript-date');
        i.value = ${JSON.stringify(day)};
        i.dispatchEvent(new Event('change', { bubbles: true }));
        return true;
      })()`);
      const w = await waitFor(
        'the day to load',
        async () => {
          const w = await js('window.__recallDebug.scrollback()');
          return w.detached ? w : null;
        },
        { timeout: 15000 }
      );

      assert(!w.following, 'jumping to a date left the view following the live tail');
      assert(w.pressed === 'false', 'the Follow button did not follow the jump');
      assert(w.rows > 0, `nothing rendered for ${day}`);
      assert(w.daySeps === 1, `${w.daySeps} day separators for a single day`);
      const allOnTheDay = await js(`(() => {
        const p = (n) => String(n).padStart(2, '0');
        const fmt = (t) => { const d = new Date(t); return \`\${d.getFullYear()}-\${p(d.getMonth() + 1)}-\${p(d.getDate())}\`; };
        return window.__recallDebug.store.segments.every((s) => fmt(s.t_ms) === ${JSON.stringify(day)});
      })()`);
      assert(allOnTheDay, `the window holds rows from outside ${day}`);
      return { day, rows: w.rows, file: await shot('date-jump') };
    });

    await step('follow-collapses-the-window-and-the-feed-keeps-appending', async () => {
      const before = await js('window.__recallDebug.scrollback()');
      const appended = (await js('window.__recallDebug.counts()')).appended;

      await js('document.getElementById("follow-btn").click()');
      const w = await waitFor(
        'the collapse back to the tail',
        async () => {
          const w = await js('window.__recallDebug.scrollback()');
          return w.following && !w.detached ? w : null;
        },
        { timeout: 15000 }
      );

      assert(w.pressed === 'true', 'the button did not go back to Following');
      assert(w.segments <= 600, `the window did not collapse: ${w.segments} segments resident`);
      assert(w.rows === w.segments, `${w.rows} rows on screen for ${w.segments} segments`);
      assert(w.lastId !== before.lastId, 'Follow did not come back to the live tail from another day');
      // At the bottom, and staying there as rows arrive.
      assert(
        w.scrollHeight - w.scrollTop - w.clientHeight < 120,
        `Follow did not land at the bottom (${w.scrollHeight - w.scrollTop - w.clientHeight}px away)`
      );
      const after = await waitFor(
        'the live feed, still running',
        async () => {
          const c = await js('window.__recallDebug.counts()');
          return c.appended > appended ? c.appended : null;
        },
        { timeout: 15000 }
      );
      const end = await js('window.__recallDebug.scrollback()');
      assert(end.segments <= 600, `the window grew past its bound again: ${end.segments}`);
      assert(end.rows === end.unique, 'appending after a collapse duplicated rows');
      return { collapsedTo: w.segments, appended: `${appended} → ${after}` };
    });

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

    // 5a2 — 0.8.0, "fix this". The correct path has been in this sheet since
    // 0.4 and almost nobody used it, because it was a textarea below a heading
    // below a picker. Now the words themselves are the control: click them,
    // type, Enter. This drives exactly that motion and then proves the three
    // things that have to be true afterwards — the daemon has it, the row says
    // so, and Escape puts a change back rather than saving it.
    await step('fixing-a-transcript-is-one-motion', async () => {
      const target = await js(`(() => {
        const rows = [...document.querySelectorAll('#seg-list .seg')];
        const row = rows[rows.length - 1];
        row.click();
        return { id: Number(row.dataset.seg) };
      })()`);
      await waitFor('the sheet', async () => js('!!document.querySelector(".sheet #segment-text")'));

      const rest = await js('window.__recallDebug.accuracy()');
      assert(!rest.sheet.editing, 'the sheet opened already in an editor');
      assert(/click the words/i.test(rest.sheet.hint), `no invitation to fix: "${rest.sheet.hint}"`);

      // Escape first, so a passing "it saved" can never be a false positive
      // from an editor that saves whatever it is holding.
      const fixed = `corrected by the driver ${Date.now()}`;
      await js(`(() => {
        document.getElementById('segment-text').click();
        const ta = document.getElementById('correct-text');
        ta.value = 'THIS MUST NOT BE SAVED';
        ta.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
        return true;
      })()`);
      const reverted = await js('window.__recallDebug.accuracy()');
      assert(!reverted.sheet.editing, 'Escape left the editor open');
      assert(
        !/MUST NOT BE SAVED/.test(reverted.sheet.reader),
        `Escape kept the abandoned edit: "${reverted.sheet.reader}"`
      );
      // …and the sheet is still open. Escape belongs to the nearer thing.
      assert(await js('!!document.querySelector(".sheet")'), 'Escape in the editor closed the whole sheet');

      await js(`(() => {
        document.getElementById('segment-text').click();
        const ta = document.getElementById('correct-text');
        ta.value = ${JSON.stringify(fixed)};
        ta.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));
        return true;
      })()`);
      const saved = await waitFor('the fix to land', async () => {
        const a = await js('window.__recallDebug.accuracy()');
        return a.sheet.reader === fixed && !a.sheet.editing ? a : null;
      });
      // The daemon really has it — not just this window.
      const onWire = await js(`(async () => {
        const r = await window.recall.request('transcript', { limit: 600 });
        return (r.data.segments.find(s => s.id === ${target.id}) || {}).text ?? null;
      })()`);
      assert(onWire === fixed, `the daemon still says ${JSON.stringify(onWire)}`);

      const file = await shot('segment-fix');
      await js('document.querySelector(".scrim").dispatchEvent(new MouseEvent("mousedown", {bubbles: true}))');
      await waitFor('the sheet to close', async () => js('!document.querySelector(".sheet")'));

      // …and the row wears the mark, through the broadcast rather than through
      // an optimistic write in this window.
      const marked = await waitFor(
        'the corrected mark on the row',
        async () => js(`!!document.querySelector('#seg-list .seg.corrected[data-seg="${target.id}"]')`),
        { timeout: 10000 }
      );
      assert(marked, 'a fixed row does not say it was fixed');
      return { segment: target.id, hint: rest.sheet.hint, text: saved.sheet.reader.slice(0, 24), file };
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

    // 6g — 0.6.1: a voice can be told which languages it speaks, and the
    // setting persists through the daemon rather than being a local opinion.
    await step('speaker-languages-persist', async () => {
      const target = await js('Number(document.querySelector("#speaker-list .sp-row").dataset.speaker)');
      const before = await js(`window.__recallDebug.speaker(${target})`);
      assert(before.chip, 'the row carries no language chip');

      // Both entry points exist: the chip on the row, and the ⋯ menu.
      await js(`document.querySelector('.sp-row[data-speaker="${target}"] .chip.lang').click()`);
      await waitFor('the language sheet', async () => js('!!document.getElementById("language-control")'));
      const options = await js(
        '[...document.querySelectorAll("#language-control .seg-opt")].map(b => b.textContent)'
      );
      for (const want of ['Any', 'German', 'English', 'German + English']) {
        assert(options.includes(want), `"${want}" is not offered: ${JSON.stringify(options)}`);
      }
      const file = await shot('speaker-languages');
      await js('document.querySelector(\'#language-control [data-lang="de"]\').click()');
      await waitFor('the sheet to close', async () => js('!document.querySelector(".sheet")'));

      // The daemon is what makes it true: the model updates from the relabel
      // broadcast, and a fresh speakers.list agrees.
      await waitFor('the model to carry the language', async () => {
        const s = await js(`window.__recallDebug.speaker(${target})`);
        return s.languages && s.languages.join(',') === 'de' ? s : null;
      });
      const listed = await js(`(async () => {
        const r = await window.recall.request('speakers.list');
        const row = r.data.speakers.find(s => s.id === ${target});
        return row ? row.languages : null;
      })()`);
      assert(
        Array.isArray(listed) && listed.join(',') === 'de',
        `speakers.list does not report the language: ${JSON.stringify(listed)}`
      );
      const after = await js(`window.__recallDebug.speaker(${target})`);
      assert(/german/i.test(after.chip), `the row chip did not follow: ${after.chip}`);

      // The ⋯ menu reaches the same sheet.
      await js(`document.querySelector('.sp-row[data-speaker="${target}"] [data-more]').click()`);
      await waitFor('the row menu', async () => js('!!document.querySelector(".row-menu")'));
      const items = await js('[...document.querySelectorAll(".row-menu .menu-item")].map(b => b.textContent)');
      assert(items.includes('Languages…'), `the ⋯ menu lost its language item: ${JSON.stringify(items)}`);
      await js('document.dispatchEvent(new KeyboardEvent("keydown", {key: "Escape", bubbles: true}))');
      await waitFor('the menu to close', async () => js('!document.querySelector(".row-menu")'));
      return { speaker: target, options, listed, chip: after.chip, file };
    });

    // 6h — 0.6.4, the reported bug: a voice at "0 segments · 0s" whose Delete
    // did nothing, repeatedly, because delete-by-speaker only ever scoped
    // SEGMENTS and there were none — while the voiceprint behind it stayed live
    // and went on matching. This drives the whole fix through the real UI: the
    // row says it is empty, the sweep counts it, the sheet offers DESIGN §8's
    // choice, and the destructive half actually removes the voice.
    await step('delete-a-voice-and-choose-what-goes', async () => {
      const openDelete = async (spId) => {
        await js(`document.querySelector('.sp-row[data-speaker="${spId}"] [data-more]').click()`);
        await waitFor('the row menu', async () => js('!!document.querySelector(".row-menu")'));
        await js('[...document.querySelectorAll(".row-menu .menu-item")].find(b => b.textContent === "Delete").click()');
        await waitFor('the delete sheet', async () => js('!!document.querySelector(".sheet .actions.stacked")'));
        return js(`(() => {
          const sheet = document.querySelector('.sheet');
          return {
            title: sheet.querySelector('h2').textContent,
            body: sheet.querySelector('.sub').textContent,
            detail: (sheet.querySelector('.quote') || {}).textContent ?? '',
            choices: [...sheet.querySelectorAll('.actions .btn')].map(b => [b.dataset.choice, b.textContent, b.className]),
          };
        })()`);
      };

      // A voice that still has conversations: both halves of the choice, and
      // only the one that takes the voiceprint is styled as destructive.
      const full = await js(`Number([...document.querySelectorAll('#speaker-list .sp-row')]
        .find(r => !r.querySelector('[data-empty]')).dataset.speaker)`);
      const rich = await openDelete(full);
      assert(
        rich.choices.map((c) => c[0]).join(',') === 'keep,all,cancel',
        `the sheet does not offer the §8 choice: ${JSON.stringify(rich.choices)}`
      );
      assert(/keep the voice/i.test(rich.choices[0][1]), `the keep action does not say so: ${rich.choices[0][1]}`);
      assert(/voiceprint/i.test(rich.choices[1][1]), `the nuke action does not say so: ${rich.choices[1][1]}`);
      assert(!/danger/.test(rich.choices[0][2]), 'keeping the voice is styled as destructive');
      assert(/danger/.test(rich.choices[1][2]), 'deleting the voiceprint is not styled as destructive');
      assert(/segments/.test(rich.detail), `the sheet does not state the real scope: ${rich.detail}`);
      // Enter must never be one keystroke from taking the voiceprint.
      const richFocus = await js('(document.activeElement.dataset || {}).choice ?? ""');
      assert(richFocus === 'keep', `the keyboard lands on "${richFocus}", not the safe action`);
      const fileChoice = await shot('delete-choice');
      await js('document.querySelector(\'.sheet [data-choice="cancel"]\').click()');
      await waitFor('the sheet to close', async () => js('!document.querySelector(".sheet")'));

      // The empty voice: the row says so plainly, rather than reading as broken.
      const empty = await js(
        'Number((document.querySelector("#speaker-list .sp-row [data-empty]") || {dataset:{empty:0}}).dataset.empty)'
      );
      assert(empty > 0, 'the fixture has no 0-segment voice, so the reported bug is not reachable');
      const note = await js(`document.querySelector('[data-empty="${empty}"]').textContent`);
      assert(/no conversations left/i.test(note), `an empty voice reads as broken, not prunable: "${note}"`);

      // …and the sweep counts it too — same rule the daemon applies, checked
      // against the daemon rather than against the view's own opinion.
      const sweepable = await js(
        "(async () => (await window.recall.request('speakers.prune', {apply: false})).data.voices.map(v => v.id))()"
      );
      assert(sweepable.includes(empty), `the sweep does not see the empty voice: ${JSON.stringify(sweepable)}`);

      const ghost = await openDelete(empty);
      assert(
        ghost.choices.map((c) => c[0]).join(',') === 'all,cancel',
        `an empty voice is offered a choice it does not have: ${JSON.stringify(ghost.choices)}`
      );
      assert(
        /no conversations left/i.test(ghost.detail) && /voiceprint/i.test(ghost.detail),
        `the copy does not degrade honestly: "${ghost.detail}"`
      );
      const ghostFocus = await js('(document.activeElement.dataset || {}).choice ?? ""');
      assert(ghostFocus === 'cancel', `the only action is destructive and the keyboard is on it: "${ghostFocus}"`);
      const fileEmpty = await shot('delete-empty-voice');

      // The act itself — the one that used to do nothing at all.
      await js('document.querySelector(\'.sheet [data-choice="all"]\').click()');
      await waitFor('the empty voice to go from the list', async () =>
        js(`!document.querySelector('.sp-row[data-speaker="${empty}"]')`)
      );
      const listed = await js(
        "(async () => (await window.recall.request('speakers.list')).data.speakers.map(s => s.id))()"
      );
      assert(!listed.includes(empty), `the daemon still has the voice: ${JSON.stringify(listed)}`);
      return { full, empty, choices: rich.choices.map((c) => c[1]), files: [fileChoice, fileEmpty] };
    });

    // 6i — 0.6.1: voices that are not people. The affordance only exists when
    // there is something to sweep, and it says how many.
    await step('sweep-one-off-voices', async () => {
      const before = await js('window.__recallDebug.sweep()');
      assert(before.present, 'the mock has one-off voices but the sweep affordance is absent');
      assert(/\(\d+\)/.test(before.label), `the affordance does not say how many: ${before.label}`);
      const rows = await js('document.querySelectorAll("#speaker-list .sp-row").length');

      await js('document.getElementById("sweep-voices").click()');
      await waitFor('the confirm sheet', async () => js('!!document.querySelector(".sheet .quote")'));
      const detail = await js('document.querySelector(".sheet .quote").textContent');
      assert(/segment/.test(detail), `the confirm does not list what would go: ${detail}`);
      const body = await js('document.querySelector(".sheet .sub").textContent');
      assert(/named/i.test(body), `the confirm does not say what is protected: ${body}`);
      const file = await shot('sweep-confirm');

      await js('[...document.querySelectorAll(".sheet .actions .btn")].find(b => /Sweep/.test(b.textContent)).click()');
      await waitFor('the voices to go', async () => {
        const n = await js('document.querySelectorAll("#speaker-list .sp-row").length');
        return n < rows ? n : null;
      });
      const after = await waitFor('the affordance to retire', async () => {
        const s = await js('window.__recallDebug.sweep()');
        return s.present === false ? s : null;
      });
      return { before: before.label, rowsBefore: rows, after, file };
    });

    await step('shot-speakers', async () => ({ file: await shot('speakers') }));

    // 6j — 0.6.2, the memory graph's first surface. A voice is a person: the
    // page has to say who they talk to, and each of those has to be a door.
    await step('person-page-from-a-voice', async () => {
      // Both entry points exist. The ⋯ menu's first item is the deliberate
      // one; the row's counts are the one you find by accident.
      const target = await js('Number(document.querySelector("#speaker-list .sp-row").dataset.speaker)');
      const stats = await js(`document.querySelectorAll('.sp-row[data-speaker="${target}"] [data-stats]').length`);
      assert(stats === 2, `the row's counts are not a way in (${stats} clickable)`);

      await js(`document.querySelector('.sp-row[data-speaker="${target}"] [data-more]').click()`);
      await waitFor('the row menu', async () => js('!!document.querySelector(".row-menu")'));
      const items = await js('[...document.querySelectorAll(".row-menu .menu-item")].map(b => b.textContent)');
      assert(items[0] === 'Person page', `"Person page" is not the first menu item: ${JSON.stringify(items)}`);
      await js('document.querySelector(".row-menu .menu-item").click()');

      const p = await waitFor(
        'the person page to fill in',
        async () => {
          const p = await js('window.__recallDebug.person()');
          return p.mounted && p.strip.length ? p : null;
        },
        { timeout: 15000 }
      );
      assert(p.id === target, `the page is about ${p.id}, not ${target}`);
      assert(p.back, 'the person page has no way back');
      const keys = p.strip.map(([k]) => k);
      for (const want of ['total-speech', 'segments', 'sessions', 'conversations', 'first-heard', 'last-heard']) {
        assert(keys.includes(want), `the stat strip is missing "${want}": ${JSON.stringify(keys)}`);
      }
      assert(
        p.strip.every(([, v]) => v && v.trim().length > 0),
        `a stat rendered no value: ${JSON.stringify(p.strip)}`
      );
      assert(/conversation/.test(p.sub), `the page's subtitle does not summarise it: "${p.sub}"`);
      // The rail has five items (Memory joined in 0.7.0) and none of them is
      // selected here: the person page is pushed state, not a place in the app.
      const rail = await js('document.querySelectorAll(".rail-item").length');
      const selected = await js('document.querySelectorAll(\'.rail-item[aria-selected="true"]\').length');
      assert(rail === 5, `the rail has ${rail} items, not the five it should`);
      assert(selected === 0, 'a rail item claims to be selected on the person page');
      return { speaker: target, strip: p.strip, sub: p.sub, file: await shot('person-page') };
    });

    // 6k — the edges. "People they talk with" is the claim the graph exists to
    // make, and every one of them has to lead to their own page.
    await step('person-edges-lead-to-another-person', async () => {
      const p = await waitFor('edges to render', async () => {
        const p = await js('window.__recallDebug.person()');
        return p.edges.length ? p : null;
      });
      assert(p.edges.every((e) => e.name), `an edge rendered without a name: ${JSON.stringify(p.edges)}`);
      const counts = await js(
        '[...document.querySelectorAll("#edge-list .edge-row .edge-num")].map(e => e.textContent).slice(0, 3)'
      );
      assert(
        counts.some((c) => /conversation/.test(c)),
        `an edge does not say how many conversations: ${JSON.stringify(counts)}`
      );

      const first = p.edges[0];
      await js(`document.querySelector('#edge-list [data-edge="${first.id}"]').click()`);
      const next = await waitFor(
        'the second person page',
        async () => {
          const q = await js('window.__recallDebug.person()');
          return q.mounted && q.id === first.id && q.strip.length ? q : null;
        },
        { timeout: 15000 }
      );
      assert(next.id !== p.id, 'clicking an edge stayed on the same person');
      return { from: p.id, to: next.id, edges: p.edges.length, file: await shot('person-edges') };
    });

    // 6l — a conversation on the page lands in the TRANSCRIPT, unfiltered, with
    // its own span marked. Unfiltered is the point: you opened a conversation
    // to read what everybody said.
    await step('a-conversation-opens-in-the-transcript', async () => {
      // Back to a person with conversations to open.
      const p = await waitFor('recent conversations', async () => {
        const p = await js('window.__recallDebug.person()');
        return p.threads.length ? p : null;
      });
      const thread = p.threads[0];
      assert(thread.names, 'a conversation row names nobody');
      assert(thread.preview, 'a conversation row has no preview line');
      // The card sits under the edges, so a shot of the top of the page would
      // not document it.
      await js('document.getElementById("person-threads").scrollIntoView({block: "end"})');
      const listShot = await shot('person-conversations');

      await js(`document.querySelector('#thread-list [data-thread="${thread.id}"]').click()`);
      await waitFor('the transcript to mount', async () => js('window.__recallDebug.view() === "transcript"'));
      const t = await waitFor(
        'the conversation to be marked',
        async () => {
          const t = await js('window.__recallDebug.threads()');
          return t.marked > 0 ? t : null;
        },
        { timeout: 15000 }
      );
      assert(t.filter === '', `the speaker filter was left set to "${t.filter}" — a thread is not one voice`);
      assert(t.rows > t.marked, 'the whole transcript is marked — that is not a span');
      assert(
        String(t.markedThread) === String(thread.id),
        `the marked span is thread ${t.markedThread}, not ${thread.id}`
      );
      return { thread: thread.id, marked: t.marked, rows: t.rows, listShot, file: await shot('transcript-thread') };
    });

    // 6m — the boundary itself, in the transcript: a hairline naming who is in
    // the conversation that just started.
    await step('transcript-separates-conversations', async () => {
      const t = await waitFor(
        'thread separators',
        async () => {
          const t = await js('window.__recallDebug.threads()');
          return t.separators.length ? t : null;
        },
        { timeout: 15000 }
      );
      assert(t.separators.length > 1, `only ${t.separators.length} conversation boundary in the whole page`);
      assert(
        t.separators.some((s) => s.trim().length > 2),
        `a separator names nobody: ${JSON.stringify(t.separators)}`
      );
      // Back to the speakers list, where the later steps expect to be.
      await js('document.querySelector(\'.rail-item[data-view="speakers"]\').click()');
      await waitFor('the speaker list again', async () => js('document.querySelectorAll("#speaker-list .sp-row").length > 0'));
      return { separators: t.separators.slice(0, 3) };
    });

    // 6m2 — audit finding #18: the person page was frozen at mount. Everything
    // on it is a query over live segments (docs/GRAPH.md) — totals, "last
    // heard", the edges, the recent conversations — so a page left open while
    // that person keeps talking has to follow them rather than quietly claim
    // they were last heard whenever the page happened to be opened.
    await step('the-person-page-follows-the-live-feed', async () => {
      // A voice the feed is actually using, read off the live transcript
      // rather than assumed: earlier steps delete and merge the fixtures.
      const target = await waitFor('a voice the live feed keeps using', async () =>
        js(`(() => {
          const d = window.__recallDebug;
          const tally = new Map();
          for (const s of d.store.segments.slice(-80)) {
            if (s.speaker == null || !d.store.speakers.has(s.speaker)) continue;
            tally.set(s.speaker, (tally.get(s.speaker) ?? 0) + 1);
          }
          const best = [...tally.entries()].sort((a, b) => b[1] - a[1])[0];
          return best ? best[0] : null;
        })()`)
      );

      await js(`window.__recallDebug.go('person', { id: ${target} })`);
      const before = await waitFor('the person page to fill in', async () => {
        const p = await js('window.__recallDebug.person()');
        return p.mounted && p.strip.length ? p : null;
      });
      assert(before.lastHeardMs > 0, `the page renders no "last heard" instant: ${JSON.stringify(before.strip)}`);

      // The rendered date is minute-resolution, so the instant behind it is
      // what proves the page re-asked rather than repainted the same answer.
      const after = await waitFor(
        'the page to follow the feed',
        async () => {
          const p = await js('window.__recallDebug.person()');
          return p.mounted && (p.lastHeardMs > before.lastHeardMs || p.segments > before.segments) ? p : null;
        },
        { timeout: 45000, every: 500 }
      );
      assert(after.id === before.id, 'the page moved to another person mid-check');

      await js('document.querySelector(\'.rail-item[data-view="speakers"]\').click()');
      await waitFor('the speaker list again', async () => js('document.querySelectorAll("#speaker-list .sp-row").length > 0'));
      return {
        speaker: target,
        lastHeard: [before.lastHeardMs, after.lastHeardMs],
        segments: [before.segments, after.segments],
      };
    });

    // 6n — 0.7.0: the memory graph gets a place of its own. The user's own
    // question was "when do the ai thing do thing? i dont see a tab for it?",
    // and this is the answer being there at all.
    await step('memory-is-the-fifth-rail-item', async () => {
      const rail = await js('[...document.querySelectorAll(".rail-item")].map(b => b.dataset.view)');
      assert(rail.length === 5, `the rail has ${rail.length} items: ${JSON.stringify(rail)}`);
      assert(rail.includes('memory'), `no Memory item in the rail: ${JSON.stringify(rail)}`);
      // Between Search and Sources: it is about what was said, not about what
      // the program is allowed to listen to.
      assert(
        rail.indexOf('memory') === rail.indexOf('sources') - 1,
        `Memory sits at ${rail.indexOf('memory')} in ${JSON.stringify(rail)}`
      );
      const label = await js('document.querySelector(\'.rail-item[data-view="memory"] span\').textContent');
      assert(label === 'Memory', `the item is called ${JSON.stringify(label)}`);

      await js('document.querySelector(\'.rail-item[data-view="memory"]\').click()');
      await waitFor('the memory view', async () => js('window.__recallDebug.view() === "memory"'));
      const railSelected = await js(
        'document.querySelector(\'.rail-item[data-view="memory"]\').getAttribute("aria-selected")'
      );
      assert(railSelected === 'true', 'the rail does not mark Memory as selected');
      const m = await waitFor('the view to fill in', async () => {
        const m = await js('window.__recallDebug.memory()');
        return m.mounted && m.commitments.length ? m : null;
      });
      // The badge is what is still OPEN, not the total: a badge is a number you
      // are meant to act on.
      assert(/^\d+$/.test(m.badge) && Number(m.badge) > 0, `the rail badge reads ${JSON.stringify(m.badge)}`);
      assert(/open/.test(m.sub), `the subtitle does not summarise: "${m.sub}"`);
      return { rail, badge: m.badge, sub: m.sub, file: await shot('memory') };
    });

    // 6o — the rule this whole feature turns on: a guess has to LOOK like a
    // guess, in the row, without hovering anything. A pattern match and a
    // language model are not the same claim.
    await step('commitments-say-which-tier-claimed-them', async () => {
      const m = await js('window.__recallDebug.memory()');
      assert(m.commitments.length >= 2, `only ${m.commitments.length} commitment(s) rendered`);
      const sources = [...new Set(m.commitments.map((c) => c.source))].sort();
      assert(
        sources.join(',') === 'llm,rules',
        `the fixture should carry both tiers, got ${JSON.stringify(sources)}`
      );
      for (const c of m.commitments) {
        assert(c.srcChip.trim().length > 0, `a row carries no visible source mark: ${JSON.stringify(c)}`);
        assert(c.srcTitle.length > 30, `the source mark does not explain itself: "${c.srcTitle}"`);
        assert(c.who && c.what, `a row is missing who or what: ${JSON.stringify(c)}`);
        // The evidence travels with the claim, so you can disagree here.
        assert(c.said.length > 5, `a row shows no transcript line: ${JSON.stringify(c)}`);
      }
      const rules = m.commitments.find((c) => c.source === 'rules');
      const llm = m.commitments.find((c) => c.source === 'llm');
      assert(/pattern/i.test(rules.srcChip), `the rule mark does not say what it is: "${rules.srcChip}"`);
      assert(/model|local/i.test(llm.srcChip), `the model mark does not say what it is: "${llm.srcChip}"`);
      assert(rules.srcChip !== llm.srcChip, 'both tiers wear the same mark');

      // Undated last, never first: no date said is not "overdue".
      const undatedAt = m.commitments.findIndex((c) => c.undated);
      assert(
        undatedAt === -1 || undatedAt === m.commitments.length - 1,
        `an undated promise sorted above a dated one (index ${undatedAt})`
      );
      // And nothing nags: the card says so in as many words.
      assert(/nothing here reminds you/i.test(m.note), `the list does not disclaim itself: "${m.note}"`);
      return { sources, rules: rules.srcChip, llm: llm.srcChip, undatedAt };
    });

    // 6p — the state machine. Only a click moves a row, and the whole app
    // hears about it: the rail badge is drawn from the daemon's own count.
    await step('a-commitment-moves-only-when-a-person-says-so', async () => {
      const before = await js('window.__recallDebug.memory()');
      const target = before.commitments.find((c) => c.state === 'candidate');
      assert(target, 'every commitment is already settled');
      assert(
        target.actions.join(',') === 'confirmed,done,dismissed',
        `a candidate offers ${JSON.stringify(target.actions)}`
      );

      await js(`document.querySelector('[data-act="confirmed"][data-commitment="${target.id}"]').click()`);
      // Settled, not merely optimistic: the row paints its new state
      // immediately and keeps its buttons disabled until the daemon agrees.
      const confirmed = await waitFor('the row to confirm', async () => {
        const m = await js('window.__recallDebug.memory()');
        const row = m.commitments.find((c) => c.id === target.id);
        return row && row.state === 'confirmed' && !row.pending ? m : null;
      });
      // Confirmed is still open, so the badge has not moved.
      assert(confirmed.badge === before.badge, `confirming changed the open count (${before.badge} → ${confirmed.badge})`);

      // …and the daemon really has it, not just this window.
      const listed = await js(`(async () => {
        const r = await window.recall.request('commitments.list', {});
        return r.data.commitments.find(c => c.id === ${target.id}).state;
      })()`);
      assert(listed === 'confirmed', `the daemon still says ${listed}`);

      // Done takes it off the open list, and the badge follows.
      await js(`document.querySelector('[data-act="done"][data-commitment="${target.id}"]').click()`);
      const done = await waitFor('the open count to fall', async () => {
        const m = await js('window.__recallDebug.memory()');
        return Number(m.badge) < Number(before.badge) ? m : null;
      });
      const gone = done.commitments.find((c) => c.id === target.id);
      assert(!gone, 'a settled commitment is still on the open list');
      // Nothing was deleted — it is still there under "settled".
      await js('document.getElementById("commitments-toggle").click()');
      const all = await waitFor('the settled rows', async () => {
        const m = await js('window.__recallDebug.memory()');
        return m.commitments.some((c) => c.id === target.id) ? m : null;
      });
      assert(
        all.commitments.find((c) => c.id === target.id).state === 'done',
        'the settled row lost its state'
      );
      await js('document.getElementById("commitments-toggle").click()');
      return { id: target.id, badgeBefore: before.badge, badgeAfter: done.badge, file: await shot('memory-commitments') };
    });

    // 6q — a commitment is a claim about a LINE, and the line has to be one
    // click away or there is no way to disagree with it.
    await step('a-commitment-opens-its-line-in-the-transcript', async () => {
      await js('document.querySelector(\'.rail-item[data-view="memory"]\').click()');
      await waitFor('the memory view', async () => js('window.__recallDebug.view() === "memory"'));
      // The view mounts empty and fills in from one query, so the list is not
      // there on the first frame.
      await waitFor('the commitment list', async () =>
        js('document.querySelectorAll("#commit-list .commit-said").length > 0')
      );
      await js('document.querySelector("#commit-list .commit-said").click()');
      await waitFor('the transcript to mount', async () => js('window.__recallDebug.view() === "transcript"'));
      const t = await waitFor(
        'the conversation to be marked',
        async () => {
          const t = await js('window.__recallDebug.threads()');
          return t.marked > 0 ? t : null;
        },
        { timeout: 15000 }
      );
      assert(t.filter === '', `the speaker filter was left set to "${t.filter}"`);
      return { marked: t.marked, rows: t.rows };
    });

    // 6r — topics: what you keep talking about, and a door into each one.
    await step('topics-group-conversations-and-open-them', async () => {
      await js('document.querySelector(\'.rail-item[data-view="memory"]\').click()');
      const m = await waitFor('the topic list', async () => {
        const m = await js('window.__recallDebug.memory()');
        return m.topics.length ? m : null;
      });
      for (const t of m.topics) {
        assert(t.topic && t.topic.length > 1, `a topic has no label: ${JSON.stringify(t)}`);
        assert(t.threads >= 1, `a topic names no conversations: ${JSON.stringify(t)}`);
        assert(/conversation/.test(t.text), `a topic row does not say how many: "${t.text}"`);
        assert(/last heard/i.test(t.text), `a topic row does not say when: "${t.text}"`);
      }
      await js('document.querySelector("#topic-list .topic-row").click()');
      await waitFor('the transcript to mount', async () => js('window.__recallDebug.view() === "transcript"'));
      const rows = await js('document.querySelectorAll("#seg-list .seg").length');
      assert(rows > 0, 'opening a topic rendered nothing');
      return { topics: m.topics.map((t) => t.topic), rows };
    });

    // -----------------------------------------------------------------------
    // 6r2..6r4 — 0.8.0's three cards, and why they are on THIS tab.
    //
    // Sources answers "what is this program allowed to hear". Notes, the
    // vocabulary and the accuracy figures answer "what did it make of what it
    // heard", which is the question Memory already exists for — and the three
    // of them are one loop: a fix moves the figures, the figures point at the
    // vocabulary, and the vocabulary changes what the next turn is heard as.
    // -----------------------------------------------------------------------

    await step('notes-to-self-render-and-flip-state', async () => {
      await js('document.querySelector(\'.rail-item[data-view="memory"]\').click()');
      const a = await waitFor('the notes list', async () => {
        const a = await js('window.__recallDebug.accuracy()');
        return a.notes.length ? a : null;
      });
      assert(a.notes.length >= 2, `only ${a.notes.length} note(s) rendered`);
      // The wake phrase is the addressing, not the note: what is shown is what
      // was said after it.
      for (const n of a.notes) {
        assert(n.text.length > 3, `a note rendered no text: ${JSON.stringify(n)}`);
        assert(!/^recall,/i.test(n.text), `a note still carries its wake phrase: "${n.text}"`);
        assert(n.segment > 0, `a note has no turn behind it: ${JSON.stringify(n)}`);
      }
      // Every state a note can be in has a chip, and the row only offers the
      // moves it is not already in.
      const states = [...new Set(a.notes.map((n) => n.state))].sort();
      assert(states.length >= 2, `every note is in the same state: ${JSON.stringify(states)}`);
      const open = a.notes.find((n) => n.state === 'open');
      assert(open, 'no open note to move');
      assert(!open.acts.includes('open'), 'an open note offers to be reopened');

      await js(`document.querySelector('[data-note-act="done"][data-note="${open.id}"]').click()`);
      const moved = await waitFor('the note to settle', async () => {
        const a = await js('window.__recallDebug.accuracy()');
        const row = a.notes.find((n) => n.id === open.id);
        return row && row.state === 'done' && !row.pending ? a : null;
      });
      // The daemon really has it, not just this window.
      const onWire = await js(`(async () => {
        const r = await window.recall.request('notes.list', {});
        return (r.data.notes.find(n => n.id === ${open.id}) || {}).state ?? null;
      })()`);
      assert(onWire === 'done', `the daemon still says ${onWire}`);
      // Put it back, so the live-note step below still has an open list to
      // prepend onto and the screenshots are not all one state.
      await js(`document.querySelector('[data-note-act="open"][data-note="${open.id}"]').click()`);
      await waitFor('the note to reopen', async () => {
        const a = await js('window.__recallDebug.accuracy()');
        return a.notes.find((n) => n.id === open.id)?.state === 'open';
      });
      await js('document.getElementById("notes-card").scrollIntoView({ block: "start" })');
      const file = await shot('memory-notes');
      return { notes: moved.notes.length, states, flipped: open.id, file };
    });

    // A note arriving live goes to the TOP of the list — it is the newest thing
    // you said to yourself, and it is why you are looking. The mock delivers it
    // on the first SIGUSR2, because a note that turns up at second 19 of a run
    // lands in whatever step happens to be on screen and proves nothing.
    if (process.env.NX_RECALL_MOCK_PID) {
      await step('a-note-arriving-live-goes-to-the-top', async () => {
        const before = await js('window.__recallDebug.accuracy()');
        process.kill(Number(process.env.NX_RECALL_MOCK_PID), 'SIGUSR2');
        const after = await waitFor(
          'the new note',
          async () => {
            const a = await js('window.__recallDebug.accuracy()');
            return a.notes.length > before.notes.length ? a : null;
          },
          { timeout: 12000 }
        );
        assert(
          after.notes[0].id !== before.notes[0]?.id,
          'the new note did not go to the top of the list'
        );
        assert(after.notes[0].state === 'open', `a fresh note arrived as "${after.notes[0].state}"`);
        // The turn itself is still a transcript row — a note is a second
        // reading of a turn, not a turn that was filed somewhere else.
        const seg = after.notes[0].segment;
        const inModel = await js(`window.__recallDebug.store.segById.has(${seg})`);
        assert(inModel, `the turn behind the note (${seg}) is not in the transcript`);
        return { was: before.notes.length, now: after.notes.length, top: after.notes[0].text.slice(0, 40) };
      });

      await step('a-re-published-archive-row-does-not-land-under-now', async () => {
        // The same SIGUSR2 re-published the mock's OLDEST segment first, the
        // way the re-decode worker announces every archive row it stamps. The
        // live transcript must not have filed it as an arrival: every DOM row
        // is a row the model holds, the model holds nothing older than its
        // head, and the newest row on screen is still the newest turn.
        await js('document.querySelector(\'.rail-item[data-view="transcript"]\').click()');
        const view = await waitFor('the transcript rows', async () => {
          const v = await js(`(() => {
            const s = window.__recallDebug.store;
            const rows = [...document.querySelectorAll('.seg')].map((r) => Number(r.dataset.seg));
            if (!rows.length) return null;
            return {
              rows,
              orphans: rows.filter((id) => !s.segById.has(id)),
              headMs: s.segments[0]?.t_ms ?? null,
              oldestHeldMs: Math.min(...s.segments.map((x) => x.t_ms)),
              lastDom: rows[rows.length - 1],
              lastModel: s.segments[s.segments.length - 1]?.id ?? null,
              domInOrder: rows.every((id, i) => i === 0 || (s.segById.get(rows[i - 1])?.t_ms ?? 0) <= (s.segById.get(id)?.t_ms ?? 0)),
            };
          })()`);
          return v;
        });
        assert(view.orphans.length === 0, `rows on screen the model does not hold: ${view.orphans.join(',')}`);
        assert(view.oldestHeldMs === view.headMs, 'the model holds a row older than its own head');
        assert(view.lastDom === view.lastModel, `newest row on screen is ${view.lastDom}, the model says ${view.lastModel}`);
        assert(view.domInOrder, 'the rows on screen are not in time order');
        return { rows: view.rows.length, last: view.lastDom };
      });
    }

    await step('the-accuracy-card-is-honest-arithmetic', async () => {
      await js('document.querySelector(\'.rail-item[data-view="memory"]\').click()');
      const a = await waitFor('the accuracy card', async () => {
        const a = await js('window.__recallDebug.accuracy()');
        return a.dash.corrections ? a : null;
      });
      // The driver fixed one transcript back in step 5a2, on top of the mock's
      // three seeded ones, so there is a real number here rather than a fixture.
      assert(Number(a.dash.corrections) >= 4, `only ${a.dash.corrections} corrections counted`);
      assert(/^\d+\.\d%$/.test(a.dash.wer), `the error estimate does not read as a rate: "${a.dash.wer}"`);
      assert(a.dash.bySource.length >= 1, 'no by-source breakdown');
      assert(a.dash.bySpeaker.length >= 1, 'no by-speaker breakdown');
      // The by-speaker rows are NAMES, not ids: a dashboard that says
      // "speaker_id 4" is a dashboard nobody reads.
      assert(
        !a.dash.bySpeaker.some((s) => /^\d+$/.test(s)),
        `a by-speaker row is a raw id: ${JSON.stringify(a.dash.bySpeaker)}`
      );
      // …and it says what kind of number it is. An estimate that presents
      // itself as a measurement is the whole failure mode of a card like this.
      assert(/estimate/i.test(a.dash.note), `the card does not own up to being an estimate: "${a.dash.note}"`);
      return { corrections: a.dash.corrections, wer: a.dash.wer, sources: a.dash.bySource, speakers: a.dash.bySpeaker };
    });

    await step('the-vocabulary-is-editable-where-it-is-a-decision', async () => {
      const a = await waitFor('the vocabulary card', async () => {
        const a = await js('window.__recallDebug.accuracy()');
        return a.vocab.user.length ? a : null;
      });
      assert(/\d+ terms/.test(a.vocab.effective), `the effective size is not stated: "${a.vocab.effective}"`);
      for (const key of ['roster', 'worlds', 'corrections']) {
        assert(a.vocab.counts[key] > 0, `the ${key} group has no count: ${JSON.stringify(a.vocab.counts)}`);
      }
      // The rule the card exists to make: one half is a decision and can be
      // taken back, the other half is an observation and cannot.
      assert(a.vocab.autoRemovable === 0, `${a.vocab.autoRemovable} derived terms offer a remove button`);

      const term = `driver-term-${Date.now()}`;
      await js(`(() => {
        const i = document.getElementById('vocab-add');
        i.value = ${JSON.stringify(term)};
        document.getElementById('vocab-add-go').click();
        return true;
      })()`);
      const added = await waitFor('the term to be added', async () => {
        const a = await js('window.__recallDebug.accuracy()');
        return a.vocab.user.includes(term) ? a : null;
      });
      // The daemon has it, and it reached the EFFECTIVE list — a glossary that
      // is stored and not used is a text field, not a feature.
      const onWire = await js(`(async () => {
        const r = await window.recall.request('vocab.get', {});
        return { user: r.data.user, effective: r.data.effective.includes(${JSON.stringify(term)}) };
      })()`);
      assert(onWire.user.includes(term), `the daemon's glossary is ${JSON.stringify(onWire.user)}`);
      assert(onWire.effective, 'a user term never reached the effective list');
      await js('document.getElementById("accuracy-card").scrollIntoView({ block: "start" })');
      await sleep(400);
      const file = await shot('memory-accuracy');
      await js('document.getElementById("vocab-card").scrollIntoView({ block: "start" })');
      await sleep(400);
      const vocabFile = await shot('memory-vocabulary');

      await js(`document.querySelector('[data-remove-term="${term}"]').click()`);
      const removed = await waitFor('the term to go', async () => {
        const a = await js('window.__recallDebug.accuracy()');
        return !a.vocab.user.includes(term) ? a : null;
      });
      const gone = await js(`(async () => {
        const r = await window.recall.request('vocab.get', {});
        return r.data.user.includes(${JSON.stringify(term)});
      })()`);
      assert(!gone, 'removing a term did not reach the daemon');
      return { terms: added.vocab.user.length, after: removed.vocab.user.length, counts: a.vocab.counts, files: [file, vocabFile] };
    });

    // 6s — the enrichment card, in all three states it has copy for, and the
    // copy itself. This is the honest-cost half of the feature: what it runs,
    // what it costs, that it keeps working while you play, that a pause stops
    // it too, that it is off by default, and that nothing leaves the machine.
    //
    // 0.7.2 changed what "when it runs" means. The old copy promised "never
    // while you are gaming" and the daemon kept that promise by standing down,
    // which meant promises surfaced hours after the evening they were made in.
    // The assertions below are the new contract, and the negative one is the
    // point of the change: this card must not tell somebody it waits.
    await step('enrichment-is-off-by-default-and-says-what-it-would-cost', async () => {
      await js('document.querySelector(\'.rail-item[data-view="memory"]\').click()');
      const off = await waitFor('the enrichment card', async () => {
        const m = await js('window.__recallDebug.memory()');
        return m.enrichment.chip ? m.enrichment : null;
      });
      assert(off.pressed === 'false', 'the local model is on by default');
      assert(off.chip === 'off', `the chip reads "${off.chip}"`);
      assert(off.progress === '', 'a progress line is showing with nothing running');

      const facts = off.facts.join(' ');
      assert(/\b1\.9 GB\b/.test(facts), `the copy does not say how big the model is: ${JSON.stringify(off.facts)}`);
      assert(/CPU cores?/i.test(facts), `the copy does not say what it spends: ${JSON.stringify(off.facts)}`);
      assert(/lowest priority/i.test(facts), `the copy does not say what priority it runs at`);
      assert(
        /keeps working while you play/i.test(facts),
        `the copy does not say it keeps running: ${JSON.stringify(off.facts)}`
      );
      assert(
        /cannot win a scheduling contest/i.test(facts),
        `the copy does not say why that is affordable: ${JSON.stringify(off.facts)}`
      );
      assert(
        !/never while you are gaming|waits for an idle|only while nothing is being captured/i.test(facts),
        `the card still promises to stand down while you play: ${JSON.stringify(off.facts)}`
      );
      assert(/paused means paused/i.test(facts), `the copy does not say a pause stops this too`);
      assert(/off until you turn it on/i.test(facts), `the copy does not say it is off by default`);
      assert(/suggestion you can dismiss/i.test(facts), `the copy does not say what it writes is dismissible`);
      assert(/leaves this machine/i.test(facts), `the copy does not say nothing leaves the machine`);
      assert(/never changes a transcript/i.test(off.note), `the card does not say what it writes: "${off.note}"`);

      // The setting that replaced standing down: how much of the machine the
      // model may use. It has to say when it takes effect, because threads are
      // an argument to an invocation and a conversation already open keeps the
      // width it started with.
      assert(off.threads === '4', `the stepper does not show the daemon's value: "${off.threads}"`);
      assert(
        /applies to the next conversation/i.test(off.threadsHint),
        `the stepper does not say when it takes effect: "${off.threadsHint}"`
      );
      assert(/1–32/.test(off.threadsHint), `the stepper does not say its range: "${off.threadsHint}"`);
      assert(
        new RegExp(`\\b${off.threads} CPU cores\\b`).test(facts),
        `the facts and the stepper disagree about the core count: ${JSON.stringify(off.facts)}`
      );
      // Turn it up two cores and watch the whole card follow: the value, the
      // fact line above it, and the daemon — which is asserted by leaving the
      // view and coming back, so what is read is a fresh `graph.summary` and
      // not this view's own optimism.
      //
      // One press at a time: the stepper disables itself while the daemon is
      // answering, exactly like every other optimistic control in this app, so
      // a second click fired into that window would land on nothing.
      const stepThreads = async (delta, want) => {
        await waitFor('the stepper to accept a press', async () => {
          const m = await js('window.__recallDebug.memory()');
          return m.enrichment.threads && !m.enrichment.threadsPending ? m.enrichment : null;
        });
        await js(`document.querySelector('#enrich-threads [data-step="${delta}"]').click()`);
        return waitFor(`the thread count to reach ${want}`, async () => {
          const m = await js('window.__recallDebug.memory()');
          // Settled, not merely optimistic: the value AND the daemon's answer.
          return m.enrichment.threads === want && !m.enrichment.threadsPending
            ? m.enrichment
            : null;
        });
      };
      await stepThreads(1, '5');
      const tuned = await stepThreads(1, '6');
      assert(
        /\b6 CPU cores\b/.test(tuned.facts.join(' ')),
        `the fact line did not follow the stepper: ${JSON.stringify(tuned.facts)}`
      );
      assert(tuned.threadsConfig === 6, `the daemon was not told: ${tuned.threadsConfig}`);
      await js('document.querySelector(\'.rail-item[data-view="speakers"]\').click()');
      await js('document.querySelector(\'.rail-item[data-view="memory"]\').click()');
      // A remounted card paints its default before `graph.summary` answers, so
      // the wait is for the value itself: settling on 6 is the proof that the
      // daemon, and not this view's optimism, is where it came from.
      const reread = await waitFor('the setting to survive a re-read', async () => {
        const m = await js('window.__recallDebug.memory()');
        return m.enrichment.threads === '6' ? m.enrichment : null;
      });
      assert(reread.threadsConfig === 6, `the store disagrees: ${reread.threadsConfig}`);

      // …and back to the shipped default, so the shot below documents what a
      // person actually opens this card to.
      await stepThreads(-1, '5');
      await stepThreads(-1, '4');

      // The card sits under the commitments and the topics, so a shot of the
      // top of the page would not document the thing this step is about.
      await js('document.getElementById("enrich-card").scrollIntoView({block: "end"})');
      const fileOff = await shot('memory-enrichment-off');

      // On: it runs, and the progress line says which conversation.
      await js('document.getElementById("enrich-toggle").click()');
      const running = await waitFor(
        'the worker to start reading',
        async () => {
          const m = await js('window.__recallDebug.memory()');
          return m.enrichment.phase === 'running' ? m.enrichment : null;
        },
        { timeout: 15000, every: 150 }
      );
      assert(running.pressed === 'true', 'the switch did not follow');
      assert(/reading/i.test(running.chip), `the chip did not follow the state: "${running.chip}"`);
      assert(
        /conversation \d+ of \d+/.test(running.progress),
        `the progress line says nothing useful: "${running.progress}"`
      );
      await js('document.getElementById("enrich-card").scrollIntoView({block: "end"})');
      const fileRunning = await shot('memory-enrichment-running');

      // …and idle when the batch is done.
      const idle = await waitFor(
        'the batch to finish',
        async () => {
          const m = await js('window.__recallDebug.memory()');
          return m.enrichment.phase === 'idle' ? m.enrichment : null;
        },
        { timeout: 20000, every: 200 }
      );
      assert(/idle/i.test(idle.chip), `the chip did not settle: "${idle.chip}"`);
      assert(idle.progress === '', 'the progress line outlived the batch');
      assert(/read/.test(idle.counts), `the card does not report what it did: "${idle.counts}"`);

      // Off again, and the switch is the only thing that moved.
      await js('document.getElementById("enrich-toggle").click()');
      const back = await waitFor(
        'the worker to stop',
        async () => {
          const m = await js('window.__recallDebug.memory()');
          return m.enrichment.phase === 'off' ? m.enrichment : null;
        },
        { timeout: 10000, every: 150 }
      );
      assert(back.pressed === 'false', 'the switch did not go back');

      // Leave the UI where the later steps expect to find it.
      await js('document.querySelector(\'.rail-item[data-view="speakers"]\').click()');
      await waitFor('the speaker list again', async () =>
        js('document.querySelectorAll("#speaker-list .sp-row").length > 0')
      );
      return {
        off: off.chip,
        threads: `${off.threads} → ${tuned.threads} → 4`,
        running: running.progress,
        idle: idle.chip,
        counts: idle.counts,
        files: [fileOff, fileRunning],
      };
    });

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
      const before = (await js('window.__recallDebug.counts()')).appended;
      await deps.setPaused(true); // exactly what the tray menu item calls
      await waitFor('the UI to show paused', async () =>
        js('document.getElementById("pause-btn").dataset.paused === "true"')
      );
      const label = await js('document.getElementById("pause-label").textContent');
      const chip = await js('(document.getElementById("live-chip")||{}).textContent || ""');
      await sleep(6000); // three feed intervals
      const after = (await js('window.__recallDebug.counts()')).appended;
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
      const before = (await js('window.__recallDebug.counts()')).appended;
      await deps.setPaused(false);
      const after = await waitFor(
        'the feed to restart',
        async () => {
          const n = (await js('window.__recallDebug.counts()')).appended;
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

    // 11a — audit finding #12, as a regression test with teeth.
    //
    // Jump to a hit that is FAR older than the live window — the single oldest
    // row in the archive, fourteen evenings back — and then wait through three
    // feed intervals. The old window model sorted the merged context in and
    // then trimmed the oldest rows to get back to 600, which threw away the
    // rows it had just been asked to show; the failure looked like "the
    // transcript went blank a second after I clicked the search result", and
    // it only happened once the window was full, which is always in practice.
    // Surviving the wait is the whole assertion.
    await step('a-jump-to-the-oldest-segment-renders-AND-SURVIVES', async () => {
      await js('document.querySelector(\'.rail-item[data-view="search"]\').click()');
      await waitFor('the search box', async () => js('!!document.getElementById("search-q")'));
      // The date facets default to the last seven days, and the row this step
      // is after is a fortnight back — so widen them, which is exactly what a
      // person looking for something old does. Their previous values are put
      // back afterwards: the facets are sticky by design and the steps that
      // run later expect the default window.
      const facets = await js(`(() => {
        const f = document.getElementById('search-from');
        const t = document.getElementById('search-to');
        const was = { from: f.value, to: t.value };
        f.value = '';
        t.value = '';
        document.getElementById('search-q').value = 'obsidian';
        document.getElementById('search-go').click();
        return was;
      })()`);
      // Wait for THIS query's results, not for whatever the previous step
      // left on screen — a row count going above zero is true before the new
      // answer has arrived, and clicking then jumps to the wrong segment.
      const hit = await waitFor('the obsidian hit', async () => {
        const rows = await js(`[...document.querySelectorAll('#search-results .seg')]
          .filter((r) => /obsidian/i.test(r.textContent))
          .map((r) => Number(r.dataset.hit))`);
        return rows.length ? rows : null;
      });
      const hitId = hit[0];
      // It really is the far end of the archive — older than everything the
      // live window holds. Without that this step proves nothing, because the
      // bug only ever bit on rows the window did not already have.
      const outside = await js(`(() => {
        const s = window.__recallDebug.store;
        return { held: s.segById.has(${hitId}), oldest: s.segments[0]?.t_ms ?? null };
      })()`);
      assert(!outside.held, 'the "oldest" segment was already in the live window — the fixture stopped being a test');

      await js(`document.querySelector('#search-results .seg[data-hit="${hitId}"]').click()`);
      await waitFor('the transcript', async () => js('window.__recallDebug.view() === "transcript"'));
      const marked = await waitFor(
        'the hit to be marked in the transcript',
        async () => js(`!!document.querySelector('#seg-list .seg.hit[data-seg="${hitId}"]')`),
        { timeout: 15000 }
      );
      assert(marked, 'the search hit never rendered in the transcript');

      const w = await js('window.__recallDebug.scrollback()');
      assert(!w.following, 'a jump into the past left the view following the live tail');
      const context = await js(`document.querySelectorAll('#seg-list .seg').length`);
      assert(context > 1, 'the hit rendered with no conversation around it');

      // Three feed intervals. This is the part that used to fail.
      await sleep(7000);
      const still = await js(`!!document.querySelector('#seg-list .seg[data-seg="${hitId}"]')`);
      assert(still, 'the merged context was trimmed away by the live feed — audit finding #12 is back');
      const inModel = await js(`window.__recallDebug.store.segById.has(${hitId})`);
      assert(inModel, 'the row left the model even though it is what the user was sent to');
      const end = await js('window.__recallDebug.scrollback()');
      assert(end.rows === end.unique, 'the live feed duplicated rows into a browsing window');
      assert(end.ordered, 'the live feed put the browsing window out of order');

      // And the toast that used to paper over the miss is gone: nothing should
      // be telling the user their segment is "outside the loaded window".
      const excuses = await js(
        '[...document.querySelectorAll(".toast")].filter(t => /outside the loaded window|scrolled out of the live window/i.test(t.textContent)).length'
      );
      assert(excuses === 0, 'the old "outside the loaded window" toast is still being shown');

      const file = await shot('jump-to-oldest');

      // Back to the tail, and the facets back to where they were. The facets
      // are sticky in module state, so putting the inputs back is not enough —
      // a search has to actually run for the view to write them down again.
      await js('document.getElementById("follow-btn").click()');
      await sleep(400);
      await js('document.querySelector(\'.rail-item[data-view="search"]\').click()');
      await waitFor('the search box', async () => js('!!document.getElementById("search-q")'));
      await js(`(() => {
        document.getElementById('search-from').value = ${JSON.stringify(facets.from)};
        document.getElementById('search-to').value = ${JSON.stringify(facets.to)};
        document.getElementById('search-q').value = 'portal';
        document.getElementById('search-go').click();
        return true;
      })()`);
      await sleep(600);
      await js('document.querySelector(\'.rail-item[data-view="transcript"]\').click()');
      await sleep(400);
      return { hits: hit.length, segment: hitId, wasOutside: !outside.held, context, file };
    });

    // 11b — the search mode toggle (0.6.5). The mock daemon has the semantic
    // model, so all three modes are live; the absent case is the GUI unit
    // suite's (`test/semantic.test.js`), which does not need a screen.
    await step('search-modes', async () => {
      await js('document.querySelector(\'.rail-item[data-view="search"]\').click()');
      await waitFor('the mode toggle', async () => js('!!document.getElementById("search-mode-both")'));
      const defaulted = await js('document.getElementById("search-mode-both").getAttribute("aria-pressed")');
      assert(defaulted === 'true', `Both should be the default with a model present, got ${defaulted}`);

      // A cross-language query: German in, and the mock's gloss reaches the
      // English turns the same way the real model does.
      await js(`(() => {
        document.getElementById('search-q').value = 'die Welt mit den Walen';
        document.getElementById('search-go').click();
        return true;
      })()`);
      // Both counts in ONE evaluation. Read separately they can straddle a
      // repaint from the live feed, and a hit count from before it against a
      // badge count from after it is a failure that is not a bug.
      const counts = await waitFor('smart hits', async () => {
        const c = await js(`(() => {
          const seg = document.querySelectorAll('#search-results .seg').length;
          const via = document.querySelectorAll('#search-results .chip.via').length;
          const sub = document.getElementById('search-sub').textContent;
          return seg > 0 && sub.includes('Walen') ? { seg, via, sub } : null;
        })()`);
        return c;
      });
      const hits = counts.seg;
      const vias = counts.via;
      assert(vias === hits, `every hit in Both must say how it was found: ${vias} of ${hits}`);
      assert(/by words and meaning/.test(counts.sub), `the mode should be stated: ${counts.sub}`);
      const both = await shot('search-both');

      // Keyword cannot answer that query at all, which is the point of the
      // toggle: the same words, a different mode, a different answer.
      await js('document.getElementById("search-mode-keyword").click()');
      const pressed = await waitFor('keyword to take over', async () =>
        js('document.getElementById("search-mode-keyword").getAttribute("aria-pressed") === "true"')
      );
      await waitFor('the keyword answer', async () =>
        js('document.querySelectorAll("#search-results .chip.via").length === 0')
      );
      const keyword = await shot('search-keyword');
      return { hits, vias, pressed, both, keyword };
    });

    // 11c — 0.8.0, the one query box. The Search view used to open with a field
    // called Query and four dropdowns beside it, which asked a person who
    // wanted to remember something to decompose their question into facets
    // first. Now they type the question; the daemon says what it understood;
    // the pills are where they disagree with it.
    await step('one-query-box-shows-what-it-understood', async () => {
      await js('document.querySelector(\'.rail-item[data-view="search"]\').click()');
      await waitFor('the search box', async () => js('!!document.getElementById("search-q")'));

      // The manual facets did not go away — they went behind something, and
      // that something has to be reachable.
      const shut = await js('window.__recallDebug.ask()');
      assert(shut.facetsHidden, 'the advanced facets are open by default — the box is not the front door');
      assert(shut.advanced === 'false', `the Advanced control says ${shut.advanced}`);
      await js('document.getElementById("search-advanced").click()');
      const open = await waitFor('the facets to open', async () => {
        const a = await js('window.__recallDebug.ask()');
        return !a.facetsHidden ? a : null;
      });
      assert(open.advanced === 'true', 'the Advanced control did not follow');
      assert(await js('!!document.getElementById("search-speaker")'), 'the speaker facet is gone entirely');
      await js('document.getElementById("search-advanced").click()');

      // A real question, typed and Entered — not the button, because Enter is
      // the motion this feature is about.
      await js(`(() => {
        const q = document.getElementById('search-q');
        q.value = 'what did Kira say yesterday about the portal';
        q.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));
        return true;
      })()`);
      const asked = await waitFor('the interpretation', async () => {
        const a = await js('window.__recallDebug.ask()');
        return a.shown && a.pills.length ? a : null;
      });
      const facets = asked.pills.map((p) => p.facet);
      for (const want of ['query', 'speaker', 'time', 'mode']) {
        assert(facets.includes(want), `no ${want} pill: ${JSON.stringify(asked.pills)}`);
      }
      const by = Object.fromEntries(asked.pills.map((p) => [p.facet, p]));
      assert(/Kira/.test(by.speaker.text), `the speaker pill reads "${by.speaker.text}"`);
      assert(/yesterday/i.test(by.time.text), `the time pill reads "${by.time.text}"`);
      assert(/portal/.test(by.query.text), `the words pill reads "${by.query.text}"`);
      assert(/both|smart|keyword/i.test(by.mode.text), `the mode pill reads "${by.mode.text}"`);
      // The words are not a facet you can drop — with them gone there is no
      // question left, and the box above is where you change them.
      assert(!by.query.removable, 'the query pill offers to remove itself');
      assert(by.speaker.removable && by.time.removable, 'a facet pill cannot be taken off');
      // The mode control must not claim one thing while the results came from
      // another.
      const pressed = asked.mode.find(([, on]) => on === 'true');
      assert(pressed, `no search mode is marked as selected: ${JSON.stringify(asked.mode)}`);
      const file = await shot('search-ask');

      // Taking a facet off re-runs with it cleared. The mock's history is a few
      // days old, so "yesterday" is a real constraint and dropping it is what
      // turns an empty answer into an answer — which is the whole reason the
      // pills are removable rather than merely informative.
      const before = asked.hits;
      await js('document.querySelector(\'#ask-pills [data-drop="time"]\').click()');
      const wider = await waitFor(
        'the search to re-run without the date',
        async () => {
          const a = await js('window.__recallDebug.ask()');
          return !a.pills.some((p) => p.facet === 'time') ? a : null;
        },
        { timeout: 15000 }
      );
      assert(wider.hits > before, `dropping the date changed nothing (${before} → ${wider.hits} hits)`);
      assert(wider.pills.some((p) => p.facet === 'speaker'), 'dropping the date took the speaker with it');
      // Every hit really is that speaker — the facet was cleared, not the query.
      const others = await js(`(() => {
        const want = document.querySelector('#ask-pills [data-facet="speaker"] .ask-pill-text').textContent;
        return [...document.querySelectorAll('#search-results .seg .who .nm')]
          .map(n => n.textContent).filter(t => t !== want).length;
      })()`);
      assert(others === 0, `${others} hit(s) are not the speaker the pill still names`);

      // And a shaky hit wears the same mark it wears in the transcript: a
      // result you are about to trust is entitled to say a second decoder did
      // not. Its own query, because the row this fixture reserves for it is
      // the only place these words appear.
      await js(`(() => {
        const q = document.getElementById('search-q');
        q.value = 'flickering';
        q.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));
        return true;
      })()`);
      const marked = await waitFor(
        'the shaky search hit',
        async () => {
          const a = await js('window.__recallDebug.accuracy()');
          return a.searchShaky > 0 ? a.searchShaky : null;
        },
        { timeout: 15000 }
      );

      // Put the view back where the later steps expect it: an explicit search
      // clears the interpretation, which is exactly what it is meant to do.
      await js(`(() => {
        document.getElementById('search-q').value = 'portal';
        document.getElementById('search-go').click();
        return true;
      })()`);
      await waitFor('the pills to clear', async () => js('!window.__recallDebug.ask().shown'));
      return { facets, hits: `${before} → ${wider.hits}`, shakyHits: marked, file };
    });

    // 11d — 0.8.0, briefs. A roster join naming a voice the user has named is
    // the one moment this app shows something unasked: you are about to talk to
    // somebody, and what is open between you is a thing you want to have
    // remembered ten seconds ago rather than ten minutes later.
    if (process.env.NX_RECALL_MOCK_PID) {
      await step('a-join-raises-a-brief-and-it-can-be-waved-away', async () => {
        await js('document.querySelector(\'.rail-item[data-view="transcript"]\').click()');
        const quiet = await js('window.__recallDebug.brief()');
        assert(!quiet.shown, 'a brief was up before anybody joined');

        process.kill(Number(process.env.NX_RECALL_MOCK_PID), 'SIGUSR2');
        const bar = await waitFor(
          'the brief bar',
          async () => {
            const b = await js('window.__recallDebug.brief()');
            return b.shown ? b : null;
          },
          { timeout: 15000 }
        );
        // It names a voice this user has NAMED. Read back from the model rather
        // than hard-coded: earlier steps merge, split and delete voices, and a
        // brief that named a fixture by string would be testing the fixture.
        const named = await js(`window.__recallDebug.store.speakers.get(${bar.speaker})?.name ?? null`);
        assert(named, `the brief is about speaker ${bar.speaker}, who has no user-given name`);
        assert(bar.text.startsWith(`${named} joined`), `the brief does not say who arrived: "${bar.text}"`);
        // The three things a brief is for, in the order somebody wants them.
        assert(/owes you:/.test(bar.text), `the brief does not say what they owe: "${bar.text}"`);
        assert(/you owe:/.test(bar.text), `the brief does not say what you owe: "${bar.text}"`);
        assert(/last talked about:/.test(bar.text), `the brief does not say what about: "${bar.text}"`);
        assert(bar.open && bar.dismiss, 'the brief offers no way in and no way out');
        const file = await shot('brief-bar');

        // A flaky instance bounces somebody in and out four times a minute, and
        // four identical bars is not four pieces of information. The mock sends
        // the same join again immediately; nothing new may happen.
        process.kill(Number(process.env.NX_RECALL_MOCK_PID), 'SIGUSR2');
        await sleep(2500);
        const again = await js('window.__recallDebug.brief()');
        assert(again.shown && again.speaker === bar.speaker, 'the repeat join disturbed the bar');

        await js('document.getElementById("brief-dismiss").click()');
        const gone = await waitFor('the brief to go', async () => js('!window.__recallDebug.brief().shown'));
        assert(gone, 'the brief could not be dismissed');

        // …and it is reachable on purpose, from the person page, which is where
        // somebody goes when they wondered about it after the bar had gone.
        await js('document.querySelector(\'.rail-item[data-view="speakers"]\').click()');
        await waitFor('the speaker list', async () => js('document.querySelectorAll("#speaker-list .sp-row").length > 0'));
        await js(`document.querySelector('.sp-row[data-speaker="${bar.speaker}"] [data-stats]').click()`);
        await waitFor('the person page', async () => js('window.__recallDebug.view() === "person"'));
        assert(await js('!!document.getElementById("person-brief")'), 'the person page offers no Brief');
        await js('document.getElementById("person-brief").click()');
        const asked = await waitFor(
          'the brief from the person page',
          async () => {
            const b = await js('window.__recallDebug.brief()');
            return b.shown ? b : null;
          },
          { timeout: 10000 }
        );
        await js('document.getElementById("brief-dismiss").click()');
        await waitFor('the brief to go again', async () => js('!window.__recallDebug.brief().shown'));
        await js('document.querySelector(\'.rail-item[data-view="transcript"]\').click()');
        return { text: bar.text.slice(0, 120), fromPage: asked.text.slice(0, 60), file };
      });
    }

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

    // 12a — 0.6.1: what all of this costs on disk, broken into the four parts
    // that behave differently. A single total would hide the fact that exactly
    // one of them shrinks on its own.
    await step('storage-card-and-footer', async () => {
      await js('document.querySelector(\'.rail-item[data-view="sources"]\').click()');
      const s = await waitFor('the storage card', async () => {
        const s = await js('window.__recallDebug.storage()');
        return s.rows.length ? s : null;
      });
      const keys = s.rows.map(([k]) => k);
      for (const want of ['db', 'audio', 'goldens', 'models', 'total']) {
        assert(keys.includes(want), `the breakdown is missing "${want}": ${JSON.stringify(keys)}`);
      }
      // Real sizes, not placeholders.
      assert(
        s.rows.every(([, v]) => /\d/.test(v)),
        `a storage row rendered no number: ${JSON.stringify(s.rows)}`
      );
      assert(/retention/i.test(s.note), `the card does not say what is capped: ${s.note}`);
      assert(/model/i.test(s.note), `the card does not say the models are fixed: ${s.note}`);
      assert(/db/.test(s.footer) && /audio/.test(s.footer), `the footer lost its storage line: "${s.footer}"`);
      // The card sits under the application list, so a screenshot of the top
      // of the page would not document it.
      await js('document.getElementById("storage-card").scrollIntoView({block: "end"})');
      const file = await shot('sources-storage');
      return { rows: s.rows, footer: s.footer, file };
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

      // The microphone is now an ALLOWED source row in the daemon's own
      // `sources.list`, which is the state audit finding #25a lived in: the
      // rail badge counted that row, the Sources view excluded it, so the same
      // daemon read 2 or 1 depending on which painted last.
      //
      // The switch does not broadcast a `source` event for its own row (it has
      // its own `mic` event), so the model only learns this on a resync — which
      // is what the daemon's answer is folded in here to reproduce, from ANOTHER
      // view, because the Sources view used to paint over the wrong number.
      await js('document.querySelector(\'.rail-item[data-view="transcript"]\').click()');
      const listed = await js(`(async () => {
        const res = await window.recall.request('sources.list');
        const rows = res?.data?.sources ?? [];
        window.__recallDebug.store.sources = rows;
        return {
          apps: rows.filter((s) => s.kind !== 'mic' && s.allowed).length,
          micAllowed: rows.some((s) => s.kind === 'mic' && s.allowed),
        };
      })()`);
      assert(listed.micAllowed, 'the daemon does not list the microphone as allowed — the case cannot be reproduced');

      // The badges are painted from the event stream, so wait for one to land.
      const seen = (await js('window.__recallDebug.counts()')).appended;
      const badge = await waitFor(
        'the rail badge to be repainted',
        async () => {
          const b = await js(`(() => ({
            appended: window.__recallDebug.counts().appended,
            badge: Number(document.getElementById('badge-sources').textContent),
          }))()`);
          return b.appended > seen ? b : null;
        },
        { timeout: 15000, every: 250 }
      );
      assert(
        badge.badge === listed.apps,
        `the rail badge says ${badge.badge} allowed sources; ${listed.apps} applications are allowed — the microphone is being counted as one`
      );
      return { denied, waiting: waiting.chip, capturing: capturing.chip, badge: badge.badge, apps: listed.apps, file };
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
      //
      // Compared against a row with the SAME meta content. The last track is
      // content-sized, so a plain row and one carrying a source chip resolve
      // to different widths for reasons that have nothing to do with the You
      // treatment — which is what this assertion is about.
      const cols = await js(`(() => {
        const you = document.querySelector('#seg-list .seg.you');
        const badges = (r) => r.querySelector('.meta').children.length;
        const other = [...document.querySelectorAll('#seg-list .seg:not(.you)')].find((r) => badges(r) === badges(you));
        if (!other) return { skipped: true };
        return {
          you: getComputedStyle(you).gridTemplateColumns,
          other: getComputedStyle(other).gridTemplateColumns,
          badges: badges(you),
        };
      })()`);
      assert(!cols.skipped, 'no comparable row to check the You layout against');
      assert(cols.you === cols.other, `the You row uses a different layout: ${cols.you} vs ${cols.other}`);

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
    // we can style. It used to be pinned to dark because the app had one ground;
    // NX Clear has two (DESIGN §14.1), so what matters now is that it FOLLOWS —
    // light widgets on the light ground are correct, and a stale `dark` here
    // would put a black dropdown in the middle of a white app.
    await step('native-widgets-follow-the-theme', async () => {
      const scheme = await js('window.__recallDebug.colorScheme()');
      const want = deps.theme?.().dark ? 'dark' : 'light';
      assert(
        scheme === want,
        `the document declares color-scheme "${scheme}" on the ${want} ground, so native popups are drawn the wrong way round`
      );

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
      assert(sel.scheme === want, `the select itself inherits "${sel.scheme}", not "${want}"`);
      assert(sel.options > 1, `the speaker dropdown has nothing in it (${sel.options})`);
      // Belt and braces behind the scheme, for platforms whose popup ignores it.
      assert(sel.rule, 'no explicit option colours are declared');
      assert(/background/.test(sel.rule) && /color/.test(sel.rule), `the option rule is incomplete: ${sel.rule}`);
      const file = await shot('search-select');
      await js('document.querySelector(\'.rail-item[data-view="transcript"]\').click()');
      return { ...sel, file };
    });

    // 12e — both of NX Clear's grounds are real, at token level (DESIGN §14.1,
    // §14.3's first checklist item). The explicit [data-theme] stamp is applied
    // each way in turn and the two must genuinely differ — a Clear app that
    // ships one palette plus a media query it never honours is exactly the
    // failure this catches, and it is invisible in a single-theme screenshot
    // run. `color-scheme` has to follow too, or the native dropdown lands on
    // the wrong ground (12d).
    await step('both-themes-are-defined', async () => {
      const was = await js('document.documentElement.getAttribute("data-theme")');
      const read = (t) =>
        js(`(() => {
          document.documentElement.setAttribute('data-theme', ${JSON.stringify(t)});
          const root = getComputedStyle(document.documentElement);
          const body = getComputedStyle(document.body);
          return {
            scheme: root.colorScheme,
            ground: body.backgroundColor,
            ink: body.color,
            surface: root.getPropertyValue('--clear-surface').trim(),
            line: root.getPropertyValue('--clear-line').trim(),
            spL: root.getPropertyValue('--sp-l').trim(),
          };
        })()`);

      const light = await read('light');
      const dark = await read('dark');

      // Put the pass back on its own ground before anything else is
      // photographed. The stamp wins over the media query in both directions,
      // so removing it is what hands the page back to the OS.
      await js(
        was == null
          ? 'document.documentElement.removeAttribute("data-theme")'
          : `document.documentElement.setAttribute('data-theme', ${JSON.stringify(was)})`
      );

      assert(light.ground !== dark.ground, `both stamps paint the same ground (${light.ground})`);
      assert(light.ink !== dark.ink, `both stamps use the same body ink (${light.ink})`);
      assert(light.surface !== dark.surface, `--clear-surface does not move between themes (${light.surface})`);
      assert(light.line !== dark.line, `--clear-line does not move between themes (${light.line})`);
      assert(light.scheme === 'light', `the light stamp declares color-scheme "${light.scheme}"`);
      assert(dark.scheme === 'dark', `the dark stamp declares color-scheme "${dark.scheme}"`);
      // §14.1 as amended: Clear's dark variant is grounded at true black.
      assert(dark.ground === 'rgb(0, 0, 0)', `the dark ground is ${dark.ground}, not #000000`);
      // The speaker hue band is the same identity on both grounds; only its
      // lightness moves, which is what keeps a voice's colour meaning one thing.
      assert(light.spL !== dark.spL, `the speaker palette does not re-tune per theme (${light.spL})`);
      return { light, dark, restored: was };
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

        // Audit finding #25b, caught in the one window where it is visible: the
        // footer used to keep the last daemon's numbers on screen — "queue 3 ·
        // db 1.2 GB" — right next to the words "daemon offline". A number with
        // no daemon behind it is a lie, so they go quiet and read "—".
        const offline = await waitFor(
          'the offline footer',
          async () => {
            const f = await js('window.__recallDebug.resync()');
            return /daemon offline/.test(f.footer) ? f : null;
          },
          { timeout: 8000, every: 100 }
        );
        assert(!/queue \d/.test(offline.footer), `the footer is still quoting a dead daemon: "${offline.footer}"`);
        assert(!/db \d/.test(offline.footer), `the footer is still quoting storage from a dead daemon: "${offline.footer}"`);
        assert(
          offline.stats.some(([t, stale]) => stale && /queue/.test(t)),
          `the daemon numbers are not greyed while offline: ${JSON.stringify(offline.stats)}`
        );

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

        // …and the resync really finished. A slice that failed keeps its old
        // data and says so in the footer (audit finding #11), so "green again"
        // has to mean every query answered rather than merely "socket back".
        const fresh = await waitFor(
          'the resync to finish',
          async () => {
            const r = await js('window.__recallDebug.resync()');
            return r.conn === 'connected' && r.stale.length === 0 ? r : null;
          },
          { timeout: 15000, every: 250 }
        );
        assert(!/catching up/.test(fresh.connText), `the footer still says it is catching up: "${fresh.connText}"`);

        // The live feed has to actually resume. This is the assertion that
        // caught the client holding its pre-restart sequence number and
        // discarding the restarted daemon's whole stream as duplicates.
        const resumed = await waitFor(
          'the feed after the restart',
          async () => {
            const n = (await js('window.__recallDebug.counts()')).appended;
            return n > after.appended ? n : null;
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
      theme: deps.theme?.() ?? null,
      passed,
      failed: results.length - passed,
      results,
    };
    mkdirSync(OUT, { recursive: true });
    writeFileSync(join(OUT, `e2e-report${SUFFIX}.json`), JSON.stringify(report, null, 2));
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
