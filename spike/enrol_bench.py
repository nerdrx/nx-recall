"""Is truth enrolment worth turning on? Held-out, on the shipping operating point.

The protocol is §32's, enforced by the same helpers in `recog_lib`: a
chronological 60/40 split of the truth rows, no row scored against a prototype
it produced, the user's own account excluded, and the scoring rule and
thresholds are the ones installed on the live box rather than the crate
defaults.

Arms: the bank as it is; the bank plus prototypes `truth::enrol_batch` would
have written from the FIT split under its own bar; the same at lower bars; the
eviction policy when a voice is at its cap; and the poisoning risk — one of the
two Discord links being wrong.

Read-only, against a COPY of the database. Never the live one.

    python3 spike/enrol_bench.py [path/to/copy.db]
"""

import sys
from collections import Counter
import numpy as np

sys.path.insert(0, __file__.rsplit("/", 1)[0])
import recog_lib as R  # noqa: E402

DB = "/tmp/nx-recall-workspace/nx-scratch/agents-2026-09-04/enrol/work.db"

# The machine as installed: config.toml, `settings`, `speakers`.
MAX_OVERLAP = 0.06
GLOBAL = (0.35, 0.0)
AGG, TOPK = "topk", 3
ENROL_SCORE = 0.55
ENROL_MARGIN = 0.06
ENROL_MAX_OVERLAP = 0.05
ENROL_MIN_DUR = 3.0
ENROL_MIN_COVERAGE = 0.95
CAP = 20

R.MAX_OVERLAP = MAX_OVERLAP  # recog_lib.decide reads the module global

ROW_QUERY = """
  SELECT g.id, g.t_start_ns, COALESCE(g.overlap_frac, 0.0),
         (g.t_end_ns - g.t_start_ns), d.speaker_id, e.vector, e.embed_model_id,
         COALESCE(g.truth_coverage, 0.0), g.truth_user_id
    FROM segments g
    JOIN discord_users d ON d.user_id = g.truth_user_id
    JOIN speakers s ON s.id = d.speaker_id
    JOIN embeddings e ON e.id = (
         SELECT MAX(x.id) FROM embeddings x WHERE x.segment_id = g.id)
   WHERE g.deleted_at IS NULL
     AND g.truth_verdict = 'single'
     AND d.speaker_id IS NOT NULL
     AND s.merged_into IS NULL
     AND (g.t_end_ns - g.t_start_ns) >= ?
   ORDER BY g.t_start_ns ASC, g.id ASC
"""


class Row:
    __slots__ = ("id", "t", "overlap", "dur", "truth", "vec", "coverage", "user")


def load_rows(c):
    out = []
    for r in c.execute(ROW_QUERY, (int(1.0 * 1e9),)):
        assert r[6] == R.MODEL, r[6]
        x = Row()
        x.id, x.t, x.overlap = r[0], r[1], float(r[2])
        x.dur = r[3] / 1e9
        x.truth, x.vec = r[4], R.blob_to_vec(r[5])
        x.coverage, x.user = float(r[7]), r[8]
        out.append(x)
    return out


def thresholds_installed(c):
    return {
        sid: (float(t), float(m))
        for sid, t, m in c.execute(
            "SELECT id, label_threshold, COALESCE(label_margin, 0.0) FROM speakers "
            "WHERE merged_into IS NULL AND label_threshold IS NOT NULL"
        )
    }


def bucket(d):
    return "<1.5" if d < 1.5 else ("1.5-3" if d < 3.0 else "3+")


def judge(rows, bank, thr, you, key=None):
    s, wrong = R.Score(), []
    buckets = {b: R.Score() for b in ("<1.5", "1.5-3", "3+")}
    for r in rows:
        truth = r.truth if key is None else key(r)
        if truth == you:
            continue
        ranked = R.rank(bank, r.vec, drop_segment=r.id, agg=AGG, topk=TOPK)
        label = R.decide(ranked, r.overlap, r.dur, thr, GLOBAL)
        s.add(label, truth)
        buckets[bucket(r.dur)].add(label, truth)
        if label is not None and label != truth:
            wrong.append((r.id, label, truth, round(ranked[0][1], 3), round(r.dur, 2)))
    return s, buckets, wrong


