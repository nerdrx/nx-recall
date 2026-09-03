#!/usr/bin/env python3
"""Is a forced-language re-decode worth it for fr/es/it? (FINDINGS §28, part two)

§23 asked this for Japanese and the answer was easy, because Japanese has a
*script*. A Japanese-only decoder handed German audio produces something with
no kana in it, `judge` throws it away, and a false positive from the identifier
costs one wasted decode and nothing else.

None of that holds here. Whisper forced to `fr` over German audio produces
**French words** — real ones, with French function words in them — and the
shipped `guess_other` will happily call the result French. The guard that made
the Japanese route safe does not exist for a Romance language, so the question
this script has to answer is not only "does the re-decode help a French turn"
but "what does it do to a German one it should never have touched".

So there are two halves, and the second is the one that decides the design:

  POSITIVES  fr/es/it cuts at 1.0/1.5/2.5 s. WER of the multilingual v3 alone
             against each forced re-decode, plus the empty rate. Gate: >= 30%
             relative WER improvement on the touched turns, <= 5% of touched
             turns made worse.

  NEGATIVES  de/en cuts at the same lengths, run through the WHOLE live route
             rather than through LID alone: decode with v3, apply `pre_route`
             (a transcript that already reads as German or English never
             reaches the identifier), ask LID, and only then re-decode and
             judge. That conditioning is not a convenience — it is most of the
             safety argument, and measuring the raw LID confusion instead would
             charge the router for turns it never sees.

Two backends for the re-decode:
  * whisper-base int8 on the CPU, forced to the language. Already on disk for
    the German arbiter (`--arbiter-de`), and the same weights take any language
    token, so fr/es/it cost zero new bytes.
  * whisper-large-v3 q5_0 on the GPU through the night-shift runtime, one
    `whisper-cli` per clip — the honest cost of re-decoding ONE turn now, not
    the packed batch the night shift uses.

Usage:
  <venv>/bin/python spike/asr_poly.py --n 150            # CPU halves
  <venv>/bin/python spike/asr_poly.py --gpu --gpu-n 40   # add the GPU rows
"""

import argparse
import json
import pathlib
import subprocess
import sys
import time
import unicodedata

import numpy as np
import soundfile as sf

sys.path.insert(0, str(pathlib.Path(__file__).parent))
from lid_bench import (  # noqa: E402
    LIVE,
    SAMPLE_RATE,
    build,
    centre_cut,
    identify,
    load_utterances,
    read_audio,
)

SCRATCH = pathlib.Path("/tmp/nx-recall-workspace/nx-scratch")
V3 = LIVE / "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8"
BASE = LIVE / "sherpa-onnx-whisper-base"
WHISPER_CLI = LIVE / "whisper/whisper-cli"
WHISPER_MODEL = LIVE / "ggml-large-v3-q5_0.bin"

POSITIVES = {"fr_fr": "fr", "es_419": "es", "it_it": "it"}
NEGATIVES = {"de_de": "de", "en_us": "en"}
LENGTHS = [1.0, 1.5, 2.5]
# The languages the router is allowed to re-decode into. A negative heard as
# anything outside this set is a turn nothing happens to, so it is not a false
# positive — which is why the set has to be fixed before the negatives are
# scored rather than chosen after.
ROUTED = ("fr", "es", "it", "pt", "nl", "pl")

PUNCT = {"Pc", "Pd", "Ps", "Pe", "Pi", "Pf", "Po", "Zs", "Zl", "Zp", "Cc", "Sk"}


# ---------------------------------------------------------------- scoring

def norm_words(text: str) -> list[str]:
    """Lower-cased words, punctuation and accents-as-punctuation removed.

    Case and punctuation are transcription conventions; nobody says them, and
    charging Whisper for capitalising a sentence the reference did not would
    measure the wrong thing. Accents are KEPT — in French and Spanish they are
    part of the word, and a decoder that drops them has made a real error.
    """
    text = unicodedata.normalize("NFC", text.lower())
    cleaned = "".join(" " if unicodedata.category(c) in PUNCT else c for c in text)
    return cleaned.split()


