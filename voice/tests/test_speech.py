import asyncio
import os
import threading
import unittest
from types import SimpleNamespace
from unittest.mock import Mock, AsyncMock

import numpy as np

from nx_recall_voice.speech import Speech, resample, DEFAULT_STT, DEFAULT_TTS


class SpeechTests(unittest.IsolatedAsyncioTestCase):
    async def test_empty_inputs_and_limits(self):
        speech = Speech()
        self.assertEqual(await speech.transcribe(b""), "")
        self.assertEqual(await speech.synthesize("  "), b"")
        with self.assertRaises(ValueError):
            await speech.transcribe(b"x")
        with self.assertRaises(ValueError):
            await speech.transcribe(b"\0" * (48000 * 21))
        with self.assertRaises(ValueError):
            await speech.synthesize("x" * 2001)

    def test_resampling_duration_and_amplitude(self):
        samples = (np.sin(np.arange(24000) * 2 * np.pi * 440 / 24000) * 12000).astype("<i2")
        output = resample(samples.tobytes(), 24000, 16000)
        self.assertEqual(len(output), 32000)
        self.assertGreater(np.max(np.frombuffer(output, dtype="<i2")), 11000)
        with self.assertRaises(ValueError):
            resample(b"x", 24000, 16000)

    def test_voice_settings_reject_invalid_values(self):
        for config in ({"tts_backend": "remote"}, {"tts_speed": float("nan")},
                       {"tts_speed": 2}, {"piper_noise_scale": -1},
                       {"kokoro_voice": "unknown"}):
            with self.assertRaises(ValueError):
                Speech(config)

    def test_piper_speed_and_variation_reach_synthesis(self):
        speech = Speech({"tts_speed": 1.25, "piper_noise_scale": 0.4})
        speech.voice = Mock(config=SimpleNamespace(sample_rate=24000))
        speech.voice.synthesize.return_value = [SimpleNamespace(
            sample_width=2, sample_channels=1, sample_rate=24000, audio_int16_bytes=b"\0\0" * 8)]
        self.assertEqual(speech._synthesize("hello"), b"\0\0" * 8)
        settings = speech.voice.synthesize.call_args.kwargs["syn_config"]
        self.assertAlmostEqual(settings.length_scale, 0.8)
        self.assertEqual(settings.noise_scale, 0.4)
        self.assertIsNone(settings.noise_w_scale)

    def test_kokoro_voice_speed_and_pcm_format(self):
        speech = Speech({"tts_backend": "kokoro", "kokoro_voice": "af_sarah", "tts_speed": 0.9})
        speech.voice = Mock()
        speech.voice.generate.return_value = SimpleNamespace(
            sample_rate=24000, samples=np.array([-2, -0.5, 0, 0.5, 2], dtype=np.float32))
        pcm = speech._synthesize("hello")
        speech.voice.generate.assert_called_once_with("hello", sid=9, speed=0.9)
        self.assertEqual(list(np.frombuffer(pcm, dtype="<i2")), [-32767, -16383, 0, 16383, 32767])
        speech.voice.generate.return_value = SimpleNamespace(sample_rate=1, samples=np.zeros(61))
        with self.assertRaises(RuntimeError):
            speech._synthesize("too long")
        speech.voice.generate.return_value = SimpleNamespace(sample_rate=24000, samples=np.array([float("nan")]))
        with self.assertRaises(RuntimeError):
            speech._synthesize("invalid")

    async def test_warmup_primes_only_kokoro_and_propagates_failure(self):
        for backend in ("piper", "kokoro"):
            speech = Speech({"tts_backend": backend})
            speech.synthesize = AsyncMock(return_value=b"synthetic PCM")
            speech.transcribe = AsyncMock(side_effect=AssertionError("warmup must not transcribe"))
            self.assertIsNone(await speech.warmup())
            if backend == "kokoro":
                speech.synthesize.assert_awaited_once_with("Hello.")
                speech.synthesize.side_effect = RuntimeError("selected voice missing")
                with self.assertRaisesRegex(RuntimeError, "selected voice missing"):
                    await speech.warmup()
            else:
                speech.synthesize.assert_not_awaited()
            speech.transcribe.assert_not_awaited()

    async def test_stream_yields_before_native_finishes_and_close_stops_callbacks(self):
        speech = Speech({"tts_backend": "kokoro"})
        resume = threading.Event()
        finished = threading.Event()
        returns = []
        def generate(text, sid, speed, callback):
            returns.append(callback(np.ones(24, dtype=np.float32), 0.5))
            resume.wait(2)
            returns.append(callback(np.ones(24, dtype=np.float32), 1.0))
            finished.set()
        speech.voice = SimpleNamespace(sample_rate=24000, generate=generate)
        stream = speech.synthesize_stream("One sentence. Another sentence.")
        chunk = await asyncio.wait_for(anext(stream), 1)
        self.assertEqual(len(chunk), 48)
        self.assertFalse(finished.is_set())
        await stream.aclose()
        resume.set()
        self.assertTrue(await asyncio.to_thread(finished.wait, 1))
        self.assertEqual(returns, [1, 0])

    async def test_stream_caps_total_pcm_and_propagates_native_errors(self):
        speech = Speech({"tts_backend": "kokoro"})
        def generate(text, sid, speed, callback):
            block = np.zeros(24000 * 31, dtype=np.float32)
            self.assertEqual(callback(block, 0.5), 1)
            self.assertEqual(callback(block, 1.0), 0)
        speech.voice = SimpleNamespace(sample_rate=24000, generate=generate)
        stream = speech.synthesize_stream("Long synthetic fixture.")
        self.assertEqual(len(await anext(stream)), 31 * 48000)
        with self.assertRaisesRegex(RuntimeError, "60 seconds"):
            await anext(stream)
        speech.voice.generate = Mock(side_effect=RuntimeError("synthetic model failure"))
        with self.assertRaisesRegex(RuntimeError, "synthetic model failure"):
            await anext(speech.synthesize_stream("Hello."))

    async def test_stream_honors_native_lock_and_cancelled_waiter(self):
        speech = Speech({"tts_backend": "kokoro"})
        speech.voice = SimpleNamespace(sample_rate=24000, generate=Mock())
        speech.tts_lock.acquire()
        stream = speech.synthesize_stream("Waiting.")
        task = asyncio.create_task(anext(stream))
        await asyncio.sleep(0.02)
        task.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await task
        speech.tts_lock.release()
        await asyncio.sleep(0.02)
        speech.voice.generate.assert_not_called()

    def test_kokoro_thread_default_and_explicit_override(self):
        self.assertEqual(Speech({"tts_backend": "kokoro"}).tts_threads, 6)
        self.assertEqual(Speech({"tts_backend": "kokoro", "speech_threads": 4}).tts_threads, 4)
        self.assertEqual(Speech({"tts_backend": "kokoro", "speech_threads": 2, "tts_threads": 6}).tts_threads, 6)
        self.assertEqual(Speech().tts_threads, 2)

    @unittest.skipUnless(os.environ.get("LANALU_TEST_LOCAL_MODELS") == "1", "explicit local-model integration check")
    async def test_existing_models_roundtrip_without_audio_devices(self):
        self.assertTrue(DEFAULT_STT.is_dir())
        self.assertTrue(DEFAULT_TTS.is_file())
        speech = Speech()
        pcm = await speech.synthesize("Hello, this is a local voice test. The weather is sunny today.")
        self.assertGreater(len(pcm), 48000)
        self.assertLess(len(pcm), 48000 * 20)
        result = (await speech.transcribe(pcm)).lower()
        self.assertIn("local", result)
        self.assertIn("weather", result)


if __name__ == "__main__":
    unittest.main()
