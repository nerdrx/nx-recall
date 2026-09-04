# NX Recall — Design v2

**Local-first, always-on conversation transcription with cross-session speaker
identity.** Part of the NX suite · Linux-first · no cloud, no telemetry · private repo.

This supersedes the original brief. Every change from v1 is backed by measurement —
evidence lives in [spike/FINDINGS.md](../spike/FINDINGS.md); this document records
the decisions. Claims the spike disproved are marked ~~struck~~ with their
replacement.

---

## 0. What this is

A background service that captures audio from **explicitly allowed applications**,
transcribes it locally, labels each utterance with a **persistent speaker identity**,
and stores it in a searchable local database. Purpose: personal memory refresh —
"wait, what did she say about that world?" — not surveillance, analytics, or anything
shared outward.

### Hard constraints (unchanged)

- Everything runs locally. No network at inference time. No telemetry. Ever.
- Default-deny capture. Apps are opted in, never out.
- The user is the only subject. No sharing, no export, no cloud sync.
- Deletion means deletion: cascade deletes, real purges, `VACUUM`.

### Non-goals (unchanged)

Real-time VR HUD captions; blind source separation; anything that writes inferred
data into NX Orbit (§9).

---

## 1. The core problem — reframed by measurement

VRChat hands you one stereo mix, already HRTF-spatialised, no per-player streams.
The chosen approach stands: **label, don't separate** — one messy transcript, every
segment tagged by voice-fingerprint identity, conversations threaded after the fact.

What changed is the failure model:

- ~~"Good on 2–3 concurrent speakers, degrading past that"~~ → **talker count is the
  wrong variable.** Coverage tracks the target's signal-to-babble ratio
  (`dominance − 10·log₁₀K`); ten talkers at +12 dB behaves like three at +6 dB.
  The system works above roughly +3 dB SNR and fails below it.
- ~~"VRChat's voice codec is the quality ceiling"~~ → **false for both branches.**
  Opus at voice bitrates costs ~0.02 cosine for identity and ~0.2 pp WER for ASR.
  VRChat audio is good enrollment material; Discord is not required.
- **The dangerous case is equal-loudness overlap**, and it is *undetectable from the
  embedding side*: at 2 equal talkers the matcher stays 92% confident while being
  wrong half the time. Score thresholds, top1−top2 margins, and sub-window agreement
  all fail to see it (measured; do not re-attempt). The only defence is an
  independent overlapped-speech detector (§4).
- **Field-validated:** a real 20-minute lobby measured **9.8% overlapped speech** —
  the failure regime is rare in practice. Two named people covered 38% of all speech.

## 2. Architecture

```
PipeWire app-stream taps (allowlist, default-deny)
        │
   resample 16 kHz mono ── stamped HERE, one monotonic clock, stored UTC ns
        ▼
     Silero VAD ──► turn merge (gap ≤ 1.5 s)
        ▼
   bounded queue (drop-oldest, counted)
        ▼
  overlap detector  (pyannote segmentation-3.0 powerset; overlap_frac per segment)
        ├─► ASR (Parakeet)                 — every segment, even overlapped
        └─► speaker embedding (ERes2Net)   — ONLY overlap_frac ≤ 0.1
                    ▼
             voicebank match (two thresholds, §5)
                    ▼
            SQLite + segment audio files
                    ▼
        Unix socket ── req/resp + pub/sub ──► clients (GUI, CLI, web panel)
```

The overlap detector is a first-class pipeline stage, not an option. It exists
because nothing downstream can detect the poison case (§1). Zero false alarms on
single-talker speech, 98% detection of equal-loudness overlap (measured).

Daemon/client split, Rust daemon + Electron GUI, Unix-socket-only transport,
protocol versioning + resync-on-reconnect: unchanged from v1.

## 3. Capture

**Correction to v1:** PipeWire does *not* give per-app monitor nodes — monitors
belong to sinks and would re-merge every allowed app. The daemon watches the
registry for `Stream/Output/Audio` nodes and taps one app by creating an
input-direction stream with `PW_KEY_TARGET_OBJECT` aimed at that node (the
`pw-record --target` mechanism). This captures the app's clean pre-mix output,
independent of output device. Implemented and verified in Step 1.

