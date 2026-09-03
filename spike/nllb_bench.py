"""Is a purpose-built translator better than the 3B that is doing it today?

0.9.0 shipped translation as a prompt: qwen2.5-3b, `--temp 0`, a grammar that
admits `{"translation": "..."}` and nothing else, and three guards
(`crate::translate`). It was measured once, on one pair, by one metric — 20
FLEURS sentence ids, English into German, cosine 0.948 in the
multilingual-e5-small space (FINDINGS §16). That is a gate, not a comparison.

This is the comparison. Meta's NLLB-200-distilled-600M is a translation model
and nothing else: no prompt, no grammar, no refusals to engineer, and a fifth
of the parameters. The question is whether a 600M model that only translates
beats a 3B model that has been talked into it, on the thirteen directions this
lobby actually needs, by the metric machine translation is actually scored
with.

  * **chrF** (sacrebleu's chrF2) against the FLEURS reference — character
    n-gram F-score, the standard MT metric, and the one that does not fall over
    on German compounds or on Japanese.
  * **e5 cosine** in `crate::semantic`'s space, so §16's number has a
    continuation and this table can be read next to it.
  * **empties**, **echoes** (output == input, the guard in
    `translate::judge`), and **seconds per sentence** on four pinned cores.

Corpus: `nx-scratch/fleurs`, `*.test.tsv`, matched by sentence id — FLEURS is
parallel, so every reference is a human's. 100 ids per pair.

Pairs: X→en for X in de fr es it pl fi ja ru tr nl (what the lobby is, read by
somebody with English), and X→de for X in en fr ja (the other half of this
household).

Short lines: 60 per pair for three pairs, 2-5 words cut from the front of the
same sentences. There is no reference for a cut phrase, so these are NOT
scored for accuracy — they measure what actually breaks on a two-word VRChat
turn: an empty answer, an echo, or a wrong-language one. That is the honest
thing a reference-less set can say, and it is the failure mode that matters,
because a turn is short far more often than it is a FLEURS sentence.

GATE to make NLLB the default translator: it beats Qwen on chrF for at least
80% of the pairs, is not worse by more than 2 chrF on any of them, and has
fewer empties and echoes and a lower median latency.

    chrt -i 0 taskset -c 20-23 nice -n 19 python3 spike/nllb_bench.py --system nllb
    chrt -i 0 taskset -c 20-23 nice -n 19 python3 spike/nllb_bench.py --system qwen
    python3 spike/nllb_bench.py --report

Answers are cached per (system, pair) under `$NXR_SCRATCH/nllb/out`, because
the Qwen leg is roughly 1500 `llama-cli` invocations and nobody should have to
run it twice to re-read the table.
"""

from __future__ import annotations

import argparse
import json
import os
import random
import re
import subprocess
import time
from pathlib import Path

import numpy as np

HERE = Path(__file__).parent
SCRATCH = Path(os.environ.get("NXR_SCRATCH", "/tmp/nx-recall-workspace/nx-scratch"))
FLEURS = SCRATCH / "fleurs"
OUT = SCRATCH / "nllb" / "out"
NLLB_DIR = SCRATCH / "nllb" / "onnx"

MODELS = Path.home() / ".local/share/nx-recall/models"
CLI = SCRATCH / "llm" / "llama-b10736" / "llama-cli"
QWEN = MODELS / "qwen2.5-3b-instruct-q4_k_m.gguf"
E5_DIR = MODELS / "multilingual-e5-small-int8"

CORES = os.environ.get("NXR_CORES", "20-23")
THREADS = int(os.environ.get("NXR_THREADS", "4"))

N_SENT = int(os.environ.get("N_SENT", "100"))
N_SHORT = int(os.environ.get("N_SHORT", "60"))
SEED = 20260903

