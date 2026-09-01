# NX Recall — the memory graph

**people ↔ topics ↔ promises ↔ dates — locally, or not at all.**

Status: **all three tiers shipped.** Tier 1 in 0.6.2 (conversation threads,
co-presence edges, the person page); Tiers 2 and 3 in 0.7.0, together with the
**Memory** view that surfaces them. Everything here obeys the hard constraints
in [DESIGN.md](DESIGN.md) §0: no network at inference time, no torch, the user
is the only subject, deletion cascades.

Where it lives, for anybody reading the code rather than the argument:

| Tier | Module | Runs |
|---|---|---|
| 1 · threads | `crates/recalld/src/threads.rs` | on the capture path, per turn |
| 2 · time references | `crates/recalld/src/timeref.rs` | on the capture path, per turn |
| 2 · promise candidates | `crates/recalld/src/commitment.rs` | on the capture path, per turn |
| 3 · the local model | `crates/recalld/src/llm.rs` | a child process, on demand |
| 3 · the idle pass | `crates/recalld/src/enrich.rs` | a background thread, gated |

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

### Tier 2 — extracted, rule-based, marked as such — **shipped in 0.7.0**

Deterministic extractors; wrong sometimes, cheap always, provenance on every
row (`extractor`, `version`, `confidence`). Both of them run **on the capture
path**, immediately after threading, for the same reason threading does: a
handful of regex scans over one line costs nothing, and there is no pass that
has to have run before a client can read the annotations.

