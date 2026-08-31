"""VRChat instance-roster watcher — prototype for the daemon's session_roster feed.

The design brief (§5) leans on "the VRChat OSC instance roster" three times, but the
roster does not come over OSC — it comes from VRChat's output log, which logs every
join/leave with a display name. This prototype validates that source.

    roster_watch.py                     # snapshot of the current live session
    roster_watch.py --follow            # tail the log, emit JSONL events
    roster_watch.py --log <file>        # parse a specific historical log

Events (JSONL on stdout in --follow mode):
    {"t": <utc_ns>, "ev": "join"|"leave"|"world", "who"|"world_id": ...}

Timestamps are parsed from the log's local-time stamps and converted to UTC ns to
match the capture clock discipline. Names stay on this machine.
"""

from __future__ import annotations

import argparse
import json
import re
import time
from datetime import datetime
from pathlib import Path

GLOBS = [
    "~/.local/share/Steam/steamapps/compatdata/438100/pfx/drive_c/users/steamuser"
    "/AppData/LocalLow/VRChat/VRChat/output_log_*.txt",
]

# `2026.08.31 18:45:57 Debug      -  [Behaviour] OnPlayerJoined Name (usr_xxx)`
# The trailing ` (usr_…)` id is present in newer builds and absent in older ones.
LINE = re.compile(
    r"^(\d{4}\.\d{2}\.\d{2} \d{2}:\d{2}:\d{2}) .*?\[Behaviour\] "
    r"(?:OnPlayer(Joined|Left) (.+?)(?: \(usr_[0-9a-f-]+\))?"
    r"|Joining (wrld_[0-9a-f-]+):(\S+)"
    r"|Entering Room: (.+))\s*$")


def utc_ns(stamp: str) -> int:
    dt = datetime.strptime(stamp, "%Y.%m.%d %H:%M:%S").astimezone()
    return int(dt.timestamp() * 1e9)


def parse_line(line: str) -> dict | None:
    m = LINE.match(line)
    if not m:
        return None
    t = utc_ns(m.group(1))
    if m.group(2):
        # Names may carry trailing whitespace before the (usr_…) id, and are
        # frequently non-ASCII; strip but never otherwise normalise.
        return {"t": t, "ev": "join" if m.group(2) == "Joined" else "leave",
                "who": m.group(3).strip()}
    if m.group(4):
        return {"t": t, "ev": "world", "world_id": m.group(4),
                "instance": m.group(5).split("~")[0]}
    return {"t": t, "ev": "room", "name": m.group(6)}


def newest_log() -> Path | None:
    hits: list[Path] = []
    for g in GLOBS:
        hits += Path(g).expanduser().parent.glob(Path(g).name)
    return max(hits, key=lambda p: p.stat().st_mtime) if hits else None


def replay(path: Path):
    roster: dict[str, int] = {}
    world = None
    for line in path.read_text(errors="replace").splitlines():
        ev = parse_line(line)
        if not ev:
            continue
        if ev["ev"] == "world":
            world, roster = ev, {}
        elif ev["ev"] == "join":
            roster[ev["who"]] = ev["t"]
        elif ev["ev"] == "leave":
            roster.pop(ev["who"], None)
        yield ev, roster, world


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--log", type=Path)
    ap.add_argument("--follow", action="store_true")
    ap.add_argument("--names", action="store_true",
                    help="print display names in the snapshot (default: counts only)")
    a = ap.parse_args()

    log = a.log or newest_log()
    if not log:
        print("no VRChat log found")
        return 1

    last = None
    for last in replay(log):
        if a.follow:
            pass  # replay history silently, then tail
    if last:
        ev, roster, world = last
        print(f"# log: {log.name}")
        if world:
            print(f"# instance: {world['world_id']}:{world['instance']}")
        print(f"# roster now: {len(roster)} players")
        if a.names:
            for who, t in sorted(roster.items(), key=lambda kv: kv[1]):
                print(f"#   {who}")
    if not a.follow:
        return 0

    with log.open(errors="replace") as f:
        f.seek(0, 2)
        while True:
            line = f.readline()
            if not line:
                time.sleep(0.5)
                nl = newest_log()
                if nl and nl != log:      # world change can rotate the log
                    log = nl
                    f.close()
                    f = log.open(errors="replace")
                continue
            ev = parse_line(line)
            if ev:
                print(json.dumps(ev), flush=True)


if __name__ == "__main__":
    raise SystemExit(main())
