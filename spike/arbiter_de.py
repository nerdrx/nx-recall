"""Is Whisper-base forced to German good enough as a FLIP ARBITER?

The user's insight: conversational context is a language prior — in a German
thread, a fragment v3 decoded as English is probably a flip. The English side
already has a hard-constrained re-decoder (the EN-only 110m); German has none,
because no German-only Parakeet exists. Whisper's language token is the one
honest way to force German.

The arbiter's job is NOT to beat v3 on German — it is to beat v3's FLIPS,
which are 103%-WER garbage. Bar to clear: on short German fragments,
whisper(language=de) must (a) produce text the classifier reads as German,
(b) at materially better WER than the flipped nonsense it replaces.

Same fragments protocol as lang_flip.py (the windows where v3 flips 12%).
"""

from __future__ import annotations

import os
import sys
from concurrent.futures import ProcessPoolExecutor
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).parent))
from asr_multilang import load_wav_16k, norm_de  # noqa: E402
from asr_spike import wer_counts  # noqa: E402
from harness import SR, opus_roundtrip  # noqa: E402
from lang_flip import classify  # noqa: E402

S = Path(os.environ.get("NXR_SCRATCH", "/tmp/nxr"))
WB = S / "models" / "sherpa-onnx-whisper-base"
WINDOWS = [1.0, 1.5, 2.0, 3.0]
N_PER = 50

_G: dict = {}


def _init(jobs):
    try:
        os.nice(19 - os.nice(0))
    except OSError:
        pass
    import sherpa_onnx as so

    _G["rec"] = so.OfflineRecognizer.from_whisper(
        encoder=str(WB / "base-encoder.int8.onnx"),
        decoder=str(WB / "base-decoder.int8.onnx"),
        tokens=str(WB / "base-tokens.txt"),
        language="de",
        task="transcribe",
        num_threads=1,
    )
    _G["jobs"] = jobs


def _run(i):
    path, off, dur, ref = _G["jobs"][i]
    x = load_wav_16k(Path(path))
    raw = x[int(off * SR):int((off + dur) * SR)]
    if len(raw) < SR // 2:
        return None
    seg = opus_roundtrip(raw, 24)
    st = _G["rec"].create_stream()
    st.accept_waveform(SR, seg.astype(np.float32))
    _G["rec"].decode_stream(st)
    txt = st.result.text
    # Fragment WER vs the FULL reference is meaningless; instead measure
    # whether the words produced EXIST in the reference (precision proxy):
    hyp = norm_de(txt)
    refw = set(norm_de(ref))
    inref = sum(1 for w in hyp if w in refw)
    return {"dur": dur, "cls": classify(txt), "n": len(hyp),
            "prec": (inref / len(hyp)) if hyp else None}


def main() -> int:
    rows = []
    for line in (S / "de" / "dev.tsv").read_text().splitlines():
        p = line.split("\t")
        if len(p) >= 3 and p[1].endswith(".wav") and (S / "de" / "dev" / p[1]).is_file():
            rows.append((S / "de" / "dev" / p[1], p[2]))
    rng = np.random.default_rng(11)
    jobs = []
    for dur in WINDOWS:
        for i in rng.choice(len(rows), N_PER, replace=False):
            off = float(rng.uniform(0.5, 10.0))
            jobs.append((str(rows[i][0]), off, dur, rows[i][1]))

    with ProcessPoolExecutor(max_workers=12, initializer=_init, initargs=(jobs,)) as ex:
        out = [r for r in ex.map(_run, range(len(jobs)), chunksize=4) if r]

    print("Whisper-base FORCED de on German fragments (the arbiter bar):")
    print(f"  {'window':>7} {'reads de':>9} {'en':>5} {'empty':>6} {'word-precision':>15}")
    for dur in WINDOWS:
        rs = [r for r in out if r["dur"] == dur]
        n = len(rs)
        de = sum(r["cls"] == "de" for r in rs) / n
        en = sum(r["cls"] == "en" for r in rs) / n
        emp = sum(r["cls"] == "empty" for r in rs) / n
        precs = [r["prec"] for r in rs if r["prec"] is not None]
        print(f"  {dur:>6.1f}s {de*100:8.0f}% {en*100:4.0f}% {emp*100:5.0f}% "
              f"{np.mean(precs)*100:14.0f}%")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
