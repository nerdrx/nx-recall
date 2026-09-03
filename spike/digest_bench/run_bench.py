"""The daily digest's gate: will a 3B refuse to summarise eight turns of "ja"?

Same shape, same discipline and the same binary as `spike/graph_bench` — four
pinned cores at nice 19, `--temp 0`, grammar-constrained decoding — because the
question is the same one the commitment bake-off asked and lost twice before the
verdict-first fix: **does the schema let the model say no before it has to fill
anything in?**

A digest is the one Tier 3 output nobody asked for. A commitment is surfaced
because somebody promised something; a topic label is three words. A paragraph
about last night's conversation is a paragraph the daemon volunteered, and the
failure mode is not a wrong summary — it is a summary of nothing, one per
thread, every morning. Hence the traps: six windows that ARE conversations by
the threading rule (eight turns, two voices, inside the gap) and are not worth a
sentence.

GATE: 6/6 traps refused and 4/4 positives summarised. The language and the
open list are reported, not gated: they are presentation, and the refusal is
the thing that decides whether this feature is worth having at all.

    taskset -c 16-31 nice -n 19 python3 spike/digest_bench/run_bench.py

0.11.6 — **whose name is in the paragraph.** The summary came back saying
"A und B reden über…", which is not what a person calls their friends. Two
designs, and this bench is what picked between them:

  --design a   prompt the model with LETTERS exactly as before, then
               substitute letter -> roster label in the text afterwards, with
               a rule that cannot misfire on an English article ("A meetup at
               eight"): only letters that were actually assigned, only as
               standalone tokens, and in English never a sentence-initial "A"
               followed by a lowercase word.
  --design b   prompt the summary call with the LABELS themselves and decode
               `people` by exact label match.

The VERDICT call is identical in both — same prompt, same letters, same
grammar — so the six traps cannot regress by construction, and the run proves
it rather than assuming it. Two new numbers per design: how many summaries
name **every** participant by their label (target 4/4) and how many contain a
name nobody in the room has (must be 0).
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).parent
S = Path("/tmp/claude-1000/-run-media-nerdrx-Lex-claude/61962651-5c6e-4a8f-93fe-d58f323a2b89/scratchpad/llm")
CLI = S / "llama-b10736" / "llama-cli"
MODEL = Path.home() / ".local/share/nx-recall/models/qwen2.5-3b-instruct-q4_k_m.gguf"

# Byte for byte what `crates/recalld/src/digest.rs` ships as DIGEST_SYSTEM. The
# test at the bottom of that file asserts the two are the same string, for the
# same reason the commitment prompt has one: editing it is editing a measured
# result.
# ---------------------------------------------------------------------------
# Two calls, and the reason is the whole finding of this bench.
#
# One call that decided AND wrote degraded monotonically as the prompt grew:
#
#   short prompt, no language steering   6/6 traps   1/4 language   1/4 open
#   + open clause + language order       5/6         2/4            4/4
#   + a worked trap example              1/6         3/4            4/4
#
# Every clause that made the summary better made the refusal worse, on the same
# ten cases and the same model. That is the verdict-first finding
# (`crate::llm`) with the knife turned the other way: the verdict is cheap to
# lose, and everything competing for the model's attention is what loses it.
#
# So the verdict gets a prompt of its own with nothing else in it, and the
# summary — which only runs for a conversation that passed — gets all the
# steering it wants. Traps cost ONE short call instead of one long one, and
# traps are most conversations, so this is also cheaper.
# ---------------------------------------------------------------------------

# The prompts are NOT restated here. They are exported from the Rust constants
# by `digest::tests::the_bench_runs_the_prompts_this_daemon_ships`, which fails
# if these files and the shipped strings differ — so a bench run is a
# measurement of what ships, and cannot quietly become a measurement of
# something else.
VERDICT_SYS = (HERE / "verdict.system.txt").read_text()

# `summary.{de,en}.txt` is the SHIPPED summary prompt — design (b), the one
# that names people — exported from the Rust constants and asserted equal to
# them. `letters.{de,en}.txt` is design (a)'s: the prompt this bench measured
# up to 0.11.5, kept as a static file so `--design a` stays reproducible. It is
# not exported and not shipped, because design (a) lost.
SUMMARY_SYS = {
    "a": {tag: (HERE / f"letters.{tag}.txt").read_text() for tag in ("de", "en")},
    "b": {tag: (HERE / f"summary.{tag}.txt").read_text() for tag in ("de", "en")},
}


# The bench's own worked example uses names that appear in no case's roster, so
# "the model copied a name out of the prompt" is a countable event rather than
# a coincidence.
PROMPT_NAMES = {"Nadia", "Timo"}

# Four pinned cores. Which four is a fact about the machine and not about the
# measurement, so it is an override rather than an edit.
CORES = os.environ.get("NXR_BENCH_CORES", "20-23")


# The language a digest is written in, as the order names it. `mixed` is a real
# thread state (`crate::langctx` gives a bilingual room no context at all) and
# the daemon resolves it the way this does: the reader's own language.
LANGS = {"de": "German", "en": "English", "mixed": "German"}

VERDICT_TOKENS = 24
MAX_TOKENS = 320

# Words that only exist in one of the two languages, for the "did it answer in
# the dialogue's language" check. Deliberately crude: the question is whether a
# German conversation came back in English, not which dialect it is.
DE = re.compile(r"\b(der|die|das|und|nicht|ist|sich|dass|noch|schick\w*|wird|"
                r"haben|hat|über|ein|eine|einen|mit|von|für|auf)\b", re.I)
EN = re.compile(r"\b(the|and|is|of|to|that|they|with|about|will|have|has|for|"
                r"a|an|discuss\w*|talk\w*|ask\w*)\b", re.I)


# ---------------------------------------------------------------------------
# design (a): letters out of the model, labels into the text
#
# The Python mirror of `digest::render_letters`. Kept here rather than shelled
# out to the daemon because the bench has to be runnable without a build, and
# the Rust test `a_letter_becomes_a_name_only_where_it_is_certainly_a_speaker`
# holds the same cases on the same rule.
# ---------------------------------------------------------------------------
def _wordish(c: str) -> bool:
    return c.isalnum() or c == "_"


def _english_article(text: str, i: int) -> bool:
    """Sentence-initial "A" followed by a lowercase word — "A meetup at eight".

    The only shape an English article can take that also looks like a speaker
    letter, and the reason design (a) is conservative in English: it declines
    "A asked for the recording" too.
    """
    j = i - 1
    while j >= 0 and text[j] in " \t":
        j -= 1
    initial = j < 0 or text[j] in ".!?\n"
    k = i + 1
    while k < len(text) and text[k] in " \t\n":
        k += 1
    lower = k < len(text) and text[k].isalpha() and text[k].islower()
    return initial and lower


def render_letters(text: str, tag: str, labels: list[str]) -> str:
    out: list[str] = []
    i, n = 0, len(text)
    while i < n:
        c = text[i]
        idx = ord(c) - ord("A") if "A" <= c <= "Z" else -1
        if 0 <= idx < len(labels):
            prev = text[i - 1] if i else ""
            nxt = text[i + 1] if i + 1 < n else ""
            standalone = not _wordish(prev) and not _wordish(nxt)
            article = tag == "en" and c == "A" and _english_article(text, i)
            if standalone and not article:
                out.append(labels[idx])
                i += 1
                continue
        out.append(c)
        i += 1
    return "".join(out)


def _call(system: str, prompt: str, grammar: str, tokens: int):
    cmd = ["chrt", "-i", "0", "taskset", "-c", CORES, "nice", "-n", "19",
           str(CLI), "-m", str(MODEL), "-t", "4", "--temp", "0",
           "-n", str(tokens), "--single-turn",
           "--grammar-file", str(HERE / grammar),
           "-sys", system, "-p", prompt,
           "--no-display-prompt", "--no-warmup", "-ngl", "0"]
    t0 = time.time()
    r = subprocess.run(cmd, capture_output=True, text=True, timeout=900,
                       env={"LD_LIBRARY_PATH": str(CLI.parent), "PATH": "/usr/bin:/bin"})
    dt = time.time() - t0
    m = re.search(r"\{.*\}", r.stdout, re.S)
    if not m:
        return None, dt, r.stdout[-200:]
    try:
        return json.loads(m.group(0)), dt, m.group(0)
    except json.JSONDecodeError:
        return None, dt, m.group(0)[:200]


def run_case(case, design: str) -> tuple[dict | None, float, str]:
    """Verdict first, in its own call; the summary only if it passed.

    The verdict call is the SAME call in both designs — same letters, same
    prompt, same grammar. Only the summary call differs, which is why the trap
    numbers cannot move and are reported anyway.
    """
    window, lang = case["window"], case["lang"]
    names = case.get("names", {})
    letters = sorted({t["who"] for t in window})
    labels = [names.get(w, w) for w in letters]

    dialogue = "\n".join(f'{t["who"]}: {t["text"]}' for t in window)
    verdict, dt, raw = _call(VERDICT_SYS, dialogue, "verdict.gbnf", VERDICT_TOKENS)
    if verdict is None:
        return None, dt, raw
    if verdict.get("worth_summarising") is not True:
        return {"worth_summarising": False}, dt, raw

    tag = "de" if LANGS[lang] == "German" else "en"
    if design == "b":
        named = "\n".join(f'{names.get(t["who"], t["who"])}: {t["text"]}' for t in window)
        body, dt2, raw2 = _call(SUMMARY_SYS["b"][tag], named, "summary.gbnf", MAX_TOKENS)
    else:
        body, dt2, raw2 = _call(SUMMARY_SYS["a"][tag], dialogue, "summary.gbnf", MAX_TOKENS)
    if body is None:
        # A verdict of yes with nothing behind it is not a digest.
        return None, dt + dt2, raw2

    out = {"worth_summarising": True, **body}
    out["summary_raw"] = out.get("summary", "") or ""
    if design == "a":
        out["summary"] = render_letters(out["summary_raw"], tag, labels)
        out["open"] = [render_letters(o, tag, labels) for o in (out.get("open") or [])]
        out["people"] = [
            labels[ord(p.strip()[:1].upper()) - ord("A")]
            for p in (out.get("people") or [])
            if p.strip() and 0 <= ord(p.strip()[:1].upper()) - ord("A") < len(labels)
        ]
    else:
        # Design (b) decodes `people` by exact label match: anything else is a
        # name the model made up, and it is dropped rather than guessed at.
        out["people"] = [p for p in (out.get("people") or []) if p.strip() in labels]
    out["_labels"] = labels
    return out, dt + dt2, raw2


def naming(summary: str, labels: list[str], all_labels: set[str]) -> tuple[bool, list[str]]:
    """(does it name everyone, what did it invent)."""
    covered = all(lab in summary for lab in labels)
    strangers = sorted(
        n
        for n in (PROMPT_NAMES | all_labels) - set(labels)
        if re.search(rf"(?<![\w]){re.escape(n)}(?![\w])", summary)
    )
    # A letter still standing where a person's name belongs is the bug this
    # round exists to fix, so it counts as a miss too.
    leftover = [
        lab
        for lab, letter in zip(labels, "ABCDEFGHIJKLMNOPQRSTUVWXYZ")
        if re.search(rf"(?<![\w]){letter}(?![\w])", summary)
    ]
    return covered and not leftover, strangers


def language_of(text: str) -> str:
    de, en = len(DE.findall(text)), len(EN.findall(text))
    if de == en:
        return "?"
    return "de" if de > en else "en"


def main() -> int:
    design = "a"
    for i, a in enumerate(sys.argv[1:]):
        if a == "--design":
            design = sys.argv[i + 2]
        elif a.startswith("--design="):
            design = a.split("=", 1)[1]
    if design not in ("a", "b"):
        print("--design a|b")
        return 2
    gold = json.loads((HERE / "cases.json").read_text())["cases"]
    if not CLI.is_file() or not MODEL.is_file():
        print(f"missing {CLI} or {MODEL}")
        return 2
    all_labels = {n for c in gold for n in c.get("names", {}).values()}

    traps_ok = traps_n = pos_ok = pos_n = lang_ok = lang_n = 0
    open_ok = open_n = parse_fail = 0
    named_ok = named_n = invented = 0
    times: list[float] = []
    print(
        f"=== digest verdict, qwen2.5-3b q4_k_m, 4 pinned cores ({CORES}), nice 19 "
        f"— design ({design}) ===\n"
    )
    for c in gold:
        out, dt, raw = run_case(c, design)
        times.append(dt)
        if out is None:
            parse_fail += 1
        got = bool(out and out.get("worth_summarising") is True)
        want = bool(c["expect"])
        verdict = "OK " if got == want else "MISS"
        if not want:
            traps_n += 1
            traps_ok += got == want
            print(f"  {verdict} trap     {c['id']:<26} worth={got}  {dt:.1f}s"
                  + (f"\n        got: {raw[:160]}" if got else ""))
        else:
            pos_n += 1
            pos_ok += got == want
            summary = (out or {}).get("summary", "") or ""
            people = (out or {}).get("people", []) or []
            openl = (out or {}).get("open", []) or []
            line = (f"  {verdict} positive {c['id']:<26} worth={got}  {dt:.1f}s")
            if got:
                lang_n += 1
                spoke = language_of(summary)
                want_lang = "de" if LANGS[c["lang"]] == "German" else "en"
                hit = spoke == want_lang
                lang_ok += hit
                if c.get("open_expected") is not None:
                    open_n += 1
                    open_ok += bool(openl) == bool(c["open_expected"])
                line += f"  lang={spoke}{'' if hit else ' <-- WRONG LANGUAGE'}"
                line += f"  people={people}  open={len(openl)}"
                labels = (out or {}).get("_labels", [])
                named_n += 1
                covered, strangers = naming(summary, labels, all_labels)
                named_ok += covered
                invented += len(strangers)
                line += f"\n        names={'ALL' if covered else 'MISSING'}"
                if strangers:
                    line += f"  INVENTED={strangers}"
                line += f"\n        {summary[:260]}"
                line += f"\n        raw: {(out or {}).get('summary_raw', '')[:180]}"
                if openl:
                    line += f"\n        open: {openl}"
            print(line)

    n = len(gold)
    print(f"\n  traps refused    : {traps_ok}/{traps_n}   <- the gate")
    print(f"  positives summed : {pos_ok}/{pos_n}")
    print(f"  right language   : {lang_ok}/{lang_n or 1}")
    print(f"  open list right  : {open_ok}/{open_n or 1}")
    print(f"  everyone named   : {named_ok}/{named_n or 1}   <- 0.11.6, target 4/4")
    print(f"  invented names   : {invented}          <- 0.11.6, must be 0")
    print(f"  parse failures   : {parse_fail}")
    print(f"  sec/case         : median {sorted(times)[n//2]:.1f}  max {max(times):.1f}")
    return 0 if (traps_ok == traps_n and pos_ok == pos_n) else 1


if __name__ == "__main__":
    raise SystemExit(main())
