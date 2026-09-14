"""Offline speech recognition and synthesis using existing local model files.

Inference never opens a network connection. FFmpeg resamples in memory; no
conversation audio or transcript is stored. Models load lazily and remain hot.
"""
import asyncio
import json
from pathlib import Path
import subprocess
import threading

import numpy as np

DEFAULT_STT = Path.home() / ".local/share/nx-recall/models/sherpa-onnx-nemo-parakeet_tdt_transducer_110m-en-36000-int8"
DEFAULT_TTS = Path.home() / ".local/share/nx-recall/models/voices/en_US-amy-medium.onnx"


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
        self.threads = max(1, min(8, int(config.get("speech_threads", 2))))
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

    def _synthesize(self, text):
        import onnxruntime as ort
        from piper import PiperVoice
        from piper.config import PiperConfig
        with self.tts_lock:
            if self.voice is None:
                config_path = Path(str(self.tts_path) + ".json")
                if not self.tts_path.is_file() or not config_path.is_file():
                    raise FileNotFoundError("Local Piper voice model or config is missing")
                with config_path.open() as handle:
                    voice_config = PiperConfig.from_dict(json.load(handle))
                options = ort.SessionOptions()
                options.intra_op_num_threads = self.threads
                options.inter_op_num_threads = 1
                self.voice = PiperVoice(
                    session=ort.InferenceSession(str(self.tts_path), sess_options=options, providers=["CPUExecutionProvider"]),
                    config=voice_config, use_tashkeel=False,
                )
            parts = []
            total = 0
            for chunk in self.voice.synthesize(text):
                if chunk.sample_width != 2 or chunk.sample_channels != 1:
                    raise RuntimeError("Piper returned an unsupported audio format")
                total += len(chunk.audio_int16_bytes)
                if total > chunk.sample_rate * 2 * 60:
                    raise RuntimeError("Synthesized speech exceeded 60 seconds")
                parts.append(chunk.audio_int16_bytes)
            return resample(b"".join(parts), self.voice.config.sample_rate, 24000)