- Match key: `application.process.binary` — **except Wine loaders**
  (`wine64-preloader` is shared by every Wine app; VRChat keys on the PE name via
  `application.name` instead, with the real binary still displayed). Implemented.
- Never key on PID. Seen-sources history so silent apps can be pre-denied. Node
  add/remove watched; device unplug degrades, not crashes.
- Clock: stamp at capture with CLOCK_MONOTONIC; store UTC nanoseconds; never stamp
  at transcription time.

### 3a. The microphone (added 0.6.0)

One source is not an application: the user's own default input. Same mechanism —
an input-direction stream — aimed at an `Audio/Source` node instead of a
`Stream/Output/Audio` one, resampled and stamped through the identical callbacks
so a mic turn and a VRChat turn sit on one clock. Three differences, all
deliberate, and **the ordering is the feature**:

- **Off by default, and not an allowlist rule.** An app rule is consent about one
  program's output; a microphone picks up the room, including people who never
  joined the instance. It gets `[mic]`, its own switch, and its own UI copy saying
  so out loud. `sources.set` refuses the `mic` key rather than letting the two
  halves of the config disagree about a consent decision.
- **`follow` is the default mode**: the tap is open exactly while at least one
  allowed application is being captured, so the mic's lifetime is a conversation's
  rather than the daemon's. VRChat opening opens it; the last allowed app closing
  closes it, inside the 250 ms rule-poll tick. `always` is a deliberate second
  choice.
- **It follows the default source** (watched on the `default` metadata object's
  `default.audio.source`) instead of pinning to one node. An app tap that
  reconnected would silently record a different *program*; a mic tap that
  reconnects records the same *person* on a different headset. A device change
  reopens the session, so provenance never spans two microphones. `[mic].device`
  pins one node for machines with several, and an absent pin is an error rather
  than a silent fallback.

Global pause covers it like everything else — enforced in the pipeline, where every
other write is, rather than by tearing down a stream.

## 4. Inference

All ONNX, no torch, models fetched on first run into the data dir (~700 MB default
set). Measured selections:

