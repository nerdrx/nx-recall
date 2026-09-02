"""The narrow bet after global hotwords failed: re-read only the near-misses.

§12 gate 2 measured contextual biasing over the WHOLE vocabulary at +9.1%
relative recall against a +20% bar, with the glossary bleeding into unrelated
utterances at the strongest setting (control WER 8.3% → 29.4%). That is the
verdict on biasing *everything, always*.

This is the version that survives that verdict, and it is a different question.
A person corrects one transcript. The correction introduces a word W the decoder
has never produced. Somewhere else in the same session there are turns whose
live text contains a **near-miss** of W — the same word, heard wrong. Those, and
only those, are re-decoded, with W as the only hotword.

Three things change, and each of them is why the number might not be 9.1%:

  * **One hotword, not five hundred.** The glossary cannot bleed into an
    utterance when the glossary is one word long.
  * **The candidate set is filtered on evidence.** A turn is re-read only if it
    already contains something within edit distance 2 of W, so the bias is
    applied where the decoder was already close.
  * **The cost is bounded.** `modified_beam_search` costs 1.6 pp of WER, and
    here it is paid by a handful of rows rather than by every turn ever
    captured.

Method (LibriSpeech dev-clean through Opus 24k, as §12 ran it):

  1. Survey the rare words (≥8 letters, 2–8 dev-clean utterances) with the
     SHIPPED decoder — greedy, no hotwords, which is the live text the daemon
     holds. One occurrence of each is "the correction" and is not measured.
  2. Keep the 40 words the decoder ACTUALLY GETS WRONG most often. This step is
     the bench: chosen by rarity alone, the pool is transcribed correctly 98.4%
     of the time and every configuration scores 98.4% — a tautology, not a
     measurement. A glossary is for words a decoder is wrong about.
  3. Keep their utterances whose live text has a word within edit distance ≤2
     of W — the `segments.redecode` candidate set the feature would build.
  4. Re-decode those with `modified_beam_search` and W as the single hotword.
     Also re-decode them with `modified_beam_search` and NO hotwords, because
     otherwise the decoder change and the biasing are one number and neither
     can be blamed (§12's own lesson).
  5. Control: utterances that trip the SAME near-miss filter for W and do not
     contain W. A false trigger is what the filter costs when it is wrong, and
     it is the set where an injected glossary word does real damage.

GATE: targeted-word recall +30% relative over the live baseline, with control
WER within ±0.3 pp.

    NXR_SCRATCH=<scratch> taskset -c 16-31 nice -n 19 \\
        python3 spike/glossary_reread_bench.py
"""

from __future__ import annotations

import json
import os
import sys
from collections import Counter, defaultdict
from concurrent.futures import ProcessPoolExecutor
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).parent))
from asr_multilang import load_wav_16k, norm_en  # noqa: E402
from asr_spike import wer_counts  # noqa: E402
from harness import SR, extract_corpus, opus_roundtrip  # noqa: E402

S = Path(os.environ.get("NXR_SCRATCH", "/tmp/nxr"))
V3 = S / "models" / "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8"
CORES = set(range(16, 32))
WORKERS = int(os.environ.get("WORKERS", "8"))

N_WORDS = 40
SURVEY_CAP = int(os.environ.get("SURVEY_CAP", "700"))
CONTROL_PER_WORD = int(os.environ.get("CONTROL_PER_WORD", "25"))
MIN_OCC, MAX_OCC = 2, 8
MIN_LEN = 7
NEAR = 2               # the `segments.redecode` candidate rule: distance <= 2
SCORE = float(os.environ.get("HOTWORD_SCORE", "1.5"))  # §12's best setting

GATE_RECALL = 0.30     # +30% relative
GATE_CONTROL_PP = 0.3  # percentage points

_G: dict = {}


# ---------------------------------------------------------------------------
# the candidate rule, which is the feature
# ---------------------------------------------------------------------------

