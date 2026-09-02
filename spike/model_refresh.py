#!/usr/bin/env python3
"""Model refresh discipline: does a newer ASR checkpoint deserve to replace v3?

§11 answered "is there a better model?" once, by hand, in September 2026. The
answer will not stay true — sherpa-onnx ships new exports every few weeks — and
re-deciding it by hand every time is how a project ends up either chasing every
release or never looking again. This script is the standing procedure: it
discovers what is new, benches it against the incumbent on exactly the §11
measurements, and applies one written-down swap rule.

Three parts, run in order:

  discover  What sherpa-onnx ASR exports newer than the pinned parakeet v3
            (2025-08-16) are worth an evening of CPU? The GitHub `asr-models`
            release plus csukuangfj/* on Hugging Face, filtered by an
            ALLOWLIST OF FAMILIES (below) — multilingual (must cover de AND
            en), offline, int8 preferred, ≤ 1.5 GB. Everything else is listed
            with the reason it was skipped, so the skip is auditable rather
            than silent.

  bench     Every candidate plus the incumbent, on the §11 measurements:
            lab WER (FLEURS German + LibriSpeech English through Opus 24k),
            RTF on four pinned cores, empty-output rate, wrong-language rate
            on 1.5 s cuts (the failure that disqualified whisper-turbo), and —
            only for candidates that beat the incumbent on lab WER, because
            the judge is the expensive part — the real-audio LLM-judge pass.

  verdict   The swap rule, printed with the criterion that failed.

Everything runs at nice 19 on four pinned cores, sequentially: this box has
five other agents and a VR session on it, and a model that only wins when it
owns the machine has not won.

    cd spike
    taskset -c 16-19 nice -n 19 venv/bin/python model_refresh.py discover
    taskset -c 16-19 nice -n 19 venv/bin/python model_refresh.py bench
    taskset -c 16-19 nice -n 19 venv/bin/python model_refresh.py verdict

`bench` writes model_refresh/<date>.json and model_refresh/<date>.md;
`verdict` reads the newest of those. Nothing outside spike/ and the scratchpad
is written, and the live store (~/.local/share/nx-recall) is never opened.
"""

from __future__ import annotations

import argparse
import json
import os
import random
import re
import subprocess
import sys
import time
import urllib.request
from datetime import date
from pathlib import Path

import numpy as np
import soundfile as sf

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
from asr_multilang import load_wav_16k, norm_de, norm_en  # noqa: E402
from asr_spike import wer_counts  # noqa: E402
from harness import SR, extract_corpus, index_corpus, opus_roundtrip  # noqa: E402
from lang_flip import classify  # noqa: E402

S = Path(os.environ.get(
    "NXR_SCRATCH",
    "/tmp/claude-1000/-run-media-nerdrx-Lex-claude/61962651-5c6e-4a8f-93fe-d58f323a2b89/scratchpad",
))
M = S / "models"
OUT = HERE / "model_refresh"
CLIPS = HERE / "clips"
PARDL = S / "pardl.py"

# The judge runs from the scratchpad's own llama.cpp + Qwen copy. The daemon
# has an identical pair under ~/.local/share/nx-recall/models, but this bench
# never opens the live store.
LLM_DIR = S / "llm" / "llama-b10736"
LLAMA_CLI = LLM_DIR / "llama-cli"
QWEN = S / "llm" / "qwen3b.gguf"

CPUS = os.environ.get("NXR_CPUS", "16-19")
THREADS = int(os.environ.get("NXR_THREADS", "4"))
N_LAB = int(os.environ.get("N_LAB", "40"))
JUDGE_N = int(os.environ.get("JUDGE_N", "120"))
MAX_BYTES = int(1.5 * 1000 ** 3)          # a live model has to fit a download
INCUMBENT_DATE = "2025-08-16"             # parakeet v3's export date
GH_RELEASE = "https://api.github.com/repos/k2-fsa/sherpa-onnx/releases/tags/asr-models"
GH_DL = "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/"
HF_API = "https://huggingface.co/api/models?author=csukuangfj&search={}&limit=100"

INCUMBENT = "parakeet_v3"

# ---------------------------------------------------------------- swap rule
RULE = {
    "lab_wer_rel_gain": 0.10,   # pooled de+en lab WER, ≥10% relative better
    "rtf_max": 0.50,            # on 4 cores, real-clip audio
    "empty_slack_pp": 1.0,      # empty rate ≤ incumbent + 1 pp
    "judge_min": 0.55,          # ≥55% of DECIDED real-audio pairs
}

