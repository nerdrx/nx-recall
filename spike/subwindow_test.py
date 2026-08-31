"""Can sub-window agreement detect the overlap that the score cannot?

margin_test.py showed the failure is not geometric: in an equal-loudness mix the
embedding is captured by ONE talker (arbitrarily), scoring high and confidently wrong.
No threshold or top1-vs-top2 margin sees it.

But if capture is arbitrary, it should also be UNSTABLE: split the window into thirds
and each third may be captured by a different talker. Clean or strongly-dominant
speech should be stable across all three.

Accept rule under test:  top1 >= thr  AND  all three sub-windows agree on the speaker.
"""
import os
import sys
from concurrent.futures import ProcessPoolExecutor
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).parent))
from harness import (Embedder, extract_corpus, index_corpus, load, mix,  # noqa: E402
                     opus_roundtrip, rms_normalise, take_window, trim_silence)

S = Path(os.environ.get("NXR_SCRATCH", "/tmp/nxr"))
MODEL = S / "models" / "eres2net_en.onnx"
N_SPK, N_ENROLL, N_PROBE = 40, 3, 5
DUR, BR, SEED = 3.0, 24, 0xBEEF
CELLS = [(2, 0.0), (3, 0.0), (4, 0.0), (2, 12.0), (3, 12.0), (10, 12.0), (1, 0.0)]
_G: dict = {}


def _init(paths):
    _G["e"] = Embedder(MODEL, num_threads=1)
    _G["a"] = [rms_normalise(trim_silence(load(Path(p)))) for p in paths]


def _enroll(u):
    return _G["e"](_G["a"][u]).tolist()


def _probe(spec):
    rng = np.random.default_rng(spec["seed"])
    a = _G["a"]

    def prep(i):
        return opus_roundtrip(take_window(a[i], DUR, rng), BR)

    m = mix(prep(spec["t"]), [prep(i) for i in spec["itf"]], spec["dom"])
    n3 = len(m) // 3
    return {**spec,
            "full": _G["e"](m).tolist(),
            "subs": [_G["e"](m[k * n3:(k + 1) * n3]).tolist() for k in range(3)]}


def main():
    root = extract_corpus(S / "corpus" / "dev-clean.tar.gz", S / "corpus")
    by = index_corpus(root, N_ENROLL + N_PROBE)
    spk = sorted(by)[:N_SPK]
    idx = {s: i for i, s in enumerate(spk)}

    utts, en, pr = [], {}, {}
    for s in spk:
        base = len(utts)
        utts.extend(by[s][: N_ENROLL + N_PROBE])
        en[s] = list(range(base, base + N_ENROLL))
        pr[s] = list(range(base + N_ENROLL, base + N_ENROLL + N_PROBE))
    paths = [str(u.path) for u in utts]

    rng = np.random.default_rng(SEED)
    specs = []
    for n, dom in CELLS:
        for s in spk:
            others = [o for o in spk if o != s]
            for p in pr[s]:
                its = list(rng.choice(others, n - 1, replace=False)) if n > 1 else []
                specs.append(dict(cell=f"{n}tk_{int(dom)}dB", tgt=s, t=p, dom=dom,
                                  itf=[int(rng.choice(pr[x])) for x in its],
                                  itf_spk=[str(x) for x in its],
                                  seed=int(rng.integers(1 << 31))))

    with ProcessPoolExecutor(max_workers=24, initializer=_init, initargs=(paths,)) as ex:
        P = np.stack([np.stack([np.asarray(v, dtype=np.float32)
                                for v in ex.map(_enroll, en[s])]) for s in spk])
        out = list(ex.map(_probe, specs, chunksize=8))

    def sims(v):
        return np.einsum("skd,d->sk", P, np.asarray(v, dtype=np.float32)).max(axis=1)

    # calibrate on the single-talker cell
    imp = []
    for r in out:
        if r["cell"] != "1tk_0dB":
            continue
        s_ = sims(r["full"])
        ti = idx[r["tgt"]]
        imp += [s_[j] for j in range(len(spk)) if j != ti]
    thr = float(np.quantile(np.asarray(imp), 0.99))
    print(f"\nsub-window agreement test (eres2net, threshold {thr:.3f})")
    print("  accept = top1>=thr [+ all 3 sub-windows agree]\n")
    print(f"  {'cell':>11} | {'plain cov':>9} {'prec':>6} | {'agree cov':>9} {'prec':>6} | {'d prec':>6}")
    print("  " + "-" * 66)

    for n, dom in CELLS:
        cell = f"{n}tk_{int(dom)}dB"
        rs = [r for r in out if r["cell"] == cell]
        pc = pn = ac = an = 0
        for r in rs:
            sf = sims(r["full"])
            b = int(np.argmax(sf))
            if sf[b] < thr:
                continue
            ok = b == idx[r["tgt"]]
            pn += 1
            pc += ok
            votes = {int(np.argmax(sims(v))) for v in r["subs"]}
            if len(votes) == 1 and votes.pop() == b:
                an += 1
                ac += ok
        pp, ap = (pc / pn * 100 if pn else 0), (ac / an * 100 if an else 0)
        print(f"  {cell:>11} | {pn/len(rs)*100:8.1f}% {pp:5.1f}% | "
              f"{an/len(rs)*100:8.1f}% {ap:5.1f}% | {ap-pp:+5.1f}pp")


if __name__ == "__main__":
    main()
