"""The recognition round: what the live box does, and what would do better.

Usage:
    python3 spike/recog_bench.py validate    # reproduce learn_truth_bench
    python3 spike/recog_bench.py shipping    # what the LIVE box actually does
"""

import sys
import numpy as np

import recog_lib as L


def corpus():
    c = L.conn()
    rows = L.load_rows(c)
    bank = L.Bank(L.load_bank(c))
    you = L.you_speaker_id(c)
    nm = L.names(c)
    cut = L.split_at([r.t for r in rows])
    return c, rows, bank, you, nm, cut


def main():
    what = sys.argv[1] if len(sys.argv) > 1 else "validate"
    c, rows, bank, you, nm, cut = corpus()
    fit, ev = rows[:cut], rows[cut:]
    print(f"truth rows      {len(rows)} total, {len(fit)} fit / {len(ev)} held out")
    print(f"bank            {len(bank.protos)} prototypes over {len(bank.voices)} voices")
    print(f"own account     speaker {you} (excluded)")

    globals_ = {}
    obs = L.obs_for_fit(fit, bank)
    ft = L.fit_thresholds(obs)
    per_voice = L.thresholds_from_fit(ft)
    for v, (t, m, n, f, fg) in sorted(ft.items()):
        print(f"  fitted voice {v:<3} {nm.get(v, '?'):<14} n={n:<4} t={t}  m={m}"
              f"  F {fg:.3f} -> {f:.3f}")

    proj, meta = L.load_projection(c)
    print(f"installed projection: {meta}")

    print()
    print(L.HEADER)
    base, _ = L.judge(ev, bank, globals_, you=you)
    print(base.row("A. globals, raw (0.10.2)"))
    pv, wrong_pv = L.judge(ev, bank, per_voice, you=you)
    print(pv.row("B. + per-voice thresholds, raw"))
    if proj is not None:
        pbank = bank.projected(proj)
        pg, _ = L.judge(ev, pbank, globals_, you=you, project=proj)
        print(pg.row("C. globals, installed projection"))
        pp, wrong_pp = L.judge(ev, pbank, per_voice, you=you, project=proj)
        print(pp.row("D. per-voice + installed projection"))
        print()
        print("  D is what the live daemon does today "
              f"(learn=true, projection installed, {len(per_voice)} learned thresholds).")

    if what == "validate":
        ok = (base.n, base.correct, base.wrong, base.declined) == (460, 422, 23, 15) and (
            pv.n, pv.correct, pv.wrong, pv.declined) == (460, 404, 13, 43)
        print(f"\nvalidate against learn_truth_bench: {'OK' if ok else 'MISMATCH'}")
        sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