# ---------------------------------------------------------------------------
# languages
# ---------------------------------------------------------------------------
#
# Our two-letter tag -> (FLEURS split name, NLLB code). The NLLB codes are the
# ones `crates/recalld/src/nllb.rs` ships; the test there asserts this table and
# that one agree, so the bench cannot be measuring a different language from the
# daemon.
LANGS: dict[str, tuple[str, str]] = {
    "de": ("de_de", "deu_Latn"),
    "en": ("en_us", "eng_Latn"),
    "fr": ("fr_fr", "fra_Latn"),
    "es": ("es_419", "spa_Latn"),
    "it": ("it_it", "ita_Latn"),
    "nl": ("nl_nl", "nld_Latn"),
    "no": ("nb_no", "nob_Latn"),
    "pl": ("pl_pl", "pol_Latn"),
    "pt": ("pt_br", "por_Latn"),
    "fi": ("fi_fi", "fin_Latn"),
    "ja": ("ja_jp", "jpn_Jpan"),
    "ru": ("ru_ru", "rus_Cyrl"),
    "uk": ("uk_ua", "ukr_Cyrl"),
    "tr": ("tr_tr", "tur_Latn"),
    "sv": ("sv_se", "swe_Latn"),
    "da": ("da_dk", "dan_Latn"),
    "cs": ("cs_cz", "ces_Latn"),
    "ko": ("ko_kr", "kor_Hang"),
    "zh": ("cmn_hans_cn", "zho_Hans"),
    "ar": ("ar_eg", "arb_Arab"),
    "el": ("el_gr", "ell_Grek"),
}

PAIRS: list[tuple[str, str]] = [(x, "en") for x in
                                ["de", "fr", "es", "it", "pl", "fi", "ja", "ru", "tr", "nl"]]
PAIRS += [(x, "de") for x in ["en", "fr", "ja"]]

# The three the short-line set is run on: the two this household reads, and the
# one whose script makes every guard behave differently.
SHORT_PAIRS = [("de", "en"), ("ja", "en"), ("en", "de")]

GATE_WIN_FRACTION = 0.80
GATE_MAX_LOSS = 2.0


# ---------------------------------------------------------------------------
# the corpus
# ---------------------------------------------------------------------------

def read_fleurs(tag: str) -> dict[str, str]:
    """sentence id -> the raw (cased, punctuated) transcript."""
    split = LANGS[tag][0]
    out: dict[str, str] = {}
    for name in (f"{split}.test.tsv", f"{split}.dev.tsv"):
        path = FLEURS / name
        if not path.exists():
            continue
        for line in path.read_text().splitlines():
            parts = line.split("\t")
            if len(parts) >= 3:
                out.setdefault(parts[0], parts[2].strip())
    return out


def sentences(src: str, tgt: str, n: int) -> list[tuple[str, str, str]]:
    """`n` (id, source, reference) triples present in both languages."""
    a, b = read_fleurs(src), read_fleurs(tgt)
    shared = sorted(set(a) & set(b))
    random.Random(SEED).shuffle(shared)
    picked = [i for i in shared if a[i] and b[i]][:n]
    return [(i, a[i], b[i]) for i in picked]


def short_lines(src: str, tgt: str, n: int) -> list[tuple[str, str]]:
    """`n` (id, 2-5 word phrase) cut from the front of the same sentences.

    No reference, on purpose: see the module note. The cut is by whitespace for
    everything with spaces in it and by character for Japanese, which has none —
    four to twelve characters is about what a two-to-five word turn is there.
    """
    rng = random.Random(SEED + 1)
    out = []
    for sid, s, _ in sentences(src, tgt, n * 3):
        if src in ("ja", "zh", "ko"):
            k = rng.randint(4, 12)
            phrase = s[:k]
        else:
            words = s.split()
            k = rng.randint(2, 5)
            if len(words) < k:
                continue
            phrase = " ".join(words[:k])
        phrase = phrase.strip(" ,.;:!?、。")
        if phrase:
            out.append((sid, phrase))
        if len(out) == n:
            break
    return out


# ---------------------------------------------------------------------------
# the two systems
# ---------------------------------------------------------------------------

