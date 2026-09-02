#!/usr/bin/env bash
# Package NX Recall for NX Hub.
#
# Produces dist/nx-recall-<version>-linux-x86_64.tar.gz laid out as `usr/…`, so
# the hub's tarball-prefix engine (stripPrefix "usr/", prefix "~/.local") copies
# it into the user's home and records every single absolute path it wrote — that
# per-file manifest is what makes uninstall exact, and it is the reason the two
# files this package puts outside its own subtrees (the systemd user unit and
# the desktop entry) are safe to ship at all. See nx-app.json and
# usr/share/nx-recall/MANIFEST.txt, which this script generates.
#
# What is NOT in the tarball, deliberately:
#   - the analysis models (~700 MB). `recalld models fetch` puts them in the
#     data dir, which is out-of-manifest and survives uninstall (DESIGN §10).
#   - anything under ~/.local/share/nx-recall or ~/.config/nx-recall.
#
#   packaging/build-release.sh                 build + package
#   packaging/build-release.sh --skip-build    package whatever is in target/release
#   packaging/build-release.sh --no-strip      keep symbols in the shipped binary
#
# This script never publishes. `gh release create` is printed, not run.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
ROOT=$PWD

SKIP_BUILD=0
STRIP=1
for arg in "$@"; do
    case "$arg" in
        --skip-build) SKIP_BUILD=1 ;;
        --no-strip)   STRIP=0 ;;
        -h|--help)    sed -n '2,22p' "${BASH_SOURCE[0]}"; exit 0 ;;
        *) echo "unknown option: $arg" >&2; exit 1 ;;
    esac
done

VERSION=$(sed -n '/^\[workspace\.package\]/,/^\[/p' Cargo.toml \
          | sed -n 's/^version = "\(.*\)"/\1/p' | head -1)
[ -n "$VERSION" ] || { echo "could not read the workspace version out of Cargo.toml" >&2; exit 1; }

NAME="nx-recall-${VERSION}-linux-x86_64"
OUT="$ROOT/dist"
TARGET="$ROOT/target/release"

echo "==> NX Recall $VERSION"

# ---------------------------------------------------------------------------
# 1. the daemon
# ---------------------------------------------------------------------------

if [ "$SKIP_BUILD" -eq 0 ]; then
    echo "==> cargo build --release"
    cargo build --release --bin recalld --bin nx-recall-overlay
fi

[ -x "$TARGET/recalld" ] || { echo "$TARGET/recalld is missing — build first" >&2; exit 1; }
[ -x "$TARGET/nx-recall-overlay" ] || { echo "$TARGET/nx-recall-overlay is missing — build first" >&2; exit 1; }

# sherpa-rs's build script drops these beside the binary. If they are not here
# the tarball would produce a binary that runs on this machine (cargo exports a
# library path) and on no other, which is exactly the bug this check exists for.
SO_FILES=(libsherpa-onnx-c-api.so libsherpa-onnx-cxx-api.so libonnxruntime.so)
for so in "${SO_FILES[@]}"; do
    [ -f "$TARGET/$so" ] || { echo "$TARGET/$so is missing — sherpa-rs did not stage its libraries" >&2; exit 1; }
done

# The rpath is the whole reason the layout below is what it is.
if command -v readelf >/dev/null; then
    RPATH=$(readelf -d "$TARGET/recalld" | sed -n 's/.*R\(UN\)\?PATH).*\[\(.*\)\]/\2/p' | head -1)
    case "$RPATH" in
        *'$ORIGIN'*) echo "==> rpath: $RPATH" ;;
        *) echo "!! recalld has no \$ORIGIN rpath ($RPATH) — it will not find its .so files once installed" >&2; exit 1 ;;
    esac
fi

# ---------------------------------------------------------------------------
# 2. the GUI
# ---------------------------------------------------------------------------
#
# Hand-rolled rather than electron-builder: the sibling NX Electron apps ship
# AppImages, and an AppImage is the wrong shape for a prefix install. What is
# assembled here is exactly what electron-packager would produce — the prebuilt
# Electron runtime from node_modules with the app tree dropped into
# resources/app and the launcher renamed — with no build-time download and no
# extra dependency to install.

ELECTRON_DIST="$ROOT/gui/node_modules/electron/dist"
[ -d "$ELECTRON_DIST" ] || {
    echo "$ELECTRON_DIST is missing — run: (cd gui && npm install)" >&2; exit 1; }
ELECTRON_VERSION=$(cat "$ROOT/gui/node_modules/electron/dist/version")
echo "==> electron $ELECTRON_VERSION"

# ---------------------------------------------------------------------------
# 3. stage
# ---------------------------------------------------------------------------

STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
U="$STAGE/usr"

mkdir -p "$U/bin" "$U/lib/nx-recall" "$U/share/applications" \
         "$U/share/systemd/user" "$U/share/nx-recall" \
         "$U/share/icons/hicolor/scalable/apps"

