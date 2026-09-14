# Local Voice

NX Recall can host Lanalu as a local voice assistant. The desktop app owns the worker and its lifecycle; no extra bridge service or paid API is required. Start and stop it in the **Lanalu** tab.

## See what Lanalu understood

The **What Lanalu heard** card in her tab shows the latest six completed voice recognitions, newest first. Each entry shows the words, recognition source, time and whether the turn passed the wake-name check. Missed wake names are shown even when no reply follows. “Passed to Lanalu” means the turn was accepted for processing, not that a reply succeeded or that its meaning was understood correctly. Words appear after a spoken turn is recognized, not word by word while you speak.

Use **Correct words** on a heard turn to save the intended wording. The original recognition and wake decision remain visible. Corrections are saved as labeled examples in Recall (up to 200 recent examples); trusted corrected names can help future recognition when corrected-name assistance is enabled. A correction does not send a new reply or retrain model weights.

This small recent-turn view lives only in the voice worker's memory and clears when Local Voice stops. It does not put recognized text into Debug or diagnostic files. Recall's normal archive still follows its separately configured capture and retention settings. Typed messages appear in the written conversation instead.

## Which virtual device goes where?

In the selected voice client, use **NX Recall - Call audio to Lanalu** as its output and **NX Recall - Lanalu microphone** as its input. Recall attaches these routes automatically while Virtual in/out is active.

A second output called **NX Recall - Internal voice bus** is the local speech engine's feed into the virtual microphone. It is not another listening device. Do not select it as the call's output: that would feed the call back into its microphone. Older installations call these devices `NX_Recall_Voice_Incoming`, `NX_Recall_Voice_Microphone`, and `NX_Recall_Voice_Output` respectively.

The input meter in Lanalu's tab measures sound actually arriving at her capture stream. A connected route with a flat meter can still mean a silent, muted or deafened call. The reply meter measures generated audio sent to the local speech bus; it cannot confirm that a remote participant heard it. Virtual mode hears incoming call audio, not your physical microphone directly. To speak to Lanalu through the call, your human-controlled account must transmit to the account running the selected client.

## Audio modes

- **Virtual in/out:** Recall exposes a private incoming speaker and generated-voice microphone. Only the configured voice-client profile is routed. Normal Discord, desktop defaults and other apps keep their routes. You join and leave Discord calls yourself.
- **This computer:** choose a microphone and normal speaker/headphones. Nothing is rerouted globally. Playback temporarily gates microphone input with a short echo tail so ordinary speakers do not feed the assistant's voice back into itself. This mode is half-duplex; Virtual in/out supports barge-in.

Your voice client initially uses its saved input until detection completes. Select the Recall virtual microphone explicitly in the voice client once visible for predictable first-call behavior. Stopping Local Voice restores the client’s prior routes, including its prior microphone. Other participants' microphone/screen-share echo remains possible; this is not an acoustic echo canceller.

## Typed chat and diagnostics

Open the **Lanalu** tab directly from the navigation rail. Typed requests bypass the wake phrase and voice recognition. Replies appear in the current view as text as well as through the selected audio output. The visible exchange is not saved by this chat view and is cleared when you leave it. In Virtual in/out mode the local model can stay ready while the call is disconnected; audio replies need a connected call or a selected local output.

The **Debug** button opens a separate window with worker state, component readiness, routing counts and recent diagnostic events. This view excludes conversation text and credentials. A stopped worker retains its last failure so it can be diagnosed.

## Voice sound

In **Lanalu → Voice**, choose Amy or Kokoro, then adjust speaking speed (0.6–1.5×). Amy also offers a voice-variation control. This changes synthesis variation, not a named emotion or a trained personality. Stop Local Voice before changing settings, save, then start it again.

Kokoro offers Heart, Bella, Sarah and Nicole. Its optional setup downloads about 350 MB and installs about 401 MB; it reuses the existing local speech runtime. Allow 5 GB free for a fresh setup including Kokoro. Amy remains the small, fast default. On the tested machine, an 8.9-second Amy sample took 0.21 seconds to synthesize warm; a 6.6-second Heart sample took about 0.65 seconds with six synthesis threads. Its first sentence was available in about 0.14 seconds. Playback consumes native sentence chunks as they arrive instead of waiting for the entire reply. These synthesis timings exclude speech recognition and reply generation. Voice quality is subjective; compare the voices at a normal speed before choosing.

