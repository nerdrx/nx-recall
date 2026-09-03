"""One-clip smoke test: does the chain load, separate, embed and score at all."""
import json
import time

import os, sys
D = os.environ["NXR_SEP"]
os.chdir(D)
import sep_lib as L

c = [x for x in json.load(open("corpus.json")) if x["pair"] == "Aspen+Rowan"]
row = c[0]
x = L.decode(row["wav"])
print("clip", round(len(x) / 16000, 2), "s", "users",
      [(u["name"], u["speaker_id"], u["coverage"]) for u in row["users"]])

e = L.Embedder()
t = time.time()
v = e(x)
print("emb", v.shape, "t", round(time.time() - t, 3))
b = L.Bank("protos.json")
print("mixture rank", [(s, round(sc, 3)) for s, sc in b.rank(v, row["segment_id"])[:3]])

t = time.time()
s = L.SepFormer("sepformer_ckpt")
print("sepformer load", round(time.time() - t, 1), "s")
t = time.time()
a, bb = s(x)
d = time.time() - t
print("separate", a.shape, "t", round(d, 2), "RTF", round(d / (len(x) / 16000), 2))
for tag, st in (("A", a), ("B", bb)):
    print(tag, [(sid, round(sc, 3)) for sid, sc in b.rank(e(st), row["segment_id"])[:3]])
