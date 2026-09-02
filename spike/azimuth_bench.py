"""Is there azimuth signal in a per-application stereo stream?

    python azimuth_bench.py <stereo.wav> [--label vrchat|discord] [--bank voicebank.json]
    python azimuth_bench.py --selftest        # synthetic positive/negative controls only

The hypothesis (FINDINGS §14): VRChat spatialises every remote player, so the
stereo stream the daemon taps carries, per talker, an interaural level
difference (ILD) and a small interaural time difference (ITD) that encode where
that player stands relative to the user's head. If true, azimuth is a free,
model-free feature that is *independent* of timbre — which is exactly what the
overlap gate's "who dominates" question is missing (FINDINGS §5: every
timbre-derived defence failed on the equal-loudness case).

What this measures, per 20 ms frame:

    ILD = 20·log10(rms_L / rms_R)          dB, positive = left
    ITD = GCC-PHAT argmax over ±1 ms       samples, positive = left leads

and then, per VAD turn, the median of each. The questions:

  A. Do turns cluster into a few stable angles? (1-D k-means on median ILD,
     silhouette score over k=2..6.) This must be TRUE on VRChat and FALSE on
     Discord — Discord is not spatialised, so it is the negative control.
  B. Do those clusters agree with who is actually speaking? (Label turns with
     the live ERes2Net voicebank, measure cluster purity.)

GATE: silhouette >= 0.5 AND purity >= 0.8 on VRChat turns the bank labels at
score >= 0.6. Anything less and azimuth is not a feature, it is a number.

`--selftest` runs the same estimators over synthetic panned signals so a
negative field result can be distinguished from a broken estimator: a bench
that cannot see a hard-panned sine cannot be trusted to report "no signal".

Read-only: never writes under ~/.local/share/nx-recall. Point --bank at a COPY.
"""

from __future__ import annotations

import argparse
import json
import math
import sys
from pathlib import Path

import numpy as np

SR = 16_000
FRAME_MS = 20
# ±1 ms of lag covers every plausible interaural delay (a head is ~0.7 ms wide)
# with room for the panner's own fractional delay, and refuses to lock onto a
# pitch period, which is the classic GCC-PHAT failure on voiced speech.
MAX_LAG_MS = 1.0

# --------------------------------------------------------------------------
# per-frame estimators
# --------------------------------------------------------------------------


def frame_ild_db(left: np.ndarray, right: np.ndarray, floor_db: float = 30.0) -> float:
    """Interaural level difference in dB. Positive means louder on the left.

    Clamped to ±`floor_db`: one silent channel is +inf dB, which is not an
    angle, it is a division. Hard-panned content saturates the clamp and that
    is the honest reading.
    """
    rl = float(np.sqrt(np.mean(left.astype(np.float64) ** 2)))
    rr = float(np.sqrt(np.mean(right.astype(np.float64) ** 2)))
    eps = 1e-12
    ild = 20.0 * math.log10((rl + eps) / (rr + eps))
    return max(-floor_db, min(floor_db, ild))


