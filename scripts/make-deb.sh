#!/usr/bin/env bash
#
# Build a .deb for PokeTokenBar (GNOME port).
#
# Usage:
#   scripts/make-deb.sh            # cargo build + package
#   PTB_SKIP_BUILD=1 scripts/make-deb.sh   # package an existing target/release
#
# Outputs: dist/poketoken_<version>_<arch>.deb
#
# Prereqs for a from-scratch build:
#   sudo apt install libgtk-4-dev libadwaita-1-dev curl
#   (dpkg-deb ships with dpkg-dev on any Debian/Ubuntu system)

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

APP_ID="io.github.poketoken.app"
PKG_NAME="poketoken"
MAINTAINER="${PTB_MAINTAINER:-PokeTokenBar contributors <noreply@local>}"
ICON_URL="${PTB_ICON_URL:-https://raw.githubusercontent.com/PokeAPI/sprites/master/sprites/pokemon/other/official-artwork/25.png}"

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
ARCH="$(dpkg --print-architecture)"
DIST_DIR="$(mktemp -d /tmp/poketoken-deb.XXXXXX)"
trap 'rm -rf "$DIST_DIR"' EXIT

echo ">> PokeTokenBar $VERSION ($ARCH)"

if [[ "${PTB_SKIP_BUILD:-0}" != "1" ]]; then
    echo ">> cargo build --release (gui)"
    cargo build --release -p poketoken-app --features gui
    cargo build --release -p poketoken-cli
fi

for bin in poketoken-app poke-token-bar; do
    if [[ ! -x "target/release/$bin" ]]; then
        echo "error: target/release/$bin not found (run without PTB_SKIP_BUILD=1)" >&2
        exit 1
    fi
done

mkdir -p \
    "$DIST_DIR/DEBIAN" \
    "$DIST_DIR/usr/bin" \
    "$DIST_DIR/usr/share/applications" \
    "$DIST_DIR/usr/share/icons/hicolor/128x128/apps"

install -m 0755 target/release/poketoken-app "$DIST_DIR/usr/bin/poketoken"
install -m 0755 target/release/poke-token-bar "$DIST_DIR/usr/bin/poke-token-bar"

# Icon: download a sprite (official artwork), fall back gracefully.
ICON_OK=0
if curl -fsSL --max-time 30 -o "$DIST_DIR/usr/share/icons/hicolor/128x128/apps/poketoken.png" "$ICON_URL" \
   && [[ -s "$DIST_DIR/usr/share/icons/hicolor/128x128/apps/poketoken.png" ]]; then
    ICON_OK=1
else
    echo "warning: icon download failed, packaging without an icon" >&2
    rm -f "$DIST_DIR/usr/share/icons/hicolor/128x128/apps/poketoken.png"
fi

cat > "$DIST_DIR/DEBIAN/control" <<EOF
Package: $PKG_NAME
Version: $VERSION
Section: utils
Priority: optional
Maintainer: $MAINTAINER
Architecture: $ARCH
Depends: libgtk-4-1, libadwaita-1-0, libgdk-pixbuf-2.0-0, libglib2.0-0
Description: Token-usage Pokemon companion for the GNOME system tray
 Reads local AI-coding CLI logs (Claude Code, Codex, Gemini, OpenCode, ...)
 and grows a Pokemon companion in the tray, with a GTK4/libadwaita window
 for usage, shop, bag and collection.
EOF

# Named after the GTK app id (and Id= for GNOME 42+) so gnome-shell binds the window to this
# .desktop and shows its icon in the titlebar instead of the default gear.
DESKTOP_FILE="io.github.poketoken.app"
{
    echo "[Desktop Entry]"
    echo "Type=Application"
    echo "Version=$VERSION"
    echo "Id=$DESKTOP_FILE"
    echo "Name=PokeTokenBar"
    echo "Comment=AI token usage as a Pokemon companion"
    echo "Exec=poketoken"
    echo "Terminal=false"
    echo "Categories=Utility;"
    if [[ "$ICON_OK" == "1" ]]; then
        echo "Icon=poketoken"
    fi
} > "$DIST_DIR/usr/share/applications/${DESKTOP_FILE}.desktop"

# Advisory: report unresolved runtime libs (does not fail the build).
if command -v ldd >/dev/null; then
    missing="$(ldd "$DIST_DIR/usr/bin/poketoken" 2>/dev/null | awk '/=> not found/ {print $1}' || true)"
    if [[ -n "$missing" ]]; then
        echo "warning: unresolved shared libraries in the binary:" >&2
        echo "$missing" | sed 's/^/  /' >&2
        echo "  add them to Depends: in DEBIAN/control" >&2
    fi
fi

OUT_DIR="$REPO_ROOT/dist"
mkdir -p "$OUT_DIR"
OUT_DEB="$OUT_DIR/${PKG_NAME}_${VERSION}_${ARCH}.deb"

rm -f "$OUT_DEB"
dpkg-deb --build --root-owner-group "$DIST_DIR" "$OUT_DEB" >/dev/null
echo ">> built $OUT_DEB"
dpkg-deb --info "$OUT_DEB" | sed 's/^/   /'
