"""Q3 -- do the separated streams read as two people, or as one person twice?

No word truth exists for these turns, so this measures three **proxies** and
says so. On a subsample of the most-overlapped rows we transcribe the mixture
and each separated stream with the night shift's whisper large-v3
(`whisper-cli`, the same binary and ggml the daemon runs), and compute:

  disjointness   1 - (word agreement between stream A and stream B). The
                 classic separation failure is both streams carrying the same
                 dominant talker; that failure reads as agreement near 1, so
                 disjointness near 0. This is the proxy that actually
                 discriminates, and it needs no reference text.
  recovery       share of the mixture's words that appear in A or B. Separation
                 that throws away a talker shows up here.
  coherence      cross-decoder agreement (whisper vs parakeet) on each stream,
                 which is exactly the daemon's `asr_confidence`
                 (`canary::agreement`, 1 - word edit distance / max length).
                 Higher on clean single-speaker audio; reported for mixture and
                 for the streams so the *change* is the reading, not the level.

None of the three is word accuracy. They are stated here as proxies because the
honest alternative -- hand-transcribing 200 overlapped turns -- was not done.

Usage: NXR_SEP=<dir> python sep_asr.py [--n 60] [--gpu]
"""
import argparse
import json
import os
import re
import subprocess
import tempfile
from pathlib import Path

import numpy as np

D = os.environ["NXR_SEP"]
os.chdir(D)
import sep_lib as L  # noqa: E402

MODELS = Path.home() / ".local/share/nx-recall/models"
WHISPER = MODELS / "whisper" / "whisper-cli"
GGML = MODELS / "ggml-large-v3-q5_0.bin"
PARAKEET = MODELS / "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8"

ap = argparse.ArgumentParser()
ap.add_argument("--n", type=int, default=60)
ap.add_argument("--gpu", action="store_true")
ap.add_argument("--out", default="sep_asr.json")
a = ap.parse_args()


def norm(s):
    return [w for w in re.sub(r"[^\w\s']", " ", (s or "").lower()).split() if w]


def agreement(x, y):
    """canary::agreement -- 1 - word edit distance / max length."""
    a_, b_ = norm(x), norm(y)
    if not a_ and not b_:
        return 1.0
    denom = max(len(a_), len(b_))
    if denom == 0:
        return 1.0
    prev = list(range(len(b_) + 1))
    for i, wa in enumerate(a_):
        cur = [i + 1] + [0] * len(b_)
        for j, wb in enumerate(b_):
            cur[j + 1] = min(prev[j] + (wa != wb), prev[j + 1] + 1, cur[j] + 1)
        prev = cur
    return max(0.0, 1.0 - prev[len(b_)] / denom)


def bag_recall(ref, hyp_words):
    """share of ref's words present in the hypothesis bag (multiset)."""
    r = norm(ref)
    if not r:
        return None
    pool = list(hyp_words)
    hit = 0
    for w in r:
        if w in pool:
            pool.remove(w)
            hit += 1
    return hit / len(r)


# Whisper's empty-audio signature. On a stream that separation left almost
# silent, large-v3 emits one of these instead of nothing, and two *different*
# hallucinations look perfectly "disjoint" -- which is why disjointness alone
# cannot be read as evidence that separation worked.
HALLUCINATIONS = {
    "thank you", "thanks for listening", "thanks for watching", "bye",
    "you", "thank you very much", "thank you for watching", "so",
    "please subscribe", "vielen dank", "untertitel von stephanie geiges",
    "untertitelung des zdf für funk", "amara org community",
}


def is_hallucination(text):
    w = " ".join(norm(text))
    return w in HALLUCINATIONS or (len(norm(text)) <= 2 and w in HALLUCINATIONS)


def whisper(wav):
    # This install is bilingual (§12, §23): forcing English mistranscribes the
    # German half and makes every agreement number a measurement of the flag.
    cmd = [str(WHISPER), "-m", str(GGML), "-f", str(wav), "-l", "auto", "-nt",
           "-t", "4", "--no-prints"]
    if not a.gpu:
        cmd += ["-ng"]
    r = subprocess.run(cmd, capture_output=True, text=True)
    return " ".join(r.stdout.split())


