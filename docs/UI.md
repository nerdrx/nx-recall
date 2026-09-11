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
