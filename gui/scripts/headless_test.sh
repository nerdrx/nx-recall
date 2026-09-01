#!/usr/bin/env bash
# Runs NX Recall against a private mock daemon inside a headless gamescope
# compositor, drives the real UI, and writes screenshots + a JSON report to
# gui/test-artifacts/ — so a UI check never opens a window on the developer's
# desktop and never touches the real recalld socket.
#
#   scripts/headless_test.sh [-o dir] [-W 1400] [-H 900] [-s secs]
#                            [--feed-ms 2000] [--theme light|dark|both]
#                            [--wayland] [--hold] [-- electron-args...]
#
#   --theme    which of NX Clear's two grounds to photograph (DESIGN §14.1).
#              Default `both`: the whole suite runs twice, once per theme, and
#              every artefact is suffixed -light / -dark so neither pass
#              overwrites the other. The theme is forced through
#              NX_RECALL_THEME, which the main process feeds to
#              nativeTheme.themeSource — the same path an OS switch takes.
#   --wayland  expose gamescope's own Wayland socket and run Electron on Ozone
#              Wayland; without it the app takes the XWayland path. Both are
#              worth testing.
#   --hold     skip the scripted driver and just leave the app running for
#              SETTLE seconds, then screenshot. Use it to LOOK at the UI. With
#              --theme both it holds on the light ground.
#
# Mirrors nx-hub/scripts/headless_test.sh; the private-socket discipline (never
# photograph or kill a gamescope that was already running) is copied verbatim
# because a real session on the desktop must never be disturbed.
set -uo pipefail
cd "$(dirname "$0")/.."

OUT=test-artifacts
W=1400; H=900; SETTLE=10; EXPOSE=0; HOLD=0; FEED_MS=2000; THEME=both
ARGS=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        -o) OUT="$2"; shift 2 ;;
        -W) W="$2"; shift 2 ;;
        -H) H="$2"; shift 2 ;;
        -s) SETTLE="$2"; shift 2 ;;
        --feed-ms) FEED_MS="$2"; shift 2 ;;
        --theme) THEME="$2"; shift 2 ;;
        --wayland) EXPOSE=1; shift ;;
        --hold) HOLD=1; shift ;;
        --) shift; ARGS=("$@"); break ;;
        *) ARGS+=("$1"); shift ;;
    esac
done

case "$THEME" in
    light|dark) THEMES=("$THEME") ;;
    both)       THEMES=(light dark) ;;
    *) echo "--theme must be light, dark or both (got: $THEME)"; exit 1 ;;
esac
# Holding is for looking at one window; two of them would just fight for the
# terminal. Take the first ground asked for.
[[ $HOLD -eq 1 ]] && THEMES=("${THEMES[0]}")

ELECTRON=node_modules/electron/dist/electron

command -v gamescope    >/dev/null || { echo "gamescope not installed"; exit 1; }
command -v gamescopectl >/dev/null || { echo "gamescopectl not installed"; exit 1; }
[[ -x $ELECTRON ]]                 || { echo "$ELECTRON missing - run npm install"; exit 1; }
[[ -f src/main/index.js ]]         || { echo "src/main/index.js missing"; exit 1; }