# ------------------------------------------------------------- the allowlist
#
# Families worth an evening of CPU. Everything not matched here is skipped by
# name, with the reason printed. `kind` names the sherpa-onnx constructor, so a
# family that needs a constructor this venv does not have fails loudly at
# discover time rather than three hours into a bench.
FAMILIES = [
    dict(key="parakeet_tdt_v_next", kind="transducer",
         match=r"^sherpa-onnx-nemo-parakeet-tdt-0\.6b-v([4-9]|\d\d)",
         why="a v3 successor is the drop-in swap — same constructor, same tokens"),
    dict(key="omnilingual_300m", kind="omnilingual_ctc",
         match=r"^sherpa-onnx-omnilingual-asr-1600-languages-300M-ctc",
         why="Meta's 1600-language CTC at parakeet-ish size; de+en covered"),
    dict(key="qwen3_asr", kind="qwen3",
         match=r"^sherpa-onnx-qwen3-asr-",
         why="LLM-decoder ASR: the class most likely to fix Denglisch, if it is fast enough"),
    dict(key="canary_1b_flash", kind="canary",
         match=r"^sherpa-onnx-nemo-canary-1b-flash",
         why="canary-180m was language-perfect but dropped 11% of turns; the 1b may not"),
    dict(key="whisper_turbo_class", kind="whisper",
         match=r"^sherpa-onnx-whisper-(turbo|large-v[4-9]|distil-large-v[4-9])",
         why="turbo-class successors only: v3/turbo/distil-v3.5 were already measured or are en-only"),
    dict(key="fast_conformer_4lang", kind="transducer",
         match=r"^sherpa-onnx-nemo-fast-conformer-transducer-en-de-es-fr",
         why="NeMo multilingual fast-conformer transducer (en/de/es/fr), int8 ~107 MB"),
    dict(key="fast_conformer_10lang", kind="transducer",
         match=r"^sherpa-onnx-nemo-fast-conformer-transducer-be-de-en",
         why="the same architecture over 10 European languages — more de data, same price"),
    dict(key="moonshine_multi", kind="moonshine_v2",
         match=r"^sherpa-onnx-moonshine-(base|small)-(multi|de)",
         why="moonshine is per-language today; a de or multilingual export would qualify"),
    dict(key="zipformer_multi", kind="transducer",
         match=r"^sherpa-onnx-zipformer-.*(de[-_]en|en[-_]de|multi)",
         why="a multilingual zipformer transducer would be the cheapest possible win"),
    dict(key="cohere_transcribe", kind="cohere",
         match=r"^sherpa-onnx-cohere-transcribe-",
         why="14 languages incl. de+en — kept in the allowlist, gated on the 1.5 GB size cap"),
]

# Structural skips, applied BEFORE the allowlist: nothing in these classes can
# ever be the live model, whatever family it belongs to.
STRUCTURAL_SKIP = [
    (r"\.wav$|\.mov$|checksum", "not a model"),
    (r"rk35\d\d|rk3588|ascend|qnn-|-android-|-linux-x64", "hardware-specific export (NPU/SoC build)"),
    (r"streaming", "streaming export; the daemon decodes finished segments offline"),
    (r"-fp16", "fp16 export; int8 preferred and a fp16 twin exists"),
]

# Reasons, for the report only — an asset reaches these lines because no family
# claimed it. Written as whole hyphen-separated tokens so that "languages" is
# not read as Spanish (it was, once).
SOFT_REASON = [
    (r"wenetspeech|paraformer|sense-voice|fire-red|telespeech|dolphin|giga-am|"
     r"t-one|medasr|shenava|vosk|kroko|funasr|x-asr|(^|-)(zh|ja|ko|yue|ru|fa|vi|"
     r"bn|uk|ar|th|id|pt|hi)(-|_|$)", "does not cover de+en"),
    (r"unified-en|nemotron|110m|gigaspeech|librispeech|distil-large|"
     r"(^|-)en-\d|(^|-)(en|es|fr|de)-\d+-", "English-only (or single-language) export"),
]


# ------------------------------------------------------------------ discover
def _get(url: str) -> object:
    req = urllib.request.Request(url, headers={"User-Agent": "nx-recall-model-refresh"})
    with urllib.request.urlopen(req, timeout=60) as r:
        return json.load(r)


