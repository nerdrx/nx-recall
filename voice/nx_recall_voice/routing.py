"""Explicit PipeWire links; never alter Pulse stream-restore/default devices."""
import asyncio
import json
import logging
import os
from pathlib import Path
import shlex
import tempfile

LOG = logging.getLogger(__name__)


async def command(*args):
    proc = await asyncio.create_subprocess_exec(*map(str, args), stdout=asyncio.subprocess.PIPE,
                                                stderr=asyncio.subprocess.PIPE)
    try:
        out, err = await asyncio.wait_for(proc.communicate(), 8)
    except BaseException:
        if proc.returncode is None:
            proc.kill()
        await proc.wait()
        raise
    if proc.returncode:
        raise RuntimeError(f"{args[0]} failed: {err.decode(errors='replace').strip()[:300]}")
    return out


async def snapshot():
    return json.loads(await command("pw-dump"))


def props(obj):
    return obj.get("info", {}).get("props", {})


def process_matches(pid, profile):
    """Require a same-user Vesktop process and an exact profile argument."""
    try:
        for _ in range(12):
            path = Path("/proc") / str(int(pid))
            if path.stat().st_uid != os.getuid() or (path / "exe").resolve().name != "vesktop":
                return False
            args = [a.decode() for a in (path / "cmdline").read_bytes().split(b"\0") if a]
            # Chromium sets its process title to a single command-line string.
            if len(args) == 1:
                args = shlex.split(args[0])
            profiles = []
            for i, arg in enumerate(args):
                if arg.startswith("--user-data-dir="):
                    profiles.append(arg.split("=", 1)[1])
                elif arg == "--user-data-dir" and i + 1 < len(args):
                    profiles.append(args[i + 1])
            if profiles:
                return len(profiles) == 1 and profiles[0] == profile
            stat = (path / "stat").read_text().rsplit(")", 1)[1].split()
            pid = int(stat[1])
        return False
    except (OSError, ValueError, UnicodeError):
        return False


class Graph:
    def __init__(self, objects):
        self.objects = {o["id"]: o for o in objects}
        self.cookie = next((o.get("info", {}).get("cookie") for o in objects
                            if o.get("type", "").endswith(":Core")), None)
        self.nodes = {i: o for i, o in self.objects.items() if o.get("type", "").endswith(":Node")}
        self.ports = {i: o for i, o in self.objects.items() if o.get("type", "").endswith(":Port")}
        self.links = set()
        for o in objects:
            if o.get("type", "").endswith(":Link"):
                info = o.get("info", {})
                if "output-port-id" in info and "input-port-id" in info:
                    self.links.add((info["output-port-id"], info["input-port-id"]))

    def ref(self, port):
        p = props(self.ports[port])
        node = int(p["node.id"])
        return (port, str(p["object.serial"]), node, str(props(self.nodes[node])["object.serial"]))

    def valid(self, ref):
        try:
            return self.ref(ref[0]) == ref
        except (KeyError, ValueError, TypeError):
            return False

    def recall_tee(self, pair):
        """Existing one-way Recall recording is allowed beside the bridge sink."""
        port = self.ports.get(pair[1])
        if not port or props(port).get("port.direction") != "in":
            return False
        try:
            node = self.nodes[int(props(port)["node.id"])]
        except (KeyError, ValueError, TypeError):
            return False
        p = props(node)
        return (p.get("node.name") == "nx-recall-capture"
                and p.get("application.name") == "nx-recall"
                and p.get("media.class") == "Stream/Input/Audio")

    def channels(self, node, direction):
        result = {}
        for i, o in self.ports.items():
            p = props(o)
            if str(p.get("node.id")) != str(node) or p.get("port.direction") != direction:
                continue
            if p.get("port.monitor") in (True, "true"):
                continue
            channel = p.get("audio.channel")
            if channel not in ("MONO", "FL", "FR") or channel in result:
                return {}  # Unknown/multiple ports must never be guessed.
            result[channel] = i
        return result


