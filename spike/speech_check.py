"""Cross-check the real lobby recording: is pyannote's 53% "speech" real speech?

FINDINGS.md §9 flagged that pyannote might be counting world audio / music as
speech, inflating the denominator. Two independent checks on the same file:

  1. Silero VAD (a dedicated speech/non-speech model, the daemon's actual first
     stage) — its speech fraction vs pyannote's.
  2. Parakeet 110m over pyannote's single-speaker regions — words per minute of a
     region is a strong tell: real conversation lands ~100-200 wpm, music/ambience
     transcribes to (near-)nothing.

Aggregate statistics only. No transcript text is printed or stored.
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).parent))
from asr_spike import find_model_dir, make_recognizer, norm  # noqa: E402
from harness import SR  # noqa: E402
from measure_lobby import SEG, decode, regions, segment  # noqa: E402

S = Path(os.environ.get("NXR_SCRATCH", "/tmp/nxr"))


def silero_fraction(x: np.ndarray) -> float:
    import sherpa_onnx as so
    cfg = so.VadModelConfig()
    cfg.silero_vad.model = str(S / "models" / "silero_vad.onnx")
    cfg.silero_vad.threshold = 0.5
    cfg.silero_vad.min_silence_duration = 0.25
    cfg.silero_vad.min_speech_duration = 0.25
    cfg.sample_rate = SR
    vad = so.VoiceActivityDetector(cfg, buffer_size_in_seconds=60)
    total = 0.0
    step = 512
    for i in range(0, len(x), step):
        vad.accept_waveform(x[i:i + step])
        while not vad.empty():
            total += len(vad.front.samples) / SR
            vad.pop()
    vad.flush()
    while not vad.empty():
        total += len(vad.front.samples) / SR
        vad.pop()
    return total / (len(x) / SR)


def main(path: Path) -> int:
    import onnxruntime as ort

    x = decode(path)
    dur = len(x) / SR
    print(f"\n{path.name}: {dur/60:.1f} min")

    sil = silero_fraction(x)

    so_ = ort.SessionOptions()
    so_.intra_op_num_threads = 8
    cls = segment(ort.InferenceSession(str(SEG), so_,
                                       providers=["CPUExecutionProvider"]), x)
    n_chunks = (len(x) + 160_000 - 1) // 160_000
    hop = 160_000 / SR / (len(cls) / n_chunks)
    pyn = float((cls != 0).mean())
    runs = regions(cls, hop, dur)

    print("\n  speech fraction of wall clock:")
    print(f"    pyannote (any speaker active)   {pyn*100:5.1f}%")
    print(f"    Silero VAD                      {sil*100:5.1f}%")
    print(f"    ratio                           {sil/pyn:5.2f}" if pyn else "")

    rec = make_recognizer("transducer", find_model_dir("*parakeet_tdt_transducer_110m*"), 8)
    wpm, empty, tot_w, tot_t = [], 0, 0, 0.0
    for a, b in runs:
        st = rec.create_stream()
        st.accept_waveform(SR, x[int(a * SR):int(b * SR)].astype(np.float32))
        rec.decode_stream(st)
        w = len(norm(st.result.text))
        tot_w += w
        tot_t += b - a
        if w == 0:
            empty += 1
        else:
            wpm.append(w / (b - a) * 60)

    wpm_a = np.array(wpm) if wpm else np.array([0.0])
    print(f"\n  ASR over {len(runs)} single-speaker regions ({tot_t/60:.1f} min):")
    print(f"    regions transcribing to NOTHING   {empty}/{len(runs)} "
          f"({empty/len(runs)*100:.0f}%)")
    print(f"    words per minute (non-empty)      median {np.median(wpm_a):.0f}, "
          f"p10 {np.percentile(wpm_a,10):.0f}, p90 {np.percentile(wpm_a,90):.0f}")
    print(f"    overall words/min of region time  {tot_w/tot_t*60:.0f}")
    print(f"    total words                       {tot_w}")
    print("\n  Interpretation: conversational speech runs ~100-200 wpm. Regions at")
    print("  near-zero wpm are non-voice audio that pyannote counted as a speaker.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(Path(sys.argv[1])))
