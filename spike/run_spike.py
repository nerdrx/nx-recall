"""
NX Recall — Step 0: does timbre labelling survive a VRChat lobby?

Protocol
--------
Enrollment is CLEAN audio, probes are DEGRADED. That asymmetry is deliberate and
matches the design: you enroll people from good sources (Discord, close mic) and
then try to recognise them through VRChat's lossy, spatialised, many-talker mix.

Each speaker gets 3 clean enrollment prototypes (kept separate, not averaged, per
the design's 'multiple prototypes per person' rule). A probe scores against a
speaker as the MAX cosine over that speaker's prototypes.

Reported per condition:
  EER              equal error rate, the standard verification number
  TAR@FAR=1%       how much speech gets labelled when the threshold is set tight
                   enough to keep impostors out -- the anti-false-merge operating
                   point, and the number that decides whether this is usable
  rank-1           closed-set: is the best match in the whole gallery the target?
  steal rate       overlap only: how often the best match is another talker who is
                   actually IN the mix -- the realistic mislabel in a lobby
"""

from __future__ import annotations

import json
import os
import sys
import time
from concurrent.futures import ProcessPoolExecutor
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).parent))
from harness import (  # noqa: E402
    Embedder, Utt, eer, extract_corpus, index_corpus, load, mix,
    opus_roundtrip, rms_normalise, take_window, tar_at_far, trim_silence,
)

SCRATCH = Path(os.environ.get("NXR_SCRATCH", "/tmp/nxr"))
CORPUS_TAR = SCRATCH / "corpus" / "dev-clean.tar.gz"
CORPUS_DIR = SCRATCH / "corpus"
MODELS = {
    "eres2net": SCRATCH / "models" / "eres2net_en.onnx",
    "titanet_small": SCRATCH / "models" / "titanet_small.onnx",
}

N_SPEAKERS = 40
N_ENROLL = 3
N_PROBE = 5
SEED = 0xC0FFEE

# VRChat-plausible voice bitrates. Swept rather than guessed at.
BITRATES = [32, 24, 16, 12, 8]
DURATIONS = [0.5, 1.0, 2.0, 3.0, 5.0, 8.0]
OVERLAPS = [1, 2, 3, 4, 6, 8, 10]
DOMINANCES = [0.0, 6.0, 12.0]

REF_BITRATE = 24
REF_DUR = 3.0

_G: dict = {}


def _init_worker(model_path: str, utt_paths: list, seed: int):
    """Each worker holds its own extractor and its own copy of the trimmed audio."""
    _G["emb"] = Embedder(Path(model_path), num_threads=1)
    _G["audio"] = [rms_normalise(trim_silence(load(Path(p)))) for p in utt_paths]
    _G["rng"] = np.random.default_rng(seed)


def _build_probe(spec: dict) -> np.ndarray:
    """Realise one probe: window each source, codec each source, then mix."""
    rng = np.random.default_rng(spec["seed"])
    audio = _G["audio"]
    dur, br = spec["dur"], spec["bitrate"]

    def prep(idx: int) -> np.ndarray:
        w = take_window(audio[idx], dur, rng)
        return opus_roundtrip(w, br) if br else w

    target = prep(spec["target_utt"])
    itfs = [prep(i) for i in spec["itf_utts"]]
    return mix(target, itfs, spec["dominance"])


def _run_probe(spec: dict) -> dict:
    v = _G["emb"](_build_probe(spec))
    return {**spec, "vec": v.tolist()}


def _embed_enroll(spec: dict) -> dict:
    x = _G["audio"][spec["utt"]]
    return {**spec, "vec": _G["emb"](x).tolist()}


def score(probes: list[dict], protos: dict[str, np.ndarray], speakers: list[str]) -> dict:
    """Turn raw probe embeddings into the four headline numbers."""
    P = np.stack([protos[s] for s in speakers])          # (S, K, D)
    same, diff, rank1, steal, n = [], [], 0, 0, 0

    for pr in probes:
        v = np.asarray(pr["vec"], dtype=np.float32)
        sims = np.einsum("skd,d->sk", P, v).max(axis=1)  # best prototype per speaker
        tgt = speakers.index(pr["target_spk"])
        present = {speakers.index(s) for s in pr["itf_spks"]}

        same.append(sims[tgt])
        # Impostor scores exclude anyone actually audible in the mix: scoring against
        # a talker who IS present would count a correct detection as a false accept.
        for j in range(len(speakers)):
            if j != tgt and j not in present:
                diff.append(sims[j])

        best = int(np.argmax(sims))
        rank1 += best == tgt
        steal += best in present
        n += 1

    same_a, diff_a = np.asarray(same), np.asarray(diff)
    e, e_thr = eer(same_a, diff_a)
    tar, t_thr = tar_at_far(same_a, diff_a, 0.01)
    return {
        "n": n, "eer": e, "eer_thr": e_thr,
        "tar_at_far1": tar, "far1_thr": t_thr,
        "rank1": rank1 / n, "steal": steal / n,
        "same_mean": float(same_a.mean()), "diff_mean": float(diff_a.mean()),
    }