- **Time references**: de+en rule parser ("Freitag", "morgen Abend", "next
  week", "am dritten") → `time_refs(segment_id, resolved_utc, raw)`. Resolution
  is relative to the segment's capture time — that is the whole trick.

  Three decisions worth keeping in one place:

  - **A bare weekday is always in the future.** "Freitag" said on a Friday is
    the Friday a week away. Nothing in the words tells "come on Friday" from
    "as I said on Friday", and the feature is about what is still owed.
  - **A day reference carries no hour.** It resolves to local midnight and says
    so through its `kind`, so a surface renders a date rather than inventing a
    time. A clock reading later in the same sentence anchors to it: "Freitag um
    18 Uhr" is one date, said twice.
  - **Local time is consulted, never stored.** A person who says "Freitag"
    means Friday where they are sitting, so the offset is read for the instant
    the words were captured and the answer is written back as UTC nanoseconds
    like everything else in the schema.

- **Promise candidates**: modal-pattern lattice ("I'll …", "ich schick dir …",
  "mach ich bis …") + a person + an optional time_ref in range →
  `commitments(who, to_whom, what_segment, due?, state=candidate)`. Expected
  recall is modest; that is fine — candidates are suggestions, never actions.

  Three conditions, and each one earns its place: a modal pattern, a **known
  voice** (a promise with no promiser is not one), and a **counterparty in the
  conversation** (an obligation needs somebody to be owed to — and this is the
  filter that kills most of "I'll probably log off soon" without understanding
  it). Cheap negatives on top: a question is not a promise, a hedge is not a
  promise, and something already done is not owed.

  What it cannot do is tell a promise from in-game banter — "I will kill you
  next round" files a candidate, and there is a test that says so out loud.
  That is exactly the tier boundary: Tier 3 is what upgrades a guess into a
  claim, **or retracts it**.

- ~~**Topics** by sentence-embedding clustering + sqlite-vec~~ → **superseded.**
  Once Tier 3 was measured, a second 120 MB model and an ANN index to produce
  labels made of *top terms* stopped being worth it: the local model already
  reads the conversation and writes one short label in the language it was held
  in, which is what "topics that read like a human wrote them" meant. A topic is
  now a string on `threads`, not a table — it dies with the conversation, and a
  `topics` table would only be a second thing that could disagree with
  `threads`. The semantic-search half of that idea is untouched and still owed.

### Tier 3 — understood, tiny local LLM, opt-in and idle-only — **shipped in 0.7.0**

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
  everything else, byte-verified. **It is an optional asset group** —
  `models fetch --graph`, ~1.95 GB, and `models status` lists it under its own
  "OPTIONAL" heading so a machine that never asked for it is not reported as
  incomplete.
- **A child process, not a linked library.** `crate::llm` shells out to
  `llama-cli` exactly as the bench harness did, so Tier 3 costs this crate no
  cmake, no C++ toolchain and no new link-time anything — and a wedged model is
  killed and forgotten where a wedged thread would be a lost evening.
- Runs ONLY as an idle-time enrichment pass: never while a game runs (same
  detection §4 uses), never in the capture path, budgeted and interruptible.
  The gates, all five re-checked **between conversations** so the worker stands
  down within one model call of anything changing:

  1. `[graph].enabled` — off by default, and the switch is live.
  2. The model is on disk. Not having it is a normal state, reported as
     `unavailable`, not an error.
  3. Capture is not paused. Pause means nothing is written down, and that has
     to include derived rows or the sentence the panic button rests on is false.
  4. **No allowed application has an open stream.** That is §4's own detection,
     reused: a captured app producing audio *is* the game running. The
     microphone does not count — it is a device, and it is open exactly when
     there is most to enrich later.
  5. The capture queue is short. A turn storm means the machine is busy being a
     tape recorder, which is the job that matters.

- Output is annotations referencing segments (`time_refs`, `commitments`,
  `threads.topic`), never modifications of the transcript. Deleting a
  segment/speaker cascades through every derived row (§0 deletion rule), and
  `Store::clear_derived` throws the whole graph away without touching a word of
  what was said — which is the proof that it is an index and not the source.
- **The refusal is the feature.** When the model reads a window and says there
  is nothing there, every rule candidate in that window that a person has not
  touched is deleted. Letting the 3B *retract* a pattern match is worth more
  than letting it add one, and it is what the 9/9 trap result buys.
- One row per segment, so the model **upgrades a rule candidate in place**
  rather than filing a second opinion next to it. Precedence, in one place:
  a person's decision is final, the model outranks the rules, the rules never
  outrank the model.
- Off by default. Its switch sits next to the mic's, with equally plain copy.

## Surfaces

- **Person page** *(0.6.2)*: the voice, its totals, "people they talk with"
  (each one a door to their own page), and the recent conversations — each of
  which lands in the transcript at that conversation, with the speaker filter
  *cleared*, because the point of opening a thread is to read what everybody
  said. Common topics and open commitments join it at Tiers 2 and 3.
  It is pushed state, not a fifth rail item: you arrive at a person from a
  voice, and Back takes you where you came from.
- **The Memory view** *(0.7.0)*: the graph's own place in the app, and the
  fifth rail item — a rail item rather than pushed state, because unlike the
  person page it is not a detail of anything. You do not arrive here from a
  voice; you come here wondering what you were supposed to do by Friday. Three
  sections: open commitments, topics, and the enrichment switch.

  Three rules run through the whole view:

  - **Nothing auto-acts.** No reminder, no notification, nothing that nags. The
    single number the app volunteers is the rail badge, and it counts what is
    still *open* rather than what has been noticed.
  - **A guess looks like a guess.** Every row wears its source in the row
    itself, not in a tooltip: `pattern match` in a dashed outline, `local model`
    in the accent. They are not the same claim and must not read as one. The
    transcript line the claim came from sits under it, one click from the
    transcript, so a person can disagree without leaving the view.
  - **The copy is honest about the cost.** The enrichment card states the model
    size, the core count, when it runs, that it is off by default, and that
    nothing leaves the machine — five flat statements next to the switch, each
    one something somebody might object to.

- **Commitments view**: candidate → confirmed → done/dismissed, due dates from
  time_refs. Confirmation is a human click; the tool never nags on a guess.
  Undated rows sort **last**, never first: a promise with no date is not
  overdue, it is merely open, and putting it above a real deadline would make
  the list lie about urgency. Settled rows are dimmed and kept, not hidden —
  what you dismissed is part of the answer to "what did the graph think".
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
