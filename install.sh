#!/bin/bash
#
# fatx-rs installer for macOS
# Downloads the latest release from GitHub and installs to /usr/local/bin.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/joshuareisbord/fatx-rs/main/install.sh | bash
#
# Or run locally:
#   bash install.sh
#
# Options:
#   FATX_VERSION=v0.3.0 bash install.sh           # install a specific version
#   FATX_INSTALL_DIR=~/.local/bin bash install.sh  # custom install directory

set -e

REPO="joshuareisbord/fatx-rs"
INSTALL_DIR="${FATX_INSTALL_DIR:-/usr/local/bin}"
BINARIES="fatx"

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

# ── Check platform ──────────────────────────────────────────────────────────

echo ""
info "fatx-rs installer"
echo ""

OS="$(uname -s)"
ARCH="$(uname -m)"

if [[ "$OS" != "Darwin" ]]; then
    fail "This installer is for macOS only. Found: $OS"
fi

case "$ARCH" in
    x86_64)  ASSET_NAME="fatx-macos-x86_64" ;;
    arm64)   ASSET_NAME="fatx-macos-arm64" ;;
    *)       fail "Unsupported architecture: $ARCH" ;;
esac

ok "Detected macOS $ARCH"

# ── Check dependencies ──────────────────────────────────────────────────────

if ! command -v curl &>/dev/null; then
    fail "curl is required but not found"
fi

if ! command -v tar &>/dev/null; then
    fail "tar is required but not found"
fi

# ── Determine version ───────────────────────────────────────────────────────

if [[ -n "$FATX_VERSION" ]]; then
    VERSION="$FATX_VERSION"
    info "Installing version: $VERSION"
else
    info "Finding latest release..."
    VERSION=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
        | grep '"tag_name"' | head -1 | cut -d'"' -f4)

    if [[ -z "$VERSION" ]]; then
        # No full release yet — try latest tag
        VERSION=$(curl -fsSL "https://api.github.com/repos/$REPO/tags" \
            | grep '"name"' | head -1 | cut -d'"' -f4)
    fi

    if [[ -z "$VERSION" ]]; then
        fail "Could not determine latest version. Set FATX_VERSION manually."
    fi
    ok "Latest version: $VERSION"
fi

# ── Download ────────────────────────────────────────────────────────────────

DOWNLOAD_URL="https://github.com/$REPO/releases/download/$VERSION/$ASSET_NAME.tar.gz"
TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

info "Downloading $ASSET_NAME.tar.gz..."
echo -e "  ${DIM}$DOWNLOAD_URL${NC}"

HTTP_CODE=$(curl -fsSL -w "%{http_code}" -o "$TMP_DIR/fatx.tar.gz" "$DOWNLOAD_URL" 2>/dev/null) || true

if [[ "$HTTP_CODE" != "200" ]] || [[ ! -s "$TMP_DIR/fatx.tar.gz" ]]; then
    echo ""
    warn "No prebuilt binary found for $VERSION ($ASSET_NAME)."
    echo ""
    echo -e "  This can happen if:"
    echo -e "  - The release hasn't published binaries yet"
    echo -e "  - This is a pre-release tag"
    echo ""
    echo -e "  ${BOLD}Build from source instead:${NC}"
    echo -e "    git clone https://github.com/$REPO.git"
    echo -e "    cd fatx-rs"
    echo -e "    bash setup.sh"
    echo ""
    exit 1
fi

ok "Downloaded $(du -h "$TMP_DIR/fatx.tar.gz" | cut -f1 | xargs) archive"

# ── Extract ─────────────────────────────────────────────────────────────────

info "Extracting..."
tar xzf "$TMP_DIR/fatx.tar.gz" -C "$TMP_DIR"

# Verify binaries exist
for bin in $BINARIES; do
    if [[ ! -f "$TMP_DIR/$bin" ]]; then
        fail "Expected binary '$bin' not found in archive"
    fi
done

ok "Extracted: $BINARIES"

# ── Install ─────────────────────────────────────────────────────────────────

info "Installing to $INSTALL_DIR..."

# Create install dir if it doesn't exist
if [[ ! -d "$INSTALL_DIR" ]]; then
    if [[ -w "$(dirname "$INSTALL_DIR")" ]]; then
        mkdir -p "$INSTALL_DIR"
    else
        sudo mkdir -p "$INSTALL_DIR"
    fi
fi

# Remove old multi-binary install (pre-v1.0.0 used 3 separate binaries)
OLD_BINARIES="fatx-mount fatx-mkimage"
for old_bin in $OLD_BINARIES; do
    if [[ -f "$INSTALL_DIR/$old_bin" ]]; then
        if [[ -w "$INSTALL_DIR" ]]; then
            rm -f "$INSTALL_DIR/$old_bin"
        else
            sudo rm -f "$INSTALL_DIR/$old_bin"
        fi
        ok "Removed old binary $INSTALL_DIR/$old_bin"
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

# ── Verify ──────────────────────────────────────────────────────────────────

echo ""
if command -v fatx &>/dev/null; then
    INSTALLED_VER=$(fatx --version 2>/dev/null || echo "unknown")
    ok "fatx is ready: $INSTALLED_VER"
else
    warn "fatx was installed but isn't on your PATH."
    echo "  Add this to your shell profile:"
    echo "    export PATH=\"$INSTALL_DIR:\$PATH\""
fi

# ── Quick start ─────────────────────────────────────────────────────────────

echo ""
info "Quick start"
echo ""
echo "  Plug in your Xbox 360 drive, then:"
echo ""
echo -e "    ${GREEN}diskutil list | grep external${NC}                    # find your device"
echo -e "    ${GREEN}diskutil unmountDisk /dev/diskN${NC}                  # unmount macOS"
echo -e "    ${GREEN}sudo fatx scan /dev/rdiskN${NC}                      # find Xbox partitions"
echo -e "    ${GREEN}sudo fatx ls /dev/rdiskN --partition \"360 Data\" /${NC}  # list files"
echo -e "    ${GREEN}sudo fatx mount /dev/rdiskN --partition \"360 Data\" --mount${NC}  # Finder"
echo -e "    ${GREEN}sudo fatx${NC}                                        # interactive mode"
echo ""
echo -e "  For full help: ${DIM}fatx --help${NC}"
echo ""