def frame_itd_samples(left: np.ndarray, right: np.ndarray, sr: int = SR) -> float:
    """GCC-PHAT interaural time difference, in samples. Positive = left leads.

    PHAT weighting (dividing the cross-spectrum by its own magnitude) is what
    makes this work on speech: it throws away the spectral envelope and keeps
    only phase, so the peak is the delay rather than the loudest formant.

    Two departures from the textbook form, both of which the selftest catches
    if they are removed:

    * **Regularised** — plain PHAT divides *every* bin by its own magnitude,
      including the ones holding nothing but numerical dust between a voice's
      harmonics. Those bins come back with unit magnitude and arbitrary phase,
      and the arbitrary phases sum coherently at lag 0, planting a spurious
      delta there that outvotes the true peak on roughly half of all frames
      (measured: a 4-sample delay read 0 on 50% of frames). Flooring the
      denominator at a fraction of the mean magnitude weights those bins down
      instead of up.
    * **Band-limited to 100–4000 Hz** — voice, and nothing else. Above the
      band there is no speech to delay; below it a 20 ms frame cannot resolve
      a phase slope anyway.
    """
    n = len(left)
    if n == 0:
        return 0.0
    nfft = 1 << (2 * n - 1).bit_length()
    # A Hann window: the frame is a truncation of a longer signal, and the
    # discontinuity at its edges is broadband, which PHAT then whitens up to
    # full weight.
    w = np.hanning(n)
    L = np.fft.rfft(left * w, nfft)
    R = np.fft.rfft(right * w, nfft)
    cross = L * np.conj(R)
    freqs = np.fft.rfftfreq(nfft, 1.0 / sr)
    band = (freqs >= 100.0) & (freqs <= 4000.0)
    cross = np.where(band, cross, 0.0)
    mag = np.abs(cross)
    ref = float(mag[band].mean()) if band.any() else 0.0
    if ref <= 0.0:
        return 0.0
    cross = cross / (mag + 0.05 * ref)
    cc = np.fft.irfft(cross, nfft)
    max_lag = int(round(MAX_LAG_MS * 1e-3 * sr))
    # Rotate so lag 0 is centred, then keep only the plausible window.
    cc = np.concatenate((cc[-max_lag:], cc[: max_lag + 1]))
    peak = int(np.argmax(cc))
    lag = peak - max_lag
    # Parabolic interpolation around the peak: at 16 kHz one sample is 62 µs,
    # which is a quarter of the whole useful range, so integer lags alone
    # quantise every angle into about five buckets.
    if 0 < peak < len(cc) - 1:
        y0, y1, y2 = cc[peak - 1], cc[peak], cc[peak + 1]
        denom = y0 - 2 * y1 + y2
        if abs(denom) > 1e-12:
            lag += 0.5 * (y0 - y2) / denom
    # irfft(L·conj(R)) peaks at the delay of L *behind* R, so the raw lag is
    # negative when the left channel leads. Flip it so the sign convention
    # matches ILD's: positive means left.
    return float(-lag)


def ild_to_deg(ild_db: float, full_scale_db: float = 20.0) -> float:
    """Map an ILD onto a nominal azimuth in degrees, positive = left.

    Deliberately a *nominal* angle, not a physical one. Recovering true azimuth
    needs the HRTF the game applied; what the pipeline actually needs is a
    stable, ordered coordinate that separates two talkers, and a clamped linear
    map gives that without pretending to a precision it does not have.
    """
    return max(-90.0, min(90.0, 90.0 * ild_db / full_scale_db))


def frames(stereo: np.ndarray, sr: int = SR):
    """Yield (start_sample, left, right) for each 20 ms frame."""
    hop = int(sr * FRAME_MS / 1000)
    n = len(stereo) // hop * hop
    for i in range(0, n, hop):
        blk = stereo[i : i + hop]
        yield i, blk[:, 0], blk[:, 1]


def frame_features(stereo: np.ndarray, sr: int = SR) -> dict[str, np.ndarray]:
    starts, ilds, itds, rms = [], [], [], []
    for i, l, r in frames(stereo, sr):
        mono = 0.5 * (l + r)
        starts.append(i)
        ilds.append(frame_ild_db(l, r))
        itds.append(frame_itd_samples(l, r, sr))
        rms.append(float(np.sqrt(np.mean(mono.astype(np.float64) ** 2))))
    return {
        "start": np.asarray(starts),
        "ild": np.asarray(ilds),
        "itd": np.asarray(itds),
        "rms": np.asarray(rms),
    }


# --------------------------------------------------------------------------
# turns
# --------------------------------------------------------------------------


def energy_vad(
    rms: np.ndarray,
    *,
    min_frames: int = 25,  # 0.5 s — the identity gate's own floor is 1 s
    hang_frames: int = 10,  # 200 ms of silence still belongs to the turn
) -> list[tuple[int, int]]:
    """Turns as [start_frame, end_frame) over an adaptive energy threshold.

    Silero would be better and the spike venv has no torch; the point here is
    to cut the stream into talker-sized pieces, and the clustering question is
    not sensitive to a few frames of boundary error. Threshold is the noise
    floor (10th percentile) plus 9 dB, which on a voice stream sits between
    room tone and speech.
    """
    if len(rms) == 0:
        return []
    db = 20.0 * np.log10(rms + 1e-12)
    floor = float(np.percentile(db, 10))
    peak = float(np.percentile(db, 95))
    if peak - floor < 6.0:
        return []  # nothing that looks like speech over noise
    thresh = floor + 9.0
    active = db > thresh

    turns: list[tuple[int, int]] = []
    i, n = 0, len(active)
    while i < n:
        if not active[i]:
            i += 1
            continue
        j = i
        gap = 0
        while j < n:
            if active[j]:
                gap = 0
            else:
                gap += 1
                if gap > hang_frames:
                    break
            j += 1
        end = j - gap
        if end - i >= min_frames:
            turns.append((i, end))
        i = j
    return turns