def discover(verbose: bool = True) -> dict:
    OUT.mkdir(parents=True, exist_ok=True)
    cache = S / "asr_models_release.json"
    try:
        rel = _get(GH_RELEASE)
        cache.write_text(json.dumps(rel))
    except Exception as e:                                   # offline re-run
        if not cache.exists():
            raise
        print(f"  (GitHub unreachable: {e}; using cached {cache})")
        rel = json.loads(cache.read_text())

    picked: dict[str, dict] = {}
    skipped: list[tuple[str, str]] = []
    for a in rel["assets"]:
        name, when, size = a["name"], a["created_at"][:10], a["size"]
        if when <= INCUMBENT_DATE:
            continue
        hard = next((why for pat, why in STRUCTURAL_SKIP if re.search(pat, name, re.I)), None)
        if hard:
            skipped.append((name, hard))
            continue
        fam = next((f for f in FAMILIES if re.match(f["match"], name, re.I)), None)
        if not fam:
            why = next((w for pat, w in SOFT_REASON if re.search(pat, name, re.I)),
                       "not in the family allowlist")
            skipped.append((name, why))
            continue
        if size > MAX_BYTES:
            skipped.append((name, f"{size/1e9:.2f} GB > {MAX_BYTES/1e9:.1f} GB cap"))
            continue
        if "int8" not in name.lower():
            # keep it only if the family has no int8 export at all
            skipped.append((name, "float export; int8 twin preferred"))
            continue
        prev = picked.get(fam["key"])
        if prev is None or name > prev["name"]:              # newest date wins
            picked[fam["key"]] = dict(family=fam["key"], kind=fam["kind"], name=name,
                                      size=size, date=when, url=GH_DL + name, why=fam["why"])

    # Hugging Face: csukuangfj mirrors the same exports, but occasionally lands
    # a family there first. Listed for the human, never auto-benched.
    hf: list[dict] = []
    for term in ("parakeet", "canary", "omnilingual", "qwen3-asr", "moonshine"):
        try:
            for m in _get(HF_API.format(term)):
                mid, when = m["id"], m.get("lastModified", "")[:10]
                if when > INCUMBENT_DATE and "int8" in mid and "mlx" not in mid:
                    hf.append(dict(id=mid, date=when))
        except Exception as e:
            print(f"  (HF search '{term}' failed: {e})")

    known = {p["name"].replace(".tar.bz2", "") for p in picked.values()}
    hf_new = [h for h in hf if h["id"].split("/")[-1] not in known]

    res = dict(generated=date.today().isoformat(), incumbent_date=INCUMBENT_DATE,
               candidates=sorted(picked.values(), key=lambda c: c["date"]),
               hf_only=sorted(hf_new, key=lambda h: h["date"]),
               skipped=len(skipped), skipped_examples=skipped[:40])
    if verbose:
        print(f"candidates newer than parakeet v3 ({INCUMBENT_DATE}), "
              f"multilingual de+en, offline, int8, ≤{MAX_BYTES/1e9:.1f} GB:\n")
        print(f"  {'date':<11} {'size':>8}  {'family':<22} name")
        for c in res["candidates"]:
            print(f"  {c['date']:<11} {c['size']/1e6:7.1f}M  {c['family']:<22} {c['name']}")
        print(f"\n  {len(skipped)} assets skipped. By reason:")
        by: dict[str, int] = {}
        for _, why in skipped:
            by[why] = by.get(why, 0) + 1
        for why, n in sorted(by.items(), key=lambda kv: -kv[1]):
            print(f"    {n:4d}  {why}")
        print("\n  Hugging Face csukuangfj/* int8 exports not already covered above:")
        for h in hf_new or []:
            print(f"    {h['date']}  {h['id']}")
        if not hf_new:
            print("    (none)")
    (OUT / "discover.json").write_text(json.dumps(res, indent=1))
    return res


# ------------------------------------------------------------------ fetching
def fetch(c: dict) -> Path | None:
    """Download + extract into the scratchpad. pardl = 12 ranged connections,
    because the uplink shapes per connection, not per client."""
    d = M / c["name"].replace(".tar.bz2", "")
    if d.is_dir():
        return d
    tar = M / c["name"]
    if not tar.exists() or tar.stat().st_size != c["size"]:
        print(f"  downloading {c['name']} ({c['size']/1e6:.0f} MB)…", flush=True)
        r = subprocess.run([sys.executable, str(PARDL), c["url"], str(c["size"]), "12", str(tar)],
                           capture_output=True, text=True)
        if r.returncode != 0:
            print(f"  FAILED: {r.stdout}{r.stderr}")
            return None
    print(f"  extracting {c['name']}…", flush=True)
    subprocess.run(["tar", "xf", str(tar), "-C", str(M)], check=True)
    tar.unlink(missing_ok=True)          # the scratchpad is tmpfs = RAM
    return d if d.is_dir() else None


