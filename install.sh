#!/bin/sh
# sem installer — https://github.com/Ataraxy-Labs/sem
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/Ataraxy-Labs/sem/main/install.sh | sh
#
#   # pin a version
#   curl -fsSL https://raw.githubusercontent.com/Ataraxy-Labs/sem/main/install.sh | SEM_VERSION=v0.23.1 sh
#
#   # or with flags (download first if you want flags)
#   curl -fsSL -o install.sh https://raw.githubusercontent.com/Ataraxy-Labs/sem/main/install.sh
#   sh install.sh --version v0.23.1 --install-dir /opt/sem/bin
#
# Env vars (all optional):
#   SEM_VERSION      version to install, e.g. "v0.23.1" or "0.23.1" (default: latest)
#   SEM_INSTALL_DIR  install directory (default: $HOME/.local/bin)
#   NO_COLOR         disable colored output (any non-empty value)
#
# Supported platforms: macOS (arm64, x86_64), Linux (x86_64, arm64).
# Windows: see https://github.com/Ataraxy-Labs/sem#installation (Scoop) or
# download sem-windows-x86_64.zip from the releases page directly.
#
# This script never uses sudo and never writes outside INSTALL_DIR. It
# verifies every downloaded archive against the release's checksums.txt and
# refuses to install (fail closed) if that file can't be fetched or doesn't
# cover the archive.

set -eu

REPO="Ataraxy-Labs/sem"
BINARY="sem"
VERSION="${SEM_VERSION:-latest}"
INSTALL_DIR="${SEM_INSTALL_DIR:-$HOME/.local/bin}"
FORCE=0
SKIP_CHECKSUM=0

# --- output -----------------------------------------------------------

# Plain-output degradation: no tty, NO_COLOR set, or CI set -> no ANSI.
if [ -n "${NO_COLOR:-}" ] || [ -n "${CI:-}" ] || [ ! -t 1 ]; then
    USE_COLOR=0
else
    USE_COLOR=1
fi

info() {
    if [ "$USE_COLOR" -eq 1 ]; then
        printf '  \033[1;32m%s\033[0m %s\n' "$1" "$2"
    else
        printf '  %s %s\n' "$1" "$2"
    fi
}
warn() {
    if [ "$USE_COLOR" -eq 1 ]; then
        printf '  \033[1;33mwarning:\033[0m %s\n' "$1"
    else
        printf '  warning: %s\n' "$1"
    fi
}
error() {
    if [ "$USE_COLOR" -eq 1 ]; then
        printf '  \033[1;31merror:\033[0m %s\n' "$1" 1>&2
    else
        printf '  error: %s\n' "$1" 1>&2
    fi
    exit 1
}

# --- flags --------------------------------------------------------------

usage() {
    cat <<EOF
sem installer

Usage: install.sh [options]

Options:
  -v, --version VERSION    install a specific version, e.g. v0.23.1 (default: latest)
  -d, --install-dir DIR    install directory (default: \$HOME/.local/bin)
  -f, --force              reinstall even if the target version is already present
      --skip-checksum      install without SHA256 verification (NOT recommended)
      --no-color           disable colored output
  -h, --help                show this help

Environment variables SEM_VERSION and SEM_INSTALL_DIR set the same defaults
as --version and --install-dir.
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        -v|--version)
            [ $# -ge 2 ] || error "--version requires an argument"
            VERSION="$2"; shift 2 ;;
        -d|--install-dir)
            [ $# -ge 2 ] || error "--install-dir requires an argument"
            INSTALL_DIR="$2"; shift 2 ;;
        -f|--force)
            FORCE=1; shift ;;
        --skip-checksum)
            SKIP_CHECKSUM=1; shift ;;
        --no-color)
            USE_COLOR=0; shift ;;
        -h|--help)
            usage; exit 0 ;;
        *)
            error "unknown option: $1 (see --help)" ;;
    esac
done

require_cmd() {
    command -v "$1" >/dev/null 2>&1 || error "required command not found: $1"
}

# --- platform detection ---------------------------------------------------

detect_platform() {
    OS=$(uname -s | tr '[:upper:]' '[:lower:]')
    ARCH=$(uname -m)

    case "$OS" in
        linux)  OS_NAME="linux" ;;
        darwin) OS_NAME="darwin" ;;
        *)
            error "Unsupported OS: $OS. sem's install.sh supports macOS and Linux. \
For Windows, see https://github.com/${REPO}#installation (Scoop) or download \
sem-windows-x86_64.zip from https://github.com/${REPO}/releases." ;;
    esac

    case "$ARCH" in
        x86_64|amd64)   ARCH_NAME="x86_64" ;;
        aarch64|arm64)  ARCH_NAME="arm64" ;;
        *)              error "Unsupported architecture: $ARCH (supported: x86_64, arm64/aarch64)" ;;
    esac

    ARTIFACT="sem-${OS_NAME}-${ARCH_NAME}"
}

# --- version resolution ---------------------------------------------------

resolve_version() {
    if [ "$VERSION" = "latest" ]; then
        info "Resolving" "latest release"
        VERSION=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
            | grep '"tag_name"' | head -1 | sed 's/.*"tag_name": *"//;s/".*//')
        [ -n "$VERSION" ] || error "Could not determine the latest version from the GitHub API"
    else
        # Accept "0.23.1" or "v0.23.1"; release tags are "vX.Y.Z".
        case "$VERSION" in
            v*) ;;
            *) VERSION="v${VERSION}" ;;
        esac
    fi
}

# --- idempotency: skip if this exact version is already installed --------