def turn_rows(feat: dict[str, np.ndarray], turns: list[tuple[int, int]]) -> list[dict]:
    """Median ILD/ITD per turn, over that turn's *loud* frames only.

    Taking the median over every frame in the turn lets the trailing quiet
    frames — whose ILD is noise divided by noise — drag the estimate toward
    zero, which is indistinguishable from a centred talker.
    """
    rows = []
    for a, b in turns:
        rms = feat["rms"][a:b]
        keep = rms >= np.percentile(rms, 40)
        if keep.sum() < 5:
            keep = np.ones_like(rms, dtype=bool)
        ild = feat["ild"][a:b][keep]
        itd = feat["itd"][a:b][keep]
        rows.append(
            {
                "frame_start": a,
                "frame_end": b,
                "duration_s": (b - a) * FRAME_MS / 1000.0,
                "ild_med": float(np.median(ild)),
                "ild_iqr": float(np.percentile(ild, 75) - np.percentile(ild, 25)),
                "itd_med": float(np.median(itd)),
                "itd_iqr": float(np.percentile(itd, 75) - np.percentile(itd, 25)),
                "deg": ild_to_deg(float(np.median(ild))),
            }
        )
    return rows


# --------------------------------------------------------------------------
# 1-D clustering + silhouette (no sklearn in the spike venv)
# --------------------------------------------------------------------------


def kmeans_1d(x: np.ndarray, k: int, iters: int = 100, seed: int = 0) -> np.ndarray:
    """Lloyd's algorithm on a scalar, seeded at evenly spaced quantiles.

    Quantile seeding rather than random restarts: in one dimension the optimum
    is an interval partition, so a monotone seed converges to it and the run is
    deterministic, which matters for a number that gates a design decision.
    """
    if len(x) < k:
        return np.zeros(len(x), dtype=int)
    qs = np.linspace(0, 100, k + 2)[1:-1]
    centres = np.percentile(x, qs).astype(float)
    labels = np.zeros(len(x), dtype=int)
    for _ in range(iters):
        d = np.abs(x[:, None] - centres[None, :])
        new = np.argmin(d, axis=1)
        if np.array_equal(new, labels):
            break
        labels = new
        for c in range(k):
            m = labels == c
            if m.any():
                centres[c] = float(np.mean(x[m]))
    return labels


def silhouette_1d(x: np.ndarray, labels: np.ndarray) -> float:
    """Mean silhouette over |x_i − x_j|. −1..1; above ~0.5 is real structure."""
    uniq = np.unique(labels)
    if len(uniq) < 2 or len(x) < 3:
        return -1.0
    d = np.abs(x[:, None] - x[None, :])
    out = []
    for i in range(len(x)):
        own = labels == labels[i]
        if own.sum() <= 1:
            out.append(0.0)
            continue
        a = d[i][own].sum() / (own.sum() - 1)
        b = min(d[i][labels == c].mean() for c in uniq if c != labels[i])
        denom = max(a, b)
        out.append(0.0 if denom == 0 else (b - a) / denom)
    return float(np.mean(out))


def best_k(x: np.ndarray, ks=(2, 3, 4, 5, 6)) -> tuple[int, float, np.ndarray]:
    best = (0, -1.0, np.zeros(len(x), dtype=int))
    for k in ks:
        if len(x) < k + 2:
            continue
        lab = kmeans_1d(x, k)
        if len(np.unique(lab)) < 2:
            continue
        s = silhouette_1d(x, lab)
        if s > best[1]:
            best = (k, s, lab)
    return best


def purity(cluster: np.ndarray, truth: list[str | None]) -> tuple[float, int]:
    """Share of labelled turns falling in their cluster's plurality speaker.

    Undefined when the clustering collapsed to one cluster: "every turn is in
    the plurality speaker's cluster" is then trivially near 1 and says nothing
    about azimuth. Returns NaN rather than a flattering number.
    """
    idx = [i for i, t in enumerate(truth) if t is not None]
    if not idx:
        return float("nan"), 0
    if len(np.unique(cluster)) < 2:
        return float("nan"), len(idx)
    total, hit = 0, 0
    for c in np.unique(cluster[idx]):
        members = [i for i in idx if cluster[i] == c]
        counts: dict[str, int] = {}
        for i in members:
            counts[truth[i]] = counts.get(truth[i], 0) + 1
        hit += max(counts.values())
        total += len(members)
    return hit / total, total


