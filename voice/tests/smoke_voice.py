"""Real offline voice loop using only synthetic speech and private audio buses."""
import argparse
import asyncio
import json
import tempfile
import types
from array import array
from pathlib import Path
from unittest.mock import patch

from nx_recall_voice.audio import Audio, Devices
from nx_recall_voice.local import run_local, LocalModel
from nx_recall_voice.control import Control
from nx_recall_voice.speech import Speech


class TestMemory:
    def __init__(self, *args):
        pass

    async def retrieve(self, query, limit=4):
        return [{'text': 'The synthetic test project is called Bluebird.', 'source': 'test fixture', 'timestamp': None}]

    async def identify(self, *args, **kwargs):
        return None


async def main(tts_backend="piper"):
    config = dict(tts_backend=tts_backend, mode='wakeword', recognition_source='local', audio_mode='vesktop', wake_words=['lanalu', 'chat gpt', 'chatgpt'],
                  llama_binary=str(Path.home()/'.local/share/nx-recall/models/llama-voice/llama-server'),
                  llm_model=str(Path.home()/'.local/share/nx-recall/models/qwen3.5-4b-q4_k_m.gguf'))
    private = tempfile.TemporaryDirectory(prefix='nx-recall-voice-test-')
    class IsolatedModel(LocalModel):
        def __init__(self, config):
            super().__init__(config)
            self.socket = Path(private.name) / 'llama.sock'
    queue = asyncio.Queue(maxsize=1)
    control_status = types.SimpleNamespace(data={'connected': False})
    control = Control(Path(private.name) / 'control.sock', queue, control_status)
    speech = Speech()
    question = await speech.synthesize('Chat GPT, what is the name of the test project?')
    class ObservedSpeech(Speech):
        async def transcribe(self, pcm):
            text = await super().transcribe(pcm)
            print('Synthetic recognized input:', repr(text), flush=True)
            return text
    devices = Devices('nx_recall_voice_test')
    audio = Audio(devices)
    feeder = None
    recorder = None
    worker = None
    collector = None
    listening = asyncio.Event()
    replied = asyncio.Event()
    events = []
    samples = bytearray()
    def status(event, **fields):
        control_status.data.update(fields)
        events.append(event)
        if event == "local_turn_failed":
            print("Synthetic smoke turn failure:", fields, flush=True)
        if event == 'local_listening':
            if 'local_speaking' in events:
                replied.set()
            listening.set()
    try:
        await control.start()
        await devices.start()
        await audio.start()
        recorder = await asyncio.create_subprocess_exec(
            'pacat','--record','--raw','--device='+devices.microphone,
            '--rate=24000','--channels=1','--format=s16le','--client-name=nx-recall-voice-selftest',
            stdout=asyncio.subprocess.PIPE,stderr=asyncio.subprocess.DEVNULL)
        async def collect():
            while chunk := await recorder.stdout.read(4096):
                samples.extend(chunk)
                if len(samples) > 48000 * 45:
                    raise RuntimeError('Unexpected test audio length')
        collector = asyncio.create_task(collect())
        with patch('nx_recall_voice.recall.RecallClient', TestMemory), patch('nx_recall_voice.local.LocalModel', IsolatedModel), patch('nx_recall_voice.speech.Speech', ObservedSpeech):
            worker = asyncio.create_task(run_local(config, audio, status, queue))
            await asyncio.wait_for(listening.wait(), 30)
            # First turn uses only typed IPC: no incoming audio or wake phrase.
            reader, writer = await asyncio.open_unix_connection(control.path)
            try:
                writer.write(b'{"type":"text","text":"What is the name of the test project?"}\n')
                await writer.drain()
                assert json.loads(await asyncio.wait_for(reader.readline(), 3)) == {'ok': True}
                result = json.loads(await asyncio.wait_for(reader.readline(), 45))
                assert result.get('type') == 'reply' and 'bluebird' in result.get('text', '').lower(), result
                assert 'bluebird' not in json.dumps(control_status.data).lower()
            finally:
                writer.close()
                await writer.wait_closed()
            print('Offline synthetic typed IPC roundtrip passed; response stayed out of diagnostics.', flush=True)
            await asyncio.wait_for(replied.wait(), 30)
            replied.clear()
            class Input:
                playback_sink = devices.incoming
            feeder = Audio(Input())
            await feeder.write(question)
            try:
                await asyncio.wait_for(replied.wait(), 30)
            except TimeoutError:
                print('Synthetic voice timeout:', {'events': events, 'output_bytes': len(samples)}, flush=True)
                raise
        assert 'local_turn_failed' not in events, events
        peak = max(abs(x) for x in array('h', samples))
        assert peak > 1000, peak
        print('Offline synthetic voice roundtrip passed:', {'output_peak':peak,'events':events})
    finally:
        if worker:
            worker.cancel()
            await asyncio.gather(worker,return_exceptions=True)
        if collector:
            collector.cancel()
            await asyncio.gather(collector,return_exceptions=True)
        if feeder:
            await feeder.close()
        await Audio.stop_process(recorder)
        await audio.close()
        await devices.close()
        await control.close()
        private.cleanup()


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument("--tts-backend", choices=("piper", "kokoro"), default="piper")
    asyncio.run(main(parser.parse_args().tts_backend))
