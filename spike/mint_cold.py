"""What the brakes cost a voice the bank has never heard.

§32 named this as the corpus's hole: "every scorable held-out row is Rowan or
Aspen, and nothing in this round measures what any of it does to *minting*".
The mint round cannot leave it there — rule (c) refuses a label to a voice with
one prototype and no name, and a genuinely new person *is* a voice with one
prototype and no name.

This install does have such evenings, just without Discord verdicts on them:
2026-09-02 20:40 brought Albe, Dexter and Idef into an empty-for-them bank, and
2026-09-03 01:46 brought Camey, kyami and JulietteDieSc. Their turns' ground
truth here is the label the archive settled on after the user named the voice —
weaker than a Discord verdict, and every number says so — but it is the only
record of what a cold start looks like on this box.

    python3 spike/mint_cold.py [path/to/copy.db]

Read-only, against a COPY of the database. Never the live one.
"""

import sys
from collections import Counter

sys.path.insert(0, __file__.rsplit("/", 1)[0])
import mint_lib as M  # noqa: E402
import recog_lib as R  # noqa: E402

R.MAX_OVERLAP = M.MAX_OVERLAP

# The two cold starts, as `(name, first mint, window end)` in local-clock ns.
EVENINGS = [
    ("2026-09-02 evening — Albe, Dexter, Idef arrive", 1788374400_000000000,
     1788381600_000000000),
    ("2026-09-03 small hours — Camey, kyami, Juliette arrive", 1788392400_000000000,
     1788399600_000000000),
]


def main(db=M.DB):
    c = R.conn(db)
    you = R.you_speaker_id(c)
    names = R.names(c)
    thr = M.installed_thresholds(c)

    for title, t0, t1 in EVENINGS:
        base = M.bank_at(c, t0)
        segs = [s for s in M.load_segments(c, t0) if s.t < t1]
        # The archive's settled answer, for turns that got one.
        key = {s.id: s.speaker_now for s in segs if s.speaker_now is not None}
        newcomers = {
            r[0] for r in c.execute(
                "SELECT id FROM speakers WHERE created_at >= ? AND created_at < ? ",
                (t0, t1))
        }
        merged = dict(c.execute("SELECT id, merged_into FROM speakers"))

        def settled(sp):
            seen = set()
            while sp is not None and merged.get(sp) is not None and sp not in seen:
                seen.add(sp)
                sp = merged[sp]
            return sp

        print(f"\n{'=' * 104}\n{title}\n{'=' * 104}")
        print(f"  bank at the start {len(base)} prototypes over "
              f"{len({p['speaker'] for p in base})} voices; {len(segs)} turns; "
              f"the archive minted {len(newcomers)} voices in the window")
        print(f"  {'rule':<50}{'mints':>7}{'named turns':>13}{'agree':>8}"
              f"{'disagree':>10}{'unnamed':>9}")
        arms = [
            ("(shipping)", M.Rules()),
            ("(a) near miss within 0.10", M.Rules(near_miss=0.10)),
            ("(b) a fitted bar declines, it never mints", M.Rules(fitted_never_mints=True)),
            ("(c) unnamed under 2 prototypes cannot win", M.Rules(min_prototypes=2)),
            ("(c) unnamed under 3 prototypes cannot win", M.Rules(min_prototypes=3)),
            ("(b)+(c3)", M.Rules(fitted_never_mints=True, min_prototypes=3)),
        ]
        for name, rules in arms:
            sim = M.Sim(base, thr, names, you, rules).run(segs)
            agree = dis = unnamed = named = 0
            for s in segs:
                if s.kind == "mic" or s.id not in key:
                    continue
                got = sim.labels.get(s.id)
                want = settled(key[s.id])
                if got is None:
                    unnamed += 1
                    continue
                named += 1
                # A voice this run invented stands for "somebody new": correct
                # when the archive also settled on a voice minted in the window.
                if got in sim.minted:
                    agree += 1 if want in newcomers else 0
                    dis += 0 if want in newcomers else 1
                else:
                    agree += 1 if settled(got) == want else 0
                    dis += 0 if settled(got) == want else 1
            print(f"  {name:<50}{len(sim.minted):>7}{named:>13}{agree:>8}{dis:>10}{unnamed:>9}")
            if name == "(shipping)":
                print(f"      the voices it minted took "
                      + str(Counter(
                          settled(key[s.id]) and names.get(settled(key[s.id]), '?')
                          for s in segs
                          if sim.labels.get(s.id) in sim.minted and s.id in key).most_common(5)))


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else M.DB)
