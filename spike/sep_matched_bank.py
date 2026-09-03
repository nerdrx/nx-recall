"""The open-set arm, redone in the separator's own space.

§29.4 showed that comparing separated audio against separated prototypes is not
a uniform shift: genuine 0.584 -> 0.689, impostor 0.403 -> 0.421, EER 10.0% ->
5.0% on held-out clean turns. But it was only two voices, and the arm that
decides whether anything ships is **open set** -- a stream ranked against the
whole voicebank, which can name an outsider.

So rebuild the whole voicebank in the matched space. Every prototype in
`speaker_prototypes` that still has its source segment's WAV on disk is pushed
through the same SepFormer, and the stream nearest the stored prototype becomes
that prototype's matched twin. The mapping is one-to-one, so a matched
prototype is dropped for the row it came from exactly as the clean one is.

Then the 203 acoustically-real overlap rows are scored again, open set, against
the matched bank -- the same "both recovered" definition as §29.1.

Usage: python sep_matched_bank.py <snapshot>.db <audio-root>   (build + score)
"""
import json
import os
import sqlite3
import struct
import sys

import numpy as np

D = os.environ["NXR_SEP"]
db_path, audio_root = sys.argv[1], sys.argv[2]
os.chdir(D)
import sep_lib as L  # noqa: E402

emb, bank = L.Embedder(), L.Bank("protos.json")
sep = L.SepFormer("sepformer_ckpt")

c = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
rows = list(c.execute(
    "select p.speaker_id, p.vector, p.source_segment_id, s.audio_path "
    "from speaker_prototypes p join segments s on s.id = p.source_segment_id "
    "where p.embed_model_id = 'eres2net_en@1' and s.audio_path is not null"))
print(f"prototypes with a source segment: {len(rows)}")

M, SRC, miss = {}, {}, 0
for i, (sid, blob, src, path) in enumerate(rows):
    wav = os.path.join(audio_root, path)
    if not os.path.isfile(wav):
        miss += 1
        continue
    ref = np.array(struct.unpack(f"<{len(blob)//4}f", blob), dtype=np.float32)
    ref /= np.linalg.norm(ref) + 1e-12
    x = L.decode(wav)
    if len(x) < 1600:
        miss += 1
        continue
    vs = [emb(s) for s in sep(x)]
    best = max(vs, key=lambda v: float(ref @ v))
    M.setdefault(sid, []).append(best)
    SRC.setdefault(sid, []).append(src)
    if (i + 1) % 40 == 0:
        print(f"  {i+1}/{len(rows)}", flush=True)
print(f"matched bank: {sum(len(v) for v in M.values())} prototypes over "
      f"{len(M)} voices ({miss} skipped)")

for sid in M:
    m = np.array(M[sid], dtype=np.float32)
    M[sid] = m / (np.linalg.norm(m, axis=1, keepdims=True) + 1e-12)
    SRC[sid] = np.array(SRC[sid])
json.dump({str(k): [v.tolist() for v in M[k]] for k in M},
          open("matched_bank.json", "w"))


def rank(v, exclude):
    out = []
    for sid, m in M.items():
        keep = SRC[sid] != exclude
        if not keep.any():
            continue
        out.append((sid, float((m[keep] @ v).max())))
    out.sort(key=lambda t: -t[1])
    return out


corpus = [r for r in json.load(open("corpus.json")) if r["pair"] == "Aspen+Rowan"]
out = []
for i, r in enumerate(corpus):
    seg = r["segment_id"]
    x = L.decode(r["wav"])
    true = [u["speaker_id"] for u in r["users"]]
    tops = [[[s, round(sc, 4)] for s, sc in rank(emb(st), seg)[:3]] for st in sep(x)]
    out.append({"segment_id": seg, "true_sids": true, "sep_top": tops,
                "mix_top": [[s, round(sc, 4)] for s, sc in rank(emb(x), seg)[:3]],
                "dur_s": r["dur_s"], "truth_overlap_frac": r["truth_overlap_frac"],
                "detector_overlap_frac": r["detector_overlap_frac"]})
    if (i + 1) % 40 == 0:
        print(f"score {i+1}/{len(corpus)}", flush=True)
json.dump(out, open("sep_matched_open.json", "w"))

# The 0.35 bar was calibrated in the clean space. In the matched space the
# equal-error point sits at ~0.54 (§29.4), so both are reported and the sweep
# is shown rather than one flattering number being picked.
print(f"\n{'bar':>6} {'sep both':>9} {'sep >=1':>8} {'steal':>7} {'mix top in pair':>16}")
for bar in (0.35, 0.45, 0.50, 0.54, 0.60, 0.65):
    both = one = steal = mixok = 0
    for r in out:
        true = set(r["true_sids"])
        got = {t[0][0] for t in r["sep_top"] if t[0][1] >= bar}
        if got == true:
            both += 1
        if got & true:
            one += 1
        steal += len(got - true)
        mt = r["mix_top"][0]
        if mt[1] >= bar and mt[0] in true:
            mixok += 1
    n = len(out)
    print(f"{bar:>6.2f} {100*both/n:>8.1f}% {100*one/n:>7.1f}% {steal:>7} "
          f"{100*mixok/n:>15.1f}%")
