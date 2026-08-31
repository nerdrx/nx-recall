"""
NX Recall — Step 0 measurement harness.

Answers one question with numbers instead of adjectives:

    Does timbre-based speaker labelling survive a VRChat lobby?

The signal chain models what actually reaches your speakers:

    per remote speaker:  clean mic  ->  Opus (voice codec)  ->  decode
                                    ->  gain (near/far dominance)
    sum all speakers    ->  captured mix  ->  speaker embedding

Each remote talker is encoded *individually* before the mix, because that is
what VRChat does: every person's mic is codec'd on their machine, and your
client mixes the decoded streams. Encoding the finished mix instead would
flatter the codec and understate the damage.
"""

from __future__ import annotations

import subprocess
import tarfile
from dataclasses import dataclass
from pathlib import Path

import numpy as np
import soundfile as sf

SR = 16_000


# --------------------------------------------------------------------------
# corpus
# --------------------------------------------------------------------------


@dataclass(frozen=True)
class Utt:
    speaker: str
    path: Path


def extract_corpus(tar_path: Path, dest: Path) -> Path:
    """Unpack LibriSpeech dev-clean once; return the split root."""
    root = dest / "LibriSpeech" / "dev-clean"
    if root.is_dir():
        return root
    dest.mkdir(parents=True, exist_ok=True)
    with tarfile.open(tar_path) as tf:
        tf.extractall(dest, filter="data")
    return root


def index_corpus(root: Path, min_utts: int) -> dict[str, list[Utt]]:
    """Map speaker id -> utterances, keeping only speakers with enough audio."""
    by_speaker: dict[str, list[Utt]] = {}
    for flac in sorted(root.rglob("*.flac")):
        spk = flac.parts[-3]
        by_speaker.setdefault(spk, []).append(Utt(spk, flac))
    return {s: u for s, u in sorted(by_speaker.items()) if len(u) >= min_utts}


# --------------------------------------------------------------------------
# audio
# --------------------------------------------------------------------------


def load(path: Path) -> np.ndarray:
    x, sr = sf.read(str(path), dtype="float32", always_2d=False)
    if x.ndim > 1:
        x = x.mean(axis=1)
    if sr != SR:
        raise ValueError(f"{path} is {sr} Hz, expected {SR}")
    return x


def trim_silence(x: np.ndarray, frame: int = 400, thresh_db: float = -35.0) -> np.ndarray:
    """Drop leading/trailing silence so 'overlap' means real overlapping speech.

    LibriSpeech utterances carry a second or more of room tone at each end. Mixing
    untrimmed audio would silently make the overlap conditions easier than they
    look, because interferers would spend part of the window not talking.
    """
    if len(x) < frame:
        return x
    n = len(x) // frame
    frames = x[: n * frame].reshape(n, frame)
    rms = np.sqrt((frames**2).mean(axis=1) + 1e-12)
    peak = rms.max()
    if peak <= 0:
        return x
    keep = np.nonzero(rms > peak * (10.0 ** (thresh_db / 20.0)))[0]
    if len(keep) == 0:
        return x
    return x[keep[0] * frame : (keep[-1] + 1) * frame]


def rms_normalise(x: np.ndarray, target_rms: float = 0.06) -> np.ndarray:
    r = float(np.sqrt((x**2).mean() + 1e-12))
    if r <= 0:
        return x
    return x * (target_rms / r)


def take_window(x: np.ndarray, seconds: float, rng: np.random.Generator) -> np.ndarray:
    """A random window of the requested length, zero-padded if the source is short."""
    want = int(round(seconds * SR))
    if len(x) <= want:
        return np.pad(x, (0, want - len(x)))
    start = int(rng.integers(0, len(x) - want))
    return x[start : start + want]


# --------------------------------------------------------------------------
# codec
# --------------------------------------------------------------------------


