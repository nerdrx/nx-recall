"""Tier-3 bake-off: can a tiny GGUF extract commitments on 4 CPU cores?

Runs each candidate over the gold cases with grammar-constrained decoding
(the model can only emit schema-valid JSON), on exactly 4 pinned cores at
nice 19 — the user's stated budget, enforced, not simulated.

Scoring:
  null-precision  correct rejections / all gold-null cases   (false alarms poison)
  detection       found a commitment when one exists
  who             right speaker, among detected
  due             due phrase contains the gold marker, among detected
  what            all gold keywords appear in extracted text (any one of the
                  alternative keyword sets), among detected
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

MODELS = {
    "qwen2.5-1.5b": S / "qwen1_5b.gguf",
    "gemma-2-2b": S / "gemma2b.gguf",
    "qwen2.5-3b": S / "qwen3b.gguf",
}

SYS = (
    "You extract commitments from chat-lobby dialogue between speakers A and B. "
    "A commitment is one speaker in THIS dialogue promising the OTHER a concrete "
    "future action. NOT commitments: suggestions (we should...), questions, "
    "refusals, hedged hypotheticals (if I ever... I guess), past/already-done "
    "actions, in-game banter (I will kill you next round), things about oneself "
    "only (I will probably sleep), or reported promises of absent third people. "
    "Dialogue may be German, English or mixed; keep what/due in the original "
    "language. Output ONLY JSON.\n"
    "Examples:\n"
    "A: we should totally do a photo -> {\"is_commitment\": false}\n"
    "B: ich hab dir das gestern geschickt -> {\"is_commitment\": false}\n"
    "B: I will probably just log off soon -> {\"is_commitment\": false}\n"
    "B: ich schick dir morgen den Link -> {\"is_commitment\": true, \"who\": \"B\", "
    "\"what\": \"den Link schicken\", \"due\": \"morgen\"}"
)


def run_case(model: Path, window) -> tuple[dict | None, float]:
    prompt = "\n".join(f'{t["who"]}: {t["text"]}' for t in window)
    cmd = ["taskset", "-c", "16,17,18,19", "nice", "-n", "19",
           str(CLI), "-m", str(model), "-t", "4", "--temp", "0",
           "-n", "160", "-no-cnv" if False else "--single-turn",
           "--grammar-file", str(HERE / "commitment.gbnf"),
           "-sys", SYS, "-p", prompt,
           "--no-display-prompt", "--no-warmup", "-ngl", "0"]
    t0 = time.time()
    r = subprocess.run(cmd, capture_output=True, text=True, timeout=180)
    dt = time.time() - t0
    m = re.search(r"\{.*\}", r.stdout, re.S)
    if not m:
        return None, dt
    try:
        return json.loads(m.group(0)), dt
    except json.JSONDecodeError:
        return None, dt


def main() -> int:
    gold = json.loads((HERE / "cases.json").read_text())["cases"]
    only = sys.argv[1:] or list(MODELS)
    for name in only:
        path = MODELS[name]
        if not path.is_file():
            print(f"{name}: missing, skipped")
            continue
        stats = {"null_ok": 0, "null_n": 0, "det": 0, "pos_n": 0,
                 "who": 0, "due": 0, "due_n": 0, "what": 0, "parse_fail": 0}
        times, misses = [], []
        for c in gold:
            out, dt = run_case(path, c["window"])
            times.append(dt)
            got = out if (out and out.get("is_commitment") is True) else None
            if out is None:
                stats["parse_fail"] += 1
            if c["expect"] is None:
                stats["null_n"] += 1
                if got is None:
                    stats["null_ok"] += 1
                else:
                    misses.append((c["id"], "false-alarm", got))
            else:
                stats["pos_n"] += 1
                if got is None:
                    misses.append((c["id"], "missed", None))
                    continue
                stats["det"] += 1
                if got.get("who") == c["expect"]["who"]:
                    stats["who"] += 1
                text = f"{got.get('what', '')}".casefold()
                if any(k in text for k in c["expect"]["what_contains"]):
                    stats["what"] += 1
                exp_due = c["expect"]["due_contains"]
                if exp_due is not None:
                    stats["due_n"] += 1
                    if exp_due in f"{got.get('due') or ''}".casefold():
                        stats["due"] += 1

        n = stats
        print(f"\n=== {name}  ({path.stat().st_size/1e9:.1f} GB, 4 cores, nice 19) ===")
        print(f"  null-precision : {n['null_ok']}/{n['null_n']}")
        print(f"  detection      : {n['det']}/{n['pos_n']}")
        print(f"  who correct    : {n['who']}/{n['det'] or 1}")
        print(f"  what keywords  : {n['what']}/{n['det'] or 1}")
        print(f"  due matched    : {n['due']}/{n['due_n'] or 1}")
        print(f"  parse failures : {n['parse_fail']}")
        print(f"  sec/case       : median {sorted(times)[len(times)//2]:.1f}  "
              f"max {max(times):.1f}")
        for mid, kind, got in misses[:6]:
            print(f"    {kind:<11} {mid}" + (f"  got={json.dumps(got, ensure_ascii=False)[:70]}" if got else ""))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
