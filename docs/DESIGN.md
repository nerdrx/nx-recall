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

- ~~Whisper hallucination filtering is mandatory~~ → **Parakeet emits zero ghost
  words on silence/noise/music (measured); the filter is Whisper-scoped.** If
  Whisper ships at all it needs `no_speech_prob` + bracket-caption stripping.
- Windowing: embed **contiguous detector-approved single-speaker audio**, as long as
  possible — not fixed short windows (10 s beat 3 s by 27 pp coverage when one
  talker dominates). ASR batches to ~30 s only if a Whisper-family model is in use.
- **Runtime rule — affinity, not model size.** The whole pipeline measures < 5% of
  one core, so throughput is a non-issue; the real risk (demonstrated live on the
  dev box) is priority/placement. The daemon sets nice 19 on inference threads and
  supports `[runtime] inference_cpus` to pin off the game's CCD (9950X3D: game keeps
  0–15/X3D). Both implemented in Step 1.
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
- ~~"ECAPA wants ~3s+"~~ → **1 s suffices for labeling** (96% coverage, 2.5% EER);
  proximity-inheritance for sub-second utterances is a nicety, not a mechanism.
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
  hotkey — non-negotiable"~~ is relaxed per the user: the *pause primitive* is
  required, a hotkey binding is not. Reachable via `recalld pause` (CLI), the GUI,
  and the tray tile; a Wayland GlobalShortcuts binding is an optional nicety, off
  the critical path, and the fullscreen-VRChat portal spike is no longer a blocker.
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