def opus_roundtrip(x: np.ndarray, bitrate_kbps: int) -> np.ndarray:
    """Encode to Opus at the given bitrate and decode back, as VRChat voice would.

    `-application voip` matches what a voice-chat client asks Opus for; it favours
    intelligibility over fidelity, which is exactly the transform we care about.
    """
    raw = x.astype("<f4").tobytes()
    enc = subprocess.run(
        ["ffmpeg", "-hide_banner", "-loglevel", "error",
         "-f", "f32le", "-ar", str(SR), "-ac", "1", "-i", "pipe:0",
         "-c:a", "libopus", "-b:a", f"{bitrate_kbps}k", "-application", "voip",
         "-f", "ogg", "pipe:1"],
        input=raw, capture_output=True, check=True,
    ).stdout
    dec = subprocess.run(
        ["ffmpeg", "-hide_banner", "-loglevel", "error",
         "-f", "ogg", "-i", "pipe:0",
         "-f", "f32le", "-ar", str(SR), "-ac", "1", "pipe:1"],
        input=enc, capture_output=True, check=True,
    ).stdout
    y = np.frombuffer(dec, dtype="<f4").copy()
    # Opus adds encoder delay; align lengths so mixing stays sample-accurate.
    if len(y) >= len(x):
        return y[: len(x)]
    return np.pad(y, (0, len(x) - len(y)))


# --------------------------------------------------------------------------
# mixing
# --------------------------------------------------------------------------


def mix(target: np.ndarray, interferers: list[np.ndarray], dominance_db: float) -> np.ndarray:
    """Sum a target talker with N interferers, target `dominance_db` louder than each.

    dominance_db = 0  -> everyone equally loud (worst case, a tight circle)
    dominance_db = 12 -> one person beside you over distant room babble
    """
    out = rms_normalise(target).copy()
    if interferers:
        g = 10.0 ** (-dominance_db / 20.0)
        for itf in interferers:
            v = rms_normalise(itf)[: len(out)]
            if len(v) < len(out):  # a short interferer just stops talking early
                v = np.pad(v, (0, len(out) - len(v)))
            out += v * g
    peak = np.abs(out).max()
    if peak > 0.99:  # keep the capture path from clipping
        out = out * (0.99 / peak)
    return out.astype(np.float32)


# --------------------------------------------------------------------------
# embeddings
# --------------------------------------------------------------------------


class Embedder:
    """Thin wrapper over sherpa-onnx's speaker embedding extractor."""

    def __init__(self, model: Path, num_threads: int = 1):
        import sherpa_onnx

        self._x = sherpa_onnx.SpeakerEmbeddingExtractor(
            sherpa_onnx.SpeakerEmbeddingExtractorConfig(
                model=str(model), num_threads=num_threads, debug=False, provider="cpu"
            )
        )

    @property
    def dim(self) -> int:
        return self._x.dim

    def __call__(self, samples: np.ndarray) -> np.ndarray:
        s = self._x.create_stream()
        s.accept_waveform(sample_rate=SR, waveform=samples.astype(np.float32))
        s.input_finished()
        v = np.asarray(self._x.compute(s), dtype=np.float32)
        n = np.linalg.norm(v)
        return v / n if n > 0 else v


# --------------------------------------------------------------------------
# metrics
# --------------------------------------------------------------------------


def eer(same: np.ndarray, diff: np.ndarray) -> tuple[float, float]:
    """Equal error rate and the threshold that achieves it.

    Scores are cosine similarities. Sweeping every observed score as a candidate
    threshold gives the exact EER without binning artefacts.
    """
    if len(same) == 0 or len(diff) == 0:
        return float("nan"), float("nan")
    thr = np.unique(np.concatenate([same, diff]))
    # false reject = same-speaker pairs scored below threshold
    fr = np.searchsorted(np.sort(same), thr, side="left") / len(same)
    # false accept = different-speaker pairs scored at or above threshold
    fa = 1.0 - np.searchsorted(np.sort(diff), thr, side="left") / len(diff)
    i = int(np.argmin(np.abs(fr - fa)))
    return float((fr[i] + fa[i]) / 2.0), float(thr[i])


def tar_at_far(same: np.ndarray, diff: np.ndarray, far: float) -> tuple[float, float]:
    """True-accept rate at a fixed false-accept rate, plus the threshold.

    This is the operating point the design actually cares about: false merges are
    poison, so the threshold is set to keep impostors out, and the question becomes
    'how much speech still gets labelled at all?'
    """
    if len(same) == 0 or len(diff) == 0:
        return float("nan"), float("nan")
    thr = float(np.quantile(diff, 1.0 - far))
    return float((same >= thr).mean()), thr
