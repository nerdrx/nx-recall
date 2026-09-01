# NX Recall — the memory graph (design, pre-implementation)

**people ↔ topics ↔ promises ↔ dates — locally, or not at all.**

Status: design. Scheduled after the 0.6.x batch (per-speaker languages, storage
panel, grunt handling). Everything here obeys the hard constraints in
[DESIGN.md](DESIGN.md) §0: no network at inference time, no torch, the user is
the only subject, deletion cascades.

## Why a graph when FTS works

Search answers "where did someone say X." It cannot answer "what do Kira and I
usually talk about," "who promised me what," or "what was I supposed to do by
Friday." Those are edges, not rows. The graph is the memory-refresh promise of
§0 taken seriously: not analytics about people — an index into *your own*
conversations.

## Three tiers, by trust

### Tier 1 — derived, deterministic, always on

Pure SQL/code over data we already store. No models, no judgement calls.

- **Conversation threads**: turn-taking adjacency (A,B,A,B interleaving from
  the original brief) materialised as `threads(id)` + `segment_thread`.
- **Co-presence edges**: `person_edges(a, b, sessions, seconds_together,
  last_seen)` from sessions × roster × threads. Powers the person page's
  "you two, over time."
- **Talk-time and cadence per person**: already computable; becomes indexed.

### Tier 2 — extracted, rule-based, marked as such

Deterministic extractors; wrong sometimes, cheap always, provenance on every
row (`extractor`, `version`, `confidence`).

- **Time references**: de+en rule parser ("Freitag", "morgen Abend", "next
  week", "am dritten") → `time_refs(segment_id, resolved_utc, raw)`. Resolution
  is relative to the segment's capture time — that is the whole trick.
- **Topics**: multilingual sentence-embedding ONNX (~120 MB, `ort`, same
  no-torch stack as everything else) → vectors per segment (this is also the
  semantic search DESIGN §6 promised) → periodic clustering → `topics` labelled
  by top terms, `segment_topics` weighted. sqlite-vec for the ANN side.
- **Promise candidates**: modal-pattern lattice ("I'll …", "ich schick dir …",
  "mach ich bis …") + a person + an optional time_ref in range →
  `commitments(who, to_whom, what_segment, due?, state=candidate)`. Expected
  recall is modest; that is fine — candidates are suggestions, never actions.

### Tier 3 — understood, tiny local LLM, opt-in and idle-only

Commitment extraction done properly, topic naming that reads like a human wrote
it, and pre-conversation briefs ("last time: you owed her the shader link").

- **Tiny by requirement, not by concession** (user constraint: ~4 CPU cores,
  GPU only if it must). Budget: ≤ 3B parameters, Q4 GGUF, ≤ ~2 GB on disk.
  Candidates to evaluate, de+en capable: Qwen2.5-1.5B-Instruct (primary),
  Gemma-2-2B, Qwen2.5-3B as the ceiling. This works because the task is
  narrow extraction over short windows with **grammar-constrained decoding**
  (GBNF → the model physically cannot emit anything but schema-valid JSON) —
  the regime where small models are strong. Anything the small model marks
  low-confidence stays a candidate; it never gets a bigger model's swagger.
- llama.cpp, CPU-first: `n_threads = 4`, pinned via the existing
  `[runtime] inference_cpus` mechanism, nice 19 — the same discipline as ASR.
  Napkin math: a 1.5B Q4 at ~30 tok/s on 4 Zen 5 cores chews through a full
  evening's transcript in low minutes of idle time. Optional
  `[graph] gpu_layers` for ROCm offload exists but the default is 0.
- No network, no torch; the model is fetched once by `models fetch` like
  everything else, byte-verified.
- Runs ONLY as an idle-time enrichment pass: never while a game runs (same
  detection §4 uses), never in the capture path, budgeted and interruptible.
- Output is annotations referencing segments (`derived_*` tables), never
  modifications of the transcript. Deleting a segment/speaker cascades through
  every derived row (§0 deletion rule).
- Off by default. Its switch sits next to the mic's, with equally plain copy.

## Surfaces

- **Person page**: the voice, shared history timeline, common topics, open
  commitments both directions, co-presence sparkline.
- **Commitments view**: candidate → confirmed → done/dismissed, due dates from
  time_refs. Confirmation is a human click; the tool never nags on a guess.
- **Thread view in the transcript**: the interleaved lobby untangled.

## Charter guards

- Nothing inferred ever flows to NX Orbit (DESIGN §9 stands).
- Derived tables are second-class citizens of deletion: purging a person purges
  their nodes, edges, topics-participation, commitments — then `VACUUM`.
- Every derived row carries provenance and is re-derivable from the transcript;
  the graph is an index, never the source of truth.
