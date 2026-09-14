"""Opt-in synthetic PCM transport test; uses only uniquely named private buses.

Run with PYTHONPATH=voice python voice/tests/smoke_audio.py.
Never routes an existing application or opens a hardware input/output.
"""
import asyncio
import json
import math
import uuid
from array import array

from nx_recall_voice.audio import Audio, Devices, command


async def main():
    defaults = (await command("pactl", "get-default-sink"),
                await command("pactl", "get-default-source"))
    devices = Devices("nx_recall_audio_test_" + uuid.uuid4().hex[:12])
    audio = Audio(devices)
    processes, collectors = [], []
    incoming, microphone = bytearray(), bytearray()

    async def collect(process, target):
        while chunk := await process.stdout.read(4096):
            target.extend(chunk)
            if len(target) > 48000 * 30:
                raise RuntimeError("Private test capture exceeded its bound")

    async def process(record, target, channels=1):
        proc = await asyncio.create_subprocess_exec(
            "pacat", "--record" if record else "--playback", "--raw",
            "--device=" + target, "--rate=24000", "--channels=" + str(channels),
            "--format=s16le", "--latency-msec=40", "--process-time-msec=20",
            "--client-name=nx-recall-private-audio-test",
            "--property=node.dont-fallback=true", "--property=node.dont-reconnect=true",
            stdin=asyncio.subprocess.DEVNULL if record else asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE if record else asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.DEVNULL)
        processes.append(proc)
        return proc

    def peak(data):
        samples = array("h", data[:len(data) // 2 * 2])
        return max(map(abs, samples), default=0)

    def tone(channels, active_channel=0):
        samples = array("h")
        for frame in range(24000 // 2):
            value = round(12000 * math.sin(2 * math.pi * 440 * frame / 24000))
            samples.extend(value if channel == active_channel else 0 for channel in range(channels))
        return samples.tobytes()

    async def feed(proc, data, channels):
        size = 960 * channels
        for offset in range(0, len(data), size):
            proc.stdin.write(data[offset:offset + size])
            await asyncio.wait_for(proc.stdin.drain(), 3)
            await asyncio.sleep(.02)

    async def sample(label, writer):
        # Drain previous tone plus server buffering before starting a new window.
        await asyncio.sleep(.45)
        incoming.clear()
        microphone.clear()
        await writer()
        await asyncio.sleep(.25)
        for collector in collectors:
            if collector.done():
                await collector
                raise RuntimeError("Private capture stopped")
        result = {"incoming_peak": peak(incoming), "microphone_peak": peak(microphone),
                  "incoming_bytes": len(incoming), "microphone_bytes": len(microphone)}
        assert min(len(incoming), len(microphone)) >= 4800, (label, result)
        print(json.dumps({label: result}), flush=True)
        return result

    try:
        await devices.start()
        graph = json.loads(await command("pw-dump"))
        nodes = {item.get("info", {}).get("props", {}).get("node.name"):
                 item.get("info", {}).get("props", {}) for item in graph
                 if item.get("type", "").endswith(":Node")}
        descriptions = {
            devices.incoming: "NX Recall - Call audio to Lanalu",
            devices.outgoing: "NX Recall - Internal voice bus",
            devices.microphone: "NX Recall - Lanalu microphone",
        }
        pulse_devices = (json.loads(await command("pactl", "-f", "json", "list", "sinks")) +
                         json.loads(await command("pactl", "-f", "json", "list", "sources")))
        pulse_descriptions = {device["name"]: device["description"] for device in pulse_devices}
        for name, description in descriptions.items():
            properties = nodes[name]
            assert properties.get("node.description") == description, (name, properties)
            assert pulse_descriptions.get(name) == description, (name, pulse_descriptions.get(name))
            assert str(properties.get("priority.session")) == "0", (name, properties)
            assert properties.get("lanalu.bridge.owner") == devices.owner, (name, properties)
            if name != devices.microphone:
                assert str(properties.get("priority.driver")) == "1", (name, properties)
                assert properties.get("node.always-process") in (True, "true"), (name, properties)
        print("Private device labels, owner, default priority and sink clock properties verified.", flush=True)
        await audio.start()
        mic = await process(True, devices.microphone)
        collectors.extend([asyncio.create_task(collect(audio.capture, incoming)),
                           asyncio.create_task(collect(mic, microphone))])
        stereo = await process(False, devices.incoming, 2)
        # Prime the private playback connection with silence before measurements.
        await feed(stereo, bytes(48000 * 2 // 2), 2)
        results = {}
        for channel, label in enumerate(("incoming_left", "incoming_right")):
            result = await sample(label, lambda channel=channel: feed(stereo, tone(2, channel), 2))
            assert result["incoming_peak"] > 1000, (label, "incoming channel missing", result)
            assert result["microphone_peak"] <= 8, (label, "incoming leaked to microphone", result)
            results[label] = result
        await Audio.stop_process(stereo)
        result = await sample("generated_voice", lambda: audio.write(tone(1)))
        assert result["microphone_peak"] > 1000, ("virtual microphone silent", result)
        assert result["incoming_peak"] <= 8, ("generated speech leaked into recognizer input", result)
        results["generated_voice"] = result
        await audio.clear()
        # Recreate playback after interruption/idle to prove the bus stays clocked.
        result = await sample("generated_voice_after_clear", lambda: audio.write(tone(1)))
        assert result["microphone_peak"] > 1000 and result["incoming_peak"] <= 8, result
        results["generated_voice_after_clear"] = result
    finally:
        for collector in collectors:
            collector.cancel()
        await asyncio.gather(*collectors, return_exceptions=True)
        for proc in processes:
            await Audio.stop_process(proc)
        try:
            await audio.close()
        finally:
            await devices.close()
        after = (await command("pactl", "get-default-sink"),
                 await command("pactl", "get-default-source"))
        assert after == defaults, "Desktop defaults changed"
        graph = json.loads(await command("pw-dump"))
        names = {item.get("info", {}).get("props", {}).get("node.name") for item in graph}
        assert not names.intersection((devices.incoming, devices.outgoing, devices.microphone)), "Private devices remain"
    print("PASS: both input channels, virtual microphone, echo isolation, playback restart; defaults preserved and private devices removed.")


if __name__ == "__main__":
    asyncio.run(asyncio.wait_for(main(), 30))
