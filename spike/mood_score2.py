#!/usr/bin/env python3
"""The second proxy for laughter: laughter is NOT SPEECH (FINDINGS §42).

The pre-registered proxy — "does the transcript carry a laughter token" — turned
out to have no signal on this corpus: 12 positives in 15,366 rows. A proxy that
fires twelve times cannot separate anything, and reporting a lift computed from
one hit would be reporting noise with a decimal point on it.

So a second proxy, stated as plainly as the first and chosen because it does not
depend on anybody typing "haha":

  **A laughter clip is a clip the speech decoder had nothing to say about.**

Laughter is not words. If the event head is finding real laughter, the rows it
tags should be far more likely to come back with no transcript, or with a bare
filler, than rows it does not tag. That is checkable without listening, it has
thousands of positives on both sides, and it is falsifiable: if the tag were
noise, the two rates would be equal.

It is not ground truth either, and it is biased in one direction worth naming:
a short clip is both more likely to be empty AND more likely to be a laugh, so
duration is a confounder. It is controlled by scoring within a duration band.
"""

import json
import pathlib
import re
import sys
from collections import Counter

IN = pathlib.Path(
    sys.argv[1] if len(sys.argv) > 1
    else "/tmp/nx-recall-workspace/nx-scratch/agents-2026-09-04/mood/mood_bench.json"
)

# What "the decoder had nothing to say" looks like: no text at all, or one of
# the back-channels `lang::FILLERS` already treats as no content.
FILLER = re.compile(
    r"^\W*(?:m+|mm+|mhm+|hm+|uh+|uhm+|um+|äh+|ähm+|ah+|oh+|eh+|ja|yeah|yep|ok|okay|so|no|nein|"
    r"yes|right|mm-hmm|mhmm|huh|hah?|na|nah)\W*$",
    re.I,
)


def wordless(c):
    t = (c["live"] or "").strip()
    return not t or bool(FILLER.match(t))


def pct(a, b):
    return 0.0 if not b else 100.0 * a / b


def band(c):
    d = c["dur"]
    if d < 1.5:
        return "1.0-1.5s"
    if d < 2.5:
        return "1.5-2.5s"
    if d < 5.0:
        return "2.5-5.0s"
    return "5.0s+"


def main():
    clips = json.loads(IN.read_text())["clips"]
    tagged = [c for c in clips if c["event"] == "Laughter"]
    plain = [c for c in clips if c["event"] not in ("Laughter",)]

    print("# proxy 2: a laughter clip is a clip the decoder had no words for\n")
    a = sum(1 for c in tagged if wordless(c))
    b = sum(1 for c in plain if wordless(c))
    print(f"  rows the model tags LAUGHTER, wordless   {a:5d}/{len(tagged):5d}  {pct(a, len(tagged)):5.1f}%")
    print(f"  rows it does not, wordless               {b:5d}/{len(plain):5d}  {pct(b, len(plain)):5.1f}%")
    lift = pct(a, len(tagged)) / pct(b, len(plain)) if b else float("inf")
    print(f"  lift                                     {lift:5.2f}x\n")

    # Duration is the confounder: short clips are both likelier to be empty and
    # likelier to be a laugh. Score inside a band and the confound is gone.
    print("  …and within a duration band, which is where the confound lives:")
    print(f"  {'band':10s} {'tagged':>18s} {'untagged':>18s}   lift")
    for name in ("1.0-1.5s", "1.5-2.5s", "2.5-5.0s", "5.0s+"):
        t = [c for c in tagged if band(c) == name]
        p = [c for c in plain if band(c) == name]
        if not t or not p:
            continue
        ta = sum(1 for c in t if wordless(c))
        pa = sum(1 for c in p if wordless(c))
        lt, lp = pct(ta, len(t)), pct(pa, len(p))
        l = lt / lp if lp else float("inf")
        print(f"  {name:10s} {ta:6d}/{len(t):<5d} {lt:5.1f}%  {pa:6d}/{len(p):<5d} {lp:5.1f}%   {l:5.2f}x")

    print("\n  the durations the tag lands on:")
    print(f"    tagged   {Counter(band(c) for c in tagged).most_common()}")
    print(f"    untagged {Counter(band(c) for c in plain).most_common()}")

    print("\n# music (BGM), the same way — a music clip should also carry few words")
    bgm = [c for c in clips if c["event"] == "BGM"]
    sing = [c for c in clips if c["event"] == "Sing"]
    if bgm:
        a = sum(1 for c in bgm if wordless(c))
        print(f"  BGM rows, wordless   {a:4d}/{len(bgm):4d}  {pct(a, len(bgm)):5.1f}%   "
              f"(vs {pct(b, len(plain)):5.1f}% baseline)")
    if sing:
        a = sum(1 for c in sing if wordless(c))
        print(f"  Sing rows, wordless  {a:4d}/{len(sing):4d}  {pct(a, len(sing)):5.1f}%")

    print("\n# twenty rows the model tagged LAUGHTER, verbatim, longest first")
    for c in sorted(tagged, key=lambda c: -c["dur"])[:10]:
        print(f"    {c['dur']:5.1f}s  {(c['live'] or '(no words)')[:64]!r}")
    print("  …and the shortest:")
    for c in sorted(tagged, key=lambda c: c["dur"])[:10]:
        print(f"    {c['dur']:5.1f}s  {(c['live'] or '(no words)')[:64]!r}")


if __name__ == "__main__":
    main()
