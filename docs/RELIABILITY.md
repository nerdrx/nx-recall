# Recall reliability review — September 2026

## Architecture and scope

NX Recall keeps capture and inference local. PipeWire feeds a bounded audio
queue; the daemon segments and transcribes turns, assigns speaker identities,
and stores results in SQLite. FTS5 supplies lexical retrieval, while a resident
vector index supplies semantic retrieval. Electron communicates with the daemon
through a reconnecting NDJSON Unix-socket client; the VR overlay is another
local presentation surface.

This change preserves those boundaries, model choices, protocol, and database
schema. It targets two reproducible defects and incorporates the existing
approved NX wordmark into the desktop rail and README banner.

## Semantic refresh correctness

The resident vector index previously advanced its database scan watermark when
live capture supplied a new vector. On startup, that could skip all vectors
already on disk. Between searches, it could also skip a vector updated by an
external backfill before the next live write.

Live writes still update resident vectors immediately. Only a completed database
scan now advances the scan watermark. Two synthetic regressions exercise the
startup archive and interleaved backfill cases; both fail with the old behavior
and pass with the fix. The existing 100,000-vector search performance guard
continues to pass. No inference model is needed for these regressions.

## Desktop frame parsing

The old client checked only the unfinished tail after parsing complete lines,
so a complete oversized frame bypassed the limit. It also counted JavaScript
string units instead of UTF-8 bytes. The parser now checks each frame before
parsing, rejects oversized complete and partial frames, and applies independent
budgets to coalesced frames. Six regressions cover those boundaries, multilingual
text, whitespace, fragmentation, and exactly-at-limit payloads.

The parser retains fragments and joins once when the newline arrives. It no
longer repeatedly concatenates and scans the growing audio reply. To reproduce
the synthetic benchmark from the repository root:

```sh
node gui/scripts/framing_bench.mjs
# Optionally compare an earlier client module:
node gui/scripts/framing_bench.mjs /path/to/baseline-client.mjs
```

A 12 MiB synthetic audio-sized frame, delivered in 64 KiB chunks, measured
118.75 ms before and 4.83 ms after (median of five measured iterations after
one warmup, Node v26.8.1 on the same machine). This is a parser microbenchmark,
not an end-to-end audio playback or transcription measurement. No recordings,
models, or daemon connection are used.

## Validation

- `cargo test --workspace --offline`: 1,483 passed, 5 explicitly ignored.
  Model-dependent tests retain their existing availability gates; this run does
  not establish real-model transcription accuracy.
- `npm test` in `gui`: 213 passed.
- Headless gamescope UI suite: 109 passed in light mode and 109 in dark mode,
  against a private mock socket. Both compositor screenshots were inspected
  for the wordmark's theme selection and layout.
- Workspace formatting and patch whitespace checks pass.

## Remaining opportunities

- Track vector deletion generations. A same-count deletion and insertion can
  reuse a sequence value under the current `MAX(seq) + 1` allocation, making
  count-based refresh invalidation insufficient. This deserves a dedicated
  schema/invalidation change with cross-connection regressions.
- Restrict FTS trigger work to text changes, after measuring metadata-heavy
  updates. Speaker or annotation updates should not require rebuilding text
  entries unnecessarily.
- Distinguish stored-vector coverage from current-text coverage in status.
  Existing vectors can still be pending re-embedding after text changes.

Those are follow-up candidates, not claims of improvements shipped here.