def pair_ports(output, inputs):
    if not output or not inputs:
        return set()
    if output.keys() == inputs.keys():
        return {(output[c], inputs[c]) for c in output}
    if set(output) == {"MONO"}:
        return {(output["MONO"], p) for p in inputs.values()}
    if set(inputs) == {"MONO"} and "FL" in output:
        # Select left rather than summing two full-volume links (+6 dB/clipping).
        return {(output["FL"], inputs["MONO"])}
    return set()


class Router:
    def __init__(self, profile, incoming="lanalu_incoming", microphone="lanalu_microphone", journal_path=None):
        self.profile = str(Path(profile).expanduser().absolute())
        self.incoming = incoming
        self.microphone = microphone
        self.cookie = None
        self.original = set()
        self.created = set()
        self.lock = asyncio.Lock()
        self.journal_path = Path(journal_path).expanduser() if journal_path is not None else None
        if self.journal_path is not None and self.journal_path.exists():
            try:
                data = json.loads(self.journal_path.read_text())
                if data.get("version") != 1 or data.get("profile") != self.profile:
                    raise ValueError("journal version or profile mismatch")
                self.cookie = data["cookie"]
                if not isinstance(self.cookie, int):
                    raise ValueError("invalid core cookie")
                for key in ("original", "created"):
                    edges = set()
                    for edge in data[key]:
                        if len(edge) != 2 or any(len(ref) != 4 or
                                not isinstance(ref[0], int) or not isinstance(ref[2], int) or
                                not isinstance(ref[1], str) or not isinstance(ref[3], str) for ref in edge):
                            raise ValueError("invalid journal reference")
                        edges.add(tuple(tuple(ref) for ref in edge))
                    setattr(self, key, edges)
            except (OSError, ValueError, KeyError, TypeError) as exc:
                raise RuntimeError("Cannot safely load routing journal") from exc

    def _save(self):
        if self.journal_path is None:
            return
        path = self.journal_path
        path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        temporary = None
        try:
            with tempfile.NamedTemporaryFile("w", dir=path.parent, prefix=".routing-", delete=False) as out:
                temporary = out.name
                os.chmod(temporary, 0o600)
                json.dump({"version": 1, "profile": self.profile, "cookie": self.cookie,
                           "original": sorted(self.original), "created": sorted(self.created)}, out)
                out.flush()
                os.fsync(out.fileno())
            os.replace(temporary, path)
        finally:
            if temporary and os.path.exists(temporary):
                os.unlink(temporary)

    def _bind_graph(self, graph):
        if graph.cookie is None:
            raise RuntimeError("PipeWire core identity missing; refusing graph changes")
        if self.cookie is not None and self.cookie != graph.cookie:
            # Object serials are scoped to one PipeWire daemon lifetime.
            self.original.clear()
            self.created.clear()
        self.cookie = graph.cookie
        self._save()

    def selected(self, graph):
        result = {"playback": [], "capture": []}
        for i, node in graph.nodes.items():
            p = props(node)
            kind = {"Stream/Output/Audio": "playback", "Stream/Input/Audio": "capture"}.get(p.get("media.class"))
            if not kind or p.get("node.name") == "discord_capture":
                continue
            if p.get("application.process.binary") != "vesktop":
                continue
            if process_matches(p.get("application.process.id"), self.profile):
                result[kind].append(i)
        return result

    async def recognition_binding(self):
        """Prove Recall and our VAD receive the same exact profile stream.

        Read-only, with serials so reconnects/reused IDs cannot preserve a stale
        proof. The opaque result is compared at onset and during transcription.
        """
        graph = Graph(await snapshot())
        selected = self.selected(graph)["playback"]
        sinks = [i for i, obj in graph.nodes.items()
                 if props(obj).get("node.name") == self.incoming
                 and props(obj).get("media.class") == "Audio/Sink"]
        if graph.cookie is None or len(selected) != 1 or len(sinks) != 1:
            return None
        outputs = set(graph.channels(selected[0], "out").values())
        inputs = set(graph.channels(sinks[0], "in").values())
        if not outputs or not inputs:
            return None
        proof = []
        try:
            for output in sorted(outputs):
                tees = sorted(edge for edge in graph.links if edge[0] == output and graph.recall_tee(edge))
                incoming = sorted(edge for edge in graph.links if edge[0] == output and edge[1] in inputs)
                if not tees or not incoming:
                    return None
                proof.append((graph.ref(output), tuple(graph.ref(edge[1]) for edge in tees),
                              tuple(graph.ref(edge[1]) for edge in incoming)))
            return (graph.cookie, str(props(graph.nodes[selected[0]])["object.serial"]), tuple(proof))
        except (KeyError, ValueError, TypeError):
            return None

    async def _change(self, edge, disconnect=False, journal=None):
        graph = Graph(await snapshot())
        if graph.cookie != self.cookie or not all(graph.valid(ref) for ref in edge):
            return False
        pair = tuple(ref[0] for ref in edge)
        if (pair in graph.links) == (not disconnect):
            return False
        # Journal before the command: cancellation may arrive after PipeWire applied it.
        if journal is not None:
            journal.add(edge)
            self._save()
        await command("pw-link", *(("-d",) if disconnect else ("-L",)), *pair)
        return True

    async def reconcile(self):
        async with self.lock:
            graph = Graph(await snapshot())
            self._bind_graph(graph)
            selected = self.selected(graph)
            status = {k: len(v) for k, v in selected.items()}
            devices = {}
            for name, cls in ((self.incoming, "Audio/Sink"), (self.microphone, "Audio/Source")):
                matches = [i for i, o in graph.nodes.items() if props(o).get("node.name") == name
                           and props(o).get("media.class") == cls]
                if len(matches) != 1:
                    return dict(status, ready=False, reason=f"Missing or ambiguous device: {name}")
                devices[name] = matches[0]
            # Drop stale journal entries; IDs may have been recycled since the last call.
            self.original = {e for e in self.original if all(graph.valid(r) for r in e)}
            self.created = {e for e in self.created if all(graph.valid(r) for r in e)}
            self._save()
            routed = 0
            planned = []
            for kind, nodes in selected.items():
                for node in nodes:
                    if kind == "playback":
                        a, b = graph.channels(node, "out"), graph.channels(devices[self.incoming], "in")
                        affected = lambda edge: edge[0] in a.values()
                    else:
                        a, b = graph.channels(devices[self.microphone], "out"), graph.channels(node, "in")
                        affected = lambda edge: edge[1] in b.values()
                    wanted = pair_ports(a, b)
                    if not wanted:
                        continue
                    # Preserve Recall's recording tee; remove hardware/other alternate paths.
                    for pair in graph.links - wanted:
                        if affected(pair) and not (kind == "playback" and graph.recall_tee(pair)):
                            edge = tuple(graph.ref(p) for p in pair)
                            journal = self.original if edge not in self.created else None
                            await self._change(edge, disconnect=True, journal=journal)
                    for pair in wanted:
                        edge = tuple(graph.ref(p) for p in pair)
                        await self._change(edge, journal=self.created)
                    planned.append((kind, {tuple(graph.ref(p) for p in pair) for pair in wanted}))
            final = Graph(await snapshot())
            for kind, wanted in planned:
                if not all(final.valid(ref) for edge in wanted for ref in edge):
                    continue
                pairs = {tuple(ref[0] for ref in edge) for edge in wanted}
                side = 0 if kind == "playback" else 1
                ports = {pair[side] for pair in pairs}
                actual = {pair for pair in final.links if pair[side] in ports
                          and not (kind == "playback" and final.recall_tee(pair))}
                routed += actual == pairs
            return dict(status, ready=routed == sum(status.values()) and routed > 0, routed=routed)

    async def restore(self):
        async with self.lock:
            self._bind_graph(Graph(await snapshot()))
            errors = []
            for edge in tuple(self.created):
                try:
                    await self._change(edge, disconnect=True)
                    self.created.discard(edge)
                    self._save()
                except RuntimeError as exc:
                    errors.append(str(exc))
            for edge in tuple(self.original):
                try:
                    await self._change(edge)
                    self.original.discard(edge)
                    self._save()
                except RuntimeError as exc:
                    errors.append(str(exc))
            if errors:
                raise RuntimeError("Route restore incomplete: " + "; ".join(errors))
