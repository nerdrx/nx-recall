"""Does an independent overlapped-speech detector catch what the embedding cannot?

FINDINGS.md concluded that auto-enrollment must be gated on an independent
single-speaker signal, because no embedding-side statistic (score, top1-vs-top2
margin, sub-window agreement) detects equal-loudness overlap. That recommendation
named pyannote segmentation but never tested it. This tests it.

pyannote segmentation-3.0 is a POWERSET classifier: 7 classes over 3 local speakers
with at most 2 simultaneous.

    0 = silence
    1,2,3 = exactly one speaker active
    4,5,6 = TWO speakers active   <- overlap

So overlap detection is just `argmax in {4,5,6}` per frame — no diarization needed.

Two things must both be true for this to be a usable gate:
  1. it flags the poison cell (equal-loudness overlap), and
  2. it does NOT flag clean single-talker speech, or gating costs all our coverage.
"""

from __future__ import annotations

import os
import sys
from concurrent.futures import ProcessPoolExecutor
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).parent))
from harness import (Embedder, extract_corpus, index_corpus, load, mix,  # noqa: E402
                     opus_roundtrip, rms_normalise, trim_silence)

S = Path(os.environ.get("NXR_SCRATCH", "/tmp/nxr"))
SEG = S / "models" / "sherpa-onnx-pyannote-segmentation-3-0" / "model.onnx"
EMB = S / "models" / "eres2net_en.onnx"

WIN = 160_000          # 10 s at 16 kHz, the model's native window
N_SPK, N_ENROLL, N_PROBE = 40, 3, 5
BR, SEED = 24, 0x5EED
CELLS = [(1, 0.0), (2, 0.0), (3, 0.0), (4, 0.0),
         (2, 6.0), (2, 12.0), (3, 12.0), (10, 12.0)]
TAUS = [0.10, 0.20, 0.30, 0.50]

_G: dict = {}


def _init(pools):
    import onnxruntime as ort
    so = ort.SessionOptions()
    so.intra_op_num_threads = 1
    _G["seg"] = ort.InferenceSession(str(SEG), so, providers=["CPUExecutionProvider"])
    _G["emb"] = Embedder(EMB, num_threads=1)
    _G["pool"] = [np.asarray(p, dtype=np.float32) for p in pools]


def _enroll(u):
    return _G["emb"](_G["pool_en"][u]).tolist() if "pool_en" in _G else None


def _window(pool: np.ndarray, rng) -> np.ndarray:
    if len(pool) <= WIN:
        return np.pad(pool, (0, WIN - len(pool)))
    o = int(rng.integers(0, len(pool) - WIN))
    return pool[o:o + WIN]


def _probe(spec):
    rng = np.random.default_rng(spec["seed"])
    pool = _G["pool"]
    tgt = opus_roundtrip(_window(pool[spec["t"]], rng), BR)
    itf = [opus_roundtrip(_window(pool[i], rng), BR) for i in spec["itf"]]
    m = mix(tgt, itf, spec["dom"])

    y = _G["seg"].run(None, {"x": m.reshape(1, 1, -1)})[0][0]   # (frames, 7)
    cls = y.argmax(axis=1)
    speech = cls != 0
    n_sp = int(speech.sum())
    ov = int(np.isin(cls, (4, 5, 6)).sum())
    return {**spec,
            "ov_frac": (ov / n_sp) if n_sp else 0.0,
            "speech_frac": n_sp / len(cls),
            "vec": _G["emb"](m).tolist()}


