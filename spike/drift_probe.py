"""What the age decline actually is: every prototype, scored against every voice.

`drift_bench.py` finds a steep decline of match score with prototype age and a
nearly flat turn-to-turn decline over the same window. Both cannot be drift.
This probe tells them apart: for each prototype of a voice, its mean cosine
against that voice's ground-truth turns and against every OTHER voice's, plus
the day it was enrolled and what its own source segment's verdict says.

  * drift  -> an old prototype matches its own voice less and everyone else no
    more; the whole cloud slides together.
  * hygiene -> an old prototype matches its own voice less and SOMEBODY ELSE
    at least as well, or matches nothing at all.

Read-only, against a COPY of the database. Never the live one.
"""

import datetime
import sys
from collections import defaultdict

import numpy as np

sys.path.insert(0, __file__.rsplit("/", 1)[0])
import drift_lib as D  # noqa: E402
import recog_lib as R  # noqa: E402


def main(db=D.DB):
    c = R.conn(db)
    you = R.you_speaker_id(c)
    nm = R.names(c)
    protos = D.load_bank(c)
    rows = D.load_truth_rows(c) + D.load_mic_rows(c, you)

    by_voice = defaultdict(list)
    for r in rows:
        by_voice[r.truth].append(r)
    voices = [v for v in by_voice if len(by_voice[v]) >= 100]
    mats = {v: np.stack([D.normed(r.vec) for r in by_voice[v]]) for v in voices}
    ids = {v: {r.id for r in by_voice[v]} for v in voices}
    srcof = {}
    for v in voices:
        for r in by_voice[v]:
            srcof[r.id] = r.id

    verdicts = dict(
        c.execute(
            "SELECT id, COALESCE(truth_verdict,'-') FROM segments WHERE deleted_at IS NULL"
        )
    )

    for target in voices:
        mine = sorted(
            [p for p in protos if p["speaker"] == target],
            key=lambda p: (p["created"] or 0, p["id"]),
        )
        if not mine:
            continue
        print(f"\n{'=' * 108}\n{nm.get(target, target)}: each prototype vs every voice's "
              f"ground-truth turns\n{'=' * 108}")
        head = f"  {'proto':>6}{'enrolled':<22}{'src verdict':<12}{'app':<10}"
        head += "".join(f"{nm.get(v, v)[:8]:>10}" for v in voices)
        print(head + "   best")
        for p in mine:
            when = datetime.datetime.fromtimestamp(
                (p["created"] or 0) / 1e9, datetime.UTC
            ).strftime("%Y-%m-%d %H:%M")
            pv = D.normed(p["vec"])
            means = {}
            for v in voices:
                keep = np.ones(len(by_voice[v]), dtype=bool)
                if p["src"] is not None and p["src"] in ids[v]:
                    keep = np.array([r.id != p["src"] for r in by_voice[v]])
                means[v] = float((mats[v][keep] @ pv).mean())
            best = max(means, key=means.get)
            line = (
                f"  {p['id']:>6}{when:<22}"
                f"{verdicts.get(p['src'], '-')[:11]:<12}{p['app'][:9]:<10}"
            )
            line += "".join(f"{means[v]:>10.3f}" for v in voices)
            print(line + f"   {nm.get(best, best)}{'  <-- not its own voice' if best != target else ''}")

        # The same split the age table sees, as one number.
        cut = np.median([p["created"] or 0 for p in mine])
        for tag, sel in (("older half", [p for p in mine if (p["created"] or 0) <= cut]),
                         ("newer half", [p for p in mine if (p["created"] or 0) > cut])):
            if not sel:
                continue
            m = np.stack([D.normed(p["vec"]) for p in sel])
            own = float((mats[target] @ m.T).mean())
            others = {
                nm.get(v, v): round(float((mats[v] @ m.T).mean()), 3)
                for v in voices
                if v != target
            }
            print(f"    {tag}: {len(sel)} prototypes, mean cosine to own turns {own:.3f}, "
                  f"to others {others}")


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else D.DB)
