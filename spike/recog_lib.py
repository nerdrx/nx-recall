"""Faithful Python port of recalld's identity replay, for the recognition round.

Mirrors, and is validated against, the Rust that ships:

  store.truth_calibration_rows(1.0)   -> load_rows()
  store.prototypes_with_source()      -> load_bank()
  identity::rank / decide_with        -> rank() / decide()
  calib::split_at / Score / the gates -> split_at() / Score / may_install()

`spike/recog_bench.py --validate` reproduces
`cargo run -p recalld --example learn_truth_bench` arm for arm; if that check
fails, nothing else in this file is worth reading.

Read-only against a *copy* of the database. Never the live one.
"""

import sqlite3
import numpy as np

DB = "/tmp/nx-recall-workspace/nx-scratch/agents-2026-09-04/recog/work.db"
MODEL = "eres2net_en@1"
MIN_DURATION_S = 1.0
FIT_FRACTION = 0.6
BETA = 0.5
LABEL_THRESHOLD = 0.35
MAX_OVERLAP = 0.1
GATE_MIN_DURATION_S = 1.0
MIN_ROWS_PER_VOICE = 30
THRESHOLD_BOUNDS = (0.30, 0.60)
MARGIN_GRID = [0.0, 0.02, 0.04, 0.06, 0.08]


def conn(path=DB):
    return sqlite3.connect(f"file:{path}?mode=ro", uri=True)


def blob_to_vec(b):
    return np.frombuffer(b, dtype="<f4").astype(np.float64)


class Score:
    __slots__ = ("n", "correct", "wrong", "declined")

    def __init__(self):
        self.n = self.correct = self.wrong = self.declined = 0

    def add(self, labelled, truth):
        self.n += 1
        if labelled is None:
            self.declined += 1
        elif labelled == truth:
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

    def f_beta(self, beta=BETA):
        p, r = self.precision, self.recall
        if p != p or r != r:
            return 0.0
        b2 = beta * beta
        denom = b2 * p + r
        return 0.0 if denom <= 0 else (1 + b2) * p * r / denom

    def row(self, name):
        return (
            f"  {name:<38}{self.n:>5}{self.correct:>9}{self.wrong:>7}{self.declined:>10}"
            f"{self.precision * 100:>10.1f}%{self.recall * 100:>8.1f}%{self.f_beta():>8.3f}"
        )


HEADER = (
    f"  {'arm':<38}{'n':>5}{'correct':>9}{'wrong':>7}{'declined':>10}"
    f"{'precision':>11}{'recall':>9}{'F-0.5':>8}"
)


def swap_is_safe(inc, cand):
    p0, p1 = inc.precision, cand.precision
    if p0 == p0 and (p1 != p1 or p1 + 1e-9 < p0):
        return False
    return cand.f_beta() > inc.f_beta() + 1e-9


def improvement_is_material(base, cand):
    recall_up = (cand.recall - base.recall) * 100.0
    wrong_down = 0.0 if base.wrong == 0 else (base.wrong - cand.wrong) / base.wrong
    return (recall_up == recall_up and recall_up >= 2.0 - 1e-9) or wrong_down >= 0.20 - 1e-9


def may_install(base, cand):
    return swap_is_safe(base, cand) and improvement_is_material(base, cand)


def verdict(base, cand):
    return "PASS" if may_install(base, cand) else "FAIL"


def split_at(times, fit_fraction=FIT_FRACTION):
    n = len(times)
    if n == 0:
        return 0
    if n == 1:
        return 1
    target = max(1, min(n - 1, int(round(n * fit_fraction))))
    cut = target
    while cut < n and times[cut] == times[cut - 1]:
        cut += 1
    if cut < n:
        return cut
    back = target
    while back > 0 and times[back] == times[back - 1]:
        back -= 1
    return back if back > 0 else n


class Row:
    __slots__ = ("id", "t", "t_end", "overlap", "dur", "truth", "vec", "source_id", "session_id")