def sha256(p: Path) -> str:
    import hashlib
    h = hashlib.sha256()
    with p.open("rb") as f:
        for b in iter(lambda: f.read(1 << 20), b""):
            h.update(b)
    return h.hexdigest()


# ----------------------------------------------------------------- decoding
def make(kind: str, d: Path, threads: int):
    import sherpa_onnx as so

    def one(*globs):
        for g in globs:
            f = sorted(d.rglob(g))
            if f:
                return str(f[0])
        return None

    if kind == "transducer":
        return so.OfflineRecognizer.from_transducer(
            encoder=one("encoder*.onnx"), decoder=one("decoder*.onnx"),
            joiner=one("joiner*.onnx"), tokens=one("tokens*.txt"),
            num_threads=threads, model_type="nemo_transducer")
    if kind == "whisper":
        return so.OfflineRecognizer.from_whisper(
            encoder=one("*encoder.int8.onnx"), decoder=one("*decoder.int8.onnx"),
            tokens=one("*tokens.txt"), language="", task="transcribe", num_threads=threads)
    if kind == "canary":
        return so.OfflineRecognizer.from_nemo_canary(
            encoder=one("encoder*.onnx"), decoder=one("decoder*.onnx"),
            tokens=one("tokens*.txt"), src_lang="de", tgt_lang="de", num_threads=threads)
    if kind == "omnilingual_ctc":
        return so.OfflineRecognizer.from_omnilingual_asr_ctc(
            model=one("model.int8.onnx", "*.int8.onnx", "*.onnx"),
            tokens=one("tokens*.txt"), num_threads=threads)
    if kind == "qwen3":
        return so.OfflineRecognizer.from_qwen3_asr(
            conv_frontend=one("*conv*front*.onnx", "*frontend*.onnx"),
            encoder=one("encoder*.onnx"), decoder=one("decoder*.onnx"),
            tokenizer=one("tokenizer*.json", "tokens*.txt"), num_threads=threads)
    if kind == "moonshine_v2":
        return so.OfflineRecognizer.from_moonshine_v2(
            encoder=one("encoder*.onnx"), decoder=one("decoder*.onnx"),
            tokens=one("tokens*.txt"), num_threads=threads)
    if kind == "cohere":
        return so.OfflineRecognizer.from_cohere_transcribe(
            encoder=one("encoder*.onnx"), decoder=one("decoder*.onnx"),
            tokens=one("tokens*.txt"), language="", num_threads=threads)
    raise SystemExit(f"no constructor for kind={kind}; this venv's sherpa_onnx may be too old")


def decode(rec, x: np.ndarray) -> tuple[str, float]:
    st = rec.create_stream()
    st.accept_waveform(SR, x.astype(np.float32))
    t0 = time.time()
    rec.decode_stream(st)
    return st.result.text.strip(), time.time() - t0


# ---------------------------------------------------------------- the sets
def lab_set() -> list[dict]:
    """§11's lab set: 40 FLEURS German + 40 LibriSpeech English through Opus 24k,
    plus 1.5 s / 3 s cuts. Same seed, so the numbers are comparable to §11."""
    rng = random.Random(3)
    jobs = []
    rows = []
    for line in (S / "de" / "dev.tsv").read_text().splitlines():
        p = line.split("\t")
        if len(p) >= 3 and (S / "de" / "dev" / p[1]).is_file():
            rows.append((S / "de" / "dev" / p[1], p[2]))
    for path, ref in rng.sample(rows, N_LAB):
        jobs.append(dict(set="de_full", path=str(path), ref=ref, lang="de"))
        for dur in (1.5, 3.0):
            jobs.append(dict(set=f"de_{dur}s", path=str(path), ref=ref, lang="de",
                             dur=dur, off=rng.uniform(0.5, 6.0)))
    root = extract_corpus(S / "corpus" / "dev-clean.tar.gz", S / "corpus")
    trans = {}
    for f in root.rglob("*.trans.txt"):
        for line in f.read_text().splitlines():
            uid, _, text = line.partition(" ")
            trans[uid] = text
    by = index_corpus(root, 5)
    en = [(u.path, trans[u.path.stem]) for s in sorted(by) for u in by[s][3:4]][:N_LAB]
    rng2 = random.Random(11)
    for path, ref in en:
        jobs.append(dict(set="en_full", path=str(path), ref=ref, lang="en"))
        jobs.append(dict(set="en_1.5s", path=str(path), ref=ref, lang="en",
                         dur=1.5, off=rng2.uniform(0.3, 3.0)))
    return jobs


