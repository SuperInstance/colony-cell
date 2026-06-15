#!/usr/bin/env bash
# cell-sandbox.sh — bwrap sandbox for colony cells
#
# Wraps the cell binary in bubblewrap isolation:
# - Entire rootfs read-only (required for statvfs, libc, TLS)
# - Secret directories (/home, /root, /opt) replaced with empty tmpfs
# - Isolated /tmp (tmpfs, not shared with host)
# - Writable access only to the cell's own directory
# - 30-second timeout with SIGKILL
#
# Usage: cell-sandbox.sh --colony <path> --cell-id <name>
#
# Requires: bwrap (bubblewrap), timeout

set -euo pipefail

COLONY=""
CELL_ID=""
TIMEOUT_SECS=${CELL_TIMEOUT:-30}
CELL_BIN="${COLONY_CELL_BIN:-}"

# Parse args
while [[ $# -gt 0 ]]; do
    case "$1" in
        --colony) COLONY="$2"; shift 2 ;;
        --cell-id) CELL_ID="$2"; shift 2 ;;
        *) echo "Unknown: $1"; exit 1 ;;
    esac
done

if [[ -z "$COLONY" || -z "$CELL_ID" ]]; then
    echo "Usage: cell-sandbox.sh --colony <path> --cell-id <name>"
    exit 1
fi

CELL_DIR="${COLONY}/cell-${CELL_ID}"

if [[ ! -d "$CELL_DIR" ]]; then
    echo "ERROR: cell directory not found: $CELL_DIR"
    exit 1
fi

# Auto-detect cell binary
if [[ -z "$CELL_BIN" ]]; then
    CELL_BIN="${COLONY}/cell/target/release/cell"
    if [[ ! -x "$CELL_BIN" ]]; then
        echo "ERROR: cell binary not found at $CELL_BIN"
        echo "Set COLONY_CELL_BIN env var to override"
        exit 1
    fi
fi

# Check for bwrap
if ! command -v bwrap &>/dev/null; then
    echo "WARNING: bwrap not found, running unsandboxed"
    exec timeout "$TIMEOUT_SECS" "$CELL_BIN" --colony "$COLONY" --cell-id "$CELL_ID"
fi

# Build bwrap command
# Strategy: mount entire rootfs read-only, then replace sensitive paths with empty tmpfs
exec timeout "$TIMEOUT_SECS" \
    bwrap \
        --unshare-all \
        --share-net \
        --new-session \
        --die-with-parent \
        --ro-bind / / \
        --tmpfs /home \
        --tmpfs /root \
        --tmpfs /opt \
        --tmpfs /tmp \
        --proc /proc \
        --dev /dev \
        --ro-bind "$COLONY" "$COLONY" \
        --bind "$COLONY/HALL_OF_CRABS.md" "$COLONY/HALL_OF_CRABS.md" \
        --bind "$CELL_DIR" "$CELL_DIR" \
        --ro-bind "$CELL_BIN" "$CELL_BIN" \
        --chdir "$CELL_DIR" \
        --setenv COLONY "$COLONY" \
        --setenv CELL_ID "$CELL_ID" \
        "$CELL_BIN" --colony "$COLONY" --cell-id "$CELL_ID"
