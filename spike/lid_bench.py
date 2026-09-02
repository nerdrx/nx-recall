#!/usr/bin/env python3
"""Spoken language identification from audio — can it catch Japanese?

The failure this answers (FINDINGS §22): Parakeet-TDT-0.6b-v3 speaks 25
European languages and no Japanese, and it does not fail *loudly*. It renders
"sumimasen, ogenki desu ka" as "Sima Sen Okenki Deska." — Latin letters,
English-shaped words, nothing for a text classifier to catch. The decision to
re-decode has to come from the AUDIO.

sherpa-onnx ships `SpokenLanguageIdentification`, which is the multilingual
Whisper encoder plus one decoder step, read for the language token. This
measures it on FLEURS at the lengths a VRChat turn actually is.

Two exports, because the cost difference is 10x and it might not buy anything:
  * whisper-tiny  int8 (encoder 12.9 MB)
  * whisper-base  int8 (encoder 29.1 MB) — already on disk for the German
    arbiter, so on an install that has `--arbiter-de` it is free.

Lengths, because full-utterance LID benchmarks say nothing about a lobby: the
median real turn is 2.4-2.7 s (FINDINGS §10).

Confidence: the sherpa C API returns a language string and NO score
(`SherpaOnnxSpokenLanguageIdentificationResult` carries one field, `lang`).
So a threshold has to be built out of something the API does give, and the
only honest one is AGREEMENT: run LID over K overlapping windows of the turn
and count how many say the same thing. That is what `--votes` measures.

Usage:
  spike/venv/bin/python spike/lid_bench.py            # single-shot, all
  spike/venv/bin/python spike/lid_bench.py --votes    # the 3-window vote
"""

import argparse
import json
import pathlib
import random
import sys
import time

import numpy as np
import sherpa_onnx
import soundfile as sf

SCRATCH = pathlib.Path("/tmp/nx-recall-workspace/nx-scratch")
FLEURS = SCRATCH / "fleurs"
LIVE = pathlib.Path.home() / ".local/share/nx-recall/models"
MODELS_JA = SCRATCH / "models-ja"

SAMPLE_RATE = 16000
# ja is the language we must catch; de and en are the ones a false positive
# would steal a turn from. They are also exactly what this user's evenings are.
LANGS = {"ja_jp": "ja", "de_de": "de", "en_us": "en"}
N_PER_LANG = 200

EXPORTS = {
    "whisper-tiny": (
        MODELS_JA / "sherpa-onnx-whisper-tiny/tiny-encoder.int8.onnx",
        MODELS_JA / "sherpa-onnx-whisper-tiny/tiny-decoder.int8.onnx",
    ),
    "whisper-base": (
        LIVE / "sherpa-onnx-whisper-base/base-encoder.int8.onnx",
        LIVE / "sherpa-onnx-whisper-base/base-decoder.int8.onnx",
    ),
}

# Full utterance, and the two cut lengths that bracket a real turn.
LENGTHS = [None, 3.0, 1.5]


def load_utterances(lang_dir: str, n: int, seed: int = 20260902):
    """`n` FLEURS test wavs for one language, deterministically chosen."""
    tsv = FLEURS / f"{lang_dir}.test.tsv"
    audio = FLEURS / "audio" / lang_dir / "test"
    names = []
    seen = set()
    with tsv.open(encoding="utf-8") as fh:
        for line in fh:
            parts = line.rstrip("\n").split("\t")
            if len(parts) < 3:
                continue
            wav, text = parts[1], parts[2]
            if wav in seen:
                continue
            seen.add(wav)
            p = audio / wav
            if p.is_file():
                names.append((p, text))
    rng = random.Random(seed)
    rng.shuffle(names)
    return names[:n]


def read_audio(path: pathlib.Path):
    data, sr = sf.read(str(path), dtype="float32", always_2d=False)
    if data.ndim > 1:
        data = data.mean(axis=1)
    assert sr == SAMPLE_RATE, f"{path} is {sr} Hz, expected {SAMPLE_RATE}"
    return data


def centre_cut(samples: np.ndarray, seconds: float) -> np.ndarray:
    """A `seconds`-long slice from the middle.

    The middle rather than the head on purpose: FLEURS read speech opens with
    a beat of silence, and a cut that is a third silence measures the VAD's
    job, not the language identifier's.
    """
    want = int(seconds * SAMPLE_RATE)
    if len(samples) <= want:
        return samples
    start = (len(samples) - want) // 2
    return samples[start : start + want]


