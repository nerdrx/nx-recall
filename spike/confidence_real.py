#!/usr/bin/env python3
"""Does the shaky/solid verdict separate good decodes from bad ones on REAL audio?

The lab (FINDINGS §12, gate 3) calibrated τ on FLEURS/LibriSpeech through Opus.
On the user's own recordings the daemon then marked 45% of rows shaky against
24% in the lab. Two readings are possible: real lobby audio is simply that hard
(§11: any two models agree on ~half of real sentences), or τ is wrong for it.

There is no human reference for these rows, so the reference is the best model
in the §11 bake-off — whisper-large-v3 (4.7% lab WER, RTF 17, far too slow to
run live). WER of the live text against large-v3, bucketed by the daemon's own
verdict. If shaky rows carry several times the "WER" of solid rows the flag is
doing its job; if the buckets look alike, τ needs re-calibrating on real audio.

Reads the LIVE database and clip store READ-ONLY (the user authorised these
recordings for exactly this — nothing leaves the machine; nothing is written
outside the scratchpad). Runs at nice 19 on the pinned cores like every bench.

    taskset -c 16-31 nice -n 19 spike/venv/bin/python spike/confidence_real.py
"""
from __future__ import annotations

import json
import os
import re
import sqlite3
import sys
import time
import unicodedata
from concurrent.futures import ProcessPoolExecutor
from pathlib import Path

import numpy as np

HOME = Path.home()
DB = HOME / ".local/share/nx-recall/recall.db"
DATA = HOME / ".local/share/nx-recall"
SCRATCH = Path(
    os.environ.get(
        "NXR_SCRATCH",
        "/tmp/claude-1000/-run-media-nerdrx-Lex-claude/61962651-5c6e-4a8f-93fe-d58f323a2b89/scratchpad",
    )
)
LARGE = SCRATCH / "models/sherpa-onnx-whisper-large-v3"
OUT = SCRATCH / "confidence_real.json"

PER_BUCKET_LONG = int(os.environ.get("N_LONG", "50"))  # ≥1.5 s rows per verdict
PER_BUCKET_SHORT = int(os.environ.get("N_SHORT", "30"))  # <1.5 s rows per verdict
WORKERS = int(os.environ.get("WORKERS", "6"))
THREADS = int(os.environ.get("THREADS", "2"))


def sample() -> list[dict]:
    con = sqlite3.connect(f"file:{DB}?mode=ro", uri=True)
    con.row_factory = sqlite3.Row
    rows: list[dict] = []
    for verdict in ("shaky", "solid"):
        for short, n in ((0, PER_BUCKET_LONG), (1, PER_BUCKET_SHORT)):
            cond = "< 1500000000" if short else ">= 1500000000"
            q = f"""
                SELECT id, audio_path, text, lang, asr_confidence,
                       (t_end_ns - t_start_ns) / 1e9 AS dur, text_via
                FROM segments
                WHERE deleted_at IS NULL AND asr_confidence = ?
                  AND text IS NOT NULL AND LENGTH(TRIM(text)) > 0
                  AND audio_path <> '' AND (t_end_ns - t_start_ns) {cond}
                ORDER BY RANDOM() LIMIT ?"""
            for r in con.execute(q, (verdict, n)):
                d = dict(r)
                d["short"] = bool(short)
                rows.append(d)
    con.close()
    return rows


def read_wav(path: Path) -> tuple[np.ndarray, int]:
    import wave

    with wave.open(str(path), "rb") as w:
        rate = w.getframerate()
        n = w.getnframes()
        ch = w.getnchannels()
        width = w.getsampwidth()
        raw = w.readframes(n)
    if width == 2:
        x = np.frombuffer(raw, dtype=np.int16).astype(np.float32) / 32768.0
    elif width == 4:
        x = np.frombuffer(raw, dtype=np.int32).astype(np.float32) / 2147483648.0
    else:
        raise ValueError(f"unsupported sample width {width}")
    if ch > 1:
        x = x.reshape(-1, ch).mean(axis=1)
    return x, rate


def resample(x: np.ndarray, rate: int, to: int = 16000) -> np.ndarray:
    if rate == to:
        return x
    n = int(round(len(x) * to / rate))
    return np.interp(np.linspace(0, len(x), n, endpoint=False), np.arange(len(x)), x).astype(
        np.float32
    )


