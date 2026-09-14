# Changelog

## 0.19.0 — Hear it, correct it, keep it once

- Show live input and reply meters in Lanalu’s tab, with clearer virtual device names and connection instructions. Meters distinguish received sound from connected-but-silent routing.
- Show the last six recognized voice turns, including missed wake names, in a private recent-turn view.
- Correct heard words inline with nearby save feedback and draft preservation. Save labeled examples in Recall without rewriting original transcripts or replaying a reply.
- Let both shared and separate recognition use trusted-name assistance. Ordinary phrases are not added as wake aliases.
- Add opt-in, source-specific suppression of strongly confirmed microphone copies in new recordings, preserving a lightweight source observation for shared recognition. Keep uncertain, overlapping and source-first captures.
- Redesign the README with a new cover, concise product tour and chibi Lanalu artwork.

## 0.18.3 — A voice that feels closer

- Add four optional local Kokoro voices, speaking-speed controls, and Amy synthesis variation. Models download only during explicit setup.
- Stream native speech chunks into playback and use six synthesis threads for Kokoro by default, reducing the tested sample’s first-audio wait without changing its text or speaking pace.
- Accept split Lanalu wake spellings including “La nalu”, “Lana Lou” and “Lana Loo”. Ordinary “no no no” is not made a wake phrase.
- Match shared recognition against the actual captured audio window, including its padding, while retaining source, age, coverage and duplicate guards.
- Add optional experimental corrected-name assistance: acoustic spelling hints can replace one trusted name while preserving all other recognized words. This is not model-weight retraining and defaults off.

## 0.18.2 — Shared words, clearer saves

- Name the virtual-audio mode **Virtual in/out** throughout Lanalu’s controls and documentation.

- Choose shared Recall recognition (default) or a separate local recognizer in Lanalu's tab. Shared mode checks the capture source and waits for matching transcript turns without running duplicate speech recognition.
- Show recognition waits and unavailable inputs in Lanalu's Debug window instead of silently falling back to a different recognizer.
- Keep save feedback beside the action across settings, speaker names, transcript corrections, review, notes, moments, searches and collections. Prevent duplicate submissions and preserve failed drafts.
- Replace repeated historical speaker-activity scans with a single timestamp lookup pass, preserving overlap and backfill behavior. The measured candidate query returned identical results in 52 ms versus 23.2 seconds on the tested archive.
- Move expensive speaker-calibration fitting outside the archive mutex so fitting does not block capture and user actions. Defer installing a fit if the archive changed while it was computed.
- Rebuild the README around conversations, memories and Lanalu, with actual app screenshots using demonstration data and original character artwork.

## 0.18.1 — A home for Lanalu

- Upgrade Lanalu and the optional Recall memory model to Qwen3.5 4B, with thinking disabled for direct, bounded replies.

- Lanalu has a dedicated navigation tab, typed input and a separate diagnostic window.
- Clock the private virtual audio devices even when no physical output drives their graph, fixing silent or stalled voice playback.
- Keep capture running when PipeWire reports a vanished stream during audio routing; only a broken core connection ends capture.
- Retry voice routing failures even when cleanup also encounters unavailable audio devices, and preserve failure details after stopping.
- Introduce Lanalu with newly generated cinematic README artwork and keep the full technical reference in the documentation.

## 0.18.0 — Local voice, shared memory

- **Local Voice in Recall.** Start and stop Lanalu from Settings, with optional
  startup alongside the app. The desktop app owns the worker; there is no extra
  bridge service or paid API.
- **Two audio modes.** Connect the dedicated voice-client call through private
  virtual speaker/microphone devices, or choose a microphone and normal local
  output. Other Discord clients and default routes stay unchanged. Recall's
  existing recording connection is preserved.
- **Offline conversation.** Parakeet hears speech, Qwen generates concise
  replies through llama.cpp Vulkan, and Piper speaks. Wake names and
  always-listening modes run locally. Call mode supports interruption; local
  speaker mode gates capture during playback to prevent feedback.
- **Recall-backed answers and names.** Bounded local search provides memory.
  Only confident, temporally aligned acoustic matches with an assigned name
  identify a speaker; generic labels and uncertainty stay unknown. Matching
  can lag behind a reply and is not an authentication mechanism.
- **Optional component setup.** Local Voice has its own runtime and model
  setup; existing model files are reused. Downloads occur during setup,
  not during conversation. English speech models are the initial default.
- Tested with synthetic end-to-end local speech, isolated PipeWire routes,
  and hidden Gamescope UI interactions. Real-person recognition accuracy and
  live conversation quality still depend on enrolled voices and the machine.

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
