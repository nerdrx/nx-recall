"""Is the loss a separation failure, or a tax the embedder charges on anything
that came out of a separator?

Run clean `single`-verdict turns -- one speaker, nothing to separate -- through
the same SepFormer pass and score the better stream against the same prototype
the mixture was scored against. If a pointless separation still costs cosine,
the loss is not the separator failing to split two voices; it is ERes2Net
refusing to recognise its own training distribution in a separator's output,
and no better separator fixes it.

Usage: NXR_SEP=<dir> python sep_tax.py [--n 120]
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
ap.add_argument("--n", type=int, default=120)
ap.add_argument("--out", default="sep_tax.json")
a = ap.parse_args()

ctrl = [r for r in json.load(open("control.json")) if r["dur_s"] >= 1.5]
random.Random(0xFACE).shuffle(ctrl)
ctrl = ctrl[:a.n]

emb, bank = L.Embedder(), L.Bank("protos.json")
sep = L.SepFormer("sepformer_ckpt")

out = []
for i, r in enumerate(ctrl):
    x = L.decode(r["wav"])
    sid, seg = r["speaker_id"], r["segment_id"]
    mix = bank.score(emb(x), sid, seg)
    if mix is None:
        continue
    streams = sep(x)
    ss = [bank.score(emb(s), sid, seg) for s in streams]
    # open-set: would the better stream still be labelled the right person?
    ranks = [bank.rank(emb(s), seg)[0] for s in streams]
    out.append({"segment_id": seg, "speaker_id": sid, "name": r["name"],
                "dur_s": r["dur_s"], "mix": round(mix, 4),
                "streams": [round(v, 4) for v in ss],
                "best": round(max(ss), 4),
                "mix_top": bank.rank(emb(x), seg)[0][0],
                "stream_tops": [t[0] for t in ranks]})
    if (i + 1) % 20 == 0:
        print(f"{i+1}/{len(ctrl)}", flush=True)

json.dump(out, open(a.out, "w"), indent=1)
mix = np.array([r["mix"] for r in out])
best = np.array([r["best"] for r in out])
print(f"\nclean single-speaker turns, n = {len(out)}")
print(f"cosine to the right prototype, mixture (i.e. the clean turn): {mix.mean():.4f}")
print(f"cosine after a pointless separation pass, better stream:      {best.mean():.4f}")
print(f"artifact tax:                                                 "
      f"{best.mean()-mix.mean():+.4f}")
lab_mix = sum(r["mix_top"] == r["speaker_id"] for r in out)
lab_sep = sum(r["speaker_id"] in r["stream_tops"] for r in out)
print(f"top-1 is the right voice: clean {100*lab_mix/len(out):.1f}%  "
      f"after separation (either stream) {100*lab_sep/len(out):.1f}%")
