"""The identity table before and after a real `recalld turns resplit --apply`.

`turnsplit_after.py` models the split in Python. This reads the database the
shipped command actually wrote — the daemon's own ASR on each piece, the
daemon's own embeddings, the wordless refusal in force — judges each new piece
with `truth::verdict`'s rules, and scores the identity ladder over both.

    python3 spike/turnsplit_applied.py <before.db> <after.db>
"""

import sys

import numpy as np

import recog_lib as R
import turnsplit_lib as L

PRESENT_MIN = 0.2
SINGLE_MIN = 0.8


def resplit_rows(c):
    """Exactly the rows a `turns.resplit --apply` touched: the shortened
    originals and the pieces it minted, from the operations it wrote."""
    import json

    ids = set()
    for (target,) in c.execute(
        "SELECT target_ids FROM operations WHERE op = 'turns.resplit'"
    ):
        ids.update(json.loads(target))
    return ids


def verdicts_for_unjudged(c):
    """`truth::verdict` over the rows the resplit left without one.

    Restricted to those rows on purpose. The archive holds 9,654 segments with
    no verdict at all — the evenings the plugin was not running — and judging
    those too would be a different feature's measurement wearing this one's
    label.
    """
    touched = resplit_rows(c)
    if not touched:
        return {}
    q = f"""
      SELECT g.id, g.t_start_ns, g.t_end_ns, so.kind
        FROM segments g
        JOIN sessions ss ON ss.id = g.session_id
        JOIN sources  so ON so.id = ss.source_id
       WHERE g.deleted_at IS NULL AND g.truth_verdict IS NULL
         AND g.id IN ({",".join(str(int(i)) for i in sorted(touched))})
    """
    spanq = """SELECT user_id, t_start_ns, COALESCE(t_end_ns, ?2)
                 FROM truth_speaking
                WHERE t_start_ns < ?2 AND COALESCE(t_end_ns, ?2) > ?1"""
    out = {}
    for sid, t0, t1, kind in c.execute(q).fetchall():
        dur = t1 - t0
        if dur <= 0:
            continue
        by = {}
        for uid, lo, hi in c.execute(spanq, (t0, t1)):
            if kind == "app" and uid in L.OWN:
                continue
            lo, hi = max(lo, t0), min(hi, t1)
            if hi > lo:
                by.setdefault(uid, []).append([lo, hi])
        present = []
        for uid, iv in by.items():
            tot = sum(b - a for a, b in L.merge(iv))
            frac = tot / dur
            if frac >= PRESENT_MIN:
                present.append((uid, frac))
        if len(present) != 1:
            continue
        uid, frac = present[0]
        if frac >= SINGLE_MIN:
            out[sid] = uid
    return out


def rows_for(c, extra_single):
    """`store.truth_calibration_rows` plus the pieces this pass just made
    scorable, in one chronological list."""
    rows = R.load_rows(c)
    if not extra_single:
        return rows
    speaker = {
        r[0]: r[1]
        for r in c.execute(
            "SELECT user_id, speaker_id FROM discord_users WHERE speaker_id IS NOT NULL"
        )
    }
    q = """
      SELECT g.id, g.t_start_ns, COALESCE(g.overlap_frac, 0.0),
             (g.t_end_ns - g.t_start_ns), e.vector, e.embed_model_id
        FROM segments g
        JOIN embeddings e ON e.id = (
             SELECT MAX(x.id) FROM embeddings x WHERE x.segment_id = g.id)
       WHERE g.id = ?1 AND g.deleted_at IS NULL
    """
    for sid, uid in extra_single.items():
        if uid not in speaker:
            continue
        got = c.execute(q, (sid,)).fetchone()
        if not got or got[5] != R.MODEL or got[3] < 1e9:
            continue
        row = R.Row()
        row.id, row.t, row.overlap = got[0], got[1], float(got[2])
        row.dur = got[3] / 1e9
        row.truth = speaker[uid]
        row.vec = R.blob_to_vec(got[4])
        row.session_id = row.source_id = row.t_end = None
        rows.append(row)
    rows.sort(key=lambda r: (r.t, r.id))
    return rows


def main(before, after):
    print(R.HEADER)
    for name, path, judge_new in (("before", before, False), ("after", after, True)):
        c = R.conn(path)
        extra = verdicts_for_unjudged(c) if judge_new else {}
        rows = rows_for(c, extra)
        bank = R.Bank(R.load_bank(c))
        cut = R.split_at([r.t for r in rows])
        held = rows[cut:]
        for agg, k in (("max", 1), ("topk", 3)):
            s, _ = R.judge(held, bank, {}, agg=agg, topk=k)
            print(s.row(f"{name}, held out, {agg}{k if agg == 'topk' else ''}"))
        if judge_new:
            print(f"  ({len(extra)} piece(s) newly judged `single`; "
                  f"corpus {len(rows)} rows, held out {len(held)})")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
