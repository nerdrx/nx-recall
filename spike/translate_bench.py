"""Is a 3B on four cores good enough to translate a turn you cannot read?

The feature is small and the failure mode is not. A translation shown under a
transcript line is read as *what that person said*, so a fluent paraphrase of
something else is worse than no translation at all. This measures it before it
ships, on parallel data, in the space the daemon already trusts.

Method:

  1. FLEURS is parallel — the same sentence ids exist in `en_us` and `de_de`.
     Take 20 sentence ids present in both, with the English as the input and
     the German as the reference nobody's model wrote.
  2. Translate the English with the shipped path: `llama-cli`, qwen2.5-3b
     q4_k_m, `--temp 0`, four pinned cores at nice 19, and the daemon's own
     grammar — `{"translation": "..."}` and nothing else.
  3. Score by cosine in the multilingual-e5-small space (`crate::semantic`'s
     model, `query: ` prefix, mean-pooled over the mask, L2-normalised), the
     candidate German against the FLEURS German.

Two baselines are printed with it, because a cosine on its own is a number
without a scale:

  * **ceiling** — the FLEURS German against itself is 1.0 by construction, so
    the useful ceiling is a DIFFERENT German sentence pair from the same
    corpus: how close two unrelated sentences already are in this space is the
    floor a real translation has to clear by a distance.
  * **passthrough** — the English input embedded against the German reference.
    e5 is multilingual, so this is high on its own; a translation that does not
    beat it has bought nothing.

GATE: mean cosine >= 0.80 against the FLEURS German.

    taskset -c 16-31 nice -n 19 python3 spike/translate_bench.py
"""

from __future__ import annotations

import json
import os
import random
import re
import subprocess
import sys
import time
from pathlib import Path

import numpy as np
import onnxruntime as ort
from tokenizers import Tokenizer

S = Path(os.environ.get(
    "NXR_SCRATCH",
    "/tmp/claude-1000/-run-media-nerdrx-Lex-claude/61962651-5c6e-4a8f-93fe-d58f323a2b89/scratchpad",
))
CLI = S / "llm" / "llama-b10736" / "llama-cli"
MODEL = Path.home() / ".local/share/nx-recall/models/qwen2.5-3b-instruct-q4_k_m.gguf"
E5_DIR = Path.home() / ".local/share/nx-recall/models/multilingual-e5-small-int8"
DE_TSV = S / "de" / "dev.tsv"
EN_TSV = S / "en_fleurs" / "data" / "en_us" / "dev.tsv"
N = int(os.environ.get("N_SENT", "20"))
GATE = 0.80

# The grammar the daemon ships, read from disk rather than restated here: a
# bench that keeps its own copy of the thing under test is a bench that can pass
# while the shipped file says something else. `crates/recalld/grammars/`'s copy
# is asserted equal to this one by a test in `crates/recalld/src/translate.rs`.
GBNF_PATH = Path(__file__).parent / "translate.gbnf"

SYS_TEMPLATE = (
    "You translate one line of overheard conversation into {lang}. Translate "
    "only what is written. Do not answer it, do not explain it, do not add or "
    "remove anything, and do not comment on it. Keep names, worlds and "
    "usernames exactly as they are spelled. If the line is already {lang}, "
    "repeat it unchanged. Output ONLY JSON.\n"
    "Examples:\n"
    "i will send you the link tomorrow -> {{\"translation\": \"ich schicke dir "
    "morgen den Link\"}}\n"
    "which portal was it -> {{\"translation\": \"welches Portal war es\"}}"
)

LANGS = {"de": "German", "en": "English"}


# ---------------------------------------------------------------------------
# the corpus
# ---------------------------------------------------------------------------

def read_fleurs(path: Path) -> dict[str, str]:
    """sentence id -> the raw (cased, punctuated) transcript."""
    out: dict[str, str] = {}
    for line in path.read_text().splitlines():
        parts = line.split("\t")
        if len(parts) < 3:
            continue
        out.setdefault(parts[0], parts[2])
    return out


# ---------------------------------------------------------------------------
# the model
# ---------------------------------------------------------------------------

def translate(text: str, to: str, grammar: Path) -> tuple[str | None, float]:
    cmd = ["taskset", "-c", "16,17,18,19", "nice", "-n", "19",
           str(CLI), "-m", str(MODEL), "-t", "4", "--temp", "0",
           "-n", "400", "--single-turn",
           "--grammar-file", str(grammar),
           "-sys", SYS_TEMPLATE.format(lang=LANGS[to]), "-p", text,
           "--no-display-prompt", "--no-warmup", "-ngl", "0"]
    t0 = time.time()
    r = subprocess.run(cmd, capture_output=True, text=True, timeout=600,
                       env={"LD_LIBRARY_PATH": str(CLI.parent), "PATH": "/usr/bin:/bin"})
    dt = time.time() - t0
    m = re.search(r"\{.*\}", r.stdout, re.S)
    if not m:
        return None, dt
    try:
        return json.loads(m.group(0)).get("translation"), dt
    except json.JSONDecodeError:
        return None, dt


