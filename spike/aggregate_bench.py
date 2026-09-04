"""Is top-3 still the right aggregate on today's corpus and today's bank?

§32 shipped `Aggregate::TopK(3)` on 460 held-out rows. §39's aside found it
losing to max on the archive-doubled corpus, and the nightly pass itself took
it back on 2026-09-04 18:14 (`operations` 2559): the box runs `max` today.
This round re-asks the question at the operating point the box actually has.

Four arms — max, top-2, top-3, top-5 — each measured twice:

  (a) under the **global** threshold, every voice on the same bar;
  (b) with per-voice thresholds **refit for that arm** on the fit split.

(b) is the half §36's scale error makes mandatory: a bar fitted under max is
not the same operating point under a top-3 mean, so comparing a top-3 arm
against max-fitted thresholds measures the scale and not the rule.

And each of those on two banks:

  (i)  the bank as it is (259 prototypes, Rowan 47 against a cap of 20);
  (ii) the bank with Rowan's merged-in, never-matching prototypes excluded
       (§44.3: minted `Speaker_NN` voices named into Rowan by hand, whose
       vectors match nobody — the cap is not a cap after a merge).

Protocol, identical to §32/§44 and enforced by the same helpers: chronological
60/40 split of the truth rows, no row scored against a prototype its own audio
produced, the user's own Discord account excluded, the operating point read off
the live box rather than the crate defaults.

    python3 spike/aggregate_bench.py [path/to/copy.db]

Read-only, against a COPY of the database. Never the live one.
"""

import sys
from collections import defaultdict

import numpy as np

sys.path.insert(0, __file__.rsplit("/", 1)[0])
import recog_lib as R  # noqa: E402

DB = "/tmp/nx-recall-workspace/nx-scratch/agents-2026-09-04/aggregate/work.db"

MAX_OVERLAP = 0.06  # config.toml, this install
GLOBAL = (0.35, 0.0)

R.MAX_OVERLAP = MAX_OVERLAP
R.LABEL_THRESHOLD = GLOBAL[0]

ARMS = [("max", 1), ("topk", 2), ("topk", 3), ("topk", 5)]
DURATION_BUCKETS = [("< 1.5 s", 0.0, 1.5), ("1.5-3 s", 1.5, 3.0), ("3 s +", 3.0, float("inf"))]


def arm_name(agg, k):
    return "max" if agg == "max" else f"top-{k}"


def bucket_of(dur):
    for name, lo, hi in DURATION_BUCKETS:
        if lo <= dur < hi:
            return name
    return DURATION_BUCKETS[-1][0]


# ---- the bank, and the merged-in prototypes -------------------------------


def source_verdicts(c):
    return {
        r[0]: (r[1], r[2])
        for r in c.execute(
            "SELECT g.id, g.truth_verdict, d.speaker_id FROM segments g "
            "LEFT JOIN discord_users d ON d.user_id = g.truth_user_id"
        )
    }


def inherited(protos, verdicts, voice):
    """One voice's prototypes that came in through a merge, not through a match.

    §44.3's rule, re-derived rather than copied: a voice is minted precisely
    *because* its turn did not match anything in the bank, so merging one into
    an existing voice files "the turn that failed to match" under that voice,
    permanently — and `merge_speakers` re-points prototypes with a bare UPDATE
    that no cap re-applies. Operationally: the prototype's own source turn
    carries no verdict naming its owner.
    """
    out = []
    for p in protos:
        if p["speaker"] != voice:
            continue
        v, sp = verdicts.get(p["src"], (None, None)) if p["src"] is not None else (None, None)
        if v in ("single", "partial") and sp == voice:
            continue
        out.append(p)
    return out


def condemned(c, model=R.MODEL, you=None, with_source=False):
    """`store::condemned_prototypes` — a prototype whose own turn Discord says
    was somebody else. What `recalld identity repair --prototypes` removes."""
    q = """
      SELECT p.id, p.speaker_id, d.speaker_id, g.id
        FROM speaker_prototypes p
        JOIN speakers own ON own.id = p.speaker_id
        JOIN segments g ON g.id = p.source_segment_id
        JOIN discord_users d ON d.user_id = g.truth_user_id
        JOIN speakers said ON said.id = d.speaker_id
       WHERE p.embed_model_id = ? AND p.is_golden = 0
         AND own.merged_into IS NULL AND said.merged_into IS NULL
         AND g.deleted_at IS NULL AND g.truth_verdict = 'single'
         AND COALESCE(g.truth_coverage, 0.0) >= 0.8
         AND d.speaker_id <> p.speaker_id
       ORDER BY p.id
    """
    rows = [r for r in c.execute(q, (model,)) if you is None or r[2] != you]
    return rows if with_source else [(r[0], r[1], r[2]) for r in rows]


