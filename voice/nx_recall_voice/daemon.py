"""NX Recall's local voice worker, started and stopped by the desktop app."""
import argparse
import asyncio
import fcntl
import json
import logging
import os
from pathlib import Path
import signal
import time
import tomllib

from .audio import Audio, Devices, command
from .control import Control
from .local import run_local
from .routing import Router


def runtime():
    folder = Path(os.environ.get("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}")) / "nx-recall-voice"
    folder.mkdir(mode=0o700, parents=True, exist_ok=True)
    return folder


class Status:
    def __init__(self):
        self.data = {"pid": os.getpid(), "backend": "local", "network_enabled": False}

    def __call__(self, event, **fields):
        self.data.update(fields, event=event, updated_at=time.time())
        target = runtime() / "status.json"
        temporary = target.with_suffix(".tmp")
        temporary.write_text(json.dumps(self.data) + "\n")
        temporary.replace(target)
        logging.info("%s %s", event, json.dumps(fields))


def load_config(path):
    config = tomllib.loads(Path(path).read_text())
    if config.get("backend", "local") != "local":
        raise ValueError("NX Recall Voice supports local inference only")
    config.setdefault("mode", "wakeword")
    config.setdefault("audio_mode", "vesktop")
    config.setdefault("wake_words", ["lanalu", "lana lu", "lana lou", "chatgpt", "chat gpt"])
    config.setdefault("vesktop_profile", "~/.config/vesktop")
    config.setdefault("llama_binary", "~/.local/share/nx-recall/models/llama-voice/llama-server")
    config.setdefault("llm_model", "~/.local/share/nx-recall/models/qwen3.5-4b-q4_k_m.gguf")
    config.setdefault("instructions", "You are Lanalu, a warm, concise local voice assistant in NX Recall.")
    if config["mode"] not in ("wakeword", "always") or config["audio_mode"] not in ("local", "vesktop"):
        raise ValueError("Invalid voice or audio mode")
    if not isinstance(config["wake_words"], list) or not all(isinstance(w, str) and any(c.isalnum() for c in w) for w in config["wake_words"]) or not config["wake_words"]:
        raise ValueError("Wake phrases must be nonempty strings")
    config["vesktop_profile"] = str(Path(config["vesktop_profile"]).expanduser().resolve())
    return config


class LocalDevices:
    """Explicit device selection; never change a desktop default."""
    def __init__(self, config):
        self.config = config

    async def start(self):
        self.capture_source = self.config.get("local_source") or await command("pactl", "get-default-source")
        self.playback_sink = self.config.get("local_sink") or await command("pactl", "get-default-sink")
        sources = json.loads(await command("pactl", "-f", "json", "list", "sources"))
        sinks = json.loads(await command("pactl", "-f", "json", "list", "sinks"))
        if self.capture_source not in {s['name'] for s in sources if not s['name'].endswith('.monitor')}:
            raise ValueError("Selected microphone is unavailable")
        if self.playback_sink not in {s['name'] for s in sinks}:
            raise ValueError("Selected speaker is unavailable")

    async def close(self):
        pass


async def run(config, status=None, text_queue=None):
    status = status or Status()
    parent = os.getppid()
    attempt = 0
    while os.getppid() == parent:
        router = None
        audio = None
        task = None
        devices = LocalDevices(config) if config['audio_mode'] == 'local' else Devices(
            prefix='nx_recall_voice', recovery_path=runtime() / 'device-owner')
        try:
            if config['audio_mode'] == 'vesktop':
                router = Router(config['vesktop_profile'], incoming=devices.incoming,
                                microphone=devices.microphone, journal_path=runtime() / 'routes.json')
                await router.restore()
            await devices.start()
            audio = Audio(devices)
            status('waiting_for_vesktop_streams' if router else 'starting', audio_mode=config['audio_mode'], connected=False)
            previous = None
            while os.getppid() == parent:
                routes = await router.reconcile() if router else {'ready': True, 'playback': 1, 'capture': 1}
                ready = routes.get('ready') and routes.get('playback') and routes.get('capture')
                if previous != routes:
                    status('routing', routes=routes)
                    previous = routes
                if task is not None and task.done():
                    await task
                    raise RuntimeError('Voice worker stopped')
                if not task:
                    # Private buses stay available without a call, so typed turns can run.
                    await audio.start()
                    task = asyncio.create_task(run_local(config, audio, status, text_queue))
                if status.data.get('audio_ready') != bool(ready):
                    status('audio_ready' if ready else 'waiting_for_vesktop_streams', audio_ready=bool(ready))
                await asyncio.sleep(1)
        except asyncio.CancelledError:
            raise
        except Exception as exc:
            attempt += 1
            status('retrying', connected=False, error=type(exc).__name__, retry_seconds=min(30, 2 ** min(attempt, 5)))
        finally:
            if task:
                task.cancel()
                await asyncio.gather(task, return_exceptions=True)
            # One broken cleanup must not skip restoring the remaining owned resources
            # or escape the reconnect loop. Preserve the last failure for diagnostics.
            for component, cleanup in [('audio', audio.close if audio else None),
                                       ('routing', router.restore if router else None),
                                       ('devices', devices.close)]:
                if cleanup is None:
                    continue
                try:
                    await cleanup()
                except Exception as exc:
                    status('cleanup_failed', connected=False, cleanup_component=component,
                           cleanup_error=type(exc).__name__)
        await asyncio.sleep(min(30, 2 ** min(attempt, 5)))


async def serve(config):
    status = Status()
    queue = asyncio.Queue(maxsize=1)
    control = Control(runtime() / 'control.sock', queue, status)
    await control.start()
    task = asyncio.create_task(run(config, status, queue))
    for signum in (signal.SIGTERM, signal.SIGINT):
        asyncio.get_running_loop().add_signal_handler(signum, task.cancel)
    try:
        await task
    except asyncio.CancelledError:
        pass
    finally:
        await control.close()
        status('stopped', connected=False)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=['run'])
    parser.add_argument('--config', required=True)
    args = parser.parse_args()
    logging.basicConfig(level=logging.INFO, format='%(asctime)s %(message)s')
    with (runtime() / 'lock').open('w') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        asyncio.run(serve(load_config(args.config)))


if __name__ == '__main__':
    main()