mkdir -p "$OUT"
rm -f "$OUT"/*.png "$OUT"/e2e-report*.json 2>/dev/null

RUNDIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"

# Every gamescope socket that exists before we start belongs to somebody else,
# and must never be photographed, killed, or waited on. Captured once for the
# whole run: a pass identifies its own compositor as "a gamescope-N that was not
# in this set", and the gap between passes waits for the set to come back to it.
gs_socks() {
    local s out=""
    for s in "$RUNDIR"/gamescope-*; do
        [[ -S "$s" && "$(basename "$s")" =~ ^gamescope-[0-9]+$ ]] || continue
        out+=" $(basename "$s")"
    done
    echo "$out"
}
PRE_SOCKS=$(gs_socks)

MOCK_PID=""; GS_PID=""; MOCK_SOCK=""
cleanup() {
    [[ -n "$GS_PID"   ]] && { kill "$GS_PID"   2>/dev/null; wait "$GS_PID"   2>/dev/null; }
    [[ -n "$MOCK_PID" ]] && { kill "$MOCK_PID" 2>/dev/null; wait "$MOCK_PID" 2>/dev/null; }
    [[ -n "$MOCK_SOCK" ]] && rm -f "$MOCK_SOCK"
    GS_PID=""; MOCK_PID=""; MOCK_SOCK=""
}
trap cleanup EXIT

# gamescope takes its XWayland display number and its own socket down
# asynchronously. Starting the next pass before that finishes means the new
# compositor claims a display the old one is still closing, and the app under
# test dies mid-run with "X connection error" — which looks exactly like a
# failing UI and is not one. Wait for our compositor's socket to actually go.
settle_compositor() {
    local cand extra
    for _ in $(seq 1 100); do
        extra=""
        for cand in $(gs_socks); do
            [[ " $PRE_SOCKS " == *" $cand "* ]] || extra="$cand"
        done
        [[ -z "$extra" ]] && break
        sleep 0.2
    done
    sleep 2
}

# ---------------------------------------------------------------------------
# one pass: one theme, one mock daemon, one compositor, one driven app
# ---------------------------------------------------------------------------
run_pass() {
    local theme=$1 suffix="-$1"

    echo
    echo "############################################################"
    echo "==> pass: the $theme ground"
    echo "############################################################"

    # A private socket per pass: the app under test must never reach the real
    # daemon, and two passes must never collide.
    MOCK_SOCK="$RUNDIR/nx-recall-e2e-$$-$theme.sock"
    local MOCK_LOG APP_LOG
    MOCK_LOG=$(mktemp /tmp/nx-recall-mockd-XXXXXX.log)
    APP_LOG=$(mktemp /tmp/nx-recall-app-XXXXXX.log)

    node mock/mockd.js --sock "$MOCK_SOCK" --feed-ms "$FEED_MS" >"$MOCK_LOG" 2>&1 &
    MOCK_PID=$!
    for _ in $(seq 1 50); do [[ -S "$MOCK_SOCK" ]] && break; sleep 0.1; done
    [[ -S "$MOCK_SOCK" ]] || { echo "mock daemon never came up:"; cat "$MOCK_LOG"; cleanup; settle_compositor; return 1; }
    echo "==> mock daemon on $MOCK_SOCK (pid $MOCK_PID)"

    local GS_ARGS=(--backend headless -W "$W" -H "$H" -w "$W" -h "$H")
    # Electron in a nested headless compositor: no sandbox (no user namespaces
    # here) and software GL, since the headless backend exposes no real device.
    local APP_ARGS=(. --no-sandbox --disable-gpu-sandbox --disable-dev-shm-usage)
    if [[ $EXPOSE -eq 1 ]]; then
        GS_ARGS+=(--expose-wayland)
        APP_ARGS+=(--ozone-platform=wayland --enable-features=UseOzonePlatform)
    fi
    APP_ARGS+=("${ARGS[@]+"${ARGS[@]}"}")

    # The driver signals the mock (SIGUSR1) to fake a daemon restart, so it needs
    # the pid. Nothing else in the app ever learns it.
    local ENVS=(
        NX_RECALL_SOCK="$MOCK_SOCK"
        NX_RECALL_E2E_OUT="$PWD/$OUT"
        NX_RECALL_MOCK_PID="$MOCK_PID"
        NX_RECALL_THEME="$theme"
        NX_RECALL_E2E_SUFFIX="$suffix"
    )
    if [[ $HOLD -eq 1 ]]; then
        ENVS+=(NX_RECALL_E2E=0)
    else
        ENVS+=(NX_RECALL_E2E=1)
    fi

    gamescope "${GS_ARGS[@]}" -- env "${ENVS[@]}" "$ELECTRON" "${APP_ARGS[@]}" >"$APP_LOG" 2>&1 &
    GS_PID=$!

    # Only `gamescope-<N>` counts (the runtime dir also carries gamescope-limiter-*
    # and -ei/.lock siblings), and only a socket that was NOT there when this run
    # began is ours — never one belonging to the developer's own session.
    local SOCK="" cand
    for _ in $(seq 1 150); do
        for cand in $(gs_socks); do
            [[ " $PRE_SOCKS " == *" $cand "* ]] && continue
            SOCK="$cand"
        done
        [[ -n "$SOCK" ]] && break
        sleep 0.1
    done
    [[ -n "$SOCK" ]] || { echo "gamescope never came up:"; tail -20 "$APP_LOG"; cleanup; settle_compositor; return 1; }
    echo "==> gamescope on $SOCK"

    sleep "$SETTLE"     # let Electron boot, connect, and paint a few frames

    # A compositor-level grab, independent of Electron's own capturePage: it
    # proves the window really reached a screen rather than only rendering
    # offscreen — and on the light ground it is also the check that the WINDOW
    # background matches the page's, with no flash-coloured frame around it.
    if GAMESCOPE_WAYLAND_DISPLAY="$SOCK" gamescopectl screenshot "$OUT/compositor$suffix.png" >/dev/null 2>&1; then
        for _ in $(seq 1 50); do [[ -s "$OUT/compositor$suffix.png" ]] && break; sleep 0.1; done
        echo "==> compositor screenshot: $OUT/compositor$suffix.png ($(stat -c%s "$OUT/compositor$suffix.png" 2>/dev/null || echo 0) bytes)"
    else
        echo "==> compositor screenshot failed (the app's own capturePage shots still apply)"
    fi

    if [[ $HOLD -eq 1 ]]; then
        echo "==> hold mode: no driver ran. App log: $APP_LOG"
        return 0
    fi

    # The driver quits the app when it is done; give it a hard ceiling anyway.
    for _ in $(seq 1 240); do
        kill -0 "$GS_PID" 2>/dev/null || break
        sleep 1
    done

    echo
    echo "=== app log, $theme (filtered) ==="
    grep -aE "\[e2e\]|\[recall\]|ERROR|Error:" "$APP_LOG" | head -40

    echo
    local rc
    if [[ -f "$OUT/e2e-report$suffix.json" ]]; then
        node -e '
          const r = require("./'"$OUT"'/e2e-report'"$suffix"'.json");
          const t = r.theme ? ` · ground ${r.theme.dark ? "dark" : "light"} ${r.theme.ground}` : "";
          console.log(`=== e2e ['"$theme"']: ${r.passed} passed, ${r.failed} failed${t} ===`);
          for (const s of r.results) console.log(` ${s.ok ? "PASS" : "FAIL"}  ${s.name}${s.ok ? "" : "  — " + s.error}`);
          process.exit(r.failed ? 1 : 0);
        '
        rc=$?
    else
        echo "=== e2e [$theme]: no report written — the driver did not finish ==="
        tail -30 "$APP_LOG"
        rc=1
    fi
    echo "=== full app log: $APP_LOG · mock log: $MOCK_LOG ==="

    cleanup
    settle_compositor
    return $rc
}

# ---------------------------------------------------------------------------

RC=0
FAILED_THEMES=()
for theme in "${THEMES[@]}"; do
    run_pass "$theme" || { RC=1; FAILED_THEMES+=("$theme"); }
done

if [[ $HOLD -eq 1 ]]; then
    exit $RC
fi

echo
echo "=== artifacts in $OUT ==="
ls -la "$OUT" 2>/dev/null | tail -n +2
echo
if [[ $RC -eq 0 ]]; then
    echo "=== all passes green: ${THEMES[*]} ==="
else
    echo "=== FAILED on: ${FAILED_THEMES[*]} ==="
fi
exit $RC
