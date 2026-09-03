#!/usr/bin/env python3
"""Can the identifier hear Korean and Chinese too? (FINDINGS §27)

§22 measured whisper-tiny's spoken-language identifier on three languages —
ja, de, en — and shipped it on the strength of one column: **zero of 400**
German and English utterances were heard as Japanese, at any length. That
asymmetry is the whole reason the router is safe to run.

Extending the route to Korean and Chinese re-opens that column, and it re-opens
it in a harder place. de→ja was never a plausible confusion; **ja↔zh↔ko** is.
The three languages share a script (Han), a great deal of vocabulary, and — for
a 3 s fragment of read speech — most of the prosody a Whisper encoder has to go
on. So this bench asks two separate questions and reports them separately:

  1. the ORIGINAL gate, unchanged: does de/en leak into any of ja/ko/zh?
     Adding a target adds a way for a German turn to be stolen.
  2. the NEW question: how much do ja, ko and zh take from each other? A
     ja→zh confusion is not a stolen turn — the SenseVoice decoder speaks both
     and would write the right script anyway (§26) — but it IS a wrong
     `segments.lang`, and this is where it is counted honestly rather than
     hidden inside a recall number.

Reuses `lid_bench.py` wholesale: the same corpus loader, the same deterministic
seed, the same centre cut, the same 4 cores at nice 19. Only the language list
and the tables are new.

Usage:
  nx-scratch/venv/bin/python spike/lid_cjk.py --n 200
"""

import argparse
import json
import pathlib
import sys
import time

import sherpa_onnx

sys.path.insert(0, str(pathlib.Path(__file__).parent))
from lid_bench import (  # noqa: E402
    EXPORTS,
    SAMPLE_RATE,
    SCRATCH,
    centre_cut,
    identify,
    load_utterances,
    read_audio,
)

# The three targets a decoder would be catalogued for, and the two negatives
# whose turns a false positive would steal. de/en are also exactly what this
# user's evenings are.
LANGS = {
    "ja_jp": "ja",
    "ko_kr": "ko",
    "cmn_hans_cn": "zh",
    "de_de": "de",
    "en_us": "en",
}
TARGETS = ["ja", "ko", "zh"]
NEGATIVES = ["de", "en"]
LENGTHS = [None, 3.0]


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


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--n", type=int, default=200)
    ap.add_argument("--threads", type=int, default=4)
    ap.add_argument("--export", default="whisper-tiny")
    ap.add_argument("--out", default=str(SCRATCH / "lid_cjk.json"))
    args = ap.parse_args()

    corpus = {}
    for lang_dir, tag in LANGS.items():
        items = load_utterances(lang_dir, args.n)
        if len(items) < args.n:
            print(f"WARNING: only {len(items)} utterances for {tag}", file=sys.stderr)
        corpus[tag] = [read_audio(p) for p, _ in items]
        print(f"{tag}: {len(corpus[tag])} utterances", flush=True)

    slid = build(args.export, args.threads)
    results = {}
    for length in LENGTHS:
        label = "full" if length is None else f"{length}s"
        for tag, clips in corpus.items():
            heard = []
            audio_s = 0.0
            t0 = time.perf_counter()
            for clip in clips:
                cut = clip if length is None else centre_cut(clip, length)
                audio_s += len(cut) / SAMPLE_RATE
                heard.append(identify(slid, cut))
            wall = time.perf_counter() - t0
            results[f"{label}|{tag}"] = {
                "length": label,
                "truth": tag,
                "n": len(heard),
                "rtf": wall / audio_s if audio_s else None,
                "heard": heard,
            }
            correct = sum(1 for h in heard if h == tag) / len(heard)
            print(
                f"{label:5s} {tag}: correct {correct:6.1%}  RTF "
                f"{results[f'{label}|{tag}']['rtf']:.4f}",
                flush=True,
            )
    del slid

    pathlib.Path(args.out).write_text(json.dumps(results, ensure_ascii=False))
    print(f"\nwrote {args.out}\n")

    def rate(label, truth, want):
        r = results.get(f"{label}|{truth}")
        if not r:
            return float("nan")
        return sum(1 for h in r["heard"] if h == want) / r["n"]

    print(f"## {args.export} — recall per target\n")
    print("| length | ja recall | ko recall | zh recall | de correct | en correct | RTF |")
    print("|--------|----------:|----------:|----------:|-----------:|-----------:|----:|")
    for length in LENGTHS:
        label = "full" if length is None else f"{length}s"
        rtf = results[f"{label}|ja"]["rtf"]
        print(
            f"| {label} | {rate(label, 'ja', 'ja'):.1%} | {rate(label, 'ko', 'ko'):.1%} | "
            f"{rate(label, 'zh', 'zh'):.1%} | {rate(label, 'de', 'de'):.1%} | "
            f"{rate(label, 'en', 'en'):.1%} | {rtf:.3f} |"
        )

    print("\n## False positives INTO a target — the gate\n")
    print("| length | source | → ja | → ko | → zh | total into CJK |")
    print("|--------|--------|-----:|-----:|-----:|---------------:|")
    for length in LENGTHS:
        label = "full" if length is None else f"{length}s"
        for src in NEGATIVES:
            into = [rate(label, src, t) for t in TARGETS]
            print(
                f"| {label} | {src} | {into[0]:.1%} | {into[1]:.1%} | {into[2]:.1%} | "
                f"{sum(into):.1%} |"
            )

    print("\n## The confusion the targets have with EACH OTHER\n")
    print("| length | truth | → ja | → ko | → zh | → something else |")
    print("|--------|-------|-----:|-----:|-----:|-----------------:|")
    for length in LENGTHS:
        label = "full" if length is None else f"{length}s"
        for truth in TARGETS:
            row = [rate(label, truth, t) for t in TARGETS]
            print(
                f"| {label} | {truth} | {row[0]:.1%} | {row[1]:.1%} | {row[2]:.1%} | "
                f"{1.0 - sum(row):.1%} |"
            )

    print(
        "\nGate: de/en → any target <= 1% each, and ja/ko/zh recall >= 90% at 3 s.\n"
        "ja<->zh<->ko confusion is reported, not gated: one decoder that speaks all\n"
        "three writes the right script either way, so it costs a lang stamp and not\n"
        "a transcript."
    )


if __name__ == "__main__":
    main()
