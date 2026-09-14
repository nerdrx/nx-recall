import copy
import unittest
import tempfile
import json
from pathlib import Path
from unittest.mock import patch

from nx_recall_voice import routing


def node(i, name, cls, binary=None, pid=None):
    return {"id": i, "type": "PipeWire:Interface:Node", "info": {"props": {
        "node.name": name, "media.class": cls, "object.serial": i + 1000,
        "application.process.binary": binary, "application.process.id": pid}}}


def port(i, parent, direction, channel="MONO"):
    return {"id": i, "type": "PipeWire:Interface:Port", "info": {"props": {
        "node.id": parent, "object.serial": i + 1000,
        "port.direction": direction, "audio.channel": channel}}}


def link(a, b):
    return {"id": a * 1000 + b, "type": "PipeWire:Interface:Link", "info": {
        "output-port-id": a, "input-port-id": b}}


class RoutingTests(unittest.IsolatedAsyncioTestCase):
    async def test_routes_only_verified_vesktop_and_restores(self):
        objects = [{"id": 0, "type": "PipeWire:Interface:Core", "info": {"cookie": 123}},
                   node(1, "lanalu_incoming", "Audio/Sink"),
                   node(2, "lanalu_microphone", "Audio/Source"),
                   node(3, "vesktop", "Stream/Output/Audio", "vesktop", 42),
                   node(4, "vesktop", "Stream/Input/Audio", "vesktop", 42),
                   node(5, "WEBRTC VoiceEngine", "Stream/Output/Audio", "Discord", 43),
                   node(6, "speaker", "Audio/Sink"), node(7, "physical_mic", "Audio/Source"),
                   node(8, "discord_capture", "Stream/Input/Audio", "vesktop", 42),
                   node(9, "other_profile", "Stream/Output/Audio", "vesktop", 44),
                   port(11, 1, "in"), port(12, 2, "out"), port(13, 3, "out"),
                   port(14, 4, "in"), port(15, 5, "out"), port(16, 6, "in"),
                   port(17, 7, "out"), port(18, 8, "in"), port(19, 9, "out")]
        original = {(13, 16), (15, 16), (17, 14), (17, 18), (19, 16)}
        edges = set(original)
        calls = []

        async def snapshot():
            return copy.deepcopy(objects) + [link(*edge) for edge in edges]

        async def command(*args):
            self.assertEqual(args[0], "pw-link")
            edge = tuple(map(int, args[-2:]))
            calls.append(args)
            saved = json.loads(journal.read_text())
            recorded = saved["original"] + saved["created"]
            self.assertIn(edge, [tuple(ref[0] for ref in item) for item in recorded])
            if "-d" in args:
                edges.remove(edge)
            else:
                edges.add(edge)
            return b""

        with patch.object(routing, "snapshot", snapshot), patch.object(routing, "command", command), \
                patch.object(routing, "process_matches", lambda pid, profile: pid == 42):
            temporary = tempfile.TemporaryDirectory()
            self.addCleanup(temporary.cleanup)
            journal = Path(temporary.name) / "routing.json"
            router = routing.Router("/profiles/lanalu", journal_path=journal)
            status = await router.reconcile()
            self.assertEqual(status, {"playback": 1, "capture": 1, "ready": True, "routed": 2})
            self.assertEqual(edges, {(13, 11), (12, 14), (15, 16), (17, 18), (19, 16)})
            count = len(calls)
            await router.reconcile()
            self.assertEqual(len(calls), count)
            # A fresh instance recovers the write-ahead journal after a killed process.
            router = routing.Router("/profiles/lanalu", journal_path=journal)
            await router.restore()
            self.assertEqual(edges, original)
            await router.reconcile()
            # The old mic node is replaced with another client using the same IDs.
            routing.props(objects[4])["object.serial"] += 100
            edges.discard((12, 14))
            edges.add((17, 14))
            count = len(calls)
            await router.restore()
            self.assertTrue(all("14" not in map(str, call[-2:]) for call in calls[count:]))
            self.assertIn((17, 14), edges)
            await router.reconcile()
            objects[0]["info"]["cookie"] = 456
            count = len(calls)
            await router.restore()
            self.assertEqual(len(calls), count)  # Restarted PipeWire may reuse all IDs.
            self.assertFalse(json.loads(journal.read_text())["original"])

    async def test_preserve_only_verified_recall_playback_tee(self):
        recorder = node(10, "nx-recall-capture", "Stream/Input/Audio")
        routing.props(recorder)["application.name"] = "nx-recall"
        impostor = node(20, "nx-recall-capture", "Stream/Input/Audio")
        routing.props(impostor)["application.name"] = "Discord"
        objects = [{"id": 0, "type": "PipeWire:Interface:Core", "info": {"cookie": 123}},
                   node(1, "lanalu_incoming", "Audio/Sink"),
                   node(2, "lanalu_microphone", "Audio/Source"),
                   node(3, "vesktop", "Stream/Output/Audio", "vesktop", 42),
                   node(6, "speaker", "Audio/Sink"), recorder, impostor,
                   port(11, 1, "in"), port(12, 2, "out"), port(13, 3, "out"),
                   port(16, 6, "in"), port(21, 10, "in"), port(22, 20, "in")]
        original = {(13, 16), (13, 21), (13, 22)}
        edges = set(original)
        changed = []
        async def snapshot():
            return copy.deepcopy(objects) + [link(*edge) for edge in edges]
        async def command(*args):
            edge = tuple(map(int, args[-2:]))
            changed.append(edge)
            if "-d" in args:
                edges.remove(edge)
            else:
                edges.add(edge)
            return b""
        with patch.object(routing, "snapshot", snapshot), patch.object(routing, "command", command), \
                patch.object(routing, "process_matches", lambda pid, profile: pid == 42):
            router = routing.Router("/profiles/lanalu")
            self.assertTrue((await router.reconcile())["ready"])
            self.assertEqual(edges, {(13, 11), (13, 21)})
            self.assertNotIn((13, 21), changed)
            # Recall replaces its tee port while the bridge is running.
            objects.append(port(23, 10, "in", "FL"))
            edges.remove((13, 21))
            edges.add((13, 23))
            self.assertTrue((await router.reconcile())["ready"])
            self.assertIn((13, 23), edges)
            self.assertNotIn((13, 23), changed)
            await router.restore()
            self.assertIn((13, 23), edges)
            self.assertIn((13, 16), edges)
            self.assertIn((13, 22), edges)

    async def test_missing_or_duplicate_device_fails_closed(self):
        objects = [{"id": 0, "type": "PipeWire:Interface:Core", "info": {"cookie": 123}},
                   node(1, "lanalu_incoming", "Audio/Sink"),
                   node(2, "lanalu_incoming", "Audio/Sink")]
        async def snapshot():
            return objects
        with patch.object(routing, "snapshot", snapshot), patch.object(routing, "command") as command:
            status = await routing.Router("/profiles/lanalu").reconcile()
            self.assertFalse(status["ready"])
            command.assert_not_called()

    def test_exact_profile_process_identity(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            process = root / "42"
            process.mkdir()
            executable = root / "vesktop"
            executable.touch()
            (process / "exe").symlink_to(executable)
            (process / "cmdline").write_bytes(b"/usr/lib/vesktop/vesktop --type=utility --user-data-dir=/profiles/lanalu\0")
            (process / "stat").write_text("42 (vesktop) S 1 0 0")
            with patch.object(routing, "Path", lambda value: root if value == "/proc" else Path(value)):
                self.assertTrue(routing.process_matches(42, "/profiles/lanalu"))
                self.assertFalse(routing.process_matches(42, "/profiles/lan"))
                (process / "cmdline").write_bytes(b"vesktop\0--user-data-dir\0/profiles/lanalu\0")
                self.assertTrue(routing.process_matches(42, "/profiles/lanalu"))
                (process / "exe").unlink()
                (root / "Discord").touch()
                (process / "exe").symlink_to(root / "Discord")
                self.assertFalse(routing.process_matches(42, "/profiles/lanalu"))

    def test_channel_mapping(self):
        self.assertEqual(routing.pair_ports({"FL": 1, "FR": 2}, {"FL": 3, "FR": 4}), {(1, 3), (2, 4)})
        self.assertEqual(routing.pair_ports({"MONO": 1}, {"FL": 3, "FR": 4}), {(1, 3), (1, 4)})
        self.assertEqual(routing.pair_ports({"FL": 1, "FR": 2}, {"MONO": 3}), {(1, 3)})
        self.assertFalse(routing.pair_ports({}, {"MONO": 3}))


if __name__ == "__main__":
    unittest.main()
