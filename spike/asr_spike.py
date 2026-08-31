"""NX Recall — ASR leg of the Step-0 spike.

The identity spike disproved "the codec is the quality ceiling" for embeddings.
This measures whether it holds for TRANSCRIPTION, which the brief also claims —
plus the two other unmeasured §4 items: hallucination on non-speech, and runtime.

Conditions mirror the identity spike (same corpus, same per-talker Opus chain, same
mixer), scored as WER against LibriSpeech reference transcripts. In overlap
conditions the reference is the DOMINANT talker's text: the product produces one
messy transcript of the mix, and the question is how badly babble corrupts the
words of the person you can hear. Insertions are reported separately — an inserted
word is babble leaking INTO the transcript, the failure mode that poisons search.

Models: Parakeet-TDT 0.6b v2 int8 (the brief's EN candidate), Parakeet 110m int8
(the plausible during-VR model), Whisper base (multilingual floor + the notorious
hallucinator, for the non-speech test).
"""

from __future__ import annotations

import os
import re
import sys
import time
from concurrent.futures import ProcessPoolExecutor
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).parent))
from harness import (SR, extract_corpus, index_corpus, load, mix,  # noqa: E402
                     opus_roundtrip, rms_normalise, trim_silence)

S = Path(os.environ.get("NXR_SCRATCH", "/tmp/nxr"))
CONDS = [("clean", 0, 1, 0.0), ("opus24k", 24, 1, 0.0), ("opus12k", 12, 1, 0.0),
         ("opus8k", 8, 1, 0.0),
         ("2tk+12dB", 24, 2, 12.0), ("2tk+6dB", 24, 2, 6.0), ("2tk+0dB", 24, 2, 0.0),
         ("3tk+12dB", 24, 3, 12.0), ("10tk+12dB", 24, 10, 12.0)]
N_UTTS = 60
SEED = 0xA5A5

_G: dict = {}


# ----------------------------------------------------------------- scoring

_norm_re = re.compile(r"[^a-z' ]+")
# LibriSpeech references spell these out; models emit the abbreviation. Expanding
# them keeps WER about acoustics rather than orthography.
_expand = {"mr": "mister", "mrs": "missus", "dr": "doctor", "st": "saint"}


def norm(text: str) -> list[str]:
    return [_expand.get(w, w) for w in _norm_re.sub(" ", text.lower()).split()]


def wer_counts(ref: list[str], hyp: list[str]) -> tuple[int, int, int]:
    """(substitutions+deletions, insertions, ref_len) via word Levenshtein."""
    R, H = len(ref), len(hyp)
    # dp[i][j] = (cost, insertions)
    dp = [[(0, 0)] * (H + 1) for _ in range(R + 1)]
    for j in range(1, H + 1):
        dp[0][j] = (j, j)
    for i in range(1, R + 1):
        dp[i][0] = (i, 0)
        for j in range(1, H + 1):
            if ref[i - 1] == hyp[j - 1]:
                dp[i][j] = dp[i - 1][j - 1]
            else:
                sub = (dp[i - 1][j - 1][0] + 1, dp[i - 1][j - 1][1])
                dele = (dp[i - 1][j][0] + 1, dp[i - 1][j][1])
                ins = (dp[i][j - 1][0] + 1, dp[i][j - 1][1] + 1)
                dp[i][j] = min(sub, dele, ins)
    cost, ins = dp[R][H]
    return cost - ins, ins, R


# ----------------------------------------------------------------- models

def find_model_dir(pattern: str) -> Path | None:
    hits = sorted((S / "models").glob(pattern))
    return hits[0] if hits else None


def make_recognizer(kind: str, d: Path, threads: int):
    import sherpa_onnx as so

    def one(glob):
        f = sorted(d.glob(glob))
        return str(f[0]) if f else None

    if kind == "whisper":
        return so.OfflineRecognizer.from_whisper(
            encoder=one("*encoder*.onnx"), decoder=one("*decoder*.onnx"),
            tokens=one("*tokens*.txt"), language="en", task="transcribe",
            num_threads=threads)
    return so.OfflineRecognizer.from_transducer(
        encoder=one("encoder*.onnx"), decoder=one("decoder*.onnx"),
        joiner=one("joiner*.onnx"), tokens=one("tokens*.txt"),
        num_threads=threads, model_type="nemo_transducer")