def edit_distance(a: str, b: str, cap: int = 3) -> int:
    """Levenshtein, bailing out once it is past `cap` — the daemon's own shape."""
    if abs(len(a) - len(b)) > cap:
        return cap + 1
    prev = list(range(len(b) + 1))
    for i, ca in enumerate(a, 1):
        cur = [i]
        for j, cb in enumerate(b, 1):
            cur.append(min(prev[j] + 1, cur[j - 1] + 1, prev[j - 1] + (ca != cb)))
        if min(cur) > cap:
            return cap + 1
        prev = cur
    return prev[-1]


def near_miss(words: list[str], target: str) -> bool:
    """Does this live transcript contain a word within `NEAR` edits of `target`?"""
    return any(edit_distance(w, target, NEAR) <= NEAR for w in words)


def bpe_vocab_from_tokens(tokens: Path, dest: Path) -> Path:
    """§12's synthesised sentencepiece vocabulary. Without it, `bpe` SIGSEGVs."""
    lines = []
    for line in tokens.read_text().splitlines():
        piece, _, idx = line.rpartition(" ")
        if piece and not piece.startswith("<") and idx.isdigit():
            lines.append(f"{piece}\t{-int(idx)}")
    dest.write_text("\n".join(lines) + "\n")
    return dest


# ---------------------------------------------------------------------------
# decoding
# ---------------------------------------------------------------------------

def make(mode: str, hotwords: Path | None, vocab: Path | None):
    import sherpa_onnx as so

    kw: dict = {}
    if mode != "greedy":
        kw["decoding_method"] = "modified_beam_search"
    if hotwords is not None:
        kw.update(hotwords_file=str(hotwords), hotwords_score=SCORE,
                  modeling_unit="bpe", bpe_vocab=str(vocab))
    return so.OfflineRecognizer.from_transducer(
        encoder=str(V3 / "encoder.int8.onnx"), decoder=str(V3 / "decoder.int8.onnx"),
        joiner=str(V3 / "joiner.int8.onnx"), tokens=str(V3 / "tokens.txt"),
        num_threads=1, model_type="nemo_transducer", **kw)


def _pin():
    try:
        os.nice(19 - os.nice(0))
        os.sched_setaffinity(0, CORES)
    except OSError:
        pass


def _init_plain(jobs, mode):
    _pin()
    _G["rec"] = make(mode, None, None)
    _G["jobs"] = jobs


def _decode(i: int) -> str:
    path, _ref, _w = _G["jobs"][i]
    x = opus_roundtrip(load_wav_16k(Path(path)), 24)
    st = _G["rec"].create_stream()
    st.accept_waveform(SR, np.ascontiguousarray(x, dtype=np.float32))
    _G["rec"].decode_stream(st)
    return st.result.text.strip()


def run_plain(jobs, mode: str) -> list[str]:
    if not jobs:
        return []
    with ProcessPoolExecutor(max_workers=WORKERS, initializer=_init_plain,
                             initargs=(jobs, mode)) as ex:
        return list(ex.map(_decode, range(len(jobs)), chunksize=1))


def _init_hot(jobs, tmp, vocab):
    _pin()
    _G["jobs"] = jobs
    _G["tmp"] = Path(tmp)
    _G["vocab"] = Path(vocab)
    _G["cache"] = {}


def _decode_hot(i: int) -> str:
    """One utterance, biased toward the ONE word the correction introduced.

    A recognizer per hotword rather than a hotword per call: sherpa fixes the
    hotword list at construction, which is exactly the shape the daemon has —
    the glossary re-read builds a decoder for one word and throws it away.
    """
    path, _ref, w = _G["jobs"][i]
    rec = _G["cache"].get(w)
    if rec is None:
        hw = _G["tmp"] / f"hw_{os.getpid()}_{w}.txt"
        hw.write_text(w.upper() + "\n")
        rec = make("beam", hw, _G["vocab"])
        _G["cache"][w] = rec
    x = opus_roundtrip(load_wav_16k(Path(path)), 24)
    st = rec.create_stream()
    st.accept_waveform(SR, np.ascontiguousarray(x, dtype=np.float32))
    rec.decode_stream(st)
    return st.result.text.strip()


