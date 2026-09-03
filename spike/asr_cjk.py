#!/usr/bin/env python3
"""One decoder for ja, ko and zh — or three? (FINDINGS §27)

§23 shipped `sherpa-onnx-nemo-parakeet-tdt_ctc-0.6b-ja-35000-int8` for Japanese
and §24 wrote down what it does not cover: a Korean or Chinese speaker in the
same lobby gets *precisely* the failure that round fixed for Japanese — the
multilingual v3 does not speak either, does not say so, and transliterates.

There is no Korean Parakeet in the zoo (`…-0.6b-ko-3000-int8` is a 404), so
Korean is not a copy-paste of the Japanese entry. The candidate §24 named
instead is **SenseVoice-Small**, `sherpa-onnx-sense-voice-zh-en-ja-ko-yue-
2024-07-17`: zh + en + ja + ko + yue in one 239 MB int8 graph. It would
*replace* the Japanese decoder rather than sit beside it, which is why this
bench re-measures Japanese against §23's numbers rather than only measuring the
two new languages.

The decision rule, fixed before the run:

  (a) SenseVoice within **2 CER points** of the ja Parakeet on ja at 3 s AND
      ko/zh usable (CER <= 20% at 3 s) → ship ONE decoder for ja/ko/zh;
  (b) clearly worse on ja → keep the Parakeet, add SenseVoice for ko/zh only;
  (c) ko/zh not usable → report and ship nothing.

**The tags.** SenseVoice does not emit a transcript, it emits a transcript
wrapped in metadata: `<|ja|><|NEUTRAL|><|Speech|><|woitn|>すみません`. Language,
emotion, audio event, and whether inverse text normalisation ran. Every one of
them has to come off before anything reads the string, and the strip is
measured here (`--show`) rather than assumed, because a decoder whose output
begins with a Latin `<` would be read as English by the very text classifier
this feature exists to route around.

CER, not WER, for the same reason as §23 — ja and zh are written without spaces
— and Korean is scored the same way so the three rows are comparable to each
other and to §23's.

Usage:
  nx-scratch/venv/bin/python spike/asr_cjk.py --n 200
"""

import argparse
import json
import pathlib
import re
import sys
import time

import sherpa_onnx

sys.path.insert(0, str(pathlib.Path(__file__).parent))
from lid_bench import SAMPLE_RATE, SCRATCH, centre_cut, load_utterances, read_audio  # noqa: E402
from asr_ja import JA_CTC, build_ja_ctc, cer, decode_ctc, fmt, fragment_cer  # noqa: E402

SENSE_VOICE = (
    SCRATCH / "models-cjk/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17"
)

# FLEURS directory → the tag the daemon would stamp.
LANGS = {"ja_jp": "ja", "ko_kr": "ko", "cmn_hans_cn": "zh"}
LENGTHS = [None, 3.0]

# `<|ja|>`, `<|NEUTRAL|>`, `<|Speech|>`, `<|woitn|>`, `<|HAPPY|>`, `<|BGM|>` …
# Anchored to nothing: they are emitted as a prefix today, and a rule that
# assumes the position is a rule that breaks on the first release that appends
# an event tag at the end.
TAG = re.compile(r"<\|[^|>]*\|>")


def strip_tags(text: str) -> str:
    """Everything between `<|` and `|>`, gone, and the whitespace it leaves."""
    return TAG.sub("", text).strip()


def build_sense_voice(threads: int, language: str, use_itn: bool = False):
    model = SENSE_VOICE / "model.int8.onnx"
    tokens = SENSE_VOICE / "tokens.txt"
    for p in (model, tokens):
        if not p.is_file():
            sys.exit(f"missing {p}")
    return sherpa_onnx.OfflineRecognizer.from_sense_voice(
        model=str(model),
        tokens=str(tokens),
        num_threads=threads,
        provider="cpu",
        language=language,
        use_itn=use_itn,
        debug=False,
    )


def decode(rec, samples) -> str:
    stream = rec.create_stream()
    stream.accept_waveform(SAMPLE_RATE, samples)
    rec.decode_stream(stream)
    return stream.result.text.strip()


