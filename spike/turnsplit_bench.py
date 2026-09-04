"""The change detector, measured against Discord's per-user spans.

Two families, both over the daemon's own ERes2Net windows:

  `adjacent`   1 - cos(window ending at t, window starting at t). No bank, no
               model beyond the one that already runs.
  `proto`      each window's nearest voice in the live bank, restricted to the
               turn's two best candidates; a change is where the argmax flips.

Scored on: change-point recall/precision at +-0.3 s and +-0.5 s, and the false
split rate on `single` turns, where a split is a duplicated row.

    python3 spike/turnsplit_bench.py [win]
"""

import os
import sys
from collections import defaultdict

import numpy as np

import turnsplit_lib as L

HOP = 0.25
MIN_PIECE = 1.0


def load_windows(win):
    z = np.load(os.path.join(L.SCRATCH, f"win_{win:.2f}.npz"))
    ids, starts, vecs = z["ids"], z["starts"], z["vecs"].astype(np.float64)
    vecs /= np.maximum(np.linalg.norm(vecs, axis=1, keepdims=True), 1e-12)
    by = defaultdict(list)
    for i, sid in enumerate(ids):
        by[int(sid)].append(i)
    return {k: (starts[v], vecs[v]) for k, v in by.items()}


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


# ---------------------------------------------------------------------------
# detectors: each returns a curve of (time, score) candidate cut points
# ---------------------------------------------------------------------------


def curve_adjacent(starts, vecs, win):
    """1 - cos between the window ending at t and the window starting at t."""
    step = int(round(win / HOP))
    if len(starts) <= step:
        return np.zeros(0), np.zeros(0)
    left, right = vecs[:-step], vecs[step:]
    d = 1.0 - np.einsum("ij,ij->i", left, right)
    return starts[:-step] + win, d


def curve_contrast(starts, vecs, win):
    """The adjacent distance, less the turn's own median of it.

    The absolute number is useless as a bar: two 1 s windows of the *same*
    person score 1 - cos ~ 0.62 in this space (FINDINGS §32's same-speaker mean
    of 0.377), so every turn is already over any threshold worth naming. What
    carries information is how far the peak stands above the rest of the curve
    in the same turn, on the same voice, at the same loudness.
    """
    times, d = curve_adjacent(starts, vecs, win)
    if len(d) == 0:
        return times, d
    return times, d - np.median(d)


def curve_named(starts, vecs, win, bank, seg_id, threshold=0.35):
    """Split only where the two sides each name a voice, and a *different* one.

    The strongest bar available, and the one tied to what a split is for: a
    piece exists to be labelled, so a cut nobody can label either side of has
    bought nothing. The score is the weaker of the two claims.
    """
    sp, src, m = bank
    keep = src != seg_id
    cos = vecs @ m[keep].T
    spk = sp[keep]
    voices = np.unique(spk)
    per = np.stack([cos[:, spk == v].max(axis=1) for v in voices], axis=1)
    top = per.argmax(axis=1)
    best = per.max(axis=1)
    step = int(round(win / HOP))
    if len(starts) <= step:
        return np.zeros(0), np.zeros(0)
    lo, hi = np.arange(len(starts) - step), np.arange(step, len(starts))
    differ = top[lo] != top[hi]
    score = np.minimum(best[lo], best[hi])
    score = np.where(differ & (score >= threshold), score, -1.0)
    return starts[lo] + win, score


def curve_proto(starts, vecs, win, bank, seg_id, top=2):
    """How far the two best candidate voices' scores swing across the turn.

    The cut score at t is how much better voice A is on the left than on the
    right, minus the same for voice B: a flip in who is winning.
    """
    sp, src, m = bank
    keep = src != seg_id
    cos = vecs @ m[keep].T
    spk = sp[keep]
    voices = np.unique(spk)
    # a voice's score for a window: max over its prototypes (the ladder's rank)
    per = np.stack([cos[:, spk == v].max(axis=1) for v in voices], axis=1)
    order = np.argsort(-per.mean(axis=0))[:top]
    if len(order) < 2:
        return np.zeros(0), np.zeros(0)
    a, b = per[:, order[0]], per[:, order[1]]
    diff = a - b
    step = int(round(win / HOP))
    if len(starts) <= step:
        return np.zeros(0), np.zeros(0)
    # |mean(diff) left of t - mean(diff) right of t| over one window each side
    swing = np.abs(diff[:-step] - diff[step:])
    return starts[:-step] + win, swing


# ---------------------------------------------------------------------------
# peak picking
# ---------------------------------------------------------------------------


def pick(times, scores, dur, tau, min_piece=MIN_PIECE, max_cuts=1):
    """The strongest peaks above `tau` that leave no piece shorter than
    `min_piece`. Greedy, strongest first — a split is a permanent extra row."""
    if len(times) == 0 or dur < 2 * min_piece:
        return []
    ok = (times >= min_piece) & (times <= dur - min_piece) & (scores >= tau)
    cand = sorted(zip(scores[ok], times[ok]), reverse=True)
    cuts = []
    for s, t in cand:
        if len(cuts) >= max_cuts:
            break
        if all(abs(t - c) >= min_piece for c in cuts):
            cuts.append(t)
    return sorted(cuts)


# ---------------------------------------------------------------------------


