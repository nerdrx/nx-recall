import asyncio
import json
import os
from pathlib import Path
import tempfile
import types
import unittest
from unittest.mock import patch

from nx_recall_voice.control import Control


class ControlTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name) / 'private/control.sock'
        self.queue = asyncio.Queue(maxsize=1)
        self.status = types.SimpleNamespace(data={'connected': True})
        self.control = Control(self.path, self.queue, self.status)
        await self.control.start()
        self.addAsyncCleanup(self.control.close)

    async def request(self, payload):
        reader, writer = await asyncio.open_unix_connection(self.path)
        writer.write(payload)
        await writer.drain()
        result = json.loads(await reader.readline())
        writer.close()
        await writer.wait_closed()
        return result

    async def test_private_socket_and_text_only_queue(self):
        self.assertEqual(self.path.stat().st_mode & 0o777, 0o600)
        self.assertEqual(self.path.parent.stat().st_mode & 0o777, 0o700)
        self.assertEqual(await self.request(b'{"type":"text","text":" hello "}\n'), {'ok': True})
        self.assertEqual(self.queue.get_nowait().text, 'hello')
        self.assertEqual(self.status.data, {'connected': True})

    async def test_reply_is_volatile_second_frame_not_status(self):
        reader, writer = await asyncio.open_unix_connection(self.path)
        writer.write(b'{"type":"text","text":"private question"}\n')
        await writer.drain()
        self.assertEqual(json.loads(await reader.readline()), {'ok': True})
        request = self.queue.get_nowait()
        request.response.set_result({'type': 'reply', 'text': 'private answer'})
        self.assertEqual(json.loads(await reader.readline()), {'type': 'reply', 'text': 'private answer'})
        self.assertEqual(self.status.data, {'connected': True})
        writer.close()
        await writer.wait_closed()

    async def test_invalid_and_oversized_frames_never_enqueue(self):
        for payload in (b'[]\n', b'{"type":"text","text":42}\n', b'{"type":"text","text":" "}\n',
                        json.dumps({'type': 'text', 'text': 'x' * 2001}).encode() + b'\n',
                        b'x' * 9000 + b'\n'):
            self.assertEqual((await self.request(payload))['error'], 'invalid_request')
        self.assertTrue(self.queue.empty())

    async def test_busy_and_not_ready_are_explicit(self):
        self.queue.put_nowait('previous')
        payload = b'{"type":"text","text":"hello"}\n'
        self.assertEqual((await self.request(payload))['error'], 'busy')
        self.status.data['connected'] = False
        self.assertEqual((await self.request(payload))['error'], 'not_ready')
        self.assertEqual(self.queue.get_nowait(), 'previous')

    async def test_peer_uid_mismatch_is_rejected(self):
        with patch('nx_recall_voice.control.os.getuid', return_value=-1):
            result = await self.request(b'{"type":"text","text":"hello"}\n')
        self.assertEqual(result, {'ok': False, 'error': 'forbidden'})
        self.assertTrue(self.queue.empty())

    async def test_close_removes_socket(self):
        await self.control.close()
        self.assertFalse(self.path.exists())
