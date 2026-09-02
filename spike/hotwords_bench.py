"""Can we bias the transducer toward a vocabulary of names, worlds and slang?

sherpa-onnx has contextual biasing (`hotwords_file` + `hotwords_score`) and
sherpa-rs 0.6.8 exposes all four knobs on `TransducerConfig`. The question this
script answers is not whether the API exists — it does — but whether it does
anything for the model NX Recall actually runs, the NeMo parakeet TDT export
(`model_type = "nemo_transducer"`).

Three probes, each in its own process because one of them takes the process
down with it:

  crash    hotwords_file + modeling_unit=bpe with no bpe_vocab — the obvious
           configuration, since the v3 tarball ships tokens.txt and nothing
           else. sherpa builds no BPE encoder and EncodeHotwords dereferences
           it: SIGSEGV, in-process, taking the daemon with it.
  encode   modeling_unit=cjkchar needs no BPE encoder and survives, but it
           encodes a hotword as single characters, which in this vocabulary
           are ids 7865+ — pieces the model essentially never emits. Most
           words do not encode at all ("Cannot find ID for token SNAPPED").
           A bpe.vocab SYNTHESISED from tokens.txt (piece, score = −id, which
           is the ordering a sentencepiece BPE vocab carries anyway) encodes
           cleanly, and that is the only configuration where biasing can be
           reaching the real token path.
  wer      the gate: ~40 LibriSpeech utterances containing a word that occurs
           exactly once in the whole dev-clean reference set, decoded with and
           without those words as hotwords at scores 1.5 / 2.0 / 3.0, plus 40
           utterances containing none of them (the false-positive control).
           Biasing only exists under modified_beam_search, so the run also
           measures modified_beam_search WITHOUT hotwords — otherwise the
           decoder change and the biasing are one number and neither can be
           blamed.

GATE: targeted-word recall +20% relative, and no more than 0.3 pp WER
regression on the control set.
"""

from __future__ import annotations

import os
import subprocess
import sys
import time
from collections import Counter
from concurrent.futures import ProcessPoolExecutor
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).parent))
from asr_multilang import load_wav_16k, norm_en  # noqa: E402
from asr_spike import wer_counts  # noqa: E402
from harness import SR, extract_corpus, opus_roundtrip  # noqa: E402

S = Path(os.environ.get("NXR_SCRATCH", "/tmp/nxr"))
V3 = S / "models" / "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8"
SCORES = (1.5, 2.0, 3.0)
N_TARGET = 40
N_CONTROL = 40
CORES = set(range(16, 32))

_G: dict = {}


def bpe_vocab_from_tokens(tokens: Path, dest: Path) -> Path:
    """A sentencepiece-shaped vocabulary reconstructed from tokens.txt.

    ssentencepiece wants `piece<TAB>score` and merges greedily by score. A BPE
    tokens.txt is already ordered by merge priority, so −id reproduces that
    ordering exactly; only the absolute values differ, and nothing reads them.
    Special tokens (`<unk>`, `<|de|>`, …) are dropped: they are not pieces."""
    lines = []
    for line in tokens.read_text().splitlines():
        piece, _, idx = line.rpartition(" ")
        if piece and not piece.startswith("<") and idx.isdigit():
            lines.append(f"{piece}\t{-int(idx)}")
    dest.write_text("\n".join(lines) + "\n")
    return dest


def make(hotwords: Path | None, score: float, unit: str, vocab: Path | None = None,
         beam: bool = False):
    import sherpa_onnx as so

    kw: dict = {}
    if beam or hotwords is not None:
        kw["decoding_method"] = "modified_beam_search"
    if hotwords is not None:
        kw.update(hotwords_file=str(hotwords), hotwords_score=score)
        if unit:
            kw["modeling_unit"] = unit
        if vocab is not None:
            kw["bpe_vocab"] = str(vocab)
    return so.OfflineRecognizer.from_transducer(
        encoder=str(V3 / "encoder.int8.onnx"), decoder=str(V3 / "decoder.int8.onnx"),
        joiner=str(V3 / "joiner.int8.onnx"), tokens=str(V3 / "tokens.txt"),
        num_threads=1, model_type="nemo_transducer", **kw)