def infix_wer(reference: list[str], hypothesis: list[str]) -> tuple[int, int]:
    """`(errors, hypothesis words)` against the best-matching SUBSTRING of the
    reference — free start and free end on the reference side only.

    Same reasoning as §23's character version, in words: FLEURS has no short
    utterances, so a 1.5 s turn can only be reached by cutting, and a cut has
    no reference of its own. Scoring against the whole sentence would charge
    the decoder for the ten seconds it was never played.

    Normalised by the HYPOTHESIS length, because the hypothesis is the only
    side whose length matches the audio that was actually decoded. A decoder
    that returns nothing therefore scores zero errors over zero words and
    contributes to neither total — which is why `empty` is reported beside the
    WER and not folded into it.
    """
    m, n = len(reference), len(hypothesis)
    if n == 0:
        return 0, 0
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
    return min(prev), n


# ---------------------------------------------------------------- decoders

def make_v3(threads: int):
    import sherpa_onnx as so

    return so.OfflineRecognizer.from_transducer(
        encoder=str(V3 / "encoder.int8.onnx"),
        decoder=str(V3 / "decoder.int8.onnx"),
        joiner=str(V3 / "joiner.int8.onnx"),
        tokens=str(V3 / "tokens.txt"),
        num_threads=threads,
        model_type="nemo_transducer",
    )


def make_base(lang: str, threads: int):
    """whisper-base int8 with its language token nailed down.

    The same export the German arbiter loads. Whisper's language is a decoding
    parameter rather than a property of the weights, which is the whole reason
    one 29 MB encoder can serve every language this router might want.
    """
    import sherpa_onnx as so

    return so.OfflineRecognizer.from_whisper(
        encoder=str(BASE / "base-encoder.int8.onnx"),
        decoder=str(BASE / "base-decoder.int8.onnx"),
        tokens=str(BASE / "base-tokens.txt"),
        language=lang,
        task="transcribe",
        num_threads=threads,
        provider="cpu",
    )


def decode(rec, samples: np.ndarray) -> str:
    stream = rec.create_stream()
    stream.accept_waveform(SAMPLE_RATE, np.ascontiguousarray(samples, dtype=np.float32))
    rec.decode_stream(stream)
    return stream.result.text.strip()


def gpu_busy() -> int:
    try:
        return int(pathlib.Path("/sys/class/drm/card1/device/gpu_busy_percent").read_text())
    except OSError:
        return 0


