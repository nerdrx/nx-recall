import unittest
from unittest.mock import patch
from types import SimpleNamespace
import numpy as np
from nx_recall_voice.speech import Speech, name_only_edit

class NameRefinementTests(unittest.TestCase):
    def test_only_one_name_changes_and_other_bytes_stay_exact(self):
        self.assertEqual(name_only_edit('Hey,  la nalu! Hello.', 'Hey Lanalu hello', ['Lanalu']), 'Hey,  Lanalu! Hello.')
        for before, after in [('no no no stop','Lanalu please stop'), ('hello there','Lanalu hello there'), ('no no no','No no no'), ('hello','hello')]:
            self.assertEqual(name_only_edit(before,after,['Lanalu']),before)
        self.assertEqual(name_only_edit('nonono','Lanalu',['Lanalu']),'Lanalu')  # Only acoustic alternate can propose this; never a text alias.

    def test_disabled_and_correct_turns_skip_extra_model(self):
        speech=Speech();samples=np.zeros(16000,dtype=np.float32)
        with patch('nx_recall_voice.name_assistance.build_recognizer') as build:
            self.assertEqual(speech._refine_names(samples,'hello'),'hello')
            speech.set_name_hints(['Lanalu']);speech._refine_names(samples,'Lanalu hello')
            speech._refine_names(np.zeros(240001),'hello')
            build.assert_not_called()

    def test_optional_failure_keeps_greedy_and_backs_off(self):
        speech=Speech();speech.set_name_hints(['Lanalu'])
        with patch('nx_recall_voice.name_assistance.build_recognizer',side_effect=ValueError('synthetic')) as build:
            for _ in range(2):self.assertEqual(speech._refine_names(np.zeros(16000),'no no no'),'no no no')
            self.assertEqual(build.call_count,1)

    def test_acoustic_alternate_is_gated_and_disable_releases_model(self):
        speech=Speech();speech.set_name_hints(['Lanalu'])
        stream=SimpleNamespace(accept_waveform=lambda *args:None,result=SimpleNamespace(text='Lanalu hello'))
        decoder=SimpleNamespace(create_stream=lambda:stream,decode_stream=lambda s:None)
        with patch('nx_recall_voice.name_assistance.build_recognizer',return_value=decoder):
            self.assertEqual(speech._refine_names(np.zeros(16000),'la nalu hello'),'Lanalu hello')
            stream.result.text='Lanalu goodbye'
            self.assertEqual(speech._refine_names(np.zeros(16000),'la nalu hello'),'la nalu hello')
            speech.set_name_hints([]);speech._refine_names(np.zeros(16000),'la nalu hello')
            self.assertIsNone(speech.hint_recognizer)