already_installed() {
    [ "$FORCE" -eq 0 ] || return 1
    [ -x "${INSTALL_DIR}/${BINARY}" ] || return 1
    current=$("${INSTALL_DIR}/${BINARY}" --version 2>/dev/null || true)
    # `sem --version` output is expected to contain the version number.
    case "$current" in
        *"${VERSION#v}"*) return 0 ;;
        *) return 1 ;;
    esac
}

# --- checksum verification (fail closed) ----------------------------------

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        error "no SHA256 tool found (need sha256sum or shasum) — cannot verify the download. Refusing to install."
    fi
}

verify_checksum() {
    archive="$1"

    if [ "$SKIP_CHECKSUM" -eq 1 ]; then
        warn "checksum verification skipped (--skip-checksum) — installing an UNVERIFIED binary"
        return 0
    fi

    sums_url="https://github.com/${REPO}/releases/download/${VERSION}/checksums.txt"
    sums=$(curl -fsSL "$sums_url" 2>/dev/null) || {
        error "could not fetch checksums.txt for ${VERSION} (${sums_url}). \
Refusing to install an unverified binary. Re-run with --skip-checksum to \
override at your own risk, or install a different --version."
    }

    expected=$(printf '%s\n' "$sums" | grep -F "${ARTIFACT}.tar.gz" | awk '{print $1}' | head -1)
    if [ -z "$expected" ]; then
        error "checksums.txt for ${VERSION} has no entry for ${ARTIFACT}.tar.gz. \
Refusing to install an unverified binary. Re-run with --skip-checksum to \
override at your own risk."
    fi

    actual=$(sha256_of "$archive")
    if [ "$actual" != "$expected" ]; then
        error "checksum mismatch for ${ARTIFACT}.tar.gz
    expected: ${expected}
    actual:   ${actual}
This could mean a corrupted download or a tampered release. Not installing."
    fi

    info "Verified" "SHA256 checksum"
}

# --- download + install ----------------------------------------------------

download_and_install() {
    URL="https://github.com/${REPO}/releases/download/${VERSION}/${ARTIFACT}.tar.gz"

    TMPDIR=$(mktemp -d "${TMPDIR:-/tmp}/sem-install.XXXXXX")
    trap 'rm -rf "$TMPDIR"' EXIT INT TERM

    info "Downloading" "${ARTIFACT} ${VERSION}"
    curl -fsSL "$URL" -o "${TMPDIR}/${ARTIFACT}.tar.gz" \
        || error "download failed: ${URL}
Check https://github.com/${REPO}/releases for available builds and versions."

    verify_checksum "${TMPDIR}/${ARTIFACT}.tar.gz"

    tar xzf "${TMPDIR}/${ARTIFACT}.tar.gz" -C "$TMPDIR"
    [ -f "${TMPDIR}/${BINARY}" ] || error "binary '${BINARY}' not found inside ${ARTIFACT}.tar.gz"

    mkdir -p "$INSTALL_DIR" || error "could not create install directory: ${INSTALL_DIR}"
    [ -w "$INSTALL_DIR" ] || error "install directory is not writable: ${INSTALL_DIR} \
(this installer never uses sudo — pass --install-dir to pick a writable location)"

    # Atomic-ish install: write alongside, then rename into place.
    mv "${TMPDIR}/${BINARY}" "${INSTALL_DIR}/${BINARY}.new"
    chmod +x "${INSTALL_DIR}/${BINARY}.new"
    mv -f "${INSTALL_DIR}/${BINARY}.new" "${INSTALL_DIR}/${BINARY}"
}

# --- PATH guidance ----------------------------------------------------------

path_guidance() {
    case ":$PATH:" in
        *":${INSTALL_DIR}:"*) return 0 ;;
    esac
    warn "${INSTALL_DIR} is not on your PATH"
    printf '\n'
    printf '  Add it in your shell rc file, then restart your shell:\n\n'
    printf '    bash/zsh:  echo '\''export PATH="%s:$PATH"'\'' >> ~/.bashrc   # or ~/.zshrc\n' "$INSTALL_DIR"
    printf '    fish:      fish_add_path %s\n' "$INSTALL_DIR"
    printf '\n'
}

# --- main -------------------------------------------------------------------

main() {
    require_cmd curl
    require_cmd tar
    require_cmd mktemp

    if [ "$USE_COLOR" -eq 1 ]; then
        printf '\n  \033[1msem\033[0m installer\n\n'
    else
        printf '\n  sem installer\n\n'
    fi

    detect_platform
    resolve_version

    if already_installed; then
        info "Already installed" "${VERSION} -> ${INSTALL_DIR}/${BINARY} (use --force to reinstall)"
        path_guidance
        exit 0
    fi

    download_and_install

    if [ -x "${INSTALL_DIR}/${BINARY}" ]; then
        installed_version=$("${INSTALL_DIR}/${BINARY}" --version 2>/dev/null || echo "unknown")
        info "Installed" "${installed_version} -> ${INSTALL_DIR}/${BINARY}"
    else
        error "install appears to have failed: ${INSTALL_DIR}/${BINARY} is not executable"
    fi

    path_guidance

    if [ "$USE_COLOR" -eq 1 ]; then
        printf '  Run \033[1msem setup\033[0m to replace git diff globally.\n'
        printf '  Run \033[1msem login\033[0m to connect to sem cloud.\n'
    else
        printf '  Run "sem setup" to replace git diff globally.\n'
        printf '  Run "sem login" to connect to sem cloud.\n'
    fi
    printf '\n  To uninstall: curl -fsSL https://raw.githubusercontent.com/%s/main/uninstall.sh | sh\n\n' "$REPO"
}

main
