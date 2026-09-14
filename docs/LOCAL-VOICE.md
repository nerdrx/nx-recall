# Local Voice

NX Recall can host Lanalu as a local voice assistant. The desktop app owns the worker and its lifecycle; no extra bridge service or paid API is required. Start and stop it in **Settings → Local Voice**.

## Audio modes

- **Vesktop call:** Recall exposes a private incoming speaker and generated-voice microphone. Only the Vesktop process with the configured profile is routed. Normal Discord, desktop defaults and other apps keep their routes. You join and leave Discord calls yourself.
- **This computer:** choose a microphone and normal speaker/headphones. Nothing is rerouted globally. Playback temporarily gates microphone input with a short echo tail so ordinary speakers do not feed the assistant's voice back into itself. This mode is half-duplex; Vesktop supports barge-in.

Vesktop initially uses its saved input until detection completes. Select the Recall virtual microphone explicitly in Vesktop once visible for predictable first-call behavior. Stopping Local Voice restores prior Vesktop routes, including its prior microphone. Other participants' microphone/screen-share echo remains possible; this is not an acoustic echo canceller.

## Local speech and memory

The worker runs Parakeet 110M speech recognition, Qwen2.5 3B through llama.cpp Vulkan, and Piper Amy synthesis using models already on this machine. Wake phrases (“Lanalu” or “Chat GPT” by default) are recognized locally. Always-listening mode responds to each detected utterance. PCM, transcripts and retrieved records never go to a cloud API; no API credentials are read by this worker.

Recall memory retrieval uses its existing same-user Unix socket, with bounded semantic/keyword results. Retrieved text is reference material, not executable instructions. Unrelated records should not be treated as evidence for a memory answer. There are no automatic tool actions or account automation.

Speaker names come from Recall's own recent acoustic matches for the exact Vesktop stream. A name is used only if assigned explicitly, non-generic, strongly matched, temporally aligned and unambiguous. “Speaker…” labels, missing scores, overlapping speakers and uncertain matches stay unknown. Similarity is not a calibrated probability. Recognition can lag behind speech processing; a reply may use no name even for an enrolled speaker. Local microphone mode currently leaves the speaker unknown.

## Runtime

Choose **Set up Local Voice** in Settings to install the optional Python runtime and missing models. Existing files are reused. A fresh setup downloads about 2.1 GB of models plus Python packages and needs at least 4 GB free. Setup never starts a conversation. Linux x86-64 with Python 3.11+, FFmpeg, PipeWire-Pulse and Vulkan drivers is the initial supported setup. For a terminal install, run `python voice/setup-local.py` from a checkout or `python ~/.local/lib/nx-recall/voice/setup-local.py` from a package.

Current machine resources:

- `~/.local/share/nx-recall/voice/venv` — private Python environment
- `~/.local/share/nx-recall/models/llama-voice/llama-server` — llama.cpp b10950 Vulkan, official release SHA256 `08f03f2b6b0cabac54017fa837c94d2def77de59b69a2de3ad392e79192703ba` for its downloaded archive
- `~/.local/share/nx-recall/models/qwen2.5-3b-instruct-q4_k_m.gguf`
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
