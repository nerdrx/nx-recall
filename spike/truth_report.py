"""Score the machine against hand labels — the unbiased numbers.

Run after tagging at http://127.0.0.1:7743 fills clips/labels.json.

Replaces the two self-graded figures from analyze_recording.py:
  - enrollment match rate (was ~95%, circular): now enroll on a person's TRUE
    3 longest clips, test on their OTHER true clips, count false accepts against
    everyone else's true clips.
  - clustering quality: purity of unsupervised clusters vs truth.

Clips tagged (unsure)/(multiple)/(not speech) are excluded from identity scoring
but reported, since their rate is itself a finding.
"""
import json
import sys
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).parent))
from harness import SR, Embedder  # noqa: E402
from measure_lobby import EMB  # noqa: E402

D = Path(__file__).parent
SPECIAL = {"(unsure)", "(multiple)", "(not speech)"}
THRS = [0.25, 0.30, 0.35, 0.40, 0.45]


def main() -> int:
    import soundfile as sf

    meta = json.loads((D / "clips" / "regions.json").read_text())
    labels = {int(k): v for k, v in
              json.loads((D / "clips" / "labels.json").read_text()).items()}
    n_all = len(meta["clips"])
    print(f"\ntagged {len(labels)}/{n_all} clips")
    for s in SPECIAL:
        c = sum(1 for v in labels.values() if v == s)
        if c:
            print(f"  {s:>13}: {c}  ({c/len(labels)*100:.0f}%)")

    people: dict[str, list[int]] = {}
    for cid, who in labels.items():
        if who not in SPECIAL:
            people.setdefault(who, []).append(cid)
    print(f"\n  named people: {len(people)}")
    for w, ids in sorted(people.items(), key=lambda kv: -len(kv[1])):
        print(f"    {w:<20} {len(ids):3d} clips")

    emb = Embedder(EMB, num_threads=4)
    vecs, durs = {}, {}
    for cid in labels:
        if labels[cid] in SPECIAL:
            continue
        x, _ = sf.read(D / "clips" / f"clip_{cid:03d}.wav", dtype="float32")
        vecs[cid] = emb(x)
        durs[cid] = len(x) / SR

    testable = {w: ids for w, ids in people.items() if len(ids) >= 5}
    if not testable:
        print("\nNeed >=5 clips for at least one person to score enrollment.")
        return 1

    print("\nUNBIASED ENROLLMENT  (3 longest true clips enrolled, rest held out)")
    print(f"  {'thr':>5} {'recall':>8} {'false-accept':>13} {'wrong-name':>11}")
    for thr in THRS:
        tp = fn = fa = wrong = imp = 0
        banks = {w: [vecs[c] for c in sorted(ids, key=lambda c: -durs[c])[:3]]
                 for w, ids in testable.items()}
        for cid, v in vecs.items():
            who = labels[cid]
            scores = {w: max(float(v @ p) for p in b) for w, b in banks.items()
                      if not (who == w and cid in
                              sorted(people[w], key=lambda c: -durs[c])[:3])}
            if not scores:
                continue
            best = max(scores, key=scores.get)
            hit = scores[best] >= thr
            if who in testable and cid not in sorted(people[who], key=lambda c: -durs[c])[:3]:
                if hit and best == who:
                    tp += 1
                elif hit:
                    wrong += 1
                else:
                    fn += 1
            elif who not in testable:      # impostor: person not in the bank
                imp += 1
                if hit:
                    fa += 1
        rec = tp / max(1, tp + fn + wrong)
        print(f"  {thr:>5.2f} {rec*100:>7.1f}% {fa}/{imp:>4} imp {'':>3} {wrong:>6}")

    print("\n  recall = named person's held-out clips correctly matched to them")
    print("  false-accept = clips of people NOT in the bank that matched anyone")
    print("  wrong-name = in-bank clips confidently matched to the WRONG person")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
