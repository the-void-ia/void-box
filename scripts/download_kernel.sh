#!/usr/bin/env bash
set -euo pipefail

# Download a prebuilt Linux kernel for void-box VM guests.
# Extracts the vmlinuz image from an Ubuntu arm64/amd64 .deb package
# using ar + tar (works on both macOS and Linux — no dpkg required).
#
# Usage:
#   scripts/download_kernel.sh
#   KERNEL_VER=6.8.0-51 KERNEL_UPLOAD=52 scripts/download_kernel.sh
#   ARCH=x86_64 scripts/download_kernel.sh
#
# The kernel is cached under target/ and reused on subsequent runs.
# Output: target/vmlinuz-arm64  or  target/vmlinuz-x86_64

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

# Pinned kernel version — override with KERNEL_VER / KERNEL_UPLOAD env vars.
# Ubuntu package versions look like: 6.8.0-51.52  (base.upload)
KERNEL_VER="${KERNEL_VER:-6.8.0-51}"
KERNEL_UPLOAD="${KERNEL_UPLOAD:-52}"
KERNEL_FULL_VER="${KERNEL_VER}.${KERNEL_UPLOAD}"

# Detect or override architecture
ARCH="${ARCH:-$(uname -m)}"

# Normalize arm64 → aarch64
if [[ "$ARCH" == "arm64" ]]; then
    ARCH="aarch64"
fi

# Expected SHA256 checksums for the pinned version.
# Update these when bumping the kernel version.
SHA256_ARM64="939693785d4a09c49e4e2edeef9b97b8cf7cd04af2ed40245acd7ba4962ee143"
SHA256_AMD64="6b5ba8fd5bfb3ab4d5430db830a1600f09416fa8e4ace6b99d1bd8b7b79de43a"

case "$ARCH" in
  aarch64)
    DEB_ARCH="arm64"
    KERNEL_URL_BASE="https://ports.ubuntu.com/pool/main/l/linux"
    EXPECTED_SHA256="$SHA256_ARM64"
    ;;
  x86_64)
    DEB_ARCH="amd64"
    KERNEL_URL_BASE="https://archive.ubuntu.com/ubuntu/pool/main/l/linux"
    EXPECTED_SHA256="$SHA256_AMD64"
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

KERNEL_DEB="linux-image-unsigned-${KERNEL_VER}-generic_${KERNEL_FULL_VER}_${DEB_ARCH}.deb"
KERNEL_URL="${KERNEL_URL_BASE}/${KERNEL_DEB}"
DEB_PATH="target/${KERNEL_DEB}"

echo "[kernel] Downloading kernel ${KERNEL_FULL_VER} (${DEB_ARCH})..."
echo "[kernel] URL: ${KERNEL_URL}"
curl -fSL -o "$DEB_PATH" "$KERNEL_URL"

# ---- Verify SHA256 checksum ----
echo "[kernel] Verifying SHA256 checksum..."
if command -v sha256sum &>/dev/null; then
    ACTUAL_SHA256=$(sha256sum "$DEB_PATH" | cut -d' ' -f1)
elif command -v shasum &>/dev/null; then
    ACTUAL_SHA256=$(shasum -a 256 "$DEB_PATH" | cut -d' ' -f1)
else
    echo "[kernel] WARNING: no sha256sum or shasum found, skipping checksum verification"
    ACTUAL_SHA256="$EXPECTED_SHA256"
fi

if [[ "$ACTUAL_SHA256" != "$EXPECTED_SHA256" ]]; then
    echo "[kernel] ERROR: SHA256 checksum mismatch!"
    echo "[kernel]   expected: $EXPECTED_SHA256"
    echo "[kernel]   actual:   $ACTUAL_SHA256"
    rm -f "$DEB_PATH"
    exit 1
fi
echo "[kernel] Checksum OK"

# ---- Extract vmlinuz from .deb ----
# A .deb is an ar archive containing debian-binary, control.tar.*, and
# data.tar.*. We extract data.tar.* then pull ./boot/vmlinuz-* from it.
#
# macOS: Apple's ar (cctools) doesn't handle .deb reliably, but bsdtar
#        (which IS tar on macOS, from libarchive) reads ar format natively.
# Linux: GNU tar doesn't support ar format, so we use ar(1) instead.
echo "[kernel] Extracting vmlinuz from .deb..."
EXTRACT_DIR=$(mktemp -d "${TMPDIR:-/tmp}/void-box-kernel-extract.XXXXXX")
trap 'rm -rf "$EXTRACT_DIR" "$DEB_PATH"' EXIT

(
    cd "$EXTRACT_DIR"

    # Step 1: Extract .deb (ar archive) members
    if [[ "$(uname -s)" == "Darwin" ]]; then
        tar xf "$ROOT_DIR/$DEB_PATH"
    else
        ar x "$ROOT_DIR/$DEB_PATH"
    fi

    # Step 2: Find and extract boot/ from data.tar (may be uncompressed
    # or compressed as .zst, .xz, .gz depending on the Ubuntu version).
    DATA_TAR=$(find . -maxdepth 1 -name 'data.tar*' -print | head -1)
    DATA_TAR="${DATA_TAR#./}"
    if [[ -z "$DATA_TAR" ]]; then
        echo "[kernel] ERROR: no data.tar found after extracting .deb"
        ls -la
        exit 1
    fi
    echo "[kernel] Found: $DATA_TAR"

    case "$DATA_TAR" in
        data.tar.zst)
            zstd -dq "$DATA_TAR"
            tar xf data.tar ./boot/
            ;;
        *)
            tar xf "$DATA_TAR" ./boot/
            ;;
    esac
)

cp "$EXTRACT_DIR"/boot/vmlinuz-* "$OUT_FILE"
chmod 644 "$OUT_FILE"

echo "[kernel] Kernel extracted to: $OUT_FILE ($(du -h "$OUT_FILE" | cut -f1))"
echo ""
echo "[kernel] Use with:"
echo "  VOID_BOX_KERNEL=$OUT_FILE"
