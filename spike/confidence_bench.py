"""Does a second decoder's agreement predict whether the first one was right?

§11 measured that any two ASR models agree about half the time on real lobby
audio, and read that as a difficulty signal rather than a model-choice signal.
This script asks the follow-up question the contract needs: if canary-180m
(RTF 0.33, cheap enough for the idle worker) agrees with parakeet v3, is v3
actually more likely to be correct?

Set: FLEURS German + LibriSpeech English through Opus 24k, full utterances AND
short spans cut the same way `context_redecode_bench.py` cuts them — the short
ones are where the interesting errors live, and a calibration set of only easy
full sentences would put τ in the wrong place.

For each item: v3's WER against the reference, and the normalised word
agreement between v3 and canary (1 − word edit distance, floored at 0). Then
v3's WER conditioned on agreement ≥ τ ("solid") versus < τ ("shaky").

Also measured: the unknown-language path. The daemon may not know a turn's
language, and canary demands a src_lang. Running canary twice (de and en) and
keeping the higher agreement is the fallback the contract allows; the table
reports how much predictive power that costs against knowing the language.

GATE: ship only if the shaky bucket's WER is at least 3× the solid bucket's at
the chosen τ.
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
from context_redecode_bench import (  # noqa: E402
    LENGTHS, fleurs, libri, make_v3, span_reference, wer, words_with_times,
)
from harness import SR, opus_roundtrip  # noqa: E402

S = Path(os.environ.get("NXR_SCRATCH", "/tmp/nxr"))
CANARY = S / "models" / "sherpa-onnx-nemo-canary-180m-flash-en-es-de-fr-int8"
N_UTTS = int(os.environ.get("N_UTTS", "50"))
TAUS = (0.4, 0.5, 0.6, 0.7, 0.8)
CORES = set(range(16, 32))

_G: dict = {}


def make_canary(lang: str, threads: int = 1):
    import sherpa_onnx as so

    return so.OfflineRecognizer.from_nemo_canary(
        encoder=str(CANARY / "encoder.int8.onnx"), decoder=str(CANARY / "decoder.int8.onnx"),
        tokens=str(CANARY / "tokens.txt"), src_lang=lang, tgt_lang=lang, num_threads=threads)


def _init(jobs):
    try:
        os.nice(19 - os.nice(0))
        os.sched_setaffinity(0, CORES)
    except OSError:
        pass
    _G["v3"] = make_v3(1)
    _G["canary"] = {lang: make_canary(lang) for lang in ("de", "en")}
    _G["jobs"] = jobs


def _decode(rec, x) -> tuple[str, float]:
    st = rec.create_stream()
    st.accept_waveform(SR, np.ascontiguousarray(x, dtype=np.float32))
    t0 = time.time()
    rec.decode_stream(st)
    return st.result.text.strip(), time.time() - t0


def agreement(a: list[str], b: list[str]) -> float:
    """1 − normalised word edit distance, the same score the daemon computes."""
    if not a and not b:
        return 1.0
    sd, ins, n = wer_counts(a, b)
    return max(0.0, 1.0 - (sd + ins) / max(1, max(n, len(b))))


def _run(i: int):
    path, ref_text, lang = _G["jobs"][i]
    norm = norm_de if lang == "de" else norm_en
    rng = random.Random(hash(path) & 0xFFFF)
    x = opus_roundtrip(load_wav_16k(Path(path)), 24)
    dur = len(x) / SR
    ref_words = norm(ref_text)

    st = _G["v3"].create_stream()
    st.accept_waveform(SR, np.ascontiguousarray(x, dtype=np.float32))
    _G["v3"].decode_stream(st)
    full = st.result
    words = words_with_times(list(full.tokens), [float(t) for t in full.timestamps])
    dec_norm = [norm(w)[0] if norm(w) else w.lower() for w in (w for w, _ in words)]

    items = [(0.0, dur, ref_words, "full")]
    for L in LENGTHS:
        cands = [k for k, (_, t) in enumerate(words) if t >= 0.3 and t + L <= dur - 0.3]
        if not cands:
            continue
        k0 = rng.choice(cands)
        t0 = words[k0][1]
        t1 = t0 + L
        inside = [k for k, (_, t) in enumerate(words) if t0 <= t < t1]
        if len(inside) < 2:
            continue
        span = span_reference(dec_norm, ref_words, inside[0], inside[-1] + 1)
        if len(span) < 2:
            continue
        items.append((t0, t1, span, f"{L}s"))

    out = []
    for t0, t1, ref_span, tag in items:
        seg = x[int(t0 * SR):int(t1 * SR)]
        v3_text, _ = _decode(_G["v3"], seg)
        can_known, dt = _decode(_G["canary"][lang], seg)
        other = "en" if lang == "de" else "de"
        can_other, dt2 = _decode(_G["canary"][other], seg)
        v3w = norm(v3_text)
        a_known = agreement(v3w, norm(can_known))
        a_blind = max(a_known, agreement(v3w, norm(can_other)))
        out.append(dict(
            lang=lang, tag=tag, dur=(t1 - t0),
            wer_v3=wer(ref_span, v3w), wer_canary=wer(ref_span, norm(can_known)),
            agree=a_known, agree_blind=a_blind, canary_empty=not can_known,
            rtf=(dt + dt2) / max(1e-6, (t1 - t0)) / 2))
    return out


def table(rows, key: str, label: str) -> dict[float, tuple]:
    print(f"\n  {label}")
    print(f"    {'τ':>5} {'n solid':>8} {'WER solid':>10} {'n shaky':>8} {'WER shaky':>10} {'ratio':>7}")
    got = {}
    for tau in TAUS:
        solid = [r for r in rows if r[key] >= tau]
        shaky = [r for r in rows if r[key] < tau]
        if not solid or not shaky:
            continue
        ws = float(np.mean([r["wer_v3"] for r in solid]))
        wk = float(np.mean([r["wer_v3"] for r in shaky]))
        ratio = wk / ws if ws > 1e-6 else float("inf")
        got[tau] = (ws, wk, ratio, len(solid), len(shaky))
        print(f"    {tau:5.2f} {len(solid):8d} {ws*100:9.1f}% {len(shaky):8d} "
              f"{wk*100:9.1f}% {ratio:6.1f}×")
    return got


def main() -> int:
    if not CANARY.is_dir():
        print(f"canary missing at {CANARY}")
        return 1
    jobs = fleurs(N_UTTS) + libri(N_UTTS)
    print(f"confidence gate · {len(jobs)} utterances → full + {len(LENGTHS)} spans each · Opus 24k\n")
    t0 = time.time()
    with ProcessPoolExecutor(max_workers=12, initializer=_init, initargs=(jobs,)) as ex:
        rows = [r for sub in ex.map(_run, range(len(jobs)), chunksize=2) for r in sub]
    print(f"{len(rows)} items ({time.time()-t0:.0f}s wall) · canary empty on "
          f"{sum(r['canary_empty'] for r in rows) / len(rows) * 100:.0f}% · "
          f"canary RTF {np.mean([r['rtf'] for r in rows]):.2f}")
    print(f"overall: v3 WER {np.mean([r['wer_v3'] for r in rows])*100:.1f}% · "
          f"canary WER {np.mean([r['wer_canary'] for r in rows])*100:.1f}% · "
          f"mean agreement {np.mean([r['agree'] for r in rows])*100:.0f}%")

    known = table(rows, "agree", "language known (canary fed the segment's lang)")
    blind = table(rows, "agree_blind", "language unknown (both de and en, higher agreement kept)")

    for tag in ("full", *(f"{L}s" for L in LENGTHS)):
        sub = [r for r in rows if r["tag"] == tag]
        if sub:
            table(sub, "agree", f"language known · {tag} only")

    best = max((v[2], k) for k, v in known.items()) if known else (0, 0)
    print(f"\nGATE (≥3× WER ratio shaky/solid): {'PASS' if best[0] >= 3 else 'FAIL'} "
          f"— best {best[0]:.1f}× at τ={best[1]:.2f}"
          + (f" · blind {blind.get(best[1], (0, 0, 0))[2]:.1f}×" if blind else ""))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
