"""Installer boundaries: all downloads are in-memory fixtures, never network."""
import hashlib
import importlib.util
import io
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('setup_local', Path(__file__).parents[1] / 'setup-local.py')
setup = importlib.util.module_from_spec(spec)
spec.loader.exec_module(setup)


class Response(io.BytesIO):
    headers = {}


class SetupTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.url = 'https://example.invalid/model'
        self.body = b'complete model'
        self.sha = hashlib.sha256(self.body).hexdigest()

    def archive(self, name='model/file', link=None):
        archive = self.root / 'test.tar'
        with tarfile.open(archive, 'w') as out:
            info = tarfile.TarInfo(name)
            if link is not None:
                info.type = tarfile.SYMTYPE
                info.linkname = link
                out.addfile(info)
            else:
                info.size = len(self.body)
                out.addfile(info, io.BytesIO(self.body))
        return archive

    def test_download_atomic_and_reuse(self):
        dest = self.root / 'model'
        with patch.object(setup.urllib.request, 'urlopen', return_value=Response(self.body)) as network:
            setup.download(self.url, dest, len(self.body), self.sha, 'speech_model')
            setup.download(self.url, dest, len(self.body), self.sha, 'speech_model')
        self.assertEqual(network.call_count, 1)
        self.assertEqual(dest.read_bytes(), self.body)
        self.assertFalse((self.root / 'model.partial').exists())

    def test_bad_download_preserves_existing_file(self):
        for data in (self.body[:-1], self.body + b'x', b'x' * len(self.body)):
            dest = self.root / 'model'
            dest.write_bytes(b'old')
            with patch.object(setup.urllib.request, 'urlopen', return_value=Response(data)):
                with self.assertRaises(ValueError):
                    setup.download(self.url, dest, len(self.body), self.sha, 'speech_model')
            self.assertEqual(dest.read_bytes(), b'old')
            self.assertFalse((self.root / 'model.partial').exists())

    def test_archive_rejects_traversal_and_escaping_link(self):
        for name, link in (('../outside', None), ('/absolute', None), ('model/link', '../../outside')):
            with self.assertRaises(ValueError):
                setup.unpack(self.archive(name, link), self.root / 'out')
        self.assertFalse((self.root / 'outside').exists())

    def test_archive_extracts_safe_file(self):
        setup.unpack(self.archive(), self.root / 'out')
        self.assertEqual((self.root / 'out/model/file').read_bytes(), self.body)

    def test_missing_file_is_incomplete(self):
        self.assertFalse(setup.complete_dir(self.root, {'missing': 4}))
        self.assertFalse(setup.llama_complete(self.root))
        self.assertFalse(setup.valid_file(self.root / 'missing', 4, self.sha))

    def test_readiness_check_is_read_only_when_missing(self):
        with patch.object(setup.urllib.request, 'urlopen', side_effect=AssertionError('network')):
            self.assertFalse(setup.check_ready(self.root / 'models', self.root / 'runtime'))
        self.assertEqual(list(self.root.iterdir()), [])

    def test_valid_existing_models_need_no_network(self):
        with patch.object(setup.urllib.request, 'urlopen', side_effect=AssertionError('network')):
            setup.install_archive(setup.LLAMA, self.root, 'llama', lambda p: p == self.root, 'llama-server')

    def test_existing_voice_reuse_and_invalid_rejection(self):
        source = self.root / 'source'
        source.write_bytes(self.body)
        target = self.root / 'voices/model'
        self.assertTrue(setup.reuse_file(source, target, len(self.body), self.sha))
        self.assertEqual(target.read_bytes(), self.body)
        self.assertFalse(setup.reuse_file(source, target, 1, self.sha))

    def test_kokoro_is_optional_and_selected_setup_preserves_piper(self):
        with patch.object(setup, 'install_archive') as archive, patch.object(setup, 'download') as download:
            setup.install_voice_model(self.root, 'kokoro')
        self.assertEqual(archive.call_count, 1)
        self.assertEqual(archive.call_args.args[0], setup.KOKORO)
        self.assertEqual(archive.call_args.args[1], self.root / 'voices' / setup.KOKORO_DIR)
        download.assert_not_called()
        with self.assertRaises(ValueError):
            setup.install_voice_model(self.root, 'remote')

    def test_kokoro_readiness_requires_phonemizer_data(self):
        required = {'model.onnx': 1, 'voices.bin': 1, 'espeak-ng-data/phontab': 1}
        for name in ('model.onnx', 'voices.bin'):
            (self.root / name).write_bytes(b'x')
        self.assertFalse(setup.complete_dir(self.root, required))
        (self.root / 'espeak-ng-data').mkdir()
        (self.root / 'espeak-ng-data/phontab').write_bytes(b'x')
        self.assertTrue(setup.complete_dir(self.root, required))

    def test_unexpected_archive_layout_does_not_replace_install(self):
        archive = self.archive()
        body = archive.read_bytes()
        target = self.root / 'installed'
        target.mkdir()
        (target / 'keep').write_text('previous')
        with patch.object(setup.urllib.request, 'urlopen', return_value=Response(body)):
            with self.assertRaises(ValueError):
                setup.install_archive((self.url, len(body), hashlib.sha256(body).hexdigest()),
                                      target, 'llama', lambda p: (p / 'missing').is_file(), 'missing')
        self.assertEqual((target / 'keep').read_text(), 'previous')


if __name__ == '__main__':
    unittest.main()
