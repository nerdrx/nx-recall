"""Offline speech recognition and synthesis using existing local model files.

Inference never opens a network connection. FFmpeg resamples in memory; no
conversation audio or transcript is stored. Models load lazily and remain hot.
"""
import asyncio
import json
import math
from pathlib import Path
import subprocess
import threading

import numpy as np

DEFAULT_STT = Path.home() / ".local/share/nx-recall/models/sherpa-onnx-nemo-parakeet_tdt_transducer_110m-en-36000-int8"
DEFAULT_TTS = Path.home() / ".local/share/nx-recall/models/voices/en_US-amy-medium.onnx"
DEFAULT_KOKORO = DEFAULT_TTS.parent / "kokoro-multi-lang-v1_0"
# IDs from the publisher's v1.0 voice map; never infer IDs across model versions.
KOKORO_VOICES = {"af_heart": 3, "af_bella": 2, "af_sarah": 9, "af_nicole": 6}



def resample(pcm: bytes, source_rate: int, target_rate: int) -> bytes:
    if len(pcm) % 2:
        raise ValueError("PCM16 contains an incomplete sample")
    if not pcm or source_rate == target_rate:
        return pcm
    result = subprocess.run(
        ["ffmpeg", "-nostdin", "-hide_banner", "-loglevel", "error", "-threads", "1",
         "-f", "s16le", "-ar", str(source_rate), "-ac", "1", "-i", "pipe:0",
         "-f", "s16le", "-ar", str(target_rate), "-ac", "1", "pipe:1"],
        input=pcm, capture_output=True, timeout=20,
    )
    if result.returncode:
        raise RuntimeError("Offline audio resampling failed")
    return result.stdout


