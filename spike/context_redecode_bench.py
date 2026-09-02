"""Does decoding a short turn WITH its surrounding audio beat decoding it alone?

§11 of FINDINGS measured the cliff: on identical audio, 1.5 s slices score
~85-90% "WER" against the full sentence while the whole utterance scores 5%.
Every model, same cliff. This is the gate for the fix the cliff implies —
re-decode the short turn inside a ±`PAD` s window of the same session's audio
and keep only the words whose token timestamps land inside the turn.

Method, stated plainly because the reference is the fragile part:

  1. Decode the FULL FLEURS utterance (Opus 24k, as VRChat would deliver it)
     with parakeet v3, keeping token timestamps. That decode is used ONLY to
     find word boundaries and to locate the turn — never as the reference.
  2. Cut a "turn" of L seconds at a word boundary. VAD cuts at silence, so a
     word boundary is the honest analogue of a turn edge.
  3. The reference for that turn is the FLEURS ground truth restricted to the
     turn: align the full decode's words to the reference words with the same
     Levenshtein alignment the WER uses, and take the reference words that
     align into the turn's word range. So the reference is human text; only
     the SPAN comes from the timestamps.
  4. Compare two hypotheses for that turn:
       alone  — decode the L-second slice on its own (what the daemon does now)
       window — decode [t0-PAD, t1+PAD] and keep the tokens whose timestamps
                fall inside [t0, t1] (what the feature would do)

Bias to declare: the window is a superset of the slice and a subset of the
utterance the span was measured on, so the window decode's timestamps agree
with the full decode's by construction more often than a stranger's would.
The reference words do not come from either decode, which is what keeps the
comparison fair; the span boundaries do.

GATE: ship only if the window decode's WER is at least 25% relatively lower
than the alone decode's, on at least one of L = 1.5 s and L = 2.5 s.
"""

from __future__ import annotations

import os
import random
import sys
import time
from concurrent.futures import ProcessPoolExecutor
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).parent))
from asr_multilang import load_wav_16k, norm_de, norm_en  # noqa: E402
from asr_spike import wer_counts  # noqa: E402
from harness import SR, extract_corpus, index_corpus, opus_roundtrip  # noqa: E402

S = Path(os.environ.get("NXR_SCRATCH", "/tmp/nxr"))
M = S / "models"
V3 = M / "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8"
PAD = float(os.environ.get("PAD_S", "3.0"))
LENGTHS = (1.5, 2.5)
N_UTTS = int(os.environ.get("N_UTTS", "60"))
CORES = set(range(16, 32))

_G: dict = {}


# ----------------------------------------------------------------- decoding

def make_v3(threads: int = 1):
    import sherpa_onnx as so

    return so.OfflineRecognizer.from_transducer(
        encoder=str(V3 / "encoder.int8.onnx"), decoder=str(V3 / "decoder.int8.onnx"),
        joiner=str(V3 / "joiner.int8.onnx"), tokens=str(V3 / "tokens.txt"),
        num_threads=threads, model_type="nemo_transducer")


def decode(x: np.ndarray) -> tuple[str, list[str], list[float]]:
    rec = _G["rec"]
    st = rec.create_stream()
    st.accept_waveform(SR, np.ascontiguousarray(x, dtype=np.float32))
    rec.decode_stream(st)
    r = st.result
    return r.text.strip(), list(r.tokens), [float(t) for t in r.timestamps]


def words_with_times(tokens: list[str], stamps: list[float]) -> list[tuple[str, float]]:
    """Group BPE pieces into words. A piece that opens a word carries the
    word-start marker — sherpa renders it as a leading space, some exports as
    U+2581. The word's time is its first piece's timestamp."""
    out: list[tuple[str, float]] = []
    for tok, ts in zip(tokens, stamps):
        opens = tok.startswith(" ") or tok.startswith("▁")
        piece = tok.lstrip(" ▁")
        if opens or not out:
            if piece:
                out.append((piece, ts))
        else:
            w, t = out[-1]
            out[-1] = (w + piece, t)
    return [(w, t) for w, t in out if w.strip()]


def tokens_in_span(tokens: list[str], stamps: list[float], t0: float, t1: float) -> str:
    """The mechanism the daemon implements: keep the WORDS whose first token's
    timestamp lands inside the turn.

    Word-level, not token-level, on purpose: a timestamp is the start of a
    piece, so cutting on pieces truncates the last word of the span to a stump
    ("Kü", "sp"). A word belongs to the turn if it *starts* in the turn."""
    return " ".join(w for w, ts in words_with_times(tokens, stamps) if t0 <= ts < t1)


# ----------------------------------------------------------------- reference

def span_reference(dec_words: list[str], ref_words: list[str], i0: int, i1: int) -> list[str]:
    """Reference words that align to decoded words [i0, i1).

    Standard Levenshtein backtrace between the full decode and the ground
    truth; the span's reference is ref[lo:hi] where lo/hi are the extreme
    reference positions that align into the span (deletions inside the span
    come along, which is what we want — a word the decoder dropped is still
    part of the turn)."""
    R, H = len(ref_words), len(dec_words)
    dp = np.zeros((R + 1, H + 1), dtype=np.int32)
    dp[:, 0] = np.arange(R + 1)
    dp[0, :] = np.arange(H + 1)
    for i in range(1, R + 1):
        for j in range(1, H + 1):
            cost = 0 if ref_words[i - 1] == dec_words[j - 1] else 1
            dp[i, j] = min(dp[i - 1, j - 1] + cost, dp[i - 1, j] + 1, dp[i, j - 1] + 1)
    i, j = R, H
    lo, hi = None, None
    while i > 0 and j > 0:
        cost = 0 if ref_words[i - 1] == dec_words[j - 1] else 1
        if dp[i, j] == dp[i - 1, j - 1] + cost:
            if i0 <= j - 1 < i1:
                lo = i - 1 if lo is None else min(lo, i - 1)
                hi = i if hi is None else max(hi, i)
            i, j = i - 1, j - 1
        elif dp[i, j] == dp[i - 1, j] + 1:
            i -= 1
        else:
            j -= 1
    if lo is None:
        return []
    return ref_words[lo:hi]