def pick_victim(vec, mine, how):
    """`identity::prototype_to_evict` (nearest), and the two alternatives."""
    cand = [p for p in mine if not p["golden"]]
    if not cand or how == "none":
        return None
    if how == "oldest":
        return min(cand, key=lambda p: ((p["created"] or 0), p["id"]))
    if how == "nearest":
        n = np.linalg.norm(vec)
        v = vec / n if n > 0 else vec
        return max(cand, key=lambda p: float(v @ (p["vec"] / np.linalg.norm(p["vec"]))))
    raise ValueError(how)


def enrol(protos, fit_rows, *, score_bar, margin_bar, require_match, cap, evict,
          truth_of=None, you=None, label_from_ladder=False,
          cover_min=ENROL_MIN_COVERAGE, truth_veto=False):
    """Replay `truth::enrol_batch` over the fit split, in time order.

    `label_from_ladder` is the freshness control: the same rows, the same
    quality gates, but the prototype is filed under whichever voice the ladder
    picked rather than the one Discord names. It isolates "the bank got newer"
    from "ground truth chose better", because the two are otherwise confounded
    on a corpus 28 hours long.
    """
    protos = [dict(p) for p in protos]
    added, evicted, seen = {}, 0, 0
    for r in sorted(fit_rows, key=lambda x: (x.t, x.id)):
        claim = r.truth if truth_of is None else truth_of(r)
        if you is not None and claim == you:
            continue
        if r.coverage < cover_min or r.dur < ENROL_MIN_DUR:
            continue
        if r.overlap > ENROL_MAX_OVERLAP:
            continue
        seen += 1
        bank = R.Bank(protos)
        ranked = R.rank(bank, r.vec, drop_segment=r.id, agg=AGG, topk=TOPK)
        if label_from_ladder:
            if not ranked:
                continue
            top_sp, top_score = ranked[0]
            runner = ranked[1][1] if len(ranked) > 1 else -np.inf
            if top_score < score_bar or (top_score - runner) < margin_bar:
                continue
            if truth_veto and top_sp != claim:
                continue  # Discord says this is somebody else: refuse the prototype
            claim = top_sp
        elif require_match:
            if not ranked:
                continue
            top_sp, top_score = ranked[0]
            runner = ranked[1][1] if len(ranked) > 1 else -np.inf
            if top_sp != claim or top_score < score_bar or (top_score - runner) < margin_bar:
                continue
        elif score_bar > 0.0:
            own = [s for sp, s in ranked if sp == claim]
            if not own or own[0] < score_bar:
                continue
        mine = [p for p in protos if p["speaker"] == claim]
        if cap and len(mine) >= cap:
            victim = pick_victim(r.vec, mine, evict)
            if victim is None:
                continue
            protos = [p for p in protos if p is not victim]
            evicted += 1
        protos.append(dict(id=-r.id, speaker=claim, src=r.id, vec=r.vec,
                           golden=0, created=r.t))
        added[claim] = added.get(claim, 0) + 1
    return protos, added, evicted, seen



SPECS = [
    ("(b)  bar 0.55 (the shipping bar)", 0.55, ENROL_MARGIN, True, "nearest", False),
    ("(c1) bar 0.45", 0.45, ENROL_MARGIN, True, "nearest", False),
    ("(c2) bar 0.35", 0.35, ENROL_MARGIN, True, "nearest", False),
    ("(c3) bar 0.45, no margin", 0.45, 0.0, True, "nearest", False),
    ("(c4) bar 0.35, no margin", 0.35, 0.0, True, "nearest", False),
    ("(c5) no ladder bar at all (truth alone)", 0.0, 0.0, False, "nearest", False),
    ("(d1) bar 0.55, evict oldest", 0.55, ENROL_MARGIN, True, "oldest", False),
    ("(d2) bar 0.55, cap kept (no eviction)", 0.55, ENROL_MARGIN, True, "none", False),
    ("(d3) bar 0.35, evict oldest", 0.35, ENROL_MARGIN, True, "oldest", False),
    ("(d4) bar 0.35, cap kept (no eviction)", 0.35, ENROL_MARGIN, True, "none", False),
    ("(g1) freshness control: ladder's own label", 0.55, ENROL_MARGIN, True, "nearest", True),
    ("(g2) freshness control, evict oldest", 0.55, ENROL_MARGIN, True, "oldest", True),
]