def real_set() -> list[dict]:
    """Real audio: the 193 single-speaker clips cut from the user's own 20-minute
    VRChat lobby recording (spike/clips, mean 2.45 s, §9's field validation set).

    §11 used 240 segments sampled live out of the product database. This bench
    deliberately does not go back to the live store — a re-runnable procedure
    must not depend on whatever happens to be in the user's recordings this
    quarter, and the refresh decision has to be reproducible across runs. These
    clips are the same audio in the same conditions (same lobby, same codec
    path, same crosstalk), frozen under spike/."""
    clips = sorted(CLIPS.glob("clip_*.wav"))
    return [dict(set="real", path=str(p), idx=i) for i, p in enumerate(clips)]


def _key(j: dict) -> str:
    return j["set"] + "|" + j["path"] + "|" + str(j.get("off", ""))


def audio_of(j: dict) -> np.ndarray | None:
    if j["set"] == "real":
        x, sr = sf.read(j["path"], dtype="float32")
        return x.mean(axis=1) if x.ndim > 1 else x
    x = load_wav_16k(Path(j["path"]))
    if j.get("dur"):
        off = j["off"]
        x = x[int(off * SR):int((off + j["dur"]) * SR)]
        if len(x) < SR // 2:
            return None
    return opus_roundtrip(x, 24)


# -------------------------------------------------------------- the metrics
def wer_counts_of(ref: str, hyp: str, lang: str) -> tuple[int, int]:
    n = norm_de if lang == "de" else norm_en
    sd, ins, nref = wer_counts(n(ref), n(hyp))
    return sd + ins, nref


def score(rows: list[dict]) -> dict:
    m: dict = {"sets": {}}
    err = words = 0
    for st in ("de_full", "de_3.0s", "de_1.5s", "en_full", "en_1.5s"):
        rs = [r for r in rows if r["set"] == st]
        if not rs:
            continue
        e = w = 0
        for r in rs:
            de, dw = wer_counts_of(r["ref"], r["hyp"], r["lang"])
            e += de
            w += dw
        m["sets"][st] = dict(n=len(rs), wer=e / max(1, w),
                             rtf=sum(r["dt"] for r in rs) / sum(r["dur_s"] for r in rs))
        if st in ("de_full", "en_full"):
            err += e
            words += w
    m["lab_wer_pooled"] = err / max(1, words)

    cuts = [r for r in rows if r["set"] in ("de_1.5s", "en_1.5s")]
    wrong = [r for r in cuts if classify(r["hyp"]) in ("de", "en")
             and classify(r["hyp"]) != r["lang"]]
    m["short_cut_n"] = len(cuts)
    m["wrong_lang_1_5s"] = len(wrong) / max(1, len(cuts))

    real = [r for r in rows if r["set"] == "real"]
    if real:
        m["real_n"] = len(real)
        m["real_rtf"] = sum(r["dt"] for r in real) / sum(r["dur_s"] for r in real)
        m["empty_real"] = sum(1 for r in real if not r["hyp"].strip()) / len(real)
    m["empty_lab"] = sum(1 for r in rows if r["set"] != "real" and not r["hyp"].strip()) \
        / max(1, len(rows) - len(real))
    return m


# ------------------------------------------------------------------- judge
JUDGE_SYS = (
    "You compare two automatic transcripts of the same short spoken turn from a "
    "chat lobby. Speech may be German, English or mixed. Given the two previous "
    "turns as context, decide which transcript is more likely what was actually "
    "said: coherent words, plausible in context, correct language. Output ONLY "
    "JSON: {\"better\": \"A\"|\"B\"|\"same\"}. Choose \"same\" only if both are "
    "equally plausible or equally unusable."
)
JUDGE_GBNF = ('root ::= "{" ws "\\"better\\":" ws ("\\"A\\"" | "\\"B\\"" | "\\"same\\"") ws "}"\n'
              'ws ::= [ \\t\\n]?\n')