def vote_windows(samples: np.ndarray, seconds: float, k: int = 3):
    """`k` evenly spaced windows of `seconds`, for the agreement confidence.

    Overlapping when the turn is short, which is the common case and is fine:
    the question the vote answers is "does this reading survive being asked
    about a different part of the audio", and on a 2 s turn the windows share
    most of their samples but not their edges.
    """
    want = int(seconds * SAMPLE_RATE)
    if len(samples) <= want:
        return [samples]
    span = len(samples) - want
    return [samples[int(i * span / (k - 1)) : int(i * span / (k - 1)) + want] for i in range(k)]


def build(name: str, threads: int):
    encoder, decoder = EXPORTS[name]
    for p in (encoder, decoder):
        if not p.is_file():
            sys.exit(f"missing {p}")
    return sherpa_onnx.SpokenLanguageIdentification(
        sherpa_onnx.SpokenLanguageIdentificationConfig(
            whisper=sherpa_onnx.SpokenLanguageIdentificationWhisperConfig(
                encoder=str(encoder), decoder=str(decoder)
            ),
            num_threads=threads,
            provider="cpu",
        )
    )


def identify(slid, samples: np.ndarray) -> str:
    stream = slid.create_stream()
    stream.accept_waveform(SAMPLE_RATE, samples)
    return slid.compute(stream)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--n", type=int, default=N_PER_LANG)
    ap.add_argument("--threads", type=int, default=4)
    ap.add_argument("--votes", action="store_true", help="3-window agreement confidence")
    ap.add_argument("--exports", default="whisper-tiny,whisper-base")
    ap.add_argument("--out", default=str(SCRATCH / "lid_bench.json"))
    args = ap.parse_args()

    corpus = {}
    for lang_dir, tag in LANGS.items():
        items = load_utterances(lang_dir, args.n)
        if len(items) < args.n:
            print(f"WARNING: only {len(items)} utterances for {tag}", file=sys.stderr)
        corpus[tag] = [read_audio(p) for p, _ in items]
        print(f"{tag}: {len(corpus[tag])} utterances", flush=True)

    results = {}
    for export in args.exports.split(","):
        slid = build(export, args.threads)
        for length in LENGTHS:
            label = "full" if length is None else f"{length}s"
            for tag, clips in corpus.items():
                heard = []
                audio_s = 0.0
                t0 = time.perf_counter()
                for clip in clips:
                    cut = clip if length is None else centre_cut(clip, length)
                    audio_s += len(cut) / SAMPLE_RATE
                    if args.votes and length is not None:
                        wins = [identify(slid, w) for w in vote_windows(clip, length)]
                        # The majority reading, and how much of the vote it took.
                        top = max(set(wins), key=wins.count)
                        heard.append((top, wins.count(top) / len(wins)))
                    else:
                        heard.append((identify(slid, cut), 1.0))
                wall = time.perf_counter() - t0
                key = f"{export}|{label}|{tag}"
                results[key] = {
                    "export": export,
                    "length": label,
                    "truth": tag,
                    "n": len(heard),
                    "rtf": wall / audio_s if audio_s else None,
                    "heard": heard,
                }
                correct = sum(1 for h, _ in heard if h == tag)
                as_ja = sum(1 for h, _ in heard if h == "ja")
                print(
                    f"{export:13s} {label:5s} {tag}: "
                    f"correct {correct / len(heard):6.1%}  "
                    f"heard-as-ja {as_ja / len(heard):6.1%}  "
                    f"RTF {results[key]['rtf']:.4f}",
                    flush=True,
                )
        del slid

    pathlib.Path(args.out).write_text(json.dumps(results, ensure_ascii=False))
    print(f"\nwrote {args.out}")

    # ---- the table, and the gate ------------------------------------------
    print("\n## LID accuracy (correct-language rate)\n")
    for export in args.exports.split(","):
        print(f"### {export}\n")
        print("| length | ja recall | de correct | en correct | de→ja | en→ja | RTF |")
        print("|--------|----------:|-----------:|-----------:|------:|------:|----:|")
        for length in LENGTHS:
            label = "full" if length is None else f"{length}s"

            def rate(tag, want):
                r = results.get(f"{export}|{label}|{tag}")
                if not r:
                    return float("nan")
                return sum(1 for h, _ in r["heard"] if h == want) / r["n"]

            rtf = results.get(f"{export}|{label}|ja", {}).get("rtf") or float("nan")
            print(
                f"| {label} | {rate('ja', 'ja'):.1%} | {rate('de', 'de'):.1%} | "
                f"{rate('en', 'en'):.1%} | {rate('de', 'ja'):.1%} | "
                f"{rate('en', 'ja'):.1%} | {rtf:.3f} |"
            )
        print()

    print("Gate for LIVE use: ja recall >= 90% at 3 s, de/en -> ja <= 1%, RTF <= 0.05")


if __name__ == "__main__":
    main()