def main():
    root = extract_corpus(S / "corpus" / "dev-clean.tar.gz", S / "corpus")
    by = index_corpus(root, N_ENROLL + N_PROBE)
    spk = sorted(by)[:N_SPK]
    idx = {s: i for i, s in enumerate(spk)}

    # Continuous >=10 s source per speaker, built from PROBE utterances only.
    pools, enroll_audio = [], []
    for s in spk:
        u = by[s][:N_ENROLL + N_PROBE]
        enroll_audio.append([rms_normalise(trim_silence(load(x.path))) for x in u[:N_ENROLL]])
        pools.append(rms_normalise(np.concatenate(
            [trim_silence(load(x.path)) for x in u[N_ENROLL:]])))
    print(f"source pools: min {min(len(p) for p in pools)/16000:.1f}s  "
          f"median {np.median([len(p) for p in pools])/16000:.1f}s", flush=True)

    rng = np.random.default_rng(SEED)
    specs = []
    for n, dom in CELLS:
        for s in spk:
            others = [o for o in spk if o != s]
            for _ in range(N_PROBE):
                its = list(rng.choice(others, n - 1, replace=False)) if n > 1 else []
                specs.append(dict(cell=f"{n}tk_{int(dom)}dB", tgt=s, t=idx[s], dom=dom,
                                  itf=[idx[str(x)] for x in its],
                                  itf_spk=[str(x) for x in its],
                                  seed=int(rng.integers(1 << 31))))

    emb = Embedder(EMB, num_threads=8)
    P = np.stack([np.stack([emb(a) for a in per]) for per in enroll_audio])
    print(f"enrolled {P.shape[0]} speakers x {P.shape[1]} prototypes", flush=True)

    with ProcessPoolExecutor(max_workers=24, initializer=_init, initargs=(pools,)) as ex:
        out = list(ex.map(_probe, specs, chunksize=4))
    print(f"{len(out)} probes done\n", flush=True)

    def sims(v):
        return np.einsum("skd,d->sk", P, np.asarray(v, dtype=np.float32)).max(axis=1)

    single = [r for r in out if r["cell"] == "1tk_0dB"]
    imp = []
    for r in single:
        s_ = sims(r["vec"])
        for j in range(len(spk)):
            if j != idx[r["tgt"]]:
                imp.append(s_[j])
    thr = float(np.quantile(np.asarray(imp), 0.99))

    print("=" * 74)
    print("OVERLAP DETECTION  (pyannote segmentation-3.0, powerset classes 4-6)")
    print("=" * 74)
    print(f"  {'cell':>11} {'mean ov%':>9} {'median':>8} "
          + "".join(f"{f'flag@{t:.1f}':>10}" for t in TAUS))
    for n, dom in CELLS:
        rs = [r for r in out if r["cell"] == f"{n}tk_{int(dom)}dB"]
        o = np.array([r["ov_frac"] for r in rs])
        print(f"  {f'{n}tk_{int(dom)}dB':>11} {o.mean()*100:8.1f}% {np.median(o)*100:7.1f}% "
              + "".join(f"{(o > t).mean()*100:9.1f}%" for t in TAUS))

    print("\n" + "=" * 74)
    print(f"GATE TEST — accept if top1>={thr:.3f} AND overlap_frac<=tau")
    print("=" * 74)
    for n, dom in CELLS:
        cell = f"{n}tk_{int(dom)}dB"
        rs = [r for r in out if r["cell"] == cell]
        rows = []
        for tau in [1.01, *TAUS]:
            acc = cor = 0
            for r in rs:
                s_ = sims(r["vec"])
                b = int(np.argmax(s_))
                if s_[b] >= thr and r["ov_frac"] <= tau:
                    acc += 1
                    cor += b == idx[r["tgt"]]
            rows.append((tau, acc / len(rs), (cor / acc if acc else float("nan"))))
        print(f"\n  {cell}")
        print(f"    {'tau':>6} {'coverage':>9} {'precision':>10}")
        for tau, cov, pre in rows:
            lbl = "none" if tau > 1 else f"{tau:.2f}"
            p = "-" if pre != pre else f"{pre*100:9.1f}%"
            print(f"    {lbl:>6} {cov*100:8.1f}% {p:>10}")


if __name__ == "__main__":
    main()
