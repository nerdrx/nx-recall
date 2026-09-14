import asyncio
from array import array
import types
import unittest
from unittest.mock import patch

from nx_recall_voice.audio import Audio


class Process:
    def __init__(self):
        self.returncode = None
        self.terminated = False
        self.killed = False
        self.written = bytearray()
        self.started = asyncio.Event()
        self.stdin = self
    def write(self, data):
        self.written.extend(data)
        self.started.set()
    async def drain(self):
        pass
    def terminate(self):
        self.terminated = True
        self.returncode = -15
    def kill(self):
        self.killed = True
        self.returncode = -9
    async def wait(self):
        return self.returncode


class AudioTests(unittest.IsolatedAsyncioTestCase):
    async def test_levels_measure_input_and_reset_without_retaining_pcm(self):
        audio = Audio(types.SimpleNamespace())
        reader = asyncio.StreamReader()
        reader.feed_data(array('h', [16384] * 480).tobytes())
        audio.capture = types.SimpleNamespace(stdout=reader)
        await audio.read()
        levels = audio.levels()
        self.assertEqual(levels['audio_input_peak'], .5)
        self.assertGreater(levels['last_input_signal_at'], 0)
        self.assertEqual(audio.levels()['audio_input_peak'], 0)
        self.assertEqual(Audio.peak(array('h', [-32768]).tobytes()), 1)
        reader.feed_data(bytes(960))
        await audio.read()
        self.assertEqual(audio.levels()['last_input_signal_at'], levels['last_input_signal_at'])

    async def test_clear_interrupts_paced_audio_and_reaps_playback(self):
        audio = Audio(types.SimpleNamespace())
        proc = Process()
        async def process(record):
            return proc
        audio.process = process
        task = asyncio.create_task(audio.write(bytes(48000)))
        await proc.started.wait()
        await audio.clear()
        await asyncio.wait_for(task, .2)
        self.assertTrue(proc.terminated)
        self.assertLess(len(proc.written), 48000)
        self.assertIsNone(audio.playback)
        self.assertEqual(audio.sent, 0)

    async def test_restarting_capture_reaps_previous_process(self):
        audio = Audio(types.SimpleNamespace())
        old, new = Process(), Process()
        audio.capture = old
        async def process(record):
            self.assertTrue(record)
            return new
        audio.process = process
        await audio.start()
        self.assertTrue(old.terminated)
        self.assertIs(audio.capture, new)
        async def fail():
            raise OSError("synthetic playback loss")
        audio.clear = fail
        with self.assertRaises(OSError):
            await audio.close()
        self.assertTrue(new.terminated)

    async def test_odd_pcm_rejected_before_process_start(self):
        audio = Audio(types.SimpleNamespace())
        with self.assertRaises(ValueError):
            await audio.write(b"x")
        self.assertIsNone(audio.playback)

    async def test_process_exit_race_is_reaped(self):
        proc = Process()
        def terminate():
            proc.returncode = 0
            raise ProcessLookupError()
        proc.terminate = terminate
        await Audio.stop_process(proc)
        self.assertEqual(proc.returncode, 0)


if __name__ == "__main__":
    unittest.main()
