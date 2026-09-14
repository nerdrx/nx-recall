"""Same-user, bounded local text input. Conversation text is never logged."""
import asyncio
from dataclasses import dataclass
from .recall import RecallClient, RecallError
import json
import os
import re
from pathlib import Path
import socket
import stat
import struct


@dataclass
class TextTurn:
    text: str
    response: asyncio.Future


class Control:
    def __init__(self, path, queue, status):
        self.path = Path(path)
        self.queue = queue
        self.status = status
        self.server = None
        self.clients = set()
        self.correction_lock = asyncio.Lock()

    async def start(self):
        folder = self.path.parent
        folder.mkdir(mode=0o700, parents=True, exist_ok=True)
        if folder.is_symlink() or folder.stat().st_uid != os.getuid():
            raise RuntimeError('Control directory ownership is invalid')
        folder.chmod(0o700)
        if self.path.exists():
            if not stat.S_ISSOCK(self.path.lstat().st_mode):
                raise RuntimeError('Control socket path is occupied')
            self.path.unlink()
        self.server = await asyncio.start_unix_server(self.handle, path=self.path, limit=8192)
        self.path.chmod(0o600)

    async def correct_heard(self, data):
        ident, text = data.get('id'), data.get('text')
        if (set(data) != {'type', 'id', 'text'} or not isinstance(ident, str)
                or not re.fullmatch(r'[0-9a-f]{32}', ident) or not isinstance(text, str)
                or not text.strip() or len(text) > 2000):
            return {'ok': False, 'error': 'invalid_request'}
        if self.correction_lock.locked():
            return {'ok': False, 'error': 'busy'}
        async with self.correction_lock:
            lookup = getattr(self.status, 'heard_turn', None)
            turn = lookup(ident) if callable(lookup) else None
            if turn is None:
                return {'ok': False, 'error': 'heard_expired'}
            try:
                saved = await RecallClient().correct_heard(turn['text'], text.strip(), turn['source'])
            except (RecallError, ValueError):
                return {'ok': False, 'error': 'save_failed'}
            self.status.corrected_heard(ident, text.strip())
            return {'ok': True, **saved}

    async def handle(self, reader, writer):
        task = asyncio.current_task()
        self.clients.add(task)
        result = {'ok': False, 'error': 'invalid_request'}
        response = None
        try:
            peer = writer.get_extra_info('socket')
            _, uid, _ = struct.unpack('3i', peer.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
            if uid != os.getuid():
                result['error'] = 'forbidden'
            elif len(self.clients) > 8:
                result['error'] = 'busy'
            else:
                async with asyncio.timeout(3):
                    raw = await reader.readline()
                if len(raw) > 8192 or not raw.endswith(b'\n'):
                    raise ValueError('Invalid frame')
                data = json.loads(raw)
                text = data.get('text') if isinstance(data, dict) else None
                if isinstance(data, dict) and data == {'type': 'heard'}:
                    snapshot = getattr(self.status, 'heard', None)
                    result = {'ok': True, 'heard': snapshot() if callable(snapshot) else []}
                elif isinstance(data, dict) and data.get('type') == 'correct_heard':
                    result = await self.correct_heard(data)
                elif not isinstance(text, str) or not text.strip() or len(text) > 2000 or data.get('type') != 'text':
                    raise ValueError('Invalid request')
                elif not self.status.data.get('connected'):
                    result['error'] = 'not_ready'
                elif self.queue.full():
                    result['error'] = 'busy'
                else:
                    response = asyncio.get_running_loop().create_future()
                    self.queue.put_nowait(TextTurn(text.strip(), response))
                    result = {'ok': True}
            writer.write(json.dumps(result, ensure_ascii=False).encode() + b'\n')
            await writer.drain()
            if response is not None:
                try:
                    reply = await asyncio.wait_for(asyncio.shield(response), 90)
                except asyncio.TimeoutError:
                    reply = {'type': 'result', 'error': 'timeout'}
                writer.write(json.dumps(reply, ensure_ascii=False).encode() + b'\n')
                await writer.drain()
        except (ValueError, UnicodeError, asyncio.TimeoutError):
            try:
                writer.write(b'{"ok":false,"error":"invalid_request"}\n')
                await writer.drain()
            except (ConnectionError, OSError):
                pass
        except (ConnectionError, OSError):
            pass
        finally:
            if response is not None and not response.done():
                response.cancel()
            writer.close()
            try:
                await writer.wait_closed()
            except (ConnectionError, OSError):
                pass
            self.clients.discard(task)

    async def close(self):
        if self.server:
            self.server.close()
            self.path.unlink(missing_ok=True)
        tasks = list(self.clients)
        for task in tasks:
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
        if self.server:
            await self.server.wait_closed()
