"""Read-only NX Recall retrieval over its local Unix socket.

Returned transcripts are untrusted reference material, never model instructions.
No recording controls, database access, cloud service, or transcript logging.
"""
import asyncio
from datetime import datetime, timezone
import json
import os
import math
import re
from pathlib import Path
import socket
import struct
import time
from collections import deque

MAX_LINE = 1024 * 1024
MAX_TEXT = 1500
MAX_TOTAL_TEXT = 6000


class RecallError(RuntimeError):
    """A sanitized local retrieval failure; contains no query/transcript text."""


class RecognitionUnavailable(RecallError):
    """A fixed diagnostic code, never transcript content or private device names."""
    def __init__(self, code, diagnostics=None):
        self.code = code
        self.diagnostics = diagnostics or {}
        super().__init__("Shared recognition is unavailable")


def _recognized(segments, start_ns, end_ns, source, consumed=(), capture_window=None):
    """Accept only whole, closely aligned turns; never splice unrelated words."""
    if not isinstance(segments, list) or len(segments) >= 64:
        return None
    # Neural and energy VAD disagree about quiet phonemes. The actual captured
    # PCM window is stronger evidence than broadening the voiced-core tolerance.
    if capture_window is None:
        lower, upper = start_ns - 400_000_000, end_ns + 400_000_000
    else:
        lower, upper = capture_window[0] - 200_000_000, capture_window[1] + 200_000_000
    rows = []
    for row in segments:
        if not isinstance(row, dict) or row.get('source') != source:
            continue
        try:
            a, b = int(row['t_start_ns']), int(row['t_end_ns'])
        except (KeyError, TypeError, ValueError):
            return None
        if b <= start_ns or a >= end_ns:
            continue
        if b <= a or a < lower or b > upper:
            return None
        ident = row.get('id')
        text = _clean(row.get('text'), 2001)
        if type(ident) is not int or ident in consumed or not text or len(text) > 2000:
            return None
        rows.append((a, b, ident, text))
    rows.sort()
    if not rows or len({r[2] for r in rows}) != len(rows):
        return None
    covered, last = 0, start_ns
    for a, b, _, _ in rows:
        left, right = max(a, start_ns), min(b, end_ns)
        covered += max(0, right - max(left, last))
        last = max(last, right)
    edge = min(300_000_000, (end_ns - start_ns) // 4)
    if covered < (end_ns - start_ns) * .75 or rows[0][0] > start_ns + edge or rows[-1][1] < end_ns - edge:
        return None
    text = ' '.join(r[3] for r in rows)
    if len(text) > 2000:
        return None
    return {'text': text, 'segment_ids': [r[2] for r in rows]}


class _RemoteError(RecallError):
    def __init__(self, code):
        self.code = code
        super().__init__("NX Recall rejected the search request")


def default_socket():
    if os.environ.get("NXR_SOCKET"):
        return Path(os.environ["NXR_SOCKET"]).expanduser()
    if os.environ.get("XDG_RUNTIME_DIR"):
        return Path(os.environ["XDG_RUNTIME_DIR"]) / "nx-recall.sock"
    runtime = Path("/run/user") / str(os.getuid()) / "nx-recall.sock"
    if runtime.exists():
        return runtime
    return Path(os.environ.get("XDG_DATA_HOME", Path.home() / ".local/share")) / "nx-recall/nx-recall.sock"


def _clean(value, length):
    if not isinstance(value, str):
        return ""
    return " ".join(value.split())[:length]


def _results(hits, limit, mode):
    if not isinstance(hits, list):
        raise RecallError("NX Recall returned invalid search results")
    results = []
    seen = set()
    remaining = MAX_TOTAL_TEXT
    for hit in hits:
        if not isinstance(hit, dict):
            continue
        text = _clean(hit.get("text"), min(MAX_TEXT, remaining))
        if not text or text in seen:
            continue
        seen.add(text)
        timestamp = None
        try:
            raw = hit.get("t_ns", hit.get("t_start_ns"))
            seconds = int(raw) // 1_000_000_000 if raw is not None else int(hit["t_ms"]) // 1000
            timestamp = datetime.fromtimestamp(seconds, timezone.utc).isoformat().replace("+00:00", "Z")
        except (KeyError, ValueError, TypeError, OverflowError, OSError):
            pass
        source = _clean(hit.get("source"), 120) or "unknown source"
        segment = hit.get("id")
        identity = f"; segment {segment}" if type(segment) is int else ""
        via = hit.get("via") if hit.get("via") in ("keyword", "semantic", "both") else mode
        results.append({"text": text, "source": f"NX Recall: {source}{identity}; {via}",
                        "timestamp": timestamp})
        remaining -= len(text)
        if len(results) >= limit or remaining <= 0:
            break
    return results


def _identify(segments, speakers, start_ns, end_ns, source):
    if not isinstance(speakers, list):
        return None
    named = {}
    for speaker in speakers:
        if not isinstance(speaker, dict) or type(speaker.get("id")) is not int:
            continue
        name = _clean(speaker.get("name"), 100)
        if not name or name == _clean(speaker.get("auto"), 100):
            continue
        if re.match(r"^(?:speaker|sprecher|voice|unknown|unnamed)(?:\b|[_#-])", name, re.I):
            continue
        named[speaker["id"]] = name
    matches = []
    for row in segments:
        if not isinstance(row, dict) or row.get("source") != source:
            continue
        try:
            a, b = int(row["t_start_ns"]), int(row["t_end_ns"])
        except (KeyError, TypeError, ValueError):
            return None
        left, right = max(a, start_ns), min(b, end_ns)
        if right <= left:
            continue
        speaker = row.get("speaker")
        score, overlap = row.get("match_score"), row.get("overlap_frac")
        if type(speaker) is not int or speaker not in named or row.get("label_via") != "match":
            return None
        if type(score) not in (int, float) or not math.isfinite(score) or not 0.80 <= score <= 1:
            return None
        if type(overlap) not in (int, float) or not math.isfinite(overlap) or not 0 <= overlap <= 0.05:
            return None
        matches.append((left, right, speaker, score))
    if not matches or len({m[2] for m in matches}) != 1:
        return None
    # Require strong temporal coverage; nearby speech alone is never identity.
    covered, last = 0, start_ns
    for left, right, _, _ in sorted(matches):
        covered += max(0, right - max(left, last))
        last = max(last, right)
    if covered / (end_ns - start_ns) < 0.75:
        return None
    return {"name": named[matches[0][2]], "confidence": min(m[3] for m in matches)}


class RecallClient:
    def __init__(self, socket_path=None, timeout=5.0, semantic=True):
        self.socket_path = Path(socket_path).expanduser() if socket_path else default_socket()
        self.timeout = timeout
        self.semantic = semantic
        self.recognized_ids = deque(maxlen=128)

    async def retrieve(self, query, limit=4):
        """Return at most eight excerpts, each 1500 chars, 6000 chars total.

        Each result has text, source (including segment and search provenance),
        and timestamp (ISO UTC or None). Missing Recall raises RecallError so
        callers can distinguish unavailable memory from no matching records.
        """
        if not isinstance(query, str) or type(limit) is not int or limit < 1:
            raise ValueError("query must be text and limit must be a positive integer")
        query = query.strip()[:512]
        if not query:
            return []
        limit = min(limit, 8)
        try:
            async with asyncio.timeout(self.timeout):
                return await self._retrieve(query, limit)
        except RecallError:
            raise
        except (OSError, TimeoutError, ValueError, UnicodeError) as exc:
            raise RecallError("NX Recall is unavailable or returned an invalid response") from None

    async def _retrieve(self, query, limit):
        async def operation(call):
            mode = "hybrid" if self.semantic else "keyword"
            params = {"q": query, "limit": limit}
            if self.semantic:
                params["mode"] = "hybrid"
            try:
                result = await call("search.semantic" if self.semantic else "search", params, 1)
            except _RemoteError as exc:
                if not self.semantic or exc.code != "unavailable":
                    raise
                mode = "keyword"
                result = await call("search", {"q": query, "limit": limit}, 2)
            return _results(result.get("hits"), limit, mode)
        return await self._session(operation)

    async def identify(self, start_ns, end_ns, source="vesktop"):
        """Reuse Recall's named acoustic match for this exact source/time span.

        Returns {name, confidence} or None. Confidence is Recall's raw cosine,
        NOT a calibrated probability. No PCM enrollment or Discord identity
        inference. A short retry tolerates processing lag; total wait <= 2s.
        """
        if type(start_ns) is not int or type(end_ns) is not int or not source or not isinstance(source, str):
            raise ValueError("identification requires UTC nanoseconds and an exact source")
        if not 500_000_000 <= end_ns - start_ns <= 30_000_000_000:
            return None
        async def operation(call):
            result = await call("transcript", {"source": source, "from": start_ns - 5_000_000_000,
                                "to": end_ns + 1, "limit": 64}, 1)
            segments = result.get("segments")
            if not isinstance(segments, list) or len(segments) >= 64:
                return None
            names = await call("speakers.list", {}, 2)
            return _identify(segments, names.get("speakers"), start_ns, end_ns, source)
        try:
            async with asyncio.timeout(min(self.timeout, 2.0)):
                for attempt in range(3):
                    result = await self._session(operation)
                    if result is not None:
                        return result
                    if attempt < 2:
                        await asyncio.sleep(0.3)
        except (RecallError, OSError, TimeoutError, ValueError, UnicodeError):
            return None
        return None

    async def recognize(self, start_ns, end_ns, *, source, capture_source=None, input_guard=None, capture_window=None, wait_seconds=8):
        """Read this live VAD interval from Recall; no alternate STT is invoked.

        Microphone reuse requires the same resolved device. Vesktop reuse requires
        one active captured application stream, so another instance cannot answer.
        Two matching reads allow split transcript commits to settle.
        """
        if type(start_ns) is not int or type(end_ns) is not int or source not in ('mic', 'vesktop'):
            raise ValueError('Shared recognition needs an exact source and UTC interval')
        if not 40_000_000 <= end_ns - start_ns <= 60_000_000_000:
            raise RecognitionUnavailable('invalid_interval')
        if end_ns > time.time_ns() + 1_000_000_000 or time.time_ns() - end_ns > 20_000_000_000:
            raise RecognitionUnavailable('stale_interval')
        if capture_window is not None:
            if (not isinstance(capture_window, (tuple, list)) or len(capture_window) != 2
                    or any(type(value) is not int for value in capture_window)
                    or not start_ns - 1_000_000_000 <= capture_window[0] <= start_ns
                    or not end_ns <= capture_window[1] <= end_ns + 2_000_000_000):
                raise RecognitionUnavailable('invalid_interval')
        if source == 'vesktop' and not callable(input_guard):
            raise RecognitionUnavailable('input_mismatch')
        if source == 'mic' and (not isinstance(capture_source, str) or not capture_source):
            raise RecognitionUnavailable('input_mismatch')
        started = time.monotonic()
        window = capture_window or (start_ns, end_ns)
        diagnostics = {'recognition_voiced_ms': (end_ns - start_ns) // 1_000_000,
                       'recognition_observed_ms': (window[1] - window[0]) // 1_000_000,
                       'recognition_candidate_count': 0}
        async def operation(call):
            sequence = 0
            async def rpc(method, params=None):
                nonlocal sequence
                sequence += 1
                return await call(method, params or {}, sequence)
            previous = None
            while True:
                state = await rpc('status')
                if state.get('paused') is not False:
                    raise RecognitionUnavailable('paused')
                if source == 'mic':
                    mic = await rpc('mic.get')
                    if mic.get('enabled') is not True or mic.get('active') is not True:
                        raise RecognitionUnavailable('source_unavailable')
                    device = mic.get('device')
                    if device is None:
                        devices = (await rpc('devices.list')).get('devices')
                        defaults = [d.get('node_name') for d in devices if isinstance(d, dict) and d.get('is_default') is True] if isinstance(devices, list) else []
                        if len(defaults) != 1:
                            raise RecognitionUnavailable('input_mismatch')
                        device = defaults[0]
                    if device != capture_source:
                        raise RecognitionUnavailable('input_mismatch')
                else:
                    if await input_guard() is not True:
                        raise RecognitionUnavailable('input_mismatch')
                    sources = (await rpc('sources.list')).get('sources')
                    matching = [s for s in sources if isinstance(s, dict) and s.get('match_key') == 'vesktop'] if isinstance(sources, list) else []
                    if len(matching) != 1 or matching[0].get('allowed') is not True or not matching[0].get('streams'):
                        raise RecognitionUnavailable('source_unavailable')
                    if matching[0]['streams'] != 1:
                        raise RecognitionUnavailable('ambiguous_source')
                # The wider READ can diagnose clock/segmentation disagreement;
                # admission remains bounded by the actual observed audio window.
                reply = await rpc('transcript', {'source': source, 'from': window[0] - 5_000_000_000,
                                                 'to': window[1] + 5_000_000_001, 'limit': 64})
                segments = reply.get('segments')
                candidates = []
                if isinstance(segments, list):
                    for row in segments:
                        if not isinstance(row, dict) or row.get('source') != source:
                            continue
                        try:
                            a, b = int(row['t_start_ns']), int(row['t_end_ns'])
                        except (KeyError, TypeError, ValueError):
                            continue
                        candidates.append((a, b))
                diagnostics['recognition_candidate_count'] = len(candidates)
                if candidates:
                    a, b = min(candidates, key=lambda bounds: abs(bounds[0] - start_ns) + abs(bounds[1] - end_ns))
                    diagnostics['recognition_start_offset_ms'] = (a - start_ns) // 1_000_000
                    diagnostics['recognition_end_offset_ms'] = (b - end_ns) // 1_000_000
                result = _recognized(segments, start_ns, end_ns, source, self.recognized_ids, capture_window)
                if result is not None and result == previous:
                    self.recognized_ids.extend(result['segment_ids'])
                    return {**result, 'wait_ms': int((time.monotonic() - started) * 1000)}
                previous = result
                await asyncio.sleep(.2)
        try:
            async with asyncio.timeout(min(10, max(.1, float(wait_seconds)))):
                return await self._session(operation)
        except RecognitionUnavailable:
            raise
        except TimeoutError:
            raise RecognitionUnavailable('timeout', diagnostics) from None
        except (RecallError, OSError, ValueError, UnicodeError):
            raise RecognitionUnavailable('unavailable') from None

    async def _session(self, operation):
        reader, writer = await asyncio.open_unix_connection(str(self.socket_path), limit=MAX_LINE)
        try:
            peer = writer.get_extra_info("socket")
            _, uid, _ = struct.unpack("3i", peer.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
            if uid != os.getuid():
                raise RecallError("NX Recall socket belongs to another user")

            async def send(payload):
                writer.write(json.dumps(payload, ensure_ascii=False).encode() + b"\n")
                await writer.drain()

            async def read():
                line = await reader.readline()
                if not line or len(line) > MAX_LINE or not line.endswith(b"\n"):
                    raise RecallError("NX Recall returned an incomplete or oversized response")
                value = json.loads(line)
                if not isinstance(value, dict):
                    raise RecallError("NX Recall returned an invalid response")
                return value

            await send({"hello": {"proto": 1, "client": "lanalu-bridge/1"}})
            hello = await read()
            if not isinstance(hello.get("welcome"), dict) or hello["welcome"].get("proto") != 1:
                raise RecallError("NX Recall protocol handshake failed")

            async def call(method, params, request_id):
                await send({"id": request_id, "method": method, "params": params})
                for _ in range(16):
                    reply = await read()
                    if reply.get("id") != request_id:
                        continue
                    if "err" in reply:
                        err = reply["err"]
                        raise _RemoteError(err.get("code") if isinstance(err, dict) else None)
                    result = reply.get("ok")
                    if not isinstance(result, dict):
                        raise RecallError("NX Recall returned invalid search results")
                    return result
                raise RecallError("NX Recall did not return the requested search")

            return await operation(call)
        finally:
            writer.close()
            await writer.wait_closed()


async def retrieve(query, limit=4, *, socket_path=None):
    """Convenience helper; use RecallClient for configured timeout/search mode."""
    return await RecallClient(socket_path=socket_path).retrieve(query, limit)
