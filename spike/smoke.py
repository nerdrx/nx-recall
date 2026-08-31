"""Sanity checks before the full grid: does each stage do what it claims?"""
import os, sys, time
from pathlib import Path
import numpy as np

sys.path.insert(0, str(Path(__file__).parent))
from harness import (Embedder, extract_corpus, index_corpus, load, mix,
                     opus_roundtrip, rms_normalise, take_window, trim_silence)

S = Path(os.environ.get("NXR_SCRATCH", "/tmp/nxr"))
root = extract_corpus(S / "corpus" / "dev-clean.tar.gz", S / "corpus")
by = index_corpus(root, 8)
spk = sorted(by)[:4]
print(f"speakers indexed: {len(by)}  using {spk}")

emb = Embedder(S / "models" / "eres2net_en.onnx", num_threads=4)

a1 = rms_normalise(trim_silence(load(by[spk[0]][0].path)))
a2 = rms_normalise(trim_silence(load(by[spk[0]][1].path)))
b1 = rms_normalise(trim_silence(load(by[spk[1]][0].path)))
print(f"trimmed lengths: {len(a1)/16000:.1f}s {len(a2)/16000:.1f}s {len(b1)/16000:.1f}s")

va1, va2, vb1 = emb(a1), emb(a2), emb(b1)
print(f"clean  same-speaker cos = {va1 @ va2:+.3f}")
print(f"clean  diff-speaker cos = {va1 @ vb1:+.3f}")

# codec must change the audio but preserve identity
rng = np.random.default_rng(0)
w = take_window(a1, 3.0, rng)
for br in (32, 24, 12, 8):
    t = time.time()
    c = opus_roundtrip(w, br)
    snr = 10 * np.log10((w**2).sum() / (((w - c) ** 2).sum() + 1e-12))
    print(f"opus {br:2d}k: len_ok={len(c)==len(w)} snr={snr:5.1f}dB "
          f"cos_to_clean={emb(c) @ emb(w):+.3f}  ({time.time()-t:.2f}s)")

# mixing must actually bury the target as talkers pile up
wt = take_window(a1, 3.0, rng)
itfs = [take_window(rms_normalise(trim_silence(load(by[s][0].path))), 3.0, rng)
        for s in sorted(by)[4:14]]
vt = emb(wt)
for n, dom in [(1, 0), (3, 0), (10, 0), (10, 12)]:
    m = mix(wt, itfs[: n - 1], float(dom))
    print(f"mix {n:2d} talkers @{dom:2d}dB: cos_to_target={emb(m) @ vt:+.3f}")

t = time.time()
for _ in range(20):
    emb(wt)
rtf = ((time.time() - t) / 20) / 3.0
print(f"\nembedding speed: {(time.time()-t)/20*1000:.0f} ms per 3s window "
      f"(realtime factor {rtf:.3f}, 4 threads)")
