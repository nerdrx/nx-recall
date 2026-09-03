"""Score the arms written by sep_bench.py against the shipping operating point.

Bars are the daemon's, not this bench's: the global label bar 0.35, and the
per-voice bars §18 fitted (Rowan 0.31, Aspen 0.41) reported alongside. A row
counts as **both recovered** only when the two streams' open-set top-1 labels,
each clearing its own bar, are exactly the two people Discord names.

Usage: NXR_SEP=<dir> python sep_score.py sep_acoustic.json [more.json ...]
"""
import json
import os
import sys

import numpy as np

D = os.environ["NXR_SEP"]
os.chdir(D)
import sep_lib as L  # noqa: E402


def labelled(rank, per_voice):
    """Top-1 if it clears its own bar, else None -- `identity::decide_with`."""
    if not rank:
        return None
    sid, sc = rank[0]
    return sid if sc >= L.thr(sid, per_voice) else None


def score(rows, per_voice):
    n = len(rows)
    s = dict(n=n, mix_labelled=0, mix_correct=0, mix_steal=0,
             sep_labelled=0, sep_both=0, sep_one=0, sep_steal=0,
             assign_both=0, assign_one=0)
    dmix, dsep = [], []
    for r in rows:
        true = set(r["true_sids"])
        # --- mixture (today) -------------------------------------------
        top = labelled(r["mix_top"], per_voice)
        if top is not None:
            s["mix_labelled"] += 1
            if top in true:
                s["mix_correct"] += 1
            else:
                s["mix_steal"] += 1
        dmix.append(max(r["mix_true"]))
        if "sep_top" not in r:
            continue
        # --- separation, open set --------------------------------------
        got = [labelled(rk, per_voice) for rk in r["sep_top"]]
        named = {g for g in got if g is not None}
        s["sep_labelled"] += sum(g is not None for g in got)
        if named == true:
            s["sep_both"] += 1
        if named & true:
            s["sep_one"] += 1
        s["sep_steal"] += len(named - true)
        # --- separation + assignment (closed set, Q2) -------------------
        m = np.array(r["sep_true"])  # [stream][true-user]
        ta, tb = (L.thr(sid, per_voice) for sid in r["true_sids"])
        direct = m[0, 0] >= ta and m[1, 1] >= tb
        swap = m[0, 1] >= tb and m[1, 0] >= ta
        if direct or swap:
            s["assign_both"] += 1
        if max(m[0, 0], m[1, 0]) >= ta or max(m[0, 1], m[1, 1]) >= tb:
            s["assign_one"] += 1
        # threshold-free: best cosine to each true user, best over streams
        dsep.append(float(max(m.max(axis=0))))
    s["mix_cos_mean"] = round(float(np.mean(dmix)), 4)
    if dsep:
        s["sep_cos_mean"] = round(float(np.mean(dsep)), 4)
    return s


def pct(a, b):
    return f"{100*a/b:5.1f}%" if b else "    - "


for path in sys.argv[1:]:
    rows = json.load(open(path))
    print(f"\n=== {path}  n={len(rows)} ===")
    print(f"{'bars':<12} {'mix lab':>8} {'mix ok':>8} {'mix steal':>10} "
          f"{'sep both':>9} {'sep one':>8} {'sep steal':>10} {'assign both':>12}")
    for per_voice, tag in ((False, "global .35"), (True, "per-voice")):
        s = score(rows, per_voice)
        n = s["n"]
        has_sep = any("sep_top" in r for r in rows)
        print(f"{tag:<12} {pct(s['mix_labelled'],n):>8} {pct(s['mix_correct'],n):>8} "
              f"{pct(s['mix_steal'],n):>10} "
              f"{pct(s['sep_both'],n) if has_sep else '   -':>9} "
              f"{pct(s['sep_one'],n) if has_sep else '   -':>8} "
              f"{s['sep_steal'] if has_sep else '-':>10} "
              f"{pct(s['assign_both'],n) if has_sep else '   -':>12}")
    s = score(rows, False)
    print(f"cosine to the true voice, best available: mixture {s['mix_cos_mean']}"
          + (f" -> separated {s['sep_cos_mean']}" if "sep_cos_mean" in s else ""))

    # by how overlapped the turn really is
    if any("sep_top" in r for r in rows):
        print(f"\n{'truth overlap':>14} {'n':>5} {'mix ok':>8} {'sep both':>9} "
              f"{'assign both':>12} {'mix cos':>8} {'sep cos':>8}")
        for lo, hi in ((0, .1), (.1, .25), (.25, .5), (.5, 1.01)):
            sub = [r for r in rows
                   if r["truth_overlap_frac"] is not None
                   and lo <= r["truth_overlap_frac"] < hi]
            if not sub:
                continue
            s = score(sub, False)
            print(f"{lo:.2f}-{hi:.2f}".rjust(14) + f" {s['n']:>5} "
                  f"{pct(s['mix_correct'],s['n']):>8} {pct(s['sep_both'],s['n']):>9} "
                  f"{pct(s['assign_both'],s['n']):>12} {s['mix_cos_mean']:>8} "
                  f"{s.get('sep_cos_mean',0):>8}")