_REC = None


def _init():
    global _REC
    import sherpa_onnx as so

    _REC = so.OfflineRecognizer.from_whisper(
        encoder=str(LARGE / "large-v3-encoder.int8.onnx"),
        decoder=str(LARGE / "large-v3-decoder.int8.onnx"),
        tokens=str(LARGE / "large-v3-tokens.txt"),
        language="",
        task="transcribe",
        num_threads=THREADS,
    )


def _decode(job: dict) -> dict:
    x, rate = read_wav(DATA / job["audio_path"])
    x = resample(x, rate)
    # One shared recogniser with language detection on: large-v3 detects de/en
    # reliably at this size, and a per-row recogniser would cost a model load.
    rec = _REC
    s = rec.create_stream()
    s.accept_waveform(16000, x)
    t0 = time.time()
    rec.decode_stream(s)
    job["ref"] = s.result.text.strip()
    job["ref_s"] = time.time() - t0
    return job


_PUNCT = re.compile(r"[^\w\s]", re.UNICODE)


def norm(s: str) -> list[str]:
    s = unicodedata.normalize("NFKC", s).lower()
    s = _PUNCT.sub(" ", s)
    return s.split()


def wer(ref: list[str], hyp: list[str]) -> float | None:
    if not ref:
        return None
    d = list(range(len(hyp) + 1))
    for i, r in enumerate(ref, 1):
        prev, d[0] = d[0], i
        for j, h in enumerate(hyp, 1):
            cur = min(d[j] + 1, d[j - 1] + 1, prev + (r != h))
            prev, d[j] = d[j], cur
    return d[len(hyp)] / len(ref)


def summarise(rows: list[dict]) -> dict:
    out: dict = {}
    for short in (False, True):
        for verdict in ("shaky", "solid"):
            sel = [r for r in rows if r["short"] == short and r["asr_confidence"] == verdict]
            scored = [r for r in sel if r.get("wer") is not None]
            empty_ref = sum(1 for r in sel if not norm(r.get("ref", "")))
            key = f"{'short' if short else 'long'}/{verdict}"
            out[key] = {
                "n": len(sel),
                "scored": len(scored),
                "ref_empty": empty_ref,
                "mean_wer_vs_large": (sum(r["wer"] for r in scored) / len(scored)) if scored else None,
                "median_wer_vs_large": float(np.median([r["wer"] for r in scored])) if scored else None,
                "exact_match": sum(1 for r in scored if r["wer"] == 0) / len(scored) if scored else None,
            }
    for length in ("long", "short"):
        a, b = out[f"{length}/shaky"], out[f"{length}/solid"]
        if a["mean_wer_vs_large"] and b["mean_wer_vs_large"]:
            out[f"{length}/ratio"] = a["mean_wer_vs_large"] / b["mean_wer_vs_large"]
    return out


def main() -> int:
    if not LARGE.exists():
        print(f"missing {LARGE}", file=sys.stderr)
        return 2
    rows = sample()
    print(f"{len(rows)} rows sampled", flush=True)
    t0 = time.time()
    done: list[dict] = []
    with ProcessPoolExecutor(max_workers=WORKERS, initializer=_init) as ex:
        for i, r in enumerate(ex.map(_decode, rows), 1):
            r["wer"] = wer(norm(r["ref"]), norm(r["text"]))
            done.append(r)
            if i % 10 == 0:
                print(f"  {i}/{len(rows)}  {time.time() - t0:.0f}s", flush=True)
    summary = summarise(done)
    OUT.write_text(json.dumps({"summary": summary, "rows": done}, ensure_ascii=False, indent=1))
    print(json.dumps(summary, indent=1))
    print("\n--- a few shaky rows (live | large-v3) ---")
    for r in [r for r in done if r["asr_confidence"] == "shaky" and not r["short"]][:12]:
        print(f"{r['dur']:.1f}s wer={r['wer']}\n   live : {r['text']}\n   large: {r['ref']}")
    print("\n--- a few solid rows ---")
    for r in [r for r in done if r["asr_confidence"] == "solid" and not r["short"]][:8]:
        print(f"{r['dur']:.1f}s wer={r['wer']}\n   live : {r['text']}\n   large: {r['ref']}")
    print(f"\nwrote {OUT}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
