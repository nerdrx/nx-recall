# NX Recall — the memory graph (design, pre-implementation)

**people ↔ topics ↔ promises ↔ dates — locally, or not at all.**

Status: **Tier 1 shipped in 0.6.2** (conversation threads, co-presence edges,
the person page); Tiers 2 and 3 are still design. Everything here obeys the hard
constraints in [DESIGN.md](DESIGN.md) §0: no network at inference time, no
torch, the user is the only subject, deletion cascades.

## Why a graph when FTS works

Search answers "where did someone say X." It cannot answer "what do Kira and I
usually talk about," "who promised me what," or "what was I supposed to do by
Friday." Those are edges, not rows. The graph is the memory-refresh promise of
§0 taken seriously: not analytics about people — an index into *your own*
conversations.

## Three tiers, by trust

### Tier 1 — derived, deterministic, always on — **shipped in 0.6.2**

Pure SQL/code over data we already store. No models, no judgement calls.

- **Conversation threads**: turn-taking adjacency (A,B,A,B interleaving from
  the original brief) materialised as `threads(id)` + `segments.thread_id`
  (schema v6). The rule lives in `crates/recalld/src/threads.rs` as a pure
  function with a table of tests; it is documented there in full, including
  what it gets wrong on purpose.

  Three decisions worth keeping in one place:

  - **Incremental, not a batch job.** Each turn is threaded as it is stored, so
    the transcript a client is reading is already threaded. There is no pass
    that has to have run.
  - **A turn is threaded once.** A later rename, merge or reassignment does not
    re-thread it. Which conversation a turn belonged to is a fact about the
    clock and the room; re-deriving it on every relabel would make the same
    transcript thread differently depending on when you looked at it.
  - **The interleaved case is only recoverable once each conversation has a
    rhythm.** Two threads that begin interleaved from their very first turns
    carry no signal separating "C answered A" from "C started talking to
    someone else"; they merge. Once each pair has taken a second turn, every
    later turn sorts correctly — which is the case an evening's transcript is
    actually made of.

- **Co-presence edges**: **not** a stored table. `person.edges` turned out to be
  a group-by over `segments` keyed on `thread_id`, cheap enough that a
  `person_edges` cache would only have been a second thing that could be wrong;
  the sketch above is superseded by covering indexes (`idx_segments_thread`,
  `idx_segments_speaker_thread`), per this document's own rule that the graph is
  an index and never the source of truth.

  An edge means **a shared conversation, not a shared instance** — a public
  lobby has forty people in it and you spoke to two. Its weight is how much the
  *other* person spoke in the threads you shared (people take turns, so the
  intersection of two speech timelines is near zero and says nothing). The
  roster adds a column and never a row: `roster_seconds` is real co-presence
  when both voices carry names the VRChat log wrote down, and `null` — not
  zero — when either does not.

- **Talk-time and cadence per person**: `person.get` totals, indexed.

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
  **Bake-off done (spike/graph_bench, 20 gold cases incl. 9 traps, de/en/mixed,
  4 pinned cores at nice 19): Qwen2.5-3B-Instruct Q4 wins** — 9/9 trap
  rejections (zero invented obligations), 9/9 who and what on everything it
  extracted, 3.3 s/case, 1.9 GB. The 1.5B was fast but gullible (5/9 traps);
  Gemma-2-2B close behind (8/9). Two design facts the bench proved: the schema
  must force a boolean verdict BEFORE any extractable fields exist (plain
  object-or-null grammars bias every model toward extraction — bigger models
  were WORSE until the verdict-first fix), and few-shot examples in the prompt
  are load-bearing. A missed promise costs a shrug; an invented one poisons the
  feature — the 3B fails in the right direction, same philosophy as the
  overlap gate. This works because the task is
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

- **Person page** *(0.6.2)*: the voice, its totals, "people they talk with"
  (each one a door to their own page), and the recent conversations — each of
  which lands in the transcript at that conversation, with the speaker filter
  *cleared*, because the point of opening a thread is to read what everybody
  said. Common topics and open commitments join it at Tiers 2 and 3.
  It is pushed state, not a fifth rail item: you arrive at a person from a
  voice, and Back takes you where you came from.
- **Commitments view**: candidate → confirmed → done/dismissed, due dates from
  time_refs. Confirmation is a human click; the tool never nags on a guess.
- **Thread view in the transcript** *(0.6.2)*: the interleaved lobby untangled —
  a hairline where the conversation changes, naming who is in the new one. It
  only appears where threads exist; rows older than threading render exactly as
  they always did.

## Charter guards

- Nothing inferred ever flows to NX Orbit (DESIGN §9 stands).
- Derived tables are second-class citizens of deletion: purging a person purges
  their nodes, edges, topics-participation, commitments — then `VACUUM`.
- Every derived row carries provenance and is re-derivable from the transcript;
  the graph is an index, never the source of truth.
