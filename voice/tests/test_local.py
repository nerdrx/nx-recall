import asyncio
from array import array
import contextlib
import sys
import types
import unittest
from unittest.mock import patch

from nx_recall_voice import local, recall

VOICE = array("h", [2000] * 480).tobytes()
SILENCE = bytes(960)


class FakeAudio:
    capture = None
    def __init__(self):
        self.devices = types.SimpleNamespace(capture_source="test-mic")
        self.frames = asyncio.Queue()
        self.started = asyncio.Event()
        self.playing = asyncio.Event()
        self.release = asyncio.Event()
        self.clears = 0
        self.reads = 0
        self.write_cancelled = False
    async def recognition_binding(self):
        return ('private-test-binding',)
    async def start(self):
        self.started.set()
    async def read(self):
        frame = await self.frames.get()
        self.reads += 1
        return frame
    def begin_item(self, item):
        pass
    async def write(self, pcm):
        self.playing.set()
        try:
            await self.release.wait()
        except asyncio.CancelledError:
            self.write_cancelled = True
            raise
    async def clear(self):
        self.clears += 1


class VoiceTests(unittest.IsolatedAsyncioTestCase):
    async def start_worker(self, mode="vesktop", texts=None, identity_error=False, trigger="wakeword", recognition="local", recognition_error=None):
        self.text_queue = asyncio.Queue(maxsize=1)
        self.audio = FakeAudio()
        self.statuses = asyncio.Queue()
        self.shared_requests = []
        self.transcriptions = 0
        self.synthesis_finished = False
        self.synthesis_release = asyncio.Event()
        self.synthesis_release.set()
        self.model_gate = asyncio.Event()
        self.model_gate.set()
        self.model_messages = []
        self.model_closed = False
        pending = list(texts or ["Lanalu say hello"])
        test = self
        class Speech:
            def __init__(self, config):
                pass
            async def warmup(self):
                pass
            async def transcribe(self, pcm):
                test.transcriptions += 1
                return pending.pop(0)
            async def synthesize_stream(self, text):
                yield VOICE
                await test.synthesis_release.wait()
                test.synthesis_finished = True
        class Model:
            process = types.SimpleNamespace(returncode=None)
            def __init__(self, config):
                pass
            async def start(self):
                pass
            async def answer(self, messages):
                await test.model_gate.wait()
                test.model_messages.append(messages)
                return "Hello."
            async def close(self):
                test.model_closed = True
        class Recall:
            def __init__(self, path):
                pass
            async def retrieve(self, text, limit):
                return []
            async def recognize(self, *args, **kwargs):
                test.shared_requests.append((args, kwargs))
                if recognition_error:
                    raise recall.RecognitionUnavailable(recognition_error)
                if kwargs.get('source') == 'vesktop' and not await kwargs['input_guard']():
                    raise recall.RecognitionUnavailable('input_mismatch')
                return {'text': pending.pop(0), 'segment_ids': [4], 'wait_ms': 200}
            async def identify(self, *args, **kwargs):
                if identity_error:
                    raise RuntimeError("synthetic unavailable")
                return None
        for patcher in [patch.object(local, "LocalModel", Model),
                        patch.object(recall, "RecallClient", Recall),
                        patch.dict(sys.modules, {"nx_recall_voice.speech": types.SimpleNamespace(Speech=Speech)})]:
            patcher.start()
            self.addCleanup(patcher.stop)
        config = {"audio_mode": mode, "mode": trigger, "recognition_source": recognition, "wake_words": ["lanalu"], "silence_duration_ms": 40}
        self.heard = []
        def status(event, **fields):
            self.statuses.put_nowait((event, fields))
        status.record_heard = lambda *args: self.heard.append(args)
        self.worker = asyncio.create_task(local.run_local(config, self.audio, status, self.text_queue))
        self.addAsyncCleanup(self.stop_worker)
        await self.wait_status("local_listening")

    async def stop_worker(self):
        self.worker.cancel()
        await asyncio.gather(self.worker, return_exceptions=True)

    async def wait_status(self, target):
        async with asyncio.timeout(1):
            while True:
                event, fields = await self.statuses.get()
                if event == target:
                    return fields

    async def utterance(self):
        for pcm in [VOICE] * 3 + [SILENCE] * 2:
            self.audio.frames.put_nowait(pcm)

    async def test_wake_gate_and_recall_failure_falls_back_unknown(self):
        await self.start_worker(texts=["ordinary conversation", "Lanalu say hello"], identity_error=True)
        await self.utterance()
        await self.wait_status("wake_word_not_detected")
        self.assertFalse(self.model_messages)
        self.assertEqual(self.heard, [("ordinary conversation", "local", False, "wake_name_missing")])
        await self.utterance()
        await self.wait_status("recall_identity_unavailable")
        await self.wait_status("local_speaking")
        self.assertIn("speaker is unknown", self.model_messages[0][0]["content"])
        self.assertEqual(self.transcriptions, 2)

    async def test_heard_keeps_empty_voice_but_not_typed_input(self):
        await self.start_worker(texts=[""])
        await self.utterance()
        async with asyncio.timeout(1):
            while not self.heard:
                await asyncio.sleep(.01)
        self.assertEqual(self.heard, [("", "local", False, "no_words")])
        await self.text_queue.put("private typed text")
        await self.wait_status("local_speaking")
        self.assertEqual(len(self.heard), 1)

    async def test_always_mode_accepts_speech_without_wake_phrase(self):
        await self.start_worker(texts=["ordinary conversation"], trigger="always")
        await self.utterance()
        await self.wait_status("local_speaking")
        self.assertEqual(len(self.model_messages), 1)

    async def test_vesktop_barge_in_cancels_playback(self):
        await self.start_worker()
        await self.utterance()
        await self.wait_status("local_speaking")
        for _ in range(3):
            self.audio.frames.put_nowait(VOICE)
        await self.wait_status("interrupted")
        self.assertTrue(self.audio.write_cancelled)
        self.assertEqual(self.audio.clears, 1)
        await self.stop_worker()
        self.assertTrue(self.model_closed)

    async def test_local_speakers_ignore_tts_echo(self):
        await self.start_worker(mode="local")
        await self.utterance()
        await self.wait_status("local_speaking")
        await self.utterance()
        async with asyncio.timeout(1):
            while self.audio.reads < 10:
                await asyncio.sleep(0)
        self.assertFalse(self.audio.write_cancelled)
        self.assertEqual(self.transcriptions, 1)
        self.audio.release.set()
        await self.wait_status("local_listening")
        await self.utterance()  # Post-playback echo tail must also be ignored.
        async with asyncio.timeout(1):
            while self.audio.reads < 15:
                await asyncio.sleep(0)
        self.assertEqual(self.transcriptions, 1)

    async def test_typed_turn_without_audio_bypasses_wake_and_stt(self):
        await self.start_worker()
        self.text_queue.put_nowait("What do you remember?")
        fields = await self.wait_status("local_thinking")
        await self.wait_status("local_speaking")
        self.assertEqual(fields['input_kind'], 'text')
        self.assertEqual(self.transcriptions, 0)
        self.assertEqual(self.model_messages[0][-1]['content'], 'What do you remember?')
        self.assertIn('speaker is unknown', self.model_messages[0][0]['content'])
        self.assertNotIn('What do you remember?', str(fields))

    async def test_typed_reply_arrives_before_blocked_audio_playback(self):
        await self.start_worker()
        response = asyncio.get_running_loop().create_future()
        self.text_queue.put_nowait(local.TextTurn('private question', response))
        reply = await asyncio.wait_for(response, 1)
        self.assertEqual(reply, {'type': 'reply', 'text': 'Hello.'})
        self.assertFalse(self.audio.release.is_set())

    async def test_visible_typed_reply_stays_in_history_when_playback_interrupted(self):
        await self.start_worker()
        response = asyncio.get_running_loop().create_future()
        self.text_queue.put_nowait(local.TextTurn('first question', response))
        await asyncio.wait_for(response, 1)
        self.text_queue.put_nowait('follow up')
        async with asyncio.timeout(1):
            while len(self.model_messages) < 2:
                await asyncio.sleep(0)
        self.assertEqual(self.model_messages[1][-3:], [
            {'role': 'user', 'content': 'first question'},
            {'role': 'assistant', 'content': 'Hello.'},
            {'role': 'user', 'content': 'follow up'}])

    async def test_typed_turn_interrupts_spoken_reply(self):
        await self.start_worker()
        await self.utterance()
        await self.wait_status("local_speaking")
        self.text_queue.put_nowait("Another question")
        await self.wait_status("local_thinking")
        self.assertTrue(self.audio.write_cancelled)
        self.assertEqual(self.audio.clears, 1)

    async def test_typed_interruption_ignores_local_speaker_echo_tail(self):
        await self.start_worker(mode='local')
        await self.utterance()
        await self.wait_status('local_speaking')
        self.model_gate.clear()
        self.text_queue.put_nowait('next question')
        await self.wait_status('local_thinking')
        for _ in range(3):
            self.audio.frames.put_nowait(VOICE)
        async with asyncio.timeout(1):
            while self.audio.reads < 8:
                await asyncio.sleep(0)
        self.assertEqual(self.audio.clears, 1)
        self.assertEqual(self.transcriptions, 1)

    async def test_first_audio_plays_before_synthesis_finishes(self):
        await self.start_worker()
        self.synthesis_release.clear()
        await self.utterance()
        await self.wait_status('local_speaking')
        await asyncio.wait_for(self.audio.playing.wait(), 1)
        self.assertFalse(self.synthesis_finished)
        self.audio.release.set()
        self.synthesis_release.set()
        await self.wait_status('local_listening')
        self.assertTrue(self.synthesis_finished)

    async def test_shared_recognition_reuses_exact_vad_span_without_local_stt(self):
        await self.start_worker(recognition='recall')
        await self.utterance()
        await self.wait_status('recall_transcript_received')
        await self.wait_status('local_speaking')
        self.assertEqual(self.transcriptions, 0)
        self.assertEqual(len(self.shared_requests), 1)
        self.assertEqual(self.heard, [('Lanalu say hello', 'recall', True, 'reply')])
        bounds, options = self.shared_requests[0]
        self.assertEqual(bounds[1] - bounds[0], 60_000_000)
        self.assertEqual(options['source'], 'vesktop')
        self.assertIsNone(options['capture_source'])
        self.assertTrue(await options['input_guard']())

    async def test_shared_recognition_failure_never_falls_back_to_local_stt(self):
        await self.start_worker(mode='local', recognition='recall', recognition_error='input_mismatch')
        await self.utterance()
        fields = await self.wait_status('recognition_unavailable')
        self.assertEqual(fields['recognition_error'], 'input_mismatch')
        self.assertEqual(self.transcriptions, 0)
        self.assertFalse(self.model_messages)
        self.assertEqual(self.shared_requests[0][1]['capture_source'], 'test-mic')

    async def test_shared_vesktop_binding_change_rejects_other_stream(self):
        await self.start_worker(recognition='recall')
        reads = 0
        async def changed_binding():
            nonlocal reads
            reads += 1
            return ('original' if reads == 1 else 'replacement',)
        self.audio.recognition_binding = changed_binding
        await self.utterance()
        fields = await self.wait_status('recognition_unavailable')
        self.assertEqual(fields['recognition_error'], 'input_mismatch')
        self.assertEqual(self.transcriptions, 0)
        self.assertFalse(self.model_messages)

    async def test_model_cleanup_even_when_audio_clear_fails(self):
        await self.start_worker()
        async def fail():
            raise OSError("synthetic audio loss")
        self.audio.clear = fail
        await self.stop_worker()
        self.assertTrue(self.model_closed)

    async def test_model_request_disables_thinking_for_fast_spoken_replies(self):
        model = local.LocalModel({})
        requests = []
        async def request(method, path, payload):
            requests.append(payload)
            return {'choices': [{'message': {'content': 'Hello.'}}]}
        model.request = request
        self.assertEqual(await model.answer([{'role': 'user', 'content': 'Hello'}]), 'Hello.')
        self.assertEqual(requests[0]['chat_template_kwargs'], {'enable_thinking': False})
        self.assertEqual(requests[0]['reasoning_effort'], 'none')
        self.assertEqual(requests[0]['max_tokens'], 160)

    def test_vad_cap_on_onset_and_sustained_speech(self):
        vad = local.VoiceActivity(max_seconds=.04)
        self.assertEqual(vad.feed(VOICE), (False, None))
        self.assertEqual(vad.feed(VOICE), (False, None))
        onset, pcm = vad.feed(VOICE)
        self.assertTrue(onset)
        self.assertEqual(len(pcm), 1920)
        vad = local.VoiceActivity(max_seconds=.2)
        results = [vad.feed(VOICE) for _ in range(30)]
        utterances = [pcm for _, pcm in results if pcm]
        self.assertTrue(utterances)
        self.assertTrue(all(len(pcm) <= 9600 for pcm in utterances))

    def test_identification_span_trims_preroll_and_silent_tail(self):
        end = 1_800_000_000_000_000_000
        pcm = SILENCE * 15 + VOICE * 4 + SILENCE * 32
        # 300ms pre-roll + 80ms speech + 640ms silence; STT PCM stays intact.
        self.assertEqual(local.voiced_time_span(pcm, end), (end - 720_000_000, end - 640_000_000))
        self.assertIsNone(local.voiced_time_span(SILENCE * 5, end))
        self.assertIsNone(local.voiced_time_span(b"", end))
        self.assertIsNone(local.voiced_time_span(VOICE, end, threshold=.2))
        self.assertEqual(local.voiced_time_span(VOICE, end), (end - 20_000_000, end))

    def test_lanalu_split_phonetic_spellings_without_unrelated_fuzzy_matches(self):
        for name in ('Lanalu', 'Lana Lu', 'Lana-Lou', 'Lanalou', 'Lana loo', 'Lanaloo', 'Lanalau', 'La nalu', 'La na lu'):
            self.assertTrue(local.contains_wake_word(f'Hey {name}, can you hear me?', ['lanalu']), name)
        for text in ('Lana is here', 'Lu said hello', 'no no no', 'nonono', 'lanaluv', 'lanaloupe', 'banana loop'):
            self.assertFalse(local.contains_wake_word(text, ['lanalu']), text)
        self.assertFalse(local.contains_wake_word('Lanalu hello', ['computer']))

    def test_wake_phrase_boundaries_and_empty_normalization(self):
        self.assertTrue(local.contains_wake_word("Hey, LANA-LU!", ["lana lu"]))
        self.assertFalse(local.contains_wake_word("lanaluv", ["lanalu"]))
        self.assertFalse(local.contains_wake_word("anything at all", ["!!!", ""]))


if __name__ == "__main__":
    unittest.main()
