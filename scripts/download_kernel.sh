#!/usr/bin/env bash
set -euo pipefail

# Download a prebuilt Linux kernel for void-box VM guests.
# Extracts the vmlinuz image from an Ubuntu arm64/amd64 .deb package
# using ar + tar (works on both macOS and Linux — no dpkg required).
#
# Usage:
#   scripts/download_kernel.sh
#   KERNEL_VER=6.8.0-51 scripts/download_kernel.sh
#   ARCH=x86_64 scripts/download_kernel.sh
#
# The kernel is cached under target/ and reused on subsequent runs.
# Output: target/vmlinuz-arm64  or  target/vmlinuz-x86_64

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

# Pinned kernel version — override with KERNEL_VER env var
KERNEL_VER="${KERNEL_VER:-6.8.0-51}"

# Detect or override architecture
ARCH="${ARCH:-$(uname -m)}"

# Normalize arm64 → aarch64
if [[ "$ARCH" == "arm64" ]]; then
    ARCH="aarch64"
fi

case "$ARCH" in
  aarch64)
    DEB_ARCH="arm64"
    KERNEL_URL_BASE="http://ports.ubuntu.com/pool/main/l/linux"
    ;;
  x86_64)
    DEB_ARCH="amd64"
    KERNEL_URL_BASE="http://archive.ubuntu.com/ubuntu/pool/main/l/linux"
    ;;
  *)
    echo "[kernel] ERROR: unsupported architecture: $ARCH"
    exit 1
    ;;
esac

OUT_FILE="target/vmlinuz-${DEB_ARCH}"

# ---- Check cache ----
if [[ -f "$OUT_FILE" ]]; then
    echo "[kernel] Cached kernel found: $OUT_FILE ($(du -h "$OUT_FILE" | cut -f1))"
    echo "[kernel] To re-download, remove it: rm $OUT_FILE"
    echo ""
    echo "[kernel] Use with:"
    echo "  VOID_BOX_KERNEL=$OUT_FILE"
    exit 0
fi

# ---- Download .deb ----
mkdir -p target

KERNEL_DEB="linux-image-${KERNEL_VER}-generic_${KERNEL_VER}.0_${DEB_ARCH}.deb"
KERNEL_URL="${KERNEL_URL_BASE}/${KERNEL_DEB}"
DEB_PATH="target/${KERNEL_DEB}"

echo "[kernel] Downloading kernel ${KERNEL_VER} (${DEB_ARCH})..."
echo "[kernel] URL: ${KERNEL_URL}"
curl -fSL -o "$DEB_PATH" "$KERNEL_URL"

# ---- Extract vmlinuz from .deb (ar + tar, no dpkg needed) ----
echo "[kernel] Extracting vmlinuz from .deb..."
EXTRACT_DIR=$(mktemp -d "${TMPDIR:-/tmp}/void-box-kernel-extract.XXXXXX")
trap 'rm -rf "$EXTRACT_DIR" "$DEB_PATH"' EXIT

(
    cd "$EXTRACT_DIR"
    ar x "$ROOT_DIR/$DEB_PATH"
    tar xf data.tar.* ./boot/
)

cp "$EXTRACT_DIR"/boot/vmlinuz-* "$OUT_FILE"
chmod 644 "$OUT_FILE"

echo "[kernel] Kernel extracted to: $OUT_FILE ($(du -h "$OUT_FILE" | cut -f1))"
echo ""
echo "[kernel] Use with:"
echo "  VOID_BOX_KERNEL=$OUT_FILE"
