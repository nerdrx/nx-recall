"""Re-embed this install's truth rows with a candidate extractor.

One model in RAM at a time, on four cores, writing an .npz per model. The
concatenated-window variant (§5) is produced for the shipping extractor only,
because it is a question about the *window*, not about the model.

    <venv>/bin/python spike/recog_embed.py <name> <model.onnx> [--concat]
"""

import os
import sys
import time
import wave

import numpy as np
import sherpa_onnx

import recog_lib as L

AUDIO_ROOT = os.path.expanduser("~/.local/share/nx-recall")
OUT = "/tmp/nx-recall-workspace/nx-scratch/agents-2026-09-04/recog"
# How far back a previous turn may be and still be glued to this one. Beyond
# this the silence is long enough that the other person may have started.
CONCAT_GAP_NS = 2_000_000_000


def read_wav(path):
    with wave.open(path) as w:
        assert w.getnchannels() == 1 and w.getsampwidth() == 2, path
        sr = w.getframerate()
        pcm = np.frombuffer(w.readframes(w.getnframes()), dtype="<i2")
    return pcm.astype(np.float32) / 32768.0, sr


def rows_with_audio(c):
    """The truth rows, plus each one's immediately preceding turn in the same
    session when the gap is short enough to glue them together."""
    q = """
      SELECT g.id, g.audio_path, g.session_id, g.t_start_ns, g.t_end_ns
        FROM segments g WHERE g.deleted_at IS NULL AND g.audio_path IS NOT NULL
       ORDER BY g.session_id, g.t_start_ns
    """
    by_session = {}
    for sid, path, sess, t0, t1 in c.execute(q):
        by_session.setdefault(sess, []).append((sid, path, t0, t1))
    prev = {}
    for items in by_session.values():
        for i in range(1, len(items)):
            sid, _p, t0, _t1 = items[i]
            psid, ppath, _pt0, pt1 = items[i - 1]
            if 0 <= t0 - pt1 <= CONCAT_GAP_NS:
                prev[sid] = ppath
    return prev


def main():
    name, model = sys.argv[1], sys.argv[2]
    concat = "--concat" in sys.argv
    threads = int(os.environ.get("RECOG_THREADS", "4"))

    c = L.conn()
    rows = L.load_rows(c)
    # The live bank's prototypes are embedded too, from the very segments they
    # were enrolled from: a bake-off in which the candidate gets a bank grown
    # this afternoon and the incumbent keeps a year of prototypes is not a
    # bake-off. Same segments, same voices, five spaces.
    protos = [
        (r[0], r[1], r[2]) for r in c.execute(
            "SELECT p.source_segment_id, p.speaker_id, g.audio_path FROM speaker_prototypes p "
            "JOIN speakers s ON s.id = p.speaker_id "
            "JOIN segments g ON g.id = p.source_segment_id "
            "WHERE s.merged_into IS NULL AND p.embed_model_id = ? "
            "AND g.audio_path IS NOT NULL ORDER BY p.id", (L.MODEL,))
    ]
    prev = rows_with_audio(c)
    paths = {
        r[0]: r[1]
        for r in c.execute("SELECT id, audio_path FROM segments WHERE audio_path IS NOT NULL")
    }

    cfg = sherpa_onnx.SpeakerEmbeddingExtractorConfig(
        model=model, num_threads=threads, debug=False, provider="cpu"
    )
    ex = sherpa_onnx.SpeakerEmbeddingExtractor(cfg)

    def embed(samples, sr):
        s = ex.create_stream()
        s.accept_waveform(sample_rate=sr, waveform=samples)
        s.input_finished()
        return np.array(ex.compute(s), dtype=np.float32)

    ids, vecs, cvecs = [], [], []
    audio_s = 0.0
    missing = 0
    t0 = time.perf_counter()
    for r in rows:
        p = os.path.join(AUDIO_ROOT, paths[r.id])
        if not os.path.exists(p):
            missing += 1
            continue
        w, sr = read_wav(p)
        audio_s += len(w) / sr
        ids.append(r.id)
        vecs.append(embed(w, sr))
        if concat:
            pp = prev.get(r.id)
            if pp and os.path.exists(os.path.join(AUDIO_ROOT, pp)):
                pw, psr = read_wav(os.path.join(AUDIO_ROOT, pp))
                assert psr == sr
                cvecs.append(embed(np.concatenate([pw, w]), sr))
            else:
                cvecs.append(vecs[-1])
    wall = time.perf_counter() - t0

    pids, pspk, pvecs = [], [], []
    for pid, spk, path in protos:
        pp = os.path.join(AUDIO_ROOT, path)
        if not os.path.exists(pp):
            continue
        w, sr = read_wav(pp)
        pids.append(pid)
        pspk.append(spk)
        pvecs.append(embed(w, sr))

    out = dict(ids=np.array(ids), vecs=np.stack(vecs),
               proto_src=np.array(pids), proto_speakers=np.array(pspk),
               proto_vecs=np.stack(pvecs))
    if concat:
        out["concat"] = np.stack(cvecs)
    np.savez(os.path.join(OUT, f"emb_{name}.npz"), **out)
    print(
        f"{name}: {len(ids)} clips ({missing} missing), dim {vecs[0].shape[0]}, "
        f"{audio_s:.0f} s of audio in {wall:.1f} s wall on {threads} threads "
        f"=> RTF {wall / audio_s:.4f}"
        + f"  (+{len(pids)} prototypes)"
        + (f"  (+{len(cvecs)} concatenated)" if concat else "")
    )


if __name__ == "__main__":
    main()
