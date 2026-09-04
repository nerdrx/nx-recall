"""Ground-truth speaker-change points inside a turn, from Discord's per-user spans.

Read-only against a *copy* of the database. Mirrors crate::truth's rules:
  * a user's own spans are merged before anything is counted;
  * `Audible`: on `sources.kind = 'app'` the user's own Discord account is not
    present, because a Discord client never plays your microphone back to you
    (FINDINGS §17 finding 1, §34).

A change point is the boundary between two consecutive *solo* intervals that
belong to different people. Where the two are separated by silence or by a
stretch with two mouths open, the change point is the middle of that stretch:
that is the interval in which any honest cut has to land.
"""

import os
import sqlite3

SCRATCH = "/tmp/nx-recall-workspace/nx-scratch/agents-2026-09-04/turnsplit"
DB = os.path.join(SCRATCH, "work.db")
CLIPS = os.path.expanduser("~/.local/share/nx-recall")
OWN = {"157558682291273728"}
NS = 1e9


def conn(path=DB):
    return sqlite3.connect(f"file:{path}?mode=ro", uri=True)


def merge(iv):
    iv = sorted(iv)
    out = []
    for lo, hi in iv:
        if out and lo <= out[-1][1]:
            out[-1][1] = max(out[-1][1], hi)
        else:
            out.append([lo, hi])
    return [(a, b) for a, b in out]


class Turn:
    __slots__ = ("id", "t0", "t1", "path", "verdict", "user", "kind", "dur",
                 "by_user", "changes", "solo", "session_id", "overlap_frac",
                 "text", "speaker_id")


def load_turns(c, verdicts=("single", "partial", "overlap")):
    q = f"""
      SELECT g.id, g.t_start_ns, g.t_end_ns, g.audio_path, g.truth_verdict,
             g.truth_user_id, so.kind, g.session_id, COALESCE(g.overlap_frac, 0.0),
             g.text, g.speaker_id
        FROM segments g
        JOIN sessions ss ON ss.id = g.session_id
        JOIN sources  so ON so.id = ss.source_id
       WHERE g.deleted_at IS NULL
         AND g.truth_verdict IN ({",".join("?" * len(verdicts))})
       ORDER BY g.t_start_ns
    """
    turns = []
    for r in c.execute(q, verdicts):
        t = Turn()
        (t.id, t.t0, t.t1, t.path, t.verdict, t.user, t.kind, t.session_id,
         t.overlap_frac, t.text, t.speaker_id) = r
        t.dur = (t.t1 - t.t0) / NS
        turns.append(t)
    return turns


def spans_for(c, t):
    q = """SELECT user_id, t_start_ns, COALESCE(t_end_ns, ?2)
             FROM truth_speaking
            WHERE t_start_ns < ?2 AND COALESCE(t_end_ns, ?2) > ?1"""
    by = {}
    for uid, lo, hi in c.execute(q, (t.t0, t.t1)):
        if t.kind == "app" and uid in OWN:
            continue
        lo, hi = max(lo, t.t0), min(hi, t.t1)
        if hi <= lo:
            continue
        by.setdefault(uid, []).append([lo, hi])
    return {u: merge(v) for u, v in by.items()}


def solo_intervals(by_user):
    """Maximal stretches where exactly one audible user is talking (ns)."""
    edges = sorted({e for iv in by_user.values() for lohi in iv for e in lohi})
    out = []
    for a, b in zip(edges, edges[1:]):
        mid = (a + b) / 2
        who = [u for u, iv in by_user.items() if any(lo <= mid < hi for lo, hi in iv)]
        if len(who) != 1:
            continue
        if out and out[-1][2] == who[0] and out[-1][1] == a:
            out[-1][1] = b
        else:
            out.append([a, b, who[0]])
    return [(a, b, u) for a, b, u in out]


def change_points(t, by_user):
    """(times in seconds from the turn's start, solo intervals in seconds)."""
    solo = solo_intervals(by_user)
    cps = []
    for (_a0, a1, ua), (b0, _b1, ub) in zip(solo, solo[1:]):
        if ua == ub:
            continue
        cps.append(((a1 + b0) / 2 - t.t0) / NS)
    return cps, [((a - t.t0) / NS, (b - t.t0) / NS, u) for a, b, u in solo]


def annotate(c, turns):
    for t in turns:
        t.by_user = spans_for(c, t)
        t.changes, t.solo = change_points(t, t.by_user)
    return turns


def owner_at(t, when_s):
    """Which user Discord says was talking at `when_s` seconds into the turn."""
    for lo, hi, u in t.solo:
        if lo <= when_s < hi:
            return u
    return None


def piece_owner(t, lo_s, hi_s):
    """The user with the most solo time in [lo_s, hi_s), or None."""
    best, who = 0.0, None
    tot = {}
    for a, b, u in t.solo:
        ov = min(b, hi_s) - max(a, lo_s)
        if ov > 0:
            tot[u] = tot.get(u, 0.0) + ov
    for u, v in tot.items():
        if v > best:
            best, who = v, u
    return who, best


if __name__ == "__main__":
    import sys
    from collections import Counter

    c = conn(sys.argv[1] if len(sys.argv) > 1 else DB)
    turns = annotate(c, load_turns(c))
    per, withcp, ncp, durs = Counter(), Counter(), Counter(), {}
    for t in turns:
        per[t.verdict] += 1
        if t.changes:
            withcp[t.verdict] += 1
            ncp[t.verdict] += len(t.changes)
        durs.setdefault(t.verdict, []).append(t.dur)
    print(f"{'verdict':<10}{'turns':>8}{'>=1 change':>13}{'change pts':>12}{'median s':>10}")
    for v in ("single", "partial", "overlap"):
        d = sorted(durs.get(v, [0]))
        print(f"{v:<10}{per[v]:>8}{withcp[v]:>13}{ncp[v]:>12}{d[len(d) // 2]:>10.2f}")
    for lim in (0.5, 0.75, 1.0, 1.5):
        n = sum(
            1
            for t in turns
            if t.verdict != "single"
            for cp in t.changes
            if cp >= lim and (t.dur - cp) >= lim
        )
        m = sum(
            1
            for t in turns
            if t.verdict == "single"
            for cp in t.changes
            if cp >= lim and (t.dur - cp) >= lim
        )
        print(f"reachable (both pieces >= {lim}s): partial/overlap {n}, single {m}")