echo "==> staging the daemon"
install -m 0755 "$TARGET/recalld" "$U/lib/nx-recall/recalld"
# The headset captions overlay (docs/OVERLAY.md): OpenXR loader is dlopen'd
# from the host at run time, nothing of ours is linked in. Ships behind
# `--overlay` and says so until it has run against a live WiVRn session.
install -m 0755 "$TARGET/nx-recall-overlay" "$U/lib/nx-recall/nx-recall-overlay"
for so in "${SO_FILES[@]}"; do
    install -m 0755 "$TARGET/$so" "$U/lib/nx-recall/$so"
done
if [ "$STRIP" -eq 1 ] && command -v strip >/dev/null; then
    # --strip-unneeded only, and never on the .so files: stripping a shared
    # library's dynamic symbols would break the very linkage we just checked.
    strip --strip-unneeded "$U/lib/nx-recall/recalld" "$U/lib/nx-recall/nx-recall-overlay"
    echo "    stripped: $(du -h "$U/lib/nx-recall/recalld" | cut -f1)"
fi

echo "==> staging the GUI"
G="$U/lib/nx-recall/gui"
mkdir -p "$G"
cp -a "$ELECTRON_DIST/." "$G/"
# Electron resolves its app from resources/ next to the executable, so the
# binary can be called anything — and it should be, because its name is the
# process name and the X11/Wayland app id.
mv "$G/electron" "$G/nx-recall-gui"
# Without this, an app tree that failed to copy would silently boot Electron's
# "no app loaded" placeholder instead of failing.
rm -f "$G/resources/default_app.asar"

mkdir -p "$G/resources/app"
cp -a "$ROOT/gui/src" "$ROOT/gui/assets" "$G/resources/app/"
cp "$ROOT/gui/package.json" "$G/resources/app/package.json"
# src/main/e2e.js travels with it, exactly as it does in the AppImage build's
# `files` glob. It is a dynamic import behind `NX_RECALL_E2E=1` and it is what
# lets the *packaged* app — not a checkout — be driven under gamescope.
[ -x "$G/nx-recall-gui" ] || { echo "the Electron launcher did not survive staging" >&2; exit 1; }

echo "==> staging launchers, desktop entry, unit, icons"
install -m 0755 "$ROOT/packaging/bin/nx-recall" "$U/bin/nx-recall"
install -m 0755 "$ROOT/packaging/bin/recalld"   "$U/bin/recalld"
install -m 0755 "$ROOT/packaging/bin/nx-recall-overlay" "$U/bin/nx-recall-overlay"
install -m 0644 "$ROOT/packaging/nx-recall.desktop" "$U/share/applications/nx-recall.desktop"
install -m 0644 "$ROOT/packaging/nx-recall.service" "$U/share/systemd/user/nx-recall.service"

# hicolor, keyed on the desktop entry's `Icon=nx-recall`. The hub also picks its
# card tile off these, largest raster first.
for png in "$ROOT"/gui/assets/icons/[0-9]*x[0-9]*.png; do
    size=$(basename "$png" .png)
    mkdir -p "$U/share/icons/hicolor/$size/apps"
    install -m 0644 "$png" "$U/share/icons/hicolor/$size/apps/nx-recall.png"
done
install -m 0644 "$ROOT/gui/assets/icon.svg" \
        "$U/share/icons/hicolor/scalable/apps/nx-recall.svg"

if command -v desktop-file-validate >/dev/null; then
    desktop-file-validate "$U/share/applications/nx-recall.desktop" \
        || { echo "!! the desktop entry is not valid" >&2; exit 1; }
    echo "    desktop entry validates"
fi
if command -v systemd-analyze >/dev/null; then
    # ExecStart points into the *installed* prefix, which does not exist on the
    # build machine — that complaint is the expected one, so it is filtered and
    # anything else is surfaced.
    verify=$(systemd-analyze verify --user "$U/share/systemd/user/nx-recall.service" 2>&1 \
             | grep -v 'is not executable' | grep -v 'Unit .* not found' || true)
    [ -z "$verify" ] && echo "    unit verifies" || echo "$verify" | sed 's/^/    /'
fi

install -m 0644 "$ROOT/nx-app.json" "$U/share/nx-recall/nx-app.json"
install -m 0644 "$ROOT/README.md" "$U/share/nx-recall/README.md"

# ---------------------------------------------------------------------------
# 4. manifest accounting
# ---------------------------------------------------------------------------
#
# The hub records what it wrote; this records what we asked it to write, so the
# two can be compared without unpacking anything. It also states the one path
# that is deliberately absent from both.

