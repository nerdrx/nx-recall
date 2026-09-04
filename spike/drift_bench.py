"""Do voices drift? Match score against a prototype, by the prototype's age.

Question 1 of the drift round. For every turn with ground truth — Rowan and
Aspen from Discord, the user from the microphone — score it against each
prototype of its OWN voice and ask two things:

  * does the cosine fall as the prototype gets older, and
  * does it jump when the prototype's audio came from a different source
    (a different Discord client, or the mic) than the turn's?

The confound that kills the naive version: a turn that is easy matches *every*
prototype well, and easy turns are not spread evenly over time. So every table
is reported twice — raw, and **within-turn**, where each cosine has its own
turn's mean subtracted. The within-turn number is the one that answers the
question, because it compares old and new prototypes on the same audio.

    python3 spike/drift_bench.py [path/to/copy.db]

Read-only, against a COPY of the database. Never the live one.
"""

import sys
from collections import defaultdict

import numpy as np

sys.path.insert(0, __file__.rsplit("/", 1)[0])
import drift_lib as D  # noqa: E402
import recog_lib as R  # noqa: E402


def summarise(items, key, title, value="cos"):
    groups = defaultdict(list)
    for it in items:
        groups[key(it)].append(it[value])
    print(f"\n  {title}")
    print(f"    {'bucket':<16}{'n':>8}{'mean':>10}{'sd':>9}{'p10':>9}{'median':>9}{'p90':>9}")
    for k in sorted(groups, key=lambda k: _order(k)):
        a = np.array(groups[k])
        print(
            f"    {str(k):<16}{len(a):>8}{a.mean():>10.4f}{a.std():>9.4f}"
            f"{np.percentile(a, 10):>9.4f}{np.median(a):>9.4f}{np.percentile(a, 90):>9.4f}"
        )
    return groups


def _order(k):
    names = [b[0] for b in D.AGE_BUCKETS]
    return (names.index(k), "") if k in names else (len(names), str(k))


def within_turn(items, value="cos"):
    """Subtract each turn's own mean, so old and new are compared on one turn."""
    by_row = defaultdict(list)
    for it in items:
        by_row[it["row"]].append(it)
    out = []
    for group in by_row.values():
        if len(group) < 2:
            continue  # a turn with one eligible prototype says nothing about age
        mu = float(np.mean([g[value] for g in group]))
        for g in group:
            out.append(dict(g, dev=g[value] - mu))
    return out


def slope(items, value="cos", x="age_h"):
    """OLS of the demeaned cosine on age, in cosine points per day."""
    if len(items) < 3:
        return float("nan"), float("nan"), 0
    xs = np.array([it[x] for it in items])
    ys = np.array([it[value] for it in items])
    if xs.std() == 0:
        return float("nan"), float("nan"), len(items)
    b, _a = np.polyfit(xs, ys, 1)
    r = float(np.corrcoef(xs, ys)[0, 1])
    return b * 24.0, r, len(items)


def report_voice(name, items):
    print(f"\n{'=' * 96}\n{name}   ({len(items)} (turn, prototype) pairs)\n{'=' * 96}")
    if not items:
        print("  nothing to measure")
        return
    summarise(items, lambda i: D.age_bucket(i["age_h"]), "raw cosine by prototype age")
    dev = within_turn(items)
    summarise(
        dev,
        lambda i: D.age_bucket(i["age_h"]),
        f"WITHIN-TURN deviation by prototype age  ({len(dev)} pairs on multi-prototype turns)",
        value="dev",
    )
    b, r, n = slope(dev, value="dev")
    print(f"\n    within-turn slope: {b:+.4f} cosine/day   r = {r:+.3f}   n = {n}")
    b2, r2, n2 = slope(items)
    print(f"    raw slope:         {b2:+.4f} cosine/day   r = {r2:+.3f}   n = {n2}")

    # Source: which client/microphone the prototype's audio came from, against
    # which the turn came from. On this install the interesting pair is the two
    # Discord clients (§37) — the official one and vesktop.
    cross = defaultdict(list)
    for it in items:
        cross[(it["row_app"], it["proto_app"])].append(it["cos"])
    if len(cross) > 1:
        print("\n    turn source x prototype source (raw cosine)")
        print(f"    {'turn':<12}{'prototype':<12}{'n':>8}{'mean':>10}{'sd':>9}")
        for k in sorted(cross):
            a = np.array(cross[k])
            print(f"    {k[0]:<12}{k[1]:<12}{len(a):>8}{a.mean():>10.4f}{a.std():>9.4f}")

    devc = defaultdict(list)
    for it in dev:
        devc[it["proto_app"]].append(it["dev"])
    if len(devc) > 1:
        print("\n    WITHIN-TURN deviation by prototype source")
        for k in sorted(devc):
            a = np.array(devc[k])
            print(f"    {k:<12}{len(a):>8}{a.mean():>+10.4f}")


