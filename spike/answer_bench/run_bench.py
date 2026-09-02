"""Grounded answers: will a 3B refuse to answer a question the archive does not?

Same shape, same discipline and the same binary as `spike/digest_bench` — four
pinned cores at nice 19, `--temp 0`, grammar-constrained decoding, verdict in
its own call — because the question is the same one every Tier 3 feature in
this project has had to answer: **does the schema let the model say no before
it has to fill anything in?**

Here it matters more than anywhere else. A wrong digest is a paragraph nobody
reads. A wrong ANSWER is the app looking a person in the eye and telling them
something that was never said, with a citation under it. The citation is what
makes it dangerous: it is a claim of evidence, and a claim of evidence is
believed.

So the traps are the gate. Twelve of them, in the shapes an archive actually
produces: the subject is all over the hits and the answer is in none of them;
the answer is one row outside k; the question names somebody who never spoke;
the answer would need the world; an intention read as a fact; a question in the
other language; and two with no hits at all, which are refused before the model
is started.

GATE: >= 11/12 traps refused AND >= 9/12 answerable answered with every
citation correct. A cited row that does not support the sentence counts as
wrong, which is why `must_cite` is in the case file and is checked.

    chrt -i 0 taskset -c 28-31 nice -n 19 python3 spike/answer_bench/run_bench.py

The prompts, the grammars and the stopword list are NOT restated here. They are
exported from the Rust constants by
`answer::tests::the_bench_runs_the_prompts_this_daemon_ships`, which fails if
these files and the shipped strings differ — so a bench run is a measurement of
what ships, and cannot quietly become a measurement of something else.
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
import time
import unicodedata
from pathlib import Path

HERE = Path(__file__).parent
CLI = Path.home() / ".local/share/nx-recall/models/llama/llama-cli"
MODEL = Path.home() / ".local/share/nx-recall/models/qwen2.5-3b-instruct-q4_k_m.gguf"

VERDICT_SYS = (HERE / "answerable.system.txt").read_text()
VERDICT_CHECK = (HERE / "answerable.check.txt").read_text()
ANSWER_SYS = {
    "de": (HERE / "answer.de.txt").read_text(),
    "en": (HERE / "answer.en.txt").read_text(),
}
ANSWER_TMPL = (HERE / "answer.gbnf.tmpl").read_text()
SCAFFOLDING = set((HERE / "scaffolding.txt").read_text().split())

VERDICT_TOKENS = 24
ANSWER_TOKENS = 200
MIN_OVERLAP = 2
MAX_ROW_CHARS = 320

# The transcript, flattened to id -> row, exactly as `crate::answer::Row`.
DOC = json.loads((HERE / "transcript.json").read_text())
ROWS = {
    t[0]: {"id": t[0], "clock": t[1], "who": t[2], "text": t[3]}
    for c in DOC["conversations"]
    for t in c["turns"]
}


def rows_text(ids: list[int]) -> str:
    """`crate::answer::rows_text`. The 6 000-character budget is not exercised
    by any case here — four rows is a few hundred characters — so it is not
    reimplemented; the Rust test covers the clipping."""
    out = ""
    for i in ids:
        r = ROWS[i]
        text = r["text"].strip()
        if len(text) > MAX_ROW_CHARS:
            text = text[:MAX_ROW_CHARS] + "…"
        out += f'[{r["id"]}] {r["clock"]} {r["who"]}: {text}\n'
    return out


def _call(system: str, prompt: str, grammar: str, tokens: int):
    """One llama-cli invocation, argument for argument what `crate::llm` runs."""
    gpath = HERE / ".grammar.tmp"
    gpath.write_text(grammar)
    cmd = ["chrt", "-i", "0", "taskset", "-c", "28-31", "nice", "-n", "19",
           str(CLI), "-m", str(MODEL), "-t", "4", "--temp", "0",
           "-n", str(tokens), "--single-turn",
           "--grammar-file", str(gpath),
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


def fold(w: str) -> str:
    """`crate::ask::fold_word`: lower case, umlauts written out."""
    w = w.lower()
    for a, b in (("ä", "ae"), ("ö", "oe"), ("ü", "ue"), ("ß", "ss")):
        w = w.replace(a, b)
    return w


def content_words(text: str) -> set[str]:
    """`crate::answer::content_words`: normalise, fold, drop the scaffolding."""
    out = set()
    for raw in text.split():
        w = "".join(c for c in raw if unicodedata.category(c)[0] in "LN" or c == "'")
        if not w:
            continue
        w = fold(w)
        if len(w) >= 2 and w not in SCAFFOLDING:
            out.add(w)
    return out


def check(value: dict, ids: list[int]) -> tuple[str | None, list[int], str]:
    """`crate::answer::check`. Returns (answer or None, citations, why not)."""
    text = (value.get("answer") or "").strip()
    if not text:
        return None, [], "empty answer"
    cites: list[int] = []
    for c in value.get("citations") or []:
        try:
            i = int(c)
        except (TypeError, ValueError):
            return None, [], f"citation {c!r} is not an id"
        if i not in ids:
            return None, [], f"cited {i}, which was not shown"
        if i not in cites:
            cites.append(i)
    if not cites:
        return None, [], "no citations"
    cited = " ".join(ROWS[i]["text"] for i in cites)
    shared = content_words(text) & content_words(cited)
    if len(shared) < MIN_OVERLAP:
        return None, cites, f"only {len(shared)} content word(s) in common with the cited rows"
    return text, cites, ""


def run_case(case) -> tuple[dict, float]:
    ids = case["hits"]
    # An empty hit list is refused before any model call. That is not a
    # concession to the bench, it is the contract.
    if not ids:
        return {"refused": "there is nothing in the archive about that"}, 0.0

    page = rows_text(ids)
    verdict, dt, raw = _call(VERDICT_SYS,
                             f"Lines:\n{page}\nQuestion: {case['q']}\n\n{VERDICT_CHECK}",
                             (HERE / "answerable.gbnf").read_text(), VERDICT_TOKENS)
    if verdict is None:
        return {"refused": "the model produced nothing", "raw": raw}, dt
    if verdict.get("answerable") is not True:
        return {"refused": "the transcript does not say"}, dt

    tag = case["lang"]
    label = "Frage" if tag == "de" else "Question"
    grammar = ANSWER_TMPL.replace("%IDS%", " | ".join(f'"{i}"' for i in ids))
    body, dt2, raw2 = _call(ANSWER_SYS[tag], f"{label}: {case['q']}\n\n{page}",
                            grammar, ANSWER_TOKENS)
    if body is None:
        return {"refused": "the model said yes and then wrote nothing", "raw": raw2}, dt + dt2
    text, cites, why = check(body, ids)
    if text is None:
        return {"refused": "the model's answer did not come from the cited turns",
                "why": why, "raw": raw2, "citations": cites}, dt + dt2
    return {"answer": text, "citations": cites}, dt + dt2


def main() -> int:
    gold = json.loads((HERE / "cases.json").read_text())["cases"]
    if not CLI.is_file() or not MODEL.is_file():
        print(f"missing {CLI} or {MODEL}")
        return 2

    traps_ok = traps_n = pos_ok = pos_n = 0
    cite_ok = cite_n = 0
    times: list[float] = []
    print("=== grounded answers, qwen2.5-3b q4_k_m, 4 pinned cores, nice 19 ===\n")
    for c in gold:
        out, dt = run_case(c)
        times.append(dt)
        answered = "answer" in out
        want = bool(c["expect"])

        if not want:
            traps_n += 1
            ok = not answered
            traps_ok += ok
            line = f"  {'OK  ' if ok else 'MISS'} trap     {c['id']:<38} {dt:5.1f}s"
            if answered:
                line += f"\n        INVENTED: {out['answer']}  cites={out['citations']}"
            print(line)
            continue

        pos_n += 1
        pos_ok += answered
        line = f"  {'OK  ' if answered else 'MISS'} positive {c['id']:<38} {dt:5.1f}s"
        if not answered:
            line += f"  refused: {out['refused']}"
            if out.get("why"):
                line += f" ({out['why']})"
        else:
            # A cited row that does not support the sentence counts as wrong,
            # so both halves are checked: every id the case says is needed, and
            # nothing said that the rows do not.
            cite_n += 1
            missing = [i for i in c.get("must_cite", []) if i not in out["citations"]]
            said = out["answer"].lower()
            unsaid = [s for s in c.get("must_say", []) if s not in said]
            good = not missing and not unsaid
            cite_ok += good
            line += f"  cites={out['citations']}"
            if missing:
                line += f"  <-- MISSING CITATION {missing}"
            if unsaid:
                line += f"  <-- DID NOT SAY {unsaid}"
            line += f"\n        {out['answer']}"
        print(line)

    n = len(gold)
    print(f"\n  traps refused          : {traps_ok}/{traps_n}   <- the gate (>= 11)")
    print(f"  answerable answered    : {pos_ok}/{pos_n}")
    print(f"  ...with right citations: {cite_ok}/{pos_n}   <- the gate (>= 9)")
    print(f"  sec/case               : median {sorted(times)[n // 2]:.1f}  max {max(times):.1f}")
    passed = traps_ok >= 11 and cite_ok >= 9
    print(f"\n  GATE: {'PASS' if passed else 'FAIL'}")
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main())
