"""Q4 -- real-time factor and memory for the separation model.

Windows of 3 s (the daemon's identity window) plus the corpus's own median turn,
on 4 CPU cores and, if a ROCm torch is present and the card is idle, on the GPU.
Peak RSS is read from /proc, so it is the whole process including torch -- the
number a scheduling decision actually has to live with, not a parameter count.

Usage: NXR_SEP=<dir> python sep_rtf.py [--device cpu|cuda] [--reps 10]
"""
import argparse
import json
import os
import resource
import time

import numpy as np

D = os.environ["NXR_SEP"]
os.chdir(D)
import sep_lib as L  # noqa: E402

ap = argparse.ArgumentParser()
ap.add_argument("--device", default="cpu")
ap.add_argument("--reps", type=int, default=8)
ap.add_argument("--out", default=None)
a = ap.parse_args()

rss0 = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1024
t = time.time()
sep = L.SepFormer("sepformer_ckpt", device=a.device)
load_s = time.time() - t
rss1 = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1024

rows = json.load(open("corpus.json"))
med = float(np.median([r["dur_s"] for r in rows if r["pair"] == "Aspen+Rowan"]))
res = {"device": a.device, "load_s": round(load_s, 2),
       "rss_after_load_mb": round(rss1, 1), "windows": []}

for dur in (1.0, 3.0, round(med, 2), 10.0):
    x = np.random.default_rng(0).standard_normal(int(dur * 16000)).astype(np.float32) * 0.05
    sep(x)  # warm
    ts = []
    for _ in range(a.reps):
        t = time.time()
        sep(x)
        ts.append(time.time() - t)
    ts = np.array(ts)
    res["windows"].append({
        "window_s": dur, "median_s": round(float(np.median(ts)), 3),
        "p90_s": round(float(np.percentile(ts, 90)), 3),
        "rtf_median": round(float(np.median(ts) / dur), 3)})
    print(f"{dur:5.2f}s window  median {np.median(ts)*1000:7.1f} ms  "
          f"RTF {np.median(ts)/dur:5.2f}", flush=True)

res["peak_rss_mb"] = round(resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1024, 1)
print(json.dumps(res, indent=1))
if a.out:
    json.dump(res, open(a.out, "w"), indent=1)
