#!/usr/bin/env bash
# build.sh — Builds Boot OS Pro and packages it as a .deb
# Always install from dist/ — the Tauri bundler deb lacks the polkit policy and helper.
set -euo pipefail

APP_NAME="bootospro"
VERSION_MAJOR="1"
VERSION_MINOR="0"
ARCH="amd64"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DIST="$SCRIPT_DIR/dist"
BUILD_COUNTER_FILE="$SCRIPT_DIR/.build_counter"

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; NC='\033[0m'
info()    { echo -e "${GREEN}▶${NC} $*"; }
warn()    { echo -e "${YELLOW}⚠${NC}  $*"; }
error()   { echo -e "${RED}✕${NC}  $*" >&2; exit 1; }
success() { echo -e "${GREEN}✓${NC} $*"; }

# Auto-increment build counter
if [[ -f "$BUILD_COUNTER_FILE" ]]; then
    BUILD_NUM=$(cat "$BUILD_COUNTER_FILE")
    BUILD_NUM=$((BUILD_NUM + 1))
else
    BUILD_NUM=1
fi
echo "$BUILD_NUM" > "$BUILD_COUNTER_FILE"

VERSION="${VERSION_MAJOR}.${VERSION_MINOR}.${BUILD_NUM}"
DEB_NAME="${APP_NAME}_${VERSION}_${ARCH}.deb"

info "Checking build dependencies…"
check() { command -v "$1" &>/dev/null || error "Required tool not found: $1. $2"; }
check cargo    "Install Rust from https://rustup.rs/"
check node     "Install Node.js 20+ from https://nodejs.org/"
check npm      "Install npm (comes with Node.js)"
check dpkg-deb "Install dpkg: sudo apt install dpkg"

info "Rust $(rustc --version | awk '{print $2}') · Node $(node --version)"

if ! cargo tauri --version &>/dev/null 2>&1; then
    warn "tauri-cli not found. Installing…"
    cargo install tauri-cli --version "^2" --locked
fi

# Stamp version into tauri.conf.json before compile
sed -i "s/\"version\": \".*\"/\"version\": \"$VERSION\"/" "$SCRIPT_DIR/src-tauri/tauri.conf.json"

info "Installing npm dependencies…"
npm ci --prefer-offline 2>/dev/null || npm install

info "Building Boot OS Pro v${VERSION}…"
cargo tauri build 2>&1 | grep -v "^$" || true

TAURI_BINARY="$SCRIPT_DIR/src-tauri/target/release/$APP_NAME"
[[ -f "$TAURI_BINARY" ]] || error "Build failed — binary not found at $TAURI_BINARY"
success "Binary built: $(du -sh "$TAURI_BINARY" | cut -f1)"

# Always assemble from skeleton — never use Tauri's .deb directly.
info "Assembling .deb from skeleton…"

BUILD_DIR="$(mktemp -d)"
trap "rm -rf $BUILD_DIR" EXIT

cp -r "$SCRIPT_DIR/deb-skeleton/." "$BUILD_DIR/"

install -Dm755 "$TAURI_BINARY" "$BUILD_DIR/usr/bin/$APP_NAME"

ICON_SRC="$SCRIPT_DIR/src-tauri/icons/128x128.png"
if [[ -f "$ICON_SRC" ]]; then
    install -Dm644 "$ICON_SRC" "$BUILD_DIR/usr/share/icons/hicolor/128x128/apps/$APP_NAME.png"
else
    warn "No icon at $ICON_SRC — skipping."
fi

INSTALLED_KB=$(du -sk "$BUILD_DIR" | cut -f1)
sed -i "s/^Installed-Size: .*/Installed-Size: $INSTALLED_KB/" "$BUILD_DIR/DEBIAN/control"
sed -i "s/^Version: .*/Version: $VERSION/" "$BUILD_DIR/DEBIAN/control"

chmod 755 "$BUILD_DIR/DEBIAN"
chmod 644 "$BUILD_DIR/DEBIAN/control"
chmod 755 "$BUILD_DIR/DEBIAN/postinst"
chmod 755 "$BUILD_DIR/DEBIAN/prerm"
find "$BUILD_DIR/usr" -type f -exec chmod 644 {} \;
find "$BUILD_DIR/usr/bin" -type f -exec chmod 755 {} \;
chmod 755 "$BUILD_DIR/usr/lib/bootospro/bootospro-helper"
find "$BUILD_DIR/usr/share/polkit-1" -type f -exec chmod 644 {} \;

mkdir -p "$DIST"
dpkg-deb --build --root-owner-group "$BUILD_DIR" "$DIST/$DEB_NAME"

success "Built: $DIST/$DEB_NAME ($(du -sh "$DIST/$DEB_NAME" | cut -f1))"
echo ""
echo "  Install with:  sudo dpkg -i $DIST/$DEB_NAME"
echo "  Remove with:   sudo apt remove bootospro"