def score(clips, decode_one, length):
    """`(row, pairs)` for one decoder at one length over one language."""
    errs = chars = 0
    audio_s = wall = 0.0
    pairs = []
    for samples, ref in clips:
        cut = samples if length is None else centre_cut(samples, length)
        audio_s += len(cut) / SAMPLE_RATE
        t0 = time.perf_counter()
        raw = decode_one(cut)
        wall += time.perf_counter() - t0
        hyp = strip_tags(raw)
        # Full utterances are scored against the whole reference; a cut is
        # scored by infix alignment against the best-matching substring of it,
        # because the cut has no reference of its own (§23).
        e, c = cer(ref, hyp) if length is None else fragment_cer(ref, hyp)
        errs += e
        chars += c
        pairs.append({"ref": ref, "raw": raw, "hyp": hyp})
    return {
        "cer": errs / chars if chars else None,
        "rtf": wall / audio_s,
        "n": len(pairs),
        "empty_rate": sum(1 for p in pairs if not p["hyp"].strip()) / len(pairs),
        "tagged_rate": sum(1 for p in pairs if p["raw"] != p["hyp"]) / len(pairs),
        "pairs": pairs[:20],
    }, pairs


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--n", type=int, default=200)
    ap.add_argument("--threads", type=int, default=4)
    ap.add_argument("--langs", default="ja,ko,zh")
    ap.add_argument("--show", type=int, default=3, help="raw decodes to print per language")
    ap.add_argument("--out", default=str(SCRATCH / "asr_cjk.json"))
    args = ap.parse_args()

    want = args.langs.split(",")
    corpus = {}
    for lang_dir, tag in LANGS.items():
        if tag not in want:
            continue
        items = load_utterances(lang_dir, args.n)
        corpus[tag] = [(read_audio(p), text) for p, text in items]
        print(f"{tag}: {len(corpus[tag])} utterances", flush=True)

    results = {}

    # ---- SenseVoice, one language at a time --------------------------------
    #
    # `language=` is forced rather than left at "auto" because that is how the
    # daemon would run it: the identifier has already said which of ja/ko/zh
    # this turn is, and a decoder asked to guess again can disagree with the
    # route that reached it. ONE model in memory at a time, so it is rebuilt
    # per language rather than three recognizers held at once.
    for tag, clips in corpus.items():
        rec = build_sense_voice(args.threads, tag)
        for length in LENGTHS:
            label = "full" if length is None else f"{length}s"
            row, pairs = score(clips, lambda c: decode(rec, c), length)
            results[f"sense-voice-int8-{tag}|{label}"] = row
            print(
                f"sense-voice {tag} {label:5s}: CER {fmt(row['cer'])}  "
                f"RTF {row['rtf']:.3f}  empty {row['empty_rate']:.1%}  "
                f"tagged {row['tagged_rate']:.1%}",
                flush=True,
            )
            if length is None and args.show:
                for p in pairs[: args.show]:
                    print(f"    raw: {p['raw'][:90]}")
                    print(f"    out: {p['hyp'][:90]}")
        del rec

    # ---- the incumbent, on the same clips ----------------------------------
    #
    # Only ja: the Parakeet speaks nothing else, and running it on Korean would
    # measure the failure this whole round exists to remove.
    if "ja" in corpus and JA_CTC.is_dir():
        rec = build_ja_ctc(args.threads)
        for length in LENGTHS:
            label = "full" if length is None else f"{length}s"
            row, _ = score(corpus["ja"], lambda c: decode_ctc(rec, c), length)
            results[f"ja-parakeet-ctc-ja|{label}"] = row
            print(
                f"ja-parakeet {label:5s}: CER {fmt(row['cer'])}  RTF {row['rtf']:.3f}  "
                f"empty {row['empty_rate']:.1%}",
                flush=True,
            )
        del rec

    pathlib.Path(args.out).write_text(json.dumps(results, ensure_ascii=False, indent=1))
    print(f"\nwrote {args.out}\n")

    print("## The CJK decoder\n")
    print("| decoder | lang | length | CER | RTF | empty | tagged |")
    print("|---------|------|--------|----:|----:|------:|-------:|")
    for key, r in results.items():
        model, label = key.split("|")
        *name, lang = model.rsplit("-", 1)
        print(
            f"| {'-'.join(name)} | {lang} | {label} | {fmt(r['cer'])} | {r['rtf']:.3f} | "
            f"{r['empty_rate']:.1%} | {r['tagged_rate']:.1%} |"
        )

    # ---- the rule ----------------------------------------------------------
    def c(key):
        r = results.get(key)
        return None if not r else r["cer"]

    sv_ja, pk_ja = c("sense-voice-int8-ja|3.0s"), c("ja-parakeet-ctc-ja|3.0s")
    sv_ko, sv_zh = c("sense-voice-int8-ko|3.0s"), c("sense-voice-int8-zh|3.0s")
    print("\n## The rule\n")
    if None in (sv_ja, pk_ja, sv_ko, sv_zh):
        print("not every row was measured — no rule fires")
        return
    usable = sv_ko <= 0.20 and sv_zh <= 0.20
    within = sv_ja - pk_ja <= 0.02
    print(f"ja at 3 s: sense-voice {sv_ja:.1%} vs parakeet {pk_ja:.1%} "
          f"(delta {sv_ja - pk_ja:+.1%}, bar +2.0 points)")
    print(f"ko at 3 s: {sv_ko:.1%}   zh at 3 s: {sv_zh:.1%}   (bar 20.0%)")
    if not usable:
        print("(c) ko/zh are not usable — report and ship nothing")
    elif within:
        print("(a) ONE decoder for ja/ko/zh, replacing the ja Parakeet")
    else:
        print("(b) keep the ja Parakeet, add SenseVoice for ko/zh only")


if __name__ == "__main__":
    main()