class Nllb:
    """NLLB-200-distilled-600M, the int8 ONNX export, greedy, max 128 tokens.

    This is the shipping path, not a stand-in for it: the same two graphs, the
    same tokenizer, the same greedy loop and the same cross-attention rule that
    `crates/recalld/src/nllb.rs` implements. Benching CTranslate2 and shipping
    ONNX would have measured a model this daemon does not run.
    """

    LAYERS = 12
    HEADS = 16
    HEAD_DIM = 64
    EOS = 2
    MAX_NEW = 128

    def __init__(self, root: Path = NLLB_DIR):
        import onnxruntime as ort
        from tokenizers import Tokenizer

        so = ort.SessionOptions()
        so.intra_op_num_threads = THREADS
        so.log_severity_level = 3
        self.enc = ort.InferenceSession(str(root / "encoder_model_quantized.onnx"), so,
                                        providers=["CPUExecutionProvider"])
        self.dec = ort.InferenceSession(str(root / "decoder_model_merged_quantized.onnx"), so,
                                        providers=["CPUExecutionProvider"])
        self.out_names = [o.name for o in self.dec.get_outputs()]
        self.tok = Tokenizer.from_file(str(root / "tokenizer.json"))

    def model_id(self) -> str:
        return "nllb-200-distilled-600m-int8"

    def __call__(self, text: str, src: str, tgt: str) -> str:
        src_id = self.tok.token_to_id(LANGS[src][1])
        tgt_id = self.tok.token_to_id(LANGS[tgt][1])
        ids = [src_id] + self.tok.encode(text, add_special_tokens=False).ids + [self.EOS]
        ids = np.array([ids], dtype=np.int64)
        mask = np.ones_like(ids)
        hidden = self.enc.run(None, {"input_ids": ids, "attention_mask": mask})[0]

        out = [self.EOS, tgt_id]
        past: dict[str, np.ndarray] | None = None
        empty = np.zeros((1, self.HEADS, 0, self.HEAD_DIM), dtype=np.float32)
        for _ in range(self.MAX_NEW):
            first = past is None
            feed = {
                "encoder_attention_mask": mask,
                "encoder_hidden_states": hidden,
                "input_ids": np.array([out if first else [out[-1]]], dtype=np.int64),
                "use_cache_branch": np.array([not first]),
            }
            if first:
                for i in range(self.LAYERS):
                    for k in ("decoder.key", "decoder.value", "encoder.key", "encoder.value"):
                        feed[f"past_key_values.{i}.{k}"] = empty
            else:
                feed.update(past)
            d = dict(zip(self.out_names, self.dec.run(None, feed)))
            nxt = int(np.argmax(d["logits"][0, -1]))
            if nxt == self.EOS:
                break
            out.append(nxt)
            if first:
                # The cross-attention KV is computed once, on the step that has
                # the encoder states, and then held. On a cached step the merged
                # export returns `present.*.encoder.*` as (0, 16, 1, 64) DUMMIES
                # — feeding those back is the crash this bench hit first, and it
                # is the one thing about this export that is not obvious.
                past = {}
                for i in range(self.LAYERS):
                    for k in ("encoder.key", "encoder.value"):
                        past[f"past_key_values.{i}.{k}"] = d[f"present.{i}.{k}"]
            for i in range(self.LAYERS):
                for k in ("decoder.key", "decoder.value"):
                    past[f"past_key_values.{i}.{k}"] = d[f"present.{i}.{k}"]
        return self.tok.decode(out[2:], skip_special_tokens=True).strip()


class Qwen:
    """The SHIPPED translation path: `crate::translate`'s prompt and grammar.

    The prompt is not restated here. It is exported from `translate::system_for`
    by `translate::tests::the_bench_runs_the_prompt_this_daemon_ships`, exactly
    as `spike/digest_bench` does it, so a bench run measures what ships and
    cannot quietly become a measurement of something else.
    """

    def __init__(self):
        self.gbnf = HERE / "translate.gbnf"
        self.system = {
            "en": (HERE / "nllb_bench" / "system.en.txt").read_text(),
            "de": (HERE / "nllb_bench" / "system.de.txt").read_text(),
        }

    def model_id(self) -> str:
        return "qwen2.5-3b-instruct-q4_k_m"

    def __call__(self, text: str, src: str, tgt: str) -> str:
        cmd = ["chrt", "-i", "0", "taskset", "-c", CORES, "nice", "-n", "19",
               str(CLI), "-m", str(QWEN), "-t", str(THREADS), "--temp", "0",
               "-n", "400", "--single-turn",
               "--grammar-file", str(self.gbnf),
               "-sys", self.system[tgt], "-p", text,
               "--no-display-prompt", "--no-warmup", "-ngl", "0"]
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=600,
                           env={"LD_LIBRARY_PATH": str(CLI.parent), "PATH": "/usr/bin:/bin"})
        m = re.search(r"\{.*\}", r.stdout, re.S)
        if not m:
            return ""
        try:
            return (json.loads(m.group(0)).get("translation") or "").strip()
        except json.JSONDecodeError:
            return ""


