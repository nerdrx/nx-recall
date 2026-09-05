"""The 2026-09-04 mint burst, reconstructed, and the five rules that could stop it.

Part 1 reconstructs the evening: the bank as of the nightly `identity calibrate`
that raised Rowan's bar to 0.41, then every turn the daemon analysed after it,
in order, through the whole ladder — including the mint path §32/§36/§44 all
replayed without.

Part 2 runs the same evening under each proposed brake and reports what it
costs: phantoms minted, correct labels lost, and held-out precision/recall.

    python3 spike/mint_bench.py [path/to/copy.db]

Read-only, against a COPY of the database. Never the live one.
"""

import sys
from collections import Counter

sys.path.insert(0, __file__.rsplit("/", 1)[0])
import mint_lib as M  # noqa: E402
import recog_lib as R  # noqa: E402

R.MAX_OVERLAP = M.MAX_OVERLAP


def calibrate_instant(c):
    row = c.execute(
        "SELECT at_utc_ns FROM operations WHERE op='identity.calibrate' "
        "ORDER BY at_utc_ns DESC LIMIT 1"
    ).fetchone()
    return row[0]


def score_online(sim, segs, you):
    """Score the labels the simulation actually produced, as it produced them.

    The only honest scoring for a cascade: a phantom label is *wrong*, not a
    decline, and the bank the next turn is scored against is the one the last
    turn left behind.
    """
    s = R.Score()
    for seg in segs:
        if seg.truth is None or seg.verdict != "single" or seg.kind == "mic":
            continue
        if seg.truth == you or seg.dur < M.MIN_DURATION_S:
            continue
        s.add(sim.labels.get(seg.id), seg.truth)
    return s


def phantom_rows(sim, segs):
    """Turns handed to a voice this evening invented."""
    return sum(
        1 for seg in segs
        if sim.labels.get(seg.id) in sim.minted and seg.truth is not None
    )


def hhmm(ns):
    import datetime
    return datetime.datetime.utcfromtimestamp(ns / 1e9).strftime("%H:%M:%S")


