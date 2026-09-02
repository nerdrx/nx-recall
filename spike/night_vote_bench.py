"""Can a third reading, on the GPU, fix the rows the cross-check calls shaky?

FINDINGS §11 parked the ensemble for want of a judge: three models disagree
with each other about half the time on real audio, and nothing in that table
said which of them to believe. §12 then found the one place where a third
reading is worth its cost anyway — the **shaky** bucket, where the live decoder
and the cross-check already disagree with each other, and where large-v3's own
disagreement with the live text runs at 76% against 27% on solid rows.

So this is not "swap the model". It is: for rows already flagged shaky, is
there a *vote* — live (parakeet v3), cross-check (canary 180m), night
(whisper-large-v3) — that lowers word error without wrecking the rows it
touches? Three rules are measured, and one of them is "do nothing but write it
down", because that is a real answer and it has to be on the table:

  (a) replace when large-v3 and canary agree with EACH OTHER (≥ TAU2) and both
      disagree with the live text. Two independent readings converging on the
      same words is the closest thing to a judge this system has.
  (b) replace when (a)'s condition holds OR the row is shaky and large-v3's
      OWN detected language matches the row's and its text clears the arbiter's
      guards (≥2 words, caption-stripped, language agrees).
  (c) never replace — store large-v3's reading as an annotation.

The hallucination case is why (b) carries the language guard at all. §12
measured whisper-large-v3 emitting Arabic, Finnish and Swedish on German lobby
audio: on the rows where the live decoder failed, the "better" model frequently
fails too, and in a way that is far more destructive because it produces
fluent, confident text in the wrong script. A vote that replaces a bad German
transcript with a good Arabic one has made the row worse, and the count of rows
made worse is therefore the gate's second number, not a footnote.

GATE: ship the rule that lowers WER on shaky rows by ≥30% relative while making
< 5% of the rows it touches worse. If none clears it, ship (c).

Two phases, because the GPU is the user's:

    # CPU: cut the lab set, decode v3 + canary, write one WAV per span
    taskset -c 16-31 nice -n 19 python spike/night_vote_bench.py cut
    # GPU: one whisper-cli invocation over all the spans (short, polite)
    python spike/night_vote_bench.py gpu
    # CPU: the vote
    python spike/night_vote_bench.py vote
    # and the same vote over the 160 real rows of spike/confidence_real.py
    python spike/night_vote_bench.py real
"""

from __future__ import annotations

import json
import os
import random
import subprocess
import sys
import time
import wave
from concurrent.futures import ProcessPoolExecutor
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).parent))
from asr_multilang import load_wav_16k, norm_de, norm_en  # noqa: E402
from asr_spike import wer_counts  # noqa: E402
from confidence_bench import agreement, make_canary  # noqa: E402
from context_redecode_bench import (  # noqa: E402
    LENGTHS, fleurs, libri, make_v3, span_reference, wer, words_with_times,
)
from harness import SR, opus_roundtrip  # noqa: E402

S = Path(os.environ.get("NXR_SCRATCH", "/tmp/nxr"))
CUTS = S / "nightcuts"
ITEMS = S / "night_items.json"
GPU_OUT = S / "night_gpu.json"
REAL = S / "confidence_real.json"
WHISPER = Path(os.environ.get("WHISPER_CLI", str(S / "whispercpp/build-vk/bin/whisper-cli")))
GGML = Path(os.environ.get("GGML_MODEL", str(S / "ggml/ggml-large-v3-q5_0.bin")))
TAU = float(os.environ.get("TAU", "0.5"))       # the daemon's shaky threshold
TAU2 = float(os.environ.get("TAU2", "0.5"))     # the vote's agreement threshold
N_UTTS = int(os.environ.get("N_UTTS", "60"))
# Cores and workers are environment, not policy: this box is somebody's desktop
# and the budget it can spare changes with what they are doing on it.
CORES = {int(c) for c in os.environ.get("NXR_CORES", "16-31").replace("-", " ").split()} \
    if "," in os.environ.get("NXR_CORES", "") else set(
        range(*(lambda a: (a[0], a[-1] + 1))(
            [int(x) for x in os.environ.get("NXR_CORES", "16-31").split("-")])))
