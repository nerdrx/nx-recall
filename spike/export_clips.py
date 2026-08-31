"""Cut the recording's single-speaker regions into clips for hand-labeling.

Writes spike/clips/clip_NNN.wav plus clips/regions.json (times, fixed shuffled
presentation order). The shuffle is seeded so the labeling order is random with
respect to time and cluster — that randomness is what makes the resulting accuracy
number unbiased.
"""
import json
import sys
from pathlib import Path

import numpy as np
import soundfile as sf

sys.path.insert(0, str(Path(__file__).parent))
from harness import SR  # noqa: E402
from measure_lobby import SEG, decode, regions, segment  # noqa: E402

src = Path(sys.argv[1])
out = Path(__file__).parent / "clips"
out.mkdir(exist_ok=True)

import onnxruntime as ort  # noqa: E402
so = ort.SessionOptions()
so.intra_op_num_threads = 4
x = decode(src)
cls = segment(ort.InferenceSession(str(SEG), so, providers=["CPUExecutionProvider"]), x)
n_chunks = (len(x) + 160_000 - 1) // 160_000
hop = 160_000 / SR / (len(cls) / n_chunks)
runs = regions(cls, hop, len(x) / SR)

meta = []
for i, (a, b) in enumerate(runs):
    clip = x[int(a * SR):int(b * SR)]
    sf.write(out / f"clip_{i:03d}.wav", clip, SR)
    meta.append({"id": i, "t0": round(a, 2), "t1": round(b, 2)})

order = list(range(len(runs)))
np.random.default_rng(0xFACE).shuffle(order)
(out / "regions.json").write_text(json.dumps(
    {"source": src.name, "sr": SR, "clips": meta, "order": order}, indent=1))
print(f"{len(runs)} clips -> {out}")
