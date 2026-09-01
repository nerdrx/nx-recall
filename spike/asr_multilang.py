"""Can the daemon speak German? Parakeet v3 (multilingual) vs the shipped EN 110m.

The user's lobbies are German and English. The shipped ASR (parakeet 110m) is
English-only — German comes out as English-shaped noise. Candidate: the
multilingual parakeet-tdt-0.6b-v3 (25 languages incl. de+en, same transducer
API, drop-in via [models] config). This measures what the swap costs and buys:

  German : FLEURS de_de dev (read sentences, reference transcripts)
  English: LibriSpeech dev-clean (same protocol as spike §10)
  Both   : clean and through the Opus-24k voice-chat codec
  Speed  : RTF per model

Env: NXR_SCRATCH points at a dir with models/, corpus/, de/ as downloaded.
"""

from __future__ import annotations

import os
import re
import sys
import time
from concurrent.futures import ProcessPoolExecutor
from pathlib import Path

import numpy as np
import soundfile as sf

sys.path.insert(0, str(Path(__file__).parent))
from asr_spike import make_recognizer, wer_counts  # noqa: E402
from harness import SR, extract_corpus, index_corpus, load, opus_roundtrip, rms_normalise, trim_silence  # noqa: E402

S = Path(os.environ.get("NXR_SCRATCH", "/tmp/nxr"))
N_UTTS = 60

MODELS = {
    "parakeet_110m_EN": S / "models" / "sherpa-onnx-nemo-parakeet_tdt_transducer_110m-en-36000-int8",
    "parakeet_v3_multi": S / "models" / "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8",
}

# German normalisation: keep umlauts and eszett, fold both cases the same way
# casefold() does (ß → ss), so references and hypotheses meet in the middle.
_de_re = re.compile(r"[^a-zäöüß' ]+")


def norm_de(text: str) -> list[str]:
    return _de_re.sub(" ", text.casefold().replace("ss", "ß").replace("ß", "ss")).split()


_en_re = re.compile(r"[^a-z' ]+")
_expand = {"mr": "mister", "mrs": "missus", "dr": "doctor", "st": "saint"}


def norm_en(text: str) -> list[str]:
    return [_expand.get(w, w) for w in _en_re.sub(" ", text.lower()).split()]


def load_wav_16k(p: Path) -> np.ndarray:
    x, sr = sf.read(str(p), dtype="float32", always_2d=False)
    if x.ndim > 1:
        x = x.mean(axis=1)
    if sr != SR:  # FLEURS ships 16 kHz, but be safe: naive decimate/interp
        idx = np.linspace(0, len(x) - 1, int(len(x) * SR / sr)).astype(np.int64)
        x = x[idx]
    return rms_normalise(trim_silence(x))


def fleurs_set() -> list[tuple[Path, str]]:
    rows = []
    for line in (S / "de" / "dev.tsv").read_text().splitlines():
        parts = line.split("\t")
        if len(parts) >= 3 and parts[1].endswith(".wav"):
            p = S / "de" / "dev" / parts[1]
            if p.is_file():
                rows.append((p, parts[2]))
    return rows[:N_UTTS]


def libri_set() -> list[tuple[Path, str]]:
    root = extract_corpus(S / "corpus" / "dev-clean.tar.gz", S / "corpus")
    trans = {}
    for f in root.rglob("*.trans.txt"):
        for line in f.read_text().splitlines():
            uid, _, text = line.partition(" ")
            trans[uid] = text
    by = index_corpus(root, 5)
    rows = []
    for spk in sorted(by):
        for u in by[spk][3:5]:
            if len(rows) < N_UTTS:
                rows.append((u.path, trans[u.path.stem]))
    return rows


_G: dict = {}


def _init(kind, mdir, jobs):
    try:
        os.nice(19 - os.nice(0))
    except OSError:
        pass
    _G["rec"] = make_recognizer(kind, Path(mdir), 1)
    _G["jobs"] = jobs


def _run(i: int) -> dict:
    path, ref, lang, codec = _G["jobs"][i]
    x = load_wav_16k(Path(path))
    if codec:
        x = opus_roundtrip(x, 24)
    st = _G["rec"].create_stream()
    st.accept_waveform(SR, x.astype(np.float32))
    t0 = time.time()
    _G["rec"].decode_stream(st)
    dt = time.time() - t0
    hyp = st.result.text
    n = norm_de if lang == "de" else norm_en
    sd, ins, nref = wer_counts(n(ref), n(hyp))
    return {"lang": lang, "codec": codec, "sd": sd, "ins": ins, "n": nref,
            "dt": dt, "dur": len(x) / SR,
            "sample": hyp[:90] if i % 30 == 0 else None}


def main() -> int:
    de, en = fleurs_set(), libri_set()
    print(f"German: {len(de)} FLEURS utts · English: {len(en)} LibriSpeech utts")
    jobs = []
    for path, ref in de:
        jobs += [(str(path), ref, "de", False), (str(path), ref, "de", True)]
    for path, ref in en:
        jobs += [(str(path), ref, "en", False), (str(path), ref, "en", True)]

    for name, mdir in MODELS.items():
        if not mdir.is_dir():
            print(f"\n{name}: missing, skipped")
            continue
        t0 = time.time()
        with ProcessPoolExecutor(max_workers=12, initializer=_init,
                                 initargs=("transducer", str(mdir), jobs)) as ex:
            out = list(ex.map(_run, range(len(jobs)), chunksize=4))
        agg: dict[tuple, list] = {}
        for r in out:
            agg.setdefault((r["lang"], r["codec"]), []).append(r)
        print(f"\n=== {name}  ({time.time()-t0:.0f}s wall) ===")
        print(f"  {'set':>14} {'WER':>7} {'ins':>6} {'RTF':>7}")
        for (lang, codec), rs in sorted(agg.items()):
            n = sum(r["n"] for r in rs)
            wer = sum(r["sd"] + r["ins"] for r in rs) / n
            ins = sum(r["ins"] for r in rs) / n
            rtf = sum(r["dt"] for r in rs) / sum(r["dur"] for r in rs)
            tag = f"{lang}{' +opus24' if codec else ' clean'}"
            print(f"  {tag:>14} {wer*100:6.1f}% {ins*100:5.1f}% {rtf:7.3f}")
        for r in out:
            if r["sample"] and r["lang"] == "de" and not r["codec"]:
                print(f"  de sample: \"{r['sample']}\"")
                break
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