WORKERS = int(os.environ.get("WORKERS", "12"))
THREADS = int(os.environ.get("THREADS", "1"))
# A ceiling on how many spans the third reading is asked for. Large-v3 is a
# gigabyte per process and this machine is somebody's desktop: the honest knob
# is "how much of it may this measurement have", not "how fast can it go".
MAX_SPANS = int(os.environ.get("MAX_SPANS", "0"))

_G: dict = {}


# --------------------------------------------------------------- phase: cut

def _init(jobs):
    try:
        os.nice(19 - os.nice(0))
        os.sched_setaffinity(0, CORES)
    except OSError:
        pass
    _G["v3"] = make_v3(1)
    _G["canary"] = {lang: make_canary(lang) for lang in ("de", "en")}
    _G["jobs"] = jobs


def _decode(rec, x) -> str:
    st = rec.create_stream()
    st.accept_waveform(SR, np.ascontiguousarray(x, dtype=np.float32))
    rec.decode_stream(st)
    return st.result.text.strip()


def write_wav(path: Path, x: np.ndarray) -> None:
    pcm = np.clip(x, -1.0, 1.0)
    pcm = (pcm * 32767.0).astype("<i2")
    with wave.open(str(path), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(SR)
        w.writeframes(pcm.tobytes())


def _cut(i: int) -> list[dict]:
    path, ref_text, lang = _G["jobs"][i]
    norm = norm_de if lang == "de" else norm_en
    rng = random.Random(hash(path) & 0xFFFF)
    x = opus_roundtrip(load_wav_16k(Path(path)), 24)
    dur = len(x) / SR
    ref_words = norm(ref_text)

    st = _G["v3"].create_stream()
    st.accept_waveform(SR, np.ascontiguousarray(x, dtype=np.float32))
    _G["v3"].decode_stream(st)
    full = st.result
    words = words_with_times(list(full.tokens), [float(t) for t in full.timestamps])
    if len(words) < 8:
        return []
    dec_norm = [norm(w)[0] if norm(w) else w.lower() for w in (w for w, _ in words)]

    items = [(0.0, dur, ref_words, "full")]
    for L in LENGTHS:
        cands = [k for k, (_, t) in enumerate(words) if t >= 0.3 and t + L <= dur - 0.3]
        if not cands:
            continue
        k0 = rng.choice(cands)
        t0, t1 = words[k0][1], words[k0][1] + L
        inside = [k for k, (_, t) in enumerate(words) if t0 <= t < t1]
        if len(inside) < 2:
            continue
        span = span_reference(dec_norm, ref_words, inside[0], inside[-1] + 1)
        if len(span) < 2:
            continue
        items.append((t0, t1, span, f"{L}s"))

    out = []
    for k, (t0, t1, ref_span, tag) in enumerate(items):
        seg = x[int(t0 * SR):int(t1 * SR)]
        if len(seg) < SR // 4:
            continue
        live = _decode(_G["v3"], seg)
        canary = _decode(_G["canary"][lang], seg)
        name = f"{Path(path).stem}-{tag}-{k}.wav"
        write_wav(CUTS / name, seg)
        out.append(dict(
            wav=name, lang=lang, tag=tag, dur=round((t1 - t0), 3),
            ref=" ".join(ref_span), live=live, canary=canary))
    return out


def phase_cut() -> int:
    CUTS.mkdir(parents=True, exist_ok=True)
    for old in CUTS.glob("*.wav"):
        old.unlink()
    jobs = fleurs(N_UTTS) + libri(N_UTTS // 2)
    t0 = time.time()
    with ProcessPoolExecutor(max_workers=WORKERS, initializer=_init, initargs=(jobs,)) as ex:
        got = [r for r in ex.map(_cut, range(len(jobs)), chunksize=2) if r]
    rows = [item for sub in got for item in sub]
    ITEMS.write_text(json.dumps(rows, indent=1))
    secs = sum(r["dur"] for r in rows)
    print(f"{len(rows)} spans, {secs:.0f}s of audio, cut in {time.time()-t0:.0f}s wall")
    return 0


# --------------------------------------------------------------- phase: gpu

def gpu_busy_pct() -> int:
    """The GPU's own utilisation counter. There is no rocm-smi on this box
    (ROCm here is the compiler stack only); amdgpu exports the same number the
    tool reads, and the daemon reads it the same way for the same reason."""
    best = 0
    for card in sorted(Path("/sys/class/drm").glob("card*/device/gpu_busy_percent")):
        try:
            vram = card.parent / "mem_info_vram_total"
            if vram.is_file() and int(vram.read_text()) < 8 << 30:
                continue  # the iGPU
            best = max(best, int(card.read_text().strip()))
        except (OSError, ValueError):
            pass
    return best


def run_whisper(wavs: list[Path], out_dir: Path, lang: str = "auto") -> float:
    """One whisper-cli invocation over many files: the model loads once, which
    is the whole reason the night shift batches. Returns wall seconds."""
    out_dir.mkdir(parents=True, exist_ok=True)
    # No `-of`: with many inputs each one writes its own `<input>.wav.json`
    # next to it, which is what `read_json_out` reads back.
    cmd = [str(WHISPER), "-m", str(GGML), "-l", lang, "-oj", "-np",
           *[str(w) for w in wavs]]
    t0 = time.time()
    proc = subprocess.run(cmd, capture_output=True, text=True)
    if proc.returncode != 0:
        print(proc.stderr[-2000:])
        raise SystemExit("whisper-cli failed")
    return time.time() - t0


def read_json_out(wav: Path) -> tuple[str, str]:
    """(text, detected language) from whisper-cli's -oj sidecar."""
    js = wav.with_suffix(wav.suffix + ".json")
    if not js.is_file():
        return "", ""
    d = json.loads(js.read_text())
    text = " ".join(seg.get("text", "") for seg in d.get("transcription", [])).strip()
    return " ".join(text.split()), (d.get("result", {}) or {}).get("language", "")


def phase_parity() -> int:
    """Which quantisation to ship: q5_0, or the fp16 the repository also carries?

    The bar is not "which is best" — it is **parity with the reading §11 and §12
    already used as their ceiling**, the sherpa int8 large-v3. Both ggml files
    and the sherpa export decode the same 20 FLEURS German utterances against
    the same human references. If q5_0 is level with the int8 the earlier
    findings were measured against, it is the one to ship: a third of the
    download and a third of the VRAM."""
    import sherpa_onnx as so

    jobs = fleurs(20)
    tmp = S / "parity"
    tmp.mkdir(parents=True, exist_ok=True)
    for old in tmp.glob("*"):
        old.unlink()
    refs, names = [], []
    for i, (path, ref, _) in enumerate(jobs):
        x = opus_roundtrip(load_wav_16k(Path(path)), 24)
        name = tmp / f"p{i:02d}.wav"
        write_wav(name, x)
        names.append(name)
        refs.append(norm_de(ref))
    secs = sum(len(load_wav_16k(n)) / SR for n in names)

    large = S / "models/sherpa-onnx-whisper-large-v3"
    rec = so.OfflineRecognizer.from_whisper(
        encoder=str(large / "large-v3-encoder.int8.onnx"),
        decoder=str(large / "large-v3-decoder.int8.onnx"),
        tokens=str(large / "large-v3-tokens.txt"),
        language="de", task="transcribe", num_threads=4)
    sherpa_wer = []
    for name, ref in zip(names, refs):
        st = rec.create_stream()
        st.accept_waveform(SR, np.ascontiguousarray(load_wav_16k(name), dtype=np.float32))
        rec.decode_stream(st)
        sherpa_wer.append(wer(ref, norm_de(st.result.text)))
    print(f"  {'model':<28} {'WER':>7} {'RTF':>7}")
    print(f"  {'sherpa int8 (the §11/§12 ceiling)':<28} "
          f"{sum(sherpa_wer)/len(sherpa_wer)*100:>6.1f}%       -")

    global GGML
    for model in ("ggml-large-v3-q5_0.bin", "ggml-large-v3.bin"):
        path = S / "ggml" / model
        if not path.is_file():
            continue
        GGML = path
        for old in tmp.glob("*.json"):
            old.unlink()
        wall = run_whisper(names, tmp, "de")
        got = [wer(ref, norm_de(read_json_out(n)[0])) for n, ref in zip(names, refs)]
        print(f"  {model:<28} {sum(got)/len(got)*100:>6.1f}% {wall/secs:>7.3f}")
    return 0


def phase_gpu() -> int:
    busy = gpu_busy_pct()
    if busy > 20:
        print(f"the GPU is {busy}% busy — the night shift would stand down; so does this")
        return 2
    wavs = sorted(CUTS.glob("*.wav"))
    rows = json.loads(ITEMS.read_text())
    secs = sum(r["dur"] for r in rows)
    wall = run_whisper(wavs, CUTS)
    got = {}
    for w in wavs:
        text, lang = read_json_out(w)
        got[w.name] = dict(night=text, night_lang=lang)
    GPU_OUT.write_text(json.dumps(got, indent=1))
    print(f"{len(wavs)} spans / {secs:.0f}s audio in {wall:.1f}s → RTF {wall/secs:.3f}")
    return 0


def _large_init():
    import sherpa_onnx as so

    try:
        os.nice(19 - os.nice(0))
        os.sched_setaffinity(0, CORES)
    except OSError:
        pass
    large = S / "models/sherpa-onnx-whisper-large-v3"
    _G["large"] = so.OfflineRecognizer.from_whisper(
        encoder=str(large / "large-v3-encoder.int8.onnx"),
        decoder=str(large / "large-v3-decoder.int8.onnx"),
        tokens=str(large / "large-v3-tokens.txt"),
        language="", task="transcribe", num_threads=THREADS)


def _large_one(name: str) -> tuple[str, str]:
    x = load_wav_16k(CUTS / name)
    st = _G["large"].create_stream()
    st.accept_waveform(SR, np.ascontiguousarray(x, dtype=np.float32))
    _G["large"].decode_stream(st)
    # sherpa's whisper wrapper detects the language but does not hand it back,
    # so the night language is left empty and the guard falls to the daemon's
    # own text classifier — which is the stricter of the two tests anyway.
    return name, st.result.text.strip()


def phase_cpu_large() -> int:
    """The third reading on the CPU, with the sherpa int8 large-v3.

    A substitution, declared: the daemon would run the ggml q5_0 build on the
    GPU, and this is the int8 ONNX export of the same weights that §11 and §12
    already used as their ceiling. It exists because the GPU on this machine
    belongs to somebody who is using it — the night shift's own rule is to
    stand down above 20% utilisation, and this branch honours that rule rather
    than measuring around it. What is under test here is the **vote**, and the
    vote reads text."""
    rows = json.loads(ITEMS.read_text())
    # The night queue is shaky rows, which are short turns; the full-utterance
    # items are in the cut set for calibration and are not what this pass is
    # about.
    spans = [r for r in rows if r["tag"] != "full"]
    if MAX_SPANS:
        spans = spans[:MAX_SPANS]
    # Resume: a pass that was stopped (this machine is somebody's desktop and
    # a measurement is not entitled to it) keeps what it already read.
    got = json.loads(GPU_OUT.read_text()) if GPU_OUT.is_file() else {}
    spans = [r for r in spans if r["wav"] not in got]
    names = [r["wav"] for r in spans]
    secs = sum(r["dur"] for r in spans)
    t0 = time.time()
    with ProcessPoolExecutor(max_workers=WORKERS, initializer=_large_init) as ex:
        for name, text in ex.map(_large_one, names, chunksize=2):
            got[name] = dict(night=text, night_lang="")
    GPU_OUT.write_text(json.dumps(got, indent=1))
    print(f"{len(got)} spans / {secs:.0f}s of audio in {time.time()-t0:.0f}s wall "
          f"({WORKERS} workers, 1 thread each, nice 19)")
    return 0


def phase_collect() -> int:
    """Gather whatever `-oj` sidecars are already next to the cuts.

    Split out from `gpu` because the decode and the reading of it are separate
    concerns the moment the decode is not this script's to schedule — on a box
    whose GPU belongs to somebody in a headset, the third reading may be
    produced by a `whisper-cli` run by hand, or on the CPU, or overnight."""
    got = {}
    for w in sorted(CUTS.glob("*.wav")):
        js = w.with_suffix(w.suffix + ".json")
        if not js.is_file():
            continue
        text, lang = read_json_out(w)
        got[w.name] = dict(night=text, night_lang=lang)
    GPU_OUT.write_text(json.dumps(got, indent=1))
    print(f"{len(got)} readings collected")
    return 0


# -------------------------------------------------------------- phase: vote

def strip_captions(text: str) -> str:
    """`crate::arbiter::strip_captions`, same rule: bracketed runs go whole."""
    out, depth = [], 0
    for ch in text:
        if ch in "([{<":
            depth += 1
        elif ch in ")]}>":
            depth = max(0, depth - 1)
        elif ch in "♪♫":
            pass
        elif depth == 0:
            out.append(ch)
    return " ".join("".join(out).split())


def nwords(text: str) -> int:
    return len([w for w in text.split() if any(c.isalnum() for c in w)])


_DE = set("""der die das und ist nicht ich du wir ihr sie es ein eine einen dem den mit von für
auf als auch aber wenn dann noch schon nur mal was wie wo ja nein doch beim vom zur zum über
unter zwischen gegen ohne durch""".split())
_EN = set("""the a an and is are was were not i you we they it this that of to in for on with as
at by from but if then just only what how where yes no about into over under between against
without through""".split())


def classify(text: str) -> str:
    """`crate::lang::classify`, ported. The point of it here: on Arabic,
    Finnish or Swedish hallucinations (§12) both stopword counts are zero and
    the answer is `unclear`, which is not a language and therefore not a match
    — which is exactly the guard that stops a hallucination replacing a row."""
    ws, cur = [], []
    for ch in text:
        if ch.isalpha() or ch in "'’":
            cur.append(ch.lower())
        elif cur:
            ws.append("".join(cur))
            cur = []
    if cur:
        ws.append("".join(cur))
    if not ws:
        return "empty"
    if any(c in "äöüß" for c in text.lower()):
        return "de"
    de = sum(1 for w in ws if w in _DE)
    en = sum(1 for w in ws if w in _EN)
    return "de" if de > en else "en" if en > de else "unclear"


def reads_as(text: str, lang: str) -> bool:
    """`crate::night::guards_pass`'s language half.

    Two tests, not one, and the second is the one that matters. `classify`
    settles on German the moment it sees an umlaut, which is right for a de/en
    question and wrong as a filter against a THIRD language: "Tack för att ni
    tittade" is Swedish with an ö in it and classifies as German — and it is a
    string large-v3 actually produced on this user's German audio (§12). So a
    replacement must also carry at least one stopword of the row's language."""
    if not lang or classify(text) != lang:
        return False
    ws, cur = [], []
    for ch in text:
        if ch.isalpha() or ch in "'’":
            cur.append(ch.lower())
        elif cur:
            ws.append("".join(cur))
            cur = []
    if cur:
        ws.append("".join(cur))
    de = sum(1 for w in ws if w in _DE)
    en = sum(1 for w in ws if w in _EN)
    mine, theirs = (de, en) if lang == "de" else (en, de)
    return mine >= 1 and mine >= theirs


def vote(live: str, canary: str, night: str, night_lang: str, lang: str,
         shaky: bool, rule: str, norm) -> tuple[str, str]:
    """(text to keep, why). The pure function the daemon ships as `judge_vote`."""
    lw, cw, nw = norm(live), norm(canary), norm(night)
    if not nw:
        return live, "night-empty"
    a_nc = agreement(nw, cw) if cw else 0.0
    a_nl = agreement(nw, lw)
    a_cl = agreement(cw, lw) if cw else 1.0
    two_of_three = bool(cw) and a_nc >= TAU2 and a_nl < TAU2 and a_cl < TAU2
    if rule == "c":
        return live, "annotate-only"
    if two_of_three:
        # Rule (g) is (a) with the arbiter's guards bolted on, and it is what
        # the daemon actually ships: two readings agreeing is the majority, and
        # the guards are the separate question of whether the winner is a
        # sentence in the row's language at all.
        if rule == "g":
            stripped = strip_captions(night)
            if nwords(stripped) >= 2 and reads_as(stripped, lang):
                return stripped, "two-agree"
            return live, "kept"
        return night, "two-agree"
    if rule == "b" and shaky:
        stripped = strip_captions(night)
        if nwords(stripped) >= 2 and reads_as(stripped, lang) and (
            not night_lang or night_lang == lang
        ):
            return stripped, "shaky-guarded"
    return live, "kept"


def score(rows: list[dict], rule: str) -> dict:
    before = after = touched = worse = better = 0.0
    nb = na = 0
    for r in rows:
        norm = norm_de if r["lang"] == "de" else norm_en
        ref = norm(r["ref"])
        if not ref:
            continue
        text, why = vote(r["live"], r["canary"], r.get("night", ""),
                         r.get("night_lang", ""), r["lang"], r["shaky"], rule, norm)
        w_before = wer(ref, norm(r["live"]))
        w_after = wer(ref, norm(text))
        before += w_before
        after += w_after
        nb += 1
        na += 1
        if why in ("two-agree", "shaky-guarded"):
            touched += 1
            if w_after > w_before + 1e-9:
                worse += 1
            elif w_after < w_before - 1e-9:
                better += 1
    return dict(n=nb, wer_before=before / max(1, nb), wer_after=after / max(1, na),
                touched=int(touched), worse=int(worse), better=int(better))


def mark_shaky(rows: list[dict]) -> None:
    for r in rows:
        norm = norm_de if r["lang"] == "de" else norm_en
        cw = norm(r["canary"])
        r["agree"] = agreement(norm(r["live"]), cw) if cw else None
        # An empty cross-check is "checked, no verdict" in the daemon, and the
        # night shift's queue is `asr_confidence = 'shaky'`, so those rows are
        # simply not in it.
        r["shaky"] = r["agree"] is not None and r["agree"] < TAU


def phase_vote() -> int:
    rows = json.loads(ITEMS.read_text())
    gpu = json.loads(GPU_OUT.read_text())
    # Only the spans that actually have a third reading. A row with no night
    # decode is not evidence about a vote, and averaging it in would dilute
    # both columns by exactly the same amount and make the table look calmer
    # than the measurement is.
    rows = [dict(r, **gpu[r["wav"]]) for r in rows if r["wav"] in gpu]
    mark_shaky(rows)
    shaky = [r for r in rows if r["shaky"]]
    solid = [r for r in rows if r["shaky"] is False and r["agree"] is not None]
    print(f"{len(rows)} spans · {len(shaky)} shaky · {len(solid)} solid "
          f"· {sum(1 for r in rows if r['agree'] is None)} no verdict\n")

    print(f"  {'rule':>6} {'set':>6} {'n':>4} {'WER before':>11} {'WER after':>10} "
          f"{'rel':>7} {'touched':>8} {'worse':>6} {'better':>7}")
    for rule in ("a", "g", "b", "c"):
        for label, rs in (("shaky", shaky), ("all", rows)):
            s = score(rs, rule)
            rel = (s["wer_before"] - s["wer_after"]) / max(1e-9, s["wer_before"])
            print(f"  {rule:>6} {label:>6} {s['n']:>4} {s['wer_before']*100:>10.1f}% "
                  f"{s['wer_after']*100:>9.1f}% {rel*100:>6.1f}% {s['touched']:>8} "
                  f"{s['worse']:>6} {s['better']:>7}")
    print("\nGATE: ≥30% relative on shaky, <5% of touched rows made worse.")
    for rule in ("a", "g", "b"):
        s = score(shaky, rule)
        rel = (s["wer_before"] - s["wer_after"]) / max(1e-9, s["wer_before"])
        hurt = s["worse"] / max(1, s["touched"])
        ok = rel >= 0.30 and hurt < 0.05
        print(f"  rule {rule}: {rel*100:.1f}% relative, {hurt*100:.1f}% of "
              f"{s['touched']} touched made worse → {'PASS' if ok else 'FAIL'}")
    return 0


# -------------------------------------------------------------- phase: real

def phase_real() -> int:
    """The 160 real rows of `confidence_real.py`. No reference exists there, so
    the honest outputs are a count and pairs to read.

    **What this leg can and cannot say.** The stored rows carry the live text,
    the daemon's own verdict and a large-v3 reading; they do NOT carry a canary
    reading, and the audio behind them lives in the user's live store, which
    this branch does not open. So rule (a)'s two-agree arm cannot be evaluated
    here at all, and what is measured is rule (b)'s second arm: shaky rows where
    large-v3's text clears the guards. The night reading is the sherpa int8
    large-v3 of §12 rather than the ggml build the daemon would run — a
    different quantisation of the same weights, which is a real difference and
    the reason this leg is for eyeballing, not for the gate."""
    rows = json.loads(REAL.read_text())["rows"]
    changed, pairs = 0, []
    for r in rows:
        lang = r.get("lang")
        norm = norm_de if lang != "en" else norm_en
        night = r.get("ref", "")
        shaky = r["asr_confidence"] == "shaky"
        # No canary text here: pass an empty one, which switches rule (a)'s arm
        # off by construction (`two_of_three` requires a cross-check reading).
        text, why = vote(r["text"], "", night, "", lang or "", shaky, "b", norm)
        if why == "shaky-guarded":
            changed += 1
            if len(pairs) < 25:
                pairs.append((r["id"], r["lang"], r["dur"], r["text"], text))
    n_shaky = sum(1 for r in rows if r["asr_confidence"] == "shaky")
    print(f"{len(rows)} real rows ({n_shaky} shaky) · rule (b)'s guarded arm "
          f"would replace {changed} ({changed / max(1, n_shaky) * 100:.0f}% of the shaky ones)\n")
    for sid, lang, dur, before, after in pairs:
        print(f"  #{sid} [{lang or '??'} {dur:.1f}s]")
        print(f"     live : {before}")
        print(f"     night: {after}")
    return 0


def main() -> int:
    phase = sys.argv[1] if len(sys.argv) > 1 else "vote"
    return {"cut": phase_cut, "gpu": phase_gpu, "vote": phase_vote,
            "parity": phase_parity, "collect": phase_collect,
            "cpu_large": phase_cpu_large,
            "real": phase_real}[phase]()


if __name__ == "__main__":
    raise SystemExit(main())
