<div align="center">

<img src="assets/readme/lanalu-hero.png" width="100%" alt="NX Recall — local, private, searchable. Lanalu, the voice assistant, listens among violet-lit memories in a new dark fantasy scene.">

<br>

# Keep the conversation.

**A memory for your voice chats. A voice for your memory.**

NX Recall turns the conversations you choose to capture into a searchable local archive.
Find the words, reconnect them to the people, and ask **Lanalu** to help you remember.

[![Download 0.18.1](https://img.shields.io/badge/DOWNLOAD-0.18.1-7700FF?style=for-the-badge)](https://github.com/nerdrx/nx-recall/releases/tag/v0.18.1)
[![Linux](https://img.shields.io/badge/LINUX-x86--64-15121D?style=for-the-badge)](#get-started)
[![Local inference](https://img.shields.io/badge/INFERENCE-LOCAL-15121D?style=for-the-badge)](#your-conversations-your-control)

[Install with NX Hub](https://github.com/nerdrx/nx-hub) · [Meet Lanalu](#meet-lanalu) · [Documentation](#go-deeper)

</div>

<br>

## The moment is still there.

A world someone recommended. A promise made at the end of the night.
The conversation you remember having, but cannot quite remember how to find.

Recall keeps the original words close. Search by keyword or meaning, narrow by
person and date, open the surrounding conversation, and replay the audio while
it is still retained. Save a moment when it matters. Come back to it tomorrow.

| Hear it | Find it | Keep it |
| :--- | :--- | :--- |
| **Local transcription** across 25 languages, with desktop captions and speaker labels that leave room for uncertainty. | **Search with context:** words, meaning, speakers, days and places. Read the conversation around a result. | **A useful archive:** saved moments, collections, notes, summaries and commitments, with original turns a click away. |

Semantic search and model-written summaries use optional local models.
Browsing recorded days, saved moments and saved searches does not require them.

## Meet Lanalu

**The character on the left of the artwork is Lanalu — Recall's local voice assistant.**

New in **0.18.0**, Local Voice brings spoken conversation into Recall itself.
Lanalu listens through local speech recognition, prepares a reply with a local
language model, and speaks with a local voice. Recall search can supply relevant
saved conversations so you can ask about what came before.

No paid API. No separate bridge service. Start, stop and configure Local Voice
from the **Lanalu** tab. The worker belongs to the app, continues in the
tray, and stops when you quit Recall. Type a request when you cannot speak;
Lanalu replies on screen as well as through the selected voice output.
Open **Debug** to check models, audio attachment and recent activity.

| In your Vesktop call | At your computer |
| :--- | :--- |
| A dedicated Vesktop profile uses private virtual audio devices. You join the call yourself; Recall routes only the selected instance. **Barge-in** lets incoming speech interrupt a reply. | Choose a microphone and speakers or headphones. Desktop audio defaults stay unchanged. Listening pauses during generated speech and a short echo tail to reduce feedback. |

Choose **wake names** to require “Lanalu” or “Chat GPT” in each request, or
**always listening** to respond to each detected spoken turn. Wake names are
recognized locally after the turn; they do not identify the speaker.
The included Local Voice recognition model and Amy voice use **English**.

Speaker recognition uses Recall's recent, confident acoustic matches for an
assigned name. Generic labels, overlapping voices and uncertain matches stay
unknown. Recognition can arrive after the reply; local microphone mode currently
leaves the speaker unknown. A voice match is not authorization.

**In a call, memory answers are audible to everyone present and may include saved
Recall information.** Vesktop remains a normal, human-controlled Discord client;
Local Voice does not log in, join calls or automate the user account.

[Set up Local Voice and read its limits →](docs/LOCAL-VOICE.md)

<details>
<summary><strong>See Local Voice in the app</strong></summary>

<br>

<img src="assets/readme/local-voice.png" width="100%" alt="NX Recall Lanalu tab with conversation controls, audio selection and listening modes.">

</details>

## Your conversations. Your control.

Capture starts with a choice: Recall records only applications you explicitly
allow, plus microphones you enable. Audio, transcripts, speaker fingerprints
and search data stay on your machine for local processing. There is no cloud
transcription or telemetry.

- **Choose what is heard.** Manage capture sources and pause recording from the app or tray.
- **Keep the evidence.** Review uncertain words, correct transcripts and inspect the original context.
- **Keep the archive yours.** Audio retention, deletion, local Markdown export and verified backups are built in.
- **Know when the network is used.** Installation, model setup and updates download files. Local inference does not send conversations to a cloud API. Vesktop carries generated replies to your call when you enable that mode.

The optional night shift re-reads difficult audio locally. It preserves correction
provenance and applies guarded replacements. Recognition remains imperfect;
the [measurement record](spike/FINDINGS.md) explains tested conditions and tradeoffs.

## Get started

**Linux x86-64 · PipeWire · systemd user session**

Install the signed desktop release through [NX Hub](https://github.com/nerdrx/nx-hub),
or get the package from [GitHub Releases](https://github.com/nerdrx/nx-recall/releases/latest).
The package includes the recording daemon, desktop app, tray and captions overlay.
Updates and uninstall preserve your data and models under `~/.local/share/nx-recall`.

### 1. Prepare recording

Fetch the base speech models and start Recall's recording service:

```bash
~/.local/bin/recalld models fetch
systemctl --user daemon-reload
systemctl --user enable --now nx-recall
```

Choose permitted applications in **Sources**, or use the terminal:

```bash
~/.local/bin/recalld probe
~/.local/bin/recalld allow VRChat.exe
systemctl --user restart nx-recall
```

### 2. Bring Lanalu online

Open **Lanalu → Set up Local Voice**. A fresh setup downloads
about **3 GB** of models plus Python packages; existing files are reused.
Allow at least **4 GB free disk space**. Initial support requires Python 3.11+,
FFmpeg, PipeWire-Pulse and working Vulkan drivers on Linux x86-64.

Setup does **not** begin listening. Choose Vesktop or local audio, pick your
listening mode, then select **Start Local Voice**. Enable **Start with Recall**
only if you want it to launch with the desktop app.

For Vesktop, select Recall's virtual microphone once it appears for predictable
first-call input. Stopping Local Voice restores the prior routes. Remote users'
microphones can still echo; this is not an acoustic echo canceller.

### 3. Add the memory tools you want

```bash
~/.local/bin/recalld models fetch --semantic
~/.local/bin/recalld models fetch --graph
```

These optional sets enable semantic search and model-assisted conversation
summaries. [Full installation and command reference →](docs/REFERENCE.md#install-on-linux)

## Built to be inspected

Recall combines a Rust recording daemon, a local SQLite archive, an Electron
desktop client and an optional Python voice worker. Live recognition, background
repair and local voice have separate responsibilities. The UI exposes measured
processing delays, recording gaps and memory use rather than promising a fixed
latency for every machine.

Desktop captions use a native Wayland layer surface. The OpenXR headset overlay
is **experimental**, behind `--overlay`, and has not been validated in a live
WiVRn session.

## Go deeper

| Start here | What is inside |
| :--- | :--- |
| [Local Voice](docs/LOCAL-VOICE.md) | Setup, audio modes, models, speaker recognition, privacy boundaries and tests |
| [Technical reference](docs/REFERENCE.md) | Complete former README: install commands, architecture, benchmarks and development history |
| [Build from source](docs/REFERENCE.md#build-from-source) | Development commands and model preparation |
| [Desktop UI](docs/UI.md) | Views, shortcuts, accessibility and headless UI validation |
| [Design](docs/DESIGN.md) · [Memory graph](docs/GRAPH.md) | Architecture, data flow and conversation memory |
| [Protocol](docs/PROTOCOL.md) · [Reliability](docs/RELIABILITY.md) | Local interfaces and operational guarantees |
| [Measurements](spike/FINDINGS.md) · [Changelog](CHANGELOG.md) | Evidence, limitations and release history |
| [Captions & overlays](docs/OVERLAY.md) | Desktop surfaces and the experimental headset path |

---

<div align="center">

**Your lobbies. Your people. Your memory.**

[**NX Recall**](https://github.com/nerdrx/nx-recall) · [The NX family](https://github.com/nerdrx/nx-hub)

</div>
