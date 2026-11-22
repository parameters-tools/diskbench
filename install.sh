#!/usr/bin/env bash
set -e

BOLD="\033[1m"
GREEN="\033[32m"
CYAN="\033[36m"
RED="\033[31m"
RESET="\033[0m"

echo -e "${CYAN}${BOLD}⚡ diskbench installer${RESET}"
echo "-------------------------------------"

REPO="parameters-tools/diskbench"
ARCH="$(uname -m)"
OS="$(uname -s)"

# Determine install directory
if [ -d "$HOME/.local/bin" ] && [[ ":$PATH:" == *":$HOME/.local/bin:"* ]]; then
    INSTALL_DIR="$HOME/.local/bin"
elif [ -d "$HOME/.cargo/bin" ]; then
    INSTALL_DIR="$HOME/.cargo/bin"
elif [ -w "/usr/local/bin" ]; then
    INSTALL_DIR="/usr/local/bin"
else
    mkdir -p "$HOME/.local/bin"
    INSTALL_DIR="$HOME/.local/bin"
fi

# 1. Try downloading prebuilt standalone single binary (instant, no compiler needed)
if [ "$OS" = "Linux" ] && [ "$ARCH" = "x86_64" ]; then
    echo -e "📦 Fetching prebuilt standalone binary for ${BOLD}Linux x86_64${RESET}..."
    TMP_DIR="$(mktemp -d)"
    TAR_URL="https://github.com/${REPO}/releases/latest/download/diskbench-x86_64-unknown-linux-gnu.tar.gz"
    
    if curl -sSL -f "$TAR_URL" -o "${TMP_DIR}/diskbench.tar.gz"; then
        tar -xzf "${TMP_DIR}/diskbench.tar.gz" -C "${TMP_DIR}"
        install -m 755 "${TMP_DIR}/diskbench" "${INSTALL_DIR}/diskbench"
        rm -rf "${TMP_DIR}"
        
        echo -e "\n${GREEN}✅ diskbench successfully installed!${RESET}"
        echo -e "Binary placed at: ${BOLD}${INSTALL_DIR}/diskbench${RESET}"
        if [[ ":$PATH:" != *":${INSTALL_DIR}:"* ]]; then
            echo -e "⚠️  Note: Make sure ${BOLD}${INSTALL_DIR}${RESET} is in your PATH."
        fi
        echo -e "Run ${BOLD}diskbench --all${RESET} to test your drive performance!"
        exit 0
    fi
    rm -rf "${TMP_DIR}"
fi

# 2. Fallback: compile via cargo if toolchain is present
if command -v cargo >/dev/null 2>&1; then
    echo -e "🚀 Compiling from source via cargo..."
    cargo install --git "https://github.com/${REPO}"
    echo -e "\n${GREEN}✅ diskbench successfully installed!${RESET}"
    echo -e "Run ${BOLD}diskbench --all${RESET} to test your drive performance!"
    exit 0
fi

echo -e "${RED}❌ Prebuilt binary download failed and Cargo was not found.${RESET}"
echo "Please install Rust or download the binary manually from:"
echo "https://github.com/${REPO}/releases"
exit 1
