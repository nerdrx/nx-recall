"""Do voices drift? Loaders shared by `drift_bench.py` and `policy_bench.py`.

Extends `recog_lib` with the two things the drift round needs and §32 did not:

  * every prototype's **age** and the **source** its audio came from
    (`sources.kind` and `sources.match_key`, i.e. which Discord client);
  * the user's own voice, whose ground truth is not Discord but the microphone:
    a `sources.kind = 'mic'` recording is the user by construction (§17), which
    is the only truth this install has for speaker 26.

Read-only, against a COPY of the database. Never the live one.
"""

import sys

import numpy as np

sys.path.insert(0, __file__.rsplit("/", 1)[0])
import recog_lib as R  # noqa: E402

DB = "/tmp/nx-recall-workspace/nx-scratch/agents-2026-09-04/drift/work.db"

HOUR_NS = 3_600_000_000_000

# Age buckets, in hours. The corpus is 3.2 days long, so anything past 72 h is
# one bucket and the round says so rather than pretending to a longer baseline.
AGE_BUCKETS = [
    ("<1 h", 0.0, 1.0),
    ("1-6 h", 1.0, 6.0),
    ("6-24 h", 6.0, 24.0),
    ("1-2 d", 24.0, 48.0),
    ("2-3 d", 48.0, 72.0),
    ("3 d +", 72.0, float("inf")),
]


def age_bucket(hours):
    for name, lo, hi in AGE_BUCKETS:
        if lo <= hours < hi:
            return name
    return AGE_BUCKETS[-1][0]


PROTO_QUERY = """
  SELECT p.id, p.speaker_id, p.source_segment_id, p.vector, p.is_golden, p.created_at,
         so.kind, so.match_key
    FROM speaker_prototypes p
    JOIN speakers s ON s.id = p.speaker_id
    LEFT JOIN segments g  ON g.id  = p.source_segment_id
    LEFT JOIN sessions ss ON ss.id = g.session_id
    LEFT JOIN sources so  ON so.id = ss.source_id
   WHERE s.merged_into IS NULL AND p.embed_model_id = ?
"""


def load_bank(c, model=R.MODEL):
    """`recog_lib.load_bank` plus the source the prototype's audio came from."""
    return [
        dict(
            id=r[0],
            speaker=r[1],
            src=r[2],
            vec=R.blob_to_vec(r[3]),
            golden=r[4],
            created=r[5],
            kind=r[6] or "?",
            app=r[7] or "?",
        )
        for r in c.execute(PROTO_QUERY, (model,))
    ]


TRUTH_ROWS = """
  SELECT g.id, g.t_start_ns, COALESCE(g.overlap_frac, 0.0),
         (g.t_end_ns - g.t_start_ns), d.speaker_id, e.vector, e.embed_model_id,
         so.kind, so.match_key, COALESCE(g.truth_coverage, 0.0), g.truth_user_id
    FROM segments g
    JOIN discord_users d ON d.user_id = g.truth_user_id
    JOIN speakers s ON s.id = d.speaker_id
    JOIN sessions ss ON ss.id = g.session_id
    JOIN sources so ON so.id = ss.source_id
    JOIN embeddings e ON e.id = (
         SELECT MAX(x.id) FROM embeddings x WHERE x.segment_id = g.id)
   WHERE g.deleted_at IS NULL
     AND g.truth_verdict = 'single'
     AND d.speaker_id IS NOT NULL
     AND s.merged_into IS NULL
     AND (g.t_end_ns - g.t_start_ns) >= ?
   ORDER BY g.t_start_ns ASC, g.id ASC
"""

MIC_ROWS = """
  SELECT g.id, g.t_start_ns, COALESCE(g.overlap_frac, 0.0),
         (g.t_end_ns - g.t_start_ns), ?, e.vector, e.embed_model_id,
         so.kind, so.match_key, 1.0, NULL
    FROM segments g
    JOIN sessions ss ON ss.id = g.session_id
    JOIN sources so ON so.id = ss.source_id
    JOIN embeddings e ON e.id = (
         SELECT MAX(x.id) FROM embeddings x WHERE x.segment_id = g.id)
   WHERE g.deleted_at IS NULL
     AND so.kind = 'mic'
     AND (g.t_end_ns - g.t_start_ns) >= ?
   ORDER BY g.t_start_ns ASC, g.id ASC
"""


class Row:
    __slots__ = ("id", "t", "overlap", "dur", "truth", "vec", "kind", "app", "coverage", "user")


def _rows(cursor, model):
    out = []
    for r in cursor:
        assert r[6] == model, r[6]
        x = Row()
        x.id, x.t, x.overlap = r[0], r[1], float(r[2])
        x.dur = r[3] / 1e9
        x.truth, x.vec = r[4], R.blob_to_vec(r[5])
        x.kind, x.app = r[7], r[8]
        x.coverage, x.user = float(r[9]), r[10]
        out.append(x)
    return out


def load_truth_rows(c, min_dur=R.MIN_DURATION_S, model=R.MODEL):
    return _rows(c.execute(TRUTH_ROWS, (int(min_dur * 1e9),)), model)


def load_mic_rows(c, you, min_dur=R.MIN_DURATION_S, model=R.MODEL):
    """Every microphone turn, filed under the user.

    The mic hears the room, not a program, so this is the user's own voice by
    construction — the one thing about speaker 26 that Discord can never say
    (§17 finding 1: a Discord client does not play your microphone back to you).
    It is noisier truth than a Discord verdict: `mode = "follow"` means the mic
    records while an allowed program is captured, so speaker bleed is possible.
    Every number computed from it is reported as such.
    """
    return _rows(c.execute(MIC_ROWS, (you, int(min_dur * 1e9))), model)


def normed(v):
    n = np.linalg.norm(v)
    return v / n if n > 0 else v


def pairs(rows, protos, causal=True):
    """(row, prototype) cosines for every prototype of the row's OWN voice.

    Two exclusions, both from §32's protocol: a row is never scored against a
    prototype its own audio produced, and — `causal` — never against one that
    did not exist yet when the turn was spoken, because "how well does a bank
    match a turn recorded before the bank existed" is not a question the live
    daemon ever asks.
    """
    by_voice = {}
    for p in protos:
        by_voice.setdefault(p["speaker"], []).append(p)
    for v in by_voice:
        by_voice[v].sort(key=lambda p: (p["created"] or 0, p["id"]))
    out = []
    for r in rows:
        mine = by_voice.get(r.truth)
        if not mine:
            continue
        v = normed(r.vec)
        for p in mine:
            if p["src"] is not None and p["src"] == r.id:
                continue
            age_ns = r.t - (p["created"] or 0)
            if causal and age_ns < 0:
                continue
            out.append(
                dict(
                    row=r.id,
                    voice=r.truth,
                    t=r.t,
                    dur=r.dur,
                    row_kind=r.kind,
                    row_app=r.app,
                    proto=p["id"],
                    proto_kind=p["kind"],
                    proto_app=p["app"],
                    age_h=age_ns / HOUR_NS,
                    cos=float(v @ normed(p["vec"])),
                )
            )
    return out
