"""The product's actual operating point.

`report.py` tunes a threshold per condition, which no shipping system can do — at
runtime you have one mixed stream and no idea how many people are talking or how
loud they are relative to each other.

So: calibrate ONE global threshold on clean single-talker audio (the calibration
material you'd realistically have), then apply it unchanged to every condition and
ask the two questions that decide whether the tool works:

  coverage  — what fraction of speech clears the bar and gets labelled at all?
  precision — of the labels it DOES emit, how many are right?

An accepted-but-wrong label is far worse than no label: it writes a wrong name into
the transcript, and if auto-enrollment is on it feeds a wrong prototype back into the
voicebank, which is the permanent-corruption path the design calls poison.
"""
import sys
from pathlib import Path

import numpy as np

D = Path(__file__).parent
TK = [1, 2, 3, 4, 6, 8, 10]
DOM = [0, 6, 12]
FAR = 0.01


def sims_for(z):
    P, probe = z["protos"], z["probe"]          # (S,K,D), (N,D)
    return np.einsum("skd,nd->nsk", P, probe).max(axis=2)   # (N,S) best prototype


def main(model: str):
    z = np.load(D / f"vecs_{model}.npz", allow_pickle=False)
    spk = list(z["speakers"])
    idx = {s: i for i, s in enumerate(spk)}
    S = sims_for(z)
    cond, tgt, itf = z["cond"], z["target"], z["itf"]
    tgt_i = np.array([idx[t] for t in tgt])
    present = [set(idx[x] for x in (s.split("|") if s else [])) for s in itf]

    # --- calibrate on clean single-talker impostor scores ---
    cal = cond == "codec/clean"
    imp = []
    for n in np.nonzero(cal)[0]:
        row = S[n]
        for j in range(len(spk)):
            if j != tgt_i[n] and j not in present[n]:
                imp.append(row[j])
    thr = float(np.quantile(np.asarray(imp), 1 - FAR))
    print(f"\n{'=' * 76}\n{model} — one global threshold, calibrated on CLEAN audio "
          f"at FAR={FAR:.0%}\n  threshold = {thr:.3f}\n{'=' * 76}")

    def block(mask):
        n = np.nonzero(mask)[0]
        if len(n) == 0:
            return None
        best = S[n].argmax(axis=1)
        top = S[n].max(axis=1)
        acc = top >= thr
        na = int(acc.sum())
        if na == 0:
            return dict(cov=0.0, corr=float("nan"), steal=float("nan"), out=float("nan"))
        ok = (best == tgt_i[n]) & acc
        st = np.array([b in present[i] for b, i in zip(best, n)]) & acc
        return dict(cov=na / len(n), corr=ok.sum() / na, steal=st.sum() / na,
                    out=(na - ok.sum() - st.sum()) / na)

    print("\n  OVERLAP — coverage / precision at the fixed threshold")
    print("     (cov = % of speech labelled;  correct/steal/outsider = % OF THOSE LABELS)")
    for dm in DOM:
        print(f"\n   dominance +{dm}dB")
        print(f"   {'talkers':>7} {'coverage':>9} {'correct':>9} {'steal':>8} {'outsider':>9}")
        for n in TK:
            r = block(cond == f"overlap/{n}tk_{dm}dB")
            if r:
                print(f"   {n:>7} {r['cov']*100:>8.1f}% {r['corr']*100:>8.1f}% "
                      f"{r['steal']*100:>7.1f}% {r['out']*100:>8.1f}%")

    print("\n  DURATION (1 talker, 24k)          CODEC (1 talker, 3s)")
    print(f"   {'win':>5} {'cov':>7} {'corr':>7}          {'rate':>6} {'cov':>7} {'corr':>7}")
    durs = ["0.5s", "1.0s", "2.0s", "3.0s", "5.0s", "8.0s"]
    brs = ["clean", "32k", "24k", "16k", "12k", "8k"]
    for d_, b_ in zip(durs, brs):
        a, b = block(cond == f"dur/{d_}"), block(cond == f"codec/{b_}")
        print(f"   {d_:>5} {a['cov']*100:>6.1f}% {a['corr']*100:>6.1f}%          "
              f"{b_:>6} {b['cov']*100:>6.1f}% {b['corr']*100:>6.1f}%")


if __name__ == "__main__":
    for m in (sys.argv[1:] or ["eres2net", "titanet_small"]):
        if (D / f"vecs_{m}.npz").is_file():
            main(m)