# ------------------------------------------------------------- crash probe

PROBE = r'''
import sys, soundfile as sf, sherpa_onnx as so
from pathlib import Path
V3 = Path(sys.argv[1]); unit = sys.argv[2]; hw = Path(sys.argv[3]); vocab = sys.argv[4]
kw = dict(hotwords_file=str(hw), hotwords_score=3.0, decoding_method="modified_beam_search")
if unit != "-":
    kw["modeling_unit"] = unit
if vocab != "-":
    kw["bpe_vocab"] = vocab
rec = so.OfflineRecognizer.from_transducer(
    encoder=str(V3/"encoder.int8.onnx"), decoder=str(V3/"decoder.int8.onnx"),
    joiner=str(V3/"joiner.int8.onnx"), tokens=str(V3/"tokens.txt"),
    num_threads=1, model_type="nemo_transducer", **kw)
x, sr = sf.read(str(V3/"test_wavs/en.wav"), dtype="float32")
st = rec.create_stream(); st.accept_waveform(sr, x); rec.decode_stream(st)
print("TEXT", st.result.text)
'''


def crash_probe(tmp: Path, vocab: Path) -> None:
    hw = tmp / "probe_hotwords.txt"
    hw.write_text("Zoological\nUpholsterer\n")
    script = tmp / "probe_hotwords.py"
    script.write_text(PROBE)
    print("sherpa hotword configurations against nemo_transducer:")
    for unit, vb in (("bpe", "-"), ("cjkchar", "-"), ("-", "-"), ("bpe", str(vocab))):
        r = subprocess.run([sys.executable, str(script), str(V3), unit, str(hw), vb],
                           capture_output=True, text=True)
        out = next((ln[5:].strip() for ln in r.stdout.splitlines() if ln.startswith("TEXT")), "")
        skipped = sum(1 for ln in r.stderr.splitlines() + r.stdout.splitlines()
                      if "Cannot find ID" in ln)
        state = "SIGSEGV" if r.returncode in (-11, 139) else (
            "ok" if r.returncode == 0 else f"exit {r.returncode}")
        tag = f"unit={unit} vocab={'synth' if vb != '-' else 'none'}"
        print(f"  {tag:<26} {state:<8} unencodable {skipped}  {out[:50]}")


# --------------------------------------------------------------- wer probe

def _init(jobs, hotwords, score, unit, vocab, beam):
    try:
        os.nice(19 - os.nice(0))
        os.sched_setaffinity(0, CORES)
    except OSError:
        pass
    _G["rec"] = make(hotwords, score, unit, vocab, beam)
    _G["jobs"] = jobs


def _run(i: int):
    path, ref, targets = _G["jobs"][i]
    x = opus_roundtrip(load_wav_16k(Path(path)), 24)
    st = _G["rec"].create_stream()
    st.accept_waveform(SR, np.ascontiguousarray(x, dtype=np.float32))
    _G["rec"].decode_stream(st)
    hyp = st.result.text.strip()
    h = norm_en(hyp)
    r = norm_en(ref)
    sd, ins, n = wer_counts(r, h)
    hits = sum(1 for t in targets if t in h)
    return dict(path=path, hyp=hyp, wer=(sd + ins) / max(1, n), n=n,
                hits=hits, n_targets=len(targets), inject=sum(1 for t in targets if t in h))


def run_set(jobs, hotwords, score, unit, vocab=None, beam=False):
    with ProcessPoolExecutor(max_workers=12, initializer=_init,
                             initargs=(jobs, hotwords, score, unit, vocab, beam)) as ex:
        return list(ex.map(_run, range(len(jobs)), chunksize=2))


