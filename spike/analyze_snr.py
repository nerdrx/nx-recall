"""Does talker count matter on its own, or only through signal-to-babble ratio?

Each interferer is RMS-matched to the target then attenuated by `dominance` dB, so
with K = N-1 interferers the summed babble power is K * 10^(-dom/10) relative to the
target:

    SNR_dB = dominance_dB - 10*log10(K)

If the overlap grid collapses onto this one axis, then "how many people are talking"
is the wrong design variable and "how much louder is the one you care about" is the
right one.
"""
import json
import math
from pathlib import Path

R = json.loads((Path(__file__).parent / "results.json").read_text())

for model, conds in R["results"].items():
    pts = []
    for name, d in conds.items():
        if not name.startswith("overlap/"):
            continue
        tk, dom = name.split("/")[1].split("_")
        n, dom = int(tk[:-2]), float(dom[:-2])
        k = n - 1
        if k == 0:
            continue
        pts.append((dom - 10 * math.log10(k), n, dom, d))
    pts.sort()

    print(f"\n{'=' * 70}\n{model} — overlap grid re-sorted by signal-to-babble ratio\n{'=' * 70}")
    print(f"{'SNR dB':>7} {'talkers':>8} {'dom':>6} {'TAR@1%':>8} {'rank-1':>8} {'steal':>7}")
    print("-" * 70)
    for snr, n, dom, d in pts:
        print(f"{snr:>7.1f} {n:>8} {dom:>5.0f}dB {d['tar_at_far1']*100:>7.1f}% "
              f"{d['rank1']*100:>7.1f}% {d['steal']*100:>6.1f}%")

    # If the collapse holds, cells with near-identical SNR but very different talker
    # counts should agree closely.
    print("\n  same-SNR / different-talker-count pairs:")
    for i in range(len(pts)):
        for j in range(i + 1, len(pts)):
            if abs(pts[i][0] - pts[j][0]) < 0.75 and pts[i][1] != pts[j][1]:
                a, b = pts[i], pts[j]
                print(f"    SNR~{a[0]:+.1f}dB : {a[1]:2d}tk@{a[2]:.0f}dB={a[3]['tar_at_far1']*100:5.1f}%"
                      f"  vs  {b[1]:2d}tk@{b[2]:.0f}dB={b[3]['tar_at_far1']*100:5.1f}%"
                      f"   (delta {abs(a[3]['tar_at_far1']-b[3]['tar_at_far1'])*100:.1f}pp)")
