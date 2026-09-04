"""The detector's curve over two LibriSpeech readers glued together.

The acceptance fixture is studio audio, not a Discord call, and the threshold
was fitted on a Discord call. This prints the curve so the gap is a number
rather than a guess.
"""

import os
import wave

import numpy as np
import sherpa_onnx

MODEL = os.path.expanduser("~/.local/share/nx-recall/models/eres2net_en.onnx")
FIX = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "fixtures")
WIN, HOP = 1.5, 0.25


def read(name):
    with wave.open(os.path.join(FIX, name), "rb") as w:
        sr = w.getframerate()
        raw = w.readframes(w.getnframes())
    return np.frombuffer(raw, dtype="<i2").astype(np.float32) / 32768.0, sr


def main():
    a, sr = read("clean_single_0.wav")
    b, _ = read("clean_single_1.wav")
    at = len(a) / sr
    x = np.concatenate([a, b])
    ex = sherpa_onnx.SpeakerEmbeddingExtractor(
        sherpa_onnx.SpeakerEmbeddingExtractorConfig(model=MODEL, num_threads=4)
    )

    def emb(s):
        st = ex.create_stream()
        st.accept_waveform(sample_rate=sr, waveform=s)
        st.input_finished()
        v = np.array(ex.compute(st), dtype=np.float64)
        return v / np.linalg.norm(v)

    n, hop = int(WIN * sr), int(HOP * sr)
    starts = list(range(0, len(x) - n + 1, hop))
    v = np.stack([emb(x[s:s + n]) for s in starts])
    step = int(WIN / HOP)
    print(f"turn {len(x) / sr:.2f}s, the readers meet at {at:.2f}s, "
          f"{len(starts)} windows")
    print(f"  {'boundary':>10}{'1 - cos':>10}")
    for i in range(len(starts) - step):
        t = (starts[i + step]) / sr
        d = 1.0 - float(v[i] @ v[i + step])
        mark = "  <- the join" if abs(t - at) <= 0.5 else ""
        print(f"  {t:>9.2f}s{d:>10.3f}{mark}")
    # And the two readers whole, which is the number a bank would see.
    print(f"\n  whole clip A vs whole clip B: 1 - cos = "
          f"{1.0 - float(emb(a) @ emb(b)):.3f}")


if __name__ == "__main__":
    main()