# --------------------------------------------------------------------------
# voicebank
# --------------------------------------------------------------------------


def load_bank(path: Path) -> list[tuple[str, np.ndarray]]:
    import base64

    out = []
    for r in json.loads(path.read_text()):
        v = np.frombuffer(base64.b64decode(r["vec"]), dtype="<f4").astype(np.float32)
        n = np.linalg.norm(v)
        out.append((r["name"], v / n if n > 0 else v))
    return out


def label_turns(
    mono: np.ndarray,
    rows: list[dict],
    bank: list[tuple[str, np.ndarray]],
    model: Path,
    min_score: float = 0.6,
) -> list[dict]:
    import sherpa_onnx

    x = sherpa_onnx.SpeakerEmbeddingExtractor(
        sherpa_onnx.SpeakerEmbeddingExtractorConfig(
            model=str(model), num_threads=1, debug=False, provider="cpu"
        )
    )
    hop = int(SR * FRAME_MS / 1000)
    names = [n for n, _ in bank]
    mat = np.stack([v for _, v in bank])
    for r in rows:
        seg = mono[r["frame_start"] * hop : r["frame_end"] * hop]
        s = x.create_stream()
        s.accept_waveform(sample_rate=SR, waveform=seg.astype(np.float32))
        s.input_finished()
        v = np.asarray(x.compute(s), dtype=np.float32)
        n = np.linalg.norm(v)
        v = v / n if n > 0 else v
        scores = mat @ v
        # A speaker's score is the best of its prototypes — identity::rank.
        bysp: dict[str, float] = {}
        for nm, sc in zip(names, scores):
            bysp[nm] = max(bysp.get(nm, -1.0), float(sc))
        top = max(bysp.items(), key=lambda kv: kv[1])
        r["bank_name"] = top[0]
        r["bank_score"] = top[1]
        r["label"] = top[0] if top[1] >= min_score else None
    return rows


# --------------------------------------------------------------------------
# synthetic controls
# --------------------------------------------------------------------------


def _voiceish(n: int, f0: float, seed: int) -> np.ndarray:
    """A harmonic stack under a syllable envelope — speech-shaped enough for
    GCC-PHAT and for an energy VAD, without needing a corpus."""
    rng = np.random.default_rng(seed)
    t = np.arange(n) / SR
    sig = np.zeros(n)
    for h in range(1, 12):
        sig += (1.0 / h) * np.sin(2 * np.pi * f0 * h * t + rng.uniform(0, 2 * np.pi))
    env = 0.5 + 0.5 * np.sin(2 * np.pi * 3.5 * t + rng.uniform(0, 2 * np.pi))
    sig *= env
    sig += 0.01 * rng.standard_normal(n)
    return (sig / (np.max(np.abs(sig)) + 1e-9)).astype(np.float32)


def pan(mono: np.ndarray, ild_db: float, itd_samples: float = 0.0) -> np.ndarray:
    """Amplitude-pan (and optionally delay) a mono signal into stereo."""
    gl = 10 ** (ild_db / 40.0)
    gr = 10 ** (-ild_db / 40.0)
    left, right = mono * gl, mono * gr
    d = int(round(abs(itd_samples)))
    if d:
        if itd_samples > 0:  # left leads
            right = np.concatenate((np.zeros(d, dtype=mono.dtype), right[:-d]))
        else:
            left = np.concatenate((np.zeros(d, dtype=mono.dtype), left[:-d]))
    return np.stack((left, right), axis=1)


def synthetic_lobby(minutes: float = 4.0, seed: int = 7) -> tuple[np.ndarray, list[tuple[int, int, str]]]:
    """A fake *spatialised* lobby: four talkers at four fixed angles, taking turns.

    This is the positive control for the whole bench, not just the estimators.
    Without it, a null on real audio has two explanations — "the stream is not
    spatialised" and "the pipeline cannot see spatialisation" — and only the
    first is a finding. Turn lengths, gaps and the room floor are deliberately
    sloppy; the angles are the only thing held still.
    """
    rng = np.random.default_rng(seed)
    people = [("A", 110.0, -16.0), ("B", 150.0, -6.0), ("C", 190.0, +6.0), ("D", 230.0, +16.0)]
    n = int(minutes * 60 * SR)
    out = np.zeros((n, 2), dtype=np.float32)
    truth: list[str] = []
    pos = 0
    while pos < n - SR * 4:
        name, f0, ild = people[rng.integers(len(people))]
        dur = int(rng.uniform(1.2, 4.0) * SR)
        dur = min(dur, n - pos - SR)
        v = _voiceish(dur, f0 * rng.uniform(0.97, 1.03), int(rng.integers(1 << 30)))
        out[pos : pos + dur] += pan(v * 0.3, ild + rng.normal(0, 0.8), ild / 4.0)
        truth.append((pos, pos + dur, name))
        pos += dur + int(rng.uniform(0.4, 1.5) * SR)
    out += (0.0015 * rng.standard_normal((n, 2))).astype(np.float32)
    return out, truth