_pk = None


def parakeet(x):
    global _pk
    import sherpa_onnx
    if _pk is None:
        _pk = sherpa_onnx.OfflineRecognizer.from_transducer(
            encoder=str(PARAKEET / "encoder.int8.onnx"),
            decoder=str(PARAKEET / "decoder.int8.onnx"),
            joiner=str(PARAKEET / "joiner.int8.onnx"),
            tokens=str(PARAKEET / "tokens.txt"),
            num_threads=4, model_type="nemo_transducer")
    s = _pk.create_stream()
    s.accept_waveform(16000, np.ascontiguousarray(x, dtype=np.float32))
    _pk.decode_stream(s)
    return s.result.text.strip()


rows = json.load(open("sep_acoustic.json"))
rows = [r for r in rows if r.get("truth_overlap_frac") is not None and r["dur_s"] >= 1.5]
rows.sort(key=lambda r: -r["truth_overlap_frac"])
rows = rows[:a.n]
corpus = {r["segment_id"]: r for r in json.load(open("corpus.json"))}
sep = L.SepFormer("sepformer_ckpt")

import soundfile as sf  # noqa: E402

out = []
tmp = Path(tempfile.mkdtemp())
for i, r in enumerate(rows):
    src = corpus[r["segment_id"]]
    x = L.decode(src["wav"])
    sa, sb = sep(x)
    rec = {"segment_id": r["segment_id"], "truth_overlap_frac": r["truth_overlap_frac"],
           "dur_s": r["dur_s"]}
    for tag, sig in (("mix", x), ("a", sa), ("b", sb)):
        p = tmp / f"{tag}.wav"
        sf.write(p, sig / max(1e-6, np.abs(sig).max()) * 0.95, 16000)
        rec[f"w_{tag}"] = whisper(p)
        rec[f"p_{tag}"] = parakeet(sig)
        rec[f"conf_{tag}"] = round(agreement(rec[f"w_{tag}"], rec[f"p_{tag}"]), 4)
    rec["disjoint"] = round(1.0 - agreement(rec["w_a"], rec["w_b"]), 4)
    rec["recovery"] = bag_recall(rec["w_mix"], norm(rec["w_a"]) + norm(rec["w_b"]))
    rec["halluc"] = [is_hallucination(rec["w_a"]), is_hallucination(rec["w_b"])]
    rec["halluc_mix"] = is_hallucination(rec["w_mix"])
    out.append(rec)
    print(f"{i+1}/{len(rows)} disj={rec['disjoint']:.2f} "
          f"conf mix={rec['conf_mix']:.2f} a={rec['conf_a']:.2f} b={rec['conf_b']:.2f}",
          flush=True)

json.dump(out, open(a.out, "w"), indent=1)


def m(key):
    v = [r[key] for r in out if r.get(key) is not None]
    return round(float(np.mean(v)), 4) if v else None


print("\n--- proxies, n =", len(out), "---")
print("disjointness A vs B (1 = no shared words):", m("disjoint"))
print("mixture words recovered by A+B          :", m("recovery"))
print("cross-decoder agreement  mixture        :", m("conf_mix"))
print("cross-decoder agreement  stream A / B   :", m("conf_a"), "/", m("conf_b"))
nh = sum(sum(r["halluc"]) for r in out)
print(f"streams whose whole transcript is a whisper empty-audio phrase: "
      f"{nh}/{2*len(out)} = {100*nh/(2*len(out)):.1f}%  "
      f"(mixtures: {sum(r['halluc_mix'] for r in out)}/{len(out)})")
clean = [r for r in out if not any(r["halluc"])]
if clean:
    print(f"disjointness over the {len(clean)} rows where neither stream "
          f"hallucinated: {np.mean([r['disjoint'] for r in clean]):.4f}")
print("mean words  mix / a / b                 :",
      round(np.mean([len(norm(r["w_mix"])) for r in out]), 1),
      round(np.mean([len(norm(r["w_a"])) for r in out]), 1),
      round(np.mean([len(norm(r["w_b"])) for r in out]), 1))