def own_voice_cosines(protos, rows, voice):
    """Mean cosine of each of a voice's prototypes to that voice's own turns,
    and how often it is the best prototype in the whole bank. §44.3's table."""
    m = np.stack([p["vec"] / np.linalg.norm(p["vec"]) for p in protos])
    ids = [p["id"] for p in protos]
    srcs = [p["src"] for p in protos]
    mine = [i for i, p in enumerate(protos) if p["speaker"] == voice]
    tot = defaultdict(list)
    wins = defaultdict(int)
    for r in rows:
        v = r.vec / np.linalg.norm(r.vec)
        cos = m @ v
        for i, s in enumerate(srcs):
            if s is not None and s == r.id:
                cos[i] = -np.inf
        wins[ids[int(np.argmax(cos))]] += 1
        if r.truth == voice:
            for i in mine:
                if cos[i] > -np.inf:
                    tot[ids[i]].append(float(cos[i]))
    return (
        {k: float(np.mean(v)) for k, v in tot.items()},
        wins,
    )


# ---- the arms ---------------------------------------------------------------


def judge(rows, bank, thresholds, you, agg, topk):
    """`identity_learn::judge`, plus the per-row detail the buckets need."""
    s, per_bucket, wrong = R.Score(), defaultdict(R.Score), []
    for r in rows:
        if you is not None and r.truth == you:
            continue
        ranked = R.rank(bank, r.vec, drop_segment=r.id, agg=agg, topk=topk)
        label = R.decide(ranked, r.overlap, r.dur, thresholds, GLOBAL)
        s.add(label, r.truth)
        per_bucket[bucket_of(r.dur)].add(label, r.truth)
        if label is not None and label != r.truth:
            wrong.append((r.id, label, r.truth, round(r.dur, 2)))
    return s, per_bucket, wrong


def refit(fit_rows, bank, agg, topk):
    """Per-voice thresholds fitted on the fit split **under this arm's rule**."""
    obs = R.obs_for_fit(fit_rows, bank, agg=agg, topk=topk)
    return R.fit_thresholds(obs, global_pair=GLOBAL)


def run(label, protos, fit, held, you, nm, incumbent=None):
    bank = R.Bank(protos)
    print(f"\n{'=' * 108}\n{label}   ({len(protos)} prototypes)\n{'=' * 108}")
    print(R.HEADER)
    results = {}
    for agg, k in ARMS:
        name = arm_name(agg, k)
        s_g, b_g, w_g = judge(held, bank, {}, you, agg, k)
        fitted = refit(fit, bank, agg, k)
        thr = R.thresholds_from_fit(fitted)
        s_t, b_t, w_t = judge(held, bank, thr, you, agg, k)
        results[name] = dict(
            globals=(s_g, b_g, w_g), fitted=(s_t, b_t, w_t), thresholds=fitted
        )
        print(s_g.row(f"{name}, global {GLOBAL[0]:.2f}"))
        shown = ", ".join(
            f"{nm.get(v, v)} {t:.2f}/{m:.2f}" for v, (t, m, *_r) in sorted(fitted.items())
        )
        print(s_t.row(f"{name}, thresholds refit for {name}") + f"   {shown or 'none fitted'}")

    print("\n  by duration, thresholds refit per arm")
    print(f"  {'bucket':<12}" + "".join(f"{arm_name(*a):>28}" for a in ARMS))
    for bname, _lo, _hi in DURATION_BUCKETS:
        cells = []
        for agg, k in ARMS:
            b = results[arm_name(agg, k)]["fitted"][1][bname]
            cells.append(f"{b.correct}/{b.wrong}/{b.declined} F{b.f_beta():.3f}")
        print(f"  {bname:<12}" + "".join(f"{c:>28}" for c in cells))

    print("\n  by duration, the global threshold")
    print(f"  {'bucket':<12}" + "".join(f"{arm_name(*a):>28}" for a in ARMS))
    for bname, _lo, _hi in DURATION_BUCKETS:
        cells = []
        for agg, k in ARMS:
            b = results[arm_name(agg, k)]["globals"][1][bname]
            cells.append(f"{b.correct}/{b.wrong}/{b.declined} F{b.f_beta():.3f}")
        print(f"  {bname:<12}" + "".join(f"{c:>28}" for c in cells))

    inc = incumbent or results["max"]["fitted"][0]
    print("\n  against the incumbent (max + thresholds refit for max) — "
          "precision must not drop, F-0.5 must rise, and the gain must be material")
    for agg, k in ARMS:
        name = arm_name(agg, k)
        for how in ("globals", "fitted"):
            s = results[name][how][0]
            print(
                f"    {name + ', ' + how:<28} {R.verdict(inc, s)}  "
                f"safe={R.swap_is_safe(inc, s)!s:<5} "
                f"material={R.improvement_is_material(inc, s)!s:<5} "
                f"dF={s.f_beta() - inc.f_beta():+.4f} "
                f"dP={(s.precision - inc.precision) * 100:+.2f}pp "
                f"dR={(s.recall - inc.recall) * 100:+.2f}pp"
            )
    return results