def evening_series(rows, protos, thresholds, you, agg, topk, names):
    """Question 3's raw material: each voice's daily own-bank score.

    Per calendar day, the median top-3-mean score of that voice's turns against
    its own bank (causally: prototypes that existed at the time), and the share
    of turns that fall under the operating threshold. A drift alarm can only be
    built on this if the series moves.
    """
    print(f"\n{'=' * 96}\nDaily own-bank score, per voice\n{'=' * 96}")
    print(f"  {'voice':<10}{'day':<12}{'turns':>7}{'median':>9}{'p25':>8}{'thr':>7}{'below':>8}")
    import datetime

    by = defaultdict(lambda: defaultdict(list))
    prot_sorted = sorted(protos, key=lambda p: (p["created"] or 0, p["id"]))
    for r in rows:
        if r.truth == you and r.kind != "mic":
            continue
        mine = [
            p
            for p in prot_sorted
            if p["speaker"] == r.truth
            and (p["created"] or 0) <= r.t
            and not (p["src"] is not None and p["src"] == r.id)
        ]
        if not mine:
            continue
        v = D.normed(r.vec)
        s = np.array([float(v @ D.normed(p["vec"])) for p in mine])
        k = min(topk, len(s))
        score = float(s.max()) if agg == "max" else float(np.sort(s)[-k:].mean())
        day = datetime.datetime.fromtimestamp(r.t / 1e9, datetime.UTC).strftime("%Y-%m-%d")
        by[r.truth][day].append(score)
    for voice in sorted(by, key=lambda v: -sum(len(x) for x in by[v].values())):
        thr = thresholds.get(voice, (R.LABEL_THRESHOLD, 0.0))[0]
        for day in sorted(by[voice]):
            a = np.array(by[voice][day])
            if len(a) < 5:
                continue
            print(
                f"  {names.get(voice, voice):<10}{day:<12}{len(a):>7}{np.median(a):>9.3f}"
                f"{np.percentile(a, 25):>8.3f}{thr:>7.2f}{(a < thr).mean() * 100:>7.1f}%"
            )


def turn_pairs(rows, name, cap=4000, seed=0):
    """The bank-free drift test: two turns of the same voice, how far apart?

    Every prototype-based table above is confounded by *which* prototypes are
    old — on this install the old ones arrived by merge, so "age" and "vector
    the daemon would not enrol today" are the same column. Turn-to-turn cosine
    has no such confound: both sides are ground-truth audio of the same person,
    and the only variable is the gap between them. If a voice drifts, this
    falls with the gap. If it does not, the prototype table was measuring bank
    hygiene and calling it drift.
    """
    rng = np.random.default_rng(seed)
    if len(rows) > cap:
        idx = rng.choice(len(rows), cap, replace=False)
        rows = [rows[i] for i in sorted(idx)]
    v = np.stack([D.normed(r.vec) for r in rows])
    t = np.array([r.t for r in rows], dtype=np.float64)
    cos = v @ v.T
    gap = np.abs(t[:, None] - t[None, :]) / D.HOUR_NS
    iu = np.triu_indices(len(rows), k=1)
    cos, gap = cos[iu], gap[iu]
    print(f"\n{'=' * 96}\n{name}: turn-to-turn cosine by the gap between the two turns"
          f"   ({len(cos)} pairs from {len(rows)} turns)\n{'=' * 96}")
    print(f"    {'gap':<16}{'n':>9}{'mean':>10}{'sd':>9}{'median':>9}")
    for label, lo, hi in D.AGE_BUCKETS:
        sel = (gap >= lo) & (gap < hi)
        if sel.sum() == 0:
            continue
        a = cos[sel]
        print(f"    {label:<16}{len(a):>9}{a.mean():>10.4f}{a.std():>9.4f}{np.median(a):>9.4f}")
    if gap.std() > 0:
        b = np.polyfit(gap, cos, 1)[0] * 24.0
        r = float(np.corrcoef(gap, cos)[0, 1])
        print(f"\n    slope {b:+.4f} cosine/day   r = {r:+.3f}")


def main(db=D.DB):
    c = R.conn(db)
    you = R.you_speaker_id(c)
    nm = R.names(c)
    protos = D.load_bank(c)
    truth = D.load_truth_rows(c)
    mic = D.load_mic_rows(c, you)
    agg_setting = dict(c.execute("SELECT key, value FROM settings")).get(
        "identity_aggregate", "max"
    )
    thr = {
        sid: (float(t), float(m))
        for sid, t, m in c.execute(
            "SELECT id, label_threshold, COALESCE(label_margin, 0.0) FROM speakers "
            "WHERE merged_into IS NULL AND label_threshold IS NOT NULL"
        )
    }
    print(f"database {db}")
    print(f"prototypes {len(protos)}  truth rows {len(truth)}  mic rows {len(mic)}  you={you}")
    print(f"installed aggregate: {agg_setting}   per-voice thresholds: {thr or 'none'}")
    span = (max(r.t for r in truth) - min(r.t for r in truth)) / D.HOUR_NS
    print(f"truth corpus spans {span:.1f} h ({span / 24:.2f} days)")

    counts = defaultdict(int)
    for r in truth:
        counts[r.truth] += 1
    print("truth rows per voice: " + str({nm.get(k): v for k, v in sorted(counts.items())}))

    all_pairs = D.pairs(truth, protos) + D.pairs(mic, protos)
    by_voice = defaultdict(list)
    for p in all_pairs:
        by_voice[p["voice"]].append(p)
    for voice in sorted(by_voice, key=lambda v: -len(by_voice[v])):
        if len(by_voice[voice]) < 200:
            continue
        report_voice(f"{nm.get(voice, voice)} (speaker {voice})", by_voice[voice])

    for voice in sorted(by_voice, key=lambda v: -len(by_voice[v])):
        rs = [r for r in truth + mic if r.truth == voice]
        if len(rs) >= 100:
            turn_pairs(rs, nm.get(voice, voice))

    agg = "topk" if agg_setting.startswith("top") else "max"
    k = int(agg_setting.split("-")[-1]) if agg_setting.startswith("top") else 3
    evening_series(truth + mic, protos, thr, you, agg, k, nm)


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else D.DB)
