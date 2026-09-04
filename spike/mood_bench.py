#!/usr/bin/env python3
"""How good are SenseVoice's emotion and event tags on THIS corpus? (FINDINGS §42)

SenseVoice-small emits four tags per decode: language, emotion (<|HAPPY|>,
<|SAD|>, <|ANGRY|>, <|NEUTRAL|>, <|FEARFUL|>, <|DISGUSTED|>, <|SURPRISED|>),
audio event (<|Speech|>, <|BGM|>, <|Applause|>, <|Laughter|>, <|Cry|>) and
whether ITN ran. The daemon strips all four today (asr_cjk::strip_tags).

This asks whether either is worth rendering, on the user's own archive, before
a line of GUI is written. Nobody here can listen to the clips, so both are
scored against text proxies, stated honestly.

  chrt -i 0 taskset -c 12-15 nice -n 19 \
    nx-scratch/venv/bin/python mood_bench.py --rows 4000
"""

import argparse
import json
import pathlib
import random
import re
import sqlite3
import sys
import time
import wave

import numpy as np
import sherpa_onnx

DB = pathlib.Path.home() / ".local/share/nx-recall/recall.db"
DATA = pathlib.Path.home() / ".local/share/nx-recall"
MODEL = DATA / "models/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17"
OUT = pathlib.Path("/tmp/nx-recall-workspace/nx-scratch/agents-2026-09-04/mood/mood_bench.json")

TAG = re.compile(r"<\|([^|>]*)\|>")


def read_wav(path):
    with wave.open(str(path)) as w:
        sr = w.getframerate()
        ch = w.getnchannels()
        n = w.getnframes()
        raw = w.readframes(n)
    a = np.frombuffer(raw, dtype=np.int16).astype(np.float32) / 32768.0
    if ch == 2:
        a = a.reshape(-1, 2).mean(axis=1)
    return sr, a


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--rows", type=int, default=4000)
    ap.add_argument("--threads", type=int, default=4)
    ap.add_argument("--seed", type=int, default=42)
    ap.add_argument("--min-s", type=float, default=1.0)
    ap.add_argument("--out", default=None)
    args = ap.parse_args()
    out_path = pathlib.Path(args.out) if args.out else OUT
    out_path.parent.mkdir(parents=True, exist_ok=True)

    con = sqlite3.connect(f"file:{DB}?mode=ro", uri=True)
    rows = con.execute(
        """SELECT id, audio_path, text, night_text, lang,
                  (t_end_ns - t_start_ns) / 1e9 AS dur
             FROM segments
            WHERE audio_path <> ''
              AND (t_end_ns - t_start_ns) / 1e9 >= ?
         ORDER BY id""",
        (args.min_s,),
    ).fetchall()
    con.close()
    print(f"{len(rows)} rows with audio and >= {args.min_s}s", file=sys.stderr)
    random.Random(args.seed).shuffle(rows)
    rows = rows[: args.rows]

    rec = sherpa_onnx.OfflineRecognizer.from_sense_voice(
        model=str(MODEL / "model.int8.onnx"),
        tokens=str(MODEL / "tokens.txt"),
        num_threads=args.threads,
        provider="cpu",
        language="",
        use_itn=False,
        debug=False,
    )

    out = []
    audio_s = 0.0
    t0 = time.perf_counter()
    for i, (sid, path, text, night, lang, dur) in enumerate(rows):
        wav = DATA / path
        if not wav.is_file():
            continue
        try:
            sr, samples = read_wav(wav)
        except Exception as e:
            print(f"skip {sid}: {e}", file=sys.stderr)
            continue
        if samples.size == 0:
            continue
        audio_s += samples.size / sr
        st = rec.create_stream()
        st.accept_waveform(sr, samples)
        rec.decode_stream(st)
        r = st.result
        emo = TAG.findall(r.emotion or "")
        evt = TAG.findall(r.event or "")
        probs = list(getattr(r, "ys_log_probs", []) or [])
        out.append(
            {
                "id": sid,
                "dur": round(samples.size / sr, 3),
                "lang": lang,
                "live": text or "",
                "night": night or "",
                "sv_text": TAG.sub("", r.text or "").strip(),
                "sv_lang": (TAG.findall(r.lang or "") or [None])[0],
                "mood": (emo or [None])[0],
                "event": (evt or [None])[0],
                "n_logprobs": len(probs),
                "head_logprobs": [round(float(p), 4) for p in probs[:4]],
            }
        )
        if (i + 1) % 250 == 0:
            el = time.perf_counter() - t0
            print(
                f"{i+1}/{len(rows)}  {audio_s:.0f}s audio  {el:.0f}s wall  RTF {el/max(audio_s,1e-9):.3f}",
                file=sys.stderr,
                flush=True,
            )

    wall = time.perf_counter() - t0
    report = {
        "rows": len(out),
        "audio_s": round(audio_s, 1),
        "wall_s": round(wall, 1),
        "rtf": round(wall / max(audio_s, 1e-9), 4),
        "threads": args.threads,
        "clips": out,
    }
    out_path.write_text(json.dumps(report, ensure_ascii=False))
    print(f"wrote {out_path} — {len(out)} rows, RTF {report['rtf']}", file=sys.stderr)


if __name__ == "__main__":
    main()
