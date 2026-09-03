"""The remedy the artifact tax suggests: enrol in the separator's space.

If the loss is a domain shift -- ERes2Net not recognising a separator's output
as speech it was trained on -- then comparing separated audio against
prototypes that are *also* separated should cancel it. This builds a matched
bank: clean `single` turns for the two voices, pushed through the same
SepFormer, dominant stream kept, and used as prototypes. Then the 203
acoustically-real overlap rows are rescored, streams against matched
prototypes.

If this does not recover the loss, the tax is not a coordinate-system problem
and no amount of re-enrolment fixes it.

Usage: NXR_SEP=<dir> python sep_matched.py [--per-voice 40]
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
ap.add_argument("--out", default="sep_matched.json")
a = ap.parse_args()

VOICES = [2, 25]  # Rowan, Aspen -- the two in the acoustically-real pair
emb = L.Embedder()
bank = L.Bank("protos.json")
sep = L.SepFormer("sepformer_ckpt")

# --- build the matched bank ------------------------------------------------
ctrl = [r for r in json.load(open("control.json"))
        if r["speaker_id"] in VOICES and r["dur_s"] >= 1.5]
random.Random(7).shuffle(ctrl)
matched, used = {v: [] for v in VOICES}, {v: [] for v in VOICES}
for r in ctrl:
    v = r["speaker_id"]
    if len(matched[v]) >= a.per_voice:
        continue
    x = L.decode(r["wav"])
    streams = sep(x)
    # keep the stream that is more like this voice under the *clean* bank --
    # the enrolment side is allowed to use the clean prototypes, because at
    # enrolment time the audio is known to be one person.
    vecs = [emb(s) for s in streams]
    best = max(vecs, key=lambda vv: bank.score(vv, v, r["segment_id"]) or -1)
    matched[v].append(best)
    used[v].append(r["segment_id"])
    if all(len(matched[u]) >= a.per_voice for u in VOICES):
        break
for v in VOICES:
    print(f"matched prototypes for speaker {v}: {len(matched[v])}")

M = {v: np.array(matched[v], dtype=np.float32) for v in VOICES}
for v in VOICES:
    M[v] /= np.linalg.norm(M[v], axis=1, keepdims=True) + 1e-12

# --- rescore the acoustically-real rows ------------------------------------
rows = [r for r in json.load(open("corpus.json")) if r["pair"] == "Aspen+Rowan"]
out = []
for i, r in enumerate(rows):
    seg = r["segment_id"]
    if seg in used[2] or seg in used[25]:
        continue
    x = L.decode(r["wav"])
    sids = [u["speaker_id"] for u in r["users"]]
    streams = sep(x)
    vs = [emb(s) for s in streams]
    rec = {"segment_id": seg, "true_sids": sids,
           "clean_mix": [round(bank.score(emb(x), s, seg) or -1, 4) for s in sids],
           "clean_sep": [[round(bank.score(v, s, seg) or -1, 4) for s in sids]
                         for v in vs],
           "matched_sep": [[round(float((M[s] @ v).max()), 4) for s in sids]
                           for v in vs]}
    out.append(rec)
    if (i + 1) % 40 == 0:
        print(f"{i+1}/{len(rows)}", flush=True)

json.dump(out, open(a.out, "w"), indent=1)
cm = np.array([r["clean_mix"] for r in out])
cs = np.array([r["clean_sep"] for r in out])
ms = np.array([r["matched_sep"] for r in out])
print(f"\nn = {len(out)} acoustically-real rows, two voices")
print(f"mixture   vs clean prototypes, best over the two users : "
      f"{cm.max(1).mean():.4f}")
print(f"separated vs clean prototypes, best stream x user      : "
      f"{cs.max(axis=(1,2)).mean():.4f}")
print(f"separated vs MATCHED prototypes, best stream x user    : "
      f"{ms.max(axis=(1,2)).mean():.4f}")
print(f"\nper-user, best stream:")
for j, nm in enumerate(("higher-coverage user", "lower-coverage user")):
    print(f"  {nm:<22} mixture {cm[:, j].mean():.4f}  "
          f"separated/clean {cs[:, :, j].max(1).mean():.4f}  "
          f"separated/matched {ms[:, :, j].max(1).mean():.4f}")
# both-recovered under the matched bank, closed set (the friendliest reading)
both = 0
for r in out:
    m = np.array(r["matched_sep"])
    ta, tb = (L.thr(s, False) for s in r["true_sids"])
    if (m[0, 0] >= ta and m[1, 1] >= tb) or (m[0, 1] >= tb and m[1, 0] >= ta):
        both += 1
print(f"\nboth recovered, matched bank, closed set, global 0.35 bar: "
      f"{both}/{len(out)} = {100*both/len(out):.1f}%")
