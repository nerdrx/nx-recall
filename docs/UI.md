# Quiet Studio

The desktop is a reading workspace for local conversations. Neutral surfaces,
clear headings, and predictable navigation keep the transcript more prominent
than its controls. The NX wordmark and violet accent remain the identity;
cyan still means active capture. Both light and dark themes use the same
component layout and semantic colors.

## Navigation

- The sidebar keeps six stable destinations and their existing shortcuts.
  It can be collapsed explicitly; the preference survives restart. Windows
  narrower than 900 pixels use an icon rail automatically. Icon buttons retain
  accessible names, including the current pause/resume action.
- Ctrl/Cmd+K opens Quick switch. Match destination names or aliases, use arrow
  keys and Enter, or search all retained conversations for entered words.
  Ctrl/Cmd+F still opens Search directly. No global desktop shortcuts are added.
- Settings and Sources use `lib/section-nav.js`: native tab buttons with arrow,
  Home/End navigation, one visible panel, and preserved controls. Switching
  category hides panels without recreating them or resetting unsaved controls.
  `ctx.go(view, {section})` supports direct destinations.
- A skip link and a named main region help keyboard and screen-reader users.
  Dialogs retain the shared focus containment and restoration behavior.

## Reading and controls

Search has a compact query/refinement area above results and conversation
context. Each pane can scroll independently. Result articles support keyboard
navigation and native context/replay actions. Counts distinguish limited lists,
loading is exposed with `aria-busy`, and errors provide a retry with the query
intact. Narrow windows stack the reading panes.

Memory keeps Recent, By day, Saved, and Commitments. Recent uses a responsive
overview; saved items are readable list entries. Speakers use aligned rows
with less repeated card decoration. The transcript keeps its existing bounded
window, replay, corrections, speaker labels, and retention semantics.

`tokens.css` owns color, geometry, spacing, motion, and type. Shared components
live in `styles.css`; Search has a scoped layout stylesheet. There is no new
UI framework, font download, model, network service, or animation dependency.
The former small 0.14 override files have been folded into shared styles.

Input boundaries use `--control-line`, independently of decorative dividers.
Text and controls are checked against both theme surfaces; focus is visible
without relying on background color alone. Reduced-motion rules remain in
force. Density changes reading spacing, not labels or access to controls.

## Transcript work

Conversation separators previously scanned the entire loaded transcript for
each conversation label. `threadNameResolver` builds a lazy participant index
once per render pass, retaining first-seen speaker order and resolving the
current names when needed. This replaces O(rows × conversations) scanning with
one O(rows) indexing pass plus the requested labels. The index is temporary,
so it cannot survive an edit or keep stale speaker assignments across renders.

Run `node gui/scripts/transcript_bench.mjs` for the synthetic before/after
lookup benchmark. It validates equivalent names and reports warm median times
and rows visited. It excludes DOM construction, browser layout, model
inference, and whole-app latency; its ratios are not whole-app speed claims.

## Verification

`npm --prefix gui test` covers command matching, category control preservation,
calendar dates, transcript labels, and established frontend behaviors.
`bash gui/scripts/headless_test.sh` drives the real Electron UI against a
private synthetic daemon inside headless gamescope. It covers light/dark,
760/1024-pixel layouts, keyboard behavior, categories, collapsed navigation,
search context, and the existing recording/privacy workflows.


## 0.16: full history, collections, and measurements

Memory History requests `history.page` in 100-turn pages. The opaque cursor
binds the requested date range and a maximum row ID; later captures cannot
shift subsequent pages. The history index follows `(t_start_ns,id)` and filters
deleted rows. Saved collections use schema v23; deleting a collection unfiles
its moments without deleting source content. Saved moment detail fetches fresh
visible segments, and replay passes exactly that range to the shared player.

Search preserves individual result order and navigation while giving adjacent
nearby hits a conversation heading. Literal highlights are DOM text/mark nodes,
never query-generated HTML. A linked audio path is labelled “Recording linked”,
not verified playable audio; actual playback retains its normal checks.

Settings Performance polls only while its category is open. `performance.get`
reports rolling nearest-rank p50/p95, latest and maximum over at most 256 samples.
Audio-to-transcript uses the captured monotonic sample endpoint to the stored
nonempty transcript before semantic enrichment. Search timings cover daemon
request handling (including failed searches and answer generation), excluding
transport and rendering. Resident memory is Linux VmRSS for recalld alone;
queue/drop counters are current-process capture data. No samples render as an
em dash, and failed refreshes label retained data stale. All measurements reset
at daemon restart and keep no transcript/query text.


### Synthetic repair-queue benchmark

Run `cargo test -p recalld --lib repair_candidate_selection_benchmark -- --ignored --nocapture`.
The fixture inserts synthetic transcript rows into in-memory SQLite and compares
20 candidate reads using the old full UNION against bounded branch reads. On
this development machine, 10,000 pending rows took 133.36 ms vs 4.79 ms; 100,000
took 1,221.13 ms vs 3.36 ms. This measures queue-selection SQL only, without model
inference, audio, disk I/O, or screen rendering. It is not an end-to-end speedup
claim. Scheduling regressions separately verify unlocked inference, pause/stop,
capture/store priority, durable resume, bounded backoff and stale-result guards.


## 0.17: conversation finder, review inbox, and windowed history

Recorded words keep all fetched cursor pages in order, but mount only the viewport plus 600px on either side and any focused turn. ResizeObserver records each full, naturally wrapped card's height. No transcript truncation or fixed row height is introduced. Measured offsets support binary lookup; page append and deletion preserve the reading anchor. Arrow keys and Home/End cross virtual row boundaries. Purging a focused turn moves focus to its retained neighbor.

`#archive-history.historyState()` exposes loaded IDs, rendered IDs, and measurements for integration validation. The existing 305-turn cursor test still checks every source ID and tied timestamp order, then checks bounded DOM, offscreen correction, long text, and focused deletion.

Node model benchmark (`node gui/bench/history-window.mjs`, local run): 50,000 variable-height rows built in 15.1ms; 10,000 binary viewport lookups in 0.79ms versus 220.8ms for linear reference lookup. Both compute identical result checksums. This measures the layout index only, not browser rendering or daemon query latency. Browser validation remains required for end-to-end claims.

Related moments use retained source-word overlap, explain the 24-query-word / 200-candidate limit, show original evidence, and open the selected saved range in one replacement dialog. Personal notes are excluded from matching; no generated summary or semantic completeness claim is made. Corrections and purges invalidate displayed related evidence.


Conversation Find loads the complete `thread.get` result rather than searching
only the current transcript window. It uses literal text/grapheme ranges, next
and previous matches, and a timestamp timeline. Controlled jumps preserve the
finder state across transcript remounts; other navigation closes it. Edits and
purges invalidate stale loaded results, including replies already in flight.

Review is an explicit Memory tab, loaded only while visible. It reuses the shared
audio player and existing correction measurements. Typed drafts are retained
with a warning when remote edits arrive; purges remove the card and its draft.
Reviewed state is revision guarded, and a correction sends the expected original
text before marking the new revision. No automatic edits or unmeasured accuracy
claims are made. Performance stages require real captured workload to populate;
synthetic layout benchmarks do not measure ASR speed or hardware accuracy.
