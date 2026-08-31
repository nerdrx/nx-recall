#!/usr/bin/env bash
# Runs NX Recall against a private mock daemon inside a headless gamescope
# compositor, drives the real UI, and writes screenshots + a JSON report to
# gui/test-artifacts/ — so a UI check never opens a window on the developer's
# desktop and never touches the real recalld socket.
#
#   scripts/headless_test.sh [-o dir] [-W 1400] [-H 900] [-s secs]
#                            [--feed-ms 2000] [--wayland] [--hold]
#                            [-- electron-args...]
#
#   --wayland  expose gamescope's own Wayland socket and run Electron on Ozone
#              Wayland; without it the app takes the XWayland path. Both are
#              worth testing.
#   --hold     skip the scripted driver and just leave the app running for
#              SETTLE seconds, then screenshot. Use it to LOOK at the UI.
#
# Mirrors nx-hub/scripts/headless_test.sh; the private-socket discipline (never
# photograph or kill a gamescope that was already running) is copied verbatim
# because a real session on the desktop must never be disturbed.
set -uo pipefail
cd "$(dirname "$0")/.."

OUT=test-artifacts
W=1400; H=900; SETTLE=10; EXPOSE=0; HOLD=0; FEED_MS=2000
ARGS=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        -o) OUT="$2"; shift 2 ;;
        -W) W="$2"; shift 2 ;;
        -H) H="$2"; shift 2 ;;
        -s) SETTLE="$2"; shift 2 ;;
        --feed-ms) FEED_MS="$2"; shift 2 ;;
        --wayland) EXPOSE=1; shift ;;
        --hold) HOLD=1; shift ;;
        --) shift; ARGS=("$@"); break ;;
        *) ARGS+=("$1"); shift ;;
    esac
done

ELECTRON=node_modules/electron/dist/electron

command -v gamescope    >/dev/null || { echo "gamescope not installed"; exit 1; }
command -v gamescopectl >/dev/null || { echo "gamescopectl not installed"; exit 1; }
[[ -x $ELECTRON ]]                 || { echo "$ELECTRON missing - run npm install"; exit 1; }
[[ -f src/main/index.js ]]         || { echo "src/main/index.js missing"; exit 1; }

