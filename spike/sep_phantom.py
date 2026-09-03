"""The corpus split that the headline turned on: which `overlap` rows are
*acoustically* overlapped at all.

Every `overlap` verdict in this database sits on Discord application audio, and
a Discord client does not play your own microphone back to you (§17, finding 1).
So a turn where Discord says "nerdrx and Aspen were both speaking" carries only
Aspen on the wire. Those rows are labelled overlap and are single-speaker audio.

This script measures that claim instead of asserting it, three ways:

  1. the mixture's top-1 name, split by whether the pair contains the user;
  2. how often the mixture names the *user's own voice* on a row where Discord
     says the user was talking -- if the user's audio were present, it should
     sometimes win;
  3. the pyannote detector's reading on each subset, which is §26's number
     recomputed on rows that can actually contain two voices.

Usage: NXR_SEP=<dir> python sep_phantom.py sep_mixture_all.json
"""
import json
import os
import sys

import numpy as np

D = os.environ["NXR_SEP"]
os.chdir(D)
import sep_lib as L  # noqa: E402

SELF = 26  # nerdrx's mic voice
rows = json.load(open(sys.argv[1] if len(sys.argv) > 1 else "sep_mixture_all.json"))
corpus = {r["segment_id"]: r for r in json.load(open("corpus.json"))}


def top(r, per_voice=False):
    sid, sc = r["mix_top"][0]
    return sid if sc >= L.thr(sid, per_voice) else None


print(f"{'pair':<16} {'n':>5} {'labelled':>9} {'top in pair':>12} {'top = self':>11} "
      f"{'top=other':>10} {'steal':>7} {'det ovl mean':>13} {'det>0.10':>9}")
groups = {}
for r in rows:
    groups.setdefault(r["pair"], []).append(r)

for pair, rs in sorted(groups.items(), key=lambda kv: -len(kv[1])):
    n = len(rs)
    lab = [top(r) for r in rs]
    inpair = sum(t is not None and t in r["true_sids"] for t, r in zip(lab, rs))
    isself = sum(t == SELF for t in lab)
    other = sum(t is not None and t in r["true_sids"] and t != SELF
                for t, r in zip(lab, rs))
    steal = sum(t is not None and t not in r["true_sids"] for t, r in zip(lab, rs))
    det = [r["detector_overlap_frac"] for r in rs
           if r["detector_overlap_frac"] is not None]
    print(f"{pair:<16} {n:>5} {100*sum(t is not None for t in lab)/n:>8.1f}% "
          f"{100*inpair/n:>11.1f}% {100*isself/n:>10.1f}% {100*other/n:>9.1f}% "
          f"{100*steal/n:>6.1f}% {np.mean(det):>13.4f} "
          f"{100*np.mean([d > 0.10 for d in det]):>8.1f}%")

# --- the same thing said as one number ------------------------------------
ac = [r for r in rows if SELF not in r["true_sids"]]
ph = [r for r in rows if SELF in r["true_sids"]]
print(f"\nacoustically real (no self in pair): n={len(ac)}, "
      f"detector mean {np.mean([r['detector_overlap_frac'] for r in ac]):.4f}")
print(f"self in pair (audio cannot contain the user): n={len(ph)}, "
      f"detector mean {np.mean([r['detector_overlap_frac'] for r in ph]):.4f}")
sself = sum(top(r) == SELF for r in ph)
print(f"rows where Discord says the user spoke and the audio was labelled as the "
      f"user's own voice: {sself}/{len(ph)} = {100*sself/len(ph):.1f}%")

# --- §26's correlation, recomputed on rows that can contain two voices -----
for tag, rs in (("all overlap rows", rows), ("acoustically real only", ac)):
    x = np.array([r["detector_overlap_frac"] for r in rs
                  if r["detector_overlap_frac"] is not None
                  and r["truth_overlap_frac"] is not None])
    y = np.array([r["truth_overlap_frac"] for r in rs
                  if r["detector_overlap_frac"] is not None
                  and r["truth_overlap_frac"] is not None])
    r_ = np.corrcoef(x, y)[0, 1] if len(x) > 2 else float("nan")
    print(f"{tag:<26} n={len(x):>5}  Pearson r(detector, truth simultaneous) = {r_:+.3f}")

# --- does the mixture name the louder half? --------------------------------
dom = sub = 0
for r in ac:
    t = top(r)
    if t is None:
        continue
    # true_sids is sorted by coverage descending in sep_corpus.py
    if t == r["true_sids"][0]:
        dom += 1
    elif t == r["true_sids"][1]:
        sub += 1
print(f"\non acoustically real rows the mixture names the higher-coverage speaker "
      f"{dom} times and the lower-coverage one {sub} times "
      f"({100*dom/max(1,dom+sub):.1f}% / {100*sub/max(1,dom+sub):.1f}%)")
