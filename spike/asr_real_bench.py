"""Is there a better ASR model — measured on the user's REAL lobby audio?

Every accuracy number so far was read speech. The user's verdict on the live
product ("kinda bad at understanding what's said") is the number that matters,
and they have authorised using the recorded voices, on this machine, to test
whether what comes out makes sense.

Two phases:

  lab   FLEURS German (full utterances AND 1.5 s / 3 s fragments) + LibriSpeech
        English, all through Opus 24k. Ground truth exists, so this ranks the
        candidates on WER and — crucially — VALIDATES THE JUDGE: given two
        transcripts, does the local LLM pick the one with the lower WER?

  real  N real segments from the live database (stratified mic / app). No
        ground truth, so three proxies: pairwise agreement between models, the
        validated LLM judge's win-rate against the current model given the two
        previous turns as context, and language-consistency with the thread.

Candidates: current (parakeet v3), whisper-turbo, canary-180m-flash (de/en/es/
fr, needs a source language — fed from the classifier / thread context),
whisper-large-v3 (the ceiling). Everything at 4 pinned cores, nice 19, RTF
reported, because "better" that cannot run live is a different feature.

Outputs aggregates only. A few side-by-side samples are printed at the end,
mic (the user's own voice) first.
"""

from __future__ import annotations

import json
import os
import random
import re
import sqlite3
import subprocess
import sys
import time
from concurrent.futures import ProcessPoolExecutor
from pathlib import Path

import numpy as np
import soundfile as sf

sys.path.insert(0, str(Path(__file__).parent))
from asr_multilang import load_wav_16k, norm_de, norm_en  # noqa: E402
from asr_spike import wer_counts  # noqa: E402
from harness import SR, extract_corpus, index_corpus, load, opus_roundtrip, rms_normalise, trim_silence  # noqa: E402
from lang_flip import classify  # noqa: E402

S = Path(os.environ.get("NXR_SCRATCH", "/tmp/nxr"))
M = S / "models"
LIVE = Path.home() / ".local/share/nx-recall"
# The bake-off's llama.cpp if the scratchpad still has it, else the copy the
# daemon installed for itself (`models fetch --graph`).
_CANDIDATES = [S / "llm" / "llama-b10736", Path.home() / ".local/share/nx-recall/models/llama"]
LLM_DIR = next((d for d in _CANDIDATES if (d / "llama-cli").exists()), _CANDIDATES[0])
CLI = LLM_DIR / "llama-cli"
QWEN = LIVE / "models" / "qwen2.5-3b-instruct-q4_k_m.gguf"

MODELS = {
    "parakeet_v3": ("transducer", M / "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8"),
    "whisper_turbo": ("whisper", M / "sherpa-onnx-whisper-turbo"),
    "canary_180m": ("canary", M / "sherpa-onnx-nemo-canary-180m-flash-en-es-de-fr-int8"),
    "whisper_large_v3": ("whisper", M / "sherpa-onnx-whisper-large-v3"),
}
N_LAB = 40
N_REAL = int(os.environ.get("N_REAL", "240"))
JUDGE_N = int(os.environ.get("JUDGE_N", "120"))
CPUS = "16,17,18,19"

_G: dict = {}


def make(kind: str, d: Path, threads: int):
    import sherpa_onnx as so

    def one(g):
        f = sorted(d.glob(g))
        return str(f[0]) if f else None

    if kind == "whisper":
        return so.OfflineRecognizer.from_whisper(
            encoder=one("*encoder.int8.onnx"), decoder=one("*decoder.int8.onnx"),
            tokens=one("*tokens.txt"), language="", task="transcribe", num_threads=threads)
    if kind == "canary":
        # src_lang is set per call by re-creating cheaply; sherpa caches nothing
        # across recognizers, so we hold one per language.
        return {
            lang: so.OfflineRecognizer.from_nemo_canary(
                encoder=one("encoder.int8.onnx"), decoder=one("decoder.int8.onnx"),
                tokens=one("tokens.txt"), src_lang=lang, tgt_lang=lang, num_threads=threads)
            for lang in ("de", "en")
        }
    return so.OfflineRecognizer.from_transducer(
        encoder=one("encoder*.onnx"), decoder=one("decoder*.onnx"), joiner=one("joiner*.onnx"),
        tokens=one("tokens*.txt"), num_threads=threads, model_type="nemo_transducer")


