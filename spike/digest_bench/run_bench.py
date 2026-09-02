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
"""

from __future__ import annotations

import json
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
SUMMARY_SYS = {
    "de": (HERE / "summary.de.txt").read_text(),
    "en": (HERE / "summary.en.txt").read_text(),
}


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


def _call(system: str, prompt: str, grammar: str, tokens: int):
    cmd = ["chrt", "-i", "0", "taskset", "-c", "20-23", "nice", "-n", "19",
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


def run_case(window, lang: str) -> tuple[dict | None, float, str]:
    """Verdict first, in its own call; the summary only if it passed."""
    dialogue = "\n".join(f'{t["who"]}: {t["text"]}' for t in window)
    verdict, dt, raw = _call(VERDICT_SYS, dialogue, "verdict.gbnf", VERDICT_TOKENS)
    if verdict is None:
        return None, dt, raw
    if verdict.get("worth_summarising") is not True:
        return {"worth_summarising": False}, dt, raw

    tag = "de" if LANGS[lang] == "German" else "en"
    body, dt2, raw2 = _call(SUMMARY_SYS[tag], dialogue, "summary.gbnf", MAX_TOKENS)
    if body is None:
        # A verdict of yes with nothing behind it is not a digest.
        return None, dt + dt2, raw2
    return {"worth_summarising": True, **body}, dt + dt2, raw2


def language_of(text: str) -> str:
    de, en = len(DE.findall(text)), len(EN.findall(text))
    if de == en:
        return "?"
    return "de" if de > en else "en"


def main() -> int:
    gold = json.loads((HERE / "cases.json").read_text())["cases"]
    if not CLI.is_file() or not MODEL.is_file():
        print(f"missing {CLI} or {MODEL}")
        return 2

    traps_ok = traps_n = pos_ok = pos_n = lang_ok = lang_n = 0
    open_ok = open_n = parse_fail = 0
    times: list[float] = []
    print(f"=== digest verdict, qwen2.5-3b q4_k_m, 4 pinned cores, nice 19 ===\n")
    for c in gold:
        out, dt, raw = run_case(c["window"], c["lang"])
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
                line += f"\n        {summary[:220]}"
            print(line)

    n = len(gold)
    print(f"\n  traps refused    : {traps_ok}/{traps_n}   <- the gate")
    print(f"  positives summed : {pos_ok}/{pos_n}")
    print(f"  right language   : {lang_ok}/{lang_n or 1}")
    print(f"  open list right  : {open_ok}/{open_n or 1}")
    print(f"  parse failures   : {parse_fail}")
    print(f"  sec/case         : median {sorted(times)[n//2]:.1f}  max {max(times):.1f}")
    return 0 if (traps_ok == traps_n and pos_ok == pos_n) else 1


if __name__ == "__main__":
    raise SystemExit(main())
