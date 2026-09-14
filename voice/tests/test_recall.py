import asyncio
import json
from pathlib import Path
import tempfile
import unittest

from nx_recall_voice.recall import RecallClient, RecallError, _results, _identify


class RecallTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.path = str(Path(self.directory.name) / "recall.sock")

    async def test_local_handshake_semantic_fallback_and_limits(self):
        requests = []
        async def handle(reader, writer):
            try:
                hello = json.loads(await reader.readline())
                self.assertEqual(hello["hello"]["proto"], 1)
                writer.write(b'{"welcome":{"proto":1}}\n')
                await writer.drain()
                first = json.loads(await reader.readline())
                requests.append(first)
                writer.write(b'{"event":"ignored"}\n{"id":1,"err":{"code":"unavailable","msg":"private message"}}\n')
                await writer.drain()
                second = json.loads(await reader.readline())
                requests.append(second)
                hits = [{"id": n, "source": "Synthetic Discord", "text": str(n) + "x" * 2000,
                         "t_ns": "1789387200000000000"} for n in range(12)]
                writer.write(json.dumps({"id": 2, "ok": {"hits": hits}}).encode() + b"\n")
                await writer.drain()
            finally:
                writer.close()
                await writer.wait_closed()
        server = await asyncio.start_unix_server(handle, self.path)
        async with server:
            results = await RecallClient(self.path).retrieve("test-only query", limit=100)
        self.assertEqual([r["method"] for r in requests], ["search.semantic", "search"])
        self.assertEqual(requests[0]["params"], {"q": "test-only query", "limit": 8, "mode": "hybrid"})
        self.assertEqual(len(results), 4)
        self.assertEqual(sum(len(r["text"]) for r in results), 6000)
        self.assertTrue(all(r["source"].endswith("keyword") for r in results))
        self.assertEqual(results[0]["timestamp"], "2026-09-14T12:00:00Z")

    async def test_missing_socket_is_explicit_and_empty_query_is_local(self):
        client = RecallClient(self.path)
        self.assertEqual(await client.retrieve("   "), [])
        with self.assertRaises(RecallError):
            await client.retrieve("test")
        with self.assertRaises(ValueError):
            await client.retrieve("test", limit=0)

    async def test_protocol_refusal_does_not_leak_server_text(self):
        async def handle(reader, writer):
            await reader.readline()
            writer.write(b'{"error":{"message":"secret transcript material"}}\n')
            await writer.drain()
            writer.close()
            await writer.wait_closed()
        server = await asyncio.start_unix_server(handle, self.path)
        async with server:
            with self.assertRaisesRegex(RecallError, "handshake failed") as caught:
                await RecallClient(self.path).retrieve("test")
        self.assertNotIn("secret", str(caught.exception))

    def test_identify_requires_named_acoustic_unambiguous_coverage(self):
        start, end = 1_800_000_000_000_000_000, 1_800_000_002_000_000_000
        row = {"source": "vesktop", "speaker": 7, "label_via": "match", "match_score": 0.91,
               "overlap_frac": 0, "t_start_ns": str(start), "t_end_ns": str(end)}
        names = [{"id": 7, "name": "Alice", "auto": "Speaker 7"}]
        self.assertEqual(_identify([row], names, start, end, "vesktop"), {"name": "Alice", "confidence": 0.91})
        for key, value in [("label_via", "proximity"), ("label_via", "discord_stream"),
                           ("match_score", 0.7), ("match_score", None), ("match_score", float("nan")),
                           ("overlap_frac", 0.5), ("overlap_frac", None), ("source", "Discord"),
                           ("speaker", None), ("t_end_ns", str(start + 500_000_000))]:
            with self.subTest(key=key, value=value):
                bad = dict(row, **{key: value})
                self.assertIsNone(_identify([bad], names, start, end, "vesktop"))
        for name in [None, "", "Speaker 7", "Speaker A", "Unknown"]:
            self.assertIsNone(_identify([row], [{"id": 7, "name": name}], start, end, "vesktop"))
        second = dict(row, speaker=8)
        self.assertIsNone(_identify([row, second], names + [{"id": 8, "name": "Bob"}], start, end, "vesktop"))
        # Normal Discord/mic duplicates cannot veto or impersonate the Vesktop speaker.
        self.assertEqual(_identify([row, dict(row, source="mic", speaker=8)], names, start, end, "vesktop")["name"], "Alice")

    async def test_identify_native_read_only_protocol(self):
        start, end = 1_800_000_000_000_000_000, 1_800_000_002_000_000_000
        requests = []
        async def handle(reader, writer):
            try:
                await reader.readline()
                writer.write(b'{"welcome":{"proto":1}}\n')
                await writer.drain()
                request = json.loads(await reader.readline())
                requests.append(request)
                row = {"source": "vesktop", "speaker": 7, "label_via": "match", "match_score": 0.91,
                       "overlap_frac": 0, "t_start_ns": str(start), "t_end_ns": str(end)}
                writer.write(json.dumps({"id": 1, "ok": {"segments": [row]}}).encode() + b"\n")
                await writer.drain()
                request = json.loads(await reader.readline())
                requests.append(request)
                writer.write(b'{"id":2,"ok":{"speakers":[{"id":7,"name":"Alice","auto":"Speaker 7"}]}}\n')
                await writer.drain()
            finally:
                writer.close()
                await writer.wait_closed()
        server = await asyncio.start_unix_server(handle, self.path)
        async with server:
            result = await RecallClient(self.path).identify(start, end, "vesktop")
        self.assertEqual(result, {"name": "Alice", "confidence": 0.91})
        self.assertEqual([r["method"] for r in requests], ["transcript", "speakers.list"])
        self.assertEqual(requests[0]["params"]["source"], "vesktop")
        self.assertEqual(requests[0]["params"]["limit"], 64)

    def test_result_validation_and_provenance(self):
        self.assertEqual(_results([], 4, "keyword"), [])
        with self.assertRaises(RecallError):
            _results({}, 4, "keyword")
        hits = [{"id": 1, "text": " synthetic  reference ", "source": "Test", "via": "semantic"},
                {"id": 2, "text": "synthetic reference", "source": "Test"},
                {"id": 3, "text": None}]
        self.assertEqual(_results(hits, 4, "hybrid"), [{"text": "synthetic reference",
                         "source": "NX Recall: Test; segment 1; semantic", "timestamp": None}])


if __name__ == "__main__":
    unittest.main()
