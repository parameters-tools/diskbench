#!/usr/bin/env bash
set -e

# ANSI Color codes
BOLD="\033[1m"
GREEN="\033[32m"
CYAN="\033[36m"
RED="\033[31m"
RESET="\033[0m"

echo -e "${CYAN}${BOLD}⚡ diskbench installer${RESET}"
echo "-------------------------------------"

# Check if cargo is installed
if command -v cargo >/dev/null 2>&1; then
    echo -e "🚀 Rust toolchain detected. Compiling and installing ${BOLD}diskbench${RESET}..."
    cargo install --git https://github.com/parameters-tools/diskbench
    
    echo -e "\n${GREEN}✅ diskbench successfully installed!${RESET}"
    echo -e "Binary placed at: ${BOLD}$(command -v diskbench || echo "$HOME/.cargo/bin/diskbench")${RESET}"
    echo -e "Run ${BOLD}diskbench --all${RESET} to test your drive performance!"
    exit 0
fi

echo -e "${RED}❌ Rust/Cargo not found.${RESET}"
echo "Please install Rust using: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
echo "Then re-run this script."
exit 1