All voice synthesis stays local. Changing voice does not clone or enroll a person's voice. Virtual in/out sends the selected generated voice to the connected client; ordinary local mode uses the selected output device.

## Local speech and memory

By default, Lanalu reuses **Recall recognition**: it reads the words Recall has already recognized for the same live input and utterance. It does not run a second speech decoder. Choose **Separate recognition** to use the optional local Parakeet 110M decoder instead. Both modes use Qwen3.5 4B through llama.cpp Vulkan and a selected local synthesis model. Amy uses Piper; optional Heart, Bella, Sarah and Nicole voices use Kokoro. Wake phrases (“Lanalu” or “Chat GPT” by default) are recognized locally. When Lanalu is a configured wake name, its split and phonetic forms “La nalu”, “Lana Lu”, “Lana Lou” and “Lana Loo” are accepted too; unrelated words are not fuzzy-matched. Always-listening mode responds to each detected utterance. PCM, transcripts and retrieved records never go to a cloud API; no API credentials are read by this worker.

Shared recognition waits up to eight seconds for a complete, stable transcript match. It accepts only the exact source and live utterance time range, including a small allowance for Recall's normal 200 ms speech padding. Split transcript segments are joined in time order; a segment is not reused for a later question. Wider merged turns, stale text, unrelated sources and ambiguous matches are rejected rather than guessed. This conservative matching can skip an utterance when Recall's segmentation differs substantially from Lanalu's local activity detector.

For a local microphone, Recall's enabled, active microphone must resolve to the same device Lanalu is capturing. For Virtual in/out, Lanalu verifies actual PipeWire links from its selected profile to both its private input and Recall's recording tee; the binding must remain unchanged from speech onset through transcript matching. Another client instance cannot supply the words. Shared recognition needs Recall recording enabled for that input and enough time for its normal decoder to commit the turn. There is no silent fallback to a second decoder.

The inline state reports **Waiting for Recall recognition**, then either continues to a reply or reports why recognition is unavailable. Diagnostic codes distinguish paused capture, missing capture, an input mismatch, ambiguous sources, stale intervals and timeout; diagnostics contain no words. Fix the input/capture setting or explicitly select separate recognition to retry. Typed chat remains usable without a matching audio transcript. Recall's existing recording and retention rules continue to govern the reused transcript.

Lanalu and the optional Recall memory writer use [Qwen3.5 4B](https://huggingface.co/Qwen/Qwen3.5-4B), with [pinned Q4_K_M weights](https://huggingface.co/unsloth/Qwen3.5-4B-GGUF/tree/e87f176479d0855a907a41277aca2f8ee7a09523). Thinking is disabled for direct replies and structured extraction. Lanalu keeps a Vulkan model server ready; Recall’s background writer uses its configured local llama-cli runtime and CPU/GPU budget. The model file is shared, while each process keeps its own conversation state.

Recall memory retrieval uses its existing same-user Unix socket, with bounded semantic/keyword results. Retrieved text is reference material, not executable instructions. Unrelated records should not be treated as evidence for a memory answer. There are no automatic tool actions or account automation.

Speaker names come from Recall's own recent acoustic matches for the selected virtual input stream. A name is used only if assigned explicitly, non-generic, strongly matched, temporally aligned and unambiguous. “Speaker…” labels, missing scores, overlapping speakers and uncertain matches stay unknown. Similarity is not a calibrated probability. Recognition can lag behind speech processing; a reply may use no name even for an enrolled speaker. Local microphone mode currently leaves the speaker unknown.

## Keep confirmed microphone copies once

In **Sources**, enable **Skip confirmed microphone copies** for an application that also carries your microphone voice. This is off by default and applies only to that application. When its audio closely matches a recently committed microphone turn, Recall keeps the microphone recording and words, plus a small source/time observation. Shared recognition can still use those words for the application without adding another archive entry or WAV. Existing recordings are not deleted.

Only near-identical audio copies are skipped. The filter checks timing, coverage and the residual after matching volume; matching words alone are not enough. Codec/effect changes, extra speech, capture gaps and uncertain matches are kept. In tests, network-codec versions of the same speech were retained, so this does not yet reliably remove Discord round-trip copies. Application turns that finish before the microphone transcript is committed also stay. The check uses a bounded recent-audio cache and adds no waiting period.

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
