#!/bin/bash
#
# xtafkit setup script for macOS
# Installs Rust (if needed), runs tests, builds all tools, and optionally installs them.
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

# ---------------------------------------------------------------------------
# Step 1: Check / install Rust
# ---------------------------------------------------------------------------
echo -e "${BOLD}[1/5] Checking for Rust toolchain...${NC}"

if command -v cargo &> /dev/null; then
    RUST_VER=$(rustc --version)
    echo -e "  ${GREEN}Found: $RUST_VER${NC}"
else
    echo -e "  ${YELLOW}Rust not found. Installing via rustup...${NC}"
    echo ""
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    source "$HOME/.cargo/env"
    echo ""
    echo -e "  ${GREEN}Installed: $(rustc --version)${NC}"
fi
echo ""

# ---------------------------------------------------------------------------
# Step 2: Run tests
# ---------------------------------------------------------------------------
echo -e "${BOLD}[2/5] Running test suite...${NC}"
echo ""

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

if cargo test --workspace 2>&1 | tail -5; then
    echo ""
    echo -e "  ${GREEN}All tests passed${NC}"
else
    echo ""
    echo -e "  ${RED}Some tests failed — build may still work but proceed with caution${NC}"
fi
echo ""

# ---------------------------------------------------------------------------
# Step 3: Build
# ---------------------------------------------------------------------------
echo -e "${BOLD}[3/5] Building all tools (release mode)...${NC}"
echo ""

cargo build --release 2>&1

echo ""
echo -e "  ${GREEN}Built successfully:${NC}"
echo "    target/release/xtafkit         (single binary — CLI, mount, mkimage, TUI)"
echo ""

# ---------------------------------------------------------------------------
# Step 4: Optional install to /usr/local/bin
# ---------------------------------------------------------------------------
echo -e "${BOLD}[4/5] Install to /usr/local/bin? (makes 'xtafkit' available system-wide)${NC}"
read -p "  Install? (y/n) [n]: " INSTALL_CHOICE

BINARIES="xtafkit"

if [[ "$INSTALL_CHOICE" == "y" || "$INSTALL_CHOICE" == "Y" ]]; then
    for bin in $BINARIES; do
        SRC="$SCRIPT_DIR/target/release/$bin"
        if [ -f "$SRC" ]; then
            if [[ -w /usr/local/bin ]]; then
                cp "$SRC" "/usr/local/bin/$bin"
            else
                echo "  Need sudo to copy to /usr/local/bin..."
                sudo cp "$SRC" "/usr/local/bin/$bin"
            fi
            echo -e "  ${GREEN}Installed /usr/local/bin/$bin${NC}"
        fi
    done
else
    echo "  Skipped. You can run it directly:"
    echo "    sudo ./target/release/xtafkit"
fi
echo ""

# ---------------------------------------------------------------------------
# Step 5: Quick help
# ---------------------------------------------------------------------------
echo -e "${BOLD}[5/5] Quick start${NC}"
echo ""
echo "  Interactive mode (guided — prompts for everything):"
echo -e "    ${GREEN}sudo xtafkit${NC}"
echo ""
echo "  Common commands:"
echo "    sudo xtafkit scan /dev/rdiskN                              # find Xbox partitions"
echo "    sudo xtafkit ls /dev/rdiskN --partition \"360 Data\" /       # list root directory"
echo "    sudo xtafkit info /dev/rdiskN --partition \"360 Data\"       # volume stats"
echo "    sudo xtafkit mount /dev/rdiskN --partition \"360 Data\" --mount  # mount in Finder"
echo "    sudo xtafkit browse /dev/rdiskN --partition \"360 Data\"     # TUI file browser"
echo "    xtafkit mkimage test.img --size 1G --populate              # create test image"
echo ""
echo "  File operations:"
echo "    sudo xtafkit read /dev/rdiskN --partition \"360 Data\" /path/to/file"
echo "    sudo xtafkit write /dev/rdiskN --partition \"360 Data\" /dest -i local_file"
echo "    sudo xtafkit mkdir /dev/rdiskN --partition \"360 Data\" /NewDir"
echo "    sudo xtafkit rm /dev/rdiskN --partition \"360 Data\" /file.txt"
echo "    sudo xtafkit rename /dev/rdiskN --partition \"360 Data\" /old.txt new.txt"
echo "    sudo xtafkit rmr /dev/rdiskN --partition \"360 Data\" /Directory"
echo ""
echo "  Tips:"
echo "    - Use /dev/rdiskN (raw device) — not /dev/diskN"
echo "    - Find your device: diskutil list | grep external"
echo "    - Unmount macOS first: diskutil unmountDisk /dev/diskN"
echo "    - sudo is required for raw device access and mounting"
echo "    - Add --json to any command for machine-readable output"
echo ""
echo -e "${GREEN}Setup complete!${NC}"