def _init(name, jobs):
    try:
        os.nice(19 - os.nice(0))
        os.sched_setaffinity(0, {16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31})
    except OSError:
        pass
    kind, d = MODELS[name]
    _G["kind"] = kind
    _G["rec"] = make(kind, d, 1)
    _G["jobs"] = jobs


def transcribe(x: np.ndarray, lang_hint: str) -> tuple[str, float]:
    rec = _G["rec"]
    if _G["kind"] == "canary":
        rec = rec["de" if lang_hint == "de" else "en"]
    st = rec.create_stream()
    st.accept_waveform(SR, x.astype(np.float32))
    t0 = time.time()
    rec.decode_stream(st)
    return st.result.text.strip(), time.time() - t0


def _run(i):
    j = _G["jobs"][i]
    if j["kind"] == "lab":
        x = load_wav_16k(Path(j["path"]))
        if j.get("dur"):
            off = j["off"]
            x = x[int(off * SR):int((off + j["dur"]) * SR)]
            if len(x) < SR // 2:
                return None
        x = opus_roundtrip(x, 24)
    else:
        x, sr = sf.read(j["path"], dtype="float32")
        if x.ndim > 1:
            x = x.mean(axis=1)
    txt, dt = transcribe(x, j.get("lang_hint", "en"))
    return {**j, "hyp": txt, "dt": dt, "dur_s": len(x) / SR}


# ----------------------------------------------------------------- judge

JUDGE_SYS = (
    "You compare two automatic transcripts of the same short spoken turn from a "
    "chat lobby. Speech may be German, English or mixed. Given the two previous "
    "turns as context, decide which transcript is more likely what was actually "
    "said: coherent words, plausible in context, correct language. Output ONLY "
    "JSON: {\"better\": \"A\"|\"B\"|\"same\"}. Choose \"same\" only if both are "
    "equally plausible or equally unusable."
)
JUDGE_GBNF = 'root ::= "{" ws "\\"better\\":" ws ("\\"A\\"" | "\\"B\\"" | "\\"same\\"") ws "}"\nws ::= [ \\t\\n]?\n'


def judge(context: list[str], a: str, b: str) -> str | None:
    gb = S / "judge.gbnf"
    if not gb.exists():
        gb.write_text(JUDGE_GBNF)
    prompt = "Previous turns:\n" + "\n".join(f"- {c}" for c in context[-2:] if c) + \
        f"\n\nTranscript A: {a}\nTranscript B: {b}"
    env = dict(os.environ, LD_LIBRARY_PATH=str(LLM_DIR))
    r = subprocess.run(
        ["taskset", "-c", CPUS, "nice", "-n", "19", str(CLI), "-m", str(QWEN), "-t", "4",
         "--temp", "0", "-n", "24", "--single-turn", "--grammar-file", str(gb),
         "-sys", JUDGE_SYS, "-p", prompt, "--no-display-prompt", "--no-warmup", "-ngl", "0"],
        capture_output=True, text=True, timeout=120, env=env)
    m = re.search(r'"better"\s*:\s*"(A|B|same)"', r.stdout)
    return m.group(1) if m else None


def run_model(name, jobs, workers=12):
    with ProcessPoolExecutor(max_workers=workers, initializer=_init, initargs=(name, jobs)) as ex:
        return [r for r in ex.map(_run, range(len(jobs)), chunksize=4) if r]


def wer_of(ref, hyp, lang):
    n = norm_de if lang == "de" else norm_en
    sd, ins, nref = wer_counts(n(ref), n(hyp))
    return (sd + ins) / max(1, nref)


