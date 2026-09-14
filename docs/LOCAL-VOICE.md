# Local Voice

NX Recall can host Lanalu as a local voice assistant. The desktop app owns the worker and its lifecycle; no extra bridge service or paid API is required. Start and stop it in the **Lanalu** tab.

## Audio modes

- **Vesktop call:** Recall exposes a private incoming speaker and generated-voice microphone. Only the Vesktop process with the configured profile is routed. Normal Discord, desktop defaults and other apps keep their routes. You join and leave Discord calls yourself.
- **This computer:** choose a microphone and normal speaker/headphones. Nothing is rerouted globally. Playback temporarily gates microphone input with a short echo tail so ordinary speakers do not feed the assistant's voice back into itself. This mode is half-duplex; Vesktop supports barge-in.

Vesktop initially uses its saved input until detection completes. Select the Recall virtual microphone explicitly in Vesktop once visible for predictable first-call behavior. Stopping Local Voice restores prior Vesktop routes, including its prior microphone. Other participants' microphone/screen-share echo remains possible; this is not an acoustic echo canceller.

## Typed chat and diagnostics

Open the **Lanalu** tab directly from the navigation rail. Typed requests bypass the wake phrase and voice recognition. Replies appear in the current view as text as well as through the selected audio output. The visible exchange is not saved by this chat view and is cleared when you leave it. In Vesktop mode the local model can stay ready while the call is disconnected; audio replies need a connected call or a selected local output.

The **Debug** button opens a separate window with worker state, component readiness, routing counts and recent diagnostic events. This view excludes conversation text and credentials. A stopped worker retains its last failure so it can be diagnosed.

## Local speech and memory

The worker runs Parakeet 110M speech recognition, Qwen3.5 4B through llama.cpp Vulkan, and Piper Amy synthesis using downloaded local models. Wake phrases (“Lanalu” or “Chat GPT” by default) are recognized locally. Always-listening mode responds to each detected utterance. PCM, transcripts and retrieved records never go to a cloud API; no API credentials are read by this worker.

Lanalu and the optional Recall memory writer use [Qwen3.5 4B](https://huggingface.co/Qwen/Qwen3.5-4B), with [pinned Q4_K_M weights](https://huggingface.co/unsloth/Qwen3.5-4B-GGUF/tree/e87f176479d0855a907a41277aca2f8ee7a09523). Thinking is disabled for direct replies and structured extraction. Lanalu keeps a Vulkan model server ready; Recall’s background writer uses its configured local llama-cli runtime and CPU/GPU budget. The model file is shared, while each process keeps its own conversation state.

Recall memory retrieval uses its existing same-user Unix socket, with bounded semantic/keyword results. Retrieved text is reference material, not executable instructions. Unrelated records should not be treated as evidence for a memory answer. There are no automatic tool actions or account automation.

Speaker names come from Recall's own recent acoustic matches for the exact Vesktop stream. A name is used only if assigned explicitly, non-generic, strongly matched, temporally aligned and unambiguous. “Speaker…” labels, missing scores, overlapping speakers and uncertain matches stay unknown. Similarity is not a calibrated probability. Recognition can lag behind speech processing; a reply may use no name even for an enrolled speaker. Local microphone mode currently leaves the speaker unknown.

## Runtime

Choose **Set up Local Voice** in the Lanalu tab to install the optional Python runtime and missing models. Existing files are reused. A fresh setup downloads about 3 GB of models plus Python packages and needs at least 4 GB free. Setup never starts a conversation. Linux x86-64 with Python 3.11+, FFmpeg, PipeWire-Pulse and Vulkan drivers is the initial supported setup. For a terminal install, run `python voice/setup-local.py` from a checkout or `python ~/.local/lib/nx-recall/voice/setup-local.py` from a package.

Local component locations:

- `~/.local/share/nx-recall/voice/venv` — private Python environment
- `~/.local/share/nx-recall/models/llama-voice/llama-server` — llama.cpp b10950 Vulkan, official release SHA256 `08f03f2b6b0cabac54017fa837c94d2def77de59b69a2de3ad392e79192703ba` for its downloaded archive
- `~/.local/share/nx-recall/models/qwen3.5-4b-q4_k_m.gguf`
- `~/.local/share/nx-recall/models/sherpa-onnx-nemo-parakeet_tdt_transducer_110m-en-36000-int8`
- `~/.local/share/nx-recall/models/voices/en_US-amy-medium.onnx` and `.onnx.json`

The model server binds only a private Unix socket. Worker status and serial-safe recovery journals live under `$XDG_RUNTIME_DIR/nx-recall-voice`. Logs report state and counts, not transcripts or names. Model startup and per-turn latency depend on hardware. Context is bounded and resets when voice restarts.

## Tests

```sh
cd voice
~/.local/share/nx-recall/voice/venv/bin/python -m unittest discover -s tests -v
PYTHONPATH=. ~/.local/share/nx-recall/voice/venv/bin/python tests/smoke_voice.py
```

The smoke test uses real local models with synthetic speech through private virtual buses. It opens no hardware microphone/speaker, joins no call and uploads nothing. GUI tests use the repository's isolated headless Gamescope workflow.

The Qwen3.5 migration was checked with 20 synthetic commitment cases: all produced valid JSON and all nine non-commitment traps were rejected. This is a small regression set, not a general accuracy guarantee. Extracted wording can change language or paraphrase; Recall only accepts a deadline that occurs in the original dialogue.
