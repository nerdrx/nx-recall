#!/usr/bin/env python3
"""A Japanese decoder for NX Recall: which one, and what does it cost?

Two candidates, and the choice between them is the choice between a live CPU
re-decode and a GPU trip:

  * `sherpa-onnx-nemo-parakeet-tdt_ctc-0.6b-ja-35000-int8` — NVIDIA's Japanese
    Parakeet, published in the k2-fsa zoo. A **CTC** export (one
    `model.int8.onnx`, not the encoder/decoder/joiner triple the multilingual
    v3 uses), 655 MB on disk, CPU, so it can run on the live path exactly where
    the German arbiter runs.
  * `whisper-large-v3` q5_0 through the already-built Vulkan `whisper-cli`,
    forced to `-l ja`. The night shift's decoder, on the GPU, borrowed for an
    immediate re-decode.

WER is the wrong metric here and would flatter both of them into meaninglessness:
Japanese is written without spaces, so "words" are whatever the tokenizer felt
like. This measures **CER** — character-level edit distance after NFKC
normalisation and punctuation stripping — which is the standard for Japanese
and is the only number the two decoders can be compared on at all.

Lengths, as everywhere in this spike: full utterance and a 3 s centre cut,
because the median VRChat turn is 2.4-2.7 s (FINDINGS §10).

Usage:
  spike/venv/bin/python spike/asr_ja.py --n 200            # CPU only
  spike/venv/bin/python spike/asr_ja.py --n 60 --gpu       # + whisper-large-v3
"""

import argparse
import json
import pathlib
import subprocess
import sys
import tempfile
import time
import unicodedata

import numpy as np
import sherpa_onnx
import soundfile as sf

sys.path.insert(0, str(pathlib.Path(__file__).parent))
from lid_bench import FLEURS, LIVE, MODELS_JA, SAMPLE_RATE, centre_cut, load_utterances, read_audio

SCRATCH = pathlib.Path("/tmp/nx-recall-workspace/nx-scratch")
JA_CTC = MODELS_JA / "sherpa-onnx-nemo-parakeet-tdt_ctc-0.6b-ja-35000-int8"
WHISPER_CLI = LIVE / "whisper/whisper-cli"
WHISPER_MODEL = LIVE / "ggml-large-v3-q5_0.bin"

# Everything that is punctuation or a space, in either width. Stripped from
# both sides of the comparison: no decoder is scored on whether it felt like
# emitting a 、 and the reference's own punctuation is a transcription
# convention, not something anybody said.
PUNCT_CATEGORIES = {"Pc", "Pd", "Ps", "Pe", "Pi", "Pf", "Po", "Zs", "Zl", "Zp", "Cc"}


def normalise_ja(text: str) -> str:
    """NFKC, punctuation and whitespace gone, nothing else touched.

    NFKC first because half-width katakana and full-width digits are the same
    characters said the same way, and a decoder that picks the other width is
    not making an error anybody can hear.
    """
    text = unicodedata.normalize("NFKC", text)
    return "".join(c for c in text if unicodedata.category(c) not in PUNCT_CATEGORIES)


def edit_distance(a: str, b: str) -> int:
    """Levenshtein, two rows. Characters, because Japanese has no words."""
    if len(a) < len(b):
        a, b = b, a
    prev = list(range(len(b) + 1))
    for i, ca in enumerate(a, 1):
        cur = [i]
        for j, cb in enumerate(b, 1):
            cur.append(min(prev[j] + 1, cur[j - 1] + 1, prev[j - 1] + (ca != cb)))
        prev = cur
    return prev[-1]


def infix_distance(reference: str, hypothesis: str) -> int:
    """Edit distance of `hypothesis` against the best-matching SUBSTRING of
    `reference` — free start and free end on the reference side only.

    This is how a 3 s cut can be scored at all. FLEURS has no short utterances
    (the shortest Japanese one is 6.4 s, the median 12.5 s), so the live regime
    — a 2.4 s VRChat turn — can only be reached by cutting, and a cut has no
    reference of its own. Scoring it against the whole sentence would charge
    the decoder for the nine seconds it was never played.

    Infix alignment charges it for exactly what it got wrong inside the window
    and nothing else, which is the number the live path cares about.
    """
    m, n = len(reference), len(hypothesis)
    if n == 0:
        return 0
    # Row 0 is all zeros: an alignment may start anywhere in the reference for
    # free. The answer is the minimum over the last row: it may end anywhere.
    prev = [0] * (m + 1)
    for i in range(1, n + 1):
        cur = [i] + [0] * m
        for j in range(1, m + 1):
            cur[j] = min(
                prev[j] + 1,
                cur[j - 1] + 1,
                prev[j - 1] + (hypothesis[i - 1] != reference[j - 1]),
            )
        prev = cur
    return min(prev)


