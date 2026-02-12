#!/usr/bin/env bash
set -euo pipefail

# Build release artifacts for void-box distributions
# Usage:
#   ./scripts/build_release_artifacts.sh v0.1.0 x86_64
#   ./scripts/build_release_artifacts.sh v0.1.0 aarch64

VERSION="${1:-v0.1.0}"
ARCH="${2:-x86_64}"

echo "[void-box] Building release artifacts for $VERSION ($ARCH)"

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

# Build directory
RELEASE_DIR="target/release-artifacts/$VERSION"
mkdir -p "$RELEASE_DIR"

# Target triple for static musl builds
TARGET="${ARCH}-unknown-linux-musl"

# 1. Build guest-agent (statically linked musl)
echo "[void-box] Building guest-agent for $TARGET..."
cargo build --release -p guest-agent --target "$TARGET"

# Copy guest-agent binary
cp "target/$TARGET/release/guest-agent" \
   "$RELEASE_DIR/guest-agent-${ARCH}"

echo "[void-box] Guest agent built: $RELEASE_DIR/guest-agent-${ARCH}"

# 2. Build initramfs with guest-agent
echo "[void-box] Building initramfs..."
OUT_DIR="$RELEASE_DIR/rootfs-tmp" \
OUT_CPIO="$RELEASE_DIR/void-box-initramfs-${VERSION}-${ARCH}.cpio.gz" \
  ./scripts/build_guest_image.sh

# Clean up temporary rootfs directory
rm -rf "$RELEASE_DIR/rootfs-tmp"

echo "[void-box] Initramfs built: $RELEASE_DIR/void-box-initramfs-${VERSION}-${ARCH}.cpio.gz"

# 3. Generate checksums
echo "[void-box] Generating checksums..."
cd "$RELEASE_DIR"
sha256sum guest-agent-${ARCH} void-box-initramfs-${VERSION}-${ARCH}.cpio.gz \
  > "checksums-${VERSION}-${ARCH}.txt"
cd "$ROOT_DIR"

echo ""
echo "[void-box] ✓ Release artifacts complete:"
ls -lh "$RELEASE_DIR"
echo ""
echo "Files ready for distribution:"
echo "  - guest-agent-${ARCH}"
echo "  - void-box-initramfs-${VERSION}-${ARCH}.cpio.gz"
echo "  - checksums-${VERSION}-${ARCH}.txt"
