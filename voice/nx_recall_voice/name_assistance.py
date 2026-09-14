"""Private bounded acoustic hint worker; no logs, network, or audio devices."""
import argparse
import json
import os
from pathlib import Path
import socket
import struct
import tempfile

MAX_FRAME = 16_000 * 15 * 4


def receive(connection, limit=MAX_FRAME):
    def exact(size):
        data = bytearray()
        while len(data) < size:
            part = connection.recv(size - len(data))
            if not part:
                raise EOFError
            data.extend(part)
        return bytes(data)
    length = struct.unpack('!I', exact(4))[0]
    if length > limit:
        raise ValueError('oversized frame')
    return exact(length)


def send(connection, payload):
    if len(payload) > 8192:
        raise ValueError('oversized reply')
    connection.sendall(struct.pack('!I', len(payload)) + payload)


def build_recognizer(config):
    import sherpa_onnx
    terms = config['terms']
    if not isinstance(terms, list) or not 1 <= len(terms) <= 8 or any(
        not isinstance(t, str) or not 3 <= len(t) <= 32 or not t.isascii() or not t.isalpha()
        for t in terms
    ):
        raise ValueError('invalid hints')
    paths = {key: Path(config[key]) for key in ('encoder', 'decoder', 'joiner', 'tokens')}
    if any(not p.is_absolute() or not p.is_file() for p in paths.values()):
        raise ValueError('missing model')
    pieces = []
    for line in paths['tokens'].read_text().splitlines():
        piece, _, index = line.rpartition(' ')
        if piece and not piece.startswith('<') and index.isdigit():
            pieces.append((piece, int(index)))
    vocabulary = {p for p, _ in pieces}
    if '▁' not in vocabulary or any(c not in vocabulary for term in terms for c in term):
        raise ValueError('unsupported hint encoding')
    with tempfile.TemporaryDirectory(prefix='nx-recall-hint-model-') as directory:
        vocab = Path(directory) / 'bpe.vocab'
        hints = Path(directory) / 'names.txt'
        vocab.write_text(''.join(f'{piece}\t{-index}\n' for piece, index in pieces))
        hints.write_text('\n'.join(terms) + '\n')
        recognizer = sherpa_onnx.OfflineRecognizer.from_transducer(
            **{key: str(value) for key, value in paths.items()},
            num_threads=max(1, min(4, int(config.get('threads', 2)))),
            model_type='nemo_transducer', decoding_method='modified_beam_search',
            hotwords_file=str(hints), hotwords_score=.7,
            modeling_unit='bpe', bpe_vocab=str(vocab),
        )
    return recognizer


def run(path):
    import numpy as np
    import sherpa_onnx
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
        connection.settimeout(10)
        connection.connect(path)
        _, uid, _ = struct.unpack('3i', connection.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
        if uid != os.getuid():
            raise PermissionError
        config = json.loads(receive(connection, 32768))
        recognizer = build_recognizer(config)
        send(connection, b'ready')
        # Parent controls idle lifetime; read timeout is only for startup and requests.
        connection.settimeout(None)
        while True:
            payload = receive(connection)
            if len(payload) % 4 or not 16000 <= len(payload) <= MAX_FRAME:
                raise ValueError('invalid audio frame')
            samples = np.frombuffer(payload, dtype='<f4')
            if not np.isfinite(samples).all():
                raise ValueError('invalid samples')
            stream = recognizer.create_stream()
            stream.accept_waveform(16000, samples)
            recognizer.decode_stream(stream)
            text = stream.result.text
            if len(text) > 2000:
                raise ValueError('oversized text')
            send(connection, text.encode())


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description='Private Recall name hint worker')
    parser.add_argument('--socket', required=True)
    args = parser.parse_args()
    try:
        run(args.socket)
    except (Exception, KeyboardInterrupt):
        # Keep user speech/hints and third-party exception text out of diagnostics.
        raise SystemExit(1) from None
