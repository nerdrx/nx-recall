# Changelog

## 0.17.1 — Find the right speaker

- Search the Speakers page by name, voice label, or ID; combine with Named or
  Unnamed filters, with live result counts and a clear reset. Queries survive
  live speaker updates.
- Narrow long speaker selectors in Transcript, Search, and Discord linking
  without changing the selected identity while typing.
- Search merge destinations and transcript assignment choices consistently,
  ignoring case and accents. Large choice lists reveal 40 voices at a time
  with keyboard navigation and explicit empty states.

## 0.17.0 — Read, review, and connect

- Search the complete conversation from its transcript boundary, with literal
  Unicode-safe highlights, next/previous matches, and a clickable match timeline.
- Add a recognition review inbox for uncertain words, with listening, guarded
  corrections, revision-aware reviewed state, and clearly scoped correction
  evidence. Speaker identity uncertainty stays separate.
- Suggest related saved moments from shared original words, showing the
  matching words and retained source evidence without requiring another model.
- Window large day histories using measured row heights: full text and all
  fetched pages remain available while only nearby/focused rows are mounted.
- Measure queue wait, recognition, voice analysis, saving, refinement, and live
  semantic indexing separately in Settings → Performance. Measurements are
  bounded, process-local, and contain no transcript/query text.
- Schema v24 adds source-linked review revisions; corrections can reject stale
  edits using an optional expected-text guard. Quiet Studio and the NX logo stay.

## 0.16.0 — Keep the whole conversation

- Automatically repair the semantic search index in the background, yielding
  to capture and active inference. Durable pending work resumes after restart;
  edits and deletions cannot be overwritten by stale model results.
- Browse a full day of history with stable, bounded pages beyond the previous
  200-turn preview. Later inserts cannot shift an in-progress history walk.
- Organize saved moments into collections, inspect their complete saved range,
  and replay that range directly through the shared player. Collections and
  bookmarks remain references; source deletion and retention still apply.
- Highlight literal search matches safely, group nearby ranked hits by
  conversation, and distinguish linked recordings from text-only results.
- Add Settings → Performance with bounded rolling transcript/search timings,
  daemon resident memory, recording queue and dropped-audio counters, and
  background index repair state. Missing samples remain explicitly unknown.
- Add schema v23 for saved collections and indexed chronological history.

## 0.15.0 — Quiet Studio

- Redesigned the desktop around flat reading surfaces, sharper typography, and
  restrained NX accents in both themes.
- Collapsible sidebar with persistent preference, narrow-window layout, named
  icon controls, skip link, and keyboard navigation.
- Ctrl/Cmd+K Quick switch finds views by name or alias and searches retained
  conversations directly.
- Focused Settings and Sources categories preserve controls while switching.
- Compact Search controls, independent result/context panes, explicit loading
  and retry states, accurate counts, and keyboard/native result actions.
- Unified Memory, speaker-list, saved-item, input, and focus styling; removed
  the small 0.14 shared-style override files.
- Conversation labels share one participant index per render pass instead of
  repeatedly scanning the loaded transcript. The synthetic benchmark validates
  equivalent labels and reports lookup timings; it excludes DOM/layout work.
- No database migration, capture-policy change, or model dependency.

## 0.14.0 — Find and keep a moment

### Desktop

- Visible Last 7 days, All history, and custom Search scope; date-only browsing;
  nearby conversation context without leaving results; stale-response guards.
- Saved queries preserve filters and rolling/fixed date intent. Saved moments
  reference consecutive transcript turns and carry optional titles and personal
  notes. Reopen, edit, remove, and paginate saved items in Memory.
- Memory is organized into Recent, By day, Saved, and Commitments. Day browsing
  includes summaries and retained words with time, speaker, and source;
  unavailable summaries do not block transcript access.
- Processing, language, sound, and recognition-quality controls move to
  Settings. Sources retains capture, captions, storage, and backups. Density
  preference, shortcut help, clear capture status, and expandable diagnostics.
- Searchable speaker picker, keyboard Memory tabs, dialog focus containment and
  restoration, and Ctrl/Cmd+F from ordinary input fields.
- Existing NX branding and both themes are preserved.

### Correctness and performance

- Schema 21 adds transactional vector mutation/deletion tracking, exact coverage
  counters, and a durable dirty-text queue. Text edits invalidate stale vectors;
  metadata-only edits avoid full-text index rewrites.
- Immutable semantic snapshots move query inference, whitening, and ranking
  outside the database lock. Final freshness, deletion, and facet validation
  prevent stale results; status no longer loads/refits the search index.
- Schema 22 adds local saved queries and source-referencing moments. Deleted
  source text is not copied into saved tables; moments with no retained source
  turns are omitted from listing. Existing source retention still applies.

A model-free debug-build benchmark on the actual in-memory migrated schema
measured mean vector writes of **65.75 / 69.40 μs**, coverage reads of
**9.79 / 9.66 μs**, and fetching 100 dirty rows in **0.406 / 0.393 ms**, at
10,000 / 100,000 synthetic rows respectively (2026-09-11). These are database
operation measurements with synthetic 2D vectors, not end-to-end speedups;
model inference, disk/fsync, recordings, and GUI work are excluded. Reproduce
with `cargo test -p recalld --lib v21_synthetic_index_benchmark -- --ignored --nocapture`.

Cold index/delta reads still take the database lock, and replacing immutable
snapshots can temporarily increase matrix memory. Archived dirty text resumes
through `recalld semantic backfill`; there is no new automatic background
reindex scheduler and no ASR/GPU speedup claim.

## 0.13.2

- Fixed semantic results missing after restart or external backfill.
- Bounded large desktop socket replies before parsing and reduced fragmented
  reply copying.
- Included the approved NX wordmark in the desktop and README.
