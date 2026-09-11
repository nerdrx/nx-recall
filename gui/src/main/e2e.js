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
import { mkdirSync, readFileSync, readdirSync, writeFileSync, rmSync } from 'node:fs';
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

  /**
   * The captions window (0.8.3), and JavaScript inside it.
   *
   * A second window means a second webContents, and every read the driver makes
   * about the captions has to go through THIS one — asking the main window what
   * the captions are showing would be asking the wrong process.
   */
  const capWin = () => {
    const w = deps.captions?.window?.();
    if (!w || w.isDestroyed()) throw new Error('no captions window');
    return w;
  };
  const capJs = (code) => capWin().webContents.executeJavaScript(code, true);

  async function shotOf(w, name) {
    // Software GL inside headless gamescope presents lazily: without waiting
    // for two real frames, capturePage hands back the PREVIOUS view and the
    // screenshots quietly document the wrong screen.
    await w.webContents
      .executeJavaScript('new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r)))', true)
      .catch(() => {});
    w.webContents.invalidate?.();
    await sleep(500);
    const img = await w.webContents.capturePage();
    const file = join(OUT, `${String(++shots).padStart(2, '0')}-${name}${SUFFIX}.png`);
    writeFileSync(file, img.toPNG());
    return file;
  }

  const shot = (name) => shotOf(win(), name);

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

    // 6i2 — 0.11.0, "heard on". Some people you only ever meet on Discord, and
    // until this release the list could not say so — which is also why the
    // voicebank could hand a VRChat turn to a voice that has never been in
    // VRChat. The chips are the visible half of that fix, so they are asserted
    // against the mock's own numbers rather than merely counted.
    await step('speaker-rows-say-where-a-voice-is-heard', async () => {
      await js('document.querySelector(\'.rail-item[data-view="speakers"]\').click()');
      await waitFor('the speaker list', async () =>
        js('document.querySelectorAll("#speaker-list .sp-row").length > 0')
      );
      const chips = await waitFor('the heard-on chips', async () =>
        js(`(() => {
          const out = {};
          for (const row of document.querySelectorAll('#speaker-list .sp-row')) {
            const id = Number(row.dataset.speaker);
            const c = [...row.querySelectorAll('.chip.heard')].map((e) => e.dataset.heardSource);
            if (c.length) out[id] = c;
          }
          return Object.keys(out).length ? out : null;
        })()`)
      );
      // The three shapes the feature is about, all on screen at once. Asserted
      // as shapes rather than by voice id on purpose: by the time this step
      // runs the driver has already renamed, reassigned and swept voices, and
      // pinning an id here would make an unrelated step's edit look like a
      // rendering bug.
      const shapes = Object.values(chips);
      assert(
        shapes.some((c) => c.length === 1 && c[0] === 'Discord'),
        `no voice is Discord-only: ${JSON.stringify(chips)}`
      );
      assert(
        shapes.some((c) => c.length > 1),
        `no voice is heard on more than one source: ${JSON.stringify(chips)}`
      );
      assert(
        shapes.some((c) => c.includes('mic')),
        `nothing is heard on the microphone: ${JSON.stringify(chips)}`
      );
      // And they agree with what the daemon actually said, chip for chip.
      const wire = await js(`(async () => {
        const r = await window.recall.request('speakers.list');
        return Object.fromEntries(r.data.speakers.map((s) => [s.id, s.sources.map((x) => x.source)]));
      })()`);
      for (const [id, got] of Object.entries(chips)) {
        assert(
          JSON.stringify(got) === JSON.stringify(wire[id]),
          `voice ${id} renders ${JSON.stringify(got)} but the daemon said ${JSON.stringify(wire[id])}`
        );
      }
      // The microphone is the one source whose label is certain rather than
      // matched, so it is the one chip that reads as a word and not an exe.
      const mic = await js(
        `(document.querySelector('.chip.heard.kind-mic') || {}).textContent || ""`
      );
      assert(/your mic/i.test(mic), `the mic chip does not name itself: "${mic}"`);
      return { chips, mic };
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
      // The rail has five PLACES (Memory joined in 0.7.0) and none of them is
      // selected here: the person page is pushed state, not a place in the app.
      // Counted by `[data-view]`, because 0.8.3 put an action in the rail as
      // well — Captions opens another window rather than navigating this one,
      // so it is not a sixth place and must not be counted as one.
      const rail = await js('document.querySelectorAll(".rail-item[data-view]").length');
      const selected = await js('document.querySelectorAll(\'.rail-item[aria-selected="true"]\').length');
      assert(rail === 6, `the rail has ${rail} items, not the six it should`);
      assert(selected === 0, 'a rail item claims to be selected on the person page');
      return { speaker: target, strip: p.strip, sub: p.sub, file: await shot('person-page') };
    });

    // 6j2 — 0.11.0. The header answers the same question the row does, with the
    // counts the row has no space for: a voice with 812 Discord turns and three
    // in VRChat is somebody you know from one place, and that is a fact about a
    // person rather than a statistic about a source.
    await step('the-person-page-header-says-where-they-are-heard', async () => {
      const chips = await waitFor('the header chips', async () =>
        js(`(() => {
          const rows = [...document.querySelectorAll('#person-header .chip.heard')];
          return rows.length ? rows.map((e) => [e.dataset.heardSource, e.textContent]) : null;
        })()`)
      );
      const id = await js('window.__recallDebug.person().id');
      const wire = await js(`(async () => {
        const r = await window.recall.request('person.get', { id: ${id} });
        return r.data.sources;
      })()`);
      assert(
        chips.length === wire.length,
        `${chips.length} chips for ${wire.length} sources: ${JSON.stringify(chips)}`
      );
      for (let i = 0; i < wire.length; i += 1) {
        assert(
          chips[i][0] === wire[i].source,
          `chip ${i} is ${chips[i][0]}, the daemon said ${wire[i].source}`
        );
        // The header's chips carry the turn count; the list's do not.
        assert(
          chips[i][1].includes(String(wire[i].segments)),
          `chip "${chips[i][1]}" does not carry its ${wire[i].segments} turns`
        );
      }
      return { chips, file: await shot('person-heard-on') };
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

    // 6k2 — 0.10.0. "Where you meet": the world chips, and the fact that one
    // of them is a way INTO the transcript rather than a badge.
    await step('the-person-page-says-where-you-meet', async () => {
      const p = await waitFor('the world chips', async () => {
        const p = await js('window.__recallDebug.person()');
        return p.worlds.length ? p : null;
      });
      assert(p.worlds.length >= 1, 'no world chips at all');
      for (const w of p.worlds) {
        assert(/^wrld_/.test(w.id), `a chip has no world id: ${JSON.stringify(w)}`);
        assert(w.name && w.name.trim(), `a chip renders no name: ${JSON.stringify(w)}`);
        // visits · time · when. All three, because one of them alone is not a
        // memory of a place.
        assert(/visit/.test(w.meta), `a chip does not say how often: "${w.meta}"`);
        assert(w.meta.split('·').length >= 3, `a chip is missing a column: "${w.meta}"`);
      }
      return { worlds: p.worlds.map((w) => w.name) };
    });

    // 6k3 — 0.10.0. "How you talk". The two approximations MUST carry the
    // daemon's own definition where they are rendered: a statistic whose caveat
    // lives in a different file is a statistic that gets shown without one.
    await step('the-person-page-says-how-you-talk', async () => {
      const p = await waitFor(
        'the turn-taking card',
        async () => {
          const p = await js('window.__recallDebug.person()');
          return p.talk && p.talk.cells.length ? p : null;
        },
        { timeout: 15000 }
      );
      const talk = p.talk;
      assert(/%$/.test(talk.share), `the share does not read as a percentage: "${talk.share}"`);
      assert(talk.shareFrac > 0 && talk.shareFrac <= 1, `the share is not a fraction: ${talk.shareFrac}`);
      // The bar has to agree with the number it sits under.
      const barPct = Number(String(talk.barWidth).replace('%', ''));
      assert(
        Math.abs(barPct - talk.shareFrac * 100) < 1.5,
        `the bar says ${talk.barWidth} and the number says ${talk.share}`
      );

      const cells = Object.fromEntries(talk.cells.map(([k, v, title]) => [k, { v, title }]));
      for (const want of ['mean-turn', 'monologue', 'interruptions', 'latency', 'tpm']) {
        assert(cells[want], `no "${want}" cell: ${JSON.stringify(Object.keys(cells))}`);
        assert(cells[want].v.trim(), `the ${want} cell renders nothing`);
      }
      assert(/\d+\s*\/\s*\d+/.test(cells.interruptions.v), `interruptions read "${cells.interruptions.v}"`);
      for (const want of ['interruptions', 'latency']) {
        assert(
          cells[want].title.length > 40,
          `the ${want} cell offers no definition — an approximation shown without its caveat is a claim`
        );
      }
      assert(
        /overlap/i.test(cells.interruptions.title),
        `the interruption definition does not mention the overlap it rests on: "${cells.interruptions.title}"`
      );
      return {
        share: talk.share,
        cells: talk.cells.map(([k, v]) => [k, v]),
        file: await shot('person-talk'),
      };
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
      // 0.9.2: the same Replay affordance the transcript's separators carry.
      // One feature reached from four places, not four features.
      assert(
        (await js('document.querySelectorAll("#thread-list .thread-replay").length')) > 0,
        'a recent conversation offers no way to play it back'
      );

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

    // 6m3 — conversation replay (0.9.2). The whole feature in one pass: it
    // starts from the boundary that names the conversation, the bar appears,
    // the playhead reads THROUGH a turn whose audio retention took rather than
    // skipping it, pause holds it, the rate moves it, and a second replay takes
    // the sound off the first. Thread 502 is the mock's mixed conversation —
    // one turn from the aged-out voice, four that still sound.
    await step('a-conversation-replays-from-its-separator', async () => {
      await js('document.querySelector(\'.rail-item[data-view="transcript"]\').click()');
      await waitFor('the transcript', async () => js('window.__recallDebug.view() === "transcript"'));
      const sel = '.thread-sep[data-thread="502"] .replay-start';
      await waitFor('a Replay affordance on a conversation boundary', async () =>
        js(`!!document.querySelector('${sel}')`)
      );
      await js(`document.querySelector('${sel}').click()`);

      const bar = await waitFor('the replay bar', async () => {
        const r = await js('window.__recallDebug.replay()');
        return r.shown && r.ticks.length ? r : null;
      });
      assert(bar.who, 'the bar names nobody');
      assert(/\d\d:\d\d/.test(bar.clock), `the bar has no clock: "${bar.clock}"`);
      assert(bar.rateLabel === '1×', `the bar did not start at 1×, it started at ${bar.rateLabel}`);
      // The mixed conversation, and the one sentence that explains it. Said for
      // the conversation, not for each row it is true of.
      const gone = bar.ticks.filter((t) => t.gone);
      assert(gone.length > 0, 'no turn in this conversation lost its audio — the mock must mix both');
      assert(gone.length < bar.ticks.length, 'every turn lost its audio — that is not a mix');
      assert(/retention/i.test(bar.note), `the bar never said why a turn is silent: "${bar.note}"`);

      // The transcript follows: a row is lit, and it is the turn the bar names.
      const first = await waitFor('a lit row', async () => {
        const r = await js('window.__recallDebug.replay()');
        return r.row ? r : null;
      });
      assert(
        String(first.row) === String(first.turns[first.index].id),
        `the lit row is ${first.row}, the playhead is on ${first.turns[first.index].id}`
      );
      const startedGone = !first.turns[first.index].has_audio;

      // Rule 1: the silent turn is READ THROUGH — the playhead leaves it on its
      // own, without anybody pressing anything, and lands on a turn that sounds.
      const moved = await waitFor(
        'the playhead to advance past the first turn',
        async () => {
          const r = await js('window.__recallDebug.replay()');
          return r.index > first.index ? r : null;
        },
        { timeout: 20000 }
      );
      assert(String(moved.row) !== String(first.row), 'the lit row did not move with the playhead');
      // Photographed here rather than on the first frame: the point of the
      // picture is the bar AND the row it has scrolled the transcript to.
      const shot1 = await shot('replay');

      // Pause holds it where it is; resume carries on from there.
      await js('document.getElementById("replay-play").click()');
      const paused = await waitFor('the bar to say paused', async () => {
        const r = await js('window.__recallDebug.replay()');
        return r.phase === 'paused' ? r : null;
      });
      assert(paused.playLabel === 'Play', `a paused bar offers "${paused.playLabel}"`);
      await sleep(1200);
      const still = await js('window.__recallDebug.replay()');
      assert(still.index === paused.index, 'the playhead moved while it was paused');
      assert(still.audioPaused !== false, 'the element kept playing through a pause');
      await js('document.getElementById("replay-play").click()');
      const resumed = await waitFor('the bar to leave paused', async () => {
        const r = await js('window.__recallDebug.replay()');
        return r.phase !== 'paused' ? r : null;
      });

      // The rate control moves both the element and the silences.
      await js('document.getElementById("replay-rate").click()');
      const faster = await js('window.__recallDebug.replay()');
      assert(faster.rate > 1, `the rate button did not change the rate (${faster.rateLabel})`);
      assert(
        faster.audioRate === null || faster.audioRate === faster.rate,
        `the element is at ${faster.audioRate}× while the bar says ${faster.rate}×`
      );

      // A second replay takes the sound from the first. This one comes in
      // through the controller — the route a digest, a person page and a search
      // hit all use — so the two entry shapes are both exercised.
      await js('window.__recallDebug.startReplay(503)');
      const second = await waitFor('the second conversation', async () => {
        const r = await js('window.__recallDebug.replay()');
        return r.thread === 503 ? r : null;
      });
      assert(second.shown, 'the second replay never raised a bar');
      assert(second.index === 0, 'the second replay inherited the first one\'s playhead');

      // Closing puts the bar away and takes the highlight with it.
      await js('document.getElementById("replay-close").click()');
      const closed = await waitFor('the bar to close', async () => {
        const r = await js('window.__recallDebug.replay()');
        return !r.shown ? r : null;
      });
      assert(!closed.active, 'the engine is still replaying with no bar on screen');
      assert(closed.row === null, 'a row is still lit after the replay closed');
      assert(closed.audioPaused !== false, 'a closed replay is still making a sound');

      return {
        thread: 502,
        turns: bar.ticks.length,
        gone: gone.length,
        startedGone,
        note: bar.note,
        advancedTo: moved.index,
        resumedPhase: resumed.phase,
        rate: faster.rateLabel,
        second: second.thread,
        affordances: bar.rowsWithButton,
        file: shot1,
      };
    });

    // 6m4 — the same one affordance on the other surfaces. It is one feature
    // reached four ways, and a Replay that exists only in the transcript would
    // make it a transcript feature. (The person page's is asserted in
    // `a-conversation-opens-in-the-transcript` and the search hit's in
    // `search-and-jump`, where those views are already open.)
    await step('replay-is-offered-wherever-a-conversation-is-named', async () => {
      const where = {};
      await js('document.querySelector(\'.rail-item[data-view="transcript"]\').click()');
      where.separators = await waitFor('separator affordances', async () =>
        js('document.querySelectorAll(".thread-sep .replay-start").length')
      );
      await js('document.querySelector(\'.rail-item[data-view="memory"]\').click()');
      where.digests = await waitFor(
        'digest replay affordances',
        async () => js('document.querySelectorAll(".digest-row .digest-replay").length'),
        { timeout: 15000 }
      );
      // Back where the later steps expect to be.
      await js('document.querySelector(\'.rail-item[data-view="speakers"]\').click()');
      await waitFor('the speaker list again', async () =>
        js('document.querySelectorAll("#speaker-list .sp-row").length > 0')
      );
      assert(where.separators > 0, 'no conversation boundary offers a replay');
      assert(where.digests > 0, 'no digest offers a replay');
      // The person page's and the search hit's are asserted where those views
      // are already open — driving the search box from here left a query behind
      // that the "one query box" step then had to undo.
      return where;
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
      const rail = await js('[...document.querySelectorAll(".rail-item[data-view]")].map(b => b.dataset.view)');
      assert(rail.length === 6, `the rail has ${rail.length} items: ${JSON.stringify(rail)}`);
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
      await js(`document.querySelector('[data-memory-tab="commitments"]').click()`);
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
      await js(`document.querySelector('[data-memory-tab="commitments"]').click()`);
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
      await js(`document.querySelector('[data-memory-tab="commitments"]').click()`);
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

    // 6r5 — 0.10.0. The Worlds card, and the share bars the digest card gained.
    await step('the-worlds-card-lists-places-and-who-is-in-them', async () => {
      await js('document.querySelector(\'.rail-item[data-view="memory"]\').click()');
      const w = await waitFor(
        'the worlds card',
        async () => {
          const w = await js('window.__recallDebug.worlds()');
          return w.shown && w.rows.length ? w : null;
        },
        { timeout: 15000 }
      );
      assert(w.rows.length >= 2, `only ${w.rows.length} world(s) — one proves nothing`);
      for (const row of w.rows) {
        assert(/^wrld_/.test(row.id), `a row has no world id: ${JSON.stringify(row)}`);
        assert(row.name && row.name.trim(), 'a world row renders no name');
        assert(Number(row.visits) > 0, `${row.name} claims ${row.visits} visits`);
        assert(row.people.length > 0, `${row.name} names nobody who has been there`);
      }
      // The digest card lists participants, so 0.10.0 gave it share bars. A
      // bar whose width disagrees with its number is worse than no bar.
      assert(w.digestShares.length > 0, 'the digest card lists participants but no shares');
      for (const sh of w.digestShares) {
        assert(sh.share > 0 && sh.share <= 1, `a share is not a fraction: ${sh.share}`);
        const width = Number(String(sh.width).replace('%', ''));
        assert(
          Math.abs(width - sh.share * 100) < 1.5,
          `a share bar is ${sh.width} wide for a share of ${sh.share}`
        );
        // Speech time, not turn count — and the tooltip has to say which.
        assert(/speech/i.test(sh.title), `a share bar does not say what it measures: "${sh.title}"`);
      }
      // The card lives below the digest and the commitments, so the artefact
      // has to be scrolled to it or it photographs the top of the page.
      await js('document.getElementById("worlds-card").scrollIntoView({ block: "center" })');
      await sleep(250);
      const file = await shot('memory-worlds');

      // A world row is a DOOR: it leads to Search, already filtered, with a
      // pill saying so. A filter you cannot see is a filter you cannot remove.
      const target = w.rows[0];
      await js(`document.querySelector('#world-list [data-world="${target.id}"]').click()`);
      await waitFor('search to mount', async () => js('window.__recallDebug.view() === "search"'));
      const pills = await waitFor(
        'the world pill',
        async () => {
          const a = await js('window.__recallDebug.ask()');
          return a.shown && a.pills.some((p) => p.facet === 'world') ? a : null;
        },
        { timeout: 15000 }
      );
      const pill = pills.pills.find((p) => p.facet === 'world');
      assert(
        pill.text.includes(target.name),
        `the pill reads "${pill.text}" for the world "${target.name}"`
      );
      assert(pill.removable, 'the world pill cannot be taken off');

      // The browse really is a browse: rows, from that world, with no query.
      // The pill renders before the rows land, so this waits for the rows.
      const browsed = await waitFor(
        'the world browse to render',
        async () => {
          const a = await js('window.__recallDebug.ask()');
          return a.hits > 0 ? a : null;
        },
        { timeout: 15000 }
      );
      const narrowed = browsed.hits;
      assert(narrowed > 0, 'the world filter returned nothing at all');

      // Taking the pill off leaves no question behind — which is the honest
      // answer, not an empty result set that reads as "nothing was said".
      await js('document.querySelector(\'#ask-pills [data-drop="world"]\').click()');
      const cleared = await waitFor(
        'the world pill to come off',
        async () => {
          const a = await js('window.__recallDebug.ask()');
          return !a.pills.some((p) => p.facet === 'world') ? a : null;
        },
        { timeout: 15000 }
      );
      assert(!cleared.shown, 'the pill row is still up with nothing in it');
      const prompt = await js(
        'document.querySelector("#search-results .empty b")?.textContent ?? ""'
      );
      assert(
        /search everything/i.test(prompt),
        `dropping the last facet left "${prompt}" rather than an invitation to ask something`
      );
      return {
        worlds: w.rows.map((r) => r.name),
        shares: w.digestShares.length,
        browsed: narrowed,
        file,
      };
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

    await step('a-new-voice-can-be-named-where-you-read-it', async () => {
      // A row spoken by a voice with no name yet (the mock's Speaker_07) opens
      // a sheet that offers a name field under the picker; Enter names it and
      // the daemon's relabel broadcast repaints the row. A named voice gets no
      // such offer here — renaming stays on the Speakers page.
      // The live window may hold nothing by an unnamed voice (the mock's tail
      // is mostly named people talking), so the driver makes one: the newest
      // row is reassigned to an unnamed voice first and put back at the end.
      await js('document.querySelector(\'.rail-item[data-view="transcript"]\').click()');
      await waitFor('transcript rows', async () => js('document.querySelectorAll("#seg-list .seg").length > 0'));
      const setup = await js(`(() => {
        const s = window.__recallDebug.store;
        const rows = [...document.querySelectorAll('#seg-list .seg')];
        const row = rows[rows.length - 1];
        if (!row) return null;
        const seg = s.segById.get(Number(row.dataset.seg));
        const unnamed = [...s.speakers.values()].find((sp) => !sp.name && !sp.you);
        return unnamed && seg ? { id: seg.id, was: seg.speaker ?? null, speaker: unnamed.id } : null;
      })()`);
      assert(setup, 'the mock has no unnamed voice to name');
      await js(`window.recall.request('segments.reassign', { segment_id: ${setup.id}, speaker_id: ${setup.speaker} })`);
      const target = await waitFor('the reassigned row', async () => {
        const v = await js(`(() => {
          const s = window.__recallDebug.store;
          const seg = s.segById.get(${setup.id});
          if (!seg || seg.speaker !== ${setup.speaker}) return null;
          const row = document.querySelector('#seg-list .seg[data-seg="${setup.id}"]');
          if (!row) return null;
          row.click();
          return { id: seg.id, speaker: seg.speaker, was: ${setup.was === null ? 'null' : setup.was} };
        })()`);
        return v;
      });
      await waitFor('the sheet', async () => js('!!document.querySelector(".sheet #segment-text")'));
      const before = await js('window.__recallDebug.accuracy()');
      assert(before.sheet.nameOffered, 'the sheet did not offer to name the voice');
      assert(/no name yet/.test(before.sheet.nameHint), `hint does not say so: "${before.sheet.nameHint}"`);
      const name = `Driver${Date.now() % 10000}`;
      await js(`(() => {
        const i = document.getElementById('name-voice');
        i.value = ${JSON.stringify(name)};
        i.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));
        return true;
      })()`);
      const named = await waitFor('the relabel to land', async () => {
        const v = await js(`(() => {
          const s = window.__recallDebug.store;
          const sp = s.speakers.get(${target.speaker});
          const row = document.querySelector('#seg-list .seg[data-seg="${target.id}"]');
          return { name: sp?.name ?? null, row: row ? row.textContent : '' , offered: !!document.getElementById('name-voice-row') && !document.getElementById('name-voice-row').hidden };
        })()`);
        // The row repaints from the relabel event and the sheet's offer repaints
        // a beat later; both are waited for, not asserted on the first paint.
        return v.name === name && v.row.includes(name) && !v.offered ? v : null;
      });
      // Put the fixture back so later steps meet the voice and the row they expect.
      await js(`window.recall.request('speakers.name', { id: ${target.speaker}, name: '' })`);
      await js(`window.recall.request('segments.reassign', { segment_id: ${target.id}, speaker_id: ${target.was === null ? 'null' : target.was} })`);
      await js('document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }))');
      return { segment: target.id, speaker: target.speaker, named: name };
    });

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

    // ---------------------------------------------------------------------
    // 6s — live captions (0.8.3)
    //
    // A second window, opened through the SAME function the tray item calls:
    // the step is about the path a person uses, not about a second path that
    // exists for tests. It has to be up BEFORE the nudge below, because the
    // thing it has to prove is what it does with a live feed — and the nudge is
    // the one moment in this run where a live turn and a re-published archive
    // row arrive together.
    // ---------------------------------------------------------------------

    await step('captions-open-on-top-of-everything', async () => {
      deps.captions.open();
      const w = await waitFor('the captions window', async () => deps.captions.window() ?? null);
      if (w.webContents.isLoading()) await new Promise((r) => w.webContents.once('did-finish-load', r));
      await waitFor('the captions window to seed itself', async () =>
        capJs('!!window.__captionsDebug && window.__captionsDebug.seeded()').catch(() => false)
      );

      // The four facts that make it furniture rather than a window: it floats,
      // it is not in the switcher, it has no frame, and the pointer goes
      // straight through it into whatever is underneath.
      assert(w.isAlwaysOnTop(), 'the captions window is not always on top');
      assert(!w.isVisible() === false, 'the captions window is not visible');
      const s = deps.captions.settings();
      assert(s.clickThrough === true, 'captions are not click-through by default');

      // The one hard rule of this surface: the ground is DARK on both of NX
      // Clear's grounds, because captions float over somebody else's pixels and
      // a light slab over a dark game is worse than no captions. Read as a
      // computed colour, not as a class — a token nobody can see is not a rule.
      const g = await capJs('window.__captionsDebug.ground()');
      const rgb = /rgba?\(\s*(\d+)[,\s]+(\d+)[,\s]+(\d+)/.exec(g.row);
      assert(rgb, `the captions ground is not a colour: ${g.row}`);
      const lum = (Number(rgb[1]) * 299 + Number(rgb[2]) * 587 + Number(rgb[3]) * 114) / 1000;
      assert(lum < 40, `the captions ground is not dark (${g.row}, luma ${lum.toFixed(1)}) on the ${deps.theme?.().forced ?? 'system'} pass`);
      assert(g.theme === 'dark', `the captions document is stamped "${g.theme}"`);

      return { alwaysOnTop: w.isAlwaysOnTop(), settings: s, ground: g, luma: Number(lum.toFixed(1)) };
    });

    // The live feed itself, before the nudge: turns arrive, they are bounded by
    // the `turns` setting, and the sibling `translation` track renders under
    // the original rather than instead of it.
    await step('captions-show-the-live-feed-and-nothing-else', async () => {
      const rows = await waitFor(
        'captions rows',
        async () => {
          const r = await capJs('window.__captionsDebug.rows()');
          return r.length ? r : null;
        },
        { timeout: 20000 }
      );
      const turns = deps.captions.settings().turns;
      assert(rows.length <= turns, `${rows.length} rows on screen for a ${turns}-turn setting`);
      assert(rows.every((r) => r.text.length), 'a caption row has no words in it');
      // Large type is the entire point of the surface.
      assert(parseFloat(rows[0].size) >= 18, `captions are set at ${rows[0].size}`);

      // The `translation` sibling track. The mock puts one on two canned lines,
      // so it takes a lap of the feed to come round — and its absence on every
      // other row is the other half of the contract.
      const tr = await waitFor(
        'a translated turn',
        async () => {
          const fed = await capJs(`(() => {
            const rows = window.__captionsDebug.rows();
            const one = rows.find((r) => r.translation);
            return one ?? null;
          })()`);
          return fed;
        },
        { timeout: 45000 }
      );
      assert(tr.translation.includes('EN') || tr.translation.length > 0, `no translation rendered: ${JSON.stringify(tr)}`);
      assert(tr.translation !== tr.text, 'the translation replaced the original instead of sitting under it');

      const file = await shotOf(capWin(), 'captions');
      return { rows: rows.length, turns, size: rows[0].size, translated: tr.translation.slice(0, 48), file };
    });

    // A note arriving live goes to the TOP of the list — it is the newest thing
    // you said to yourself, and it is why you are looking. The mock delivers it
    // on the first SIGUSR2, because a note that turns up at second 19 of a run
    // lands in whatever step happens to be on screen and proves nothing.
    if (process.env.NX_RECALL_MOCK_PID) {
      // When the nudge went out, so the captions step below can say how long it
      // took rather than how long the driver took to get round to asking.
      let nudgeAt = null;

      await step('a-note-arriving-live-goes-to-the-top', async () => {
        const before = await js('window.__recallDebug.accuracy()');
        nudgeAt = Date.now();
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

      // The same two events, read from the OTHER window. This is the step the
      // captions feature exists to survive: one SIGUSR2 delivered a turn that
      // was said just now AND an archive row the re-decode worker re-published,
      // and the caption bar has to show exactly one of them.
      await step('captions-take-the-live-turn-and-refuse-the-archive-row', async () => {
        const noteSaid = 'the portal in the stairwell only opens at night';
        const row = await waitFor(
          'the live turn in the captions',
          async () => {
            const rows = await capJs('window.__captionsDebug.rows()');
            return rows.find((r) => r.text.includes(noteSaid)) ?? null;
          },
          { timeout: 8000 }
        );
        // Not "the driver found it in eight seconds" — when the window actually
        // rendered it, measured from the instant the nudge went out.
        const latency = row.at - nudgeAt;
        assert(latency >= 0 && latency <= 2000, `the captions took ${latency} ms to show the live turn`);
        // It came off the microphone, so it is the user's own voice: the one
        // place violet is spent in that window.
        assert(row.you, 'a turn from your own microphone is not marked as yours in the captions');

        // And the archive row is not there — not on screen, and not even in the
        // feed the window has been handed. `applyEvent` refuses it because this
        // window seeded itself from the live tail first, which is the whole
        // reason the seeding exists.
        const headMs = await capJs('window.__captionsDebug.headMs()');
        const fed = await capJs('window.__captionsDebug.fed()');
        assert(headMs != null, 'the captions window never learned where the live window starts');
        const stale = fed.filter((f) => f.t_ms < headMs);
        assert(stale.length === 0, `re-published archive rows reached the captions: ${JSON.stringify(stale)}`);
        const rows = await capJs('window.__captionsDebug.rows()');
        assert(rows.every((r) => r.t_ms >= headMs), 'a caption on screen is older than the live window');

        return { latency, fed: fed.length, rows: rows.length, headMs };
      });

      // ---------------------------------------------------------------
      // 0.11.0 — partial turns. The second SIGUSR2 delivers ONE turn the way
      // a turn really arrives: three provisional readings a second apart, all
      // with the same `t_start_ns`, and then the `segment` that replaces them.
      //
      // Four claims, and every one of them is about a row that must not be
      // there afterwards: the provisional row appears, it UPDATES rather than
      // stacking, exactly one final row is left, and the model holds no
      // partial once the turn has landed.
      // ---------------------------------------------------------------
      await step('a-partial-turn-updates-in-place-and-is-replaced-by-one-row', async () => {
        // The main window is on the transcript and following, so the SAME turn
        // has to produce a live tail there too — and it must be a tail rather
        // than a row: outside `#seg-list`, and not in the count.
        await js('document.querySelector(\'.rail-item[data-view="transcript"]\').click()');
        // …and back ON the tail. Earlier steps read history, played a
        // conversation back and jumped to a day, and the live tail is for the
        // tail: a provisional row under July would be a row in the wrong place.
        // That refusal is the behaviour, so the test has to undo it to see the
        // row at all.
        await js(`(() => {
          const b = document.getElementById('follow-btn');
          if (b && b.getAttribute('aria-pressed') !== 'true') b.click();
        })()`);
        await waitFor('the transcript to be following again', async () =>
          js('window.__recallDebug.scrollback().following')
        );
        process.kill(Number(process.env.NX_RECALL_MOCK_PID), 'SIGUSR2');

        // 1. It appears, lighter and unfinished.
        const first = await waitFor(
          'the provisional caption',
          async () => capJs('window.__captionsDebug.partial()'),
          { timeout: 10000 }
        );
        assert(first.ellipsis, 'a provisional caption does not say it is unfinished');
        assert(first.text.trim().length > 0, 'the provisional caption is empty');
        // Its speaker is a proximity guess or nobody, never a settled label
        // (PROTOCOL 0.11.0: no embedding is computed for a partial).
        const firstText = first.text;

        // 2. It updates IN PLACE. The sequence number rises and the words grow;
        // what must NOT happen is a second provisional row appearing under it.
        // The transcript's own live tail is read INSIDE this wait rather than
        // after it: both windows are fed by the same event and the turn settles
        // a second later, so a second round trip is a second in which the row
        // it is asking about has legitimately gone.
        let tailWhileOpen = null;
        const grown = await waitFor(
          'the provisional caption to be re-read',
          async () => {
            const p = await capJs('window.__captionsDebug.partial()');
            tailWhileOpen = (await js('window.__recallDebug.partial()')).drawn ?? tailWhileOpen;
            return p && p.seq > first.seq && p.text !== firstText ? p : null;
          },
          { timeout: 10000 }
        );
        const stacked = await capJs('document.querySelectorAll(".cap-row.provisional").length');
        assert(stacked === 1, `${stacked} provisional rows are on screen; there can only be one`);
        // Lighter ink than a settled row: the app saying these words are a
        // first reading and first readings are often wrong (FINDINGS §12).
        const settled = await capJs(
          '(() => { const el = document.querySelector(".cap-row:not(.provisional) .cap-text"); return el ? getComputedStyle(el).color : null; })()'
        );
        assert(!settled || settled !== grown.dim, `the provisional row is inked like a settled one (${grown.dim})`);
        // The transcript's own live tail, while it is still up. It has to be
        // OUTSIDE `#seg-list` (`.seg:last-of-type`, the trim loop and the
        // separator walker all reason about the last child of that list) and it
        // must not be counted.
        assert(tailWhileOpen, 'the transcript drew no live tail for a turn being said now');
        const sawTail = tailWhileOpen;
        assert(sawTail.ellipsis, 'the transcript tail does not say the sentence is unfinished');
        assert(!sawTail.inList, 'the transcript tail is inside #seg-list, where it would become "the last segment"');
        const modelRows = await js('window.__recallDebug.scrollback().rows');
        assert(
          sawTail.counted === modelRows,
          `the tail is being counted as a segment: ${sawTail.counted} rows against ${modelRows}`
        );
        const file = await shotOf(capWin(), 'captions-partial');

        // 3. The final row replaces it — ONE settled row for the turn, not two,
        // and no provisional row left behind.
        //
        // Counted by matching the words rather than by the length of the stack:
        // the bar shows the last N turns and the mock's ordinary feed keeps
        // arriving, so "the stack got longer" is not a fact about this turn.
        // "The provisional reading appears in exactly one settled row" is.
        const needle = firstText.replace('…', '').trim();
        const done = await waitFor(
          'the turn to settle',
          async () => {
            const p = await capJs('window.__captionsDebug.partial()');
            if (p) return null;
            const rows = await capJs('window.__captionsDebug.rows()');
            const hits = rows.filter((r) => r.text.includes(needle));
            return hits.length ? { rows, hits } : null;
          },
          { timeout: 15000 }
        );
        assert(done.hits.length === 1, `${done.hits.length} rows carry this turn's words; one turn is one row`);
        const settledRow = done.hits[0];
        assert(!settledRow.text.includes('…'), `the settled row is still hedged: "${settledRow.text}"`);
        // The provisional reading survived into the final, which is what
        // "converges" means and what lets a client replace rather than append.
        assert(
          settledRow.text.length > needle.length,
          `the final row is no longer than the provisional one: "${settledRow.text}"`
        );
        const added = done.hits.length;

        // 4. And the MODEL holds nothing: a partial is never stored, never
        // counted, and never left behind (PROTOCOL 0.11.0).
        const held = await capJs('window.__captionsDebug.partialModel()');
        assert(held == null, `the captions model is still holding a partial: ${JSON.stringify(held)}`);
        // …and neither does the main window's. Its tail is gone with the same
        // event, and the row that replaced it is an ordinary segment in the
        // list — never a leftover under it.
        const tail = await js('window.__recallDebug.partial()');
        assert(tail.model == null, `the transcript model is still holding a partial: ${JSON.stringify(tail.model)}`);
        assert(tail.drawn == null, 'the transcript is still drawing a live tail after the turn landed');
        assert(
          (await js('document.querySelectorAll("#seg-list .seg.partial").length')) === 0,
          'a provisional row leaked into the segment list'
        );
        return { seqs: [first.seq, grown.seq], added, provisional: firstText.slice(0, 40), tail: sawTail, file };
      });

      // ---- 0.12.4, sliced turns ------------------------------------------
      //
      // The claim: a long turn arrives in PIECES and grows ONE row, on both
      // grounds, and the row that finally settles is one row carrying every
      // piece. The failure this is written against is the obvious
      // implementation — a new row per slice — which would turn one sentence
      // into three captions and push the rest of the conversation off the bar.
      await step('a-sliced-turn-grows-one-row-on-both-surfaces', async () => {
        await js('document.querySelector(\'.rail-item[data-view="transcript"]\').click()');
        await js(`(() => {
          const b = document.getElementById('follow-btn');
          if (b && b.getAttribute('aria-pressed') !== 'true') b.click();
        })()`);
        await waitFor('the transcript to be following again', async () =>
          js('window.__recallDebug.scrollback().following')
        );
        // The mock's third delivery: three slices of one monologue, then the
        // row. (The second was the partial turn the step above drove.)
        process.kill(Number(process.env.NX_RECALL_MOCK_PID), 'SIGUSR2');

        // 1. The first slice appears as a GROWING row — unfinished, but not
        // hedged: its words came from their own audio at a boundary the VAD
        // found and will not be taken back.
        const first = await waitFor(
          'the first slice',
          async () => {
            const p = await capJs('window.__captionsDebug.partial()');
            return p && p.growing ? p : null;
          },
          { timeout: 10000 }
        );
        assert(first.ellipsis, 'a growing row does not say the sentence is unfinished');
        assert(first.text.trim().length > 0, 'the growing row is empty');

        // 2. It GROWS. The distinguishing property against a partial: the text
        // that was there is still there, with more after it. A renderer that
        // replaced the row wholesale would pass a "the text changed" assertion
        // and fail this one.
        const opening = first.text.replace('…', '').trim().slice(0, 40);
        let tailWhileOpen = null;
        const grown = await waitFor(
          'the row to grow',
          async () => {
            const p = await capJs('window.__captionsDebug.partial()');
            tailWhileOpen = (await js('window.__recallDebug.partial()')).drawn ?? tailWhileOpen;
            return p && p.growing && p.seq > first.seq && p.text.length > first.text.length ? p : null;
          },
          { timeout: 10000 }
        );
        assert(
          grown.text.includes(opening),
          `the row was rewritten rather than grown — "${opening}" is gone from "${grown.text}"`
        );
        const stacked = await capJs('document.querySelectorAll(".cap-row.provisional").length');
        assert(stacked === 1, `${stacked} growing rows are on screen; one turn is one row`);
        // Settled ink, unlike a partial. The words are not in doubt.
        const settledInk = await capJs(
          '(() => { const el = document.querySelector(".cap-row:not(.provisional) .cap-text"); return el ? getComputedStyle(el).color : null; })()'
        );
        assert(
          !settledInk || settledInk === grown.dim,
          `a growing row is inked like a guess (${grown.dim} against ${settledInk})`
        );

        // 3. The transcript's own live tail did the same thing, and is still a
        // TAIL: outside `#seg-list`, and not counted as a segment.
        assert(tailWhileOpen, 'the transcript drew no live tail for a turn being sliced');
        assert(tailWhileOpen.growing, 'the transcript tail is not marked as growing');
        assert(tailWhileOpen.ellipsis, 'the transcript tail does not say the sentence is unfinished');
        assert(!tailWhileOpen.inList, 'the transcript tail is inside #seg-list, where it would become "the last segment"');
        const modelRows = await js('window.__recallDebug.scrollback().rows');
        assert(
          tailWhileOpen.counted === modelRows,
          `the growing tail is being counted as a segment: ${tailWhileOpen.counted} against ${modelRows}`
        );
        const file = await shotOf(capWin(), 'captions-sliced');

        // 4. It settles into ONE row carrying the whole monologue — every piece
        // that was ever on the glass, in order, in one place.
        const done = await waitFor(
          'the sliced turn to settle',
          async () => {
            const p = await capJs('window.__captionsDebug.partial()');
            if (p) return null;
            const rows = await capJs('window.__captionsDebug.rows()');
            const hits = rows.filter((r) => r.text.includes(opening));
            return hits.length ? { rows, hits } : null;
          },
          { timeout: 20000 }
        );
        assert(done.hits.length === 1, `${done.hits.length} rows carry this turn; one turn is one row`);
        const settled = done.hits[0];
        assert(!settled.text.includes('…'), `the settled row is still hedged: "${settled.text}"`);
        // The last slice's words are in the final row: the pieces were JOINED
        // rather than the last one winning.
        const lastPiece = grown.text.replace('…', '').trim().slice(-40);
        assert(
          settled.text.includes(lastPiece),
          `the settled row lost a slice — "${lastPiece}" is not in "${settled.text}"`
        );
        const tail = await js('window.__recallDebug.partial()');
        assert(tail.drawn == null, 'the transcript is still drawing a tail after the turn landed');
        assert(tail.model == null, 'the growing row survived the segment that replaces it');
        assert(
          (await js('document.querySelectorAll("#seg-list .seg.partial").length')) === 0,
          'a growing row leaked into the segment list'
        );
        return { seqs: [first.seq, grown.seq], words: settled.text.split(' ').length, file };
      });

      // …and it goes away again through the same toggle the tray offers.
      await step('captions-close-from-the-same-place-they-opened', async () => {
        deps.captions.close();
        await sleep(400);
        assert(deps.captions.window() == null, 'the captions window survived being closed');
        const labels = deps.buildTrayMenu().items.map((i) => i.label).filter(Boolean);
        assert(labels.some((l) => /^Captions$/.test(l)), `the tray no longer offers to open them: ${labels.join(' | ')}`);
        return { labels };
      });
    }

    // ---- 0.9.0, the assistant --------------------------------------------

    await step('a-note-with-a-date-is-a-reminder-and-says-so', async () => {
      await js('document.querySelector(\'.rail-item[data-view="memory"]\').click()');
      const a = await waitFor('the notes card', async () => {
        const a = await js('window.__recallDebug.accuracy()');
        return a.notes.length ? a : null;
      });
      const timed = a.notes.filter((n) => n.due != null);
      assert(timed.length >= 2, `only ${timed.length} notes carry a date`);
      // The chip is the whole difference between a note and a reminder, and it
      // has to be readable rather than merely present.
      for (const n of timed) {
        assert(n.dueChip, `note ${n.id} has a date and no chip`);
      }
      // A note with no date wears no chip: a reminder is a note with a date on
      // it, not a second kind of row.
      const plain = a.notes.filter((n) => n.due == null);
      assert(plain.length, 'the fixture has no undated note left to compare against');
      assert(
        plain.every((n) => !n.dueChip),
        'a note with no date is wearing a due chip'
      );
      // …and one that has already come round says so quietly rather than
      // claiming to be due.
      const fired = timed.find((n) => n.fired);
      assert(fired, 'no fired reminder in the fixture');
      assert(
        /reminded/i.test(fired.dueChip),
        `a reminder that has gone off reads "${fired.dueChip}"`
      );
      // Snooze is offered on open notes only.
      const open = a.notes.filter((n) => n.state === 'open');
      assert(open.every((n) => n.snoozes.length === 3), 'an open note is missing its snoozes');
      assert(
        a.notes.filter((n) => n.state !== 'open').every((n) => !n.snoozes.length),
        'a settled note offers a snooze'
      );
      await js('document.getElementById("notes-card").scrollIntoView({ block: "start" })');
      const file = await shot('memory-reminders');
      return { timed: timed.length, chips: timed.map((n) => n.dueChip), file };
    });

    await step('a-snooze-moves-the-date-and-the-daemon-agrees', async () => {
      const before = await js('window.__recallDebug.accuracy()');
      const target = before.notes.find((n) => n.state === 'open' && n.due != null);
      assert(target, 'no open reminder to snooze');
      await js(`document.querySelector('[data-snooze="60"][data-note="${target.id}"]').click()`);
      const after = await waitFor('the snooze to land', async () => {
        const a = await js('window.__recallDebug.accuracy()');
        const row = a.notes.find((n) => n.id === target.id);
        return row && !row.pending && row.due !== target.due ? row : null;
      });
      assert(after.due > target.due, 'the date did not move forward');
      assert(!after.fired, 'a snooze must un-fire the reminder');
      // The daemon really has it, not just this window.
      const onWire = await js(`(async () => {
        const r = await window.recall.request('notes.list', {});
        const n = (r.data.notes || []).find(x => x.id === ${target.id});
        return n ? { due: n.due_ms, fired: n.fired, state: n.state } : null;
      })()`);
      assert(onWire, 'the note is gone from the daemon');
      assert(onWire.state === 'open', `the daemon says ${onWire.state}`);
      assert(onWire.fired === false, 'the daemon still has it fired');
      assert(onWire.due === after.due, `the daemon says ${onWire.due}, the window says ${after.due}`);
      return { was: target.due, now: after.due, chip: after.dueChip };
    });

    if (process.env.NX_RECALL_MOCK_PID) {
      await step('a-reminder-coming-round-raises-a-toast-that-opens-the-note', async () => {
        // The mock's second SIGUSR2 fires a reminder and a digest — both are
        // things a canned world cannot produce on its own.
        await js('document.querySelector(\'.rail-item[data-view="transcript"]\').click()');
        process.kill(Number(process.env.NX_RECALL_MOCK_PID), 'SIGUSR2');
        const toast = await waitFor(
          'the reminder toast',
          async () =>
            js(`(() => {
              const t = [...document.querySelectorAll('#toasts .toast')]
                .find((x) => /reminder/i.test(x.textContent));
              return t ? { text: t.textContent, clickable: t.classList.contains('clickable') } : null;
            })()`),
          { timeout: 12000 }
        );
        assert(toast.clickable, 'the reminder toast is not pressable');
        const file = await shot('reminder-toast');
        // Pressing it goes to Memory and lands on the row, from the transcript.
        await js(`[...document.querySelectorAll('#toasts .toast')].find((x) => /reminder/i.test(x.textContent)).click()`);
        const landed = await waitFor('the note to be focused', async () => {
          const v = await js(`(() => ({
            view: window.__recallDebug.view(),
            flashed: document.querySelectorAll('.note-row.flash').length,
          }))()`);
          return v.view === 'memory' && v.flashed ? v : null;
        });
        return { toast: toast.text.slice(0, 60), view: landed.view, file };
      });

      await step('a-digest-arriving-live-goes-to-the-top-of-yesterday', async () => {
        const a = await waitFor(
          'the digest card',
          async () => {
            const v = await js('window.__recallDebug.assistant()');
            return v.digests.rows.length >= 3 ? v : null;
          },
          { timeout: 12000 }
        );
        assert(a.digests.shown, 'the Yesterday card is hidden with digests in it');
        // A digest is written once per conversation, so no thread may appear
        // twice however many events arrive.
        const threads = a.digests.rows.map((r) => r.thread);
        assert(new Set(threads).size === threads.length, `a conversation is summarised twice: ${threads}`);
        // Every row is a paragraph with the people who were in it.
        for (const row of a.digests.rows) {
          assert(row.summary.length > 40, `a digest of ${row.summary.length} characters is not a paragraph`);
          assert(row.people.length >= 1, `digest ${row.thread} names nobody`);
        }
        assert(/summarised/.test(a.digests.sub), `the card's sub reads "${a.digests.sub}"`);
        // 0.11.6 — the paragraph is about PEOPLE. Read off the rendered card
        // rather than the payload: the chips and the sentence beside them
        // have to be calling the same person the same thing, and a digest
        // that still says "A und B" is the bug this round closed.
        const named = await js(`[...document.querySelectorAll('.digest-row')].map((r) => ({
          thread: Number(r.dataset.digest),
          summary: r.querySelector('.digest-text')?.textContent ?? '',
          chips: [...r.querySelectorAll('.chip.person')].map((c) => c.textContent.trim()),
        }))`);
        for (const row of named) {
          assert(
            row.chips.some((name) => name && row.summary.includes(name)),
            `digest ${row.thread} names none of ${JSON.stringify(row.chips)}: "${row.summary}"`
          );
          assert(
            !/(^|[^\wÄÖÜäöüß])[AB]([^\wÄÖÜäöüß]|$)/.test(row.summary),
            `digest ${row.thread} still calls somebody by a letter: "${row.summary}"`
          );
        }
        await js('document.getElementById("digest-card").scrollIntoView({ block: "start" })');
        const file = await shot('memory-yesterday');
        return { rows: a.digests.rows.length, groups: a.digests.groups, file };
      });

      await step('a-digest-opens-the-conversation-it-is-about', async () => {
        const before = await js('window.__recallDebug.assistant()');
        const row = before.digests.rows[0];
        await js(`document.querySelector('[data-digest="${row.thread}"]').click()`);
        const landed = await waitFor('the transcript', async () =>
          js(`(() => {
            const v = window.__recallDebug.view();
            return v === 'transcript' ? { view: v, rows: document.querySelectorAll('.seg').length } : null;
          })()`)
        );
        assert(landed.rows > 0, 'the transcript is empty after opening a digest');
        return { thread: row.thread, ...landed };
      });
    }

    await step('a-translated-turn-keeps-the-words-that-were-said', async () => {
      await js('document.querySelector(\'.rail-item[data-view="transcript"]\').click()');
      const v = await waitFor('translated rows', async () => {
        const v = await js('window.__recallDebug.assistant()');
        return v.translated.rows ? v : null;
      });
      assert(v.translated.rows > 0, 'no translated rows on screen');
      // The original is still there, above the reading, and they are not the
      // same string: a translation that replaced the words would be a
      // quotation nobody uttered.
      for (const pair of v.translated.pairs) {
        assert(pair.said, 'a translated row lost the words that were said');
        assert(pair.reading, 'a translated row has an empty reading');
        assert(pair.said !== pair.reading, `the reading is the original: ${pair.said}`);
        assert(pair.lang === 'de', `the reading claims to be "${pair.lang}"`);
        assert(pair.via, 'the reading does not say which model wrote it');
      }
      // Rows with nothing to translate render exactly as they always did.
      assert(v.translated.plain > 0, 'every row on screen is translated, which is not the rule');
      const file = await shot('transcript-translated');
      return { translated: v.translated.rows, plain: v.translated.plain, file };
    });

    // 0.10.2 — the three controls, and the one that changes the transcript.
    await step('the-translation-card-sets-what-is-translated-and-how-it-reads', async () => {
      // Read the transcript BEFORE leaving it: `#seg-list` only exists while
      // that view is mounted, and a row count taken from the Memory view is
      // zero for a reason that has nothing to do with translation.
      const before = await js('window.__recallDebug.assistant()');
      assert(before.translated.main > 0, 'the mock ships "main" and no row leads with its translation');
      assert(
        before.translated.first.every((c) => c.includes('txt-translated')),
        `the translation is not the first line: ${JSON.stringify(before.translated.first)}`
      );

      await js('document.querySelector(\'.rail-item[data-view="settings"]\').click()');
      await js(`document.querySelector('button[data-settings-category="language"]').click()`);
      const card = await waitFor('the translation card', async () => {
        const t = await js('window.__recallDebug.translation()');
        return t.target !== null && t.read.length ? t : null;
      });
      // All three, and the target selector's "off" option: translation being on
      // is not a fourth control, it is what having a target means.
      assert(card.targets.includes(''), 'the target selector cannot be turned off');
      assert(card.targets.length > 10, `only ${card.targets.length} languages offered`);
      assert(card.modes.join(',') === 'main,under', `display modes: ${card.modes}`);
      assert(card.target === 'de', `the mock translates into "${card.target}"`);
      // The target's own chip is on and is not yours to switch off.
      const de = card.read.find((r) => r.code === 'de');
      assert(de?.on && de?.locked, `the target's chip is not locked on: ${JSON.stringify(de)}`);
      assert(card.read.some((r) => r.code === 'en' && !r.on), 'English is already read, so nothing would translate');

      // 1. Flipping the display re-renders a translated ROW, which is the point
      //    of the setting — the card is not the surface it changes.
      await js('document.getElementById("translate-display-under").click()');
      await js('document.querySelector(\'.rail-item[data-view="transcript"]\').click()');
      const under = await waitFor('the original leading', async () => {
        const v = await js('window.__recallDebug.assistant()');
        return v.translated.rows && v.translated.main === 0 ? v : null;
      });
      assert(
        under.translated.first.every((c) => c.includes('txt-said')),
        `"under" did not put the original first: ${JSON.stringify(under.translated.first)}`
      );
      // Both lines are still there in both modes. Nothing here removes a line.
      for (const pair of under.translated.pairs) assert(pair.said && pair.reading, 'a line went missing');

      // …and back, where the original keeps its language code so the reader
      // can see what they are being shown instead of.
      await js('document.querySelector(\'.rail-item[data-view="settings"]\').click()');
      await js(`document.querySelector('button[data-settings-category="language"]').click()`);
      await js('document.getElementById("translate-display-main").click()');
      await js('document.querySelector(\'.rail-item[data-view="transcript"]\').click()');
      const main = await waitFor('the translation leading again', async () => {
        const v = await js('window.__recallDebug.assistant()');
        return v.translated.main > 0 ? v : null;
      });
      assert(main.translated.saidLangs.length > 0, 'the original lost its language code');

      // 2. Setting the target moves the card's own badge, and takes the new
      //    target's chip with it.
      await js('document.querySelector(\'.rail-item[data-view="settings"]\').click()');
      await js(`document.querySelector('button[data-settings-category="language"]').click()`);
      await js(`(() => {
        const s = document.getElementById('translate-target');
        s.value = 'en';
        s.dispatchEvent(new Event('change', { bubbles: true }));
      })()`);
      // The badge and the chip repaint from the same `assist` event, but the
      // probe can land between the two paints — so both are waited for.
      const en = await waitFor('the new target, badge and chip', async () => {
        const t = await js('window.__recallDebug.translation()');
        const chip = (t.read || []).find((r) => r.code === 'en');
        return t.target === 'en' && chip?.on && chip?.locked ? t : null;
      });
      assert(/English/.test(en.sub), `the badge still says "${en.sub}"`);

      // 3. A language you read is a chip you can turn off. French is not read,
      //    so pressing it says "leave French alone".
      await js('document.querySelector(\'#translate-read .toggle-chip[data-lang="fr"]\').click()');
      const fr = await waitFor('French read', async () => {
        const t = await js('window.__recallDebug.translation()');
        return t.read.find((r) => r.code === 'fr')?.on ? t : null;
      });
      assert(fr.read.find((r) => r.code === 'fr').on, 'the chip did not go on');

      await js('document.getElementById("translate-card").scrollIntoView({ block: "start" })');
      const file = await shot('memory-translation');
      return { targets: card.targets.length, target: en.target, sub: en.sub, file };
    });

    await step('the-accuracy-card-is-honest-arithmetic', async () => {
      await js('document.querySelector(\'.rail-item[data-view="settings"]\').click()');
      await js(`document.querySelector('button[data-settings-category="quality"]').click()`);
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
      // 0.12.4: the one line that looks forwards. Under the bar it must say
      // how many more corrections are wanted — a card that only reported
      // "nothing learned" would leave the reader with nothing to do about it.
      assert(/learned from \d+ correction/i.test(a.dash.learned), `no learned line: "${a.dash.learned}"`);
      assert(/\d+ more/.test(a.dash.learned), `the countdown is missing: "${a.dash.learned}"`);
      return {
        corrections: a.dash.corrections,
        wer: a.dash.wer,
        sources: a.dash.bySource,
        speakers: a.dash.bySpeaker,
        learned: a.dash.learned,
      };
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
      await js('document.querySelector(\'.rail-item[data-view="settings"]\').click()');
      await js(`document.querySelector('button[data-settings-category="processing"]').click()`);
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
      await js('document.querySelector(\'.rail-item[data-view="settings"]\').click()');
      await js(`document.querySelector('button[data-settings-category="processing"]').click()`);
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

    // 7b — per-person highlights (0.12.0). Set a colour and an emoji through
    // the real picker, see them on the transcript row and the person page,
    // clear them, and see them GO. The last half is the point: a highlight
    // that can be set and not unset is a decoration somebody is stuck with.
    await step('highlight-set-and-clear', async () => {
      // A voice the fixture leaves unhighlighted, so "it appeared" means this
      // step put it there rather than the fixture having it all along. Kira (1)
      // and Ash (4) ship highlighted precisely so the OTHER assertions here —
      // that a highlight survives a reload and rides the wire — have something
      // to look at.
      const target = 2;
      await js('document.querySelector(\'.rail-item[data-view="transcript"]\').click()');
      await waitFor('transcript rows', async () => js('document.querySelectorAll("#seg-list .seg").length > 0'));

      // The fixture's two highlighted people are visible without anybody
      // touching a control — the "shipped state" half of the check.
      const fixture = await js(`(() => {
        const icons = [...document.querySelectorAll('#seg-list [data-sp="1"] .sp-icon')];
        const rows = [...document.querySelectorAll('#seg-list .seg.person-hl')];
        // KIRA's row, not \`rows[0]\`. Both fixture-highlighted voices (Kira,
        // violet; Ash, teal) wear the accent, so "the first accented row" is
        // whichever of them the resident window happens to start with — an
        // assumption about trimming that this step never meant to make, and
        // that flipped the moment the feed grew by one row.
        // data-sp is on the row's .who cell, not on the row itself.
        const kira = rows.find((r) => r.querySelector('[data-sp="1"]'));
        return {
          kiraIcons: icons.length,
          kiraIcon: icons[0]?.textContent || '',
          accented: rows.length,
          accentColour: kira?.style.getPropertyValue('--person-hl') || '',
        };
      })()`);
      assert(fixture.kiraIcons > 0, 'the fixture-highlighted voice shows no icon on the transcript');
      assert(fixture.kiraIcon === '\u{1F319}', `wrong icon on the fixture voice: ${JSON.stringify(fixture.kiraIcon)}`);
      assert(fixture.accented > 0, 'no transcript row wears the highlight accent');
      assert(/^hsl\(268 /.test(fixture.accentColour), `the accent is not the violet token: "${fixture.accentColour}"`);

      // What the unhighlighted voice looks like BEFORE, so "unchanged" is a
      // measurement rather than a hope.
      const before = await js(`(() => {
        const nm = document.querySelector('#seg-list [data-sp="${target}"] .nm');
        const row = nm?.closest('.seg');
        return {
          rows: document.querySelectorAll('#seg-list [data-sp="${target}"]').length,
          colour: nm?.style.color || '',
          icons: document.querySelectorAll('#seg-list [data-sp="${target}"] .sp-icon').length,
          accent: !!row?.classList.contains('person-hl'),
        };
      })()`);
      assert(before.rows > 0, 'the voice under test has no transcript rows');
      assert(before.icons === 0, 'the unhighlighted voice already had an icon');
      assert(before.accent === false, 'the unhighlighted voice already had a row accent');

      // Open the segment sheet on one of its rows — this is the transcript's
      // name row, where naming a voice lives and where the picker now sits.
      await js(`document.querySelector('#seg-list [data-sp="${target}"]').closest('.seg').click()`);
      await waitFor('the segment sheet', async () => js('!!document.getElementById("highlight-row")'));
      const pickerVisible = await js(
        '(() => { const r = document.getElementById("highlight-row"); return !r.hidden && !!r.querySelector(".hl-sw"); })()'
      );
      assert(pickerVisible, 'the highlight picker is not shown for a picked voice');

      // Click a swatch, and type an emoji. Both go through the real controls.
      await js('document.querySelector(\'#highlight-row .hl-sw[data-hl-colour="amber"]\').click()');
      await waitFor('the colour to land', async () =>
        js(`window.__recallDebug.store.speakers.get(${target})?.colour === 'amber'`)
      );
      await js(`(() => {
        const i = document.querySelector('#highlight-row .hl-icon');
        i.value = '\u{2B50}';
        i.dispatchEvent(new Event('input', {bubbles: true}));
        i.dispatchEvent(new KeyboardEvent('keydown', {key: 'Enter', bubbles: true}));
        return true;
      })()`);
      await waitFor('the icon to land', async () =>
        js(`window.__recallDebug.store.speakers.get(${target})?.icon === '\u{2B50}'`)
      );

      // The sheet is in the way of the rows it changed. Escape closes it, and
      // nothing needs saving: a swatch writes through `speakers.set` on click,
      // the way `startRename` does — there is no draft to lose.
      await js('document.dispatchEvent(new KeyboardEvent("keydown", {key: "Escape", bubbles: true}))');
      await waitFor('the sheet to close', async () => js('!document.querySelector(".sheet")'));

      // On the transcript: the name is amber, the icon is in front of it, and
      // the row wears the thin accent.
      const after = await js(`(() => {
        const nm = document.querySelector('#seg-list [data-sp="${target}"] .nm');
        const row = nm?.closest('.seg');
        const who = nm?.closest('.who');
        return {
          colour: nm?.style.color || '',
          icon: who?.querySelector('.sp-icon')?.textContent || '',
          iconFirst: who?.firstElementChild?.classList.contains('dot') &&
                     who?.children[1]?.classList.contains('sp-icon') &&
                     who?.children[2]?.classList.contains('nm'),
          accent: !!row?.classList.contains('person-hl'),
          accentColour: row?.style.getPropertyValue('--person-hl') || '',
        };
      })()`);
      assert(/^hsl\(44 /.test(after.colour), `the transcript name is not amber: "${after.colour}"`);
      assert(after.icon === '\u{2B50}', `the transcript row has no icon: ${JSON.stringify(after.icon)}`);
      assert(after.iconFirst, 'the icon is not between the dot and the name');
      assert(after.accent, 'the transcript row gained no accent');
      assert(/^hsl\(44 /.test(after.accentColour), `the row accent is not amber: "${after.accentColour}"`);

      // And on the person page, which is the other place the picker lives.
      await js(`window.__recallDebug.go('person', { id: ${target} })`);
      const person = await waitFor('the person page', async () => {
        const got = await js(`(() => {
          const n = document.querySelector('.person-name');
          if (!n) return null;
          const who = n.closest('.person-who') || n.parentElement;
          return {
            colour: n.style.color || '',
            icon: who?.querySelector('.sp-icon')?.textContent || '',
            picker: !!document.querySelector('.hl-picker'),
            pressed: document.querySelector('.hl-sw[data-hl-colour="amber"]')?.getAttribute('aria-pressed') || '',
          };
        })()`);
        return got && got.picker ? got : null;
      });
      assert(/^hsl\(44 /.test(person.colour), `the person page name is not amber: "${person.colour}"`);
      assert(person.icon === '\u{2B50}', 'the person page shows no icon');
      assert(person.pressed === 'true', 'the person page picker does not show the current colour as pressed');

      // Now take it off. Both halves, through the controls, and both must go.
      await js('document.querySelector(\'.hl-picker .hl-sw[data-hl-colour="none"]\').click()');
      await waitFor('the colour to clear', async () =>
        js(`window.__recallDebug.store.speakers.get(${target})?.colour == null`)
      );
      await js('document.querySelector(\'.hl-picker [data-hl-icon-clear]\').click()');
      await waitFor('the icon to clear', async () =>
        js(`!window.__recallDebug.store.speakers.get(${target})?.icon`)
      );

      // Back to the transcript: it must look EXACTLY as it did before, which
      // is the promise the whole feature is bounded by.
      await js('document.querySelector(\'.rail-item[data-view="transcript"]\').click()');
      await waitFor('transcript rows', async () => js('document.querySelectorAll("#seg-list .seg").length > 0'));
      const cleared = await waitFor('the highlight to come off the rows', async () => {
        const got = await js(`(() => {
          const nm = document.querySelector('#seg-list [data-sp="${target}"] .nm');
          const row = nm?.closest('.seg');
          return {
            colour: nm?.style.color || '',
            icons: document.querySelectorAll('#seg-list [data-sp="${target}"] .sp-icon').length,
            accent: !!row?.classList.contains('person-hl'),
          };
        })()`);
        return got && got.icons === 0 && !got.accent ? got : null;
      });
      assert(cleared.icons === 0, 'the icon survived being cleared');
      assert(cleared.accent === false, 'the row accent survived being cleared');
      assert(
        cleared.colour === before.colour,
        `a cleared voice did not go back to its original colour ("${cleared.colour}" vs "${before.colour}")`
      );

      // The fixture's highlighted people are untouched by all of that — a
      // highlight belongs to one voice.
      const others = await js(
        '[...document.querySelectorAll(\'#seg-list [data-sp="1"] .sp-icon\')].length'
      );
      assert(others > 0, 'clearing one voice took another voice\'s highlight with it');

      return {
        speaker: target,
        set: { colour: 'amber', icon: '\u{2B50}' },
        rowsAffected: before.rows,
        fixtureHighlighted: fixture.accented,
      };
    });

    await step('shot-highlights', async () => ({ file: await shot('highlights') }));

    // 0.12.4 — how a turn sounded.
    //
    // Four steps, and they are four because the feature has two independent
    // gates and both have to be exercised in both positions:
    //
    //   1. the four states of `mood_display` — every one of them ACTS;
    //   2. the daemon's `rendered` flag, which decides whether the MOOD half
    //      may be drawn at all, in both worlds;
    //   3. the tint's legibility rule, which is what makes a coloured sentence
    //      shippable on two grounds;
    //   4. the card, which has to say WHY when it is withholding something.

    await step('the-mood-chips-are-events-and-only-events-by-default', async () => {
      await js('document.querySelector(\'.rail-item[data-view="transcript"]\').click()');
      const v = await waitFor('rows with chips', async () => {
        const v = await js('window.__recallDebug.mood()');
        return v.rows && v.chips ? v : null;
      });
      // The shipped state: `tags`, and the daemon says the mood half is not
      // measured — so there are event chips and NO mood chips, whatever the
      // fixture's `mood` column says.
      assert(v.mode === 'tags', `mood_display ships as "${v.mode}"`);
      assert(v.rendered === false, `the mock claims mood is rendered: ${v.rendered}`);
      assert(v.events > 0, 'no event chips on any row');
      assert(
        v.moods === 0,
        `a mood chip was drawn while the daemon says it is not measured: ${JSON.stringify(v.sample)}`
      );
      // …and nothing is tinted, because `tags` is not `tint`.
      assert(v.tinted === 0, `${v.tinted} rows were tinted in "tags" mode`);
      // Most rows wear nothing at all. A mark on every row is not a mark, and
      // this is the assertion that would catch a fixture that over-seeded.
      assert(v.chips < v.rows, `every one of ${v.rows} rows carries a chip`);
      return { rows: v.rows, chips: v.chips, events: v.events, sample: v.sample };
    });

    await step('each-mood-display-mode-changes-what-a-row-wears', async () => {
      const set = async (mode) => {
        await js('document.querySelector(\'.rail-item[data-view="settings"]\').click()');
        await js(`document.querySelector('button[data-settings-category="language"]').click()`);
        await waitFor('the mood card', async () =>
          (await js('window.__recallDebug.mood()')).modes.length ? true : null
        );
        await js(`document.getElementById("mood-display-${mode}").click()`);
        await js('document.querySelector(\'.rail-item[data-view="transcript"]\').click()');
        // Waited on the STORE and not on the radio button: `display` reads a
        // control that only exists while the Memory view is mounted, and the
        // assertions below are about a transcript ROW.
        return waitFor(`mood_display=${mode} on a row`, async () => {
          const v = await js('window.__recallDebug.mood()');
          return v.mode === mode ? v : null;
        });
      };

      // The radio buttons only exist while the Memory view is mounted, so the
      // card is read from THERE — `set()` leaves the driver on the transcript,
      // which is where the row assertions belong and where a `modes` read
      // would correctly find nothing.
      await js('document.querySelector(\'.rail-item[data-view="settings"]\').click()');
      await js(`document.querySelector('button[data-settings-category="language"]').click()`);
      const modes = await waitFor('the mood card', async () => {
        const m = (await js('window.__recallDebug.mood()')).modes;
        return m.length ? m : null;
      });
      assert(
        modes.join(',') === 'tags,tint,both,off',
        `the card offers ${JSON.stringify(modes)}`
      );

      // `off` — neither. The one state where a bug is invisible unless it is
      // asserted, which is exactly why it is asserted.
      const off = await set('off');
      assert(off.chips === 0, `"off" still drew ${off.chips} chips`);
      assert(off.tinted === 0, `"off" still tinted ${off.tinted} rows`);

      // `tint` — the MOOD moves to the words, and the events keep their chips,
      // because there is no such thing as the colour of laughter. That split is
      // what makes this state act on a daemon that is withholding the mood
      // (the shipped one) rather than being a no-op waiting on a measurement.
      const tint = await set('tint');
      assert(tint.moods === 0, `"tint" drew ${tint.moods} mood chips`);
      assert(tint.events > 0, '"tint" dropped the event chips, which have no colour to move to');

      // `both` — chips are back, and they are still events only.
      const both = await set('both');
      assert(both.events > 0, '"both" drew no event chips');

      // …and back to the shipped state, so the screenshots below and every
      // step after this one see the app as a person gets it.
      const tags = await set('tags');
      assert(tags.events > 0, '"tags" drew no event chips');
      return { off: off.chips, both: both.events, tags: tags.events };
    });

    await step('the-mood-tint-appears-only-when-the-daemon-says-it-is-measured', async () => {
      // The other world. The mock ships the daemon's real answer (false); this
      // flips it, which is the only way to exercise the half of the renderer a
      // person gets the day the measurement changes.
      // Through the driver's own daemon client, not `window.recall`: the
      // renderer's ALLOWED set refuses anything that is not a protocol method,
      // and `mock.mood` is deliberately not one.
      await deps.request('mock.mood', { rendered: true });
      await waitFor('the daemon to say mood is rendered', async () =>
        (await js('window.__recallDebug.mood()')).rendered === true ? true : null
      );
      await js('document.querySelector(\'.rail-item[data-view="settings"]\').click()');
      await js(`document.querySelector('button[data-settings-category="language"]').click()`);
      await js('document.getElementById("mood-display-both").click()');
      await js('document.querySelector(\'.rail-item[data-view="transcript"]\').click()');
      const v = await waitFor('a tinted row', async () => {
        const v = await js('window.__recallDebug.mood()');
        return v.mode === 'both' && v.tinted ? v : null;
      });
      assert(v.moods > 0, 'the mood chip did not appear once the daemon allowed it');
      assert(v.tinted > 0, 'no row took a mood colour');
      // The colour is the token's hue through the GROUND's own saturation and
      // lightness — never a literal — which is the whole legibility argument
      // and the one thing a screenshot cannot check.
      for (const style of v.tintColors) {
        assert(
          /hsl\(\d+ var\(--mood-s\) var\(--mood-l\)\)/.test(style),
          `a row was tinted with a literal colour: ${style}`
        );
      }
      // `neutral` is a mood the fixture carries and no surface paints, so the
      // tinted rows must be strictly fewer than the rows with a mood at all.
      const neutral = await js(
        `document.querySelectorAll('#seg-list .txt.mood-neutral').length`
      );
      assert(neutral === 0, `${neutral} rows were tinted "neutral"`);
      const light = await js('window.__recallDebug.mood()');
      const file = await shot('transcript-mood');
      return { tinted: v.tinted, moods: v.moods, sample: light.sample, file };
    });

    await step('a-person-page-says-how-they-sound-and-refuses-to-say-more', async () => {
      // Still in the world where the daemon allows the mood half (the step
      // above turned it on and the step below turns it back off), so this can
      // check both claims the page is willing to make and the shape of the one
      // it is not.
      await js('document.querySelector(\'.rail-item[data-view="speakers"]\').click()');
      const target = await js(
        'Number(document.querySelector("#speaker-list .sp-row").dataset.speaker)'
      );
      await js(`window.__recallDebug.go('person', { id: ${target} })`);
      const p = await waitFor('the person page', async () => {
        const p = await js('window.__recallDebug.person()');
        return p.mounted && p.sound ? p : null;
      });
      // The DENOMINATOR is in the subtitle, where it cannot be scrolled past:
      // "they laugh a lot" over thirty turns and over three thousand are
      // different claims and only one of them is worth anything.
      assert(
        /over \d+ turns? the decoder has listened to/.test(p.sound.sub),
        `the card does not say what it is over: "${p.sound.sub}"`
      );
      const keys = p.sound.cells.map(([k]) => k);
      assert(keys.includes('laughter'), `no laughter cell: ${JSON.stringify(p.sound.cells)}`);
      // …and the note says what kind of number this is, because a tag on the
      // AUDIO is not a count of anything anybody said.
      assert(/audio/i.test(p.sound.note), `the note does not say what it is: "${p.sound.note}"`);
      return { sub: p.sound.sub, cells: p.sound.cells };
    });

    await step('the-mood-card-says-what-it-is-withholding-and-why', async () => {
      // Back to the world the user actually gets, and the card has to explain
      // itself in it — in the DAEMON's sentence, not one this app invented.
      await deps.request('mock.mood', { rendered: false });
      await waitFor('the daemon to withhold the mood again', async () =>
        (await js('window.__recallDebug.mood()')).rendered === false ? true : null
      );
      await js('document.querySelector(\'.rail-item[data-view="settings"]\').click()');
      await js(`document.querySelector('button[data-settings-category="language"]').click()`);
      const card = await waitFor('the reason on the card', async () => {
        const v = await js('window.__recallDebug.mood()');
        return v.why ? v : null;
      });
      assert(/§42/.test(card.why), `the card does not cite the measurement: "${card.why}"`);
      assert(/not shown/.test(card.why), `the card does not say it is withholding: "${card.why}"`);
      // The chip half is still promised, because it was measured separately.
      assert(/[Ll]aughter/.test(card.why), 'the card does not say what IS shown');
      // The subtitle is a fact and not a mood: how much has been read.
      assert(/turns read|off —|not installed/.test(card.sub), `the badge says "${card.sub}"`);
      // And the setting is back where a person left it.
      await js('document.getElementById("mood-display-tags").click()');
      await js('document.getElementById("mood-card").scrollIntoView({ block: "start" })');
      const file = await shot('memory-mood');
      // Put the driver back where it found it. Every step after this one reads
      // the TRANSCRIPT header — the live chip, the pause banner — and a step
      // that wanders off and leaves the app somewhere else fails the next one
      // with a message about a feature it never touched.
      await js('document.querySelector(\'.rail-item[data-view="transcript"]\').click()');
      return { why: card.why, sub: card.sub, file };
    });

    // 0.12.5 — the mood pass's own switch. `mood.set` is a real socket method
    // now, not the test-only `mock.mood`, so this exercises the whole path: a
    // click flips the switch, `status.mood.enabled` moves, the amber notice
    // comes and goes with it, and the header starts saying how much has been
    // read instead of "nothing is listened to".
    await step('the-mood-switch-turns-listening-on-and-off', async () => {
      await js('document.querySelector(\'.rail-item[data-view="settings"]\').click()');
      await js(`document.querySelector('button[data-settings-category="language"]').click()`);
      await js('document.getElementById("mood-card").scrollIntoView({ block: "start" })');
      const off = await waitFor('the mood card in its shipped, off state', async () => {
        const v = await js('window.__recallDebug.mood()');
        return v.sub ? v : null;
      });
      assert(/^off —/.test(off.sub), `the mood card did not ship off: "${off.sub}"`);
      assert(await js('!!document.getElementById("mood-off")'), 'no notice shown while off');
      const offShot = await shot('memory-mood-off');

      await js('document.getElementById("mood-toggle").click()');
      const on = await waitFor('the switch to report on, with counts in the header', async () => {
        const v = await js('window.__recallDebug.mood()');
        return v.sub && /read/.test(v.sub) ? v : null;
      });
      assert(/newest first/.test(on.sub), `the header does not say the read order: "${on.sub}"`);
      assert(
        !(await js('!!document.getElementById("mood-off")')),
        'the amber notice stayed up once the switch was on'
      );
      const onShot = await shot('memory-mood-on');

      // Back off, and the notice returns — the two states are a round trip,
      // not a one-way door.
      await js('document.getElementById("mood-toggle").click()');
      await waitFor('the switch to report off again', async () => {
        const v = await js('window.__recallDebug.mood()');
        return v.sub && /^off —/.test(v.sub) ? true : null;
      });
      assert(
        await js('!!document.getElementById("mood-off")'),
        'the notice did not come back once switched off'
      );

      await js('document.querySelector(\'.rail-item[data-view="transcript"]\').click()');
      return { offSub: off.sub, onSub: on.sub, offShot, onShot };
    });

    // 0.13.x — light mode. `asr.light.set` is a real socket method, so a click
    // moves the switch and persists it; `mock.light` (test-only, like
    // `mock.mood`) is how the driver exercises the automatic swap a captured
    // game or a busy GPU would otherwise trigger, which this mock has neither
    // of.
    await step('light-mode-switches-the-decoder-and-says-why', async () => {
      await js('document.querySelector(\'.rail-item[data-view="settings"]\').click()');
      await js(`document.querySelector('button[data-settings-category="processing"]').click()`);
      await js('document.getElementById("light-card").scrollIntoView({ block: "start" })');
      // Ships OFF: the 110m export reads English only and measured 104.5% WER
      // on this install's German-heavy archive (FINDINGS §49). A person opts
      // in from the card; a default must not make that trade for them.
      const auto = await waitFor('the light card in its shipped, off state', async () => {
        const v = await js('window.__recallDebug.light()');
        return v.mode ? v : null;
      });
      assert(auto.mode === 'off', `light mode did not ship off: ${auto.mode}`);
      assert(auto.light === false, 'the mock ships the full decoder live');
      assert(/full decoder/.test(auto.sub), `the card does not say which decoder: "${auto.sub}"`);
      const autoShot = await shot('memory-light-off');

      // The manual switch: a click on "Always" pins the light decoder,
      // unconditionally, through the real `asr.light.set` path.
      await js('document.getElementById("light-mode-on").click()');
      const on = await waitFor('the switch to report the light decoder live', async () => {
        const v = await js('window.__recallDebug.light()');
        return v.light === true ? v : null;
      });
      assert(on.mode === 'on', `the switch did not land on "on": ${on.mode}`);
      assert(/smaller decoder/.test(on.sub), `the card does not say which decoder: "${on.sub}"`);
      assert(/manual/.test(on.reason), `the reason does not say it is manual: "${on.reason}"`);

      // Back to auto, then the automatic swap a captured game would cause —
      // exercised through `mock.light` because this mock simulates neither a
      // captured source nor a GPU counter.
      await js('document.getElementById("light-mode-auto").click()');
      await waitFor('auto to report the full decoder again', async () => {
        const v = await js('window.__recallDebug.light()');
        return v.mode === 'auto' && v.light === false ? true : null;
      });
      await deps.request('mock.light', {
        light: true,
        reason: 'a captured source matches light_mode_games',
      });
      const gamed = await waitFor('the automatic swap to reach every window', async () => {
        const v = await js('window.__recallDebug.light()');
        return v.light === true ? v : null;
      });
      assert(/vrchat|game/i.test(gamed.reason), `the reason does not name a game: "${gamed.reason}"`);
      assert(/smaller decoder/.test(gamed.sub), `the card did not follow the swap: "${gamed.sub}"`);
      const gameShot = await shot('memory-light-game');

      // Put the driver back where it found it — auto, full decoder — the same
      // discipline the mood step above ends on.
      await deps.request('mock.light', {
        light: false,
        reason: 'no game captured and the GPU is not sustained-busy',
      });
      await waitFor('the mock world to reset', async () => {
        const v = await js('window.__recallDebug.light()');
        return v.light === false ? true : null;
      });
      // …and the switch back to the shipped `off`, so later steps and a re-run
      // start where a fresh install does.
      await js('document.getElementById("light-mode-off").click()');
      await waitFor('the switch to land back on off', async () => {
        const v = await js('window.__recallDebug.light()');
        return v.mode === 'off' && v.light === false ? true : null;
      });
      await js('document.querySelector(\'.rail-item[data-view="transcript"]\').click()');
      return { offSub: auto.sub, onReason: on.reason, gameReason: gamed.reason, autoShot, gameShot };
    });

    // 8 — pause from the TRAY path stops the feed (DESIGN §8, the marquee case)
    await step('tray-pause-stops-feed', async () => {
      await deps.setPaused(true); // exactly what the tray menu item calls
      await waitFor('the UI to show paused', async () =>
        js('document.getElementById("pause-btn").dataset.paused === "true"')
      );
      // Counted AFTER the pause is in effect: a row the mock emitted in the
      // round trip between "count" and "pause" is not the feed running while
      // paused, and under load that window is wide enough to hit (42 → 43).
      const before = (await js('window.__recallDebug.counts()')).appended;
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
      // 0.9.2: a hit offers to replay the conversation it came out of, from
      // this line. Only where the row was threaded, so it is a count and not
      // one-per-hit.
      const replayable = await js('document.querySelectorAll("#search-results .seg .hit-replay").length');
      assert(replayable > 0, 'no search hit offers to replay the conversation it sits in');
      const file = await shot('search');
      await js('document.querySelector("#search-results .seg").click()');
      await js(`[...document.querySelectorAll('.search-context button')].find(b => b.textContent === 'Open transcript').click()`);
      await waitFor('the transcript to take focus', async () => js('window.__recallDebug.view() === "transcript"'));
      const marked = await waitFor('the hit to be marked', async () => js('!!document.querySelector("#seg-list .seg.hit")'));
      return { hits, facets, replayable, marked, file };
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
      await js(`[...document.querySelectorAll('.search-context button')].find(b => b.textContent === 'Open transcript').click()`);
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
      try {
        await waitFor('the pills to clear', async () => ((await js('!window.__recallDebug.ask().shown')) ? true : null));
      } catch (e) {
        // Say WHAT is still up: the element, its hidden flag, the mounted view
        // and the pills' text — a bare `false` cost an hour.
        const dump = await js(`(() => {
          const p = document.getElementById('ask-pills');
          return JSON.stringify({ has: !!p, hidden: p?.hidden, view: window.__recallDebug.view(), pills: [...(p?.querySelectorAll('.ask-pill') ?? [])].map((x) => x.textContent), q: document.getElementById('search-q')?.value });
        })()`);
        throw new Error(`${e.message} · ${dump}`);
      }
      return { facets, hits: `${before} → ${wider.hits}`, shakyHits: marked, file };
    });

    // 11c2 — 0.11.0, grounded answers. A question gets a sentence with the
    // turns it was read off attached; a question the archive cannot answer
    // gets one quiet line and the hits anyway; and a plain keyword query gets
    // neither, because answering one would be the app talking over the user.
    await step('an-answer-cites-the-turns-it-came-from', async () => {
      await js('document.querySelector(\'.rail-item[data-view="search"]\').click()');
      await waitFor('the search box', async () => js('!!document.getElementById("search-q")'));

      const typeQuestion = async (text) =>
        js(`(() => {
          const q = document.getElementById('search-q');
          q.value = ${JSON.stringify(text)};
          q.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));
          return true;
        })()`);

      // -- a question the archive answers --------------------------------
      await typeQuestion('which portal was it?');
      const answered = await waitFor(
        'the answer card',
        async () => {
          const a = await js('window.__recallDebug.ask()');
          return a.answer && !a.answer.refused ? a : null;
        },
        { timeout: 20000 }
      );
      const card = answered.answer;
      assert(card.text.trim().length > 0, 'the answer card is empty');
      assert(card.first, 'the answer is not above the hits it was read off');
      // Never a sentence without its evidence. This is the assertion the whole
      // feature turns on: a claim with no chips is the app asserting something.
      assert(card.cites.length > 0, 'an answer rendered with no citations');
      assert(
        card.cites.every((c) => c.lands),
        `a citation points at a turn this page does not have: ${JSON.stringify(card.cites)}`
      );
      // A chip reads as a moment and a voice, which is what makes it followable.
      assert(
        card.cites.every((c) => /^\d{2}:\d{2}\s+\S/.test(c.text)),
        `a chip does not read as a clock and a name: ${JSON.stringify(card.cites.map((c) => c.text))}`
      );
      assert(answered.hits > 0, 'the hits went away when the answer arrived');
      const file = await shot('search-answer');

      // …and pressing one takes you to the turn.
      const target = card.cites[0].id;
      await js(`document.querySelector('#answer-cites [data-cite="${target}"]').click()`);
      const jumped = await waitFor(
        'the cited turn to flash',
        async () => {
          const a = await js('window.__recallDebug.ask()');
          return a.cited.includes(target) ? a.cited : null;
        },
        { timeout: 8000 }
      );
      assert(jumped.includes(target), `the chip flashed ${jumped} instead of ${target}`);

      // -- a question it cannot answer -----------------------------------
      await typeQuestion('what did anybody say about kryptonite?');
      const refused = await waitFor(
        'the refusal note',
        async () => {
          const a = await js('window.__recallDebug.ask()');
          return a.answer?.refused ? a : null;
        },
        { timeout: 20000 }
      );
      assert(
        /does not say|nothing in the transcript/i.test(refused.answer.note),
        `the refusal reads "${refused.answer.note}"`
      );
      assert(refused.answer.cites.length === 0, 'a refusal came with citations');
      // One quiet line, not a red box, and never a sentence.
      assert(!refused.answer.text, 'a refusal rendered an answer as well');

      // -- a plain keyword query -----------------------------------------
      // Not a question, so it goes down the search path and gets no card. A
      // keyword search that grew a sentence would be the app answering
      // something nobody asked.
      await typeQuestion('portal');
      const plain = await waitFor(
        'the keyword search to land',
        async () => {
          const a = await js('window.__recallDebug.ask()');
          return a.hits > 0 && !a.answer ? a : null;
        },
        { timeout: 20000 }
      );
      assert(plain.answer === null, 'a keyword query rendered an answer card');
      assert(plain.shown, 'the interpretation pills went away on a keyword query');

      // Put the view back the way the later steps expect it.
      await js(`(() => {
        document.getElementById('search-q').value = 'portal';
        document.getElementById('search-go').click();
        return true;
      })()`);
      try {
        await waitFor('the pills to clear', async () => ((await js('!window.__recallDebug.ask().shown')) ? true : null));
      } catch (e) {
        // Say WHAT is still up: the element, its hidden flag, the mounted view
        // and the pills' text — a bare `false` cost an hour.
        const dump = await js(`(() => {
          const p = document.getElementById('ask-pills');
          return JSON.stringify({ has: !!p, hidden: p?.hidden, view: window.__recallDebug.view(), pills: [...(p?.querySelectorAll('.ask-pill') ?? [])].map((x) => x.textContent), q: document.getElementById('search-q')?.value });
        })()`);
        throw new Error(`${e.message} · ${dump}`);
      }
      return { cites: card.cites.map((c) => c.text).join(', '), file };
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
      await js(`document.querySelector('button[data-sources-category="capture"]').click()`);
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

    // 12a2 — 0.8.3: the captions card. It is on THIS page because this page is
    // already where the app's shape is decided — what it listens to, what it
    // keeps — and a window that floats over everything is the same kind of
    // decision. The step drives the real sliders and reads the value back out
    // of the main process, because a setting that only exists in a renderer is
    // a setting that is gone the next time the window is opened.
    await step('the-captions-card-really-sets-the-captions', async () => {
      await js(`document.querySelector('button[data-sources-category="captions"]').click()`);
      const card = await waitFor('the captions card', async () => {
        const c = await js('window.__recallDebug.captions()');
        return c.card ? c : null;
      });
      const wanted = ['turns', 'size', 'hold_s', 'opacity', 'showYou', 'clickThrough'];
      const missing = wanted.filter((k) => !(k in card.values));
      assert(missing.length === 0, `the captions card has no control for ${missing.join(', ')}`);
      assert(card.open, 'the captions card offers no way to put them on screen');
      // The rail's own button is the third way in (tray, rail, --captions), and
      // it reflects a window that can be opened from any of the other two.
      assert(card.pressed === 'false' || card.pressed === 'true', `the rail button has no state: ${card.pressed}`);

      // Move the real slider, the way a person does.
      await js(`(() => {
        const el = document.querySelector('#captions-card [data-cap="size"]');
        el.value = "34";
        el.dispatchEvent(new Event('input', { bubbles: true }));
      })()`);
      const took = await waitFor('the main process to take the new size', async () =>
        deps.captions.settings().size === 34
      );
      assert(took, 'the size slider did not reach the main process');
      // …and the card is showing the value it just set, in the units it set it
      // in: a slider with no number on it is a slider you have to guess at.
      const after = await js('window.__recallDebug.captions()');
      assert(after.shown.size === '34 px', `the card reads "${after.shown.size}"`);

      // The You switch is a real switch and it really flips.
      await js('document.querySelector(\'#captions-card [data-cap="showYou"]\').click()');
      await waitFor('the You switch', async () => deps.captions.settings().showYou === false);
      await js('document.querySelector(\'#captions-card [data-cap="showYou"]\').click()');
      await waitFor('the You switch back', async () => deps.captions.settings().showYou === true);

      await js('document.getElementById("captions-card").scrollIntoView({ block: "start" })');
      const file = await shot('sources-captions');
      // Leave the size where the defaults had it, so the artefacts of a later
      // pass are not a different size from this one's.
      deps.captions.set({ size: 26 });
      return { values: card.values, shown: after.shown, file };
    });

    // 12a — 0.6.1: what all of this costs on disk, broken into the four parts
    // that behave differently. A single total would hide the fact that exactly
    // one of them shrinks on its own.
    await step('storage-card-and-footer', async () => {
      await js('document.querySelector(\'.rail-item[data-view="sources"]\').click()');
      await js(`document.querySelector('button[data-sources-category="storage"]').click()`);
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

    // 12a.5 — capture health (0.14.0). Gaps per hour by cause, one bar row
    // per source, with a legend explaining each cause and its fix. The mock
    // ships a nonzero table on purpose (FINDINGS §50) — a card that only ever
    // renders "no gaps" would never be photographed doing its actual job.
    await step('capture-health-card', async () => {
      await js('document.querySelector(\'.rail-item[data-view="sources"]\').click()');
      await js(`document.querySelector('button[data-sources-category="storage"]').click()`);
      const c = await waitFor('the capture health card', async () => {
        const c = await js('window.__recallDebug.captureHealth()');
        return c.rows.length ? c : null;
      });
      assert(c.health && c.health.total > 0, `the mock's health block is empty: ${JSON.stringify(c.health)}`);
      // Every rendered row's count must equal the sum of its bar segments —
      // the bar is drawn FROM by_cause, so a mismatch means the two drifted.
      for (const row of c.rows) {
        const source = c.health.top_sources.find((s) => s.match_key === row.source);
        assert(source, `rendered a row for a source not in status.capture.health.top_sources: ${row.source}`);
        assert(row.count === source.count, `${row.source}: bar says ${row.count}, status says ${source.count}`);
        const causesOnBar = new Set(row.segs);
        for (const cause of Object.keys(source.by_cause)) {
          assert(causesOnBar.has(cause), `${row.source}: ${cause} is in by_cause but has no bar segment`);
        }
      }
      // The legend explains every cause actually present, each with a fix.
      assert(c.legend.length > 0, 'no legend rows rendered');
      for (const cause of Object.keys(c.health.by_cause)) {
        if (!c.health.by_cause[cause]) continue;
        assert(
          c.legend.some((line) => line.toLowerCase().includes(cause.replace(/_/g, ' ').split(' ')[0])),
          `the legend does not explain "${cause}": ${JSON.stringify(c.legend)}`
        );
      }
      await js('document.getElementById("health-card").scrollIntoView({block: "end"})');
      const file = await shot('sources-capture-health');
      return { total: c.health.total, rows: c.rows.length, file };
    });

    // 12b — the microphone. Off by default and NOT in the application list;
    // enabling it in follow mode has to read as "waiting", not as "recording",
    // because that difference is the whole privacy model (0.6.0).
    await step('mic-card-is-off-and-separate', async () => {
      await js('document.querySelector(\'.rail-item[data-view="sources"]\').click()');
      await js(`document.querySelector('button[data-sources-category="capture"]').click()`);
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
      await js(`document.querySelector('button[data-sources-category="capture"]').click()`);
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

    // ---- 0.10.0 ------------------------------------------------------------

    // 12e — the room microphone. A SECOND physical device, off, with no device
    // chosen and therefore nothing its switch can do until one is: the state a
    // fresh install is in, and the one the card has to be usable from.
    await step('room-card-needs-a-device-before-it-can-do-anything', async () => {
      await js('document.querySelector(\'.rail-item[data-view="sources"]\').click()');
      await js(`document.querySelector('button[data-sources-category="capture"]').click()`);
      await waitFor('the room card', async () => js('!!document.getElementById("room-card")'));

      // It sits beside the microphone's card, not in the application list.
      const order = await js(`(() => {
        const cards = [...document.querySelector('.view-body').querySelectorAll('.card')];
        return [cards.indexOf(document.getElementById('mic-card')), cards.indexOf(document.getElementById('room-card'))];
      })()`);
      assert(order[1] === order[0] + 1, `the room card is not beside the microphone's (${JSON.stringify(order)})`);
      const inList = await js(`[...document.querySelectorAll('#source-list .src-row')].some(r => r.dataset.source === 'room')`);
      assert(!inList, 'the room microphone is listed as an application — it is not one');

      // The device list is fetched on mount, so wait for the picker to be
      // populated rather than reading it mid-flight.
      const room = await waitFor('the device picker', async () => {
        const r = await js('window.__recallDebug.room()');
        return r.devices.length ? r : null;
      });
      assert(room.enabled === false, 'the room microphone is not off by default');
      assert(room.state === 'off', `the state reads "${room.state}"`);
      assert(room.device == null, 'a second microphone shipped with a device already chosen');
      assert(room.toggleDisabled === true, 'the switch is usable with no device — the daemon would refuse it');
      // The copy has to be as blunt as the microphone's, and about the people
      // it hears rather than about the user.
      assert(/room/i.test(room.warning), `the warning does not say what it hears: ${room.warning}`);
      assert(
        /nothing on this device is marked as you/i.test(room.warning),
        `the warning does not say whose voices these are: ${room.warning}`
      );
      // And the picker offers real devices, with the system default named as
      // the one that is almost always wrong.
      assert(room.devices.length >= 2, `the device picker offered ${room.devices.length} devices`);
      const labels = await js(`[...document.getElementById('room-device').options].map(o => o.textContent)`);
      assert(labels.some((l) => /system default/i.test(l)), `no device is marked as the system default: ${JSON.stringify(labels)}`);
      return { devices: room.devices.length, warning: room.warning.slice(0, 70) };
    });

    await step('room-mic-turns-on-once-a-device-is-picked', async () => {
      await js('document.querySelector(\'.rail-item[data-view="sources"]\').click()');
      await js(`document.querySelector('button[data-sources-category="capture"]').click()`);
      await waitFor('the room card', async () => js('!!document.getElementById("room-card")'));
      await waitFor('the device picker', async () => (await js('window.__recallDebug.room()')).devices.length > 0);
      // Pick the desk mic — the second option, i.e. NOT the system default.
      const picked = await js(`(() => {
        const sel = document.getElementById('room-device');
        const opt = [...sel.options].find(o => o.value && !/system default/i.test(o.textContent));
        sel.value = opt.value;
        sel.dispatchEvent(new Event('change'));
        return opt.value;
      })()`);
      // Wait for the round trip to land, not merely for the optimistic paint:
      // the switch is deliberately disabled while a change is in flight, so
      // reading it mid-flight would assert on the pending state.
      const chosen = await waitFor(
        'the device to be pinned and the switch to arm',
        async () => {
          const r = await js('window.__recallDebug.room()');
          return r.device === picked && r.toggleDisabled === false ? r : null;
        },
        { timeout: 10000, every: 150 }
      );
      assert(chosen.state === 'off', `pinning a device turned it on by itself (${chosen.state})`);

      await js('document.getElementById("room-toggle").click()');
      const on = await waitFor(
        'the room microphone to come on',
        async () => {
          const r = await js('window.__recallDebug.room()');
          return r.enabled && r.state !== 'off' ? r : null;
        },
        { timeout: 10000, every: 150 }
      );
      assert(on.state !== 'needs-device', 'it came on and still says it has no device');
      assert(on.chip !== 'off', `the chip did not follow the switch: "${on.chip}"`);

      // Both modes are reachable, and `always` is the one that does not wait.
      await js('document.querySelector(\'#room-modes [data-room-mode="always"]\').click()');
      const always = await waitFor(
        'always mode',
        async () => {
          const r = await js('window.__recallDebug.room()');
          return r.mode === 'always' ? r : null;
        },
        { timeout: 8000, every: 150 }
      );
      const file = await shot('sources-room');

      // Put it back where the rest of the suite expects it.
      await js('document.getElementById("room-toggle").click()');
      await waitFor('the room microphone to go off', async () => {
        const r = await js('window.__recallDebug.room()');
        return r.state === 'off';
      });
      return { device: picked, state: always.state, chip: always.chip, file };
    });

    // 12f — the Discord bridge (0.9.0's ground truth), as a card on this page:
    // what is arriving, from whom, and how the voicebank is doing against it.
    await step('discord-card-links-a-user-and-scores-the-voicebank', async () => {
      await js('document.querySelector(\'.rail-item[data-view="sources"]\').click()');
      await js(`document.querySelector('button[data-sources-category="connections"]').click()`);
      await waitFor('the Discord card', async () => js('!!document.getElementById("truth-card")'));
      const before = await waitFor('the Discord users', async () => {
        const t = await js('window.__recallDebug.truth()');
        return t.users.length ? t : null;
      });
      assert(!before.off, 'the card says the bridge is off; the mock has it on');
      assert(/receiving|waiting/i.test(before.chip), `the state chip reads "${before.chip}"`);
      // The privacy posture: what is received, and where it goes.
      assert(/no audio/i.test(before.hint), `the copy does not say what is NOT received: ${before.hint}`);
      assert(/leaves this machine/i.test(before.hint), `the copy does not say where it goes: ${before.hint}`);
      // The scorecard, with real numbers rather than a placeholder.
      assert(/precision \d+%/.test(before.score), `no precision in the scorecard: ${before.score}`);
      assert(/recall \d+%/.test(before.score), `no recall in the scorecard: ${before.score}`);
      assert(/n \d+/.test(before.score), `no sample size in the scorecard: ${before.score}`);

      // Link an unlinked account to a named voice.
      const target = before.users.find((u) => !u.linked);
      assert(target, 'every account was already linked; the link path is untestable');
      const linked = await js(`(() => {
        const sel = document.querySelector('[data-truth-link="${target.id}"]');
        const opt = [...sel.options].find(o => o.value);
        sel.value = opt.value;
        sel.dispatchEvent(new Event('change'));
        return opt.value;
      })()`);
      const after = await waitFor(
        'the link to stick',
        async () => {
          const t = await js('window.__recallDebug.truth()');
          const row = t.users.find((u) => u.id === target.id);
          return row && row.linked === linked ? t : null;
        },
        { timeout: 10000, every: 150 }
      );
      await js('document.getElementById("truth-card").scrollIntoView({block: "start"})');
      const file = await shot('sources-discord');
      return { users: after.users.length, linked, score: before.score, file };
    });

    // 12f2 — two Discord clients, one plugin (0.12.2). The user runs Vesktop
    // with the bridge and a second Discord in another call; until 0.12.2 the
    // mute took BOTH calls off the record. The card has to say which client is
    // muted, and the control has to move it — in both directions, because a
    // role that is read and ignored is a blocker and not a footnote.
    await step('the-discord-card-says-which-client-is-muted-and-lets-you-move-it', async () => {
      await js('document.querySelector(\'.rail-item[data-view="sources"]\').click()');
      await js(`document.querySelector('button[data-sources-category="connections"]').click()`);
      const shown = await waitFor('the Discord clients', async () => {
        const t = await js('window.__recallDebug.truth()');
        return t.clients.length ? t : null;
      });
      assert(shown.clients.length === 2, `${shown.clients.length} client(s); the fixture runs two`);
      const by = (src) => shown.clients.find((c) => c.source === src);
      assert(by('vesktop') && by('Discord'), `the two clients are not both drawn: ${JSON.stringify(shown.clients)}`);
      assert(by('vesktop').muted, 'the plugin’s own client is not shown as muted');
      assert(!by('Discord').muted, 'the OTHER call is shown as muted — this is the bug');
      assert(/recording/.test(by('Discord').state), `the other client's chip reads "${by('Discord').state}"`);
      assert(/different call/i.test(by('Discord').why), `no reason given: ${by('Discord').why}`);
      assert(by('vesktop').role === 'auto' && by('Discord').role === 'auto', 'both start on auto');
      assert(/never muted/i.test(shown.clientsHint), `the copy does not say what an override does: ${shown.clientsHint}`);
      await js('document.getElementById("truth-clients").scrollIntoView({block: "center"})');
      const before = await shot('sources-discord-clients');

      // The user overrules it: the client the rule was sure about is marked as
      // NOT the bridge, and the other one is marked as the one with the plugin.
      await js(`(() => {
        const s = document.querySelector('[data-bridge-role="vesktop"]');
        s.value = 'other'; s.dispatchEvent(new Event('change'));
      })()`);
      await waitFor('the override to land', async () => {
        const t = await js('window.__recallDebug.truth()');
        const v = t.clients.find((c) => c.source === 'vesktop');
        return v && v.role === 'other' && !v.muted ? t : null;
      });
      await js(`(() => {
        const s = document.querySelector('[data-bridge-role="Discord"]');
        s.value = 'bridge'; s.dispatchEvent(new Event('change'));
      })()`);
      const after = await waitFor('the second override to land', async () => {
        const t = await js('window.__recallDebug.truth()');
        const d = t.clients.find((c) => c.source === 'Discord');
        return d && d.role === 'bridge' && d.muted ? t : null;
      });
      const now = (src) => after.clients.find((c) => c.source === src);
      assert(!now('vesktop').muted, 'a client marked “no plugin” must never be muted');
      assert(now('Discord').muted, 'a client marked “has the plugin” must be muted while streams are live');
      assert(/muted/.test(now('Discord').state), `the chip did not follow: "${now('Discord').state}"`);
      assert(/not the bridge/i.test(now('vesktop').why), `the reason did not follow: ${now('vesktop').why}`);

      await js('document.getElementById("truth-clients").scrollIntoView({block: "center"})');
      const file = await shot('sources-discord-clients-overridden');

      // Put it back, so the rest of the pass sees the daemon's own answer.
      for (const src of ['vesktop', 'Discord']) {
        await js(`(() => {
          const s = document.querySelector('[data-bridge-role="${src}"]');
          s.value = 'auto'; s.dispatchEvent(new Event('change'));
        })()`);
      }
      await waitFor('the roles to clear', async () => {
        const t = await js('window.__recallDebug.truth()');
        return t.clients.every((c) => c.role === 'auto') ? t : null;
      });
      return { clients: after.clients.map((c) => `${c.source}:${c.role}:${c.muted}`), before, file };
    });

    // 12g — the Markdown export. DESIGN §12 is the whole shape of this step:
    // the card writes FILES, into a folder chosen in a native dialog, and
    // nothing else — and it refuses to touch a file it did not write.
    await step('export-previews-then-writes-markdown-to-a-folder', async () => {
      const dir = join(OUT, `export${SUFFIX}`);
      // A previous run's files would make the preview look as if it had
      // written something; the folder starts empty every time.
      rmSync(dir, { recursive: true, force: true });
      mkdirSync(dir, { recursive: true });
      // The driver cannot click an OS folder dialog, so it supplies the answer
      // the dialog would have given. Everything after that is the real path.
      process.env.NX_RECALL_E2E_EXPORT_DIR = dir;

      await js('document.querySelector(\'.rail-item[data-view="sources"]\').click()');
      await js(`document.querySelector('button[data-sources-category="storage"]').click()`);
      await waitFor('the export card', async () => js('!!document.getElementById("export-card")'));
      const card = await js('window.__recallDebug.exportCard()');
      // The sentence that licenses the feature at all.
      assert(/writes files to your disk and nothing else/i.test(card.note), `the card does not say what it does: ${card.note}`);

      await js('document.getElementById("export-choose").click()');
      const planned = await waitFor(
        'the preview',
        async () => {
          const c = await js('window.__recallDebug.exportCard()');
          return c.files.length ? c : null;
        },
        { timeout: 15000, every: 200 }
      );
      assert(planned.dir.includes(dir), `the card does not show the chosen folder: ${planned.dir}`);
      assert(planned.files.includes('people.md'), `no people.md in the plan: ${JSON.stringify(planned.files)}`);
      assert(/turn/.test(planned.counts), `the counts say nothing about turns: ${planned.counts}`);
      // A preview writes NOTHING.
      assert(readdirSync(dir).length === 0, 'the preview wrote files');

      await js('document.getElementById("export-run").click()');
      const done = await waitFor(
        'the export to finish',
        async () => {
          const c = await js('window.__recallDebug.exportCard()');
          return c.done ? c : null;
        },
        { timeout: 20000, every: 200 }
      );
      const written = readdirSync(dir);
      assert(written.includes('people.md'), `people.md was not written: ${JSON.stringify(written)}`);
      const day = written.find((n) => n !== 'people.md' && n.endsWith('.md'));
      assert(day, `no day file was written: ${JSON.stringify(written)}`);
      const body = readFileSync(join(dir, day), 'utf8');
      assert(body.startsWith('<!-- nx-recall export -->'), `the file carries no export header:\n${body.slice(0, 120)}`);
      assert(/^## \d\d:\d\d — /m.test(body), `no conversation heading in the export:\n${body.slice(0, 400)}`);
      assert(/^- \*\*\d\d:\d\d\*\* .+: /m.test(body), `no turn lines in the export:\n${body.slice(0, 400)}`);
      assert(done.openable, 'no way to open the folder after an export');

      // And the guard: a file NX Recall did not write is never overwritten.
      writeFileSync(join(dir, day), '# my own notes about that evening\n');
      await js('document.getElementById("export-preview").click()');
      const blocked = await waitFor(
        'the overwrite guard',
        async () => {
          const c = await js('window.__recallDebug.exportCard()');
          return c.error ? c : null;
        },
        { timeout: 15000, every: 200 }
      );
      assert(blocked.error.includes(day), `the refusal does not name the file: ${blocked.error}`);
      assert(blocked.runDisabled === true, 'the Export button is still armed over somebody else’s file');
      assert(
        readFileSync(join(dir, day), 'utf8') === '# my own notes about that evening\n',
        'the export wrote over a file it did not write'
      );
      await js('document.getElementById("export-card").scrollIntoView({block: "start"})');
      const file = await shot('sources-export');
      return { files: written, day, error: blocked.error.slice(0, 80), file };
    });

    // ---- end 0.10.0 --------------------------------------------------------

    // 0.13.0 — a backup you can trust. The drill: back a snapshot up, verify
    // it clean, corrupt one byte in the copy, and watch verify catch it.
    await step('backup-creates-and-verifies-a-snapshot', async () => {
      const dir = join(OUT, `backup${SUFFIX}`);
      rmSync(dir, { recursive: true, force: true });
      mkdirSync(dir, { recursive: true });
      process.env.NX_RECALL_E2E_BACKUP_DIR = dir;

      await js('document.querySelector(\'.rail-item[data-view="sources"]\').click()');
      await js(`document.querySelector('button[data-sources-category="storage"]').click()`);
      await waitFor('the backup card', async () => js('!!document.getElementById("backup-card")'));

      await js('document.getElementById("backup-choose").click()');
      await waitFor('the folder to be chosen', async () => {
        const c = await js('window.__recallDebug.backupCard()');
        return c.dir.includes(dir) ? c : null;
      });

      await js('document.getElementById("backup-create").click()');
      const done = await waitFor(
        'the backup to finish',
        async () => {
          const c = await js('window.__recallDebug.backupCard()');
          return c.done ? c : null;
        },
        { timeout: 20000, every: 200 }
      );
      const written = readdirSync(dir);
      assert(written.includes('recall.db'), `recall.db was not written: ${JSON.stringify(written)}`);
      assert(written.includes('manifest.json'), `manifest.json was not written: ${JSON.stringify(written)}`);
      assert(written.includes('manifest.sig'), `manifest.sig was not written: ${JSON.stringify(written)}`);
      assert(done.openable, 'no way to open the folder after a backup');

      await js('document.getElementById("backup-verify").click()');
      const clean = await waitFor(
        'the verify to finish clean',
        async () => {
          const c = await js('window.__recallDebug.backupCard()');
          return c.verifyResult ? c : null;
        },
        { timeout: 15000, every: 200 }
      );
      assert(/verified clean/i.test(clean.verifyResult), `verify did not report clean: ${clean.verifyResult}`);

      // The drill: corrupt one byte in the copy, and verify must catch it.
      const dbPath = join(dir, 'recall.db');
      const bytes = readFileSync(dbPath);
      bytes[Math.floor(bytes.length / 2)] ^= 0xff;
      writeFileSync(dbPath, bytes);

      await js('document.getElementById("backup-verify").click()');
      const corrupted = await waitFor(
        'the verify to catch the corruption',
        async () => {
          const c = await js('window.__recallDebug.backupCard()');
          return c.verifyResult && !/verified clean/i.test(c.verifyResult) ? c : null;
        },
        { timeout: 15000, every: 200 }
      );
      assert(/did not verify/i.test(corrupted.verifyResult), `a corrupted backup still verified clean: ${corrupted.verifyResult}`);

      await js('document.getElementById("backup-card").scrollIntoView({block: "start"})');
      const file = await shot('sources-backup');
      return { written, verify: clean.verifyResult, corrupted: corrupted.verifyResult, file };
    });

    // ---- end 0.13.0 ----------------------------------------------------------

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
      assert(light.ground === 'rgb(246, 246, 247)', `the light ground is ${light.ground}, not #f6f6f7`);
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

    // 0.14 — follow the actual UI, then independently read saved records back.
    const clickLabel014 = (scope, label) => js(`(() => {
      const button = [...document.querySelectorAll(${JSON.stringify(scope)} + ' button')].find(b => b.textContent.trim() === ${JSON.stringify(label)});
      if (!button) throw new Error('Missing action: ' + ${JSON.stringify(label)});
      button.click();
    })()`);
    const nav014 = name => js(`document.querySelector('.rail-item[data-view="${name}"]').click()`);
    const saved014 = async () => {
      await nav014('memory');
      await js(`document.querySelector('[data-memory-tab="saved"]').click()`);
      await waitFor('saved groups', () => js(`document.querySelectorAll('[data-saved-group]').length === 2`));
    };
    const readSaved014 = kind => js(`(async () => (await window.recall.request('saved.${kind}.list', {})).data.${kind})()`);
    const saveSearch014 = async name => {
      await js(`document.getElementById('search-save').click()`);
      await js(`document.getElementById('saved-search-name').value = ${JSON.stringify(name)}`);
      await clickLabel014('.sheet', 'Save search');
      await waitFor('saved search dialog closed', () => js(`!document.getElementById('saved-search-name')`));
      return waitFor('saved query on wire', async () => (await readSaved014('searches')).find(r => r.name === name));
    };
    let fixed014, rolling014, moment014;

    await step('014-settings-separates-processing-and-remembers-density', async () => {
      await nav014('memory');
      assert(await js(`!document.getElementById('enrich-card') && !document.getElementById('translate-card')`), 'processing still clutters Memory');
      await nav014('settings');
      await js(`document.querySelector('button[data-settings-category="appearance"]').click()`);
      assert(await js(`!!document.getElementById('enrich-card') && !!document.getElementById('translate-card') && !!document.getElementById('accuracy-card')`), 'Settings lost an existing processing control');
      const old = await js(`document.getElementById('settings-density').value`);
      await js(`(() => { const s = document.getElementById('settings-density'); s.value = 'compact'; s.dispatchEvent(new Event('change')); })()`);
      await nav014('transcript'); await nav014('settings');
      assert(await js(`document.getElementById('settings-density').value === 'compact' && document.documentElement.dataset.density === 'compact'`), 'density lost on navigation');
      await js(`(() => { const s = document.getElementById('settings-density'); s.value = ${JSON.stringify(old)}; s.dispatchEvent(new Event('change')); })()`);
      return { file: await shot('014-settings') };
    });

    await step('014-memory-tabs-work-with-keyboard-and-show-day-attribution', async () => {
      await nav014('memory');
      await js(`(() => { const tab = document.querySelector('[data-memory-tab="recent"]'); tab.focus(); tab.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowRight', bubbles: true })); })()`);
      assert(await js(`document.activeElement.dataset.memoryTab === 'day' && document.querySelector('[data-memory-tab="day"]').getAttribute('aria-selected') === 'true'`), 'arrow navigation did not select By day');
      await waitFor('archive day loaded', () => js(`!!document.querySelector('.archive-content h2')`));
      assert(await js(`!!document.getElementById('archive-day')`), 'By day has no date control');
      const count = await js(`document.querySelectorAll('.archive-segment').length`);
      if (count) assert(await js(`[...document.querySelectorAll('.archive-segment .sub')].every(e => / · /.test(e.textContent))`), 'archive words lost attribution');
      await js(`document.querySelector('[data-memory-tab="day"]').dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowRight', bubbles: true }))`);
      await waitFor('Saved tab loaded', () => js(`document.querySelectorAll('[data-saved-group]').length === 2`));
      return { count, file: await shot('014-memory-saved') };
    });

    await step('014-fixed-search-survives-saving-and-reopening', async () => {
      await nav014('search');
      const bounds = await js(`(() => { const end = new Date(); const start = new Date(); start.setDate(start.getDate() - 7);
        const day = d => [d.getFullYear(), String(d.getMonth()+1).padStart(2,'0'), String(d.getDate()).padStart(2,'0')].join('-');
        document.getElementById('search-q').value = 'portal';
        document.getElementById('search-speaker').value = ''; document.getElementById('search-source').value = '';
        const from = document.getElementById('search-from'); const to = document.getElementById('search-to');
        from.value = day(start); to.value = day(end); from.dispatchEvent(new Event('change'));
        return { from: from.value, to: to.value }; })()`);
      await waitFor('fixed portal results', () => js(`document.querySelectorAll('#search-results .seg').length > 0`));
      fixed014 = await saveSearch014('E2E fixed ' + Date.now());
      assert(fixed014.filters.date.kind === 'fixed', 'fixed dates were stored as a rolling window');
      await saved014();
      await clickLabel014(`[data-saved-kind="searches"][data-saved-id="${fixed014.id}"]`, 'Run search');
      assert(await js(`document.getElementById('search-from').value === ${JSON.stringify(bounds.from)} && document.getElementById('search-to').value === ${JSON.stringify(bounds.to)} && document.getElementById('search-q').value === 'portal'`), 'reopened search changed its dates or query');
      return { id: fixed014.id, bounds };
    });

    await step('014-rolling-search-keeps-relative-date-intent', async () => {
      await clickLabel014('#search-date-scope', 'Last 7 days');
      rolling014 = await saveSearch014('E2E rolling ' + Date.now());
      assert(rolling014.filters.date.kind === 'rolling' && rolling014.filters.date.days === 7, 'relative scope lost when saved');
      await saved014();
      await clickLabel014(`[data-saved-kind="searches"][data-saved-id="${rolling014.id}"]`, 'Run search');
      assert(await js(`document.getElementById('search-q').value === 'portal' && !!document.getElementById('search-from').value`), 'rolling search did not reopen');
      return { id: rolling014.id };
    });

    await step('studio-search-keyboard-counts-and-retry', async () => {
      await js(`document.getElementById('search-mode-keyword').click()`);
      await waitFor('keyword search settled', () => js(`document.getElementById('search-results').getAttribute('aria-busy') === 'false'`));
      await deps.request('mock.search_fail_once', {});
      const busy = await js(`(() => {
        document.getElementById('search-q').value = 'portal';
        document.getElementById('search-go').click();
        return { busy: document.getElementById('search-results').getAttribute('aria-busy'), message: document.getElementById('search-result-status').textContent };
      })()`);
      assert(busy.busy === 'true' && /Searching/.test(busy.message), 'the in-flight search has no visible or accessible loading state');
      await waitFor('actionable search error', () => js(`!!document.querySelector('#search-results .search-feedback[role="alert"] button')`));
      assert(await js(`document.getElementById('search-q').value === 'portal' && document.getElementById('search-results').getAttribute('aria-busy') === 'false'`), 'search failure lost the query or left it busy');
      await clickLabel014('#search-results', 'Try again');
      const count = await waitFor('retry results', () => js(`document.getElementById('search-results').getAttribute('aria-busy') === 'false' && document.querySelectorAll('#search-results .seg').length`));
      assert(count > 1, 'fixture needs at least two results for keyboard navigation');
      const heading = await js(`document.getElementById('search-result-count').textContent`);
      assert(heading.startsWith(String(count)) && /match/.test(heading), `count does not explain the displayed page: ${heading}`);
      assert(await js(`document.querySelector('#search-results .seg').tagName === 'ARTICLE' && document.querySelector('#search-results .search-read').tagName === 'BUTTON'`), 'result mixes nested button roles instead of native context actions');
      const key = value => js(`document.activeElement.dispatchEvent(new KeyboardEvent('keydown', { key: ${JSON.stringify(value)}, bubbles: true }))`);
      await js(`document.getElementById('search-q').focus()`); await key('ArrowDown');
      assert(await js(`document.activeElement === document.querySelectorAll('#search-results .seg')[0]`), 'Down from query did not enter results');
      await key('ArrowDown');
      assert(await js(`document.activeElement === document.querySelectorAll('#search-results .seg')[1]`), 'Down did not move to the next result');
      await key('End');
      assert(await js(`document.activeElement === [...document.querySelectorAll('#search-results .seg')].at(-1)`), 'End did not select the last result');
      await key('Home'); await key('ArrowUp');
      assert(await js(`document.activeElement === document.querySelectorAll('#search-results .seg')[0]`), 'Up wrapped away from the first result');
      await key('Enter');
      await waitFor('keyboard context', () => js(`!!document.querySelector('.search-context-turn.selected')`));
      assert(await js(`document.querySelector('#search-results .seg').getAttribute('aria-current') === 'true'`), 'open context is not identified on its source result');
      assert(await js(`document.querySelector('.search-workspace').getBoundingClientRect().bottom <= document.querySelector('.main').getBoundingClientRect().bottom + 1`), 'reading panes extend below the available workspace instead of scrolling internally');
      await clickLabel014('.search-context', 'Close');
      assert(await js(`document.activeElement === document.querySelector('#search-results .seg')`), 'Close lost keyboard position');
      return { count, heading, file: await shot('studio-search-keyboard') };
    });

    await step('014-all-history-preserves-other-facets-and-context-preserves-results', async () => {
      await js(`(() => { const q = document.getElementById('search-q'); q.value = 'portal yesterday'; document.getElementById('search-ask').click(); })()`);
      await waitFor('explicit date interpretation', () => js(`!!document.querySelector('#ask-pills [data-facet="time"]')`));
      const before = await js(`window.__recallDebug.ask().pills.filter(p => p.facet !== 'time').map(p => [p.facet,p.text])`);
      await js(`document.getElementById('search-all-history').click()`);
      await waitFor('all-history results', () => js(`!document.querySelector('#ask-pills [data-facet="time"]') && document.querySelectorAll('#search-results .seg').length > 0`));
      const after = await js(`window.__recallDebug.ask().pills.filter(p => p.facet !== 'time').map(p => [p.facet,p.text])`);
      assert(JSON.stringify(before) === JSON.stringify(after), 'all-history altered another interpreted facet');
      assert(await js(`document.getElementById('search-date-scope').getClientRects().length > 0 && /all history/.test(document.getElementById('search-scope-label').textContent)`), 'active date scope is not visible');
      const ids = await js(`(() => { window.__searchRows014 = [...document.querySelectorAll('#search-results .seg')]; return window.__searchRows014.map(r => r.dataset.hit); })()`);
      await js(`document.querySelector('#search-results .seg').click()`);
      await waitFor('adjacent turns', () => js(`!!document.querySelector('.search-context-turn.selected')`));
      assert(await js(`window.__recallDebug.view() === 'search' && window.__searchRows014.every(r => r.isConnected)`), 'context replaced the results');
      await clickLabel014('.search-context', 'Close');
      assert(await js(`document.activeElement === window.__searchRows014[0] && document.querySelector('.search-context').hidden`), 'close did not return focus to the hit');
      assert(JSON.stringify(ids) === JSON.stringify(await js(`[...document.querySelectorAll('#search-results .seg')].map(r => r.dataset.hit)`)), 'closing context changed result order');
      await js(`document.querySelector('#search-results .seg').click()`);
      await waitFor('context reopened', () => js(`!!document.querySelector('.search-context-turn.selected')`));
      const normalSize = win().getSize();
      let narrow;
      try {
        win().setSize(1024, 768); await sleep(300);
        assert(await js(`document.documentElement.scrollWidth <= window.innerWidth && [...document.querySelectorAll('.search014, .search-workspace, .search-result-card, .search-context')].every(e => e.getBoundingClientRect().right <= window.innerWidth + 1)`), 'search/context overflows the supported narrow window');
        narrow = await shot('014-search-context-1024');
      } finally { win().setSize(...normalSize); await sleep(200); }
      return { hits: ids.length, narrow, file: await shot('014-search-context') };
    });

    await step('014-search-saves-a-contiguous-moment-with-a-personal-note', async () => {
      await clickLabel014('.search-context', 'Save moment');
      await waitFor('moment range picker', () => js(`!!document.getElementById('moment-start')`));
      const chosen = await js(`(() => { const a = document.getElementById('moment-start'), b = document.getElementById('moment-end');
        if (a.options.length < 2) throw new Error('Fixture has no adjacent turn');
        a.value = a.options[0].value; b.value = b.options[1].value;
        document.getElementById('moment-title').value = 'E2E conversation'; document.getElementById('moment-note').value = 'My own note';
        return [Number(a.value), Number(b.value)]; })()`);
      await js(`document.getElementById('moment-save').click()`);
      await waitFor('moment dialog closed', () => js(`!document.getElementById('moment-title')`));
      moment014 = await waitFor('saved moment on wire', async () => (await readSaved014('moments')).find(m => m.title === 'E2E conversation'));
      assert(JSON.stringify(moment014.segment_ids) === JSON.stringify(chosen), 'saved range differs from chosen adjacent turns');
      assert(moment014.note === 'My own note', 'personal note was not stored');
      return { id: moment014.id, chosen };
    });

    await step('014-transcript-picker-filters-and-save-moment-is-reachable', async () => {
      await clickLabel014('.search-context', 'Open transcript');
      await waitFor('transcript hit', () => js(`!!document.querySelector('#seg-list .seg.hit')`));
      await js(`(() => { const r = document.querySelector('#seg-list .seg.hit'); r.focus(); r.click(); })()`);
      await waitFor('speaker filter', () => js(`!!document.getElementById('speaker-find')`));
      await js(`(() => { const i = document.getElementById('speaker-find'); i.value = 'no-person-with-this-name-014'; i.dispatchEvent(new Event('input')); })()`);
      assert(await js(`document.querySelectorAll('.sp-pick button').length === 1 && /Unassigned/.test(document.querySelector('.sp-pick').textContent)`), 'speaker filter did not narrow to the unassigned option');
      await js(`document.getElementById('segment-save-moment').click()`);
      await waitFor('transcript save moment', () => js(`!!document.getElementById('moment-save')`));
      assert(await js(`document.getElementById('moment-title').value === ''`), 'transcript text was copied into a saved title');
      await js(`document.getElementById('moment-title').value = 'E2E transcript entry'; document.getElementById('moment-save').click()`);
      await waitFor('transcript bookmark persisted', async () => (await readSaved014('moments')).some(m => m.title === 'E2E transcript entry'));
    });

    await step('014-replay-can-save-a-range-at-the-current-turn', async () => {
      await nav014('search');
      await js(`document.getElementById('search-q').value = 'portal'; document.getElementById('search-all-history').click()`);
      await waitFor('replayable search result', () => js(`!!document.querySelector('#search-results .hit-replay')`));
      await js(`document.querySelector('#search-results .hit-replay').click()`);
      await waitFor('conversation replay', () => js(`!!document.getElementById('replay-save-moment') && !document.getElementById('replay-bar')?.hidden`));
      await js(`document.getElementById('replay-save-moment').click()`);
      await waitFor('replay moment picker', () => js(`!!document.getElementById('moment-start')`));
      const selected = await js(`(() => { const start = document.getElementById('moment-start'), end = document.getElementById('moment-end');
        const anchor = start.value; const index = [...end.options].findIndex(o => o.value === anchor);
        end.value = end.options[Math.min(end.options.length - 1, index + 1)].value;
        document.getElementById('moment-title').value = 'E2E replay entry';
        return { start: Number(start.value), end: Number(end.value) }; })()`);
      await js(`document.getElementById('moment-save').click()`);
      const record = await waitFor('replay range persisted', async () => (await readSaved014('moments')).find(m => m.title === 'E2E replay entry'));
      assert(record.segment_ids[0] === selected.start && record.segment_ids.at(-1) === selected.end, 'replay saved a different range from the picker');
      return { ids: record.segment_ids };
    });

    await step('014-saved-items-edit-and-remove-without-deleting-transcript', async () => {
      await saved014();
      const scope = `[data-saved-kind="moments"][data-saved-id="${moment014.id}"]`;
      await clickLabel014(scope, 'Edit');
      await js(`document.getElementById('saved-edit-title').value = 'E2E renamed'; document.getElementById('saved-edit-note').value = 'Updated personal note'`);
      await clickLabel014('.sheet', 'Save');
      await waitFor('renamed saved row', () => js(`document.querySelector(${JSON.stringify(scope)} + ' h3')?.textContent === 'E2E renamed'`));
      await nav014('search'); await saved014();
      assert(await js(`document.querySelector(${JSON.stringify(scope)}).textContent.includes('Updated personal note')`), 'edited note did not survive reopening');
      await clickLabel014(scope, 'Remove'); await clickLabel014('.sheet', 'Remove');
      await waitFor('bookmark removed', async () => !(await readSaved014('moments')).some(m => m.id === moment014.id));
      const context = await js(`(async () => (await window.recall.request('segments.context', { id: ${moment014.segment_ids[0]} })).data)()`);
      assert(context.segments.some(s => s.id === moment014.segment_ids[0]), 'removing a bookmark deleted its source turn');
      for (const record of [fixed014, rolling014]) {
        await waitFor('saved search row', () => js(`!!document.querySelector('[data-saved-kind="searches"][data-saved-id="${record.id}"]')`));
        await clickLabel014(`[data-saved-kind="searches"][data-saved-id="${record.id}"]`, 'Remove'); await clickLabel014('.sheet', 'Remove');
        await waitFor('saved query removed', async () => !(await readSaved014('searches')).some(s => s.id === record.id));
      }
    });

    await step('014-sheet-keyboard-traps-and-restores-focus', async () => {
      await nav014('search');
      await js(`document.getElementById('search-save').focus(); document.getElementById('search-save').click()`);
      assert(await js(`document.activeElement.id === 'saved-search-name'`), 'saved search did not focus its name');
      await js(`document.activeElement.dispatchEvent(new KeyboardEvent('keydown', { key: 'Tab', shiftKey: true, bubbles: true }))`);
      assert(await js(`document.activeElement.textContent === 'Save search'`), 'Shift+Tab escaped the sheet');
      await js(`document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))`);
      assert(await js(`!document.querySelector('.sheet') && document.activeElement.id === 'search-save'`), 'Escape did not restore the original control focus');
    });

    await step('014-settings-and-sources-categories-keep-controls-and-keyboard-focus', async () => {
      const summaries = [];
      for (const [view, first, last, next] of [['settings', 'appearance', 'quality', 'processing'], ['sources', 'capture', 'storage', 'captions']]) {
        await js(`window.__recallDebug.go(${JSON.stringify(view)}, { section: ${JSON.stringify(first)} })`);
        const info = await js(`(() => {
          const nav = document.querySelector('.section-nav');
          const tabs = [...nav.querySelectorAll('[role="tab"]')];
          const originalPanels = [...document.querySelectorAll('.section-panel')];
          window.__sectionControl014 = document.querySelector(${JSON.stringify(view === 'settings' ? '#settings-density' : '#mic-card')});
          const tab = tabs.find(t => t.dataset.section === ${JSON.stringify(first)});
          tab.focus(); tab.dispatchEvent(new KeyboardEvent('keydown', { key: 'End', bubbles: true }));
          return { selected: nav.querySelector('[aria-selected="true"]').dataset.section,
            focused: document.activeElement.dataset.section, visible: originalPanels.filter(p => !p.hidden).map(p => p.dataset.section),
            tabstops: tabs.filter(t => t.tabIndex === 0).length, count: tabs.length };
        })()`);
        assert(info.count === 4 && info.tabstops === 1, `${view} category tab stops invalid: ${JSON.stringify(info)}`);
        assert(info.selected === last && info.focused === last && info.visible.join() === last, `${view} End key did not select/focus its last category`);
        await js(`document.activeElement.dispatchEvent(new KeyboardEvent('keydown', { key: 'Home', bubbles: true })); document.activeElement.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowDown', bubbles: true }))`);
        assert(await js(`document.activeElement.dataset.section === ${JSON.stringify(next)}`), `${view} arrow key did not move categories`);
        await js(`document.querySelector('.section-nav [data-section="${first}"]').click()`);
        assert(await js(`window.__sectionControl014 === document.querySelector(${JSON.stringify(view === 'settings' ? '#settings-density' : '#mic-card')})`), `${view} navigation remounted an existing control`);
        summaries.push({ view, ...info });
      }
      return summaries;
    });

    await step('014-quick-switch-filters-and-opens-a-settings-category', async () => {
      await nav014('transcript');
      await js(`document.getElementById('quick-switch').focus(); document.getElementById('quick-switch').click()`);
      await waitFor('quick switch input', () => js(`document.activeElement.id === 'quick-switch-input'`));
      await js(`(() => { const input = document.getElementById('quick-switch-input'); input.value = 'Processing'; input.dispatchEvent(new Event('input', { bubbles: true })); })()`);
      const names = await js(`[...document.querySelectorAll('.quick-result')].map(b => b.textContent)`);
      assert(names.length > 0 && names.some(name => /processing/i.test(name)), `quick switch did not find Processing: ${JSON.stringify(names)}`);
      await js(`document.querySelector('.quick-result').click()`);
      await waitFor('processing settings selected', () => js(`window.__recallDebug.view() === 'settings' && document.querySelector('.section-nav [aria-selected="true"]')?.dataset.section === 'processing'`));
      assert(await js(`!document.getElementById('quick-switch-input')`), 'quick switch stayed open after navigation');
      await js(`document.dispatchEvent(new KeyboardEvent('keydown', { key: 'k', ctrlKey: true, bubbles: true }))`);
      await waitFor('keyboard quick switch', () => js(`!!document.getElementById('quick-switch-input')`));
      await js(`document.activeElement.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowDown', bubbles: true }))`);
      assert(await js(`document.activeElement === document.querySelector('.quick-result')`), 'quick switch ArrowDown did not focus the first result');
      await js(`document.activeElement.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowUp', bubbles: true }))`);
      assert(await js(`document.activeElement.id === 'quick-switch-input'`), 'quick switch ArrowUp did not return to the input');
      await js(`document.activeElement.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }))`);
      await waitFor('keyboard command opens transcript', () => js(`!document.getElementById('quick-switch-input') && window.__recallDebug.view() === 'transcript'`));
      await js(`document.dispatchEvent(new KeyboardEvent('keydown', { key: 'k', ctrlKey: true, bubbles: true }))`);
      await waitFor('quick switch reopened', () => js(`!!document.getElementById('quick-switch-input')`));
      await js(`document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))`);
      assert(await js(`!document.getElementById('quick-switch-input')`), 'Escape did not close quick switch');
      return { names, file: await shot('014-quiet-settings') };
    });

    await step('014-quiet-layout-fits-760-and-1024-without-horizontal-overflow', async () => {
      const normalSize = win().getSize();
      const normalMinimum = win().getMinimumSize();
      const wasCollapsed = await js(`document.body.dataset.rail === 'collapsed'`);
      const layouts = [];
      try {
        win().setMinimumSize(600, 500);
        if (wasCollapsed) await js(`document.getElementById('rail-collapse').click()`);
        for (const width of [760, 1024]) {
          win().setSize(width, 768); await sleep(250);
          for (const [view, section] of [['transcript', null], ['memory', null], ['settings', 'appearance'], ['settings', 'processing'], ['settings', 'language'], ['settings', 'quality'], ['sources', 'capture'], ['sources', 'captions'], ['sources', 'connections'], ['sources', 'storage']]) {
            await js(`window.__recallDebug.go(${JSON.stringify(view)}, ${JSON.stringify(section ? { section } : null)})`);
            await sleep(160);
            const size = await js(`(() => {
              const main = document.getElementById('main');
              return { viewport: innerWidth, document: document.documentElement.scrollWidth, main: main.clientWidth, content: main.scrollWidth,
                selected: document.querySelector('.section-nav [aria-selected="true"]')?.dataset.section ?? null };
            })()`);
            assert(size.document <= size.viewport + 1, `${view}/${section} overflows viewport at ${width}: ${JSON.stringify(size)}`);
            assert(size.content <= size.main + 1, `${view}/${section} overflows content at ${width}: ${JSON.stringify(size)}`);
            if (section) assert(size.selected === section, `${view} selected ${size.selected}, expected ${section}`);
            layouts.push({ width, view, section, ...size });
          }
          await shot(`014-quiet-sources-${width}`);
        }
      } finally {
        win().setMinimumSize(...normalMinimum); win().setSize(...normalSize);
        if (wasCollapsed !== await js(`document.body.dataset.rail === 'collapsed'`)) await js(`document.getElementById('rail-collapse').click()`);
        await sleep(200);
      }
      return layouts;
    });

    await step('014-collapsed-navigation-survives-renderer-reload', async () => {
      const collapsed = await js(`document.body.dataset.rail === 'collapsed'`);
      try {
        if (!collapsed) await js(`document.getElementById('rail-collapse').click()`);
        assert(await js(`document.body.dataset.rail === 'collapsed' && document.getElementById('rail-collapse').getAttribute('aria-expanded') === 'false'`), 'collapse did not update layout and accessible state');
        await nav014('settings');
        assert(await js(`document.body.dataset.rail === 'collapsed'`), 'navigation reset collapsed state');
        await new Promise(resolve => { win().webContents.once('did-finish-load', resolve); win().webContents.reload(); });
        await waitFor('reloaded app ready', () => js(`!!window.__recallDebug && !!document.querySelector('#seg-list')`), { timeout: 15000 });
        assert(await js(`document.body.dataset.rail === 'collapsed'`), 'collapsed preference did not survive reload');
        return { file: await shot('014-quiet-collapsed') };
      } finally {
        if (collapsed !== await js(`document.body.dataset.rail === 'collapsed'`)) await js(`document.getElementById('rail-collapse').click()`);
      }
    });

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
