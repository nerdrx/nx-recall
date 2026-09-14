import time
import unittest

from nx_recall_voice.recall import RecallClient, RecallError, RecognitionUnavailable, _recognized


async def verified_input():
    return True


class SharedRecognitionTests(unittest.IsolatedAsyncioTestCase):
    def setUp(self):
        self.end = time.time_ns() - 500_000_000
        self.start = self.end - 2_000_000_000
        self.row = dict(id=7, source='vesktop', text='Lanalu hello',
                        t_start_ns=str(self.start), t_end_ns=str(self.end))

    def client(self, replies, source='vesktop', device='test-mic', paused=False, streams=1):
        client = RecallClient()
        self.calls = []
        self.reads = 0
        async def session(operation):
            async def call(method, params, ident):
                self.calls.append((method, params))
                if method == 'status':
                    return {'paused': paused}
                if method == 'sources.list':
                    return {'sources': [{'match_key': source, 'allowed': True, 'streams': streams}]}
                if method == 'mic.get':
                    return {'enabled': True, 'active': True, 'device': device}
                if method == 'devices.list':
                    return {'devices': [{'node_name': 'test-mic', 'is_default': True}]}
                if method == 'voice.transcript':
                    reply = replies[min(self.reads, len(replies) - 1)]
                    self.reads += 1
                    return {'segments': reply}
                self.fail(method)
            return await operation(call)
        client._session = session
        return client

    def test_exact_and_split_segments_are_assembled_chronologically(self):
        middle = self.start + 1_000_000_000
        first = dict(self.row, text='Lanalu', t_end_ns=str(middle))
        last = dict(self.row, id=8, text='hello', t_start_ns=str(middle))
        result = _recognized([last, first], self.start, self.end, 'vesktop')
        self.assertEqual(result, {'text': 'Lanalu hello', 'segment_ids': [7, 8]})
        self.assertIsNone(_recognized([first], self.start, self.end, 'vesktop'))

    def test_wrong_source_partial_wide_and_consumed_segments_are_rejected(self):
        for row in [dict(self.row, source='discord'),
                    dict(self.row, t_start_ns=str(self.end - 200_000_000)),
                    dict(self.row, t_start_ns=str(self.start - 1_000_000_000)),
                    dict(self.row, t_end_ns=str(self.end + 1_000_000_000))]:
            self.assertIsNone(_recognized([row], self.start, self.end, 'vesktop'))
        self.assertIsNone(_recognized([self.row], self.start, self.end, 'vesktop', consumed=[7]))

    async def test_delayed_split_commit_waits_for_complete_stable_match(self):
        middle = self.start + 1_000_000_000
        first = dict(self.row, text='Lanalu', t_end_ns=str(middle))
        last = dict(self.row, id=8, text='hello', t_start_ns=str(middle))
        client = self.client([[], [first], [last, first], [first, last]])
        result = await client.recognize(self.start, self.end, source='vesktop', input_guard=verified_input, wait_seconds=2)
        self.assertEqual(result['text'], 'Lanalu hello')
        self.assertEqual(result['segment_ids'], [7, 8])
        self.assertEqual(self.reads, 4)
        self.assertEqual(list(client.recognized_ids), [7, 8])
        self.assertTrue(all(params['source'] == 'vesktop' for method, params in self.calls if method == 'voice.transcript'))

    async def test_canonical_id_is_consumed_once_across_verified_sources(self):
        # Native voice.transcript retains canonical ID while projecting the exact
        # observed source and its interval; no Python fallback to mic is allowed.
        self.row.update(canonical_source='mic', canonical_segment_id=7,
                        provenance='confirmed_mic_audio', audio_correlation=.99)
        client = self.client([[self.row]])
        first = await client.recognize(self.start, self.end, source='vesktop',
                                       input_guard=verified_input, wait_seconds=1)
        self.assertEqual(first['segment_ids'], [7])
        self.row['source'] = 'mic'
        with self.assertRaises(RecognitionUnavailable) as caught:
            await client.recognize(self.start, self.end, source='mic',
                                   capture_source='test-mic', wait_seconds=.1)
        self.assertEqual(caught.exception.code, 'timeout')
        self.assertEqual(list(client.recognized_ids), [7])

    async def test_canonical_mic_row_does_not_substitute_for_observed_virtual_source(self):
        client = self.client([[dict(self.row, source='mic')]])
        with self.assertRaises(RecognitionUnavailable) as caught:
            await client.recognize(self.start, self.end, source='vesktop',
                                   input_guard=verified_input, wait_seconds=.1)
        self.assertEqual(caught.exception.code, 'timeout')
        queries = [params for method, params in self.calls if method == 'voice.transcript']
        self.assertTrue(queries)
        self.assertTrue(all(params['source'] == 'vesktop' for params in queries))
        self.assertFalse(any(method == 'transcript' for method, _ in self.calls))

    async def test_source_binding_must_remain_valid_until_stable_read(self):
        client = self.client([[self.row]])
        checks = iter([True, False])
        async def guard():
            return next(checks)
        with self.assertRaises(RecognitionUnavailable) as caught:
            await client.recognize(self.start, self.end, source='vesktop', input_guard=guard, wait_seconds=1)
        self.assertEqual(caught.exception.code, 'input_mismatch')
        self.assertEqual(self.reads, 1)
        self.assertFalse(client.recognized_ids)

    async def test_old_native_rpc_fails_explicitly_without_legacy_fallback(self):
        client = RecallClient()
        calls = []
        async def session(operation):
            async def call(method, params, ident):
                calls.append(method)
                if method == 'status':
                    return {'paused': False}
                if method == 'sources.list':
                    return {'sources': [{'match_key': 'vesktop', 'allowed': True, 'streams': 1}]}
                if method == 'voice.transcript':
                    raise RecallError('unsupported method')
                self.fail(method)
            return await operation(call)
        client._session = session
        with self.assertRaises(RecognitionUnavailable) as caught:
            await client.recognize(self.start, self.end, source='vesktop', input_guard=verified_input)
        self.assertEqual(caught.exception.code, 'unavailable')
        self.assertEqual(calls, ['status', 'sources.list', 'voice.transcript'])

    async def test_microphone_matches_explicit_pin_or_resolved_default(self):
        for device in ('test-mic', None):
            client = self.client([[dict(self.row, source='mic')]], device=device)
            result = await client.recognize(self.start, self.end, source='mic', capture_source='test-mic', wait_seconds=1)
            self.assertEqual(result['text'], 'Lanalu hello')
            self.assertEqual(any(m == 'devices.list' for m, _ in self.calls), device is None)

    async def test_wrong_input_paused_and_ambiguous_sources_never_read_transcripts(self):
        for opts, source, capture, code in [({'device': 'another-mic'}, 'mic', 'test-mic', 'input_mismatch'),
                                         ({'paused': True}, 'vesktop', None, 'paused'),
                                         ({'streams': 2}, 'vesktop', None, 'ambiguous_source'),
                                         ({'source': 'discord'}, 'vesktop', None, 'source_unavailable')]:
            client = self.client([[self.row]], **opts)
            with self.assertRaises(RecognitionUnavailable) as caught:
                await client.recognize(self.start, self.end, source=source, capture_source=capture, input_guard=verified_input, wait_seconds=1)
            self.assertEqual(caught.exception.code, code)
            self.assertFalse(any(m == 'voice.transcript' for m, _ in self.calls))

    async def test_missing_or_unrelated_transcript_times_out_without_text(self):
        client = self.client([[dict(self.row, source='discord')]])
        began = time.monotonic()
        with self.assertRaises(RecognitionUnavailable) as caught:
            await client.recognize(self.start, self.end, source='vesktop', input_guard=verified_input, wait_seconds=.25)
        self.assertEqual(caught.exception.code, 'timeout')
        self.assertLess(time.monotonic() - began, .6)
        self.assertNotIn('Lanalu', str(caught.exception))

    async def test_unproven_or_changed_vesktop_binding_never_reads_words(self):
        for guard in (None, self.wrong_binding):
            client = self.client([[self.row]])
            with self.assertRaises(RecognitionUnavailable) as caught:
                await client.recognize(self.start, self.end, source='vesktop', input_guard=guard)
            self.assertEqual(caught.exception.code, 'input_mismatch')
            self.assertFalse(any(method == 'voice.transcript' for method, _ in self.calls))

    async def wrong_binding(self):
        return False

    def test_quiet_native_tail_inside_observed_audio_is_accepted_without_widening_core(self):
        native = dict(self.row, t_start_ns=str(self.start - 200_000_000),
                      t_end_ns=str(self.end + 650_000_000))
        self.assertIsNone(_recognized([native], self.start, self.end, 'vesktop'))
        window = (self.start - 300_000_000, self.end + 650_000_000)
        self.assertEqual(_recognized([native], self.start, self.end, 'vesktop', capture_window=window)['text'], 'Lanalu hello')
        unrelated = dict(native, t_end_ns=str(self.end + 1_500_000_000))
        self.assertIsNone(_recognized([unrelated], self.start, self.end, 'vesktop', capture_window=window))
        self.assertIsNone(_recognized([dict(native, source='discord')], self.start, self.end, 'vesktop', capture_window=window))
        self.assertIsNone(_recognized([native], self.start, self.end, 'vesktop', consumed=[7], capture_window=window))

    async def test_timeout_reports_only_numeric_alignment_diagnostics(self):
        native = dict(self.row, t_start_ns=str(self.start - 2_000_000_000))
        client = self.client([[native]])
        with self.assertRaises(RecognitionUnavailable) as caught:
            await client.recognize(self.start, self.end, source='vesktop', input_guard=verified_input, wait_seconds=.1)
        values = caught.exception.diagnostics
        self.assertEqual(values['recognition_candidate_count'], 1)
        self.assertEqual(values['recognition_start_offset_ms'], -2000)
        self.assertTrue(all(type(value) is int for value in values.values()))

    async def test_captured_window_cannot_expand_to_unrelated_historical_audio(self):
        client = self.client([[self.row]])
        with self.assertRaises(RecognitionUnavailable) as caught:
            await client.recognize(self.start, self.end, source='vesktop', input_guard=verified_input,
                                   capture_window=(self.start - 10_000_000_000, self.end))
        self.assertEqual(caught.exception.code, 'invalid_interval')
        self.assertEqual(self.calls, [])

    async def test_stale_interval_is_rejected_before_rpc(self):
        client = self.client([[self.row]])
        with self.assertRaises(RecognitionUnavailable) as caught:
            await client.recognize(self.start - 30_000_000_000, self.end - 30_000_000_000, source='vesktop')
        self.assertEqual(caught.exception.code, 'stale_interval')
        self.assertEqual(self.calls, [])