class Ct2:
    """CTranslate2 int8, the same 600M weights. A cross-check on the ONNX
    quantisation, not a shipping candidate: it would mean a second native
    dependency for a model the daemon can already run through `ort`."""

    def __init__(self, root: Path = SCRATCH / "nllb" / "ct2"):
        import ctranslate2
        from tokenizers import Tokenizer
        self.tr = ctranslate2.Translator(str(root), device="cpu",
                                         inter_threads=1, intra_threads=THREADS)
        self.tok = Tokenizer.from_file(str(NLLB_DIR / "tokenizer.json"))

    def model_id(self) -> str:
        return "nllb-200-distilled-600m-ct2-int8"

    def __call__(self, text: str, src: str, tgt: str) -> str:
        enc = self.tok.encode(text, add_special_tokens=False)
        tokens = [LANGS[src][1]] + enc.tokens + ["</s>"]
        res = self.tr.translate_batch([tokens], target_prefix=[[LANGS[tgt][1]]],
                                      beam_size=1, max_decoding_length=128)
        out = res[0].hypotheses[0][1:]
        ids = [i for i in (self.tok.token_to_id(t) for t in out) if i is not None]
        return self.tok.decode(ids, skip_special_tokens=True).strip()


SYSTEMS = {"nllb": Nllb, "qwen": Qwen, "ct2": Ct2}


# ---------------------------------------------------------------------------
# scoring
# ---------------------------------------------------------------------------

class E5:
    """multilingual-e5-small, exactly as `crate::semantic::TextEmbedder` runs
    it, so this column continues FINDINGS §16 rather than restarting it."""

    def __init__(self, root: Path = E5_DIR):
        import onnxruntime as ort
        from tokenizers import Tokenizer
        self.tok = Tokenizer.from_file(str(root / "tokenizer.json"))
        so = ort.SessionOptions()
        so.intra_op_num_threads = THREADS
        so.log_severity_level = 3
        self.sess = ort.InferenceSession(str(root / "model.onnx"), so,
                                         providers=["CPUExecutionProvider"])
        self.inputs = {i.name for i in self.sess.get_inputs()}

    def __call__(self, text: str) -> np.ndarray:
        enc = self.tok.encode(f"query: {text}")
        ids = np.array([enc.ids[:256]], dtype=np.int64)
        mask = np.array([enc.attention_mask[:256]], dtype=np.int64)
        feed = {"input_ids": ids, "attention_mask": mask}
        if "token_type_ids" in self.inputs:
            feed["token_type_ids"] = np.zeros_like(ids)
        hidden = self.sess.run(None, feed)[0][0]
        m = mask[0][:, None].astype(np.float32)
        v = (hidden * m).sum(0) / max(m.sum(), 1.0)
        return v / (np.linalg.norm(v) + 1e-12)


def normalise_words(s: str) -> str:
    """`crate::asr::normalise_words`, near enough for an echo test: lowercase,
    letters and digits and spaces only, collapsed."""
    keep = [c.lower() if (c.isalnum() or c.isspace()) else " " for c in s]
    return " ".join("".join(keep).split())


def chrf(hyps: list[str], refs: list[str]) -> float:
    import sacrebleu
    return sacrebleu.CHRF().corpus_score(hyps, [refs]).score


# ---------------------------------------------------------------------------
# running
# ---------------------------------------------------------------------------

def cache_path(system: str, src: str, tgt: str, short: bool) -> Path:
    kind = "short" if short else "sent"
    return OUT / f"{system}.{kind}.{src}-{tgt}.json"


