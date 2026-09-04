#!/usr/bin/env python3
"""What the own-account rule does to the verdicts already on disk (§34).

Read-only, against a `.backup` copy. Mirrors `truth::coverage`,
`truth::verdict` and `truth::simultaneous_frac` exactly, once with every
Discord account counted as present (today's rule) and once with the user's
own account dropped on application audio (the corrected one).

    python3 spike/truth_rejudge.py <copy>.db [own_user_id]
"""
import bisect
import collections
import sqlite3
import sys

DB = sys.argv[1] if len(sys.argv) > 1 else "copy.db"
PRESENT_MIN, SINGLE_MIN = 0.2, 0.8
REACH = 5 * 60 * 10**9

c = sqlite3.connect(f"file:{DB}?mode=ro", uri=True)
names = dict(c.execute("SELECT user_id, name FROM discord_users").fetchall())
if len(sys.argv) > 2:
    OWN = sys.argv[2]
else:
    you = c.execute("SELECT value FROM settings WHERE key='you_speaker_id'").fetchone()
    OWN = c.execute(
        "SELECT user_id FROM discord_users WHERE speaker_id = ?", (int(you[0]),)
    ).fetchone()[0]
print(f"own account: {OWN} ({names.get(OWN)})")

segs = c.execute(
    """
  SELECT g.id, g.t_start_ns, g.t_end_ns, g.truth_verdict, g.truth_user_id,
         g.truth_coverage, sc.kind, g.overlap_frac, g.truth_overlap_frac
    FROM segments g
    JOIN sessions ss ON ss.id = g.session_id
    JOIN sources sc ON sc.id = ss.source_id
   WHERE g.deleted_at IS NULL AND g.truth_verdict IS NOT NULL
   ORDER BY g.t_start_ns
"""
).fetchall()
spans = c.execute(
    "SELECT user_id, t_start_ns, COALESCE(t_end_ns, t_start_ns) FROM truth_speaking "
    "ORDER BY t_start_ns"
).fetchall()
starts = [s[1] for s in spans]


def spans_between(a, b):
    i = max(0, bisect.bisect_left(starts, a) - 2000)
    out = []
    for u, s, e in spans[i:]:
        if s >= b:
            break
        if e > a:
            out.append((u, s, e))
    return out


def merged(sp, a, b, drop):
    by = collections.defaultdict(list)
    for u, s, e in sp:
        if u == drop:
            continue
        lo, hi = max(s, a), min(e, b)
        if hi > lo:
            by[u].append((lo, hi))
    out = {}
    for u, lst in by.items():
        lst.sort()
        runs, cur = [], None
        for lo, hi in lst:
            if cur and lo <= cur[1]:
                cur = (cur[0], max(cur[1], hi))
            else:
                if cur:
                    runs.append(cur)
                cur = (lo, hi)
        if cur:
            runs.append(cur)
        out[u] = runs
    return out


def coverage(sp, a, b, drop=None):
    dur = b - a
    if dur <= 0:
        return []
    cov = [
        (u, min(1.0, sum(hi - lo for lo, hi in runs) / dur))
        for u, runs in merged(sp, a, b, drop).items()
    ]
    cov.sort(key=lambda x: (-x[1], x[0]))
    return cov


def verdict(cov, nearby):
    present = [x for x in cov if x[1] >= PRESENT_MIN]
    if not present:
        return ("nobody" if nearby else "unknown", None, None)
    if len(present) == 1:
        u, f = present[0]
        return ("single" if f >= SINGLE_MIN else "partial", u, f)
    return ("overlap", None, None)


def simultaneous(sp, a, b, drop=None):
    dur = b - a
    if dur <= 0:
        return 0.0
    evs = []
    for runs in merged(sp, a, b, drop).values():
        for lo, hi in runs:
            evs.append((lo, 1))
            evs.append((hi, -1))
    evs.sort()
    depth, prev, tot = 0, None, 0
    for t, d in evs:
        if depth >= 2:
            tot += t - prev
        depth += d
        prev = t
    return tot / dur


rows = []
disagree = collections.Counter()
for sid, a, b, v, tu, tc, kind, det, tof in segs:
    sp = spans_between(a, b)
    nearby = bool(spans_between(a - REACH, b + REACH))
    drop = OWN if kind == "app" else None
    old = verdict(coverage(sp, a, b), nearby)
    new = verdict(coverage(sp, a, b, drop), nearby)
    if old[0] != v:
        disagree[(v, old[0])] += 1
    rows.append(
        dict(
            id=sid,
            t=a,
            stored=v,
            kind=kind,
            det=det,
            old=old,
            new=new,
            spans=bool(sp),
            simul_old=simultaneous(sp, a, b),
            simul_new=simultaneous(sp, a, b, drop),
        )
    )

print(f"\nverdicted segments: {len(segs)}")
print(f"rows with no surviving span: {sum(1 for r in rows if not r['spans'])}")
if disagree:
    print("\nstored verdict vs recomputed under TODAY's rule (should be empty):")
    for (a_, b_), n in disagree.most_common():
        print(f"  {a_:8} -> {b_:8} {n}")

print("\nre-verdict under the corrected rule (recomputed old -> new):")
tab = collections.Counter((r["old"][0], r["new"][0]) for r in rows)
for (o, n_), k in sorted(tab.items(), key=lambda x: -x[1]):
    mark = "" if o == n_ else "   <-- changed"
    print(f"  {o:8} -> {n_:8} {k:6}{mark}")

print("\nverdict totals   before   after")
for v in ("single", "overlap", "partial", "nobody", "unknown"):
    print(
        f"  {v:8} {sum(1 for r in rows if r['old'][0]==v):8} "
        f"{sum(1 for r in rows if r['new'][0]==v):7}"
    )

print("\nwho the overlap rows become:")
for u, n in collections.Counter(
    r["new"][1] for r in rows if r["old"][0] == "overlap" and r["new"][0] in ("single", "partial")
).most_common():
    print(f"  {names.get(u, u):20} {n}")

with open("rejudge_rows.csv", "w") as f:
    f.write("id,t_start_ns,stored,old,new,new_user,new_cov,detector,simul_old,simul_new\n")
    for r in rows:
        f.write(
            f"{r['id']},{r['t']},{r['stored']},{r['old'][0]},{r['new'][0]},"
            f"{r['new'][1] or ''},{r['new'][2] if r['new'][2] is not None else ''},"
            f"{r['det'] if r['det'] is not None else ''},{r['simul_old']:.4f},{r['simul_new']:.4f}\n"
        )
print("\nwrote rejudge_rows.csv")
