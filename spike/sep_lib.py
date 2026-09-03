"""Shared pieces for the overlap-separation bench: audio, the daemon's embedder,
the daemon's scoring ladder, and the separation models.

The embedder is deliberately sherpa-onnx's `SpeakerEmbeddingExtractor` over the
same `eres2net_en.onnx` the daemon loads (`crate::embed`), because that object
owns the kaldi 80-bin fbank front end and the model's `global-mean`
normalisation. Reimplementing the front end here is the easiest way to produce
vectors that look plausible and compare wrongly, so we do not.
"""
import json
import subprocess
from pathlib import Path

import numpy as np

SR = 16_000
MODELS = Path.home() / ".local/share/nx-recall/models"
EMB_MODEL = MODELS / "eres2net_en.onnx"

# Shipping operating point (0.11.5): the global label bar, and the per-voice
# bars §18 fitted but did not ship. Both are reported.
GLOBAL_THR = 0.35
PER_VOICE_THR = {2: 0.31, 25: 0.41}  # Rowan, Aspen


def decode(path, sr=SR):
    """Anything ffmpeg reads -> mono float32 at `sr`."""
    raw = subprocess.run(
        ["ffmpeg", "-hide_banner", "-loglevel", "error", "-i", str(path),
         "-f", "f32le", "-ar", str(sr), "-ac", "1", "pipe:1"],
        capture_output=True, check=True).stdout
    return np.frombuffer(raw, dtype="<f4").copy()


class Embedder:
    """The daemon's speaker embedder, unmodified."""

    def __init__(self, model=EMB_MODEL, num_threads=4):
        import sherpa_onnx
        self._x = sherpa_onnx.SpeakerEmbeddingExtractor(
            sherpa_onnx.SpeakerEmbeddingExtractorConfig(
                model=str(model), num_threads=num_threads, debug=False, provider="cpu"))

    def __call__(self, samples):
        s = self._x.create_stream()
        s.accept_waveform(sample_rate=SR, waveform=np.ascontiguousarray(
            samples, dtype=np.float32))
        s.input_finished()
        v = np.asarray(self._x.compute(s), dtype=np.float32)
        n = np.linalg.norm(v)
        return v / n if n > 0 else v


class Bank:
    """Prototypes, scored the way `identity::rank` scores them: a speaker's
    score is the **max** cosine over that speaker's prototypes, and a prototype
    sourced from the segment under judgement is dropped for that judgement --
    without that rule the bench is a memory test."""

    def __init__(self, protos_json):
        raw = json.loads(Path(protos_json).read_text())
        self.sid = [int(k) for k in raw]
        self.vecs, self.src = {}, {}
        for k, lst in raw.items():
            m = np.array([p["v"] for p in lst], dtype=np.float32)
            m /= np.linalg.norm(m, axis=1, keepdims=True) + 1e-12
            self.vecs[int(k)] = m
            self.src[int(k)] = np.array([p["src"] if p["src"] is not None else -1
                                         for p in lst])

    def score(self, vec, speaker_id, exclude_segment=None):
        m, s = self.vecs[speaker_id], self.src[speaker_id]
        if exclude_segment is not None:
            keep = s != exclude_segment
            if not keep.any():
                return None
            m = m[keep]
        return float((m @ vec).max())

    def rank(self, vec, exclude_segment=None):
        """All speakers, descending -- the full ladder, for steal accounting."""
        out = []
        for sid in self.sid:
            sc = self.score(vec, sid, exclude_segment)
            if sc is not None:
                out.append((sid, sc))
        out.sort(key=lambda t: -t[1])
        return out


def thr(speaker_id, per_voice):
    return PER_VOICE_THR.get(speaker_id, GLOBAL_THR) if per_voice else GLOBAL_THR


# ---------------------------------------------------------------------------
# separation
# ---------------------------------------------------------------------------


class SepFormer:
    """speechbrain/sepformer-whamr16k (Apache-2.0). 16 kHz native, trained on
    WHAMR! -- noisy *and* reverberant 2-speaker mixtures, which is the closest
    public condition to a Discord call."""

    name = "sepformer-whamr16k"

    def __init__(self, savedir, device="cpu"):
        from speechbrain.inference.separation import SepformerSeparation
        self.m = SepformerSeparation.from_hparams(
            source="speechbrain/sepformer-whamr16k", savedir=str(savedir),
            run_opts={"device": device})
        self.device = device

    def __call__(self, x):
        import torch
        with torch.no_grad():
            est = self.m.separate_batch(torch.from_numpy(x[None, :]).to(self.device))
        est = est[0].cpu().numpy().T  # (2, T)
        return [np.ascontiguousarray(s) for s in est]


class ConvTasNet:
    """speechbrain/sepformer-wsj02mix is the same family; the fast candidate is
    a Conv-TasNet, which is a plain 1-D conv stack and therefore the one that
    exports to ONNX without heroics. Loaded from asteroid weights if present."""

    name = "conv-tasnet"

    def __init__(self, savedir, device="cpu"):
        from speechbrain.inference.separation import SepformerSeparation
        self.m = SepformerSeparation.from_hparams(
            source="speechbrain/resepformer-wsj02mix", savedir=str(savedir),
            run_opts={"device": device})
        self.device = device
        self.native_sr = 8000

    def __call__(self, x):
        import torch
        from scipy.signal import resample_poly
        x8 = resample_poly(x, 1, 2).astype(np.float32)
        with torch.no_grad():
            est = self.m.separate_batch(torch.from_numpy(x8[None, :]).to(self.device))
        est = est[0].cpu().numpy().T
        return [np.ascontiguousarray(resample_poly(s, 2, 1).astype(np.float32))
                for s in est]