def judge(context: list[str], a: str, b: str) -> str | None:
    gb = S / "judge.gbnf"
    if not gb.exists():
        gb.write_text(JUDGE_GBNF)
    prompt = ("Previous turns:\n" + "\n".join(f"- {c}" for c in context[-2:] if c) +
              f"\n\nTranscript A: {a}\nTranscript B: {b}")
    env = dict(os.environ, LD_LIBRARY_PATH=str(LLM_DIR))
    try:
        r = subprocess.run(
            ["taskset", "-c", CPUS, "nice", "-n", "19", str(LLAMA_CLI), "-m", str(QWEN),
             "-t", str(THREADS), "--temp", "0", "-n", "24", "--single-turn",
             "--grammar-file", str(gb), "-sys", JUDGE_SYS, "-p", prompt,
             "--no-display-prompt", "--no-warmup", "-ngl", "0"],
            capture_output=True, text=True, timeout=180, env=env)
    except subprocess.TimeoutExpired:
        return None
    m = re.search(r'"better"\s*:\s*"(A|B|same)"', r.stdout)
    return m.group(1) if m else None


def judge_pass(inc_rows: list[dict], cand_rows: list[dict], seed: int = 7) -> dict:
    """Blind A/B against the incumbent on the real clips, context = the two
    preceding clips as the incumbent decoded them (the clips are in recording
    order, so that is a genuine conversational context)."""
    rng = random.Random(seed)
    inc = {r["idx"]: r for r in inc_rows if r["set"] == "real"}
    cand = {r["idx"]: r for r in cand_rows if r["set"] == "real"}
    wins = losses = same = identical = 0
    for i in sorted(set(inc) & set(cand))[:JUDGE_N]:
        a_row, b_row = inc[i], cand[i]
        if a_row["hyp"] == b_row["hyp"]:
            identical += 1
            continue
        ctx = [inc[j]["hyp"] for j in (i - 2, i - 1) if j in inc and inc[j]["hyp"]]
        flip = rng.random() < 0.5
        a, b = (b_row["hyp"], a_row["hyp"]) if flip else (a_row["hyp"], b_row["hyp"])
        v = judge(ctx, a, b)
        if v in (None, "same"):
            same += 1
        elif (v == "A") == flip:                 # the candidate was shown as A
            wins += 1
        else:
            losses += 1
    decided = wins + losses
    return dict(wins=wins, losses=losses, same=same, identical=identical,
                decided=decided, win_rate=(wins / decided) if decided else None)


# -------------------------------------------------------------------- bench
def bench(only: list[str] | None = None) -> dict:
    OUT.mkdir(parents=True, exist_ok=True)
    try:
        os.nice(19 - os.nice(0))
        os.sched_setaffinity(0, {int(c) for c in _cpu_list(CPUS)})
    except OSError:
        pass

    disc = json.loads((OUT / "discover.json").read_text()) if (OUT / "discover.json").exists() \
        else discover(verbose=False)
    models = [dict(key=INCUMBENT, kind="transducer", name="sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8",
                   date=INCUMBENT_DATE, dir=str(M / "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8"),
                   size=None, incumbent=True)]
    for c in disc["candidates"]:
        if only and c["family"] not in only:
            continue
        d = fetch(c)
        if d is None:
            print(f"  {c['family']}: download failed, skipped")
            continue
        models.append(dict(key=c["family"], kind=c["kind"], name=c["name"], date=c["date"],
                           dir=str(d), size=c["size"], incumbent=False))

    lab, real = lab_set(), real_set()
    print(f"lab: {len(lab)} items · real: {len(real)} clips "
          f"({sum(sf.info(j['path']).duration for j in real):.0f}s of the user's lobby)\n")

    # Decode the audio once (the Opus round trip shells out to ffmpeg) and hand
    # every model the identical samples.
    audio: dict[str, np.ndarray] = {}
    for j in lab + real:
        k = _key(j)
        if k not in audio:
            x = audio_of(j)
            if x is not None:
                audio[k] = x

    out: dict[str, dict] = {}
    for mdl in models:
        t0 = time.time()
        try:
            rec = make(mdl["kind"], Path(mdl["dir"]), THREADS)
        except Exception as e:
            print(f"=== {mdl['key']}: LOAD FAILED — {type(e).__name__}: {str(e)[:300]}\n")
            out[mdl["key"]] = dict(meta=mdl, error=f"{type(e).__name__}: {e}")
            continue
        rows = []
        for n, j in enumerate(lab + real, 1):
            k = _key(j)
            x = audio.get(k)
            if x is None:
                continue
            hyp, dt = decode(rec, x)
            rows.append({**j, "hyp": hyp, "dt": dt, "dur_s": len(x) / SR})
            if n % 100 == 0:
                print(f"    {mdl['key']} {n}/{len(lab)+len(real)}  {time.time()-t0:.0f}s", flush=True)
        del rec
        m = score(rows)
        out[mdl["key"]] = dict(meta=mdl, metrics=m, rows=rows, wall_s=time.time() - t0)
        print(f"=== {mdl['key']}  ({time.time()-t0:.0f}s wall)")
        for st, v in m["sets"].items():
            print(f"  {st:>8} WER {v['wer']*100:6.1f}%  RTF {v['rtf']:7.3f}")
        print(f"  pooled lab WER {m['lab_wer_pooled']*100:.1f}%  ·  real RTF {m.get('real_rtf',0):.3f}"
              f"  ·  empty(real) {m.get('empty_real',0)*100:.0f}%"
              f"  ·  wrong-language on 1.5 s cuts {m['wrong_lang_1_5s']*100:.0f}%\n", flush=True)

    # the judge, only where it can change the answer
    inc = out.get(INCUMBENT, {})
    if "metrics" in inc and LLAMA_CLI.exists() and QWEN.exists():
        base = inc["metrics"]["lab_wer_pooled"]
        for key, r in out.items():
            if key == INCUMBENT or "metrics" not in r:
                continue
            if r["metrics"]["lab_wer_pooled"] >= base:
                r["judge"] = dict(skipped="does not beat the incumbent on lab WER")
                continue
            print(f"judge: {key} vs {INCUMBENT} on real clips…", flush=True)
            t0 = time.time()
            r["judge"] = judge_pass(inc["rows"], r["rows"])
            r["judge"]["wall_s"] = time.time() - t0
            print(f"  {r['judge']}\n", flush=True)
    else:
        for key, r in out.items():
            if key != INCUMBENT and "metrics" in r:
                r["judge"] = dict(skipped="judge LLM unavailable")

    stamp = date.today().isoformat()
    payload = dict(generated=stamp, cpus=CPUS, threads=THREADS, rule=RULE,
                   lab_n=len(lab), real_n=len(real),
                   models={k: {kk: vv for kk, vv in v.items() if kk != "rows"}
                           for k, v in out.items()},
                   transcripts={k: v.get("rows", []) for k, v in out.items()})
    (OUT / f"{stamp}.json").write_text(json.dumps(payload, ensure_ascii=False, indent=1))
    (OUT / f"{stamp}.md").write_text(table(payload))
    print(table(payload))
    print(f"\nwrote {OUT / (stamp + '.json')} and {OUT / (stamp + '.md')}")
    return payload