def run_hot(jobs, tmp: Path, vocab: Path) -> list[str]:
    if not jobs:
        return []
    # Sorted by hotword so each worker builds few recognizers.
    with ProcessPoolExecutor(max_workers=WORKERS, initializer=_init_hot,
                             initargs=(jobs, str(tmp), str(vocab))) as ex:
        return list(ex.map(_decode_hot, range(len(jobs)), chunksize=4))


# ---------------------------------------------------------------------------

def score(hyps: list[str], jobs, want: bool) -> tuple[float, float]:
    """(recall of the target word, WER) over a decoded set."""
    hits = 0
    sd_ins = n = 0
    for hyp, (_p, ref, w) in zip(hyps, jobs):
        h = norm_en(hyp)
        if (w in h) == want:
            hits += 1
        s, ins, nn = wer_counts(norm_en(ref), h)
        sd_ins += s + ins
        n += nn
    return hits / max(1, len(jobs)), sd_ins / max(1, n)


def main() -> int:
    if not V3.is_dir():
        print(f"missing {V3}")
        return 2
    tmp = S / "glossary_gate"
    tmp.mkdir(exist_ok=True)
    vocab = bpe_vocab_from_tokens(V3 / "tokens.txt", tmp / "bpe.vocab")

    root = extract_corpus(S / "corpus" / "dev-clean.tar.gz", S / "corpus")
    trans: dict[str, str] = {}
    for f in root.rglob("*.trans.txt"):
        for line in f.read_text().splitlines():
            uid, _, text = line.partition(" ")
            trans[uid] = text
    wavs = {p.stem: p for p in root.rglob("*.flac")}

    counts: Counter[str] = Counter()
    where: dict[str, list[str]] = defaultdict(list)
    for uid, text in trans.items():
        for w in set(norm_en(text)):
            counts[w] += 1
            where[w].append(uid)

    pool = sorted(w for w, c in counts.items()
                  if MIN_OCC <= c <= MAX_OCC and len(w) >= MIN_LEN and w.isalpha())
    print(f"=== glossary re-read, parakeet v3 int8, Opus 24k, nice 19 ===")
    print(f"    {len(pool)} candidate rare words ({MIN_OCC}-{MAX_OCC} occurrences,"
          f" >={MIN_LEN} letters), hotword score {SCORE}\n")

    # Step 2: decode every utterance of the pool with the SHIPPED path. This is
    # the live text the daemon would be holding, and it is what the whole
    # measurement rests on: the feature only ever sees these words.
    survey = []
    for w in pool:
        uids = sorted(u for u in where[w] if u in wavs)
        for uid in uids[1:]:            # uids[0] is "the correction"
            survey.append((str(wavs[uid]), trans[uid], w))
    survey = survey[:SURVEY_CAP]
    print(f"    {len(survey)} neighbour utterances surveyed with the live decoder")
    live = run_plain(survey, "greedy")

    # Step 2b: keep only words the decoder ACTUALLY GETS WRONG somewhere.
    #
    # The first version of this bench skipped this step and measured nothing:
    # picking rare words by frequency alone gave a set the shipped decoder
    # already transcribed correctly 98.4% of the time, so every configuration
    # scored 98.4% and the gate was a tautology. A glossary is for the words a
    # decoder is wrong about; a bench that plants words it is right about is
    # measuring its own sampling.
    missed_by: dict[str, int] = defaultdict(int)
    for hyp, (_p, _r, w) in zip(live, survey):
        if w not in norm_en(hyp):
            missed_by[w] += 1
    words = sorted(missed_by, key=lambda w: -missed_by[w])[:N_WORDS]
    print(f"    {len(missed_by)} of them are missed somewhere; taking the"
          f" {len(words)} worst as the planted corrections")

    # Step 3: the candidate rule, over those words only. A turn is re-read
    # exactly when its LIVE text already holds something within two edits of
    # the corrected word — including the word itself, because the daemon cannot
    # know it is already right and the cost of re-reading it counts.
    keep_w = set(words)
    targets, target_live = [], []
    for hyp, job in zip(live, survey):
        if job[2] in keep_w and near_miss(norm_en(hyp), job[2]):
            targets.append(job)
            target_live.append(hyp)

    # Step 5: the control — the same filter tripped by an utterance that does
    # NOT contain the word. A false trigger is what the filter costs when it is
    # wrong, and it is the set where an injected glossary word does real damage.
    # It is a SECOND control: the first is the candidate set's own overall WER,
    # printed below, which is what biasing costs the rest of the sentence.
    pick = []
    seen = {(j[0], j[2]) for j in targets}
    for w in words:
        n = 0
        for uid, text in sorted(trans.items()):
            if n >= CONTROL_PER_WORD or uid not in wavs:
                break
            if w in norm_en(text) or (str(wavs[uid]), w) in seen:
                continue
            pick.append((str(wavs[uid]), text, w))
            n += 1
    control_hyps = run_plain(pick, "greedy")
    keep = [(j, h) for j, h in zip(pick, control_hyps) if near_miss(norm_en(h), j[2])]
    control = [j for j, _ in keep]
    control_live = [h for _, h in keep]

    print(f"    {len(targets)} utterances trip the near-miss filter — the candidate set")
    print(f"    {len(control)} false triggers out of {len(pick)} tried, for the control\n")
    if not targets:
        print("no candidates; the filter selected nothing and there is nothing to measure")
        return 2

    r_live, w_live = score(target_live, targets, True)
    beam = run_plain(targets, "beam")
    r_beam, w_beam = score(beam, targets, True)
    hot = run_hot(sorted(targets, key=lambda j: j[2]), tmp, vocab)
    hot_jobs = sorted(targets, key=lambda j: j[2])
    r_hot, w_hot = score(hot, hot_jobs, True)

    c_live, cw_live = score(control_live, control, False)
    c_beam_h = run_hot(sorted(control, key=lambda j: j[2]), tmp, vocab)
    c_jobs = sorted(control, key=lambda j: j[2])
    _c_hot, cw_hot = score(c_beam_h, c_jobs, False)
    injected = sum(1 for hyp, (_p, _r, w) in zip(c_beam_h, c_jobs)
                   if w in norm_en(hyp))

    rel = (r_hot - r_live) / max(1e-9, r_live) if r_live else float("inf")
    rel_beam = (r_hot - r_beam) / max(1e-9, r_beam) if r_beam else float("inf")
    dpp = (cw_hot - cw_live) * 100

    print(f"  {'configuration':<38} {'target recall':>14} {'target WER':>11}")
    print(f"  {'greedy — what ships (the live text)':<38} {r_live:>13.1%} {w_live:>10.1%}")
    print(f"  {'modified_beam_search, no hotword':<38} {r_beam:>13.1%} {w_beam:>10.1%}")
    print(f"  {'+ the one corrected word @' + str(SCORE):<38} {r_hot:>13.1%} {w_hot:>10.1%}")
    print(f"\n  relative recall vs the live text : {rel:+.1%}   <- the gate is +30%")
    print(f"  relative recall vs beam alone    : {rel_beam:+.1%}")
    print(f"  control WER  live {cw_live:.1%} -> re-read {cw_hot:.1%}  "
          f"({dpp:+.2f} pp)   <- the gate is +/-0.30 pp")
    print(f"  control utterances the word was injected into: {injected}/{len(control)}")

    passed = rel >= GATE_RECALL and abs(dpp) <= GATE_CONTROL_PP
    print(f"\n  VERDICT: {'SHIP' if passed else 'DO NOT SHIP'}")
    (tmp / "result.json").write_text(json.dumps({
        "pool": len(pool), "words": len(words), "surveyed": len(survey), "candidates": len(targets),
        "control": len(control), "score": SCORE,
        "recall_live": r_live, "recall_beam": r_beam, "recall_hot": r_hot,
        "wer_live": w_live, "wer_beam": w_beam, "wer_hot": w_hot,
        "control_wer_live": cw_live, "control_wer_hot": cw_hot,
        "control_injected": injected,
        "relative": rel, "relative_vs_beam": rel_beam, "control_pp": dpp,
        "passed": passed,
    }, indent=2))
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