MF="$U/share/nx-recall/MANIFEST.txt"
{
    echo "NX Recall $VERSION — install manifest"
    echo
    echo "Every path below is relative to the install prefix (~/.local by default),"
    echo "written by nx-hub's tarball-prefix engine and recorded by it for an exact"
    echo "uninstall. Two of them are outside this package's own subtrees and are"
    echo "called out because of it:"
    echo
    echo "  share/systemd/user/nx-recall.service   the daemon's user unit"
    echo "  share/applications/nx-recall.desktop   the app-menu entry"
    echo
    echo "NOT in this manifest, and NOT removed by an uninstall, by design"
    echo "(DESIGN §10):"
    echo
    echo "  \$XDG_DATA_HOME/nx-recall/       transcripts, voicebank, segment audio,"
    echo "                                  and the ~700 MB model set"
    echo "  \$XDG_CONFIG_HOME/nx-recall/     config.toml, including the allowlist"
    echo
    echo "Deleting recordings is a thing you do inside the app, on purpose, with a"
    echo "cascade and a VACUUM behind it — never a side effect of removing a binary."
    echo
    echo "--- files ---"
    ( cd "$U" && find . -type f -o -type l ) | sed 's|^\./||' | LC_ALL=C sort
} > "$MF"
chmod 0644 "$MF"

# README-NX.md sits at the archive root, outside `usr/`, so the engine skips it
# with a log line and it exists only for someone who downloaded the tarball and
# wants to know what is in it.
cat > "$STAGE/README-NX.md" <<EOF
# NX Recall $VERSION — Linux x86-64

Local-first, always-on conversation transcription with cross-session speaker
identity. Private repo, no cloud, no telemetry, no export.

## Install through NX Hub

Add \`nerdrx\` (or pin \`nerdrx/nx-recall\`) as a source and press Install. The hub
copies \`usr/\` into \`~/.local\`, records every file, and uninstalls exactly.

## Install by hand

    cp -r usr/* ~/.local/

## Then, once

    ~/.local/bin/recalld models fetch
    systemctl --user daemon-reload
    systemctl --user enable --now nx-recall

\`models fetch\` is the only part of this program that opens a network socket. It
pulls ~500 MB of ONNX models from the sherpa-onnx GitHub releases into
\`~/.local/share/nx-recall/models\` and verifies every file's exact byte size.
Transcription itself never touches the network.

## Capture is default-deny

Nothing is recorded until you say so:

    recalld probe                 # what is playing audio right now
    recalld allow VRChat.exe      # opt one program in
    systemctl --user restart nx-recall

## Notes

- Needs PipeWire, and a host Qt/GTK/Wayland stack for the desktop app. The two
  ONNX libraries ship in \`usr/lib/nx-recall/\`; everything else comes from your
  system on purpose.
- Chromium's setuid sandbox cannot be set up by an unprivileged install, so the
  launcher passes \`--no-sandbox\`. To get it back:
  \`sudo chown root:root ~/.local/lib/nx-recall/gui/chrome-sandbox && sudo chmod 4755 ~/.local/lib/nx-recall/gui/chrome-sandbox\`
- \`~/.local/bin\` must be on your PATH for the app-menu entry to work.
- Your database and models live in \`~/.local/share/nx-recall\` and are NOT
  removed by an uninstall. See \`usr/share/nx-recall/MANIFEST.txt\`.
EOF

# ---------------------------------------------------------------------------
# 5. tar it
# ---------------------------------------------------------------------------

mkdir -p "$OUT"
rm -f "$OUT/$NAME.tar.gz" "$OUT/$NAME.tar.gz.sha256"

echo "==> packing"
tar --owner=0 --group=0 --numeric-owner \
    --sort=name --mtime="@${SOURCE_DATE_EPOCH:-$(date +%s)}" \
    -czf "$OUT/$NAME.tar.gz" -C "$STAGE" README-NX.md usr

# Hash from inside dist/, so the sidecar holds a bare filename and
# `sha256sum -c` works for anyone who downloaded both.
( cd "$OUT" && sha256sum "$NAME.tar.gz" > "$NAME.tar.gz.sha256" )

# The manifest travels as its own release asset (PUBLISHING §5: a release asset
# named exactly nx-app.json beats the branch-root fallback, costs the hub no
# extra API call, and works for a private repo through the user's token).
install -m 0644 "$ROOT/nx-app.json" "$OUT/nx-app.json"

echo
echo "==> $OUT/$NAME.tar.gz  ($(du -h "$OUT/$NAME.tar.gz" | cut -f1), $(tar tzf "$OUT/$NAME.tar.gz" | wc -l) entries)"
echo "    $(cat "$OUT/$NAME.tar.gz.sha256")"
echo
echo "Not published. To publish:"
echo "    nx manifest check --file nx-app.json"
echo "    gh release create v$VERSION --repo nerdrx/nx-recall --title \"NX Recall $VERSION\" \\"
echo "        dist/$NAME.tar.gz dist/$NAME.tar.gz.sha256 dist/nx-app.json"
