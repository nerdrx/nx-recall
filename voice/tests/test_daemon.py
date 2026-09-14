import tempfile
from pathlib import Path
import unittest
from unittest.mock import patch

from nx_recall_voice.daemon import LocalDevices, load_config


class DaemonTests(unittest.IsolatedAsyncioTestCase):
    def test_config_requires_local_backend_and_real_wake_phrase(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "voice.toml"
            for data in ['backend="openai"', 'wake_words=["!!!"]', 'audio_mode="all"']:
                path.write_text(data)
                with self.subTest(data=data), self.assertRaises(ValueError):
                    load_config(path)
            path.write_text('mode="always"')
            self.assertEqual(load_config(path)["mode"], "always")

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