class Speech:
    def __init__(self, config=None):
        config = config or {}
        self.stt_path = Path(config.get("stt_model_dir", DEFAULT_STT)).expanduser()
        self.tts_path = Path(config.get("tts_model_path", DEFAULT_TTS)).expanduser()
        self.tts_backend = config.get("tts_backend", "piper")
        if self.tts_backend not in {"piper", "kokoro"}:
            raise ValueError("Unknown local voice backend")
        self.tts_speed = float(config.get("tts_speed", 1.0))
        self.piper_noise = float(config.get("piper_noise_scale", 0.667))
        if not math.isfinite(self.tts_speed) or not 0.6 <= self.tts_speed <= 1.5:
            raise ValueError("Voice speed must be between 0.6 and 1.5")
        if not math.isfinite(self.piper_noise) or not 0 <= self.piper_noise <= 1:
            raise ValueError("Voice variation must be between 0 and 1")
        self.kokoro_path = Path(config.get("kokoro_model_dir", DEFAULT_KOKORO)).expanduser()
        self.kokoro_voice = config.get("kokoro_voice", "af_heart")
        if self.kokoro_voice not in KOKORO_VOICES:
            raise ValueError("Unknown Kokoro voice")
        self.threads = max(1, min(8, int(config.get("speech_threads", 2))))
        tts_default = self.threads if "speech_threads" in config else (6 if self.tts_backend == "kokoro" else 2)
        self.tts_threads = max(1, min(8, int(config.get("tts_threads", tts_default))))
        self.max_seconds = min(60, max(1, float(config.get("max_utterance_seconds", 20))))
        self.recognizer = None
        self.voice = None
        # A cancelled asyncio.to_thread keeps running; locks protect native models
        # when the next utterance arrives before previous inference has completed.
        self.stt_lock = threading.Lock()
        self.tts_lock = threading.Lock()

    async def transcribe(self, pcm_24k_mono: bytes) -> str:
        if len(pcm_24k_mono) % 2:
            raise ValueError("PCM16 contains an incomplete sample")
        if len(pcm_24k_mono) > int(self.max_seconds * 48000):
            raise ValueError("Utterance exceeds configured duration")
        if not pcm_24k_mono:
            return ""
        return await asyncio.to_thread(self._transcribe, pcm_24k_mono)

    def _transcribe(self, pcm):
        import sherpa_onnx
        with self.stt_lock:
            if self.recognizer is None:
                paths = {name: self.stt_path / filename for name, filename in {
                    "encoder": "encoder.int8.onnx", "decoder": "decoder.int8.onnx",
                    "joiner": "joiner.int8.onnx", "tokens": "tokens.txt",
                }.items()}
                if not all(path.is_file() for path in paths.values()):
                    raise FileNotFoundError("Local Parakeet speech model is incomplete")
                self.recognizer = sherpa_onnx.OfflineRecognizer.from_transducer(
                    **{name: str(path) for name, path in paths.items()},
                    num_threads=self.threads, sample_rate=16000,
                    model_type="nemo_transducer", provider="cpu", decoding_method="greedy_search",
                )
            samples = np.frombuffer(resample(pcm, 24000, 16000), dtype="<i2").astype(np.float32) / 32768.0
            stream = self.recognizer.create_stream()
            stream.accept_waveform(16000, samples)
            self.recognizer.decode_stream(stream)
            return stream.result.text.strip()

    async def synthesize(self, text: str) -> bytes:
        if not text.strip():
            return b""
        if len(text) > 2000:
            raise ValueError("Speech text exceeds 2000 characters")
        return await asyncio.to_thread(self._synthesize, text)

    async def warmup(self):
        """Prime the selected Kokoro voice before listening; discard all audio."""
        if self.tts_backend == "kokoro":
            await self.synthesize("Hello.")

    async def synthesize_stream(self, text: str):
        """Native sentence chunks; same full text and prosody as whole synthesis.

        The queue holds at most 60 seconds of PCM (2.88 MB). Closing the
        iterator cancels subsequent native callbacks without waiting for a
        currently running model operation; the shared model lock stays held
        until that operation actually finishes.
        """
        if not text.strip():
            return
        if len(text) > 2000:
            raise ValueError("Speech text exceeds 2000 characters")
        loop = asyncio.get_running_loop()
        queue = asyncio.Queue()
        stopped = threading.Event()

        def post(value):
            if not stopped.is_set():
                loop.call_soon_threadsafe(queue.put_nowait, value)

        def worker():
            try:
                with self.tts_lock:
                    if stopped.is_set():
                        return
                    if self.tts_backend == "kokoro":
                        self._synthesize_kokoro(text, emit=post, stopped=stopped)
                    else:
                        post(self._synthesize_piper(text))
            except Exception as error:
                post(error)
            finally:
                post(None)

        job = asyncio.create_task(asyncio.to_thread(worker))
        try:
            while True:
                value = await queue.get()
                if value is None:
                    break
                if isinstance(value, Exception):
                    raise value
                if value:
                    yield value
        finally:
            stopped.set()
            job.cancel()

    def _synthesize(self, text):
        with self.tts_lock:
            if self.tts_backend == "kokoro":
                return self._synthesize_kokoro(text)
            return self._synthesize_piper(text)

    def _synthesize_piper(self, text):
        import onnxruntime as ort
        from piper import PiperVoice, SynthesisConfig
        from piper.config import PiperConfig
        if self.voice is None:
            config_path = Path(str(self.tts_path) + ".json")
            if not self.tts_path.is_file() or not config_path.is_file():
                raise FileNotFoundError("Local Piper voice model or config is missing")
            with config_path.open() as handle:
                voice_config = PiperConfig.from_dict(json.load(handle))
            options = ort.SessionOptions()
            options.intra_op_num_threads = self.tts_threads
            options.inter_op_num_threads = 1
            self.voice = PiperVoice(
                session=ort.InferenceSession(str(self.tts_path), sess_options=options, providers=["CPUExecutionProvider"]),
                config=voice_config, use_tashkeel=False,
            )
        parts = []
        total = 0
        settings = SynthesisConfig(length_scale=1.0 / self.tts_speed, noise_scale=self.piper_noise)
        for chunk in self.voice.synthesize(text, syn_config=settings):
            if chunk.sample_width != 2 or chunk.sample_channels != 1:
                raise RuntimeError("Piper returned an unsupported audio format")
            total += len(chunk.audio_int16_bytes)
            if total > chunk.sample_rate * 2 * 60:
                raise RuntimeError("Synthesized speech exceeded 60 seconds")
            parts.append(chunk.audio_int16_bytes)
        return resample(b"".join(parts), self.voice.config.sample_rate, 24000)

    def _synthesize_kokoro(self, text, emit=None, stopped=None):
        import sherpa_onnx
        if self.voice is None:
            root = self.kokoro_path
            required = ["model.onnx", "voices.bin", "tokens.txt", "lexicon-us-en.txt"]
            if not all((root / name).is_file() for name in required) or not (root / "espeak-ng-data").is_dir():
                raise FileNotFoundError("Local Kokoro voice model is incomplete; run voice setup for Kokoro")
            config = sherpa_onnx.OfflineTtsConfig(
                model=sherpa_onnx.OfflineTtsModelConfig(
                    kokoro=sherpa_onnx.OfflineTtsKokoroModelConfig(
                        model=str(root / "model.onnx"), voices=str(root / "voices.bin"),
                        tokens=str(root / "tokens.txt"), lexicon=str(root / "lexicon-us-en.txt"),
                        data_dir=str(root / "espeak-ng-data"), lang="en-us",
                    ),
                    num_threads=self.tts_threads, provider="cpu", debug=False,
                ),
                max_num_sentences=1,
            )
            if not config.validate():
                raise ValueError("Local Kokoro voice configuration is invalid")
            self.voice = sherpa_onnx.OfflineTts(config)
        if emit is not None:
            total = 0
            failure = None
            rate = self.voice.sample_rate

            def callback(samples, progress):
                nonlocal total, failure
                if stopped is not None and stopped.is_set():
                    return 0
                try:
                    pcm = self._kokoro_pcm(samples, rate)
                    total += len(pcm)
                    if total > 60 * 48000:
                        raise RuntimeError("Synthesized speech exceeded 60 seconds")
                    emit(pcm)
                except Exception as error:
                    failure = error
                    return 0
                return 1

            self.voice.generate(text, sid=KOKORO_VOICES[self.kokoro_voice],
                                speed=self.tts_speed, callback=callback)
            if failure is not None:
                raise failure
            return b""
        audio = self.voice.generate(text, sid=KOKORO_VOICES[self.kokoro_voice], speed=self.tts_speed)
        return self._kokoro_pcm(audio.samples, audio.sample_rate)

    @staticmethod
    def _kokoro_pcm(samples, rate):
        samples = np.asarray(samples, dtype=np.float32)
        if rate <= 0 or samples.ndim != 1 or not np.all(np.isfinite(samples)):
            raise RuntimeError("Kokoro returned an unsupported audio format")
        if len(samples) > rate * 60:
            raise RuntimeError("Synthesized speech exceeded 60 seconds")
        pcm = (np.clip(samples, -1, 1) * 32767).astype("<i2").tobytes()
        return resample(pcm, rate, 24000)
