"""Detailed table for the winning arm from turnsplit2_bench.py: (d) proto veto
on top of `adjacent`, at the fit-split-picked tau. Mirrors FINDINGS §39's own
table shape (recall/precision at +-0.3s and +-0.5s, false/single against both
denominators) so the two are directly comparable.
"""
import os
import numpy as np
import turnsplit_lib as L
import turnsplit2_bench as B

SCRATCH2 = B.SCRATCH2


def score_tol(turns, wins, cutfn, tol, min_piece=B.MIN_PIECE):
    tp = fp = 0
    truth_n = 0
    single_turns = single_cuttable = single_split = single_split_true = 0
    for t in turns:
        w = wins.get(t.id)
        if w is None:
            continue
        cuttable = t.dur >= 2 * min_piece
        if t.verdict == "single":
            single_turns += 1
            single_cuttable += cuttable
        if not cuttable:
            continue
        cuts = cutfn(t, w)
        gt = [cp for cp in t.changes if cp >= min_piece and (t.dur - cp) >= min_piece]
        if t.verdict == "single":
            if cuts:
                single_split += 1
                if any(any(abs(c - g) <= tol for g in t.changes) for c in cuts):
                    single_split_true += 1
            continue
        truth_n += len(gt)
        used = set()
        for c in cuts:
            hit = None
            for i, g in enumerate(gt):
                if i not in used and abs(c - g) <= tol:
                    hit = i
                    break
            if hit is None:
                fp += 1
            else:
                used.add(hit)
                tp += 1
    false = single_split - single_split_true
    return dict(tp=tp, fp=fp, truth=truth_n,
                recall=tp / truth_n if truth_n else float("nan"),
                precision=tp / (tp + fp) if (tp + fp) else float("nan"),
                single_turns=single_turns, single_cuttable=single_cuttable,
                false_all=false / single_turns if single_turns else float("nan"),
                false_cuttable=false / single_cuttable if single_cuttable else float("nan"))


def main():
    c = L.conn(os.path.join(SCRATCH2, "work.db"))
    turns = L.annotate(c, L.load_turns(c))
    wins = B.load_windows(os.path.join(SCRATCH2, "win_1.50.npz"))
    bank = B.load_bank(c)

    allcp = sum(len(t.changes) for t in turns if t.verdict != "single")
    reachable = sum(1 for t in turns if t.verdict != "single"
                     for cp in t.changes if cp >= B.MIN_PIECE and (t.dur - cp) >= B.MIN_PIECE)
    print(f"all change points {allcp}, reachable at min_piece {B.MIN_PIECE}s: {reachable}")

    fit, held = B.split_chronological(turns)

    for tau in [0.75, 0.78, 0.80, 0.82, 0.85]:
        fn = lambda t, w, tau=tau: B.cuts_proto_veto(t, w, bank, tau)
        print(f"\n-- proto_veto tau={tau} --")
        for label, pop in (("fit", fit), ("held", held), ("whole", turns)):
            r3 = score_tol(pop, wins, fn, 0.3)
            r5 = score_tol(pop, wins, fn, 0.5)
            rr = f"{100*r5['tp']/reachable:.1f}%" if label == "whole" else "n/a"
            print(f"  {label:>6}: recall.3 {r3['recall']*100:5.1f}% prec.3 {r3['precision']*100:5.1f}%  "
                  f"recall.5 {r5['recall']*100:5.1f}% prec.5 {r5['precision']*100:5.1f}%  "
                  f"false/single(all) {r5['false_all']*100:5.2f}%  false/single(cuttable) {r5['false_cuttable']*100:5.2f}%  "
                  f"reachable-recall.5 {rr}")


if __name__ == "__main__":
    main()
