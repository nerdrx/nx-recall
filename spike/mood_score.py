#!/usr/bin/env python3
"""Score SenseVoice's emotion and event tags against text proxies (FINDINGS §42).

Nobody here can listen to the clips, so both halves are scored against text, and
the proxies are stated rather than hidden:

  laughter — does the row's LIVE transcript or the NIGHT SHIFT's transcript
             carry a laughter token ("haha", "hehe", "lol", "[laughs]")?
  mood     — a small German/English sentiment lexicon over the same two texts.

Neither is ground truth. A person laughs without typing "haha", so RECALL
against this proxy is meaningless and only precision-shaped agreement is
reported, always beside the base rate — which is what says how much of the
agreement is chance.

The decision rules were fixed before the run:

  events: ship if agreement on tagged rows beats the base rate by >= 3x.
  mood:   ship if agreement with the lexicon beats chance (the majority-class
          rate) by >= 10 percentage points on rows where BOTH have an opinion.
"""

import json
import pathlib
import random
import re
import sys
from collections import Counter

IN = pathlib.Path(
    sys.argv[1] if len(sys.argv) > 1
    else "/tmp/nx-recall-workspace/nx-scratch/agents-2026-09-04/mood/mood_bench.json"
)

# The proxy for "somebody laughed", in the two languages this archive is in.
# Deliberately narrow: a single "ha", or "lol" inside a word, is noise.
LAUGH = re.compile(
    r"(?:^|\W)(?:h[ae](?:h[ae])+h?|hi(?:hi)+|lol+|lmao|rofl|xd|\[laugh\w*\]|\*laugh\w*\*)(?:\W|$)",
    re.I,
)

POS = set("""
good great nice cool awesome amazing love loved lovely happy fun funny yes yeah
thanks thank perfect best beautiful wow haha lol excited glad enjoy enjoyed
sweet wonderful excellent right true win won congrats hype based
gut super toll schoen schön geil klasse prima danke lieb liebe freude freut
lustig witzig spass spaß perfekt ja richtig gluecklich glücklich schaffen
""".split())

NEG = set("""
bad worse worst hate hated angry annoying annoyed sad sorry awful terrible
stupid shit fuck fucking damn ugh never broken bug crash fail failed problem
wrong tired sick pain hurt boring cringe dead lag laggy
schlecht schlimm hasse boese böse aerger ärger nervt nervig traurig leider
scheisse scheiße verdammt dumm bloed blöd kaputt fehler falsch muede müde
krank schmerz langweilig nein
""".split())


def lex(text):
    """+1 / -1 / 0 for a text."""
    words = re.findall(r"[\w']+", (text or "").lower())
    p = sum(1 for w in words if w in POS)
    n = sum(1 for w in words if w in NEG)
    return 1 if p > n else -1 if n > p else 0


def both(c):
    return f"{c['live']} {c['night']}".strip()


def pct(a, b):
    return 0.0 if not b else 100.0 * a / b