def _cpu_list(spec: str) -> list[int]:
    out: list[int] = []
    for part in spec.split(","):
        if "-" in part:
            a, b = part.split("-")
            out += list(range(int(a), int(b) + 1))
        else:
            out.append(int(part))
    return out


# ------------------------------------------------------------------- report
def table(p: dict) -> str:
    inc = p["models"].get(INCUMBENT, {}).get("metrics", {})
    L = [f"# ASR model refresh — {p['generated']}", "",
         f"Cores `{p['cpus']}` at nice 19, sequential, {p['threads']} ONNX threads. "
         f"Lab {p['lab_n']} items (FLEURS de + LibriSpeech en through Opus 24k), "
         f"real {p['real_n']} clips from the user's own lobby recording.", "",
         "| model | date | pooled lab WER | de full | en full | RTF (real, 4 cores) | "
         "empty (real) | wrong-lang 1.5 s | judge vs v3 (win/loss/decided) |",
         "|---|---|---:|---:|---:|---:|---:|---:|---|"]
    for k, r in p["models"].items():
        meta = r["meta"]
        if "metrics" not in r:
            L.append(f"| {k} | {meta['date']} | — | — | — | — | — | — | load failed: "
                     f"{r.get('error','')[:60]} |")
            continue
        m = r["metrics"]
        s = m["sets"]
        j = r.get("judge") or {}
        jt = j.get("skipped") or (
            f"{j.get('wins')} / {j.get('losses')} / {j.get('decided')}"
            + (f" ({j['win_rate']*100:.0f}%)" if j.get("win_rate") is not None else ""))
        star = " **(incumbent)**" if k == INCUMBENT else ""
        L.append(f"| {k}{star} | {meta['date']} | {m['lab_wer_pooled']*100:.1f}% | "
                 f"{s.get('de_full',{}).get('wer',0)*100:.1f}% | "
                 f"{s.get('en_full',{}).get('wer',0)*100:.1f}% | "
                 f"{m.get('real_rtf',0):.3f} | {m.get('empty_real',0)*100:.0f}% | "
                 f"{m['wrong_lang_1_5s']*100:.0f}% | {jt} |")
    L += ["", "## Verdict", "", "```", verdict(p, quiet=True), "```", ""]
    if inc:
        L += ["Short-cut WER (every model falls off the same cliff — §11, §12):", "",
              "| model | de 1.5 s | de 3 s | en 1.5 s |", "|---|---:|---:|---:|"]
        for k, r in p["models"].items():
            if "metrics" not in r:
                continue
            s = r["metrics"]["sets"]
            L.append(f"| {k} | {s.get('de_1.5s',{}).get('wer',0)*100:.1f}% | "
                     f"{s.get('de_3.0s',{}).get('wer',0)*100:.1f}% | "
                     f"{s.get('en_1.5s',{}).get('wer',0)*100:.1f}% |")
    return "\n".join(L) + "\n"