def wer(ref: list[str], hyp: list[str]) -> float:
    sd, ins, n = wer_counts(ref, hyp)
    return (sd + ins) / max(1, n)


# ----------------------------------------------------------------- one item

def _init(jobs):
    try:
        os.nice(19 - os.nice(0))
        os.sched_setaffinity(0, CORES)
    except OSError:
        pass
    _G["rec"] = make_v3(1)
    _G["jobs"] = jobs


def _run(i: int):
    path, ref_text, lang = _G["jobs"][i]
    rng = random.Random(hash(path) & 0xFFFF)
    norm = norm_de if lang == "de" else norm_en
    x = opus_roundtrip(load_wav_16k(Path(path)), 24)
    dur = len(x) / SR
    if dur < 2 * PAD:
        return None
    _, tokens, stamps = decode(x)
    words = words_with_times(tokens, stamps)
    if len(words) < 8:
        return None
    ref_words = norm(ref_text)
    dec_words = [w for w, _ in words]
    dec_norm = [norm(w)[0] if norm(w) else w.lower() for w in dec_words]

    out = []
    for L in LENGTHS:
        # A turn that starts at a word boundary and is L seconds long, placed
        # away from the utterance edges so that a ±PAD window exists on both
        # sides for at least one of the picks.
        cands = [k for k, (_, t) in enumerate(words) if t >= 0.3 and t + L <= dur - 0.3]
        if not cands:
            continue
        k0 = rng.choice(cands)
        t0 = words[k0][1]
        t1 = t0 + L
        inside = [k for k, (_, t) in enumerate(words) if t0 <= t < t1]
        if len(inside) < 2:
            continue
        i0, i1 = inside[0], inside[-1] + 1
        ref_span = span_reference(dec_norm, ref_words, i0, i1)
        if len(ref_span) < 2:
            continue

        a0, a1 = int(t0 * SR), int(min(dur, t1) * SR)
        alone_text, _, _ = decode(x[a0:a1])

        w0 = max(0.0, t0 - PAD)
        w1 = min(dur, t1 + PAD)
        _, wt, ws = decode(x[int(w0 * SR):int(w1 * SR)])
        win_text = tokens_in_span(wt, ws, t0 - w0, t1 - w0)

        out.append(dict(
            L=L, lang=lang, n_ref=len(ref_span),
            wer_alone=wer(ref_span, norm(alone_text)),
            wer_window=wer(ref_span, norm(win_text)),
            ref=" ".join(ref_span), alone=alone_text, window=win_text))
    return out


# ----------------------------------------------------------------- main

def fleurs(n: int) -> list[tuple[str, str, str]]:
    rows = []
    for line in (S / "de" / "dev.tsv").read_text().splitlines():
        p = line.split("\t")
        if len(p) >= 3 and (S / "de" / "dev" / p[1]).is_file():
            rows.append((str(S / "de" / "dev" / p[1]), p[2], "de"))
    return random.Random(11).sample(rows, min(n, len(rows)))


def libri(n: int) -> list[tuple[str, str, str]]:
    root = extract_corpus(S / "corpus" / "dev-clean.tar.gz", S / "corpus")
    trans = {}
    for f in root.rglob("*.trans.txt"):
        for line in f.read_text().splitlines():
            uid, _, text = line.partition(" ")
            trans[uid] = text
    by = index_corpus(root, 5)
    rows = [(str(u.path), trans[u.path.stem], "en") for s in sorted(by) for u in by[s][3:4]]
    return rows[:n]


def main() -> int:
    jobs = fleurs(N_UTTS) + libri(N_UTTS // 2)
    print(f"context re-decode gate · {len(jobs)} utterances · pad ±{PAD}s · Opus 24k\n")
    t0 = time.time()
    with ProcessPoolExecutor(max_workers=12, initializer=_init, initargs=(jobs,)) as ex:
        got = [r for r in ex.map(_run, range(len(jobs)), chunksize=2) if r]
    rows = [item for sub in got for item in sub]
    print(f"{len(rows)} turns cut ({time.time()-t0:.0f}s wall)\n")

    print(f"  {'set':>10} {'n':>4} {'WER alone':>10} {'WER window':>11} {'rel. gain':>10}")
    verdict = {}
    for lang in ("de", "en", None):
        for L in LENGTHS:
            rs = [r for r in rows if r["L"] == L and (lang is None or r["lang"] == lang)]
            if not rs:
                continue
            a = float(np.mean([r["wer_alone"] for r in rs]))
            w = float(np.mean([r["wer_window"] for r in rs]))
            gain = (a - w) / a if a else 0.0
            tag = f"{lang or 'both'} {L}s"
            print(f"  {tag:>10} {len(rs):4d} {a*100:9.1f}% {w*100:10.1f}% {gain*100:9.1f}%")
            if lang is None:
                verdict[L] = gain

    print()
    ok = [L for L, g in verdict.items() if g >= 0.25]
    print(f"GATE (≥25% relative on ≥1 length): {'PASS' if ok else 'FAIL'} "
          f"— {', '.join(f'{L}s {verdict[L]*100:.1f}%' for L in sorted(verdict))}")

    print("\nthree examples:")
    for r in rows[:3]:
        print(f"  ref    : {r['ref']}")
        print(f"  alone  : {r['alone']}")
        print(f"  window : {r['window']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
