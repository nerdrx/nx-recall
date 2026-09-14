#!/usr/bin/env python3
"""One-time optional downloads; never starts inference, capture, or audio routing."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.request

MODELS = Path.home() / '.local/share/nx-recall/models'
STT = 'sherpa-onnx-nemo-parakeet_tdt_transducer_110m-en-36000-int8'
STT_FILES = {'encoder.int8.onnx': 131113202, 'decoder.int8.onnx': 3955863,
             'joiner.int8.onnx': 1411403, 'tokens.txt': 9953}
# Match Recall's shared model catalogue rather than replacing its quantization.
QWEN = ('https://huggingface.co/bartowski/Qwen2.5-3B-Instruct-GGUF/resolve/'
        'f302c64a2269a69fb27b2f9473b362f5bb8e78d8/Qwen2.5-3B-Instruct-Q4_K_M.gguf',
        1929903264, '9c9f56a391a3abbd5b89d0245bf6106081bcc3173119d4229235dd9d23253f94')
LLAMA = ('https://github.com/ggml-org/llama.cpp/releases/download/b10950/'
         'llama-b10950-bin-ubuntu-vulkan-x64.tar.gz', 30162811,
         '08f03f2b6b0cabac54017fa837c94d2def77de59b69a2de3ad392e79192703ba')
PARAKEET = ('https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/'
            + STT + '.tar.bz2', 108035095,
            'f628312e9fdf8686374cb01a69425c41732529d540860311f16f37cbc32cfe9b')
PIPER_BASE = ('https://huggingface.co/rhasspy/piper-voices/resolve/'
              '1162a9173d0ce503555aed757976b7a9912eae4c/en/en_US/amy/medium/')
PIPER = {
    'en_US-amy-medium.onnx': (63201294, 'b3a6e47b57b8c7fbe6a0ce2518161a50f59a9cdd8a50835c02cb02bdd6206c18'),
    'en_US-amy-medium.onnx.json': (4882, '95a23eb4d42909d38df73bb9ac7f45f597dbfcde2d1bf9526fdeaf5466977d77'),
}


def progress(stage, percent):
    print(json.dumps({'event': 'setup_progress', 'stage': stage, 'percent': percent}), flush=True)


def valid_file(path, size, sha=None):
    if not path.is_file() or path.stat().st_size != size:
        return False
    if sha is None:
        return True
    digest = hashlib.sha256()
    with path.open('rb') as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(chunk)
    return digest.hexdigest() == sha


def download(url, target, size, sha, stage):
    """Bound both transfer length and memory; publish only verified whole files."""
    if valid_file(target, size, sha):
        return
    target.parent.mkdir(parents=True, exist_ok=True)
    partial = target.with_name(target.name + '.partial')
    last = -1
    try:
        request = urllib.request.Request(url, headers={'User-Agent': 'NX-Recall-Local-Voice/1'})
        with urllib.request.urlopen(request, timeout=60) as response, partial.open('wb') as out:
            expected = response.headers.get('Content-Length')
            if expected is not None and int(expected) != size:
                raise ValueError('Download length does not match the pinned model')
            total = 0
            digest = hashlib.sha256()
            while chunk := response.read(1024 * 1024):
                total += len(chunk)
                if total > size:
                    raise ValueError('Download exceeds the pinned model size')
                out.write(chunk)
                digest.update(chunk)
                percent = total * 100 // size
                if percent // 5 != last:
                    progress(stage, percent)
                    last = percent // 5
            if total != size or digest.hexdigest() != sha:
                raise ValueError('Download is incomplete or checksum does not match')
            out.flush()
            os.fsync(out.fileno())
        partial.replace(target)
    finally:
        partial.unlink(missing_ok=True)


def complete_dir(path, required):
    return path.is_dir() and all(valid_file(path / name, size) for name, size in required.items())


def llama_complete(path):
    required = ['llama-server', 'libllama.so', 'libggml.so', 'libggml-base.so',
                'libggml-vulkan.so', 'libllama-server-impl.so']
    return all((path / name).is_file() and (path / name).stat().st_size > 0 for name in required)


def unpack(archive, destination):
    """Extraction stays within a disposable directory and cannot create devices."""
    with tarfile.open(archive) as bundle:
        members = bundle.getmembers()
        if len(members) > 10000 or sum(m.size for m in members) > 1024 ** 3:
            raise ValueError('Archive exceeds extraction limits')
        root = destination.resolve()
        for member in members:
            name = Path(member.name)
            if name.is_absolute() or '..' in name.parts:
                raise ValueError('Unsafe archive path')
            if not (member.isfile() or member.isdir() or member.issym() or member.islnk()):
                raise ValueError('Unsupported archive member')
            if member.issym() or member.islnk():
                link = Path(member.linkname)
                target = (root / name.parent / link).resolve() if member.issym() else (root / link).resolve()
                if link.is_absolute() or not target.is_relative_to(root):
                    raise ValueError('Unsafe archive link')
        bundle.extractall(destination, members=members, filter='data')


def install_archive(asset, target, stage, validator, marker):
    if validator(target):
        return
    target.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='.voice-setup-', dir=target.parent) as temp:
        temp = Path(temp)
        archive = temp / 'model.tar'
        download(asset[0], archive, asset[1], asset[2], stage)
        extracted = temp / 'extracted'
        extracted.mkdir()
        unpack(archive, extracted)
        candidates = [p.parent for p in extracted.rglob(marker) if validator(p.parent)]
        if len(candidates) != 1:
            raise ValueError('Archive does not contain the expected model layout')
        # Retain an incomplete prior install until the replacement is fully validated.
        backup = None
        if target.exists():
            backup = Path(tempfile.mkdtemp(prefix=target.name + '.previous-', dir=target.parent))
            backup.rmdir()
            target.rename(backup)
        try:
            candidates[0].rename(target)
        except BaseException:
            if backup is not None:
                backup.rename(target)
            raise


def reuse_file(source, target, size, sha):
    if not valid_file(source, size, sha):
        return False
    target.parent.mkdir(parents=True, exist_ok=True)
    partial = target.with_name(target.name + '.partial')
    try:
        shutil.copyfile(source, partial)
        if not valid_file(partial, size, sha):
            raise ValueError('Existing model changed during copy')
        partial.replace(target)
    finally:
        partial.unlink(missing_ok=True)
    return True


def install_models(models=MODELS):
    progress('llama', 0)
    install_archive(LLAMA, models / 'llama-voice', 'llama', llama_complete, 'llama-server')
    progress('llama', 100)
    progress('language_model', 0)
    target = models / 'qwen2.5-3b-instruct-q4_k_m.gguf'
    download(QWEN[0], target, *QWEN[1:], 'language_model')
    progress('language_model', 100)
    progress('speech_model', 0)
    install_archive(PARAKEET, models / STT, 'speech_model',
                    lambda p: complete_dir(p, STT_FILES), 'encoder.int8.onnx')
    progress('speech_model', 100)
    progress('voice_model', 0)
    for name, (size, sha) in PIPER.items():
        target = models / 'voices' / name
        if not valid_file(target, size, sha):
            old = Path.home() / '.local/share/nx-wisp/models' / name
            if not reuse_file(old, target, size, sha):
                download(PIPER_BASE + name, target, size, sha, 'voice_model')
    progress('voice_model', 100)


def check_ready(models=MODELS, runtime=None):
    """Read-only install validation; no imports that load models or audio devices."""
    runtime = runtime or Path.home() / '.local/share/nx-recall/voice'
    python = runtime / 'venv/bin/python'
    target = models / 'qwen2.5-3b-instruct-q4_k_m.gguf'
    checks = {
        'runtime': python.is_file(),
        'llama': llama_complete(models / 'llama-voice'),
        'language_model': valid_file(target, *QWEN[1:]),
        'speech_model': complete_dir(models / STT, STT_FILES),
        'voice_model': all(valid_file(models / 'voices' / name, *metadata)
                           for name, metadata in PIPER.items()),
    }
    if checks['runtime']:
        result = subprocess.run([str(python), '-c', 'import numpy, sherpa_onnx, piper, nx_recall_voice'],
                                capture_output=True, timeout=60)
        checks['runtime'] = result.returncode == 0
    for stage, ready in checks.items():
        progress(stage, 100 if ready else 0)
    ready = all(checks.values())
    print(json.dumps({'event': 'setup_check', 'ready': ready, 'checks': checks}), flush=True)
    return ready


def main():
    if sys.version_info < (3, 11) or not hasattr(tarfile, 'data_filter'):
        raise RuntimeError('Local Voice requires Python 3.11.8+ with safe tar extraction support')
    if platform.system() != 'Linux' or platform.machine() not in ('x86_64', 'AMD64'):
        raise RuntimeError('Local Voice automatic setup currently supports Linux x86_64')
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--check', action='store_true', help='Check installed runtime and models without downloads or changes')
    args = parser.parse_args()
    if args.check:
        if not check_ready():
            raise RuntimeError('Local Voice setup is incomplete')
        return
    root = Path(__file__).resolve().parent
    if not (root / 'pyproject.toml').is_file():
        raise RuntimeError('The packaged Local Voice project is missing; reinstall NX Recall')
    runtime = Path.home() / '.local/share/nx-recall/voice'
    runtime.mkdir(parents=True, exist_ok=True)
    with (runtime / 'setup.lock').open('w') as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise RuntimeError('Local Voice setup is already running') from None
        progress('runtime', 0)
        venv = runtime / 'venv'
        subprocess.run([sys.executable, '-m', 'venv', str(venv)], check=True)
        subprocess.run([str(venv / 'bin/python'), '-m', 'pip', 'install', '--disable-pip-version-check', str(root)], check=True)
        subprocess.run([str(venv / 'bin/python'), '-c', 'import numpy, sherpa_onnx, piper, nx_recall_voice'], check=True)
        progress('runtime', 100)
        install_models()
        progress('complete', 100)
        print('Local Voice is ready. Enable it in NX Recall Settings when you want to listen.', flush=True)


if __name__ == '__main__':
    try:
        main()
    except (OSError, ValueError, RuntimeError, subprocess.CalledProcessError, tarfile.TarError) as error:
        print(f'Local Voice setup failed: {error}', file=sys.stderr, flush=True)
        sys.exit(1)