def suite(title, base, fit, held, thr, you, nm, swaps):
    """Every arm against one starting bank."""
    print(f"\n{'=' * 100}\n{title}\n{'=' * 100}")
    print(f"  starting bank: {len(base)} prototypes, "
          f"{ {nm.get(k, k): v for k, v in sorted(Counter(p['speaker'] for p in base).items())} }")
    arms = []

    def run(name, protos, added=None, evicted=0, key=None):
        s, buckets, wrong = judge(held, R.Bank(protos), thr, you, key=key)
        arms.append([name, s, buckets, added or {}, evicted, wrong])
        return s

    base_score = run("(a)  the bank as it is", base)
    for name, bar, margin, req, ev, ladder in SPECS:
        protos, added, evicted, _ = enrol(base, fit, score_bar=bar, margin_bar=margin,
                                          require_match=req, cap=CAP, evict=ev, you=you,
                                          label_from_ladder=ladder)
        run(name, protos, added, evicted)

    # ---- truth as a veto rather than an enroller ----------------------------
    # The live path enrols on `Matched{enroll:true}` whatever Discord says, and
    # coverage < 0.95 rows are enrolable by it and invisible to the truth pass.
    # These two arms are that path with and without a veto from ground truth.
    for tag, veto in (("(h1) live-path enrolment, no veto", False),
                      ("(h2) live-path enrolment, truth vetoes a disagreement", True)):
        protos, added, evicted, _ = enrol(base, fit, score_bar=0.55, margin_bar=ENROL_MARGIN,
                                          require_match=True, cap=CAP, evict="nearest",
                                          you=you, label_from_ladder=True, cover_min=0.0,
                                          truth_veto=veto)
        run(tag, protos, added, evicted)

    # ---- the poisoning risk -------------------------------------------------
    for tag, swap in swaps:
        def claimed(r, swap=swap):
            return swap.get(r.user, r.truth)

        for bar, req, ev, nm2 in ((0.55, True, "nearest", "bar 0.55"),
                                  (0.0, False, "nearest", "truth alone"),
                                  (0.0, False, "oldest", "truth alone, evict oldest")):
            protos, added, evicted, _ = enrol(base, fit, score_bar=bar, margin_bar=(
                ENROL_MARGIN if req else 0.0), require_match=req, cap=CAP,
                evict=ev, truth_of=claimed, you=you)
            real = {r.id: r.truth for r in fit}
            poison = sum(1 for p in protos
                         if p["id"] < 0 and real.get(p["src"], p["speaker"]) != p["speaker"])
            print(f"  {tag}, {nm2}: {len(protos)} prototypes, "
                  f"{poison} of them another person's voice")
            run(f"(e)  {tag}, {nm2}, real key", protos, added, evicted)
            run(f"(e') {tag}, {nm2}, the gate's own key", protos, key=claimed)
        run(f"(a') the bank as it is, {tag}, the gate's own key", base, key=claimed)

    print("\n" + R.HEADER)
    for name, s, _b, added, ev, _w in arms:
        extra = ""
        if added:
            extra = "   +" + ", ".join(f"{nm.get(k, k)}:{v}" for k, v in sorted(added.items()))
            extra += f", evicted {ev}" if ev else ", nothing evicted"
        elif name.startswith(("(b)", "(c", "(d", "(e)")):
            extra = "   + nothing"
        print(s.row(name) + extra)

    print("\n  gates against (a), on the real key:")
    for name, s, _b, _a, _e, _w in arms:
        if name.startswith(("(a)", "(a'")) or "gate's own key" in name:
            continue
        print(f"    {name:<44} {R.verdict(base_score, s)}  "
              f"safe={R.swap_is_safe(base_score, s)!s:<5} "
              f"material={R.improvement_is_material(base_score, s)!s:<5} "
              f"dF={s.f_beta() - base_score.f_beta():+.4f} "
              f"dP={(s.precision - base_score.precision) * 100:+.2f}pp "
              f"dR={(s.recall - base_score.recall) * 100:+.2f}pp")

    # Would the nightly gate notice a poisoned bank? It scores both banks
    # against the SAME wrong key, so this is the comparison it actually makes.
    print("\n  what the nightly gate would decide, judging on its own (wrong) key:")
    by_name = {a[0]: a[1] for a in arms}
    for name, s, _b, _a, _e, _w in arms:
        if "gate's own key" not in name or name.startswith("(a'"):
            continue
        tag = name.split(", ", 1)[0].replace("(e') ", "")
        ref = by_name.get(f"(a') the bank as it is, {tag}, the gate's own key")
        if ref is None:
            continue
        print(f"    {name:<58} {R.verdict(ref, s)}  "
              f"safe={R.swap_is_safe(ref, s)!s:<5} "
              f"material={R.improvement_is_material(ref, s)!s:<5} "
              f"(incumbent F {ref.f_beta():.3f} -> candidate {s.f_beta():.3f})")

    print("\n  per duration bucket:")
    for name, _s, buckets, _a, _e, _w in arms:
        if "gate's own key" in name:
            continue
        print(f"    {name}")
        for b in ("<1.5", "1.5-3", "3+"):
            print(buckets[b].row("    " + b))

    print("\n  wrong labels:")
    for name, _s, _b, _a, _e, wrong in arms:
        if "gate's own key" in name:
            continue
        print(f"    {name}: {len(wrong)}  " + ", ".join(
            f"{w[0]}:{nm.get(w[1], w[1])}/{nm.get(w[2], w[2])}@{w[3]}" for w in wrong[:14]))
    return arms


