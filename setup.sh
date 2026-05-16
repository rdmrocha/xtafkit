#!/bin/bash
#
# xtafkit setup script for macOS
# Installs Rust (if needed), runs tests, builds, and optionally installs.
#

set -e

BOLD='\033[1m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
RED='\033[0;31m'
NC='\033[0m'

echo ""
echo -e "${BOLD}========================================${NC}"
echo -e "${BOLD}  xtafkit setup — Xbox FATX for macOS   ${NC}"
echo -e "${BOLD}========================================${NC}"
echo ""

# [1/5] Rust
echo -e "${BOLD}[1/5] Checking for Rust toolchain...${NC}"
if command -v cargo &> /dev/null; then
    echo -e "  ${GREEN}Found: $(rustc --version)${NC}"
else
    echo -e "  ${YELLOW}Rust not found. Installing via rustup...${NC}"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    source "$HOME/.cargo/env"
    echo -e "  ${GREEN}Installed: $(rustc --version)${NC}"
fi
echo ""

# [2/5] Tests
echo -e "${BOLD}[2/5] Running test suite...${NC}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"
if cargo test --workspace 2>&1 | tail -5; then
    echo -e "  ${GREEN}All tests passed${NC}"
else
    echo -e "  ${RED}Some tests failed — build may still work but proceed with caution${NC}"
fi
echo ""

# [3/5] Build
echo -e "${BOLD}[3/5] Building (release mode)...${NC}"
cargo build --release 2>&1
echo ""
echo -e "  ${GREEN}Built target/release/xtafkit${NC}"
echo ""

# [4/5] Optional install
echo -e "${BOLD}[4/5] Install to /usr/local/bin?${NC}"
read -p "  Install? (y/n) [n]: " INSTALL_CHOICE
if [[ "$INSTALL_CHOICE" == "y" || "$INSTALL_CHOICE" == "Y" ]]; then
    SRC="$SCRIPT_DIR/target/release/xtafkit"
    if [[ -w /usr/local/bin ]]; then
        cp "$SRC" "/usr/local/bin/xtafkit"
    else
        echo "  Need sudo to copy to /usr/local/bin..."
        sudo cp "$SRC" "/usr/local/bin/xtafkit"
    fi
    echo -e "  ${GREEN}Installed /usr/local/bin/xtafkit${NC}"
else
    echo "  Skipped. You can run it directly:"
    echo "    sudo ./target/release/xtafkit"
fi
echo ""

# [5/5] Quick help
echo -e "${BOLD}[5/5] Quick start${NC}"
echo ""
echo "  Launch the TUI (guided picker):"
echo -e "    ${GREEN}sudo xtafkit${NC}"
echo ""
echo "  Other commands:"
echo "    sudo xtafkit scan /dev/rdiskN                          # find partitions"
echo "    sudo xtafkit ls /dev/rdiskN --partition \"360 Data\" /   # list root"
echo "    sudo xtafkit browse /dev/rdiskN --partition \"360 Data\" # direct TUI"
echo "         xtafkit mkimage test.img --size 1G --populate     # test image"
echo "    sudo xtafkit resolve /dev/rdiskN /Content/.../<title>  # STFS resolve"
echo ""
echo "  Tips:"
echo "    - Use /dev/rdiskN (raw device) — not /dev/diskN"
echo "    - Find your device: diskutil list | grep external"
echo "    - Unmount macOS first: diskutil unmountDisk /dev/diskN"
echo "    - sudo is required for raw device access"
echo "    - File transfers, mkdir, rm, rename, cleanup all live in the TUI"
echo ""
echo -e "${GREEN}Setup complete!${NC}"
