import asyncio
import os
import unittest

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