def curve_for(detector, starts, vecs, win, bank, seg_id):
    if detector == "adjacent":
        return curve_adjacent(starts, vecs, win)
    if detector == "contrast":
        return curve_contrast(starts, vecs, win)
    if detector == "named":
        return curve_named(starts, vecs, win, bank, seg_id)
    if detector == "proto":
        return curve_proto(starts, vecs, win, bank, seg_id)
    if detector == "proto+contrast":
        # An AND of two bars on one scale: the proto swing, but only where the
        # adjacent distance also stands above the turn's own median.
        ta, a = curve_proto(starts, vecs, win, bank, seg_id)
        tb, b = curve_contrast(starts, vecs, win)
        n = min(len(a), len(b))
        return ta[:n], np.where(b[:n] > 0.05, a[:n], -1.0)
    raise ValueError(detector)


def score_arm(turns, wins, detector, tau, tol, min_piece=MIN_PIECE, max_cuts=1,
              bank=None, win=1.0, min_turn=0.0):
    tp = fp = 0
    truth_n = 0
    single_turns = single_cuttable = single_split = single_split_true = 0
    matched_turns = 0
    for t in turns:
        w = wins.get(t.id)
        if w is None:
            continue
        cuttable = t.dur >= max(2 * min_piece, min_turn)
        if t.verdict == "single":
            single_turns += 1
            single_cuttable += cuttable
        if not cuttable:
            continue
        starts, vecs = w
        times, sc = curve_for(detector, starts, vecs, win, bank, t.id)
        cuts = pick(times, sc, t.dur, tau, min_piece, max_cuts)
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
        if used:
            matched_turns += 1
    false = single_split - single_split_true
    return dict(tp=tp, fp=fp, truth=truth_n,
                recall=tp / truth_n if truth_n else float("nan"),
                precision=tp / (tp + fp) if (tp + fp) else float("nan"),
                single_turns=single_turns, single_cuttable=single_cuttable,
                single_split=single_split, single_true=single_split_true,
                false_all=false / single_turns if single_turns else float("nan"),
                false_cuttable=false / single_cuttable if single_cuttable else float("nan"),
                matched_turns=matched_turns)


def table(turns, wins, bank, win, detector, taus, max_cuts=1, min_piece=MIN_PIECE,
          min_turn=0.0):
    print(f"\n  detector {detector}, window {win:.2f}s, hop {HOP}s, "
          f"min piece {min_piece}s, min turn {max(2 * min_piece, min_turn):.2f}s, "
          f"<= {max_cuts} cut(s)")
    print(f"  {'tau':>6}{'recall .3':>11}{'prec .3':>10}{'recall .5':>11}{'prec .5':>10}"
          f"{'cuts':>7}{'false/single':>14}{'false/cuttable':>16}")
    for tau in taus:
        a3 = score_arm(turns, wins, detector, tau, 0.3, min_piece, max_cuts, bank, win,
                       min_turn)
        a5 = score_arm(turns, wins, detector, tau, 0.5, min_piece, max_cuts, bank, win,
                       min_turn)
        print(
            f"  {tau:>6.2f}"
            f"{a3['recall'] * 100:>10.1f}%{a3['precision'] * 100:>9.1f}%"
            f"{a5['recall'] * 100:>10.1f}%{a5['precision'] * 100:>9.1f}%"
            f"{a5['tp'] + a5['fp']:>7}{a5['false_all'] * 100:>13.2f}%"
            f"{a5['false_cuttable'] * 100:>15.2f}%"
        )


if __name__ == "__main__":
    win = float(sys.argv[1]) if len(sys.argv) > 1 else 1.0
    c = L.conn()
    turns = L.annotate(c, L.load_turns(c))
    wins = load_windows(win)
    bank = load_bank(c)
    gt = sum(1 for t in turns if t.verdict != "single"
             for cp in t.changes if cp >= MIN_PIECE and (t.dur - cp) >= MIN_PIECE)
    allcp = sum(len(t.changes) for t in turns if t.verdict != "single")
    ncut = sum(1 for t in turns if t.verdict == "single" and t.dur >= 2 * MIN_PIECE)
    print(f"turns {len(turns)}, windowed {len(wins)}, change points {allcp}, "
          f"reachable at min piece {MIN_PIECE}s {gt}, cuttable singles {ncut}")
    table(turns, wins, bank, win, "adjacent",
          [0.50, 0.60, 0.70, 0.75, 0.80, 0.85, 0.90, 0.95])
    table(turns, wins, bank, win, "contrast",
          [0.00, 0.05, 0.10, 0.15, 0.20, 0.25, 0.30])
    table(turns, wins, bank, win, "proto",
          [0.20, 0.25, 0.30, 0.35, 0.40, 0.50])
    table(turns, wins, bank, win, "proto+contrast",
          [0.15, 0.20, 0.25, 0.30, 0.35, 0.40])
    table(turns, wins, bank, win, "named",
          [0.35, 0.40, 0.45, 0.50, 0.55])
    # Does giving each piece more audio buy the precision back?
    table(turns, wins, bank, win, "proto",
          [0.20, 0.25, 0.30, 0.35, 0.40], min_piece=1.5)
    table(turns, wins, bank, win, "adjacent",
          [0.75, 0.80, 0.85, 0.90], min_piece=1.5)