def run_pair(sysname: str, engine, src: str, tgt: str, short: bool, force: bool) -> dict:
    path = cache_path(sysname, src, tgt, short)
    if path.exists() and not force:
        return json.loads(path.read_text())
    if short:
        rows = [{"id": i, "src": s, "ref": None} for i, s in short_lines(src, tgt, N_SHORT)]
    else:
        rows = [{"id": i, "src": s, "ref": r} for i, s, r in sentences(src, tgt, N_SENT)]
    print(f"  {sysname} {src}->{tgt} {'short' if short else 'sent'}: {len(rows)} lines",
          flush=True)
    for n, row in enumerate(rows, 1):
        t0 = time.time()
        try:
            row["out"] = engine(row["src"], src, tgt)
        except Exception as e:  # a runner that dies on one line is a datum
            row["out"] = ""
            row["error"] = str(e)[:200]
        row["s"] = round(time.time() - t0, 3)
        if n % 25 == 0:
            print(f"    {n}/{len(rows)}  {row['s']:.1f}s", flush=True)
    out = {"system": sysname, "model_id": engine.model_id(), "src": src, "tgt": tgt,
           "short": short, "rows": rows}
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(out, ensure_ascii=False, indent=1))
    return out


def score(data: dict, e5: E5 | None) -> dict:
    rows = data["rows"]
    outs = [r["out"] for r in rows]
    empties = sum(1 for o in outs if not o.strip())
    echoes = sum(1 for r in rows
                 if r["out"].strip() and normalise_words(r["out"]) == normalise_words(r["src"]))
    times = sorted(r["s"] for r in rows)
    res = {
        "n": len(rows),
        "empties": empties,
        "echoes": echoes,
        "median_s": round(times[len(times) // 2], 2) if times else 0.0,
        "p90_s": round(times[int(len(times) * 0.9)], 2) if times else 0.0,
    }
    if data["short"]:
        return res
    ok = [r for r in rows if r["out"].strip()]
    res["chrf"] = round(chrf([r["out"] for r in ok], [r["ref"] for r in ok]), 2) if ok else 0.0
    # …and the same chrF with an empty answer counted as an empty string, which
    # is what a reader of the transcript actually gets. Reported next to it
    # rather than instead of it: one says how good the translations are, the
    # other says how good the FEATURE is.
    res["chrf_all"] = round(chrf(outs, [r["ref"] for r in rows]), 2)
    if e5 is not None:
        cos = []
        for r in ok:
            cos.append(float(e5(r["out"]) @ e5(r["ref"])))
        res["cosine"] = round(float(np.mean(cos)), 3) if cos else 0.0
    return res


def report(systems: list[str]) -> int:
    e5 = E5()
    table: dict[str, dict[tuple[str, str], dict]] = {s: {} for s in systems}
    for s in systems:
        for src, tgt in PAIRS:
            p = cache_path(s, src, tgt, False)
            if p.exists():
                table[s][(src, tgt)] = score(json.loads(p.read_text()), e5)

    base, cand = "qwen", "nllb"
    print("\n=== 100 FLEURS sentences per pair, matched by id ===")
    head = f"{'pair':<9}"
    for s in systems:
        head += f"{s + ' chrF':>12}{s + ' cos':>11}{s + ' empt':>11}{s + ' echo':>11}{s + ' med s':>12}"
    print(head)
    print("-" * len(head))
    wins, losses, worst = 0, 0, 0.0
    for src, tgt in PAIRS:
        line = f"{src + '->' + tgt:<9}"
        for s in systems:
            r = table[s].get((src, tgt))
            if not r:
                line += f"{'-':>12}{'-':>11}{'-':>11}{'-':>11}{'-':>12}"
                continue
            line += (f"{r['chrf']:>12.2f}{r.get('cosine', 0):>11.3f}"
                     f"{r['empties']:>11}{r['echoes']:>11}{r['median_s']:>12.2f}")
        if (src, tgt) in table.get(base, {}) and (src, tgt) in table.get(cand, {}):
            d = table[cand][(src, tgt)]["chrf"] - table[base][(src, tgt)]["chrf"]
            line += f"   {d:+6.2f}"
            if d > 0:
                wins += 1
            else:
                losses += 1
                worst = min(worst, d)
        print(line)

    for s in systems:
        rs = list(table[s].values())
        if not rs:
            continue
        print(f"\n  {s}: mean chrF {np.mean([r['chrf'] for r in rs]):.2f}   "
              f"mean chrF (empties counted) {np.mean([r['chrf_all'] for r in rs]):.2f}   "
              f"mean cosine {np.mean([r.get('cosine', 0) for r in rs]):.3f}   "
              f"empties {sum(r['empties'] for r in rs)}   "
              f"echoes {sum(r['echoes'] for r in rs)}   "
              f"median {np.median([r['median_s'] for r in rs]):.2f}s")

    print("\n=== 60 short lines (2-5 words) per pair — no reference, so no chrF ===")
    print(f"{'pair':<9}" + "".join(f"{s + ' empt':>11}{s + ' echo':>11}{s + ' med s':>12}"
                                   for s in systems))
    for src, tgt in SHORT_PAIRS:
        line = f"{src + '->' + tgt:<9}"
        for s in systems:
            p = cache_path(s, src, tgt, True)
            if not p.exists():
                line += f"{'-':>11}{'-':>11}{'-':>12}"
                continue
            r = score(json.loads(p.read_text()), None)
            line += f"{r['empties']:>11}{r['echoes']:>11}{r['median_s']:>12.2f}"
        print(line)

    if base in systems and cand in systems and wins + losses == len(PAIRS):
        frac = wins / len(PAIRS)
        q = [table[base][p] for p in PAIRS]
        n = [table[cand][p] for p in PAIRS]
        checks = [
            (f"chrF wins on {wins}/{len(PAIRS)} pairs ({frac:.0%})", frac >= GATE_WIN_FRACTION),
            (f"worst loss {worst:+.2f} chrF", worst >= -GATE_MAX_LOSS),
            (f"empties {sum(r['empties'] for r in n)} vs {sum(r['empties'] for r in q)}",
             sum(r["empties"] for r in n) <= sum(r["empties"] for r in q)),
            (f"echoes {sum(r['echoes'] for r in n)} vs {sum(r['echoes'] for r in q)}",
             sum(r["echoes"] for r in n) <= sum(r["echoes"] for r in q)),
            (f"median {np.median([r['median_s'] for r in n]):.2f}s vs "
             f"{np.median([r['median_s'] for r in q]):.2f}s",
             np.median([r["median_s"] for r in n]) < np.median([r["median_s"] for r in q])),
        ]
        print("\n  GATE")
        for what, ok in checks:
            print(f"    [{'PASS' if ok else 'FAIL'}] {what}")
        passed = all(ok for _, ok in checks)
        print(f"\n  VERDICT: {'SWITCH THE DEFAULT TO NLLB' if passed else 'KEEP QWEN AS THE DEFAULT'}")
        return 0 if passed else 1
    print("\n  (not every pair has been run for both systems yet)")
    return 2


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--system", choices=sorted(SYSTEMS))
    ap.add_argument("--report", action="store_true")
    ap.add_argument("--systems", default="qwen,nllb")
    ap.add_argument("--force", action="store_true")
    ap.add_argument("--pairs", default="")
    ap.add_argument("--no-short", action="store_true")
    a = ap.parse_args()

    if a.report:
        return report([s for s in a.systems.split(",") if s])
    if not a.system:
        ap.error("--system or --report")

    pairs = PAIRS
    if a.pairs:
        pairs = [tuple(p.split("-")) for p in a.pairs.split(",")]
    engine = SYSTEMS[a.system]()
    started = time.time()
    for src, tgt in pairs:
        run_pair(a.system, engine, src, tgt, False, a.force)
    if not a.no_short:
        for src, tgt in SHORT_PAIRS:
            if (src, tgt) in pairs:
                run_pair(a.system, engine, src, tgt, True, a.force)
    print(f"\n{a.system}: {time.time() - started:.0f}s total")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
