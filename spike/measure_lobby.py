"""Measure a REAL lobby recording against the lab findings.

Usage:  measure_lobby.py <recording.(wav|flac|opus|mp4|...)>

The lab swept "dominance" because it was a knob we controlled. You cannot read
dominance off a real single-channel mix, and you do not need to: dominance was only
ever a proxy for the question that actually decides the design —

    what fraction of real lobby speech survives the overlap gate
    and yields a confident identity?

That is measurable with no ground truth at all, because every stage here is
unsupervised:

  1. pyannote segmentation-3.0  -> how much is speech, how much of it is overlapped
  2. contiguous single-speaker regions -> the only material worth embedding
  3. greedy online clustering   -> exactly the voicebank matching the daemon will do,
                                   so the score distribution it produces is the real
                                   one, not a lab analogue

The output is compared against the measured lab reference to say which regime the
lobby actually sits in.
"""

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).parent))
from harness import SR, Embedder  # noqa: E402

# Models ship next to this script; NXR_SCRATCH is only a fallback for the synthetic
# experiments, which also need the corpus.
_HERE = Path(__file__).parent
S = _HERE if (_HERE / "models").is_dir() else Path(os.environ.get("NXR_SCRATCH", "/tmp/nxr"))
SEG = S / "models" / "sherpa-onnx-pyannote-segmentation-3-0" / "model.onnx"
EMB = S / "models" / "eres2net_en.onnx"

WIN = 160_000          # pyannote's native 10 s window
MIN_SEG = 1.0          # shortest region worth embedding (lab: 1 s -> 96% coverage)
MATCH_THR = 0.45       # lab-calibrated; near the FAR=1% operating point

# Lab reference points from FINDINGS.md (eres2net, fixed clean-calibrated threshold).
LAB = [("clean / 1 talker", 0.00, 99.0), ("10 talkers @ +12 dB", 0.05, 74.5),
       ("2 talkers @ +6 dB", 0.58, 13.0), ("2 talkers @ +0 dB", 0.77, 2.0),
       ("3 talkers @ +0 dB", 0.84, 1.5)]


def decode(path: Path) -> np.ndarray:
    """Anything ffmpeg reads -> 16 kHz mono float32."""
    raw = subprocess.run(
        ["ffmpeg", "-hide_banner", "-loglevel", "error", "-i", str(path),
         "-f", "f32le", "-ar", str(SR), "-ac", "1", "pipe:1"],
        capture_output=True, check=True).stdout
    return np.frombuffer(raw, dtype="<f4").copy()


def segment(sess, x: np.ndarray):
    """Per-frame powerset classes over the whole file, in 10 s chunks."""
    out = []
    for i in range(0, len(x), WIN):
        chunk = x[i:i + WIN]
        if len(chunk) < WIN:
            chunk = np.pad(chunk, (0, WIN - len(chunk)))
        y = sess.run(None, {"x": chunk.reshape(1, 1, -1)})[0][0]
        out.append(y.argmax(axis=1))
    return np.concatenate(out)


def regions(cls: np.ndarray, hop: float, total: float):
    """Contiguous runs where exactly ONE speaker is active (powerset classes 1-3)."""
    runs, start, cur = [], None, 0
    for i, c in enumerate(cls):
        single = c in (1, 2, 3)
        if single and c == cur:
            continue
        if start is not None and cur in (1, 2, 3):
            t0, t1 = start * hop, i * hop
            if t1 - t0 >= MIN_SEG and t0 < total:
                runs.append((t0, min(t1, total)))
        start, cur = (i, c) if single else (None, 0)
    if start is not None and cur in (1, 2, 3):
        t0, t1 = start * hop, len(cls) * hop
        if t1 - t0 >= MIN_SEG and t0 < total:
            runs.append((t0, min(t1, total)))
    return runs


