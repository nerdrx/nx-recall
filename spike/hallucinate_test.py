"""How much text does each ASR model invent on NON-speech audio?

The brief (§4) flags Whisper's silence hallucination as a mandatory thing to filter.
This measures it — and whether Parakeet (a transducer, architecturally less prone)
actually needs the same defence. Inputs are 15 s each of:

  silence      digital zero
  noise        low white noise (idle mic / room hiss)
  roomtone     low-passed noise (HVAC-ish rumble)
  music        synthesized chord loop with melody (world-audio stand-in)
  babble       8 equal talkers at low level (distant crowd, nobody dominant)

A word emitted on any of these is a GHOST — it goes into the transcript, into FTS,
and into search results for things nobody said.
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).parent))
from asr_spike import find_model_dir, make_recognizer, norm  # noqa: E402
from harness import SR, extract_corpus, index_corpus, load, rms_normalise, trim_silence  # noqa: E402

S = Path(os.environ.get("NXR_SCRATCH", "/tmp/nxr"))
DUR = 15 * SR


def inputs() -> dict[str, np.ndarray]:
    rng = np.random.default_rng(42)
    t = np.arange(DUR) / SR

    noise = (rng.standard_normal(DUR) * 0.01).astype(np.float32)

    lp = np.copy(noise)                       # crude one-pole low-pass ~150 Hz
    a = float(np.exp(-2 * np.pi * 150 / SR))
    for i in range(1, DUR):
        lp[i] = a * lp[i - 1] + (1 - a) * noise[i] * 8

    music = np.zeros(DUR, dtype=np.float32)   # I-vi-IV-V loop + melody line
    for bar, chord in enumerate([(220, 277, 330), (185, 220, 277),
                                 (175, 220, 262), (196, 247, 294)] * 4):
        seg = slice(bar * SR, min((bar + 1) * SR, DUR))
        tt = t[seg]
        for f in chord:
            for h, g in [(1, .12), (2, .05), (3, .02)]:
                music[seg] += g * np.sin(2 * np.pi * f * h * tt)
        music[seg] += .08 * np.sin(2 * np.pi * chord[2] * 2 * tt) * \
            np.clip(np.sin(2 * np.pi * 2 * tt), 0, 1)

    root = extract_corpus(S / "corpus" / "dev-clean.tar.gz", S / "corpus")
    by = index_corpus(root, 4)
    spk = sorted(by)[:8]
    bab = np.zeros(DUR, dtype=np.float32)
    for s in spk:
        x = rms_normalise(trim_silence(load(by[s][0].path)))
        x = np.tile(x, int(np.ceil(DUR / len(x))))[:DUR]
        bab += x * 0.03
    return {"silence": np.zeros(DUR, dtype=np.float32), "noise": noise,
            "roomtone": lp.astype(np.float32), "music": music * 0.5, "babble": bab}


def main() -> int:
    cases = inputs()
    models = [("parakeet_0.6b", "transducer", find_model_dir("*parakeet-tdt-0.6b*")),
              ("parakeet_110m", "transducer", find_model_dir("*parakeet_tdt_transducer_110m*")),
              ("whisper_base", "whisper", find_model_dir("*whisper-base*"))]
    print(f"\nGHOST WORDS on 15 s of non-speech (count; text shown when short)")
    print(f"  {'input':>9}" + "".join(f"{n:>16}" for n, *_ in models))
    rows = {k: [] for k in cases}
    for name, kind, mdir in models:
        rec = make_recognizer(kind, mdir, 8)
        for cname, x in cases.items():
            st = rec.create_stream()
            st.accept_waveform(SR, x)
            rec.decode_stream(st)
            words = norm(st.result.text)
            rows[cname].append((len(words), " ".join(words)[:40]))
    for cname in cases:
        print(f"  {cname:>9}" + "".join(f"{c:>15}w" for c, _ in rows[cname]))
    print("\n  emitted text (truncated):")
    for cname in cases:
        for (c, txt), (mname, *_z) in zip(rows[cname], models):
            if c:
                print(f"    {cname:>9} / {mname:<14} \"{txt}\"")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
