"""What splitting does downstream: the verdicts, and the identity ladder.

The "after" arm is the archive as it would be if the daemon had cut every turn
the detector says to cut — `single` turns included, because a false split there
is a duplicated row and it has to be allowed to cost something.

Each piece is re-judged by `truth::verdict`'s rules over its own span and
re-embedded from its own samples, and the identity table is `calib`'s: a
chronological 60/40 split, no row scored against a prototype it produced, the
operating point the install actually has.

    python3 spike/turnsplit_after.py <window> <tau> [min_piece]
"""

import os
import sys
import wave

import numpy as np
import sherpa_onnx

import turnsplit_lib as L
import turnsplit_bench as B

MODEL = os.path.expanduser("~/.local/share/nx-recall/models/eres2net_en.onnx")
DETECTOR = os.environ.get("TURNSPLIT_DETECTOR", "adjacent")
MAX_CUTS = int(os.environ.get("TURNSPLIT_MAX_CUTS", "3"))
PRESENT_MIN = 0.2
SINGLE_MIN = 0.8
LABEL_THRESHOLD = 0.35
MAX_OVERLAP = 0.1
MIN_DURATION_S = 1.0
FIT_FRACTION = 0.6


def read_wav(path):
    with wave.open(path, "rb") as w:
        sr = w.getframerate()
        raw = w.readframes(w.getnframes())
    return np.frombuffer(raw, dtype="<i2").astype(np.float32) / 32768.0, sr


def verdict_of(t, lo_s, hi_s):
    """`truth::verdict` over a piece of the turn, from the same merged spans."""
    dur = hi_s - lo_s
    if dur <= 0:
        return "nobody", None, 0.0
    cov = []
    for u, iv in t.by_user.items():
        tot = 0.0
        for a, b in iv:
            a = (a - t.t0) / L.NS
            b = (b - t.t0) / L.NS
            ov = min(b, hi_s) - max(a, lo_s)
            if ov > 0:
                tot += ov
        cov.append((u, tot / dur))
    present = [(u, f) for u, f in cov if f >= PRESENT_MIN]
    if not present:
        return "nobody", None, 0.0
    if len(present) > 1:
        return "overlap", None, 0.0
    u, f = present[0]
    return ("single" if f >= SINGLE_MIN else "partial"), u, f


class Score:
    def __init__(self):
        self.n = self.correct = self.wrong = self.declined = 0

    def add(self, lab, truth):
        self.n += 1
        if lab is None:
            self.declined += 1
        elif lab == truth:
            self.correct += 1
        else:
            self.wrong += 1

    @property
    def precision(self):
        d = self.correct + self.wrong
        return float("nan") if d == 0 else self.correct / d

    @property
    def recall(self):
        return float("nan") if self.n == 0 else self.correct / self.n

    def f(self, beta=0.5):
        p, r = self.precision, self.recall
        if p != p or r != r:
            return 0.0
        b2 = beta * beta
        d = b2 * p + r
        return 0.0 if d <= 0 else (1 + b2) * p * r / d

    def row(self, name):
        return (f"  {name:<34}{self.n:>6}{self.correct:>9}{self.wrong:>7}{self.declined:>10}"
                f"{self.precision * 100:>10.1f}%{self.recall * 100:>8.1f}%{self.f():>8.3f}")


HEADER = (f"  {'arm':<34}{'n':>6}{'correct':>9}{'wrong':>7}{'declined':>10}"
          f"{'precision':>11}{'recall':>9}{'F-0.5':>8}")


def rank(bank, vec, drop_segment, agg, topk):
    sp, src, m = bank
    n = np.linalg.norm(vec)
    if n <= 0:
        return []
    cos = m @ (vec / n)
    keep = src != drop_segment
    out = []
    for v in np.unique(sp):
        sel = (sp == v) & keep
        if not sel.any():
            continue
        s = cos[sel]
        out.append((int(v), float(s.max()) if agg == "max"
                    else float(np.sort(s)[-min(topk, len(s)):].mean())))
    out.sort(key=lambda x: (-x[1], x[0]))
    return out


def judge(rows, bank, agg, topk):
    s = Score()
    for r in rows:
        if r["overlap"] > MAX_OVERLAP or r["dur"] < MIN_DURATION_S:
            s.add(None, r["truth"])
            continue
        ranked = rank(bank, r["vec"], r["drop"], agg, topk)
        lab = None
        if ranked and ranked[0][1] >= LABEL_THRESHOLD:
            lab = ranked[0][0]
        s.add(lab, r["truth"])
    return s


