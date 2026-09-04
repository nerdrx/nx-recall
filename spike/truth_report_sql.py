#!/usr/bin/env python3
"""`truth report`'s arithmetic, straight off a copy, so before/after can be
compared without a running daemon (§34).

    python3 spike/truth_report_sql.py <copy>.db [max_overlap]
"""
import sqlite3
import sys

DB = sys.argv[1]
THR = float(sys.argv[2]) if len(sys.argv) > 2 else 0.06
c = sqlite3.connect(f"file:{DB}?mode=ro", uri=True)
you = int(c.execute("SELECT value FROM settings WHERE key='you_speaker_id'").fetchone()[0])

print(DB)
for v, n in c.execute(
    "SELECT truth_verdict, COUNT(*) FROM segments WHERE deleted_at IS NULL "
    "AND truth_verdict IS NOT NULL GROUP BY 1 ORDER BY 2 DESC"
):
    print(f"  {v:8} {n}")

rows = c.execute(
    """SELECT d.speaker_id, g.speaker_id FROM segments g
       JOIN discord_users d ON d.user_id = g.truth_user_id
      WHERE g.deleted_at IS NULL AND g.truth_verdict='single'
        AND d.speaker_id IS NOT NULL AND (g.t_end_ns-g.t_start_ns) >= 1000000000""",
).fetchall()
own = sum(1 for t, _ in rows if t == you)
rows = [r for r in rows if r[0] != you]
correct = sum(1 for t, h in rows if h == t)
wrong = sum(1 for t, h in rows if h is not None and h != t)
declined = sum(1 for t, h in rows if h is None)
print(f"\nidentity scored on {len(rows)} (own-account rows excluded: {own})")
print(f"  correct {correct}  wrong {wrong}  declined {declined}")
print(f"  precision {correct/(correct+wrong):.3%}  recall {correct/len(rows):.3%}")

gate = c.execute(
    "SELECT truth_verdict, COALESCE(overlap_frac,0.0) FROM segments "
    "WHERE deleted_at IS NULL AND truth_verdict IN ('single','overlap')"
).fetchall()
caught = sum(1 for v, f in gate if v == "overlap" and f > THR)
total = sum(1 for v, _ in gate if v == "overlap")
false = sum(1 for v, f in gate if v == "single" and f > THR)
print(f"\noverlap gate at {THR}: caught {caught}/{total}, false alarms {false}")
print(f"  precision {caught/(caught+false):.3%}  recall {caught/total:.3%}")
