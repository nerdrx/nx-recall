"""Generate the golden regression fixtures the brief's §10 calls for.

A small, fixed set of synthetic mixed-speaker WAVs with complete ground truth
(who, when, what was said, mix parameters). The daemon's future test suite runs
its pipeline over these and diffs against the manifest — the only way to know a
model swap or code change made labeling/transcription worse.

Deterministic (fixed seed), ~15 files, a few MB. Fixture speech is LibriSpeech
(CC-BY 4.0) — public read speech, safe to commit.
"""
import json
import sys
from pathlib import Path

import numpy as np
import soundfile as sf

sys.path.insert(0, str(Path(__file__).parent))
from harness import (SR, extract_corpus, index_corpus, load, mix,  # noqa: E402
                     opus_roundtrip, rms_normalise, trim_silence)

S = Path(sys.argv[1]) if len(sys.argv) > 1 else Path("/tmp/nxr")
OUT = Path(__file__).parent.parent / "fixtures"
OUT.mkdir(exist_ok=True)

# (name, n_talkers, dominance_db, opus_kbps)  — one per regime that matters
CASES = [
    ("clean_single", 1, 0.0, 0),
    ("opus24_single", 1, 0.0, 24),
    ("opus8_single", 1, 0.0, 8),
    ("duo_dominant", 2, 12.0, 24),
    ("duo_equal", 2, 0.0, 24),        # the poison cell — pipeline must REFUSE this
    ("trio_dominant", 3, 12.0, 24),
    ("lobby_dominant", 10, 12.0, 24),
    ("lobby_equal", 10, 0.0, 24),
]
PER_CASE = 2
SEED = 0xF17E


def main() -> int:
    root = extract_corpus(S / "corpus" / "dev-clean.tar.gz", S / "corpus")
    trans = {}
    for f in root.rglob("*.trans.txt"):
        for line in f.read_text().splitlines():
            uid, _, text = line.partition(" ")
            trans[uid] = text
    by = index_corpus(root, 6)
    spk = sorted(by)

    rng = np.random.default_rng(SEED)
    manifest = []
    for cname, ntk, dom, br in CASES:
        for rep in range(PER_CASE):
            tgt_s = str(rng.choice(spk))
            others = [s for s in spk if s != tgt_s]
            itf_s = [str(x) for x in rng.choice(others, ntk - 1, replace=False)]

            tu = by[tgt_s][int(rng.integers(len(by[tgt_s])))]
            tgt = rms_normalise(trim_silence(load(tu.path)))
            if br:
                tgt = opus_roundtrip(tgt, br)
            itfs = []
            for s in itf_s:
                u = by[s][int(rng.integers(len(by[s])))]
                w = rms_normalise(trim_silence(load(u.path)))
                w = np.tile(w, int(np.ceil(len(tgt) / len(w))))[:len(tgt)]
                itfs.append(opus_roundtrip(w, br or 24))
            m = mix(tgt, itfs, dom) if itfs else tgt

            fname = f"{cname}_{rep}.wav"
            sf.write(OUT / fname, m, SR)
            manifest.append({
                "file": fname, "condition": cname,
                "n_talkers": ntk, "dominance_db": dom, "opus_kbps": br,
                "duration_s": round(len(m) / SR, 2),
                "target_speaker": tgt_s, "interferer_speakers": itf_s,
                "target_utterance": tu.path.stem,
                "target_transcript": trans[tu.path.stem],
                # what a correct pipeline should do with this file:
                "expect": ("label+transcribe" if dom > 0 or ntk == 1
                           else "refuse (overlap gate)"),
            })

    # Also: the non-speech ghosts — ASR must emit NOTHING on these.
    sil = np.zeros(10 * SR, dtype=np.float32)
    noi = (np.random.default_rng(1).standard_normal(10 * SR) * 0.01).astype(np.float32)
    for fname, arr in [("silence.wav", sil), ("noise.wav", noi)]:
        sf.write(OUT / fname, arr, SR)
        manifest.append({"file": fname, "condition": "non_speech",
                         "expect": "no output (any ghost word is a regression)"})

    (OUT / "manifest.json").write_text(json.dumps({
        "seed": SEED, "sample_rate": SR,
        "source": "LibriSpeech dev-clean (CC-BY 4.0)",
        "note": "Regression goldens: run the pipeline, diff against `expect`. "
                "duo_equal/lobby_equal MUST be refused by the overlap gate — "
                "labelling them 'correctly' by luck is still a fail.",
        "fixtures": manifest}, indent=1))
    print(f"{len(manifest)} fixtures -> {OUT}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
