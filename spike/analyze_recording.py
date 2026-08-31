"""Follow-up analysis on a real recording: what should the daemon's parameters be?

measure_lobby.py reported 62 clusters for what is certainly far fewer real people,
and a mean single-speaker region of only 2.4 s. Both are tunable, and both change the
headline number. This answers three questions from the same audio:

  1. Is the over-splitting threshold-driven, or is the audio just too fragmented?
  2. Does merging adjacent regions from the same voice buy back segment length?
  3. What happens once a few people are ENROLLED? The 53.9% headline was fully
     unsupervised; the product will have named prototypes, which is a much easier
     problem and the one users actually experience.

Aggregate statistics only — no transcription, no audio export, nothing that
identifies anyone.
"""

from __future__ import annotations

import sys
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).parent))
from harness import SR, Embedder  # noqa: E402
from measure_lobby import EMB, SEG, decode, regions, segment  # noqa: E402

THRS = [0.30, 0.35, 0.40, 0.45, 0.50, 0.55]
MERGE_GAP = 1.5     # seconds of silence still counted as the same turn


def cluster(vecs, thr, cap=20):
    protos: list[list[np.ndarray]] = []
    assign, scores = [], []
    for v in vecs:
        best, bi = -1.0, -1
        for i, ps in enumerate(protos):
            s = max(float(v @ p) for p in ps)
            if s > best:
                best, bi = s, i
        if best >= thr:
            assign.append(bi)
            scores.append(best)
            if len(protos[bi]) < cap:
                protos[bi].append(v)
        else:
            assign.append(len(protos))
            scores.append(best)
            protos.append([v])
    return np.array(assign), np.array(scores), protos


def main(path: Path):
    import onnxruntime as ort

    x = decode(path)
    so = ort.SessionOptions()
    so.intra_op_num_threads = 8
    cls = segment(ort.InferenceSession(str(SEG), so, providers=["CPUExecutionProvider"]), x)
    n_chunks = (len(x) + 160_000 - 1) // 160_000
    hop = 160_000 / SR / (len(cls) / n_chunks)
    runs = regions(cls, hop, len(x) / SR)

    emb = Embedder(EMB, num_threads=8)
    vecs = np.stack([emb(x[int(a * SR):int(b * SR)]) for a, b in runs])
    durs = np.array([b - a for a, b in runs])
    print(f"\n{path.name}: {len(runs)} regions, {durs.sum()/60:.1f} min of "
          f"single-speaker speech\n")

    print("=" * 70)
    print("1. IS THE OVER-SPLITTING THRESHOLD-DRIVEN?")
    print("=" * 70)
    print(f"  {'thr':>5} {'voices':>7} {'>=3seg':>7} {'matched':>8} {'time in top-8':>14}")
    for t in THRS:
        a, s, _ = cluster(vecs, t)
        sz = np.bincount(a)
        top = np.argsort(-sz)[:8]
        share = durs[np.isin(a, top)].sum() / durs.sum()
        print(f"  {t:>5.2f} {len(sz):>7} {int((sz>=3).sum()):>7} "
              f"{(s>=t).mean()*100:>7.1f}% {share*100:>13.1f}%")

    print("\n" + "=" * 70)
    print("2. DOES SEGMENT LENGTH EXPLAIN THE SINGLETONS?")
    print("=" * 70)
    a, s, _ = cluster(vecs, 0.45)
    sz = np.bincount(a)
    lone = sz[a] == 1
    print(f"  singleton regions : {lone.sum():3d}  mean {durs[lone].mean():.1f}s")
    print(f"  clustered regions : {(~lone).sum():3d}  mean {durs[~lone].mean():.1f}s")
    for lo, hi in [(1, 1.5), (1.5, 2.5), (2.5, 4), (4, 8), (8, 99)]:
        m = (durs >= lo) & (durs < hi)
        if m.sum():
            print(f"    {lo:>4.1f}-{hi:<4.1f}s  n={m.sum():3d}  "
                  f"singleton {lone[m].mean()*100:5.1f}%  mean score {s[m].mean():+.3f}")

    # merge adjacent regions separated by only a short gap
    merged, cur = [], list(runs[0])
    for (a0, b0) in runs[1:]:
        if a0 - cur[1] <= MERGE_GAP:
            cur[1] = b0
        else:
            merged.append(tuple(cur))
            cur = [a0, b0]
    merged.append(tuple(cur))
    print(f"\n  merging turns separated by <= {MERGE_GAP}s of silence:")
    md = np.array([b - a for a, b in merged])
    print(f"    {len(runs)} regions (mean {durs.mean():.1f}s) -> "
          f"{len(merged)} (mean {md.mean():.1f}s)")
    mv = np.stack([emb(x[int(a * SR):int(b * SR)]) for a, b in merged])
    am, sm, _ = cluster(mv, 0.45)
    szm = np.bincount(am)
    print(f"    voices {len(sz)} -> {len(szm)},  matched "
          f"{(s>=0.45).mean()*100:.1f}% -> {(sm>=0.45).mean()*100:.1f}%")

    print("\n" + "=" * 70)
    print("3. WHAT IF A FEW PEOPLE ARE ENROLLED?")
    print("=" * 70)
    print("  (bank built from each top voice's longest regions; measured on the rest)")
    a, s, _ = cluster(vecs, 0.45)
    sz = np.bincount(a)
    order = np.argsort(-sz)
    for n_people in (2, 4, 6, 8):
        bank, held = [], []
        for cid in order[:n_people]:
            mem = np.nonzero(a == cid)[0]
            if len(mem) < 3:
                continue
            best = mem[np.argsort(-durs[mem])][:3]     # 3 longest = enrollment
            bank.append(vecs[best])
            held += [i for i in mem if i not in set(best)]
        if not bank or not held:
            continue
        B = np.stack([b.mean(axis=0) / np.linalg.norm(b.mean(axis=0)) for b in bank])
        Bm = [b for b in bank]
        hit = 0
        for i in held:
            sc = [max(float(vecs[i] @ p) for p in ps) for ps in Bm]
            hit += max(sc) >= 0.45
        cov = durs[held].sum() / durs.sum()
        print(f"  {len(bank)} enrolled -> {hit/len(held)*100:5.1f}% of their remaining "
              f"speech matched  ({cov*100:.0f}% of all speech is theirs)")


if __name__ == "__main__":
    main(Path(sys.argv[1]))