def main():
    d = json.loads(IN.read_text())
    clips = d["clips"]
    print(f"# corpus\n{len(clips)} clips, {d['audio_s']:.0f}s of audio, "
          f"RTF {d['rtf']} on {d['threads']} niced cores "
          f"({d['wall_s']:.0f}s wall)\n")

    # ---- what the model says at all ------------------------------------
    print("# what the model says")
    moods = Counter(c["mood"] for c in clips)
    events = Counter(c["event"] for c in clips)
    for k, v in moods.most_common():
        print(f"  emotion {str(k):16s} {v:6d}  {pct(v, len(clips)):5.1f}%")
    for k, v in events.most_common():
        print(f"  event   {str(k):16s} {v:6d}  {pct(v, len(clips)):5.1f}%")
    lp = Counter(c["n_logprobs"] for c in clips)
    print(f"  ys_log_probs lengths: {dict(lp)}  "
          f"(any non-zero would be a per-tag confidence; there is none)")
    print()

    # ---- laughter ------------------------------------------------------
    print("# laughter, against the transcript's own laughter tokens")
    tagged = [c for c in clips if c["event"] == "Laughter"]
    plain = [c for c in clips if c["event"] != "Laughter"]
    base = sum(1 for c in clips if LAUGH.search(both(c)))
    base_rate = pct(base, len(clips))
    hit_t = sum(1 for c in tagged if LAUGH.search(both(c)))
    hit_p = sum(1 for c in plain if LAUGH.search(both(c)))
    print(f"  base rate over the whole corpus       {base:5d}/{len(clips):5d}  {base_rate:5.2f}%")
    print(f"  rows the model tags LAUGHTER          {hit_t:5d}/{len(tagged):5d}  {pct(hit_t, len(tagged)):5.2f}%")
    print(f"  rows it does not                      {hit_p:5d}/{len(plain):5d}  {pct(hit_p, len(plain)):5.2f}%")
    lift = pct(hit_t, len(tagged)) / base_rate if base_rate else float("inf")
    print(f"  lift over the base rate               {lift:5.2f}x   (gate: >= 3.00x)")
    # The gate is only meaningful if the proxy fires often enough to separate
    # anything. On this archive it does not — twelve positives in fifteen
    # thousand — so the lift is computed from a handful of rows and printing a
    # verdict off it would be printing noise with a decimal point on it. Say so
    # rather than passing. `mood_score2.py` is the proxy that has signal.
    if base < 30 or hit_t < 10:
        print("  VERDICT: NO MEASUREMENT — this proxy has too few positives to")
        print("           separate anything. See mood_score2.py and FINDINGS 42.4.\n")
    else:
        print(f"  VERDICT: {'SHIP' if lift >= 3.0 else 'DO NOT SHIP'}\n")

    # The hand-checkable sample the brief asked for: 60 of each.
    rng = random.Random(7)
    s_t = rng.sample(tagged, min(60, len(tagged)))
    s_p = rng.sample(plain, min(60, len(plain)))
    st = sum(1 for c in s_t if LAUGH.search(both(c)))
    sp = sum(1 for c in s_p if LAUGH.search(both(c)))
    print(f"  the 60/60 hand sample: {st}/{len(s_t)} tagged carry a laughter token, "
          f"{sp}/{len(s_p)} untagged do\n")
    print("  five rows the model tagged LAUGHTER, verbatim:")
    for c in s_t[:5]:
        print(f"    [{c['id']}] {c['dur']:.1f}s  {(c['live'] or '(no words)')[:70]!r}")
    print()

    # ---- music ---------------------------------------------------------
    bgm = [c for c in clips if c["event"] == "BGM"]
    print(f"# music: {len(bgm)} rows ({pct(len(bgm), len(clips)):.2f}%) tagged BGM\n")

    # ---- mood ----------------------------------------------------------
    print("# mood, against a sentiment lexicon over the same two texts")
    named = [c for c in clips if c["mood"] in ("HAPPY", "SAD", "ANGRY", "NEUTRAL")]
    print(f"  the model names a mood on            {len(named):5d}/{len(clips):5d}  "
          f"{pct(len(named), len(clips)):5.1f}%  "
          f"(it declines on {pct(len(clips) - len(named), len(clips)):.1f}%)")

    # Only rows where BOTH have an opinion — the lexicon is silent on most
    # short turns, and scoring its silence as a disagreement would be scoring
    # the lexicon rather than the model.
    pairs = []
    for c in named:
        want = lex(both(c))
        if want == 0:
            continue
        got = 1 if c["mood"] == "HAPPY" else -1 if c["mood"] in ("SAD", "ANGRY") else 0
        if got == 0:
            # NEUTRAL against a lexicon that has an opinion is a disagreement,
            # and counting it is the honest choice: the model had a chance to
            # say something and said "no feeling" where the words carry one.
            got = 0
        pairs.append((got, want))
    if not pairs:
        print("  no rows where both have an opinion; nothing to score.")
        return
    agree = sum(1 for g, w in pairs if g == w)
    # Chance = the majority class of the PROXY, which is what a constant
    # predictor would score. That is the bar, not 50%.
    maj = max(Counter(w for _, w in pairs).values())
    print(f"  rows where both have an opinion      {len(pairs):5d}")
    print(f"  the tag agrees with the lexicon      {agree:5d}  {pct(agree, len(pairs)):5.1f}%")
    print(f"  a constant predictor would score     {maj:5d}  {pct(maj, len(pairs)):5.1f}%  (chance)")
    margin = pct(agree, len(pairs)) - pct(maj, len(pairs))
    print(f"  margin over chance                   {margin:+5.1f} points   (gate: >= +10.0)")
    print(f"  VERDICT: {'SHIP' if margin >= 10.0 else 'DO NOT SHIP'}\n")

    # And the confusion, so the failure has a shape rather than a number.
    print("  confusion (model tag x lexicon):")
    conf = Counter((g, w) for g, w in pairs)
    for g in (1, 0, -1):
        row = "  ".join(f"{conf.get((g, w), 0):5d}" for w in (1, 0, -1))
        name = {1: "happy", 0: "neutral", -1: "sad/angry"}[g]
        print(f"    {name:10s} {row}      (lexicon: pos  neu  neg)")


if __name__ == "__main__":
    main()