def main(db=M.DB):
    c = R.conn(db)
    you = R.you_speaker_id(c)
    names = R.names(c)
    t0 = calibrate_instant(c)
    base = M.bank_at(c, t0)
    segs = M.load_segments(c, t0)
    thr = M.installed_thresholds(c)

    print(f"the nightly pass ran at {hhmm(t0)} UTC; the bank it left behind has "
          f"{len(base)} prototypes over {len({p['speaker'] for p in base})} voices")
    print(f"thresholds it installed: "
          + ", ".join(f"{names.get(k, k)} {v[0]:.2f}/{v[1]:.2f}" for k, v in sorted(thr.items())))
    print(f"turns analysed after it: {len(segs)} "
          f"({sum(1 for s in segs if s.kind == 'mic')} mic, "
          f"{sum(1 for s in segs if s.verdict == 'single' and s.truth is not None)} "
          f"with a Discord verdict)\n")

    # ---- part 1: what happened ------------------------------------------
    ship = M.Sim(base, thr, names, you, M.Rules()).run(segs)
    print(f"{'=' * 110}\n1. The evening, replayed as it ran\n{'=' * 110}")
    print(f"  {'time':>9}{'seg':>7}{'dur':>6}{'wd':>4}  {'best voice':<14}"
          f"{'score':>7}{'its bar':>9}{'margin':>8}  {'truth':<8}{'cov':>6}")
    for e in ship.mint_events:
        print(f"  {hhmm(e['t']):>9}{e['segment']:>7}{e['dur']:>6.1f}{e['words']:>4}  "
              f"{e['best_name'][:13]:<14}"
              f"{(f'{e['score']:.3f}' if e['score'] is not None else '-'):>7}"
              f"{(f'{e['bar']:.2f}' if e['bar'] is not None else '-'):>9}"
              f"{(f'{e['margin']:.3f}' if e['margin'] is not None else '-'):>8}  "
              f"{(names.get(e['truth'], '-') if e['truth'] else '-'):<8}"
              f"{e['coverage'] * 100:>5.0f}%")
    print(f"\n  {len(ship.mint_events)} mints. by the voice the ladder had on top: "
          + str(Counter(e['best_name'] for e in ship.mint_events).most_common()))
    print("  by what Discord says the turn was: "
          + str(Counter(names.get(e['truth'], 'no verdict') for e in ship.mint_events)
                .most_common()))
    if ship.mint_events:
        span = (ship.mint_events[-1]["t"] - ship.mint_events[0]["t"]) / 60e9
        print(f"  span {span:.0f} min; phantom voices {len(ship.minted)}, "
              f"turns they took {phantom_rows(ship, segs)}")

    # what really happened, from the archive, as the check on the replay
    real = [
        (r[0], r[1], r[2]) for r in c.execute(
            "SELECT id, display_name, created_at FROM speakers WHERE created_at >= ? "
            "ORDER BY created_at", (t0,))
    ]
    print(f"\n  the archive's own count over the same window: {len(real)} voices minted, "
          f"{', '.join(n for _i, n, _t in real)}")

    # ---- part 2: the rules ----------------------------------------------
    print(f"\n{'=' * 110}\n2. The brakes\n{'=' * 110}")
    print(f"  {'rule':<52}{'mints':>7}{'phantom rows':>14}{'correct':>9}"
          f"{'wrong':>7}{'declined':>10}{'prec':>8}{'recall':>8}{'F-0.5':>8}")

    arms = [("(shipping) the evening as it ran", M.Rules())]
    for x in (0.05, 0.10, 0.15, 0.20):
        arms.append((f"(a) near miss within {x:.2f} declines, never mints", M.Rules(near_miss=x)))
    arms.append(("(a') near miss within 0.10, but it LABELS",
                 M.Rules(near_miss=0.10, near_miss_labels=True)))
    arms.append(("(b) a fitted bar declines, it never mints",
                 M.Rules(fitted_never_mints=True)))
    arms.append(("(b*) no per-voice bar above the global 0.35 (at calibrate)",
                 M.Rules(bar_ceiling=M.LABEL_THRESHOLD)))
    for n in (2, 3, 5):
        arms.append((f"(c) an unnamed voice under {n} prototypes cannot win",
                     M.Rules(min_prototypes=n)))
    arms.append(("(b)+(c2)", M.Rules(fitted_never_mints=True, min_prototypes=2)))
    arms.append(("(b)+(c3)", M.Rules(fitted_never_mints=True, min_prototypes=3)))
    arms.append(("(b*)+(c3)", M.Rules(bar_ceiling=M.LABEL_THRESHOLD, min_prototypes=3)))

    results = []
    for name, rules in arms:
        sim = M.Sim(base, thr, names, you, rules).run(segs)
        s = score_online(sim, segs, you)
        results.append((name, sim, s))
        print(f"  {name:<52}{len(sim.minted):>7}{phantom_rows(sim, segs):>14}"
              f"{s.correct:>9}{s.wrong:>7}{s.declined:>10}"
              f"{s.precision * 100:>7.1f}%{s.recall * 100:>7.1f}%{s.f_beta():>8.3f}")

    base_s = results[0][2]
    print("\n  against the evening as it ran: correct labels lost, phantoms prevented")
    print(f"  {'rule':<52}{'phantoms prevented':>20}{'correct lost':>14}{'dF-0.5':>9}")
    for name, sim, s in results[1:]:
        lost = max(0, base_s.correct - s.correct)
        print(f"  {name:<52}{len(results[0][1].minted) - len(sim.minted):>20}"
              f"{lost:>14}{s.f_beta() - base_s.f_beta():>+9.3f}")

    # ---- (d) a ceiling tied to the voice's own score distribution ---------
    print(f"\n{'=' * 110}\n2c. (d) A fitted bar against the voice's own fit-split scores"
          f"\n{'=' * 110}")
    import numpy as np
    import drift_lib as D
    trows = D.load_truth_rows(c)
    cut = R.split_at([r.t for r in trows])
    fit_rows, split_t = trows[:cut], trows[cut].t
    fit_bank = R.Bank([p for p in M.bank_at(c, split_t)])
    own = {}
    for r in fit_rows:
        ranked = R.rank(fit_bank, r.vec, drop_segment=r.id)
        s = [sc for sp, sc in ranked if sp == r.truth]
        if s:
            own.setdefault(r.truth, []).append(s[0])
    capped = {}
    print(f"  {'voice':<14}{'bar':>7}{'margin':>8}{'own rows':>10}{'p50':>7}{'p60':>7}"
          f"{'p75':>7}   verdict")
    for sp, (t, m) in sorted(thr.items()):
        a = np.array(own.get(sp, []))
        if a.size == 0:
            print(f"  {names.get(sp, sp)[:13]:<14}{t:>7.2f}{m:>8.2f}{0:>10}"
                  f"{'-':>7}{'-':>7}{'-':>7}   REFUSED: ground truth never confirms it")
            continue
        p60 = float(np.percentile(a, 60))
        ok = t <= p60
        capped[sp] = (t, m) if ok else (min(t, p60), m)
        print(f"  {names.get(sp, sp)[:13]:<14}{t:>7.2f}{m:>8.2f}{a.size:>10}"
              f"{float(np.percentile(a, 50)):>7.3f}{p60:>7.3f}"
              f"{float(np.percentile(a, 75)):>7.3f}   "
              + ("kept" if ok else f"capped to {p60:.2f}"))
    sim_d = M.Sim(base, capped, names, you, M.Rules()).run(segs)
    s_d = score_online(sim_d, segs, you)
    print(f"  the evening under (d): mints {len(sim_d.minted)}, phantom rows "
          f"{phantom_rows(sim_d, segs)}, {s_d.correct}/{s_d.wrong}/{s_d.declined}, "
          f"prec {s_d.precision * 100:.1f}% rec {s_d.recall * 100:.1f}% "
          f"F-0.5 {s_d.f_beta():.3f}")
    sim_dc = M.Sim(base, capped, names, you, M.Rules(fitted_never_mints=True)).run(segs)
    s_dc = score_online(sim_dc, segs, you)
    print(f"  the evening under (d)+(b): mints {len(sim_dc.minted)}, phantom rows "
          f"{phantom_rows(sim_dc, segs)}, {s_dc.correct}/{s_dc.wrong}/{s_dc.declined}, "
          f"prec {s_dc.precision * 100:.1f}% rec {s_dc.recall * 100:.1f}% "
          f"F-0.5 {s_dc.f_beta():.3f}")

    # ---- what the nightly gate saw, and what it would see -----------------
    print(f"\n{'=' * 110}\n2b. (e) The nightly gate, with and without the mint path\n{'=' * 110}")
    import json
    op = json.loads(c.execute(
        "SELECT prior_state FROM operations WHERE op='identity.calibrate' "
        "ORDER BY at_utc_ns DESC LIMIT 1").fetchone()[0])
    for tag in ("baseline", "candidate"):
        s = op[tag]
        print(f"  as the gate scored it, {tag:<10} n={s['n']} correct={s['correct']} "
              f"wrong={s['wrong']} declined={s['declined']} "
              f"prec={s['precision'] * 100:.1f}% rec={s['recall'] * 100:.1f}% "
              f"F-0.5={s['f_beta']:.3f}")
    print(f"  the gate installed it: declines {op['baseline']['declined']} → "
          f"{op['candidate']['declined']} (+{op['candidate']['declined'] - op['baseline']['declined']}), "
          f"and a decline is free in that arithmetic.")
    # the same two arms, scored with a mint counted for what it is
    for tag, rules, thrs in (("baseline (globals)", M.Rules(), {}),
                             ("candidate (fitted)", M.Rules(), thr),
                             ("baseline, ladder+(b)", M.Rules(fitted_never_mints=True), {}),
                             ("candidate, ladder+(b)", M.Rules(fitted_never_mints=True), thr)):
        sim = M.Sim(base, thrs, names, you, rules).run(segs)
        s = score_online(sim, segs, you)
        print(f"  mint-aware, {tag:<20} n={s.n} correct={s.correct} wrong={s.wrong} "
              f"declined={s.declined} prec={s.precision * 100:.1f}% "
              f"rec={s.recall * 100:.1f}% F-0.5={s.f_beta():.3f}  "
              f"mints={len(sim.minted)}")

    # ---- the counterfactual the whole thing turns on --------------------
    print(f"\n{'=' * 110}\n3. Rowan's own turns against Rowan's own bar\n{'=' * 110}")
    rowan = [sp for sp, n in names.items() if n == "Rowan"]
    for sp in rowan:
        bar = thr.get(sp, M.GLOBAL)[0]
        scores = []
        sim = M.Sim(base, thr, names, you, M.Rules())
        for seg in segs:
            if seg.truth == sp and seg.kind != "mic" and seg.verdict == "single":
                ranked = sim._rank(seg)
                own = [s for v, s in ranked if v == sp]
                if own:
                    scores.append(own[0])
            sim.step(seg)
        if scores:
            import numpy as np
            a = np.array(scores)
            print(f"  {names[sp]}: n={len(a)}  own-voice score against the bank of the moment")
            for q in (10, 25, 40, 50, 60, 75, 90):
                print(f"    p{q:<3} {np.percentile(a, q):.3f}")
            print(f"    the bar the nightly pass installed: {bar:.2f} "
                  f"→ {100 * float((a < bar).mean()):.0f}% of Rowan's own turns fall under it")


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else M.DB)