def main(db=DB):
    c = R.conn(db)
    you = R.you_speaker_id(c)
    nm = R.names(c)
    protos = R.load_bank(c)
    rows = R.load_rows(c)
    settings = dict(c.execute("SELECT key, value FROM settings"))
    installed_thr = {
        sid: (float(t), float(m))
        for sid, t, m in c.execute(
            "SELECT id, label_threshold, COALESCE(label_margin, 0.0) FROM speakers "
            "WHERE merged_into IS NULL AND label_threshold IS NOT NULL"
        )
    }
    cut = R.split_at([r.t for r in rows])
    fit, held = rows[:cut], rows[cut:]

    print(f"rows {len(rows)}  fit {len(fit)}  held {len(held)}  you={you}")
    print(f"installed aggregate {settings.get('identity_aggregate', 'max')!r}; "
          f"installed thresholds "
          + (", ".join(f"{nm.get(k, k)} {t:.2f}/{m:.2f}" for k, (t, m) in sorted(installed_thr.items()))
             or "none")
          + f"; global {GLOBAL[0]:.2f}, max_overlap {MAX_OVERLAP}")
    counts = defaultdict(int)
    for p in protos:
        counts[p["speaker"]] += 1
    over = {nm.get(k, k): v for k, v in sorted(counts.items(), key=lambda kv: -kv[1]) if v > 20}
    print(f"bank {len(protos)} prototypes over {len(counts)} voices; over the cap of 20: {over}")

    verdicts = source_verdicts(c)
    rowan = next((k for k, v in nm.items() if v == "Rowan"), None)
    merged_in = inherited(protos, verdicts, rowan)
    means, wins = own_voice_cosines(protos, rows, rowan)
    grown = [p for p in protos if p["speaker"] == rowan and p not in merged_in]
    fmt = lambda ps: (  # noqa: E731
        f"n={len(ps)}  mean cosine to Rowan's turns "
        f"{np.mean([means.get(p['id'], float('nan')) for p in ps]):.3f}  "
        f"best-prototype wins {sum(wins.get(p['id'], 0) for p in ps)}  "
        f"never any row's best {sum(1 for p in ps if wins.get(p['id'], 0) == 0)}"
    )
    print(f"\nRowan's bank, split the way §44.3 split it")
    print(f"  merged in (no verdict naming Rowan): {fmt(merged_in)}")
    print(f"  enrolled on a match:                 {fmt(grown)}")

    doomed = condemned(c, you=you, with_source=True)
    by_owner = defaultdict(int)
    for _pid, owner, _truth, _seg in doomed:
        by_owner[nm.get(owner, owner)] += 1
    print(f"\nprototypes whose own turn Discord says was somebody else "
          f"(`identity repair --prototypes`): {len(doomed)}  {dict(by_owner)}")

    # A held-out row whose own verdict is what condemns a prototype must not be
    # scored, or the repaired bank is being graded on the answer it was handed
    # (`identity_learn::repair_prototypes`'s one honesty rule). The same rows are
    # dropped from every bank, so the three columns are comparable.
    consumed = {seg for _p, _o, _t, seg in doomed}
    before = len(held)
    held = [r for r in held if r.id not in consumed]
    print(f"held-out rows {before}; {before - len(held)} dropped because their own verdict "
          f"is the evidence that condemns a prototype -> {len(held)} scored")

    # What the box is actually running right now, as the reference row.
    live, live_b, _w = judge(held, R.Bank(protos), installed_thr, you, "max", 1)
    print("\nthe live box, as installed")
    print(R.HEADER)
    print(live.row("max + the installed thresholds"))

    banks = [
        ("(i) the bank as it is", protos),
        (
            f"(ii) minus Rowan's {len(merged_in)} merged-in prototypes",
            [p for p in protos if p["id"] not in {q["id"] for q in merged_in}],
        ),
        (
            f"(iii) minus the {len(doomed)} prototypes `repair --prototypes` condemns",
            [p for p in protos if p["id"] not in {d[0] for d in doomed}],
        ),
    ]
    out = [(label, run(label, ps, fit, held, you, nm)) for label, ps in banks]

    print(f"\n{'=' * 108}\ndoes the answer depend on the bank?\n{'=' * 108}")
    print(f"  {'arm':<30}" + "".join(f"{lab.split(')')[0] + ')':>26}" for lab, _r in out))
    for agg, k in ARMS:
        name = arm_name(agg, k)
        for how in ("globals", "fitted"):
            cells = []
            for _lab, res in out:
                s = res[name][how][0]
                cells.append(f"{s.correct}/{s.wrong}/{s.declined} F{s.f_beta():.3f}")
            print(f"  {name + ', ' + how:<30}" + "".join(f"{c:>26}" for c in cells))


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else DB)