ROW_QUERY = """
  SELECT g.id, g.t_start_ns, COALESCE(g.overlap_frac, 0.0),
         (g.t_end_ns - g.t_start_ns), d.speaker_id, e.vector, e.embed_model_id,
         g.session_id, ss.source_id, g.t_end_ns
    FROM segments g
    JOIN discord_users d ON d.user_id = g.truth_user_id
    JOIN speakers s ON s.id = d.speaker_id
    JOIN sessions ss ON ss.id = g.session_id
    JOIN embeddings e ON e.id = (
         SELECT MAX(x.id) FROM embeddings x WHERE x.segment_id = g.id)
   WHERE g.deleted_at IS NULL
     AND g.truth_verdict = 'single'
     AND d.speaker_id IS NOT NULL
     AND s.merged_into IS NULL
     AND (g.t_end_ns - g.t_start_ns) >= ?
   ORDER BY g.t_start_ns ASC, g.id ASC
"""


def load_rows(c, model=MODEL):
    out = []
    for r in c.execute(ROW_QUERY, (int(MIN_DURATION_S * 1e9),)):
        assert r[6] == model, r[6]
        row = Row()
        row.id, row.t, row.overlap = r[0], r[1], float(r[2])
        row.dur = r[3] / 1e9
        row.truth = r[4]
        row.vec = blob_to_vec(r[5])
        row.session_id, row.source_id, row.t_end = r[7], r[8], r[9]
        out.append(row)
    return out


def load_bank(c, model=MODEL):
    q = """
      SELECT p.id, p.speaker_id, p.source_segment_id, p.vector, p.is_golden, p.created_at
        FROM speaker_prototypes p
        JOIN speakers s ON s.id = p.speaker_id
       WHERE s.merged_into IS NULL AND p.embed_model_id = ?
    """
    return [
        dict(id=r[0], speaker=r[1], src=r[2], vec=blob_to_vec(r[3]), golden=r[4], created=r[5])
        for r in c.execute(q, (model,))
    ]


def you_speaker_id(c):
    row = c.execute(
        "SELECT id FROM speakers WHERE auto_label='You' AND merged_into IS NULL ORDER BY id LIMIT 1"
    ).fetchone()
    return row[0] if row else None


def names(c):
    return {r[0]: r[1] for r in c.execute("SELECT id, display_name FROM speakers")}


class Bank:
    """Prototypes as one normalised matrix, plus each row's speaker and source."""

    def __init__(self, protos):
        self.protos = list(protos)
        if self.protos:
            m = np.stack([p["vec"] for p in self.protos])
            self.m = m / np.linalg.norm(m, axis=1, keepdims=True)
        else:
            self.m = np.zeros((0, 192))
        self.speakers = np.array([p["speaker"] for p in self.protos], dtype=np.int64)
        self.srcs = np.array(
            [p["src"] if p["src"] is not None else -1 for p in self.protos], dtype=np.int64
        )
        self.voices = sorted(set(self.speakers.tolist()))
        self._masks = {sp: (self.speakers == sp) for sp in self.voices}

    def cosines(self, vec):
        n = np.linalg.norm(vec)
        if n <= 0:
            return np.zeros(len(self.protos))
        return self.m @ (vec / n)

    def projected(self, fn):
        return Bank([dict(p, vec=fn(p["vec"])) for p in self.protos])


def rank(bank, vec, drop_segment=None, agg="max", topk=2, candidates=None):
    """identity::rank — [(speaker, score)] descending, ties to the lower id."""
    cos = bank.cosines(vec)
    keep = None
    if drop_segment is not None:
        keep = bank.srcs != drop_segment
    if candidates is not None:
        c = np.isin(bank.speakers, np.array(sorted(candidates), dtype=np.int64))
        keep = c if keep is None else (keep & c)
    out = []
    for sp in bank.voices:
        sel = bank._masks[sp] if keep is None else (bank._masks[sp] & keep)
        if not sel.any():
            continue
        s = cos[sel]
        if agg == "max":
            score = float(s.max())
        elif agg == "topk":
            k = min(topk, len(s))
            score = float(np.sort(s)[-k:].mean())
        elif agg == "mean":
            score = float(s.mean())
        else:
            raise ValueError(agg)
        out.append((sp, score))
    out.sort(key=lambda t: (-t[1], t[0]))
    return out


def decide(ranked, overlap, dur, thresholds, global_pair=(LABEL_THRESHOLD, 0.0)):
    """identity::decide_with, reduced to the label (None == declined)."""
    if overlap > MAX_OVERLAP or dur < GATE_MIN_DURATION_S:
        return None
    if not ranked:
        return None
    top_sp, top_score = ranked[0]
    t, m = thresholds.get(top_sp, global_pair)
    runner = ranked[1][1] if len(ranked) > 1 else -np.inf
    if top_score < t or (top_score - runner) < m:
        return None
    return top_sp