def main() -> int:
    rng = random.Random(3)

    # ---------- lab set ----------
    lab = []
    rows = []
    for line in (S / "de" / "dev.tsv").read_text().splitlines():
        p = line.split("\t")
        if len(p) >= 3 and (S / "de" / "dev" / p[1]).is_file():
            rows.append((S / "de" / "dev" / p[1], p[2]))
    for path, ref in rng.sample(rows, N_LAB):
        lab.append(dict(kind="lab", set="de_full", path=str(path), ref=ref, lang="de", lang_hint="de"))
        for dur in (1.5, 3.0):
            lab.append(dict(kind="lab", set=f"de_{dur}s", path=str(path), ref=ref, lang="de",
                            lang_hint="de", dur=dur, off=rng.uniform(0.5, 6.0)))
    root = extract_corpus(S / "corpus" / "dev-clean.tar.gz", S / "corpus")
    trans = {}
    for f in root.rglob("*.trans.txt"):
        for line in f.read_text().splitlines():
            uid, _, text = line.partition(" ")
            trans[uid] = text
    by = index_corpus(root, 5)
    en_utts = [(u.path, trans[u.path.stem]) for s in sorted(by) for u in by[s][3:4]][:N_LAB]
    for path, ref in en_utts:
        lab.append(dict(kind="lab", set="en_full", path=str(path), ref=ref, lang="en", lang_hint="en"))

    # ---------- real set (authorised by the user, stays on this machine) ----------
    db = sqlite3.connect(f"file:{LIVE / 'recall.db'}?mode=ro", uri=True)
    q = """SELECT g.id, g.audio_path, g.text, g.lang, sc.kind, g.thread_id, g.t_start_ns,
                  (g.t_end_ns-g.t_start_ns)/1e9
           FROM segments g JOIN sessions ss ON ss.id=g.session_id JOIN sources sc ON sc.id=ss.source_id
           WHERE g.deleted_at IS NULL AND g.text IS NOT NULL AND LENGTH(g.text) > 10
             AND g.audio_path IS NOT NULL ORDER BY RANDOM() LIMIT 3000"""
    cand = [r for r in db.execute(q) if (LIVE / r[1]).is_file()]
    mic = [r for r in cand if r[4] == "mic"][: N_REAL // 3]
    app = [r for r in cand if r[4] != "mic"][: N_REAL - len(mic)]
    real = []
    for (sid, ap, text, lang, kind, tid, t0, dur) in mic + app:
        prev = [r[0] for r in db.execute(
            "SELECT text FROM segments WHERE thread_id=? AND t_start_ns<? AND deleted_at IS NULL "
            "AND text IS NOT NULL ORDER BY t_start_ns DESC LIMIT 2", (tid, t0))] if tid else []
        hint = lang or classify(text)
        real.append(dict(kind="real", sid=sid, path=str(LIVE / ap), current=text, kind_src=kind,
                         lang_hint=hint if hint in ("de", "en") else "de", prev=prev[::-1], dur=dur))

    print(f"lab: {len(lab)} items · real: {len(real)} segments ({len(mic)} mic, {len(app)} app)\n")

    results: dict[str, dict] = {}
    for name in MODELS:
        if not MODELS[name][1].is_dir():
            print(f"{name}: missing"); continue
        t0 = time.time()
        out_lab = run_model(name, lab)
        out_real = run_model(name, real)
        results[name] = {"lab": out_lab, "real": {r["sid"]: r for r in out_real}}
        print(f"=== {name}  ({time.time()-t0:.0f}s wall)")
        print(f"  {'set':>8} {'WER':>7} {'RTF':>6}")
        for st in ("de_full", "de_3.0s", "de_1.5s", "en_full"):
            rs = [r for r in out_lab if r["set"] == st]
            if not rs:
                continue
            w = np.mean([wer_of(r["ref"], r["hyp"], r["lang"]) for r in rs])
            rtf = sum(r["dt"] for r in rs) / sum(r["dur_s"] for r in rs)
            print(f"  {st:>8} {w*100:6.1f}% {rtf:6.3f}")
        rr = list(results[name]["real"].values())
        rtf = sum(r["dt"] for r in rr) / sum(r["dur_s"] for r in rr)
        langs = [classify(r["hyp"]) for r in rr]
        agree = np.mean([classify(r["hyp"]) == r["lang_hint"] for r in rr if classify(r["hyp"]) in ("de", "en")])
        print(f"  real: RTF {rtf:.3f} · empty {langs.count('empty')/len(rr)*100:.0f}% · "
              f"lang matches context {agree*100:.0f}%\n")

    # ---------- agreement matrix on real audio ----------
    names = [n for n in MODELS if n in results]
    sids = set.intersection(*(set(results[n]["real"]) for n in names))
    print("pairwise agreement on real audio (1 − normalised word edit distance):")
    print("  " + " " * 18 + "".join(f"{n[:14]:>15}" for n in names))
    for a in names:
        row = f"  {a[:18]:<18}"
        for b in names:
            vals = []
            for s in sids:
                ha, hb = results[a]["real"][s]["hyp"], results[b]["real"][s]["hyp"]
                lang = results[a]["real"][s]["lang_hint"]
                vals.append(1 - min(1.0, wer_of(ha, hb, lang)))
            row += f"{np.mean(vals)*100:14.0f}%"
        print(row)

    # ---------- judge: validate on lab, then apply on real ----------
    if CLI.exists() and QWEN.exists():
        print("\njudge validation on FLEURS (does the LLM pick the lower-WER transcript?)")
        cur = "parakeet_v3"
        for other in names:
            if other == cur:
                continue
            ok = tot = 0
            for i, r in enumerate([x for x in results[cur]["lab"] if x["set"] == "de_3.0s"][:20]):
                o = next((y for y in results[other]["lab"] if y["path"] == r["path"] and y["set"] == r["set"]), None)
                if not o or r["hyp"] == o["hyp"]:
                    continue
                wa, wb = wer_of(r["ref"], r["hyp"], "de"), wer_of(r["ref"], o["hyp"], "de")
                if abs(wa - wb) < 0.05:
                    continue
                flip = rng.random() < 0.5
                a, b = (o["hyp"], r["hyp"]) if flip else (r["hyp"], o["hyp"])
                v = judge([], a, b)
                if v in ("A", "B"):
                    picked_other = (v == "A") == flip
                    ok += (picked_other == (wb < wa)); tot += 1
            print(f"  v3 vs {other:<18} judge agrees with WER {ok}/{tot}")

        print("\njudge on REAL audio — win-rate vs current (parakeet v3), context = 2 previous turns:")
        for other in names:
            if other == cur:
                continue
            wins = losses = same = 0
            for s in list(sids)[:JUDGE_N]:
                r, o = results[cur]["real"][s], results[other]["real"][s]
                if r["hyp"] == o["hyp"]:
                    same += 1; continue
                flip = rng.random() < 0.5
                a, b = (o["hyp"], r["hyp"]) if flip else (r["hyp"], o["hyp"])
                v = judge(r["prev"], a, b)
                if v == "same" or v is None:
                    same += 1
                elif (v == "A") == flip:
                    wins += 1
                else:
                    losses += 1
            n = wins + losses + same
            print(f"  {other:<18} better {wins/n*100:4.0f}%  worse {losses/n*100:4.0f}%  same {same/n*100:4.0f}%")

    # ---------- a handful of side-by-sides, mic first ----------
    print("\nside by side (mic = the user's own voice), 6 samples:")
    shown = 0
    for s in sids:
        r = results["parakeet_v3"]["real"][s]
        if r["kind_src"] != "mic" and shown < 4:
            continue
        print(f"\n  [{r['kind_src']}, {r['dur']:.1f}s]")
        for n in names:
            print(f"    {n[:16]:<16} {results[n]['real'][s]['hyp'][:110]}")
        shown += 1
        if shown >= 6:
            break
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
