"""Sliding-window ERes2Net embeddings inside every truth-labelled turn.

One pass over the archive's `single` / `partial` / `overlap` clips, writing one
`.npz` per window length. The daemon's own extractor and the daemon's own model
file: a change detector measured in a different space is a number that looks
like a distance and is not one.

    python3 spike/turnsplit_embed.py 1.0 1.5
"""

import os
import sys
import time
import wave

import numpy as np
import sherpa_onnx

import turnsplit_lib as L

MODEL = os.path.expanduser("~/.local/share/nx-recall/models/eres2net_en.onnx")
HOP_S = 0.25
THREADS = 4


def read_wav(path):
    with wave.open(path, "rb") as w:
        assert w.getnchannels() == 1 and w.getsampwidth() == 2, path
        sr = w.getframerate()
        raw = w.readframes(w.getnframes())
    return np.frombuffer(raw, dtype="<i2").astype(np.float32) / 32768.0, sr


def main(windows):
    c = L.conn()
    turns = L.load_turns(c)
    cfg = sherpa_onnx.SpeakerEmbeddingExtractorConfig(
        model=MODEL, num_threads=THREADS, debug=False, provider="cpu"
    )
    ex = sherpa_onnx.SpeakerEmbeddingExtractor(cfg)

    def embed(samples, sr):
        s = ex.create_stream()
        s.accept_waveform(sample_rate=sr, waveform=samples)
        s.input_finished()
        return np.array(ex.compute(s), dtype=np.float32)

    for win in windows:
        ids, starts, vecs, offsets = [], [], [], []
        audio_s = 0.0
        missing = 0
        t0 = time.perf_counter()
        for t in turns:
            p = os.path.join(L.CLIPS, t.path)
            if not os.path.exists(p):
                missing += 1
                continue
            w, sr = read_wav(p)
            n = int(win * sr)
            hop = int(HOP_S * sr)
            if len(w) < n:
                # Shorter than one window: one embedding of the whole thing, so
                # the row still exists and the detector can say "no change".
                offsets.append(len(vecs))
                ids.append(t.id)
                starts.append(0.0)
                vecs.append(embed(w, sr))
                audio_s += len(w) / sr
                continue
            offsets.append(len(vecs))
            for s0 in range(0, len(w) - n + 1, hop):
                ids.append(t.id)
                starts.append(s0 / sr)
                vecs.append(embed(w[s0:s0 + n], sr))
                audio_s += n / sr
        dt = time.perf_counter() - t0
        out = os.path.join(L.SCRATCH, f"win_{win:.2f}.npz")
        np.savez_compressed(
            out,
            ids=np.array(ids, dtype=np.int64),
            starts=np.array(starts, dtype=np.float32),
            vecs=np.stack(vecs).astype(np.float32),
        )
        print(f"win {win:.2f}s hop {HOP_S}s: {len(vecs)} windows over {len(turns) - missing} "
              f"turns, {audio_s:.0f} s of audio in {dt:.0f} s (RTF {dt / audio_s:.4f}), "
              f"missing clips {missing} -> {out}")


if __name__ == "__main__":
    main([float(a) for a in sys.argv[1:]] or [1.0, 1.5])
