#!/usr/bin/env python3
"""Can the identifier hear a SHORT Romance turn? (FINDINGS §28, part one)

§22 measured whisper-tiny at full length, 3 s and 1.5 s, and shipped a 1.5 s
floor because that is `[lang].arbiter_min_duration_s` — the length below which
the German arbiter's replacement was measured to be no better than what it
would overwrite. The Japanese route inherited that floor without re-asking
whether it was the right one for a *language decision*, which is a different
question from a *replacement* decision: the arbiter floor is about whether new
words are better words, and LID is about whether we know what language it is.

The live rows that motivate this are all under it:

    "Mon petit chou."       1.01 s
    "During apartments."    1.20 s   (French)
    "Wanky Daska."          1.30 s   (Japanese, "genki desu ka")

None reached the identifier. All three are the transliteration failure §22
describes, and two of them are in a language §22 never measured — because a
French turn rendered as English words is invisible to `lang::classify` and to
`lang::guess_other` for exactly the same reason a Japanese one is.

So: whisper-tiny at 1.0 s and 1.25 s, on six languages rather than three.

THE GATE, and it is the same asymmetry §22 turned on: **de/en false positives
into any other language ≤ 1% at the new floor.** Recall is reported, not
gated — a missed turn stays as wrong as it is today, and a German turn handed
to a decoder forced to French is a new kind of wrong.

Usage:
  <venv>/bin/python spike/lid_poly.py --n 200
"""

import argparse
import json
import pathlib
import sys
import time

sys.path.insert(0, str(pathlib.Path(__file__).parent))
from lid_bench import (  # noqa: E402
    SAMPLE_RATE,
    build,
    centre_cut,
    identify,
    load_utterances,
    read_audio,
)

SCRATCH = pathlib.Path("/tmp/nx-recall-workspace/nx-scratch")

# Six languages. ja/de/en are §22's, carried forward so the new rows can be
# read against the old ones; fr/es/it are the ones this round is about and the
# ones the shipped `guess_other` already has stopword tables for.
LANGS = {
    "ja_jp": "ja",
    "fr_fr": "fr",
    "es_419": "es",
    "it_it": "it",
    "de_de": "de",
    "en_us": "en",
}
# The negatives: the two languages a false positive would steal a turn from.
NEGATIVES = ("de", "en")

# 1.5 s is §22's floor, kept as the anchor row so the two tables join up.
LENGTHS = [1.5, 1.25, 1.0]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--n", type=int, default=200)
    ap.add_argument("--threads", type=int, default=4)
    ap.add_argument("--export", default="whisper-tiny")
    ap.add_argument("--out", default=str(SCRATCH / "lid_poly.json"))
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
        label = f"{length}s"
        for tag, clips in corpus.items():
            heard, audio_s = [], 0.0
            t0 = time.perf_counter()
            for clip in clips:
                cut = centre_cut(clip, length)
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
            print(f"{label:6s} {tag}: correct {correct:6.1%}  RTF {wall / audio_s:.4f}", flush=True)

    pathlib.Path(args.out).write_text(json.dumps(results, ensure_ascii=False))
    print(f"\nwrote {args.out}")
    report(results, args.export)


def report(results, export):
    tags = list(LANGS.values())

    def rate(length, truth, want):
        r = results.get(f"{length}|{truth}")
        if not r:
            return float("nan")
        return sum(1 for h in r["heard"] if h == want) / r["n"]

    print(f"\n## {export} — recall per language\n")
    print("| length | " + " | ".join(tags) + " | RTF |")
    print("|--------|" + "----:|" * (len(tags) + 1))
    for length in LENGTHS:
        label = f"{length}s"
        rtf = results.get(f"{label}|ja", {}).get("rtf") or float("nan")
        cells = " | ".join(f"{rate(label, t, t):.1%}" for t in tags)
        print(f"| {label} | {cells} | {rtf:.3f} |")

    # The gate. Every de/en utterance heard as something that is not itself,
    # broken out per destination language, because "1% wrong" spread over five
    # languages and "1% wrong" all landing in French are different risks.
    print("\n## The gate — de/en heard as another language\n")
    others = [t for t in tags if t not in NEGATIVES]
    print("| length | source | " + " | ".join(others) + " | any |")
    print("|--------|--------|" + "----:|" * (len(others) + 1))
    worst = 0.0
    for length in LENGTHS:
        label = f"{length}s"
        for src in NEGATIVES:
            per = [rate(label, src, t) for t in others]
            r = results.get(f"{label}|{src}")
            any_other = (
                sum(1 for h in r["heard"] if h != src) / r["n"] if r else float("nan")
            )
            into_shipped = sum(per)
            worst = max(worst, into_shipped)
            cells = " | ".join(f"{p:.1%}" for p in per)
            print(f"| {label} | {src} | {cells} | {any_other:.1%} |")
    print(
        f"\nGate: de/en into a *routed* language <= 1% at the new floor. "
        f"Worst observed: {worst:.1%}"
    )

    # What the identifier says instead, when it is wrong about a positive.
    # Not gated — it is here because a French turn heard as Italian is a
    # different failure from one heard as English, and only the first one
    # produces a re-decode that the judge then has to throw away.
    print("\n## Where a missed positive goes\n")
    for length in LENGTHS:
        label = f"{length}s"
        for t in tags:
            r = results.get(f"{label}|{t}")
            if not r:
                continue
            wrong = [h for h in r["heard"] if h != t]
            if not wrong:
                continue
            top = sorted({w: wrong.count(w) for w in set(wrong)}.items(), key=lambda kv: -kv[1])
            said = ", ".join(f"{k} {v / r['n']:.1%}" for k, v in top[:4])
            print(f"* {label} {t}: {said}")


if __name__ == "__main__":
    main()