| Stage | Model | Size | Measured |
|---|---|---|---|
| VAD | Silero | ~1 MB | v4/v5 contract auto-detected |
| Overlap | pyannote segmentation-3.0 | 6 MB | RTF 0.0017 · **MIT** (v1's "gated license" note was wrong — the HF form gates the download, not the license) |
| Embedding | ERes2Net-en (192-d) | 26 MB | 1.38% EER clean; TitaNet-small equals it, so "small model during VR" costs nothing |
| ASR (EN) | Parakeet-TDT 110m int8 | 108 MB | 2.0% WER clean, RTF 0.011 |
| ASR (EN, large) | Parakeet-TDT 0.6b int8 | 480 MB | 1.3% WER, RTF 0.026 |
| ASR (non-EN) | evaluate parakeet-ja / parakeet-v3 **before** Whisper | — | Whisper base: 4× worse WER, hallucinates on all non-speech |
| Memory graph (optional) | Qwen2.5-3B-Instruct Q4 GGUF, via llama.cpp | 1.9 GB | 9/9 trap rejections, 3.3 s/case on 4 pinned cores ([GRAPH.md](GRAPH.md)) |

- The graph model was the **first optional** entry: `models fetch --graph`, off by
  default in `[graph].enabled`, and `models status` lists it under its own
  heading so a machine that never asked for it is not reported as incomplete.
  It is also the only model this daemon does not link — it runs as a child
  `llama-cli`, which keeps a 3B model's failure modes out of the capture process
  and costs the build no C++ toolchain. The semantic embedder and the German
  flip arbiter followed the same pattern.

- **Whisper is back, as an arbiter and nothing else** (0.7.7, `models fetch
  --arbiter-de`, and optional). The table above rejected it as a *transcriber*
  and that stands — 4× the WER and a caption-hallucination habit Parakeet does
  not have. But the multilingual export flips short German to English 12% of the
  time at 1 s (§10) and what it produces then is 103%-WER garbage, so the bar an
  arbiter has to clear is not "beat Parakeet" but "beat a flip". Measured
  (`spike/arbiter_de.py`): Whisper base forced to `language=de` flips 2-3%, comes
  back empty ~0% of the time at 1.5 s and above, and its words are in the
  reference 54% of the time at 1.5 s and 66% at 3 s. It is asked *only* about a
  fragment something else already got wrong, its answer may only replace text
  above 1.5 s (28% precision at 1.0 s — below the floor the row is flagged
  instead), and its output is caption-stripped before anything classifies it.

- ~~Whisper hallucination filtering is mandatory~~ → **Parakeet emits zero ghost
  words on silence/noise/music (measured); the filter is Whisper-scoped.** Which
  is exactly where it now lives: `arbiter::strip_captions` runs on every arbiter
  answer and on nothing else. `(Musik)` classifies as perfectly good German, so
  an unstripped arbiter would "confirm" a flip on a fragment containing no
  speech at all.
- Windowing: embed **contiguous detector-approved single-speaker audio**, as long as
  possible — not fixed short windows (10 s beat 3 s by 27 pp coverage when one
  talker dominates). ASR batches to ~30 s only if a Whisper-family model is in use.
- **Runtime rule — affinity, not model size.** The real risk (demonstrated live on
  the dev box) is priority/placement, not throughput. The daemon sets nice 19 on
  inference threads and supports `[runtime] inference_cpus` to pin off the game's
  CCD (9950X3D: game keeps 0–15/X3D). Both implemented in Step 1.
  - ~~The whole pipeline measures < 5% of one core~~ → **it measures 30 CPU
    seconds per audio minute, half of one core** (0.11.5 config, FINDINGS §40,
    the user's own 22.8 minutes). The old figure was §10's and it was true: it
    measured *Parakeet 110m at one thread*. The daemon ships the 0.6b v3
    multilingual export at four threads. Affinity is still the rule — half a
    core at nice 19 on the non-game CCD is not what stutters a frame — but the
    number should not be quoted as if throughput were free.

| stage | runtime | CPU s / audio min | share of the live path |
|---|---|---:|---:|
| VAD (silero) | `ort` | 0.17 | 0.6% |
| overlap (pyannote-3.0) | `ort` | 0.16 | 0.5% |
| **ASR (parakeet-tdt-0.6b-v3)** | **sherpa-onnx** | **26.76** | **89.5%** |
| embed (eres2net) | sherpa-onnx | 2.81 | 9.4% |

- **Three tiers, and all three are on the CPU** — the live path has no GPU leg
  and will not get one soon (FINDINGS §40, and `crate::device` reports it in
  `recalld status`). Not for want of a card: the night shift runs whisper.cpp on
  this same 7900 XTX through Vulkan. The live path cannot follow it because
  **90% of its cost is Parakeet, and Parakeet runs under sherpa-onnx, whose
  provider enum has no AMD variant at all** — `cuda`, `coreml`, `xnnpack`,
  `nnapi`, `trt`, `directml`, and nothing for AMD. The two models that *do* go
  through `ort`, which has AMD execution providers in its binding, are 1.1% of
  the bill between them, and reaching even those needs ~19 GB of ROCm math
  libraries installed system-wide. Measured, not assumed: a per-turn whisper
  Vulkan decoder was benchmarked against the incumbent and was **both slower to
  answer (1639 ms p50 against 360 ms) and no cheaper in CPU**, because a live
  decoder cannot amortise the model load the way a night batch does. There is
  deliberately **no `live_gpu` setting**: all three of its states would do the
  same thing.
- **Three tiers, not one (0.11.2).** The first night with the night shift, the
  digests, the translator, the cross-check and the truth pass all running, the
  capture thread missed its PipeWire deadlines 658 times in one hour — because
  the unit ran the *whole* daemon at nice 19, so capture queued at the same
  priority as its own homework. The rule is now: the **process** runs at normal
  priority (capture, socket, roster, sweeper — nothing that loads a model); the
  **live inference thread** (VAD, ASR, identity) lowers itself to nice 19 in the
  normal class, so it still transcribes under load; every **background pass**
  lowers itself to nice 19 *and* `SCHED_IDLE`, which yields to anything runnable
  at all, and the child processes it spawns (llama-cli, whisper-cli) inherit that
  class on top of their own explicit nice. Capture costs under one percent of a
  core and must never wait for a paragraph.
- Singing passes VAD (observed at first light): the §8 sustained-non-speech filter
  for world music remains a real requirement.

## 5. Speaker identity

Matching: cosine against multiple prototypes per speaker (max over prototypes),
prototypes capped ~20/speaker, diverse. Unchanged in shape; recalibrated in numbers:

- **Two thresholds, separate, both config:** `label_threshold` ≈ 0.35
  (~~corpus-calibrated 0.45~~ over-splits real speech 3× — thresholds must be
  calibrated on real captured audio, and should be duration-aware: short turns score
  systematically lower), and `enroll_threshold` ≈ 0.55 **plus** margin ≥ 0.06
  **plus** overlap_frac ≤ 0.05 **plus** duration ≥ 3 s.
- **Auto-enrollment is never gated on match score alone.** The score cannot see the
  case where it is confidently wrong (§1). The overlap gate is the independent
  signal; mic loopback and push-to-talk boundaries add more when available.
- **Mic loopback is a free perfect label, and it is provenance, not a match**
  (implemented 0.6.0). A turn off the user's own microphone skips the voicebank
  entirely and carries the pinned "You" speaker with `match_score` NULL — a score
  would claim a comparison that never happened. The *overlap gate still runs*:
  speakers-bleed is real, and a segment whose name is certain can still have audio
  that is not, so `overlap_frac` is stored either way and the UI can distrust the
  audio without distrusting the name. Enrolment keeps the same overlap and duration
  gates the matching leg has, minus the two that are about identifying, and the
  first few qualifying turns (longest, capped) are also written as golden samples.
  That is the payoff: one voice the daemon can be certain about, enrolled for free.
  The pin lives in `settings` so it survives a restart, and it **follows a merge** —
  merging "You" into a named voice re-points it at the target rather than minting a
  second user.
- ~~"ECAPA wants ~3s+"~~ → **1 s suffices for labeling** (96% coverage, 2.5% EER);
  proximity-inheritance for sub-second utterances is a nicety, not a mechanism.
- **The mint bar sits above the label bar** (added 0.6.1). Matching a grunt to a
  voice already in the bank costs nothing and is often right; *minting* a new
  identity from one is how a voicebank fills with rows nobody can ever name, and
  a wrong new identity is permanent in a way a wrong label is not. So a new
  voice additionally needs `mint_min_duration_s` (2.0 s) **and**
  `mint_min_words` (2) — seconds *and* words, because either alone passes things
  that are not speech. Below the bar the segment keeps its transcript and its
  embedding (a later reassignment still has the evidence) and stays speaker-NULL.
  Its corollary is the nicety above, now implemented: an unlabelled fragment
  whose neighbours within 2.5 s carry the *same* confident speaker inherits it,
  with `match_score` NULL and `label_via = "proximity"` so every client shows it
  as uncertain. One side is enough — a session has edges — but two confident
  neighbours that disagree are a handover, and it inherits nothing. **It never
  inherits from an inherited row**: one guess may not become the evidence for
  the next. And `speakers.prune` sweeps up the one-off voices that predate the
  bar, refusing "You" and every named voice.
- **A voice's languages are a standing fact worth storing** (added 0.6.1). One
  turn is 1-3 s of audio and the multilingual export flips language on 12% of
  those (§10 / `spike/lang_flip.py`); a person's languages do not flip at all.
  `speakers.languages` turns an unfixable annoyance into a decidable question:
  a voice pinned to exactly one language gets its transcripts checked against a
  text classifier, and an English-only voice's German-looking transcript is
  decoded again with the English-only export — whose language is a property of
  the model, not a hint. Up to 0.7.6 the other direction was only flagged,
  because the catalogue held no German-constrained decoder; 0.7.7's optional
  arbiter (§4) closes that, and the declaration stays load-bearing in both
  directions: a wrong one now costs transcript quality either way, until it is
  widened.
- **A conversation is a language prior, and it needs nobody's permission**
  (added 0.7.7). The declaration above is the right answer and almost nobody
  gives it; the flip does not wait. What is always there is the thread — so the
  rolling majority language over a conversation's last ten clear turns becomes
  the fallback evidence, on exactly the terms the declaration has. A turn the
  classifier could not read takes the conversation's language (`lang_via =
  "context"`, text untouched); a turn that reads as the *opposite* of a settled
  conversation is a suspected flip and goes to the arbiter, **whoever is
  speaking** — including a voice the bank did not recognise, because a flip is a
  property of the audio. A declaration always wins over a vote, and an inherited
  stamp is never evidence for the next one: one guess may not become the ground
  for another, which is the same rule proximity inheritance obeys. The bar is
  deliberately high enough that a genuinely bilingual room gets no context at
  all (three clear turns, 70% agreement), because in that room there is nothing
  to infer. And because every flip flagged since 0.6.1 still has its audio,
  `recalld lang repair` walks that backlog through the identical guards.
- Turn merging (≤ 1.5 s gaps) before embedding: measured free win.
- Golden samples, model-migration via re-enrollment from goldens,
  `embed_model_id` versioning with cross-model comparison forbidden in code: as v1.
- Voice changers: accepted as separate bank entries, as v1.
- **Onboarding is naming.** Two named voices covered 38% of all speech in the field
  recording, and ~95% of a named person's later speech matches. The GUI's first-run
  flow is "these voices are most of your conversations — who are they?", not
  settings.

## 6. Schema

As v1 (SQLite, WAL, `schema_version`, tombstone merges, soft delete, provenance on
every row), with these additions:

- `segments.overlap_frac` and `segments.match_score` — the correction UI needs to
  know which labels to distrust; both come free from the pipeline.
- v4: `sources.kind` (`"app"` | `"mic"`, backfilled to `"app"`) and `settings`, the
  key/value table that pins the "You" speaker. Golden samples for that speaker live
  under `goldens/`, not `segments/` — the retention sweeper walks the latter, so
  being retention-exempt costs the sweeper no special case at all.
- v5: `speakers.languages` (JSON array; NULL is *any*) and two provenance
  columns on `segments`. Before v5 the only marker was `match_score IS NULL`,
  which conflated a microphone pin, a hand reassignment and a split's softened
  score; `label_via` says which (`match` | `mic` | `manual` | `proximity`) and
  `lang_via` says where the language came from (`model` | `classified` |
  `re-decode` | `mismatch`, plus `context` since 0.7.7). The backfill reads the
  old convention as faithfully
  as it can: every labelled row is `match`, except the pinned voice's scoreless
  ones, which were the microphone.
- v6: `threads(id, session_id, started_ns, ended_ns)` and `segments.thread_id` —
  which conversation a turn belongs to ([GRAPH.md](GRAPH.md) Tier 1). The one
  derived table in the schema, and it earns that by obeying the two rules the
  graph is bound by: it is re-derivable from the transcript alone (the migration
  backfills it by replaying the live rule), and it dies with the rows it indexes
  — purging a segment deletes any thread it emptied. There is deliberately **no**
  `person_edges` table: edges turned out to be a group-by over `segments` keyed
  on `thread_id`, and a cache of something derived is only a second thing that
  can be wrong. Covering indexes instead.
- `session_roster(session_id, display_name, joined_at, left_at)` — the roster is
  load-bearing for candidate pruning and the Orbit name-picker, so it must be stored,
  not just observed live.
- `operations(id, op, target_ids, prior_state, at)` — an audit log making **merge,
  rename, and reassign undoable**, not just deletes. False merges are poison; the
  recovery path deserves first-class support.
- **Merge tombstones never chain:** merge(a→b) re-points every tombstone that
  targeted `a`. Single-hop resolution, enforced at write time.
- Full-text: FTS5 as v1. Semantic search over text embeddings gets **sqlite-vec**
  (or equivalent ANN index) — the segment-embedding table will pass 100k rows and
  linear scan is the product surface. The voicebank stays brute-force (thousands of
  vectors, sub-ms).
- Loose audio files get a **reconciliation sweeper** (orphaned files / dangling
  paths after a crash between DB delete and unlink), alongside the nightly
  soft-delete purge + `VACUUM`.
- **A row with neither words nor a voice ages out on the audio clock** (0.6.1).
  The tiers exist because a transcript is cheap and *is* the memory while audio
  is heavy and is only evidence. A segment with no transcript and no speaker is
  neither: not searchable, not attributable — a door closing, a cough. Keeping
  it in the light tier forever would grow the database with rows that answer no
  question anybody can ask, so it expires with the audio it was evidence of.
- **Disk usage is measured by the sweeper, not by `status`** (0.6.1), in the
  four parts that behave differently: database, audio, goldens, models. Only one
  of them shrinks on its own, and a single total would hide that. The status
  poll runs every three seconds in every open client and must never pay for a
  directory walk, so it reads a cache the sweeper refills once a pass.

## 7. Roster (correction)

~~VRChat OSC instance roster~~ — **the roster does not come over OSC.** It comes
from VRChat's output log (`OnPlayerJoined` / `OnPlayerLeft` / world-change lines,
which also rotate the log file). Prototype validated live: `spike/roster_watch.py`.
The daemon tails the newest log, emits join/leave/world events with UTC-ns
timestamps into `session_roster`, and survives log rotation.

## 8. Manual labeling, retention, deletion

As v1 (retroactive name/merge/split/reassign/correct, broadcast to all clients;
tiered retention by artifact; delete-by-speaker as a first-class cascade with
preview; soft-delete undo window; panic-delete; source blocklist), plus:

- The relabel broadcast carries **sequence numbers**; clients request "events since
  N" or full resync. Long operations (bulk delete, split re-cluster) run as **async
  operation handles with progress events** — a multi-second purge must not block
  every client.
- Global pause stays first-class — instant, zero writes — but ~~"global pause
  hotkey — non-negotiable"~~ is removed per the user: **pause surfaces are the tray
  dropdown and the GUI** (plus `recalld pause` for scripts), and that is the whole
  requirement. No hotkey, no Wayland GlobalShortcuts spike, no in-world trigger.
  For anything said before a pause: panic-delete-last-N-minutes and retention
  limits cover it after the fact.
- **Delete-by-speaker is a choice, and both halves exist (0.6.4).** Deleting a
  voice asks which of two things should go: the conversations, or the
  conversations *and* the bank entry — keep it and the voice is still labelled
  going forward, nuke it and that person re-enrolls from scratch. Until 0.6.4
  only the segments were ever in scope, so every "delete" kept the voiceprint
  and went on matching, and a voice whose rows had already gone could not be
  deleted at all: the button matched zero rows and did nothing, with
  `recalld speakers prune --apply` as the only escape and no way to know it.
  The method is `speakers.delete {id, keep_voiceprint}` (PROTOCOL), the GUI asks
  in a confirm sheet that states the real scope, and the speakers list says
  "no conversations left" on a voice that has none, so an empty entry reads as
  prunable rather than broken. Two things are refused rather than guessed at:
  your own pinned voice (the microphone switch is what stops recording you) and,
  on the nuke path, a voice others were merged into — its tombstones would
  dangle, and keeping the voiceprint still takes every conversation.
- **The memory graph has its own switch, and it is off (0.7.0).** Tier 3 of
  [GRAPH.md](GRAPH.md) is a 3B local model reading conversations nobody has
  looked at yet. It is a second consent question of the same shape as the
  microphone's — the mic asks to hear the room, this asks to read what the room
  said — so it gets the same treatment: its own switch, its own default (off),
  its own copy stating what it costs, and its own card in the app rather than a
  line in a settings list. It never runs while an allowed application is being
  captured, never while capture is paused, and never in the capture path.
- Socket: `$XDG_RUNTIME_DIR/nx-recall.sock`, mode 0600.
- At-rest encryption: **honestly scoped.** A user-service that autostarts cannot
  prompt for a passphrase, so a local key would be theater against filesystem
  access. The model is filesystem permissions (0700 data dir) + the user's own
  full-disk encryption; an optional passphrase mode may exist for users who accept
  manual daemon start. Backups: local story only, documented.

## 9. NX Orbit (unchanged, deliberately)

The link stays manual, one-way, and in *this* database (`speaker_links`). Orbit's
person list may be read once, read-only, to fill a name-picker. Nothing flows into
Orbit. Its charter conflicts with this project's nature; the conflict is resolved by
distance, not integration.

## 10. Packaging & testing

As v1 (nx-hub prefix install, `nx-app.json`, manifest accounting for the systemd
user unit + desktop entry, data dir out-of-manifest and uninstall-surviving, models
fetched on first run), plus:

- **Golden fixtures are committed** (`fixtures/`, 18 WAVs + manifest): clean/codec/
  dominant/equal-loudness/non-speech, each with expected behavior. The equal-loudness
  fixtures carry `expect: refuse` — the overlap gate must decline them; labeling
  them correctly by luck is a regression. Silence/noise must produce zero words.
- Headless gamescope for GUI verification, as v1.
- Failure drills (disk full, device unplug, GPU OOM, stale clients): as v1; the
  Step 1 shutdown-signal bug (PipeWire signalfd requires all threads to block
  SIGINT/SIGTERM) is the canonical example of why these are tested, not assumed.

## 11. Build order — status

| Step | Scope | Status |
|---|---|---|
| 0 | Measurement spike (was not in v1 — added, decisively worth it) | **done** — see FINDINGS |
| 1 | Capture + VAD + allowlist | **done, first light verified** on a live lobby |
| 2 | ASR into the daemon | in progress |
| 3 | Embeddings + voicebank + overlap gate | in progress (with 2) |
| 4 | Socket protocol + GUI + search + relabel | next |
| 5 | Roster integration · Orbit picker · Windows · gamescope e2e | later |

Deferred validation: unbiased identity accuracy and real-speech WER need a
hand-labeled friend-group recording (tooling ready: `spike/label_server.py`,
`spike/truth_report.py`).

## 12. Legal note (unchanged)

User is in Germany and has decided to proceed; §201 StGB and Art. 9 GDPR
considerations recorded in v1. Repo private; no export or sharing features anywhere
in the design — distribution remains the axis that changes the calculus.

**0.10.0 amendment — the local Markdown export.** The sentence above is about
*distribution*, and it stands. What 0.10.0 adds is not a distribution feature and
is defined by that: `export.run` writes Markdown files into **one directory on a
local filesystem that the user chose in a native folder dialog**, and there is no
other destination anywhere in the code path — no upload, no share sheet, no
clipboard, no link, no network target of any kind. The daemon refuses a path that
is not absolute, that does not already exist, that is a volatile runtime directory
(`/run/user`, `/proc`, `/sys`, `/dev`), or that `statfs` identifies as a network
mount, because a network mount is a share whatever the file manager calls it. It
overwrites only files carrying its own `<!-- nx-recall export -->` header and
refuses by name otherwise: the folder belongs to the user, not to this program.

The copy on the card and in the CLI is one sentence, and it is the sentence this
paragraph exists to license: **this writes files to your disk and nothing else.**
Moving those files anywhere afterwards is a person's own act, made deliberately,
with the file manager — which is exactly where that decision belongs, and is a
different thing from a program offering to do it.

**0.10.0 — the room microphone.** A second physical input (`[room]`, source kind
`room`), off by default, requiring an explicit device. It hears the people
physically present, who never consented to an instance because they never joined
one — so its consent surface is deliberately the bluntest in the program, and its
voices are matched and enrolled like any other voice rather than pinned to "You".