def main(db=DB):
    c = R.conn(db)
    rows = load_rows(c)
    you = R.you_speaker_id(c)
    installed = R.load_bank(c)
    thr = thresholds_installed(c)
    nm = R.names(c)
    cut = R.split_at([r.t for r in rows])
    fit, held = rows[:cut], rows[cut:]
    split_t = rows[cut].t
    asof = [p for p in installed if (p["created"] or 0) < split_t]
    print(f"rows {len(rows)}  fit {len(fit)}  held {len(held)}  you={you}")
    print(f"thresholds {thr}  aggregate top-{TOPK}  max_overlap {MAX_OVERLAP}  cap {CAP}")
    print(f"installed bank {len(installed)}; as of the split {len(asof)}")
    cands = [r for r in fit if r.coverage >= ENROL_MIN_COVERAGE and r.dur >= ENROL_MIN_DUR
             and r.overlap <= ENROL_MAX_OVERLAP and r.truth != you]
    print(f"fit-split candidates passing the quality gates: {len(cands)}  "
          + str({nm.get(k, k): v for k, v in Counter(r.truth for r in cands).items()}))
    heldc = [r for r in held if r.coverage >= ENROL_MIN_COVERAGE and r.dur >= ENROL_MIN_DUR
             and r.overlap <= ENROL_MAX_OVERLAP and r.truth != you]
    print(f"held-out rows that would themselves be candidates: {len(heldc)}")

    users = {}
    for r in rows:
        users.setdefault(r.user, [r.truth, 0])[1] += 1
    print("linked users: " + str({u: (nm.get(v[0]), v[1]) for u, v in users.items()}))
    two = sorted(users, key=lambda u: -users[u][1])[:2]
    both = {two[0]: users[two[1]][0], two[1]: users[two[0]][0]}
    one = {two[0]: users[two[1]][0]}
    swaps = [("one link wrong", one), ("both links wrong", both)]
    for tag, sw in swaps:
        print(f"  {tag}: " + str({u: nm.get(v) for u, v in sw.items()}))

    suite("A. against the bank as of the split point (the honest growth test)",
          asof, fit, held, thr, you, nm, swaps)
    suite("B. against the bank as installed today (31 prototypes are from the "
          "held-out period)", installed, fit, held, thr, you, nm, swaps)


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else DB)
