"""Is the matched-space gain discrimination, or a uniform shift?

`sep_matched.py` reports that separated streams score 0.638 against prototypes
that are themselves separated, beating the 0.551 the clean mixture gets against
clean prototypes. That is the shape of a real fix -- and it is also exactly the
shape of an artifact. Two SepFormer outputs share the separator's own colouring,
so cosine between *any* two of them is raised, whether or not they are the same
person. A uniform shift raises genuine and impostor scores together and buys
nothing; it only looks like a win because the 0.35 bar was calibrated in the
other space.

So: held-out clean `single` turns for the two voices, separated, scored against
matched prototypes of the **right** voice (genuine) and the **wrong** voice
(impostor), and the same pair in the clean space. The number that decides it is
the gap, not the level.

Usage: NXR_SEP=<dir> python sep_matched_check.py [--per-voice 40] [--n 80]
"""
import argparse
import json
import os
import random

import numpy as np

D = os.environ["NXR_SEP"]
os.chdir(D)
import sep_lib as L  # noqa: E402

ap = argparse.ArgumentParser()
ap.add_argument("--per-voice", type=int, default=40)
ap.add_argument("--n", type=int, default=80)
ap.add_argument("--out", default="sep_matched_check.json")
a = ap.parse_args()

VOICES = [2, 25]
emb, bank = L.Embedder(), L.Bank("protos.json")
sep = L.SepFormer("sepformer_ckpt")

ctrl = [r for r in json.load(open("control.json"))
        if r["speaker_id"] in VOICES and r["dur_s"] >= 1.5]
random.Random(7).shuffle(ctrl)  # same seed as sep_matched.py: same enrolment set

matched, used = {v: [] for v in VOICES}, set()
for r in ctrl:
    v = r["speaker_id"]
    if len(matched[v]) >= a.per_voice:
        continue
    x = L.decode(r["wav"])
    vecs = [emb(s) for s in sep(x)]
    matched[v].append(max(vecs, key=lambda vv: bank.score(vv, v, r["segment_id"]) or -1))
    used.add(r["segment_id"])
    if all(len(matched[u]) >= a.per_voice for u in VOICES):
        break
M = {}
for v in VOICES:
    m = np.array(matched[v], dtype=np.float32)
    M[v] = m / (np.linalg.norm(m, axis=1, keepdims=True) + 1e-12)

# held-out clean turns for the same two voices
probe = [r for r in ctrl if r["segment_id"] not in used][:a.n]
rows = []
for i, r in enumerate(probe):
    v = r["speaker_id"]
    other = 25 if v == 2 else 2
    x = L.decode(r["wav"])
    seg = r["segment_id"]
    vc = emb(x)
    vs = [emb(s) for s in sep(x)]
    best = max(vs, key=lambda vv: float((M[v] @ vv).max()))
    rows.append({
        "segment_id": seg, "speaker_id": v,
        "clean_gen": bank.score(vc, v, seg), "clean_imp": bank.score(vc, other, seg),
        "match_gen": float((M[v] @ best).max()),
        "match_imp": float((M[other] @ best).max()),
    })
    if (i + 1) % 20 == 0:
        print(f"{i+1}/{len(probe)}", flush=True)

json.dump(rows, open(a.out, "w"), indent=1)
rows = [r for r in rows if r["clean_gen"] is not None and r["clean_imp"] is not None]


def stat(g, im):
    g, im = np.array(g), np.array(im)
    # equal error rate over the pooled scores
    sc = np.concatenate([g, im])
    best = (1.0, None)
    for t in np.unique(sc):
        far = float((im >= t).mean())
        frr = float((g < t).mean())
        if abs(far - frr) < best[0]:
            best = (abs(far - frr), (t, (far + frr) / 2))
    return g.mean(), im.mean(), g.mean() - im.mean(), best[1]


print(f"\nheld-out clean turns for Rowan and Aspen, n = {len(rows)}")
print(f"{'space':<34} {'genuine':>8} {'impostor':>9} {'gap':>7} {'EER':>7} {'@thr':>6}")
for tag, gk, ik in (("clean audio vs clean prototypes", "clean_gen", "clean_imp"),
                    ("separated vs MATCHED prototypes", "match_gen", "match_imp")):
    g, im, gap, (thr, eer) = stat([r[gk] for r in rows], [r[ik] for r in rows])
    print(f"{tag:<34} {g:>8.4f} {im:>9.4f} {gap:>7.4f} {eer:>6.1%} {thr:>6.3f}")
print("\nIf the gap and the EER do not improve, the level did and the fix did not.")