def main() -> int:
    t0 = time.time()
    root = extract_corpus(CORPUS_TAR, CORPUS_DIR)
    by_spk = index_corpus(root, N_ENROLL + N_PROBE)
    speakers = sorted(by_spk)[:N_SPEAKERS]
    if len(speakers) < N_SPEAKERS:
        print(f"only {len(speakers)} speakers with enough audio", file=sys.stderr)
    print(f"corpus: {len(speakers)} speakers", flush=True)

    # Flat utterance table; workers index into it so paths cross process boundaries once.
    utts: list[Utt] = []
    enroll_idx: dict[str, list[int]] = {}
    probe_idx: dict[str, list[int]] = {}
    for s in speakers:
        chosen = by_spk[s][: N_ENROLL + N_PROBE]
        base = len(utts)
        utts.extend(chosen)
        enroll_idx[s] = list(range(base, base + N_ENROLL))
        probe_idx[s] = list(range(base + N_ENROLL, base + N_ENROLL + N_PROBE))
    paths = [str(u.path) for u in utts]

    rng = np.random.default_rng(SEED)
    conditions: list[tuple[str, dict]] = []
    for br in [0, *BITRATES]:
        conditions.append((f"codec/{'clean' if br == 0 else str(br) + 'k'}",
                           dict(dur=REF_DUR, bitrate=br, n_talkers=1, dominance=0.0)))
    for d in DURATIONS:
        conditions.append((f"dur/{d}s",
                           dict(dur=d, bitrate=REF_BITRATE, n_talkers=1, dominance=0.0)))
    for n in OVERLAPS:
        for dom in DOMINANCES:
            conditions.append((f"overlap/{n}tk_{int(dom)}dB",
                               dict(dur=REF_DUR, bitrate=REF_BITRATE,
                                    n_talkers=n, dominance=dom)))

    # Build every probe spec up front so all models see identical trials.
    specs: list[dict] = []
    for cname, c in conditions:
        for s in speakers:
            others = [o for o in speakers if o != s]
            for pi in probe_idx[s]:
                itf_spks = list(rng.choice(others, size=c["n_talkers"] - 1, replace=False)) \
                    if c["n_talkers"] > 1 else []
                specs.append(dict(
                    cond=cname, target_spk=s, target_utt=pi,
                    itf_spks=[str(x) for x in itf_spks],
                    itf_utts=[int(rng.choice(probe_idx[x])) for x in itf_spks],
                    dur=c["dur"], bitrate=c["bitrate"], dominance=c["dominance"],
                    seed=int(rng.integers(1 << 31)),
                ))
    print(f"conditions: {len(conditions)}  probes/model: {len(specs)}", flush=True)

    results: dict[str, dict] = {}
    for mname, mpath in MODELS.items():
        if not mpath.is_file():
            print(f"skip {mname}: missing {mpath}", file=sys.stderr)
            continue
        print(f"\n=== {mname} ===", flush=True)
        tm = time.time()
        with ProcessPoolExecutor(
            max_workers=min(24, os.cpu_count() or 8),
            initializer=_init_worker, initargs=(str(mpath), paths, SEED),
        ) as ex:
            en_specs = [dict(spk=s, utt=u) for s in speakers for u in enroll_idx[s]]
            protos_flat = list(ex.map(_embed_enroll, en_specs, chunksize=4))
            protos = {s: np.stack([np.asarray(r["vec"], dtype=np.float32)
                                   for r in protos_flat if r["spk"] == s])
                      for s in speakers}
            print(f"  enrolled {len(protos)} speakers x {N_ENROLL} prototypes "
                  f"({time.time() - tm:.0f}s)", flush=True)

            done, out = 0, []
            for r in ex.map(_run_probe, specs, chunksize=8):
                out.append(r)
                done += 1
                if done % 2000 == 0:
                    print(f"  {done}/{len(specs)} probes ({time.time() - tm:.0f}s)", flush=True)

        per_cond: dict[str, list] = {}
        for r in out:
            per_cond.setdefault(r["cond"], []).append(r)
        results[mname] = {c: score(v, protos, speakers) for c, v in sorted(per_cond.items())}

        # Persist raw embeddings so new metrics never require re-running inference.
        np.savez_compressed(
            Path(__file__).parent / f"vecs_{mname}.npz",
            probe=np.stack([np.asarray(r["vec"], dtype=np.float32) for r in out]),
            cond=np.array([r["cond"] for r in out]),
            target=np.array([r["target_spk"] for r in out]),
            itf=np.array(["|".join(r["itf_spks"]) for r in out]),
            protos=np.stack([protos[s] for s in speakers]),
            speakers=np.array(speakers),
        )
        print(f"  done in {time.time() - tm:.0f}s", flush=True)

    outp = Path(__file__).parent / "results.json"
    outp.write_text(json.dumps(
        {"config": {"speakers": len(speakers), "n_enroll": N_ENROLL, "n_probe": N_PROBE,
                    "ref_bitrate": REF_BITRATE, "ref_dur": REF_DUR, "seed": SEED},
         "results": results}, indent=2))
    print(f"\nwrote {outp}  (total {time.time() - t0:.0f}s)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