def decode_gpu(clips: list[np.ndarray], lang: str, tmpdir: pathlib.Path):
    """One `whisper-cli` per clip, forced to `lang`. Returns texts and wall time."""
    texts, wall = [], 0.0
    tmpdir.mkdir(parents=True, exist_ok=True)
    for i, clip in enumerate(clips):
        wav = tmpdir / f"clip{i:04d}.wav"
        sf.write(str(wav), clip, SAMPLE_RATE)
        t0 = time.perf_counter()
        proc = subprocess.run(
            [str(WHISPER_CLI), "-m", str(WHISPER_MODEL),
             "-l", lang, "-oj", "-np", "-nt", str(wav)],
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


# ---------------------------------------------------------------- the runs

def corpus_for(mapping, n):
    out = {}
    for lang_dir, tag in mapping.items():
        items = load_utterances(lang_dir, n)
        if len(items) < n:
            print(f"WARNING: only {len(items)} utterances for {tag}", file=sys.stderr)
        out[tag] = [(read_audio(p), text) for p, text in items]
        print(f"{tag}: {len(out[tag])} utterances", flush=True)
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--n", type=int, default=150)
    ap.add_argument("--threads", type=int, default=4)
    ap.add_argument("--gpu", action="store_true")
    ap.add_argument("--gpu-n", type=int, default=40)
    ap.add_argument("--gpu-busy-max", type=int, default=50)
    ap.add_argument("--out", default=str(SCRATCH / "asr_poly.json"))
    ap.add_argument(
        "--reuse",
        help="load v3 and LID from a previous run's file instead of recomputing them; "
        "the GPU leg is the only thing that then costs anything",
    )
    args = ap.parse_args()

    rows = []  # every (half, truth, length, backend, clip) decision, flat

    pos = corpus_for(POSITIVES, args.n)
    neg = corpus_for(NEGATIVES, args.n)

    if args.reuse:
        prior = json.loads(pathlib.Path(args.reuse).read_text())
        v3_text = {
            (s["half"], s["truth"], s["length"], s["i"]): s["v3"] for s in prior["seen"]
        }
        heard = {
            (s["half"], s["truth"], s["length"], s["i"]): s["heard"] for s in prior["seen"]
        }
        rows.extend(prior["rows"])
        print(f"reusing {len(v3_text)} decodes and {len(rows)} rows", flush=True)
        run_gpu(args, pos, v3_text, heard, rows)
        finish(args, rows, v3_text, heard)
        return

    # ---- v3, the baseline and the thing the router reads ------------------
    # ONE model in memory at a time: v3 for every clip of both halves, then it
    # goes away before whisper-base is built.
    v3 = make_v3(args.threads)
    v3_text = {}
    for half, corpus in (("pos", pos), ("neg", neg)):
        for tag, items in corpus.items():
            for length in LENGTHS:
                t0 = time.perf_counter()
                for i, (clip, ref) in enumerate(items):
                    cut = centre_cut(clip, length)
                    v3_text[(half, tag, length, i)] = decode(v3, cut)
                print(f"v3 {half} {tag} {length}s: {time.perf_counter() - t0:.1f}s", flush=True)
    del v3

    # ---- LID, on the same cuts -------------------------------------------
    slid = build("whisper-tiny", args.threads)
    heard = {}
    for half, corpus in (("pos", pos), ("neg", neg)):
        for tag, items in corpus.items():
            for length in LENGTHS:
                for i, (clip, _) in enumerate(items):
                    heard[(half, tag, length, i)] = identify(slid, centre_cut(clip, length))
    del slid
    print("LID done", flush=True)

    # ---- whisper-base, forced, one language at a time ---------------------
    for lang in sorted({*POSITIVES.values(), *ROUTED}):
        # Only build a decoder some clip will actually be sent to.
        wanted = [
            (half, tag, length, i)
            for (half, tag, length, i), h in heard.items()
            if h == lang or (half == "pos" and tag == lang)
        ]
        if not wanted:
            continue
        rec = make_base(lang, args.threads)
        t0, chars = time.perf_counter(), 0.0
        for key in wanted:
            half, tag, length, i = key
            corpus = pos if half == "pos" else neg
            clip, ref = corpus[tag][i]
            cut = centre_cut(clip, length)
            chars += len(cut) / SAMPLE_RATE
            rows.append({
                "half": half, "truth": tag, "length": length, "i": i,
                "backend": "base", "forced": lang, "heard": heard[key],
                "ref": ref, "v3": v3_text[key], "hyp": decode(rec, cut),
            })
        print(
            f"base[{lang}]: {len(wanted)} clips, RTF {(time.perf_counter() - t0) / chars:.3f}",
            flush=True,
        )
        del rec

    run_gpu(args, pos, v3_text, heard, rows)
    finish(args, rows, v3_text, heard)


def run_gpu(args, pos, v3_text, heard, rows):
    """whisper-large-v3 on the GPU, positives only.

    Positives only, and a smaller n: the question the GPU rows answer is "would
    the bigger model have decoded this French turn better", and the negatives'
    safety is a property of the ROUTE rather than of the decoder — the judge
    rejects a bad re-decode whichever model produced it.

    The busy check is per block rather than once, because the GPU belongs to
    whatever the user is doing and a run that started under the ceiling can
    finish well over it.
    """
    if not args.gpu:
        return
    tmpdir = SCRATCH / "asr_poly_gpu"
    for tag in POSITIVES.values():
        for length in LENGTHS:
            busy = gpu_busy()
            if busy >= args.gpu_busy_max:
                print(f"GPU at {busy}% — skipping {tag} {length}s", flush=True)
                continue
            items = pos[tag][: args.gpu_n]
            clips = [centre_cut(c, length) for c, _ in items]
            texts, wall = decode_gpu(clips, tag, tmpdir)
            audio_s = sum(len(c) for c in clips) / SAMPLE_RATE
            print(
                f"gpu[{tag}] {length}s: {len(clips)} clips, RTF {wall / audio_s:.3f}, "
                f"gpu_busy was {busy}%",
                flush=True,
            )
            for i, ((_, ref), hyp) in enumerate(zip(items, texts)):
                rows.append({
                    "half": "pos", "truth": tag, "length": length, "i": i,
                    "backend": "gpu", "forced": tag,
                    "heard": heard[("pos", tag, length, i)],
                    "ref": ref, "v3": v3_text[("pos", tag, length, i)], "hyp": hyp,
                })


def finish(args, rows, v3_text, heard):
    # Every clip, re-decoded or not, so the Rust side can apply `pre_route`
    # and count the negatives the way the live route actually would. Without
    # this the file would only hold the clips something happened to, and the
    # denominator of the false-positive rate would be missing.
    seen = [
        {
            "half": half, "truth": tag, "length": length, "i": i,
            "v3": v3_text[(half, tag, length, i)],
            "heard": heard[(half, tag, length, i)],
        }
        for (half, tag, length, i) in sorted(v3_text, key=str)
    ]
    payload = {"rows": rows, "seen": seen, "lengths": LENGTHS, "routed": list(ROUTED)}
    pathlib.Path(args.out).write_text(json.dumps(payload, ensure_ascii=False))
    print(f"\nwrote {args.out} ({len(rows)} rows)")
    report(payload)


# ---------------------------------------------------------------- reporting

def report(payload):
    rows = payload["rows"]

    print("\n## Positives — WER of v3 alone against each forced re-decode\n")
    print("| lang | length | backend | n | v3 WER | re-decode WER | rel. | worse | empty |")
    print("|------|--------|---------|--:|-------:|--------------:|-----:|------:|------:|")
    for tag in ("fr", "es", "it"):
        for length in payload["lengths"]:
            for backend in ("base", "gpu"):
                sel = [
                    r for r in rows
                    if r["half"] == "pos" and r["truth"] == tag
                    and r["length"] == length and r["backend"] == backend
                    and r["forced"] == tag
                ]
                if not sel:
                    continue
                v3_e = v3_n = rd_e = rd_n = 0
                worse = empty = 0
                for r in sel:
                    ref = norm_words(r["ref"])
                    a, an = infix_wer(ref, norm_words(r["v3"]))
                    b, bn = infix_wer(ref, norm_words(r["hyp"]))
                    v3_e, v3_n = v3_e + a, v3_n + an
                    rd_e, rd_n = rd_e + b, rd_n + bn
                    if not norm_words(r["hyp"]):
                        empty += 1
                    # "Made worse" is per turn and is a rate comparison, since
                    # the two hypotheses have different lengths.
                    elif an and bn and (b / bn) > (a / an):
                        worse += 1
                v3w = v3_e / v3_n if v3_n else float("nan")
                rdw = rd_e / rd_n if rd_n else float("nan")
                rel = (v3w - rdw) / v3w if v3_n and v3w else float("nan")
                print(
                    f"| {tag} | {length}s | {backend} | {len(sel)} | {v3w:.1%} | {rdw:.1%} "
                    f"| {rel:+.1%} | {worse / len(sel):.1%} | {empty / len(sel):.1%} |"
                )

    print("\n## Negatives — raw LID confusion, before any of the route's guards\n")
    print("| length | source | n | heard as a routed language | re-decoded |")
    print("|--------|--------|--:|---------------------------:|-----------:|")
    routed = set(payload["routed"])
    for length in payload["lengths"]:
        for tag in ("de", "en"):
            seen = [
                s for s in payload["seen"]
                if s["half"] == "neg" and s["truth"] == tag and s["length"] == length
            ]
            hit = [s for s in seen if s["heard"] in routed]
            done = [
                r for r in rows
                if r["half"] == "neg" and r["truth"] == tag and r["length"] == length
            ]
            n = len(seen) or 1
            print(f"| {length}s | {tag} | {len(seen)} | {len(hit) / n:.1%} | {len(done)} |")

    print(
        "\nThese are the RAW numbers and they are not the ones that decide anything: "
        "the live route only\nasks the identifier about a turn whose transcript is "
        "already unreadable, and only keeps a re-decode\nthat the judge accepts. "
        "Both of those live in Rust (`lang::classify`, `lang::guess_other`), so the "
        "conditioned\nfalse-positive rate comes from:\n\n"
        "  NXR_POLY_SPIKE=" + str(SCRATCH / "asr_poly.json") + " \\\n"
        "    cargo test -p recalld --lib -- --ignored --nocapture the_spike_through_the_shipped_guards"
    )


if __name__ == "__main__":
    main()
