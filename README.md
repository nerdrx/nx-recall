<div align="center">

<img src="assets/readme/banner.svg" width="100%" alt="NX RECALL — total recall for your social life">

<br>

**The always-on conversation memory for VR. Local transcription in 25
languages, persistent speaker identity, search by meaning — on your silicon,
nowhere else.**

<br>

![local](https://img.shields.io/badge/inference-100%25_local-7700FF?style=for-the-badge)
![telemetry](https://img.shields.io/badge/telemetry-none._ever.-0a0714?style=for-the-badge)
![languages](https://img.shields.io/badge/languages-25-7700FF?style=for-the-badge)
![rust](https://img.shields.io/badge/daemon-rust-b7410e?style=for-the-badge)
![tests](https://img.shields.io/badge/tests-714-2ea44f?style=for-the-badge)
![releases](https://img.shields.io/badge/releases-19_in_3_days-7700FF?style=for-the-badge)
![footprint](https://img.shields.io/badge/pipeline-%3C5%25_of_one_core-2ea44f?style=for-the-badge)

<br>

*"wait — what did she say about that world?"*

**Now you know. Forever. And nobody else does.**

</div>

<br>

```console
$ recalld probe
 NODE   MATCH KEY    APPLICATION           PID     CAPTURE
  190   VRChat.exe   VRChat.exe            635606  unknown (default-deny)
  313   Discord      WEBRTC VoiceEngine    4206    unknown (default-deny)
  380   firefox      Firefox               6917    unknown (default-deny)

$ recalld allow VRChat.exe
$ recalld search --smart "her cat spilled a drink on the keyboard"
semantic 0.510  Kira   She said her cat knocked the coffee over the keyboard…
semantic 0.454  Jonas  Sie meinte ihre Katze hat den Kaffee über die Tastatur gekippt…

$ recalld graph commitments
open  Rowan → You   "den Link schicken"   due morgen (resolved: Do 03.09)
```

Nothing is recorded until you say so. Then everything you allow becomes
searchable — by word, by meaning, by speaker, by when — and a 1.9 GB model on
four polite CPU cores quietly writes down who promised what.

## The problem nobody shipped a fix for

You spend your evenings in lobbies where five conversations run at once through
one spatialized stereo mix. You meet someone brilliant, talk for an hour, and
three days later you cannot remember their name, their voice, or the world they
recommended. Every cloud transcription product would happily fix this — by
uploading your friends' voices to someone else's datacenter.

That is not a fix. That is a breach with a subscription fee.

**NX Recall is the other path**: a Rust daemon that captures audio only from
apps you explicitly allow (plus, if you switch it on, your own microphone —
which follows your sessions rather than your room), transcribes everything
locally, recognizes *who* said it with voice fingerprints that never leave your
disk, threads the interleaved lobby back into its separate conversations, and
answers questions you no longer remember the words to. The GPU keeps rendering
your headset. The network cable stays cold.

## The pipeline

<img src="assets/readme/pipeline.svg" width="100%" alt="capture to meaning, one machine, no exits">

The box that earns its keep is the **overlap gate**. Every naive approach
confidently mislabels overlapping speakers about half the time — and confidence
scores *cannot see it happening*. NX Recall would rather write *several voices*
than write the wrong name into your memory. A missed label costs a shrug. A
false one corrupts the voicebank forever. We chose accordingly — and the same
philosophy repeats at every layer: the ASR outputs **nothing** on dense babble
where lesser models invent sentences; the promise extractor was selected for
refusing all nine trap cases, not for finding the most promises; identity is a
ladder where *creating* a voice costs more evidence than labeling one, and
*enrolling* costs more than creating:

```
label (0.35, calibrated on real lobbies — the corpus value over-split 3×)
  < mint  (2 s of speech AND 2 real words — grunts stop becoming people)
    < enroll (0.55 + margin + overlap-clean + 3 s — the bank cannot poison itself)
```

## Numbers we actually measured

The measurement harness came first — 28 experiment scripts in
[`spike/`](spike/FINDINGS.md) — and two of the original design's core claims
died in it before a line of the daemon existed.

| Claim | Measured |
|---|---|
| Transcription | **1.4% WER English, 8.4% German** — one model, no mode switch |
| What the old English-only model made of German | 103% WER — *"The Vision Shaftwise Nooner of Hindus Deemers"* |
| VRChat's voice codec as "quality ceiling" | Debunked — Opus to 8 kbps costs ~0.2 pp WER, ~0.02 cosine |
| Speaker ID from one second of speech | 96% coverage, 2.5% EER |
| A real 20-minute lobby | **9.8% overlapped speech** — the failure regime is rare in the wild |
| Two named friends | cover 38% of all lobby speech; ~95% of their later speech auto-matches |
| Ghost words on silence / noise / music | Zero. |
| Language flips on 1-second German fragments | 12% read as English — hence the conversational prior below |
| The promise model's trap-rejection | 9/9 — banter, suggestions, past tense, hypotheticals, absent third parties |
| Cross-language search, German query → English memory | mean rank 2.7 after the language-hub correction (raw model: rank-32 tail disasters) |
| Full pipeline: VAD, gate, ASR, identity, vectors | under 5% of one CPU core |
| A 1.5-second turn decoded alone vs. inside 3 s of its neighbours | **56.7% WER → 20.4%** (2.5 s turns: 34.3% → 17.1%) — same model, more audio |
| A second decoder disagreeing as a warning light | shaky rows carry **4.2×** the word errors of solid ones in the lab, **2.8×** on this user's own lobby audio against whisper-large-v3 |
| Hotword biasing toward the roster and glossary | +9.1% recall on rare words against a +20% gate; at strength the glossary leaked into unrelated turns (control WER 8% → 29%). Not shipped |

## The graveyard of clever ideas

Everything below was **tried, measured, and killed** — recorded so nobody
respectfully re-implements a corpse.

- **Top1-vs-top2 margin as an overlap detector.** Precision stayed ~50% at
  every margin while coverage bled out. A blended voice isn't *between* two
  speakers — one of them captures it, confidently.
- **Sub-window agreement.** Capture is arbitrary per mix but *stable within
  it*: 90% → 19.5% coverage for +2.7 points of precision.
- **Zero-padding short segments for the overlap model.** Shifts its chunk
  normalisation: a 1.8 s dominant turn read 0.914 overlap padded, 0.000 at its
  real length. The daemon feeds native lengths.
- **Tile-padding instead.** Fabricated periodicity un-flags dense equal babble
  entirely (1.000 → 0.000) — the exact poison case. Both padding schemes are
  pinned by a test that fails if either creeps back.
- **The "better" paraphrase embedding model.** Beat e5 on cross-language
  medians, then ranked *"Ja genau."* first for a keyword query. Search boxes
  get keywords; asymmetric retrieval models exist for a reason.
- **Raw multilingual embeddings.** A bilingual index grows a *language hub*:
  "this is German" outweighs "this is about dentists." Centre + project out the
  top two components — but centring *alone* makes it worse; the two halves are
  one transform.
- **Object-or-null grammars for LLM extraction.** Constrained decoding biases
  every model toward filling the fields that exist — *bigger models
  false-alarmed more* (9/9 traps failed) until the schema forced
  `"is_commitment": true/false` *before* any extractable field existed.
- **Hotword biasing.** The obvious lever — tell the transducer the names in
  the room. Measured: a few points of recall on rare words, only under a beam
  search that costs 1.6 pp of WER before the first hotword, and at useful
  strength the glossary starts appearing in sentences that never contained it.
  The vocabulary is assembled, stored and served anyway; every reply says
  `applied_to_decoder: false` until something can use it without that trade.
- **`COUNT(*)+1` as an id.** Delete two rows and the next two mints collide.
  Numbers come from row ids now, like they always should have.

## Found by using it

This repo's QA department is **one user with strong opinions** and **an
adversarial audit told to find what he would have found next**.

- Day one: eight bugs in the first hours of real use — a signature over the
  wrong bytes, a Launch button into a dead socket, a speakers list frozen at
  connect-time, voices you couldn't hear before naming, ETXTBSY on live-binary
  updates (a hub-engine fix every NX app inherited), unreadable native
  dropdowns, German flips, and Delete-that-didn't.
- Then a 25-finding audit hunting one theme — **operations that appear to
  succeed while doing nothing** — every finding verified in source before its
  fix, every fix shipped with a test confirmed to fail on the old code.
- The meta-lesson, now enforced: the GUI's mock daemon diverged from the real
  one **seven times, and every divergence was a shipped bug**. The mock is a
  conformance twin now, held to the daemon's own expectation tables.
- The morning after enrichment's first full night, the daemon's own telemetry
  confessed three more: the model holding the database lock (audio gaps), and
  conversations split down the middle because your voice lived in its own
  session. Now the mic **bridges** — you are the one voice that exists across
  sessions — so threads contain both halves and promises have someone they are
  owed *to*.

## Sprache, ehrlich

The multilingual model needs no language switch — but on one-second fragments
it *picks wrong and commits* (12% of German shorts read as English). The
defence is layered, each layer measured:

1. **Per-speaker tags** — an English-only friend's German-looking line is
   re-decoded by a model that *cannot produce German*.
2. **The conversational prior** — ten German turns make the eleventh's
   "English" reading a suspected flip and the unreadable mumble German, for
   *any* speaker, no tags needed.
3. **Constrained arbiters** — suspected flips are re-read by a decoder told
   which language to hear (Whisper's language token: its one honest use).
   Guarded: only above 1.5 s (below, measurement said no), only when the
   output actually reads as the target language, captions stripped.
4. **`recalld lang repair`** — the backlog heals retroactively while its audio
   is still inside the retention window.

## Getting it right, then getting it useful

Four models agreed on only half of a real lobby's sentences, and the biggest
error was never the model — it was the **window**. A turn cut at 1.5 s loses
its consonants at both ends, so the daemon now re-reads short turns inside the
audio around them, at idle priority, and keeps only the words inside the turn.
A second, cheaper decoder reads every turn too; where it disagrees the row is
marked **shaky** and muted rather than silently trusted. Fix a transcript in
place and three things move: the row, the **measured error rate** on the
Memory tab, and the vocabulary the next turn is checked against.

Then the parts that make an archive worth having: **one query box** that
understands *"was hat Aspen gestern über den Shader gesagt?"* and shows the
speaker and the day it read as removable pills; **notes to self** — say
*"Recall, merk dir …"* into the microphone and it is filed, in VR, without a
keyboard; and a **brief** when a named friend joins the instance: what they owe
you, what you owe them, what you last talked about.

## What never leaves this machine

| Artifact | Lives | Leaves |
|---|---|---|
| Audio segments (apps, and your mic if you enable it) | your disk, retention-capped (default: days) | never |
| Transcripts, threads, promises, topics | SQLite on your disk | never |
| Voice fingerprints | your voicebank | never |
| Golden enrollment samples | your disk, retention-exempt | never |
| Search vectors | your disk | never |
| Telemetry, analytics, crash reports | nowhere — they do not exist | n/a |

`models fetch` is the only command in the program that opens a network socket:
setup-time, byte-verified against a pinned catalogue, 12-way parallel because
consumer uplinks shape per-connection (measured: 50 kB/s single vs 13 MB/s
ranged). This repo is private by design and the software contains **no export
or sharing surface at all** — not a missing feature, the
[legal architecture](docs/DESIGN.md#12-legal-note).

## The machine room

| | |
|---|---|
| Daemon | Rust, 39k lines — PipeWire capture, four ONNX runtimes, one GGUF via llama.cpp, SQLite WAL, NDJSON socket |
| Client | Electron, 12k lines, zero runtime dependencies, NX Clear in both grounds |
| Tests | **647 Rust + 100 node + 70 headless-compositor steps × 2 themes** |
| Schema | v10, migrated in place from v1 on a live database, every step idempotent |
| Models | pyannote gate 6 MB · Parakeet v3 620 MB · ERes2Net 26 MB · e5 135 MB · Qwen 3B 1.9 GB · Canary cross-checker 154 MB · arbiters on demand — all pinned to exact bytes |
| Updates | the daemon watches its own binary, drains, restarts; the GUI offers one click; a dozen hands-free updates and counting |
| Provenance | every derived row carries its model id, confidence, and how the label arrived: `match · mic · proximity · re-decode · context` |

## Ship log

Installed by its first user on day one; every finding became a release.

| Version | Clock | What |
|---|---|---|
| 0.5.0 | +0h | first light: capture, VAD, gate, ASR, voicebank, GUI, tray |
| 0.5.1–0.5.2 | +2h | Launch self-heals; the speakers list learns about new voices |
| 0.5.3 | +26h | an update should update — the daemon restarts itself |
| 0.5.4 | +27h | you can hear a voice before you are asked to name it |
| 0.5.5 | +28h | update banner, calm rows, honest "several voices" |
| 0.5.6 | +29h | German. And 23 other languages |
| 0.6.0 | +30h | your own voice joins, pre-labelled, following your sessions |
| 0.6.1 | +44h | per-speaker languages, storage panel, grunts stop becoming people |
| 0.6.2 | +46h | the memory graph: threads, person pages, co-presence |
| 0.6.3 | +47h | NX Clear — the lights come on, both grounds |
| 0.6.4 | +48h | deleting a voice deletes the voice |
| 0.7.0 | +50h | the Memory tab: promises and topics, read by a jailed 1.9 GB model |
| 0.7.1 | +51h | six audit findings, fixed by hand |
| 0.7.2 | +52h | enabled means running; the thread knob goes live |
| 0.7.3 | +53h | semantic search — the German query finds the English sentence |
| 0.7.4 | +54h | the transcript becomes the whole archive |
| 0.7.5 | +55h | the audit closes: all 25 findings resolved |
| 0.7.6 | +65h | the night shift's three bugs — the lock, the split conversations |
| 0.7.7 | +67h | the conversational language prior + the German arbiter |
| 0.8.0 | +80h | the accuracy round: short turns re-read in context, a second decoder as a warning light, fix-in-place, one query box, notes to self, briefs |
| 0.8.1 | +83h | the afternoon-after check: one-word rows get no verdict, re-decodes keep the words they replace, the flag re-measured on real audio |
| 0.8.2 | +84h | yesterday stops landing under now: a re-published archive row is history, not an arrival |

## Quickstart

```bash
cargo build --release
./target/release/recalld models fetch          # the speech set; --semantic --graph --arbiter-de for the rest
./target/release/recalld probe                 # see every app making sound — none captured
./target/release/recalld allow VRChat.exe
./target/release/recalld run                   # first light
```

Or install it like a product: it ships through NX Hub as a signed prefix
tarball — daemon, GUI, tray, systemd unit, delta-updatable, exact-manifest
uninstall that leaves your data untouched. Pause lives in the tray: instant,
write-free, and it means it.

## The NX suite

Recall runs alongside nx-hub, nx-orbit, and the rest of the NX family. Orbit
integration is deliberately one-way and manual: Recall may read Orbit's
name-picker once; **nothing ever flows back**. Orbit's charter stays clean.

---

<div align="center">

**Built for one user, at full send.**

*Your lobbies. Your friends. Your memory. Your hardware.*

◢ **NX** ◣

</div>
