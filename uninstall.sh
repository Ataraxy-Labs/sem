#!/bin/sh
# sem uninstaller — https://github.com/Ataraxy-Labs/sem
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/Ataraxy-Labs/sem/main/uninstall.sh | sh
#
# Env vars:
#   SEM_INSTALL_DIR  directory sem was installed to (default: $HOME/.local/bin)
#
# Removes only the sem binary from INSTALL_DIR. Never touches sudo-owned
# paths, config, cache, or anything outside INSTALL_DIR. Safe to run more
# than once (idempotent) or when sem was never installed.

set -eu

BINARY="sem"
INSTALL_DIR="${SEM_INSTALL_DIR:-$HOME/.local/bin}"

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

while [ $# -gt 0 ]; do
    case "$1" in
        -d|--install-dir)
            [ $# -ge 2 ] || { echo "  error: --install-dir requires an argument" 1>&2; exit 1; }
            INSTALL_DIR="$2"; shift 2 ;;
        -h|--help)
            printf 'Usage: uninstall.sh [--install-dir DIR]\n'
            exit 0 ;;
        *)
            echo "  error: unknown option: $1" 1>&2; exit 1 ;;
    esac
done

TARGET="${INSTALL_DIR}/${BINARY}"

if [ -e "$TARGET" ]; then
    rm -f "$TARGET"
    info "Removed" "$TARGET"
else
    info "Nothing to remove" "$TARGET was not found (already uninstalled?)"
fi

info "Note" "shell completions, config, and cache (if any) are left in place; \
remove ~/.config/sem or your shell rc PATH line manually if desired."
