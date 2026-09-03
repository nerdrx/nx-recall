"""Q1/Q2 -- does separating a two-speaker turn recover two names?

Arms, all scored with the daemon's own embedder and the daemon's ladder
(`identity::rank`, a speaker's score is the max cosine over its prototypes, and
a prototype sourced from the row under judgement is dropped for that row):

  mixture      today's behaviour: one embedding of the whole turn, ranked
               against the whole voicebank, top-1 above its bar.
  sep-open     separate into two streams, rank each against the **whole**
               voicebank independently. "Both" means the two top-1 labels that
               clear their bars are exactly the two people Discord names.
  sep-assign   Q2's cheap target-speaker stand-in: separate, then assign the two
               streams to the two known prototypes by the better of the two
               permutations. Closed-set, so it cannot name an outsider -- it is
               the ceiling a real TSE model would be chasing, not a shipping
               arm on its own.

Usage:  NXR_SEP=<dir> python sep_bench.py [--limit N] [--pair Aspen+Rowan]
                                          [--device cpu] [--out results.json]
"""
import argparse
import json
import os
import time

import numpy as np

D = os.environ["NXR_SEP"]
os.chdir(D)
import sep_lib as L  # noqa: E402

ap = argparse.ArgumentParser()
ap.add_argument("--limit", type=int, default=0)
ap.add_argument("--pair", default=None, help="restrict to one truth pair")
ap.add_argument("--device", default="cpu")
ap.add_argument("--out", default="sep_results.json")
ap.add_argument("--no-sep", action="store_true", help="mixture arm only")
a = ap.parse_args()

corpus = json.load(open("corpus.json"))
if a.pair:
    corpus = [r for r in corpus if r["pair"] == a.pair]
corpus.sort(key=lambda r: r["segment_id"])
if a.limit:
    corpus = corpus[:a.limit]

emb = L.Embedder()
bank = L.Bank("protos.json")
sep = None if a.no_sep else L.SepFormer("sepformer_ckpt", device=a.device)

out = []
t0 = time.time()
for i, row in enumerate(corpus):
    x = L.decode(row["wav"])
    seg = row["segment_id"]
    true_sids = [u["speaker_id"] for u in row["users"]]
    rec = {"segment_id": seg, "pair": row["pair"], "dur_s": row["dur_s"],
           "truth_overlap_frac": row["truth_overlap_frac"],
           "detector_overlap_frac": row["detector_overlap_frac"],
           "true_sids": true_sids,
           "coverage": [u["coverage"] for u in row["users"]]}

    mix_rank = bank.rank(emb(x), seg)
    rec["mix_top"] = [[s, round(c, 4)] for s, c in mix_rank[:3]]
    rec["mix_true"] = [round(bank.score(emb(x), s, seg) or -1, 4) for s in true_sids]

    if sep is not None:
        ts = time.time()
        streams = sep(x)
        rec["sep_s"] = round(time.time() - ts, 3)
        ranks, trues = [], []
        for st in streams:
            v = emb(st)
            ranks.append([[s, round(c, 4)] for s, c in bank.rank(v, seg)[:3]])
            trues.append([round(bank.score(v, s, seg) or -1, 4) for s in true_sids])
        rec["sep_top"] = ranks
        rec["sep_true"] = trues  # [stream][true-user] cosine
    out.append(rec)
    if (i + 1) % 20 == 0:
        el = time.time() - t0
        print(f"{i+1}/{len(corpus)}  {el:.0f}s  eta {el/(i+1)*(len(corpus)-i-1):.0f}s",
              flush=True)

json.dump(out, open(a.out, "w"))
print(f"wrote {a.out}: {len(out)} rows in {time.time()-t0:.0f}s")
