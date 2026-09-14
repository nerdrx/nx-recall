import tempfile
import json
import asyncio
from pathlib import Path
import unittest
from unittest.mock import patch

from nx_recall_voice.daemon import LocalDevices, load_config
from nx_recall_voice import daemon


class DaemonTests(unittest.IsolatedAsyncioTestCase):
    def test_audio_meter_preserves_turn_state_without_logging(self):
        with tempfile.TemporaryDirectory() as directory, patch.object(daemon, 'runtime', lambda: Path(directory)), patch.object(daemon.logging, 'info') as log:
            status = daemon.Status()
            status('local_speaking', input_kind='text')
            updated = status.data['updated_at']
            status.audio_levels(dict(audio_input_peak=.3, audio_levels_at=123))
            saved = json.loads((Path(directory) / 'status.json').read_text())
            self.assertEqual(saved['event'], 'local_speaking')
            self.assertEqual(saved['updated_at'], updated)
            self.assertEqual(saved['audio_input_peak'], .3)
            self.assertEqual(log.call_count, 1)

    def test_config_requires_local_backend_and_real_wake_phrase(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "voice.toml"
            for data in ['backend="openai"', 'wake_words=["!!!"]', 'audio_mode="all"', 'recognition_source="cloud"']:
                path.write_text(data)
                with self.subTest(data=data), self.assertRaises(ValueError):
                    load_config(path)
            path.write_text('mode="always"')
            self.assertEqual(load_config(path)["mode"], "always")
            self.assertEqual(load_config(path)["recognition_source"], "recall")
            self.assertTrue(load_config(path)["llm_model"].endswith("qwen3.5-4b-q4_k_m.gguf"))

    async def test_transient_start_and_cleanup_errors_still_retry(self):
        started_again = asyncio.Event()
        attempts = []
        closes = []
        class Devices:
            def __init__(self, config):
                self.number = len(attempts)
                attempts.append(self.number)
            async def start(self):
                if self.number == 0:
                    raise OSError("synthetic PipeWire disconnect")
                started_again.set()
                await asyncio.Future()
            async def close(self):
                closes.append(self.number)
                if self.number == 0:
                    raise OSError("synthetic unavailable PipeWire cleanup")
        with patch.object(daemon, "LocalDevices", Devices), \
                patch.object(daemon, "Status", lambda: lambda *args, **kwargs: None):
            task = asyncio.create_task(daemon.run({"audio_mode": "local"}))
            try:
                await asyncio.wait_for(started_again.wait(), 3)
            finally:
                task.cancel()
                result = await asyncio.gather(task, return_exceptions=True)
        self.assertEqual(attempts, [0, 1])
        self.assertEqual(closes, [0, 1])
        self.assertIsInstance(result[0], asyncio.CancelledError)

    async def test_cancel_attempts_every_cleanup_and_keeps_cancellation(self):
        waiting = asyncio.Event()
        cleaned = []
        class Devices:
            incoming = "synthetic_incoming"
            microphone = "synthetic_mic"
            def __init__(self, **kwargs):
                pass
            async def start(self):
                pass
            async def close(self):
                cleaned.append("devices")
        class Router:
            def __init__(self, *args, **kwargs):
                self.restores = 0
            async def restore(self):
                self.restores += 1
                if self.restores > 1:
                    cleaned.append("router")
                    raise OSError("synthetic graph gone")
            async def reconcile(self):
                waiting.set()
                return {"ready": False}
        class Audio:
            def __init__(self, devices):
                pass
            async def start(self):
                await asyncio.Future()
            async def close(self):
                cleaned.append("audio")
                raise OSError("synthetic playback gone")
        with tempfile.TemporaryDirectory() as directory, \
                patch.object(daemon, "runtime", lambda: Path(directory)), \
                patch.object(daemon, "Devices", Devices), patch.object(daemon, "Router", Router), \
                patch.object(daemon, "Audio", Audio), \
                patch.object(daemon, "Status", lambda: lambda *args, **kwargs: None):
            task = asyncio.create_task(daemon.run({"audio_mode": "vesktop", "vesktop_profile": "/synthetic"}))
            try:
                await asyncio.wait_for(waiting.wait(), 1)
            finally:
                task.cancel()
                result = await asyncio.gather(task, return_exceptions=True)
        self.assertEqual(cleaned, ["audio", "router", "devices"])
        self.assertIsInstance(result[0], asyncio.CancelledError)

    async def test_local_devices_only_read_defaults_and_reject_monitor_mic(self):
        calls = []
        async def command(*args):
            calls.append(args)
            if args[-1] == 'get-default-source':
                return 'speaker.monitor'
            if args[-1] == 'get-default-sink':
                return 'speaker'
            if args[-1] == 'sources':
                return '[{"name":"speaker.monitor"}]'
            return '[{"name":"speaker"}]'
        with patch('nx_recall_voice.daemon.command', command):
            with self.assertRaisesRegex(ValueError, 'microphone'):
                await LocalDevices({}).start()
        self.assertTrue(all('set-default-source' not in call and 'set-default-sink' not in call for call in calls))


if __name__ == "__main__":
    unittest.main()
