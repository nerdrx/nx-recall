"""Record a VRChat lobby, then measure it.

    python record_lobby.py                 # record until you hit Ctrl+C, then analyse
    python record_lobby.py 20              # record 20 minutes, then analyse
    python record_lobby.py 20 --no-analyse # just record

Records the OUTPUT MONITOR — what you actually hear — not your microphone. That is
the correct signal: it is the mixed, spatialised, codec'd audio the daemon would
capture, and it excludes your own voice (VRChat does not play you back to yourself).

Captured straight to 16 kHz mono FLAC, which is what the analyser wants and what the
production pipeline resamples to anyway. ~7 MB per 20 minutes.

Note on Bluetooth: the sink monitor taps the PCM stream *before* the Bluetooth codec,
so a BT headset does not degrade the recording.
"""

from __future__ import annotations

import signal
import subprocess
import sys
import time
from datetime import datetime
from pathlib import Path

SOURCE = "@DEFAULT_MONITOR@"      # pipewire-pulse resolves this to the active sink


def default_sink() -> str:
    try:
        return subprocess.run(["pactl", "get-default-sink"], capture_output=True,
                              text=True, check=True).stdout.strip()
    except Exception:
        return "unknown"


def record(out: Path, minutes: float | None) -> bool:
    cmd = ["ffmpeg", "-hide_banner", "-loglevel", "error", "-y",
           "-f", "pulse", "-i", SOURCE, "-ac", "1", "-ar", "16000"]
    if minutes:
        cmd += ["-t", str(int(minutes * 60))]
    cmd += ["-c:a", "flac", str(out)]

    print(f"  sink   : {default_sink()}")
    print(f"  output : {out}")
    print(f"  length : {f'{minutes:g} min' if minutes else 'until Ctrl+C'}\n")
    print("  Recording. Get into a busy lobby and just talk normally.")
    print("  Music and world audio are fine — leave them in, they are realistic.\n")

    p = subprocess.Popen(cmd, stdin=subprocess.PIPE)
    t0 = time.time()
    try:
        while p.poll() is None:
            time.sleep(1)
            el = time.time() - t0
            print(f"\r  {int(el)//60:02d}:{int(el)%60:02d} elapsed", end="", flush=True)
    except KeyboardInterrupt:
        print("\n  stopping…")
        try:                       # 'q' lets ffmpeg finalise the FLAC header cleanly
            p.communicate(b"q", timeout=5)
        except Exception:
            p.send_signal(signal.SIGINT)
            p.wait(timeout=10)
    print()

    if not out.is_file() or out.stat().st_size < 4096:
        print("  Nothing was captured.")
        print("  If VRChat plays through a different device than the default sink,")
        print("  run:  pactl list short sinks     and set SOURCE to '<name>.monitor'")
        return False
    print(f"  saved {out.stat().st_size/1e6:.1f} MB")
    return True


def main() -> int:
    if {"--help", "-h"} & set(sys.argv[1:]):
        print(__doc__)
        return 0
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    minutes = float(args[0]) if args else None
    out = Path(f"lobby_{datetime.now():%Y%m%d_%H%M}.flac")

    print("\n" + "=" * 60 + "\nNX Recall — lobby recording\n" + "=" * 60)
    if not record(out, minutes):
        return 1

    if "--no-analyse" in sys.argv or "--no-analyze" in sys.argv:
        print(f"\n  analyse later with:  python measure_lobby.py {out}")
        return 0

    print("\n  analysing…")
    return subprocess.run([sys.executable,
                           str(Path(__file__).parent / "measure_lobby.py"),
                           str(out)]).returncode


if __name__ == "__main__":
    raise SystemExit(main())