def main(path: Path):
    import onnxruntime as ort

    x = decode(path)
    dur = len(x) / SR
    print(f"\n{path.name}: {dur/60:.1f} min\n")

    so = ort.SessionOptions()
    so.intra_op_num_threads = 4
    sess = ort.InferenceSession(str(SEG), so, providers=["CPUExecutionProvider"])
    cls = segment(sess, x)
    hop = WIN / SR / (len(cls) / max(1, (len(x) + WIN - 1) // WIN))   # sec per frame

    n = len(cls)
    silent = int((cls == 0).sum())
    speech = n - silent
    overlap = int(np.isin(cls, (4, 5, 6)).sum())
    ov_frac = overlap / speech if speech else 0.0

    print("=" * 66)
    print("WHAT IS IN THE RECORDING")
    print("=" * 66)
    print(f"  speech            {speech/n*100:5.1f}%  of wall clock "
          f"({speech*hop/60:.1f} min)")
    print(f"  silence           {silent/n*100:5.1f}%")
    print(f"  OVERLAPPED        {ov_frac*100:5.1f}%  of speech   <- rejected by the gate")
    print(f"  single-speaker    {(1-ov_frac)*100:5.1f}%  of speech   <- labelable material")

    runs = regions(cls, hop, dur)
    usable = sum(b - a for a, b in runs)
    print(f"\n  contiguous single-speaker regions >= {MIN_SEG}s: {len(runs)}"
          f"  ({usable/60:.1f} min, mean {usable/max(1,len(runs)):.1f}s)")
    if not runs:
        print("\n  Nothing labelable. Either the recording is silent or everyone talks at once.")
        return

    emb = Embedder(EMB, num_threads=8)
    vecs = np.stack([emb(x[int(a * SR):int(b * SR)]) for a, b in runs])
    durs = np.array([b - a for a, b in runs])

    # Greedy online clustering — the same match-or-mint rule the daemon will run.
    protos: list[list[np.ndarray]] = []
    assign, scores = [], []
    for v in vecs:
        best, bi = -1.0, -1
        for i, ps in enumerate(protos):
            s = max(float(v @ p) for p in ps)
            if s > best:
                best, bi = s, i
        if best >= MATCH_THR:
            assign.append(bi)
            scores.append(best)
            if len(protos[bi]) < 20:
                protos[bi].append(v)
        else:
            assign.append(len(protos))
            scores.append(best)
            protos.append([v])
    assign, scores = np.array(assign), np.array(scores)

    sizes = np.bincount(assign)
    order = np.argsort(-sizes)
    conf = scores >= MATCH_THR
    conf_time = float(durs[conf].sum())

    print("\n" + "=" * 66)
    print("IDENTITY RECOVERY  (unsupervised, no enrollment)")
    print("=" * 66)
    print(f"  distinct voices found        {len(protos)}")
    print(f"  voices with >= 3 segments    {int((sizes >= 3).sum())}")
    print("  largest clusters (segments / minutes):")
    for i in order[:8]:
        m = float(durs[assign == i].sum())
        print(f"    voice {i:<3} {sizes[i]:4d} seg   {m/60:5.1f} min")

    print(f"\n  confident matches            {conf.mean()*100:5.1f}% of regions")
    print(f"  match score  mean {scores[conf].mean() if conf.any() else float('nan'):+.3f}"
          f"   (lab same-speaker reference: +0.76)")

    lab_frac = conf_time / (speech * hop) if speech else 0.0
    print("\n" + "=" * 66)
    print("THE HEADLINE")
    print("=" * 66)
    print(f"  {lab_frac*100:.1f}% of speech time is single-speaker AND confidently identified")
    print(f"  ({conf_time/60:.1f} min of {speech*hop/60:.1f} min of speech)")

    print(f"\n  Your lobby's overlap rate is {ov_frac*100:.0f}%. Lab reference:")
    for name, o, cov in LAB:
        mark = "  <-- closest" if min(LAB, key=lambda z: abs(z[1] - ov_frac))[0] == name else ""
        print(f"    {name:<22} overlap {o*100:4.0f}%   gated coverage {cov:5.1f}%{mark}")
    print("\n  Above ~60% overlap the design cannot label reliably and should say so")
    print("  in the UI rather than guess. Below ~20% it works well.")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print(__doc__)
        raise SystemExit(2)
    main(Path(sys.argv[1]))
