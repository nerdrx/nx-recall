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

MAX_LINE = 1024 * 1024
MAX_TEXT = 1500
MAX_TOTAL_TEXT = 6000


class RecallError(RuntimeError):
    """A sanitized local retrieval failure; contains no query/transcript text."""


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