# ------------------------------------------------------------------ verdict
def verdict(p: dict | None = None, quiet: bool = False) -> str:
    if p is None:
        runs = sorted(OUT.glob("20*.json"))
        if not runs:
            return "no bench run found — run `model_refresh.py bench` first"
        p = json.loads(runs[-1].read_text())
    inc = p["models"].get(INCUMBENT, {}).get("metrics")
    if not inc:
        return "the incumbent did not bench; nothing to compare against"
    R = p.get("rule", RULE)
    L = [f"swap rule (all five must hold, else keep {INCUMBENT}):",
         f"  1. pooled lab WER (de+en) at least {R['lab_wer_rel_gain']*100:.0f}% relative better",
         f"  2. RTF on 4 cores ≤ {R['rtf_max']}",
         f"  3. wrong-language rate on 1.5 s cuts ≤ v3's ({inc['wrong_lang_1_5s']*100:.0f}%)",
         f"  4. empty-output rate ≤ v3's + {R['empty_slack_pp']:.0f} pp "
         f"({(inc.get('empty_real',0)*100)+R['empty_slack_pp']:.0f}%)",
         f"  5. the real-audio judge prefers it on ≥{R['judge_min']*100:.0f}% of DECIDED pairs",
         ""]
    winners = []
    for k, r in p["models"].items():
        if k == INCUMBENT:
            continue
        if "metrics" not in r:
            L.append(f"{k}: KEEP v3 — the model did not load ({r.get('error','')[:80]})")
            continue
        m = r["metrics"]
        j = r.get("judge") or {}
        gain = (inc["lab_wer_pooled"] - m["lab_wer_pooled"]) / max(1e-9, inc["lab_wer_pooled"])
        checks = [
            (gain >= R["lab_wer_rel_gain"],
             f"lab WER {m['lab_wer_pooled']*100:.1f}% vs {inc['lab_wer_pooled']*100:.1f}% "
             f"= {gain*100:+.0f}% relative, needs ≥{R['lab_wer_rel_gain']*100:.0f}%"),
            (m.get("real_rtf", 9) <= R["rtf_max"],
             f"RTF {m.get('real_rtf',0):.3f} on 4 cores, needs ≤{R['rtf_max']}"),
            (m["wrong_lang_1_5s"] <= inc["wrong_lang_1_5s"] + 1e-9,
             f"wrong-language on 1.5 s cuts {m['wrong_lang_1_5s']*100:.0f}% vs v3's "
             f"{inc['wrong_lang_1_5s']*100:.0f}%"),
            (m.get("empty_real", 1) <= inc.get("empty_real", 0) + R["empty_slack_pp"] / 100 + 1e-9,
             f"empty on real {m.get('empty_real',0)*100:.0f}% vs v3's "
             f"{inc.get('empty_real',0)*100:.0f}% + {R['empty_slack_pp']:.0f} pp"),
            (j.get("win_rate") is not None and j["win_rate"] >= R["judge_min"],
             (f"judge {j['win_rate']*100:.0f}% of {j['decided']} decided pairs"
              if j.get("win_rate") is not None
              else f"judge not run ({j.get('skipped','no result')})")),
        ]
        failed = [why for ok, why in checks if not ok]
        if failed:
            L.append(f"{k}: KEEP v3 — fails {len(failed)}/5: " + "; ".join(failed))
        else:
            winners.append(k)
            L.append(f"{k}: SWAP — clears all five: " + "; ".join(w for _, w in checks))
    L.append("")
    L.append(f"decision: {'SWAP to ' + winners[0] if winners else 'keep ' + INCUMBENT}")
    s = "\n".join(L)
    if not quiet:
        print(s)
    return s


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("cmd", choices=["discover", "bench", "verdict"])
    ap.add_argument("--only", nargs="*", help="bench only these families")
    a = ap.parse_args()
    if a.cmd == "discover":
        discover()
    elif a.cmd == "bench":
        bench(a.only)
    else:
        verdict()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
