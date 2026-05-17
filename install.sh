#!/bin/bash
#
# xtafkit installer for macOS
# Downloads the latest release from GitHub and installs to /usr/local/bin.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/rdmrocha/xtafkit/main/install.sh | bash
#
# Or run locally:
#   bash install.sh
#
# Options:
#   XTAFKIT_VERSION=v1.3.0 bash install.sh           # install a specific version
#   XTAFKIT_INSTALL_DIR=~/.local/bin bash install.sh  # custom install directory

set -e

REPO="rdmrocha/xtafkit"
INSTALL_DIR="${XTAFKIT_INSTALL_DIR:-/usr/local/bin}"
BINARIES="xtafkit"

BOLD='\033[1m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
RED='\033[0;31m'
DIM='\033[2m'
NC='\033[0m'

info()  { echo -e "${BOLD}$1${NC}"; }
ok()    { echo -e "  ${GREEN}✓ $1${NC}"; }
warn()  { echo -e "  ${YELLOW}⚠ $1${NC}"; }
fail()  { echo -e "  ${RED}✗ $1${NC}"; exit 1; }

echo ""
info "xtafkit installer"
echo ""

OS="$(uname -s)"
ARCH="$(uname -m)"

if [[ "$OS" != "Darwin" ]]; then
    fail "This installer is for macOS only. Found: $OS"
fi

case "$ARCH" in
    x86_64)  ASSET_NAME="xtafkit-macos-x86_64" ;;
    arm64)   ASSET_NAME="xtafkit-macos-arm64" ;;
    *)       fail "Unsupported architecture: $ARCH" ;;
esac

ok "Detected macOS $ARCH"

command -v curl &>/dev/null || fail "curl is required but not found"
command -v tar  &>/dev/null || fail "tar is required but not found"

if [[ -n "$XTAFKIT_VERSION" ]]; then
    VERSION="$XTAFKIT_VERSION"
    info "Installing version: $VERSION"
else
    info "Finding latest release..."
    VERSION=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
        | grep '"tag_name"' | head -1 | cut -d'"' -f4)
    if [[ -z "$VERSION" ]]; then
        VERSION=$(curl -fsSL "https://api.github.com/repos/$REPO/tags" \
            | grep '"name"' | head -1 | cut -d'"' -f4)
    fi
    [[ -z "$VERSION" ]] && fail "Could not determine latest version. Set XTAFKIT_VERSION manually."
    ok "Latest version: $VERSION"
fi

DOWNLOAD_URL="https://github.com/$REPO/releases/download/$VERSION/$ASSET_NAME.tar.gz"
TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

info "Downloading $ASSET_NAME.tar.gz..."
echo -e "  ${DIM}$DOWNLOAD_URL${NC}"

HTTP_CODE=$(curl -fsSL -w "%{http_code}" -o "$TMP_DIR/xtafkit.tar.gz" "$DOWNLOAD_URL" 2>/dev/null) || true

if [[ "$HTTP_CODE" != "200" ]] || [[ ! -s "$TMP_DIR/xtafkit.tar.gz" ]]; then
    echo ""
    warn "No prebuilt binary found for $VERSION ($ASSET_NAME)."
    echo ""
    echo -e "  ${BOLD}Build from source instead:${NC}"
    echo -e "    git clone https://github.com/$REPO.git"
    echo -e "    cd xtafkit"
    echo -e "    bash setup.sh"
    echo ""
    exit 1
fi

ok "Downloaded $(du -h "$TMP_DIR/xtafkit.tar.gz" | cut -f1 | xargs) archive"

info "Extracting..."
tar xzf "$TMP_DIR/xtafkit.tar.gz" -C "$TMP_DIR"

for bin in $BINARIES; do
    [[ -f "$TMP_DIR/$bin" ]] || fail "Expected binary '$bin' not found in archive"
done
ok "Extracted: $BINARIES"

info "Installing to $INSTALL_DIR..."

if [[ ! -d "$INSTALL_DIR" ]]; then
    if [[ -w "$(dirname "$INSTALL_DIR")" ]]; then mkdir -p "$INSTALL_DIR"
    else sudo mkdir -p "$INSTALL_DIR"; fi
fi

# Sweep legacy binaries from the pre-rename / pre-cleanup history.
OLD_BINARIES="fatx fatx-mount fatx-mkimage"
for old_bin in $OLD_BINARIES; do
    if [[ -f "$INSTALL_DIR/$old_bin" ]]; then
        if [[ -w "$INSTALL_DIR" ]]; then rm -f "$INSTALL_DIR/$old_bin"
        else sudo rm -f "$INSTALL_DIR/$old_bin"; fi
        ok "Removed legacy binary $INSTALL_DIR/$old_bin"
    fi
done

for bin in $BINARIES; do
    if [[ -w "$INSTALL_DIR" ]]; then
        cp "$TMP_DIR/$bin" "$INSTALL_DIR/$bin"
        chmod +x "$INSTALL_DIR/$bin"
    else
        sudo cp "$TMP_DIR/$bin" "$INSTALL_DIR/$bin"
        sudo chmod +x "$INSTALL_DIR/$bin"
    fi
    ok "Installed $INSTALL_DIR/$bin"
done

echo ""
if command -v xtafkit &>/dev/null; then
    INSTALLED_VER=$(xtafkit --version 2>/dev/null || echo "unknown")
    ok "xtafkit is ready: $INSTALLED_VER"
else
    warn "xtafkit was installed but isn't on your PATH."
    echo "  Add this to your shell profile:"
    echo "    export PATH=\"$INSTALL_DIR:\$PATH\""
fi

echo ""
info "Quick start"
echo ""
echo "  Plug in your Xbox 360 drive, then:"
echo ""
echo -e "    ${GREEN}diskutil list | grep external${NC}                    # find your device"
echo -e "    ${GREEN}diskutil unmountDisk /dev/diskN${NC}                  # unmount macOS"
echo -e "    ${GREEN}sudo xtafkit scan /dev/rdiskN${NC}                    # find Xbox partitions"
echo -e "    ${GREEN}sudo xtafkit${NC}                                     # launch TUI"
echo ""
echo -e "  For full help: ${DIM}xtafkit --help${NC}"
echo ""