def selftest() -> int:
    """Prove the estimators see what they claim to see, before trusting a null."""
    ok = True

    def check(name: str, got: float, want: float, tol: float) -> None:
        nonlocal ok
        good = abs(got - want) <= tol
        ok &= good
        print(f"  [{'ok ' if good else 'FAIL'}] {name:38s} {got:+8.2f} (want {want:+.2f} ±{tol})")

    n = SR * 3
    v = _voiceish(n, 120.0, 1)

    for ild, want_deg in ((-20.0, -90.0), (-10.0, -45.0), (0.0, 0.0), (10.0, 45.0), (20.0, 90.0)):
        f = frame_features(pan(v, ild))
        check(f"pan {ild:+.0f} dB -> deg", ild_to_deg(float(np.median(f['ild']))), want_deg, 5.0)

    for itd in (-8.0, -4.0, 0.0, 4.0, 8.0):
        f = frame_features(pan(v, 0.0, itd))
        check(f"delay {itd:+.0f} samples -> ITD", float(np.median(f["itd"])), itd, 1.0)

    # The load-bearing case: two talkers on opposite sides in ONE turn must
    # read as a wide spread, not as a centred single talker. This is the whole
    # proposed use in the overlap gate.
    a = pan(_voiceish(n, 110.0, 2), -18.0)
    b = pan(_voiceish(n, 200.0, 3), +18.0)
    mix = frame_features(a + b)
    single = frame_features(pan(_voiceish(n, 110.0, 2), 0.0))
    spread_mix = float(np.percentile(mix["ild"], 75) - np.percentile(mix["ild"], 25))
    spread_one = float(np.percentile(single["ild"], 75) - np.percentile(single["ild"], 25))
    print(f"  [   ] two-sided mix ILD IQR            {spread_mix:8.2f} dB")
    print(f"  [   ] centred single ILD IQR           {spread_one:8.2f} dB")
    good = spread_mix > 3.0 * max(spread_one, 0.1)
    ok &= good
    print(f"  [{'ok ' if good else 'FAIL'}] a two-sided mix is wider than a single talker")

    print("\n  selftest:", "PASS" if ok else "FAIL")
    return 0 if ok else 1


# --------------------------------------------------------------------------