mkdir -p "$OUT"
rm -f "$OUT"/*.png "$OUT"/e2e-report.json 2>/dev/null

RUNDIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
# A private socket per run: the app under test must never reach the real daemon,
# and two runs must never collide.
MOCK_SOCK="$RUNDIR/nx-recall-e2e-$$.sock"
MOCK_LOG=$(mktemp /tmp/nx-recall-mockd-XXXXXX.log)
APP_LOG=$(mktemp /tmp/nx-recall-app-XXXXXX.log)

node mock/mockd.js --sock "$MOCK_SOCK" --feed-ms "$FEED_MS" >"$MOCK_LOG" 2>&1 &
MOCK_PID=$!
for _ in $(seq 1 50); do [[ -S "$MOCK_SOCK" ]] && break; sleep 0.1; done
[[ -S "$MOCK_SOCK" ]] || { echo "mock daemon never came up:"; cat "$MOCK_LOG"; kill $MOCK_PID 2>/dev/null; exit 1; }
echo "==> mock daemon on $MOCK_SOCK (pid $MOCK_PID)"

GS_ARGS=(--backend headless -W "$W" -H "$H" -w "$W" -h "$H")
# Electron in a nested headless compositor: no sandbox (no user namespaces
# here) and software GL, since the headless backend exposes no real device.
APP_ARGS=(. --no-sandbox --disable-gpu-sandbox --disable-dev-shm-usage)
if [[ $EXPOSE -eq 1 ]]; then
    GS_ARGS+=(--expose-wayland)
    APP_ARGS+=(--ozone-platform=wayland --enable-features=UseOzonePlatform)
fi
APP_ARGS+=("${ARGS[@]+"${ARGS[@]}"}")

ENVS=(NX_RECALL_SOCK="$MOCK_SOCK" NX_RECALL_E2E_OUT="$PWD/$OUT")
if [[ $HOLD -eq 1 ]]; then
    ENVS+=(NX_RECALL_E2E=0)
else
    ENVS+=(NX_RECALL_E2E=1)
fi

export GAMESCOPE_WAYLAND_DISPLAY=""
export ELECTRON_DISABLE_SANDBOX=1
export ELECTRON_ENABLE_LOGGING=1

# Every gamescope socket that already exists belongs to somebody else.
PRE_SOCKS=""
for s in "$RUNDIR"/gamescope-*; do
    [[ -S "$s" ]] || continue
    PRE_SOCKS+=" $(basename "$s")"
done

gamescope "${GS_ARGS[@]}" -- env "${ENVS[@]}" "$ELECTRON" "${APP_ARGS[@]}" >"$APP_LOG" 2>&1 &
GS_PID=$!
cleanup() {
    kill "$GS_PID" 2>/dev/null; wait "$GS_PID" 2>/dev/null
    kill "$MOCK_PID" 2>/dev/null; wait "$MOCK_PID" 2>/dev/null
    rm -f "$MOCK_SOCK"
}
trap cleanup EXIT

# Only `gamescope-<N>` counts (the runtime dir also carries gamescope-limiter-*
# and -ei/.lock siblings), and only a socket that was NOT there before is ours.
is_gs_sock() { [[ -S "$1" && "$(basename "$1")" =~ ^gamescope-[0-9]+$ ]]; }
SOCK=""
for _ in $(seq 1 150); do
    for s in "$RUNDIR"/gamescope-*; do
        is_gs_sock "$s" || continue
        cand=$(basename "$s")
        [[ " $PRE_SOCKS " == *" $cand "* ]] && continue
        SOCK="$cand"
    done
    [[ -n "$SOCK" ]] && break
    sleep 0.1
done
[[ -n "$SOCK" ]] || { echo "gamescope never came up:"; tail -20 "$APP_LOG"; exit 1; }
echo "==> gamescope on $SOCK"

sleep "$SETTLE"     # let Electron boot, connect, and paint a few frames

# A compositor-level grab, independent of Electron's own capturePage: it proves
# the window really reached a screen rather than only rendering offscreen.
if GAMESCOPE_WAYLAND_DISPLAY="$SOCK" gamescopectl screenshot "$OUT/compositor.png" >/dev/null 2>&1; then
    for _ in $(seq 1 50); do [[ -s "$OUT/compositor.png" ]] && break; sleep 0.1; done
    echo "==> compositor screenshot: $OUT/compositor.png ($(stat -c%s "$OUT/compositor.png" 2>/dev/null || echo 0) bytes)"
else
    echo "==> compositor screenshot failed (the app's own capturePage shots still apply)"
fi

if [[ $HOLD -eq 1 ]]; then
    echo "==> hold mode: no driver ran. App log: $APP_LOG"
    exit 0
fi

# The driver quits the app when it is done; give it a hard ceiling anyway.
for _ in $(seq 1 240); do
    kill -0 "$GS_PID" 2>/dev/null || break
    sleep 1
done

echo
echo "=== app log (filtered) ==="
grep -aE "\[e2e\]|\[recall\]|ERROR|Error:" "$APP_LOG" | head -40

echo
if [[ -f "$OUT/e2e-report.json" ]]; then
    node -e '
      const r = require("./'"$OUT"'/e2e-report.json");
      console.log(`=== e2e: ${r.passed} passed, ${r.failed} failed (socket ${r.socket}) ===`);
      for (const s of r.results) console.log(` ${s.ok ? "PASS" : "FAIL"}  ${s.name}${s.ok ? "" : "  — " + s.error}`);
      process.exit(r.failed ? 1 : 0);
    '
    RC=$?
else
    echo "=== e2e: no report written — the driver did not finish ==="
    tail -30 "$APP_LOG"
    RC=1
fi

echo
echo "=== artifacts in $OUT ==="
ls -la "$OUT" 2>/dev/null | tail -n +2
echo "=== full app log: $APP_LOG · mock log: $MOCK_LOG ==="
exit $RC
