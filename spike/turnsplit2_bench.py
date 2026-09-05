"""§52 candidates to clear the 50% recall bar §39 missed, without breaking G1/G3.

Reuses the cached 1.5s/0.25s-hop ERes2Net windows from §39
(win_1.50.npz, win_1.00.npz) and the same ground truth
(turnsplit_lib.change_points over Discord's spans). Nothing here re-runs the
embedder: the windows are the same audio, the same model, the same numbers
§39 measured `adjacent`/`contrast`/`proto` from.

Three candidates fit here, chronologically, fit-then-held-out:

  (a) adaptive   -- per-turn median + k*MAD bar on the adjacent curve, so the
                    bar travels with the turn's own noise floor instead of
                    being one absolute number fitted on Discord's.
  (b) combine    -- cut if the absolute `adjacent` bar fires OR the per-turn
                    `contrast` bar fires (their own separately calibrated
                    points), each arm catching what the other's regime misses.
  (d) proto_veto -- an `adjacent` candidate boundary is kept only if the
                    bank's own top-1 voice differs on the two sides. No
                    magnitude bar on the proto side at all -- it is asked one
                    yes/no question, never given a vote on where to cut.

Candidates (c) (0.125s hop, two window lengths voted) and (e) (VAD-dip
refinement) need audio neither cached file was built to answer -- a shorter
hop needs re-embedding at that hop, and refinement needs the VAD's own frame
scores per turn, which the sliding-window pass never asked the daemon for.
Both are costed and left for the next round in the FINDINGS writeup rather
than approximated here.

    python3 spike/turnsplit2_bench.py
"""

import os
import sys
from collections import defaultdict

import numpy as np

import turnsplit_lib as L

SCRATCH2 = "/tmp/nx-recall-workspace/nx-scratch/agents-2026-09-05/turnsplit2"
HOP = 0.25
MIN_PIECE = 1.0
WIN = 1.5


def load_windows(path):
    z = np.load(path)
    ids, starts, vecs = z["ids"], z["starts"], z["vecs"].astype(np.float64)
    vecs /= np.maximum(np.linalg.norm(vecs, axis=1, keepdims=True), 1e-12)
    by = defaultdict(list)
    for i, sid in enumerate(ids):
        by[int(sid)].append(i)
    return {k: (starts[np.array(v)], vecs[np.array(v)]) for k, v in by.items()}


def load_bank(c):
    q = """SELECT p.speaker_id, p.source_segment_id, p.vector
             FROM speaker_prototypes p JOIN speakers s ON s.id = p.speaker_id
            WHERE s.merged_into IS NULL AND p.embed_model_id = 'eres2net_en@1'"""
    sp, src, vec = [], [], []
    for a, b, v in c.execute(q):
        sp.append(a)
        src.append(-1 if b is None else b)
        vec.append(np.frombuffer(v, dtype="<f4").astype(np.float64))
    m = np.stack(vec)
    m /= np.maximum(np.linalg.norm(m, axis=1, keepdims=True), 1e-12)
    return np.array(sp), np.array(src), m


def curve_adjacent(starts, vecs, win=WIN):
    step = int(round(win / HOP))
    if len(starts) <= step:
        return np.zeros(0), np.zeros(0)
    left, right = vecs[:-step], vecs[step:]
    d = 1.0 - np.einsum("ij,ij->i", left, right)
    return starts[:-step] + win, d


def curve_proto_top1(starts, vecs, bank, seg_id, win=WIN):
    """The bank's argmax voice for every window, and the boundary series of
    'does the top-1 differ across this boundary'. No magnitude anywhere."""
    sp, src, m = bank
    keep = src != seg_id
    if not keep.any():
        return np.zeros(0), np.zeros(0, dtype=bool)
    cos = vecs @ m[keep].T
    spk = sp[keep]
    voices = np.unique(spk)
    per = np.stack([cos[:, spk == v].max(axis=1) for v in voices], axis=1)
    top = voices[per.argmax(axis=1)]
    step = int(round(win / HOP))
    if len(starts) <= step:
        return np.zeros(0), np.zeros(0, dtype=bool)
    differ = top[:-step] != top[step:]
    return starts[:-step] + win, differ


def pick(times, scores, dur, tau, min_piece=MIN_PIECE, max_cuts=1, extra_ok=None):
    if len(times) == 0 or dur < 2 * min_piece:
        return []
    ok = (times >= min_piece) & (times <= dur - min_piece) & (scores >= tau)
    if extra_ok is not None:
        ok = ok & extra_ok
    cand = sorted(zip(scores[ok], times[ok]), reverse=True)
    cuts = []
    for s, t in cand:
        if len(cuts) >= max_cuts:
            break
        if all(abs(t - c) >= min_piece for c in cuts):
            cuts.append(t)
    return sorted(cuts)


# ---------------------------------------------------------------------------
# the three candidate detectors
# ---------------------------------------------------------------------------