class E5:
    """multilingual-e5-small, exactly as `crate::semantic::Embedder` runs it."""

    def __init__(self, root: Path):
        self.tok = Tokenizer.from_file(str(root / "tokenizer.json"))
        so = ort.SessionOptions()
        so.intra_op_num_threads = 4
        self.sess = ort.InferenceSession(str(root / "model.onnx"), so,
                                         providers=["CPUExecutionProvider"])
        self.inputs = {i.name for i in self.sess.get_inputs()}

    def __call__(self, text: str) -> np.ndarray:
        enc = self.tok.encode(f"query: {text}")
        ids = np.array([enc.ids], dtype=np.int64)
        mask = np.array([enc.attention_mask], dtype=np.int64)
        feed = {"input_ids": ids, "attention_mask": mask}
        if "token_type_ids" in self.inputs:
            feed["token_type_ids"] = np.zeros_like(ids)
        hidden = self.sess.run(None, feed)[0][0]
        m = mask[0][:, None].astype(np.float32)
        v = (hidden * m).sum(0) / max(m.sum(), 1.0)
        return v / (np.linalg.norm(v) + 1e-12)


def main() -> int:
    for p in (CLI, MODEL, DE_TSV, EN_TSV, GBNF_PATH, E5_DIR / "model.onnx"):
        if not p.exists():
            print(f"missing {p}")
            return 2

    de, en = read_fleurs(DE_TSV), read_fleurs(EN_TSV)
    shared = sorted(set(de) & set(en))
    if len(shared) < N:
        print(f"only {len(shared)} parallel sentence ids; need {N}")
        return 2
    random.Random(20260902).shuffle(shared)
    picked = shared[:N]

    grammar = GBNF_PATH

    e5 = E5(E5_DIR)
    print(f"=== translation, qwen2.5-3b q4_k_m -> de, 4 pinned cores, nice 19 ===")
    print(f"    {N} FLEURS sentence ids present in both en_us and de_de\n")

    cos, through, times, drops = [], [], [], 0
    ref_vecs = []
    for i, sid in enumerate(picked, 1):
        src, ref = en[sid].strip(), de[sid].strip()
        got, dt = translate(src, "de", grammar)
        times.append(dt)
        rv = e5(ref)
        ref_vecs.append(rv)
        pv = float(e5(src) @ rv)
        through.append(pv)
        if not got:
            drops += 1
            print(f"  {i:2}. DROPPED (no JSON)  {src[:70]}")
            continue
        c = float(e5(got) @ rv)
        cos.append(c)
        flag = "" if c >= GATE else "  <- under the gate"
        print(f"  {i:2}. cos={c:.3f} (passthrough {pv:.3f}) {dt:.1f}s{flag}")
        print(f"      en  {src[:110]}")
        print(f"      got {got[:110]}")
        print(f"      ref {ref[:110]}")

    # The scale: two DIFFERENT German FLEURS sentences against each other.
    unrelated = [float(ref_vecs[i] @ ref_vecs[(i + 1) % len(ref_vecs)])
                 for i in range(len(ref_vecs))]

    mean = float(np.mean(cos)) if cos else 0.0
    print(f"\n  translated       : {len(cos)}/{N}  ({drops} produced no JSON)")
    print(f"  MEAN COSINE      : {mean:.3f}   <- the gate is {GATE:.2f}")
    print(f"  median           : {float(np.median(cos)):.3f}" if cos else "")
    print(f"  min / max        : {min(cos):.3f} / {max(cos):.3f}" if cos else "")
    print(f"  passthrough (en) : {float(np.mean(through)):.3f}  "
          f"(the English input against the German reference)")
    print(f"  unrelated pairs  : {float(np.mean(unrelated)):.3f}  "
          f"(two different German sentences — the floor)")
    print(f"  sec/sentence     : median {sorted(times)[len(times)//2]:.1f}  max {max(times):.1f}")
    print(f"\n  VERDICT: {'SHIP' if mean >= GATE else 'DO NOT SHIP'}")
    return 0 if mean >= GATE else 1


if __name__ == "__main__":
    raise SystemExit(main())
