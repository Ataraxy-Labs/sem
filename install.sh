#!/bin/sh
set -e

# sem installer — https://github.com/Ataraxy-Labs/sem
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/Ataraxy-Labs/sem/main/install.sh | sh

REPO="Ataraxy-Labs/sem"
BINARY="sem"
INSTALL_DIR="${SEM_INSTALL_DIR:-/usr/local/bin}"

info()  { printf '  \033[1;32m%s\033[0m %s\n' "$1" "$2"; }
warn()  { printf '  \033[1;33m%s\033[0m %s\n' "warning:" "$1"; }
error() { printf '  \033[1;31m%s\033[0m %s\n' "error:" "$1"; exit 1; }

detect_platform() {
    OS=$(uname -s | tr '[:upper:]' '[:lower:]')
    ARCH=$(uname -m)

    case "$OS" in
        linux)  OS_NAME="linux" ;;
        darwin) OS_NAME="darwin" ;;
        *)      error "Unsupported OS: $OS" ;;
    esac

    case "$ARCH" in
        x86_64|amd64)   ARCH_NAME="x86_64" ;;
        aarch64|arm64)  ARCH_NAME="arm64" ;;
        *)              error "Unsupported architecture: $ARCH" ;;
    esac

    ARTIFACT="sem-${OS_NAME}-${ARCH_NAME}"
}

get_latest_version() {
    VERSION=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
        | grep '"tag_name"' | head -1 | sed 's/.*"tag_name": *"//;s/".*//')

    if [ -z "$VERSION" ]; then
        error "Could not determine latest version"
    fi
}

download_and_install() {
    URL="https://github.com/${REPO}/releases/download/${VERSION}/${ARTIFACT}.tar.gz"

    TMPDIR=$(mktemp -d)
    trap 'rm -rf "$TMPDIR"' EXIT

    info "Downloading" "${ARTIFACT} ${VERSION}"
    curl -fsSL "$URL" -o "${TMPDIR}/${ARTIFACT}.tar.gz" \
        || error "Download failed. Check https://github.com/${REPO}/releases for available builds."

    tar xzf "${TMPDIR}/${ARTIFACT}.tar.gz" -C "$TMPDIR"

    if [ ! -f "${TMPDIR}/${BINARY}" ]; then
        error "Binary not found in archive"
    fi

    # Install — try direct, fall back to sudo
    if [ -w "$INSTALL_DIR" ]; then
        mv "${TMPDIR}/${BINARY}" "${INSTALL_DIR}/${BINARY}"
    else
        info "Elevating" "sudo required to install to ${INSTALL_DIR}"
        sudo mv "${TMPDIR}/${BINARY}" "${INSTALL_DIR}/${BINARY}"
    fi

    chmod +x "${INSTALL_DIR}/${BINARY}"
}

verify() {
    if command -v "$BINARY" >/dev/null 2>&1; then
        INSTALLED=$("$BINARY" --version 2>/dev/null || echo "unknown")
        info "Installed" "${INSTALLED} -> ${INSTALL_DIR}/${BINARY}"
    else
        warn "${INSTALL_DIR} may not be in your PATH"
        info "Installed" "${INSTALL_DIR}/${BINARY}"
    fi
}

main() {
    printf '\n  \033[1msem\033[0m installer\n\n'

    detect_platform
    get_latest_version
    download_and_install
    verify

    printf '\n  Run \033[1msem setup\033[0m to replace git diff globally.\n'
    printf '  Run \033[1msem login\033[0m to connect to sem cloud.\n\n'
}

main