def judge(rows, bank, thresholds, you=None, agg="max", topk=2,
          candidates_for=None, project=None, scorer=None):
    s = Score()
    wrong = []
    for r in rows:
        if you is not None and r.truth == you:
            continue
        cands = None if candidates_for is None else candidates_for(r)
        if scorer is not None:
            ranked = scorer(r, candidates=cands)
        else:
            vec = r.vec if project is None else project(r.vec)
            ranked = rank(bank, vec, drop_segment=r.id, agg=agg, topk=topk, candidates=cands)
        label = decide(ranked, r.overlap, r.dur, thresholds)
        s.add(label, r.truth)
        if label is not None and label != r.truth:
            wrong.append((r.id, label, ranked[0][1], r.truth, r.dur))
    return s, wrong


def obs_for_fit(rows, bank, agg="max", topk=2, project=None, scorer=None, candidates_for=None):
    """calib::Obs for the fit split, under the global operating point."""
    out = []
    for r in rows:
        if r.overlap > MAX_OVERLAP or r.dur < GATE_MIN_DURATION_S:
            continue
        cands = None if candidates_for is None else candidates_for(r)
        if scorer is not None:
            ranked = scorer(r, candidates=cands)
        else:
            vec = r.vec if project is None else project(r.vec)
            ranked = rank(bank, vec, drop_segment=r.id, agg=agg, topk=topk, candidates=cands)
        if not ranked:
            continue
        runner = ranked[1][1] if len(ranked) > 1 else np.inf
        out.append(
            dict(top=ranked[0][0], score=ranked[0][1],
                 margin=ranked[0][1] - runner, truth=r.truth)
        )
    return out


def fit_thresholds(obs, min_rows=MIN_ROWS_PER_VOICE, bounds=THRESHOLD_BOUNDS,
                   global_pair=(LABEL_THRESHOLD, 0.0)):
    """calib::fit_thresholds -> {voice: (threshold, margin, n, f, f_global)}."""
    by = {}
    for o in obs:
        by.setdefault(o["top"], []).append(o)
    out = {}
    for voice in sorted(by):
        rws = by[voice]
        if len(rws) < min_rows:
            continue

        def score_at(t, m, rws=rws, voice=voice):
            s = Score()
            for o in rws:
                s.add(voice if (o["score"] >= t and o["margin"] >= m) else None, o["truth"])
            return s

        base = score_at(*global_pair)
        basef = base.f_beta()
        best = None
        t = bounds[0]
        while t <= bounds[1] + 1e-6:
            for m in MARGIN_GRID:
                sc = score_at(t, m)
                f = sc.f_beta()
                if best is None:
                    best = (f, sc.wrong, t, m)
                else:
                    bf, bw, bt, bm = best
                    if f > bf + 1e-12 or (
                        abs(f - bf) <= 1e-12
                        and (sc.wrong < bw or (sc.wrong == bw and (t > bt or (t == bt and m > bm))))
                    ):
                        best = (f, sc.wrong, t, m)
            t += 0.01
        f, wrong, t, m = best
        if f > basef + 1e-12 or (abs(f - basef) <= 1e-12 and wrong < base.wrong):
            out[voice] = (round(t * 1000) / 1000, round(m * 1000) / 1000, len(rws), f, basef)
    return out


def thresholds_from_fit(fit):
    return {v: (t, m) for v, (t, m, *_rest) in fit.items()}


def load_projection(c):
    """The projection the daemon would actually apply, or None."""
    row = c.execute(
        "SELECT embed_model_id, dim, matrix, n_rows, n_classes, shrinkage, power, centred, "
        "fitted_at_ns FROM identity_projection WHERE id = 1"
    ).fetchone()
    if not row:
        return None, None
    model, dim, blob = row[0], row[1], row[2]
    f = np.frombuffer(blob, dtype="<f4").astype(np.float64)
    mean, a = f[:dim], f[dim:].reshape(dim, dim)
    return (lambda v: a @ (v - mean)), dict(
        model=model, dim=dim, n_rows=row[3], n_classes=row[4],
        shrinkage=row[5], power=row[6], centred=row[7], fitted_at_ns=row[8],
    )