def _init(kind: str, mdir: str, audio_paths: list[str]):
    # Run at idle priority — the whole point of this project is not stealing CPU.
    try:
        os.nice(19 - os.nice(0))
    except OSError:
        pass
    _G["rec"] = make_recognizer(kind, Path(mdir), 1)
    _G["audio"] = [rms_normalise(trim_silence(load(Path(p)))) for p in audio_paths]


def _transcribe(samples: np.ndarray) -> str:
    st = _G["rec"].create_stream()
    st.accept_waveform(SR, samples.astype(np.float32))
    _G["rec"].decode_stream(st)
    return st.result.text


def _run(spec: dict) -> dict:
    rng = np.random.default_rng(spec["seed"])
    a = _G["audio"]
    tgt = a[spec["t"]]
    if spec["br"]:
        tgt = opus_roundtrip(tgt, spec["br"])
    itfs = []
    for i in spec["itf"]:
        w = a[i]
        if len(w) < len(tgt):
            w = np.tile(w, int(np.ceil(len(tgt) / len(w))))
        o = int(rng.integers(0, max(1, len(w) - len(tgt))))
        itfs.append(opus_roundtrip(w[o:o + len(tgt)], spec["br"] or 24))
    m = mix(tgt, itfs, spec["dom"]) if itfs else tgt
    sd, ins, n = wer_counts(norm(spec["ref"]), norm(_transcribe(m)))
    return {"cond": spec["cond"], "sd": sd, "ins": ins, "n": n}


# ----------------------------------------------------------------- main

def read_transcripts(root: Path) -> dict[str, str]:
    t = {}
    for f in root.rglob("*.trans.txt"):
        for line in f.read_text().splitlines():
            uid, _, text = line.partition(" ")
            t[uid] = text
    return t


def main() -> int:
    root = extract_corpus(S / "corpus" / "dev-clean.tar.gz", S / "corpus")
    trans = read_transcripts(root)
    by = index_corpus(root, 8)
    spk = sorted(by)

    rng = np.random.default_rng(SEED)
    utts, paths = [], []
    for i, s in enumerate(spk):
        for u in by[s][3:5]:                      # probe range, 2 per speaker
            if len(utts) < N_UTTS:
                utts.append((len(paths), trans[u.path.stem], s))
                paths.append(str(u.path))
    others = list(range(len(paths)))

    specs = []
    for cond, br, ntk, dom in CONDS:
        for ti, ref, s in utts:
            pool = [i for i in others if i != ti]
            itf = list(rng.choice(pool, ntk - 1, replace=False)) if ntk > 1 else []
            specs.append(dict(cond=cond, t=ti, ref=ref, br=br, dom=dom,
                              itf=[int(i) for i in itf],
                              seed=int(rng.integers(1 << 31))))

    models = [("parakeet_0.6b", "transducer", find_model_dir("*parakeet-tdt-0.6b*")),
              ("parakeet_110m", "transducer", find_model_dir("*parakeet_tdt_transducer_110m*")),
              ("whisper_base", "whisper", find_model_dir("*whisper-base*"))]

    print(f"{len(utts)} utterances x {len(CONDS)} conditions")
    for name, kind, mdir in models:
        if mdir is None:
            print(f"\n=== {name}: MISSING, skipped ===")
            continue
        t0 = time.time()
        with ProcessPoolExecutor(max_workers=12, initializer=_init,
                                 initargs=(kind, str(mdir), paths)) as ex:
            out = list(ex.map(_run, specs, chunksize=4))
        agg: dict[str, list] = {}
        for r in out:
            agg.setdefault(r["cond"], []).append(r)
        print(f"\n=== {name}  ({time.time()-t0:.0f}s) ===")
        print(f"  {'condition':>10} {'WER':>7} {'ins-rate':>9}")
        for cond, *_ in CONDS:
            rs = agg[cond]
            n = sum(r["n"] for r in rs)
            print(f"  {cond:>10} {sum(r['sd']+r['ins'] for r in rs)/n*100:6.1f}% "
                  f"{sum(r['ins'] for r in rs)/n*100:8.1f}%")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
