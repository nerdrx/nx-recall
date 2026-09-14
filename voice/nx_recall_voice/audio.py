"""Private PipeWire-Pulse buses and bounded, paced PCM subprocess transport."""
import asyncio
import json
import time
from pathlib import Path
import uuid


async def command(*args):
    proc = await asyncio.create_subprocess_exec(*args, stdout=asyncio.subprocess.PIPE,
                                                stderr=asyncio.subprocess.PIPE)
    try:
        out, err = await asyncio.wait_for(proc.communicate(), 8)
    except BaseException:
        if proc.returncode is None:
            proc.kill()
        await proc.wait()
        raise
    if proc.returncode:
        raise RuntimeError(f"{args[0]} {args[1]} failed ({proc.returncode})")
    return out.decode().strip()


class Devices:
    def __init__(self, prefix="lanalu", recovery_path=None):
        self.incoming = prefix + "_incoming"
        self.outgoing = prefix + "_outgoing"
        self.microphone = prefix + "_microphone"
        self.modules = []
        self.refs = []
        self.recovery_path = Path(recovery_path) if recovery_path else None
        if self.recovery_path and self.recovery_path.exists():
            self.owner = str(uuid.UUID(self.recovery_path.read_text().strip()))
        else:
            self.owner = str(uuid.uuid4())
            if self.recovery_path:
                self.recovery_path.write_text(self.owner)
        self.refs_path = self.recovery_path.with_suffix(".json") if self.recovery_path else None
        if self.refs_path and self.refs_path.exists():
            self.refs = json.loads(self.refs_path.read_text())

    async def save_refs(self):
        graph = json.loads(await command("pw-dump"))
        cookie = graph[0].get("info", {}).get("cookie")
        for obj in graph:
            p = obj.get("info", {}).get("props", {})
            if p.get("node.name") in (self.incoming, self.outgoing, self.microphone) and str(p.get("pulse.module.id")) in self.modules:
                self.refs.append([cookie, p['node.name'], p['object.serial'], p['pulse.module.id']])
        if self.refs_path:
            temp = self.refs_path.with_suffix(".tmp")
            temp.write_text(json.dumps(self.refs))
            temp.replace(self.refs_path)

    async def recover(self):
        graph = json.loads(await command("pw-dump"))
        cookie = graph[0].get("info", {}).get("cookie")
        owned = []
        for obj in graph:
            p = obj.get("info", {}).get("props", {})
            ref = [cookie, p.get('node.name'), p.get('object.serial'), p.get('pulse.module.id')]
            if (p.get("lanalu.bridge.owner") == self.owner or ref in self.refs) and "pulse.module.id" in p:
                owned.append(str(p["pulse.module.id"]))
        for module in reversed(list(dict.fromkeys(owned))):
            await command("pactl", "unload-module", module)
        self.refs.clear()
        if self.refs_path:
            self.refs_path.unlink(missing_ok=True)

    async def start(self):
        await self.recover()
        # Never adopt or unload an existing device with an unverified owner.
        sinks = json.loads(await command("pactl", "-f", "json", "list", "sinks"))
        sources = json.loads(await command("pactl", "-f", "json", "list", "sources"))
        names = {d["name"] for d in sinks + sources}
        if names & {self.incoming, self.outgoing, self.microphone}:
            raise RuntimeError("Lanalu device name already exists; stop the other bridge first")
        try:
            for name, label in [(self.incoming, "NX_Recall_Voice_Incoming"),
                                (self.outgoing, "NX_Recall_Voice_Output")]:
                stereo = name == self.incoming
                self.modules.append(await command(
                    "pactl", "load-module", "module-null-sink", f"sink_name={name}",
                    "rate=24000", "channels=2" if stereo else "channels=1",
                    "channel_map=front-left,front-right" if stereo else "channel_map=mono",
                    # A zero driver priority leaves an isolated bus unclocked.
                    # Keep it below hardware drivers, with default-device priority zero.
                    # Keep owned buses clocked across playback teardown and idle gaps.
                    f"sink_properties=device.description={label} priority.session=0 priority.driver=1 node.always-process=true lanalu.bridge.owner={self.owner}"))
                await self.save_refs()
            self.modules.append(await command(
                "pactl", "load-module", "module-remap-source",
                f"master={self.outgoing}.monitor", f"source_name={self.microphone}",
                "channels=1", "channel_map=mono",
                f"source_properties=device.description=NX_Recall_Voice_Microphone priority.session=0 lanalu.bridge.owner={self.owner}"))
            await self.save_refs()
        except BaseException:
            await self.close()
            raise

    async def close(self):
        # Identify ownership again: module IDs can be reused after server restart.
        await self.recover()
        self.modules.clear()


class Audio:
    def __init__(self, devices):
        self.devices = devices
        self.capture = None
        self.playback = None
        self.epoch = 0
        self.sent = 0
        self.started = None
        self.lock = asyncio.Lock()

    async def process(self, record):
        target = (self.devices.capture_source if hasattr(self.devices, 'capture_source') else self.devices.incoming + '.monitor') if record else (self.devices.playback_sink if hasattr(self.devices, 'playback_sink') else self.devices.outgoing)
        return await asyncio.create_subprocess_exec(
            "pacat", "--record" if record else "--playback", "--raw",
            "--device=" + target, "--rate=24000", "--channels=1", "--format=s16le",
            "--latency-msec=40", "--process-time-msec=20",
            "--client-name=nx-recall-voice", "--stream-name=lanalu-capture" if record else "--stream-name=lanalu-speech",
            "--property=node.dont-fallback=true", "--property=node.dont-reconnect=true",
            stdin=asyncio.subprocess.DEVNULL if record else asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE if record else asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.DEVNULL)

    async def start(self):
        await self.stop_process(self.capture)
        self.capture = await self.process(True)

    async def read(self):
        return await self.capture.stdout.readexactly(960)  # 20ms, mono PCM16 @ 24kHz

    def begin_item(self, item_id):
        self.sent = 0
        self.started = None

    async def write(self, data):
        if len(data) % 2:
            raise ValueError("Odd PCM16 byte count")
        epoch = self.epoch
        for offset in range(0, len(data), 960):
            async with self.lock:
                if epoch != self.epoch:
                    return
                if self.playback is None:
                    self.playback = await self.process(False)
                if self.playback.returncode is not None:
                    raise RuntimeError("Audio playback disconnected")
                part = data[offset:offset + 960]
                self.playback.stdin.write(part)
                await asyncio.wait_for(self.playback.stdin.drain(), 3)
                if self.started is None:
                    self.started = time.monotonic()
                self.sent += len(part)
            await asyncio.sleep(len(part) / 48000)

    @staticmethod
    async def stop_process(proc):
        if proc and proc.returncode is None:
            try:
                proc.terminate()
            except ProcessLookupError:
                await proc.wait()
                return
            try:
                await asyncio.wait_for(proc.wait(), 2)
            except asyncio.TimeoutError:
                try:
                    proc.kill()
                except ProcessLookupError:
                    pass
                await proc.wait()

    async def clear(self):
        async with self.lock:
            self.epoch += 1
            elapsed = 0 if self.started is None else max(0, int((time.monotonic() - self.started) * 1000) - 80)
            played = min(elapsed, self.sent // 48)
            await self.stop_process(self.playback)
            self.playback = None
            self.started = None
            self.sent = 0
            return played

    async def close(self):
        try:
            await self.clear()
        finally:
            await self.stop_process(self.capture)
