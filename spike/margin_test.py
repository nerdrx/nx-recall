"""Can a top1-vs-top2 margin test buy back precision where the threshold fails?

score_ops.py shows the dangerous cell: two talkers at equal loudness gives 92%
coverage at 50% precision. The score alone is high, so a plain threshold accepts it
and emits a confident wrong name.

Hypothesis: in an equal-loudness mix the embedding sits BETWEEN two speakers, so both
of their prototypes score similarly. The absolute score stays high but the GAP to the
runner-up collapses. Requiring a margin should reject exactly those blended frames
while leaving clean single-talker speech untouched.

If that holds, the accept rule is not `top1 >= thr` but
`top1 >= thr AND (top1 - top2) >= margin`.
"""
import sys
from pathlib import Path

import numpy as np

D = Path(__file__).parent
MARGINS = [0.00, 0.03, 0.06, 0.10, 0.15]
CELLS = [("overlap/2tk_0dB", "2 talkers  +0dB  (the dangerous cell)"),
         ("overlap/3tk_0dB", "3 talkers  +0dB"),
         ("overlap/4tk_0dB", "4 talkers  +0dB"),
         ("overlap/10tk_12dB", "10 talkers +12dB (the realistic lobby)"),
         ("overlap/3tk_12dB", "3 talkers  +12dB"),
         ("codec/24k", "1 talker   24k   (must not be harmed)"),
         ("dur/1.0s", "1 talker   1.0s  (must not be harmed)")]


def main(model: str):
    z = np.load(D / f"vecs_{model}.npz", allow_pickle=False)
    spk = list(z["speakers"])
    idx = {s: i for i, s in enumerate(spk)}
    S = np.einsum("skd,nd->nsk", z["protos"], z["probe"]).max(axis=2)
    cond, tgt, itf = z["cond"], z["target"], z["itf"]
    tgt_i = np.array([idx[t] for t in tgt])
    present = [set(idx[x] for x in (s.split("|") if s else [])) for s in itf]

    cal = np.nonzero(cond == "codec/clean")[0]
    imp = [S[n][j] for n in cal for j in range(len(spk))
           if j != tgt_i[n] and j not in present[n]]
    thr = float(np.quantile(np.asarray(imp), 0.99))

    order = np.argsort(-S, axis=1)
    top1 = S[np.arange(len(S)), order[:, 0]]
    top2 = S[np.arange(len(S)), order[:, 1]]
    best = order[:, 0]
    gap = top1 - top2

    print(f"\n{'=' * 78}\n{model} — margin test (threshold {thr:.3f} fixed)\n{'=' * 78}")
    for cname, label in CELLS:
        m = np.nonzero(cond == cname)[0]
        if len(m) == 0:
            continue
        print(f"\n  {label}")
        print(f"    {'margin':>7} {'coverage':>9} {'correct':>9} {'steal':>8} {'outsider':>9}")
        for mg in MARGINS:
            acc = (top1[m] >= thr) & (gap[m] >= mg)
            na = int(acc.sum())
            if na == 0:
                print(f"    {mg:>7.2f} {0.0:>8.1f}% {'-':>9} {'-':>8} {'-':>9}")
                continue
            ok = (best[m] == tgt_i[m]) & acc
            st = np.array([b in present[i] for b, i in zip(best[m], m)]) & acc
            print(f"    {mg:>7.2f} {na/len(m)*100:>8.1f}% {ok.sum()/na*100:>8.1f}% "
                  f"{st.sum()/na*100:>7.1f}% {(na-ok.sum()-st.sum())/na*100:>8.1f}%")


if __name__ == "__main__":
    for mdl in (sys.argv[1:] or ["eres2net", "titanet_small"]):
        if (D / f"vecs_{mdl}.npz").is_file():
            main(mdl)