def load_stereo(path: Path) -> np.ndarray:
    import soundfile as sf

    x, sr = sf.read(str(path), dtype="float32", always_2d=True)
    if x.shape[1] < 2:
        raise SystemExit(f"{path} is mono; azimuth needs a stereo capture")
    x = x[:, :2]
    if sr != SR:
        # Whole-signal linear resample. Good enough for level; the ITD is
        # measured after it, so the delay is preserved to the sample.
        n = int(round(len(x) * SR / sr))
        t = np.linspace(0, len(x) - 1, n)
        x = np.stack([np.interp(t, np.arange(len(x)), x[:, c]) for c in range(2)], axis=1)
    return x.astype(np.float32)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("wav", nargs="?", type=Path)
    ap.add_argument("--label", default="unknown", help="vrchat | discord | synthetic")
    ap.add_argument("--bank", type=Path, help="voicebank.json (a COPY of the live bank)")
    ap.add_argument("--model", type=Path, help="eres2net_en.onnx")
    ap.add_argument("--min-score", type=float, default=0.6)
    ap.add_argument("--json", type=Path, help="write per-turn rows here")
    ap.add_argument("--selftest", action="store_true")
    ap.add_argument("--synthetic-lobby", action="store_true",
                    help="run the whole pipeline on a synthetic spatialised lobby (positive control)")
    a = ap.parse_args()

    if a.selftest:
        print("\nazimuth_bench selftest — synthetic panned signals\n")
        return selftest()

    if a.synthetic_lobby:
        x, truth = synthetic_lobby()
        a.label = "synthetic-lobby"
        a.wav = Path("synthetic")
        a.bank = None
    else:
        if not a.wav:
            ap.error("give a stereo wav, --selftest, or --synthetic-lobby")
        truth = None
        x = load_stereo(a.wav)
    mono = 0.5 * (x[:, 0] + x[:, 1])
    print(f"\n{a.label}: {a.wav.name}  {len(x)/SR/60:.1f} min\n")

    # The cheapest possible answer, checked before anything expensive: a stream
    # whose two channels are the same samples carries no interaural anything,
    # and no amount of clustering will find an angle in it.
    diff = float(np.max(np.abs(x[:, 0] - x[:, 1])))
    corr = float(np.corrcoef(x[:, 0], x[:, 1])[0, 1]) if len(x) > 1 else 1.0
    print(f"  channels: max|L-R| {diff:.3e}   corr(L,R) {corr:.6f}"
          f"{'   DUAL-MONO — no azimuth information exists in this stream' if diff == 0.0 else ''}")

    feat = frame_features(x)
    turns = energy_vad(feat["rms"])
    rows = turn_rows(feat, turns)
    speech_s = sum(r["duration_s"] for r in rows)
    print(f"  frames {len(feat['rms'])}   turns {len(rows)}   speech {speech_s/60:.1f} min")
    if not rows:
        print("  no turns — nothing to cluster")
        return 0

    ild = np.asarray([r["ild_med"] for r in rows])
    itd = np.asarray([r["itd_med"] for r in rows])
    print(f"  turn ILD  median {np.median(ild):+.2f} dB   p5..p95 "
          f"{np.percentile(ild,5):+.2f}..{np.percentile(ild,95):+.2f}   sd {ild.std():.2f}")
    print(f"  turn ITD  median {np.median(itd):+.2f} smp  p5..p95 "
          f"{np.percentile(itd,5):+.2f}..{np.percentile(itd,95):+.2f}   sd {itd.std():.2f}")

    k, sil, lab = best_k(ild)
    print(f"\n  Question A — angle clusters (1-D k-means on median ILD)")
    for kk in (2, 3, 4, 5, 6):
        if len(ild) < kk + 2:
            continue
        l = kmeans_1d(ild, kk)
        print(f"    k={kk}  silhouette {silhouette_1d(ild, l):+.3f}")
    print(f"    best k={k}  silhouette {sil:+.3f}   (gate: >= 0.500)")

    pur, n_lab = float("nan"), 0
    if truth is not None:
        # Positive control: ground truth is who was synthesised, matched to
        # each detected turn by span overlap.
        hop = int(SR * FRAME_MS / 1000)
        names: list[str | None] = []
        for r in rows:
            lo, hi = r["frame_start"] * hop, r["frame_end"] * hop
            best, who = 0, None
            for s, e, nm in truth:
                ov = min(hi, e) - max(lo, s)
                if ov > best:
                    best, who = ov, nm
            names.append(who)
        pur, n_lab = purity(lab, names)
        print("\n  Question B — cluster purity vs synthesis ground truth")
        print(f"    turns matched: {n_lab}/{len(rows)}   purity {pur:.3f}   (gate: >= 0.800)")
    elif a.bank and a.model:
        bank = load_bank(a.bank)
        rows = label_turns(mono, rows, bank, a.model, a.min_score)
        truth = [r.get("label") for r in rows]
        pur, n_lab = purity(lab, truth)
        named = sorted({t for t in truth if t})
        print(f"\n  Question B — cluster purity vs the voicebank")
        print(f"    turns labelled at score >= {a.min_score}: {n_lab}/{len(rows)}"
              f"   distinct speakers: {len(named)}")
        print(f"    purity {pur:.3f}   (gate: >= 0.800)"
              if not math.isnan(pur) else
              "    purity n/a — the clustering collapsed to a single cluster")
    else:
        print("\n  Question B skipped (no --bank/--model)")

    gate = sil >= 0.5 and not math.isnan(pur) and pur >= 0.8
    print(f"\n  GATE ({a.label}): {'PASS' if gate else 'FAIL'}"
          f"   silhouette {sil:+.3f}  purity {pur:.3f}")
    if a.label == "discord":
        print("  (Discord is the negative control — a FAIL here is the expected result.)")

    if a.json:
        for i, r in enumerate(rows):
            r["cluster"] = int(lab[i])
        a.json.write_text(json.dumps(
            {"label": a.label, "wav": str(a.wav), "k": int(k), "silhouette": sil,
             "purity": None if math.isnan(pur) else pur, "turns": rows}, indent=1))
        print(f"  wrote {a.json}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