def cuts_adaptive(t, w, k, min_piece=MIN_PIECE, max_cuts=3):
    starts, vecs = w
    times, d = curve_adjacent(starts, vecs)
    if len(d) == 0:
        return []
    med, mad = np.median(d), np.median(np.abs(d - np.median(d)))
    bar = med + k * mad
    return pick(times, d, t.dur, bar, min_piece, max_cuts)


def cuts_combine(t, w, tau_abs, k_rel, min_piece=MIN_PIECE, max_cuts=3):
    starts, vecs = w
    times, d = curve_adjacent(starts, vecs)
    if len(d) == 0:
        return []
    med, mad = np.median(d), np.median(np.abs(d - np.median(d)))
    bar = min(tau_abs, med + k_rel * mad)  # either arm's own bar fires
    return pick(times, d, t.dur, bar, min_piece, max_cuts)


def cuts_proto_veto(t, w, bank, tau_abs, min_piece=MIN_PIECE, max_cuts=3):
    starts, vecs = w
    times, d = curve_adjacent(starts, vecs)
    if len(d) == 0:
        return []
    _, differ = curve_proto_top1(starts, vecs, bank, t.id)
    n = min(len(d), len(differ))
    return pick(times[:n], d[:n], t.dur, tau_abs, min_piece, max_cuts, extra_ok=differ[:n])


# ---------------------------------------------------------------------------


def score(turns, wins, cutfn, min_piece=MIN_PIECE, max_cuts=3):
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
                if any(any(abs(c - g) <= 0.5 for g in t.changes) for c in cuts):
                    single_split_true += 1
            continue
        truth_n += len(gt)
        used = set()
        for c in cuts:
            hit = None
            for i, g in enumerate(gt):
                if i not in used and abs(c - g) <= 0.5:
                    hit = i
                    break
            if hit is None:
                fp += 1
            else:
                used.add(hit)
                tp += 1
    false = single_split - single_split_true
    return dict(
        tp=tp, fp=fp, truth=truth_n,
        recall=tp / truth_n if truth_n else float("nan"),
        precision=tp / (tp + fp) if (tp + fp) else float("nan"),
        false_rate=false / single_turns if single_turns else float("nan"),
        single_turns=single_turns,
    )


def split_chronological(turns, frac=0.6):
    ordered = sorted(turns, key=lambda t: t.t0)
    n = int(len(ordered) * frac)
    return ordered[:n], ordered[n:]


def main():
    c = L.conn(os.path.join(SCRATCH2, "work.db"))
    turns = L.annotate(c, L.load_turns(c))
    wins = load_windows(os.path.join(SCRATCH2, "win_1.50.npz"))
    bank = load_bank(c)
    fit, held = split_chronological(turns)
    print(f"turns {len(turns)} (fit {len(fit)}, held {len(held)}), "
          f"windowed {len(wins)}")

    def report(name, fn, taus, extra=""):
        print(f"\n== {name} {extra}")
        print(f"  {'param':>10}{'fit recall':>12}{'fit prec':>10}{'fit false':>11}"
              f"{'held recall':>13}{'held prec':>10}{'held false':>12}")
        best = None
        for tau in taus:
            f = score(fit, wins, lambda t, w, tau=tau: fn(t, w, tau))
            if f["false_rate"] <= 0.01 and (best is None or f["recall"] > best[1]["recall"]):
                best = (tau, f)
        for tau in taus:
            f = score(fit, wins, lambda t, w, tau=tau: fn(t, w, tau))
            h = score(held, wins, lambda t, w, tau=tau: fn(t, w, tau))
            mark = "  <-- picked on FIT (G1<=1%)" if best and tau == best[0] else ""
            print(f"  {tau!s:>10}{f['recall']*100:>11.1f}%{f['precision']*100:>9.1f}%"
                  f"{f['false_rate']*100:>10.2f}%"
                  f"{h['recall']*100:>12.1f}%{h['precision']*100:>9.1f}%"
                  f"{h['false_rate']*100:>11.2f}%{mark}")
        return best

    # (a) adaptive: k in MAD units
    report("(a) adaptive median+k*MAD", lambda t, w, k: cuts_adaptive(t, w, k),
           [1.5, 2.0, 2.5, 3.0, 3.5, 4.0, 4.5, 5.0])

    # (b) combine: fix the abs arm at §39's shipped 0.85 and sweep the relative one
    report("(b) combine: min(0.85, med + k*MAD)",
           lambda t, w, k: cuts_combine(t, w, 0.85, k),
           [1.5, 2.0, 2.5, 3.0, 3.5, 4.0])

    # (d) proto veto: sweep the abs bar with the proto-disagreement veto applied
    report("(d) proto veto on top of adjacent",
           lambda t, w, tau: cuts_proto_veto(t, w, bank, tau),
           [0.70, 0.75, 0.80, 0.85, 0.90])


if __name__ == "__main__":
    main()
