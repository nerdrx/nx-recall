<div align="center">

<img src="assets/readme/banner.svg" width="100%" alt="NX RECALL — total recall for your social life">

<br>

**The always-on conversation memory for VR. Local transcription, persistent
speaker identity, instant search — on your silicon, nowhere else.**

<br>

![local](https://img.shields.io/badge/inference-100%25_local-7700FF?style=for-the-badge)
![telemetry](https://img.shields.io/badge/telemetry-none._ever.-0a0714?style=for-the-badge)
![languages](https://img.shields.io/badge/languages-25-7700FF?style=for-the-badge)
![rust](https://img.shields.io/badge/daemon-rust-b7410e?style=for-the-badge)
![tests](https://img.shields.io/badge/tests-284-2ea44f?style=for-the-badge)
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
$ recalld search "portal"
2026-09-01 20:16  Kira   wait, which [portal] was it — the one behind the bar
                         or the one in the stairwell?

$ recalld search --smart "die Welt mit den Walen"
2026-09-01 20:14  both      Jonas  Die Welt mit den riesigen schwebenden Walen
                                   hieß glaube ich Cetacea.
2026-09-01 20:14  semantic  Kira   That world with the giant floating whales was
                                   called Cetacea, I think.
```

Nothing is recorded until you say so. Then everything you allow becomes
searchable — who said it, when, in which app — seconds after it is said.

`--smart` searches by **meaning** rather than by word, so the German query finds
the English turn it shares no word with, and "what did she say about that world"
works when you cannot remember what she actually said. It needs one optional
model (`recalld models fetch --semantic`, ~135 MB) and `recalld semantic
backfill` to index what was captured before it.

## The problem nobody shipped a fix for

You spend your evenings in lobbies where five conversations run at once through
one spatialized stereo mix. You meet someone brilliant, talk for an hour, and
three days later you cannot remember their name, their voice, or the world they
recommended. Every cloud transcription product would happily fix this — by
uploading your friends' voices to someone else's datacenter.

That is not a fix. That is a breach with a subscription fee.

**NX Recall is the other path**: a Rust daemon that captures audio only from
apps you explicitly allow, transcribes it on your CPU in 25 languages,
recognizes *who* said it with voice fingerprints that never leave your disk,
and hands you a search box over your own social memory. The GPU keeps rendering
your headset. The network cable stays cold.

## The pipeline

<img src="assets/readme/pipeline.svg" width="100%" alt="capture to search, one machine, no exits">

The box that earns its keep is the **overlap gate**. Every naive approach
confidently mislabels overlapping speakers about half the time — and confidence
scores *cannot see it happening* (we measured the cheap defenses; they are
falsified in [FINDINGS.md](spike/FINDINGS.md) so nobody retries them). NX
Recall would rather write *several voices* than write the wrong name into your
memory. A missed label costs a shrug. A false one corrupts the voicebank
forever. We chose accordingly.

## Numbers we actually measured

This project runs on receipts, not vibes. The measurement harness came first;
two of the original design's core claims died in it, and the architecture is
what survived.

| Claim | Measured |
|---|---|
| Transcription, English | **1.4% WER** — and German **8.4%**, same model, no mode switch |
| What the old English-only model made of German | 103% WER — "The Vision Shaftwise Nooner of Hindus Deemers" |
| VRChat's voice codec as "quality ceiling" | Debunked — Opus to 8 kbps costs ~0.2 pp WER, ~0.02 cosine |
| Speaker ID from one second of speech | 96% coverage, 2.5% EER |
| A real 20-minute lobby, measured | 9.8% overlapped speech — the failure regime is rare in the wild |
| Two named friends | cover 38% of all lobby speech; ~95% of their later speech auto-matches |
| Ghost words on silence / noise / music | Zero. (Whisper hallucinated on all three; it is not in the default stack) |
| Full pipeline: VAD, gate, ASR, identity | under 5% of one CPU core — your framerate never hears it |

## Built like it means it

- **Default-deny capture.** Unknown apps trigger a history entry, never a
  recording. VRChat in, banking tab out, forever.
- **Label, don't separate.** Voice timbre is invariant to exactly what VR
  scrambles — position, distance, head movement. Fingerprint, tag, thread the
  conversations afterwards.
- **Two-threshold identity.** Labeling and enrollment are separate gates;
  auto-enrollment additionally requires the overlap detector's blessing. The
  voicebank cannot poison itself on a confident mistake.
- **Updates update.** The hub replaces the running binary; the daemon notices
  its own executable was swapped, drains, and restarts onto the new version;
  the GUI offers one-click restart. Proven in the journal, three releases in a
  row.
- **Deletion means deletion.** Delete-by-speaker with a preview, soft-delete
  undo window, nightly purge, `VACUUM`, and a sweeper that reconciles loose
  audio against the database.
- **Everything has provenance.** Every segment carries its model ids,
  confidence, and overlap fraction. Merge tombstones never chain. Renames are
  retroactive and broadcast live to every client.

## What never leaves this machine

| Artifact | Lives | Leaves |
|---|---|---|
| Audio segments (apps, and your mic if you turn it on) | your disk, retention-capped (default: days) | never |
| Transcripts | SQLite on your disk | never |
| Voice fingerprints | your voicebank | never |
| Golden enrollment samples | your disk, retention-exempt | never |
| Telemetry, analytics, crash reports | nowhere — they do not exist | n/a |

`models fetch` is the only command in the program that opens a network socket:
once, at setup, byte-verified. After that the feature is done having opinions
about the internet. This repo is private by design and the software contains
**no export or sharing surface at all** — not a missing feature, the
[legal architecture](docs/DESIGN.md#12-legal-note).

## Ship log

Installed by its first user on day one; every finding became a release.

| Version | Clock | What |
|---|---|---|
| 0.5.0 | install +0h | first light: capture, VAD, gate, ASR, voicebank, GUI, tray |
| 0.5.1 | +1h | hub Launch self-heals the daemon |
| 0.5.2 | +2h | speakers view learns about voices minted mid-session |
| 0.5.3 | +26h | an update should update: the daemon restarts itself onto new binaries |
| 0.5.4 | +27h | you can hear a voice before you are asked to name it |
| 0.5.5 | +28h | usability: update banner, calm rows, honest "several voices" labels |
| 0.5.6 | +29h | German. And 23 other languages. The default ASR goes multilingual |
| 0.6.0 | +30h | your own voice joins the transcript, pre-labelled — mic as a source |
| 0.6.1 | +31h | voices get languages, grunts stop becoming people, disk stops being a mystery |
| 0.6.2 | +32h | the lobby untangles: conversation threads, who you talk with, the person page |
| 0.6.3 | +33h | the lights come on: the whole UI moves to NX Clear, in both grounds |
| 0.6.4 | +34h | Delete stops lying: deleting a voice asks whether the voiceprint goes too, and an empty voice can finally be deleted at all |
| 0.7.0 | +36h | the memory graph gets a tab: dates and promises by rule, a 3B local model that reads them properly when the machine is idle, and a Memory view that never nags |
| 0.7.2 | +37h | the local model stops waiting for an idle machine — it reads while you play, jailed to as many cores as you hand it, and how many is now a setting |

Golden fixtures gate every release: the equal-loudness mixes carry
`expect: refuse` — a pipeline that labels them correctly *by luck* fails the
suite. Silence and noise must produce zero words, always.

| Milestone | State |
|---|---|
| 0 · Measurement spike — identity, ASR, field recording | `[ SHIPPED ]` |
| 1 · Capture · VAD · allowlist | `[ SHIPPED ]` first light on a live 40-player lobby |
| 2+3 · ASR · voicebank · overlap gate | `[ SHIPPED ]` |
| 4 · Socket · tray · GUI · pause · roster · split | `[ SHIPPED ]` 284 tests, real-daemon interop |
| NX Hub packaging · signed releases · delta-updated hub | `[ SHIPPED ]` |
| Mic as a source — off by default, follows the allowed apps, enrols itself | `[ SHIPPED ]` |
| Per-speaker languages · storage panel | `[ SHIPPED ]` |
| Memory graph Tier 1 — conversation threads · co-presence · the person page | `[ SHIPPED ]` 390 cargo · 32 node · 42 e2e |
| Delete-by-speaker as the choice DESIGN §8 always specified — the voiceprint goes or stays | `[ SHIPPED ]` 397 cargo · 34 node · 45 e2e |
| Memory graph Tiers 2–3 — time refs · topics · commitments · tiny local LLM | `[ MAPPED ]` [docs/GRAPH.md](docs/GRAPH.md) |
| Windows backend | `[ MAPPED ]` |

## Quickstart

```bash
cargo build --release
./target/release/recalld models fetch     # once; the only network use in the program
./target/release/recalld probe            # see every app making sound — none captured
./target/release/recalld allow VRChat.exe
./target/release/recalld run              # first light
```

Your own microphone is the one source that is not an application, so it has its
own switch and it is **off** until you say otherwise:

```bash
./target/release/recalld mic              # what it is doing right now
./target/release/recalld mic on           # follow mode: only while an allowed app is captured
./target/release/recalld mic always       # whenever the daemon is running
./target/release/recalld mic off
```

It hears the *room*, not the game — anyone near you is recorded, whether or not
they are in the instance. In return it is the one voice the daemon never has to
guess at: your turns are labelled from where the audio came, they enrol
themselves, and you are never asked who you are.

Or install it like a product: it ships through NX Hub as a signed prefix
tarball — daemon, GUI, tray, systemd unit, and an exact-manifest uninstall that
leaves your data untouched. Pause lives in the tray dropdown and the GUI,
instant and write-free. `packaging/build-release.sh` builds the artifact and
prints the release command; it never publishes on its own.

## The NX suite

Recall runs alongside nx-hub, nx-orbit, and the rest of the NX family — liquid
glass on deep space, `#7700FF`, local-first to the bone. Orbit integration is
deliberately one-way and manual: Recall may read Orbit's name-picker once;
nothing ever flows back. Orbit's charter stays clean.

---

<div align="center">

**Built for one user, at full send.**

*Your lobbies. Your friends. Your memory. Your hardware.*

◢ **NX** ◣

</div>