def main() -> int:
    tmp = S / "hotwords_gate"
    tmp.mkdir(exist_ok=True)
    vocab = bpe_vocab_from_tokens(V3 / "tokens.txt", tmp / "bpe.vocab")
    crash_probe(tmp, vocab)

    root = extract_corpus(S / "corpus" / "dev-clean.tar.gz", S / "corpus")
    trans: dict[str, str] = {}
    for f in root.rglob("*.trans.txt"):
        for line in f.read_text().splitlines():
            uid, _, text = line.partition(" ")
            trans[uid] = text
    counts: Counter[str] = Counter()
    for text in trans.values():
        counts.update(norm_en(text))
    wavs = {p.stem: p for p in root.rglob("*.flac")}

    # Utterances carrying a hapax (a word used exactly once in dev-clean) of at
    # least 7 letters — a proper noun or a rare word, the kind a glossary holds.
    target_jobs, rare = [], []
    for uid, text in sorted(trans.items()):
        if uid not in wavs:
            continue
        ws = [w for w in norm_en(text) if counts[w] == 1 and len(w) >= 7]
        if ws and len(target_jobs) < N_TARGET:
            target_jobs.append((str(wavs[uid]), text, ws))
            rare += ws
    used = {j[0] for j in target_jobs}
    control_jobs = []
    for uid, text in sorted(trans.items()):
        if uid not in wavs or str(wavs[uid]) in used:
            continue
        if any(w in rare for w in norm_en(text)):
            continue
        control_jobs.append((str(wavs[uid]), text, rare))
        if len(control_jobs) >= N_CONTROL:
            break

    hw = tmp / "hotwords.txt"
    # Cased as the reference has them: the BPE encoder is case-sensitive, and
    # an upper-cased glossary encodes into a different (rarer) token path.
    hw.write_text("\n".join(sorted({w.capitalize() for w in rare})) + "\n")
    print(f"\n{len(rare)} rare words over {len(target_jobs)} utterances, "
          f"{len(control_jobs)} control utterances\n")

    t0 = time.time()
    base_t = run_set(target_jobs, None, 0.0, "")
    base_c = run_set(control_jobs, None, 0.0, "")

    def agg(rs):
        return (float(np.mean([r["wer"] for r in rs])),
                sum(r["hits"] for r in rs) / max(1, sum(r["n_targets"] for r in rs)))

    bt_wer, bt_rec = agg(base_t)
    bc_wer, _ = agg(base_c)
    print(f"  {'config':>22} {'target WER':>11} {'recall':>8} {'control WER':>12} {'changed':>8}")
    print(f"  {'greedy, no hotwords':>22} {bt_wer*100:10.1f}% {bt_rec*100:7.1f}% "
          f"{bc_wer*100:11.1f}% {'—':>8}")

    def report(label, rt, rc):
        t_wer, t_rec = agg(rt)
        c_wer, _ = agg(rc)
        changed = sum(1 for a, b in zip(base_t, rt) if a["hyp"] != b["hyp"]) + \
            sum(1 for a, b in zip(base_c, rc) if a["hyp"] != b["hyp"])
        print(f"  {label:>22} {t_wer*100:10.1f}% {t_rec*100:7.1f}% "
              f"{c_wer*100:11.1f}% {changed:8d}")
        return t_rec, c_wer

    # The decoder change on its own, so the biasing is not blamed for it.
    beam_rec, beam_c = report("beam, no hotwords",
                              run_set(target_jobs, None, 0.0, "", beam=True),
                              run_set(control_jobs, None, 0.0, "", beam=True))

    passed = False
    for score in SCORES:
        rec, c_wer = report(f"beam + hotwords {score}",
                            run_set(target_jobs, hw, score, "bpe", vocab),
                            run_set(control_jobs, hw, score, "bpe", vocab))
        # Against the beam baseline: biasing must earn its own keep, and the
        # false-positive budget is measured against the same decoder.
        rel = (rec - beam_rec) / beam_rec if beam_rec else 0.0
        if rel >= 0.20 and (c_wer - beam_c) <= 0.003:
            passed = True

    print(f"\n  ({time.time()-t0:.0f}s wall) 'changed' counts transcripts differing from the "
          f"greedy run, out of {len(target_jobs) + len(control_jobs)}.")
    print(f"\nGATE (targeted recall +20% rel. over the same decoder, control WER +≤0.3 pp): "
          f"{'PASS' if passed else 'FAIL'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
