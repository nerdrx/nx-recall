"""How often does the multilingual ASR decode German as English on SHORT audio?

The user reports language confusion in live transcripts. The FLEURS benchmark
(8.4% WER) used full read sentences; lobby speech is 1-3 s fragments, which is
where a multilingual model picks the wrong language and commits. This measures
the flip rate as a function of fragment length, and doubles as the test bed for
the text-side de/en detector the per-speaker language feature needs.

Windows are cut from FLEURS German utterances (so ground truth = German), run
through v3 clean and through Opus 24k, and the OUTPUT text is classified.
"""

from __future__ import annotations

import os
import re
import sys
from concurrent.futures import ProcessPoolExecutor
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).parent))
from asr_multilang import load_wav_16k  # noqa: E402
from asr_spike import make_recognizer  # noqa: E402
from harness import SR, opus_roundtrip  # noqa: E402

S = Path(os.environ.get("NXR_SCRATCH", "/tmp/nxr"))
V3 = S / "models" / "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8"
WINDOWS = [1.0, 1.5, 2.0, 3.0, 5.0]
N_PER = 80

# ---------------------------------------------------------------- detector
# Stopword voting: tiny, deterministic, and exactly what the daemon would ship.
DE = set("der die das und ist nicht ich du wir ihr sie es ein eine einen dem den "
         "mit von für auf als auch aber wenn dann noch schon nur mal was wie wo "
         "ja nein doch beim vom zur zum über unter zwischen gegen ohne durch".split())
EN = set("the a an and is are was were not i you we they it this that of to in "
         "for on with as at by from but if then just only what how where yes no "
         "about into over under between against without through".split())
_w = re.compile(r"[a-zäöüß']+")


def classify(text: str) -> str:
    words = _w.findall(text.casefold())
    if not words:
        return "empty"
    if any(c in "äöüß" for c in text.casefold()):
        return "de"
    d = sum(w in DE for w in words)
    e = sum(w in EN for w in words)
    if d > e:
        return "de"
    if e > d:
        return "en"
    return "unclear"


_G: dict = {}


def _init(jobs):
    try:
        os.nice(19 - os.nice(0))
    except OSError:
        pass
    _G["rec"] = make_recognizer("transducer", V3, 1)
    _G["jobs"] = jobs


def _run(i):
    path, off, dur, codec = _G["jobs"][i]
    x = load_wav_16k(Path(path))
    seg = x[int(off * SR):int((off + dur) * SR)]
    if len(seg) < SR // 2:
        return None
    if codec:
        seg = opus_roundtrip(seg, 24)
    st = _G["rec"].create_stream()
    st.accept_waveform(SR, seg.astype(np.float32))
    _G["rec"].decode_stream(st)
    txt = st.result.text
    return {"dur": dur, "codec": codec, "cls": classify(txt),
            "sample": txt[:70] if classify(txt) == "en" else None}


def main() -> int:
    rng = np.random.default_rng(7)
    wavs = sorted((S / "de" / "dev").glob("*.wav"))
    jobs = []
    for dur in WINDOWS:
        picks = rng.choice(len(wavs), size=N_PER, replace=False)
        for i in picks:
            x_len = 12.0  # FLEURS utterances are long; offset conservatively
            off = float(rng.uniform(0.5, max(0.6, x_len - dur - 0.5)))
            for codec in (False, True):
                jobs.append((str(wavs[i]), off, dur, codec))

    with ProcessPoolExecutor(max_workers=12, initializer=_init, initargs=(jobs,)) as ex:
        out = [r for r in ex.map(_run, range(len(jobs)), chunksize=6) if r]

    print("German fragments through v3 — what language does the OUTPUT read as?")
    print(f"  {'window':>7} {'codec':>7} {'de':>6} {'en(FLIP)':>9} {'unclear':>8} {'empty':>6}")
    flips = []
    for dur in WINDOWS:
        for codec in (False, True):
            rs = [r for r in out if r["dur"] == dur and r["codec"] == codec]
            if not rs:
                continue
            n = len(rs)
            c = {k: sum(r["cls"] == k for r in rs) / n for k in ("de", "en", "unclear", "empty")}
            print(f"  {dur:>6.1f}s {'opus24' if codec else 'clean':>7} "
                  f"{c['de']*100:5.0f}% {c['en']*100:8.0f}% {c['unclear']*100:7.0f}% {c['empty']*100:5.0f}%")
            flips += [r for r in rs if r["cls"] == "en"]
    print(f"\n  flip examples (German audio, English-looking output):")
    for r in flips[:5]:
        if r["sample"]:
            print(f"    [{r['dur']}s] \"{r['sample']}\"")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
