"""Real offline voice loop using only synthetic speech and private audio buses."""
import asyncio
from array import array
from pathlib import Path
from unittest.mock import patch

from nx_recall_voice.audio import Audio, Devices
from nx_recall_voice.local import run_local
from nx_recall_voice.speech import Speech


class TestMemory:
    def __init__(self, *args):
        pass

    async def retrieve(self, query, limit=4):
        return [{'text': 'The synthetic test project is called Bluebird.', 'source': 'test fixture', 'timestamp': None}]

    async def identify(self, *args, **kwargs):
        return None


async def main():
    config = dict(mode='wakeword', audio_mode='vesktop', wake_words=['lanalu', 'chat gpt'],
                  llama_binary=str(Path.home()/'.local/share/nx-recall/models/llama-voice/llama-server'),
                  llm_model=str(Path.home()/'.local/share/nx-recall/models/qwen2.5-3b-instruct-q4_k_m.gguf'))
    speech = Speech()
    question = await speech.synthesize('Chat GPT, what is the name of the test project?')
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
        events.append(event)
        if event == 'local_listening':
            if 'local_speaking' in events:
                replied.set()
            listening.set()
    try:
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
        with patch('nx_recall_voice.recall.RecallClient', TestMemory):
            worker = asyncio.create_task(run_local(config, audio, status))
            await asyncio.wait_for(listening.wait(), 30)
            class Input:
                playback_sink = devices.incoming
            feeder = Audio(Input())
            await feeder.write(question)
            await asyncio.wait_for(replied.wait(), 30)
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


if __name__ == '__main__':
    asyncio.run(main())