def fragment_cer(reference: str, hypothesis: str) -> tuple[int, int]:
    """`(errors, hypothesis characters)` for a cut, by infix alignment.

    Normalised by the HYPOTHESIS length, not the reference's: the reference is
    the whole sentence and the hypothesis is what the decoder made of the
    window, so the hypothesis is the only side whose length matches the audio
    that was actually decoded.
    """
    ref, hyp = normalise_ja(reference), normalise_ja(hypothesis)
    return infix_distance(ref, hyp), len(hyp)


def cer(reference: str, hypothesis: str) -> tuple[int, int]:
    """`(errors, reference characters)`, summed by the caller.

    Summed rather than averaged per utterance on purpose: a corpus CER is the
    total edits over the total characters, and averaging per-utterance rates
    lets one short line with a bad decode outweigh a paragraph.
    """
    ref, hyp = normalise_ja(reference), normalise_ja(hypothesis)
    return edit_distance(ref, hyp), len(ref)


def build_ja_ctc(threads: int):
    model = JA_CTC / "model.int8.onnx"
    tokens = JA_CTC / "tokens.txt"
    for p in (model, tokens):
        if not p.is_file():
            sys.exit(f"missing {p}")
    return sherpa_onnx.OfflineRecognizer.from_nemo_ctc(
        model=str(model), tokens=str(tokens), num_threads=threads, provider="cpu", debug=False
    )


def decode_ctc(rec, samples: np.ndarray) -> str:
    stream = rec.create_stream()
    stream.accept_waveform(SAMPLE_RATE, samples)
    rec.decode_stream(stream)
    return stream.result.text.strip()


def decode_whisper_gpu(clips: list[np.ndarray], tmpdir: pathlib.Path) -> tuple[list[str], float]:
    """One `whisper-cli` process per clip, forced to Japanese.

    Per clip rather than one packed batch: the night shift packs because it is
    re-reading a day's backlog and the model load dominates, while the thing
    being measured here is what ONE turn costs when the router sends it to the
    GPU right now. Packing would measure the wrong number.
    """
    texts, wall = [], 0.0
    for i, clip in enumerate(clips):
        wav = tmpdir / f"clip{i:04d}.wav"
        sf.write(str(wav), clip, SAMPLE_RATE)
        t0 = time.perf_counter()
        proc = subprocess.run(
            [
                str(WHISPER_CLI), "-m", str(WHISPER_MODEL),
                "-l", "ja", "-oj", "-np", "-nt", str(wav),
            ],
            env={"LD_LIBRARY_PATH": str(WHISPER_CLI.parent), "PATH": "/usr/bin:/bin"},
            capture_output=True, text=True, timeout=300,
        )
        wall += time.perf_counter() - t0
        js = wav.with_suffix(".wav.json")
        if js.is_file():
            data = json.loads(js.read_text(encoding="utf-8"))
            texts.append("".join(s["text"] for s in data.get("transcription", [])).strip())
        else:
            print(f"  no json for clip {i}: {proc.stderr[-200:]}", file=sys.stderr)
            texts.append("")
        wav.unlink(missing_ok=True)
        js.unlink(missing_ok=True)
    return texts, wall


