"""Build the labelled 2-speaker overlap corpus from Discord ground truth.

Read-only. For every `truth_verdict='overlap'` segment we recompute the same
coverage the daemon does (`truth::coverage`, each user's own spans merged first,
clipped to the segment) and keep the rows where **exactly two** users clear the
0.2 presence bar and **both** are linked to a speaker with prototypes. Those are
the rows where we know who the two voices are, so a separated stream can be
scored against the right person rather than against "somebody".

Writes corpus.json, protos.json (prototype vectors per speaker) and
control.json (clean `single` rows for the same voices -- the ceiling any
separated stream is being asked to reach).

    python sep_corpus.py <snapshot.db> <audio-root> <out-dir>
"""
import json
import sqlite3
import struct
import sys
from pathlib import Path

PRESENT_MIN = 0.2  # truth_verdict::PRESENT_MIN

db_path, audio_root, out_dir = sys.argv[1], Path(sys.argv[2]), Path(sys.argv[3])
out_dir.mkdir(parents=True, exist_ok=True)
c = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)


def merged(spans):
    out = []
    for lo, hi in sorted(spans):
        if out and lo <= out[-1][1]:
            out[-1][1] = max(out[-1][1], hi)
        else:
            out.append([lo, hi])
    return out


def coverage(spans, t0, t1):
    """user_id -> covered fraction of [t0, t1), own spans merged first."""
    dur = t1 - t0
    if dur <= 0:
        return {}
    by_user = {}
    for uid, lo, hi in spans:
        lo, hi = max(lo, t0), min(hi, t1)
        if hi > lo:
            by_user.setdefault(uid, []).append((lo, hi))
    return {u: sum(b - a for a, b in merged(s)) / dur for u, s in by_user.items()}


linked = {u: s for u, s in c.execute(
    "select user_id, speaker_id from discord_users where speaker_id is not null")}
names = dict(c.execute("select user_id, name from discord_users"))

protos = {}
for sid, blob, src in c.execute(
        "select speaker_id, vector, source_segment_id from speaker_prototypes "
        "where embed_model_id = 'eres2net_en@1'"):
    v = list(struct.unpack(f"<{len(blob)//4}f", blob))
    protos.setdefault(str(sid), []).append({"src": src, "v": v})

rows = list(c.execute(
    "select id, t_start_ns, t_end_ns, audio_path, overlap_frac, truth_overlap_frac, "
    "       text, session_id "
    "from segments where truth_verdict = 'overlap' and deleted_at is null "
    "order by id"))

corpus, stats = [], {"rows": len(rows), "two_present": 0, "both_linked": 0,
                     "audio_missing": 0, "kept": 0, "pairs": {}}
for sid_, t0, t1, path, ovl, tovl, text, sess in rows:
    spans = list(c.execute(
        "select user_id, t_start_ns, coalesce(t_end_ns, ?) from truth_speaking "
        "where t_start_ns < ? and coalesce(t_end_ns, ?) > ?", (t1, t1, t1, t0)))
    cov = {u: f for u, f in coverage(spans, t0, t1).items() if f >= PRESENT_MIN}
    if len(cov) != 2:
        continue
    stats["two_present"] += 1
    users = sorted(cov, key=lambda u: -cov[u])
    if not all(u in linked and str(linked[u]) in protos for u in users):
        continue
    stats["both_linked"] += 1
    wav = audio_root / path
    if not wav.is_file():
        stats["audio_missing"] += 1
        continue
    pair = "+".join(sorted(names.get(u, u) for u in users))
    stats["pairs"][pair] = stats["pairs"].get(pair, 0) + 1
    stats["kept"] += 1
    corpus.append({
        "segment_id": sid_, "session_id": sess, "wav": str(wav),
        "dur_s": (t1 - t0) / 1e9,
        "users": [{"user_id": u, "name": names.get(u, u), "speaker_id": linked[u],
                   "coverage": round(cov[u], 4)} for u in users],
        "pair": pair,
        "detector_overlap_frac": ovl,
        "truth_overlap_frac": tovl,
        "text": text,
    })

(out_dir / "corpus.json").write_text(json.dumps(corpus, indent=1))
(out_dir / "protos.json").write_text(json.dumps(protos))

ctrl = []
for sid_, t0, t1, path, uid in c.execute(
        "select id, t_start_ns, t_end_ns, audio_path, truth_user_id from segments "
        "where truth_verdict = 'single' and deleted_at is null "
        "and truth_user_id is not null"):
    if uid in linked and str(linked[uid]) in protos and (audio_root / path).is_file():
        ctrl.append({"segment_id": sid_, "wav": str(audio_root / path),
                     "dur_s": (t1 - t0) / 1e9, "user_id": uid,
                     "name": names.get(uid, uid), "speaker_id": linked[uid]})
(out_dir / "control.json").write_text(json.dumps(ctrl, indent=1))

stats["control"] = len(ctrl)
print(json.dumps(stats, indent=1))