def main(win, tau, min_piece):
    c = L.conn()
    turns = L.annotate(c, L.load_turns(c))
    wins = B.load_windows(win)
    bank = B.load_bank(c)
    speaker_of = {r[0]: r[1] for r in c.execute(
        "SELECT user_id, speaker_id FROM discord_users WHERE speaker_id IS NOT NULL")}
    stored = {r[0]: (r[1], r[2]) for r in c.execute(
        "SELECT e.segment_id, e.vector, e.embed_model_id FROM embeddings e "
        "WHERE e.id = (SELECT MAX(x.id) FROM embeddings x WHERE x.segment_id = e.segment_id)")}

    cfg = sherpa_onnx.SpeakerEmbeddingExtractorConfig(
        model=MODEL, num_threads=4, debug=False, provider="cpu")
    ex = sherpa_onnx.SpeakerEmbeddingExtractor(cfg)

    def embed(samples, sr):
        s = ex.create_stream()
        s.accept_waveform(sample_rate=sr, waveform=samples)
        s.input_finished()
        return np.array(ex.compute(s), dtype=np.float64)

    moves = {}
    before_rows, after_rows = [], []
    n_split = {"single": 0, "partial": 0, "overlap": 0}
    for t in turns:
        # ---- the row as it stands ----
        if t.verdict == "single" and t.user in speaker_of and t.id in stored:
            before_rows.append(dict(
                t=t.t0, dur=t.dur, overlap=t.overlap_frac, drop=t.id,
                truth=speaker_of[t.user],
                vec=np.frombuffer(stored[t.id][0], dtype="<f4").astype(np.float64)))

        w = wins.get(t.id)
        cuts = []
        if w is not None and t.dur >= 2 * min_piece:
            times, sc = B.curve_for(DETECTOR, w[0], w[1], win, bank, t.id)
            cuts = B.pick(times, sc, t.dur, tau, min_piece, MAX_CUTS)
        if not cuts:
            if t.verdict == "single" and t.user in speaker_of and t.id in stored:
                after_rows.append(before_rows[-1])
            continue
        n_split[t.verdict] += 1
        p = os.path.join(L.CLIPS, t.path)
        if not os.path.exists(p):
            continue
        samples, sr = read_wav(p)
        edges = [0.0] + cuts + [t.dur]
        for k, (lo, hi) in enumerate(zip(edges, edges[1:])):
            v, u, frac = verdict_of(t, lo, hi)
            moves[(t.verdict, v)] = moves.get((t.verdict, v), 0) + 1
            if v != "single" or u not in speaker_of:
                continue
            piece = samples[int(lo * sr):int(hi * sr)]
            if len(piece) < sr:
                continue
            after_rows.append(dict(
                t=t.t0 + int(lo * L.NS), dur=hi - lo, overlap=t.overlap_frac,
                drop=t.id if k == 0 else -1, truth=speaker_of[u],
                vec=embed(piece, sr)))

    print("\n  turns the detector cuts")
    for v in ("single", "partial", "overlap"):
        tot = sum(1 for t in turns if t.verdict == v)
        print(f"    {v:<9}{n_split[v]:>6} of {tot}")
    print("\n  what the pieces are judged as")
    print(f"    {'from':<10}{'to':<10}{'pieces':>8}")
    for (a, b), n in sorted(moves.items(), key=lambda x: -x[1]):
        print(f"    {a:<10}{b:<10}{n:>8}")

    for arm, rows in (("before", before_rows), ("after", after_rows)):
        rows.sort(key=lambda r: (r["t"],))
    print(f"\n  identity corpus: before {len(before_rows)} rows, after {len(after_rows)}")

    for agg, topk in (("max", 1), ("topk", 3)):
        print(f"\n  aggregate {agg}{topk if agg == 'topk' else ''}, "
              f"global threshold {LABEL_THRESHOLD}")
        print(HEADER)
        for arm, rows in (("before", before_rows), ("after", after_rows)):
            cut = int(round(len(rows) * FIT_FRACTION))
            held = rows[cut:]
            print(judge(held, bank, agg, topk).row(f"{arm} (held out)"))


if __name__ == "__main__":
    main(float(sys.argv[1]), float(sys.argv[2]),
         float(sys.argv[3]) if len(sys.argv) > 3 else 1.0)