def gpu_busy() -> int:
    try:
        return int(pathlib.Path("/sys/class/drm/card1/device/gpu_busy_percent").read_text())
    except OSError:
        return 0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--n", type=int, default=200)
    ap.add_argument("--threads", type=int, default=4)
    ap.add_argument("--gpu", action="store_true", help="also run whisper-large-v3 on the GPU")
    ap.add_argument("--gpu-n", type=int, default=60, help="utterances for the GPU run")
    ap.add_argument("--out", default=str(SCRATCH / "asr_ja.json"))
    args = ap.parse_args()

    items = load_utterances("ja_jp", args.n)
    clips = [(read_audio(p), text) for p, text in items]
    print(f"ja: {len(clips)} utterances", flush=True)

    lengths = [None, 3.0]
    results = {}

    # ---- the CPU candidate ------------------------------------------------
    rec = build_ja_ctc(args.threads)
    for length in lengths:
        label = "full" if length is None else f"{length}s"
        errs = chars = 0
        audio_s = wall = 0.0
        pairs = []
        for samples, ref in clips:
            cut = samples if length is None else centre_cut(samples, length)
            audio_s += len(cut) / SAMPLE_RATE
            t0 = time.perf_counter()
            hyp = decode_ctc(rec, cut)
            wall += time.perf_counter() - t0
            # A 3 s cut is scored against the whole sentence only if the whole
            # sentence is what it contains. It is not — so the cut rows measure
            # the decoder's behaviour on a fragment (does it produce kana at
            # all, does it hallucinate) and the FULL rows are the CER that
            # decides the model. Both are reported; only `full` is comparable
            # to a published number.
            e, c = cer(ref, hyp) if length is None else fragment_cer(ref, hyp)
            errs += e
            chars += c
            pairs.append({"ref": ref, "hyp": hyp})
        results[f"ja-parakeet-ctc|{label}"] = {
            "cer": errs / chars if chars else None,
            "rtf": wall / audio_s,
            "n": len(clips),
            "kana_rate": sum(1 for p in pairs if has_kana(p["hyp"])) / len(pairs),
            "empty_rate": sum(1 for p in pairs if not p["hyp"].strip()) / len(pairs),
            "pairs": pairs[:20],
        }
        r = results[f"ja-parakeet-ctc|{label}"]
        print(
            f"ja-parakeet-ctc {label:5s}: CER {fmt(r['cer'])}  RTF {r['rtf']:.3f}  "
            f"kana {r['kana_rate']:.1%}  empty {r['empty_rate']:.1%}",
            flush=True,
        )
    del rec

    # ---- the GPU candidate ------------------------------------------------
    if args.gpu:
        busy = gpu_busy()
        if busy >= 50:
            print(f"GPU is {busy}% busy — skipping the GPU run", file=sys.stderr)
        elif not WHISPER_CLI.is_file():
            print(f"no {WHISPER_CLI} — skipping the GPU run", file=sys.stderr)
        else:
            sub = clips[: args.gpu_n]
            with tempfile.TemporaryDirectory(dir=str(SCRATCH)) as td:
                tmp = pathlib.Path(td)
                for length in lengths:
                    label = "full" if length is None else f"{length}s"
                    cuts = [c if length is None else centre_cut(c, length) for c, _ in sub]
                    audio_s = sum(len(c) for c in cuts) / SAMPLE_RATE
                    texts, wall = decode_whisper_gpu(cuts, tmp)
                    errs = chars = 0
                    for (_, ref), hyp in zip(sub, texts):
                        e, c = cer(ref, hyp) if length is None else fragment_cer(ref, hyp)
                        errs += e
                        chars += c
                    key = f"whisper-large-v3-ja|{label}"
                    results[key] = {
                        "cer": errs / chars if chars else None,
                        "rtf": wall / audio_s,
                        "n": len(sub),
                        "kana_rate": sum(1 for t in texts if has_kana(t)) / len(texts),
                        "empty_rate": sum(1 for t in texts if not t.strip()) / len(texts),
                        "pairs": [
                            {"ref": r, "hyp": h} for (_, r), h in list(zip(sub, texts))[:20]
                        ],
                    }
                    r = results[key]
                    print(
                        f"whisper-large-v3-ja {label:5s}: CER {fmt(r['cer'])}  "
                        f"RTF {r['rtf']:.3f}  kana {r['kana_rate']:.1%}  "
                        f"empty {r['empty_rate']:.1%}",
                        flush=True,
                    )

    pathlib.Path(args.out).write_text(json.dumps(results, ensure_ascii=False, indent=1))
    print(f"\nwrote {args.out}")

    print("\n## The Japanese decoder\n")
    print("| decoder | length | CER | RTF | kana rate | empty |")
    print("|---------|--------|----:|----:|----------:|------:|")
    for key, r in results.items():
        model, label = key.split("|")
        print(
            f"| {model} | {label} | {fmt(r['cer'])} | {r['rtf']:.3f} | "
            f"{r['kana_rate']:.1%} | {r['empty_rate']:.1%} |"
        )


def has_kana(text: str) -> bool:
    return sum(1 for c in text if 0x3040 <= ord(c) <= 0x30FF) >= 2


def fmt(v):
    return "—" if v is None else f"{v:.1%}"


if __name__ == "__main__":
    main()
