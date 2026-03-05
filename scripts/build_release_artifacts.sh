#!/usr/bin/env bash
set -euo pipefail

# Build release artifacts for void-box distributions.
#
# Produces:
#   - voidbox host binary (statically linked musl)
#   - guest-agent binary
#   - initramfs (via build_guest_image.sh)
#   - kernel (via download_kernel.sh)
#   - Staged dist/ directory ready for nfpm packaging
#   - .tar.gz tarball for shell installer
#
# Usage:
#   ./scripts/build_release_artifacts.sh v0.1.0 x86_64
#   ./scripts/build_release_artifacts.sh v0.1.0 aarch64

VERSION="${1:-v0.1.0}"
ARCH="${2:-x86_64}"

echo "[void-box] Building release artifacts for $VERSION ($ARCH)"

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

# Build directory (legacy) and dist/ staging for nfpm
RELEASE_DIR="target/release-artifacts/$VERSION"
DIST_DIR="dist"
mkdir -p "$RELEASE_DIR" "$DIST_DIR"

# Target triple for static musl builds
TARGET="${ARCH}-unknown-linux-musl"

# ── 1. Build voidbox host binary ────────────────────────────────────────────

echo "[void-box] Building voidbox host binary for $TARGET..."
cargo build --release --bin voidbox --target "$TARGET"
cp "target/$TARGET/release/voidbox" "$DIST_DIR/voidbox"
cp "target/$TARGET/release/voidbox" "$RELEASE_DIR/voidbox-${ARCH}"
echo "[void-box] Host binary built: $DIST_DIR/voidbox"

# ── 2. Build guest-agent ────────────────────────────────────────────────────

echo "[void-box] Building guest-agent for $TARGET..."
cargo build --release -p guest-agent --target "$TARGET"
cp "target/$TARGET/release/guest-agent" "$RELEASE_DIR/guest-agent-${ARCH}"
echo "[void-box] Guest agent built: $RELEASE_DIR/guest-agent-${ARCH}"

# ── 3. Build initramfs ──────────────────────────────────────────────────────

echo "[void-box] Building initramfs..."
OUT_DIR="$RELEASE_DIR/rootfs-tmp" \
OUT_CPIO="$RELEASE_DIR/void-box-initramfs-${VERSION}-${ARCH}.cpio.gz" \
  ./scripts/build_guest_image.sh

rm -rf "$RELEASE_DIR/rootfs-tmp"

# Stage initramfs for nfpm
cp "$RELEASE_DIR/void-box-initramfs-${VERSION}-${ARCH}.cpio.gz" "$DIST_DIR/initramfs.cpio.gz"
echo "[void-box] Initramfs built and staged"

# ── 4. Download kernel ───────────────────────────────────────────────────────

echo "[void-box] Downloading kernel for $ARCH..."
ARCH="$ARCH" ./scripts/download_kernel.sh

# Map download_kernel.sh output to dist/ staging
case "$ARCH" in
  x86_64)  KERNEL_FILE="target/vmlinuz-amd64" ;;
  aarch64) KERNEL_FILE="target/vmlinuz-arm64" ;;
  *) echo "[void-box] ERROR: unsupported arch: $ARCH"; exit 1 ;;
esac

if [[ -f "$KERNEL_FILE" ]]; then
  cp "$KERNEL_FILE" "$DIST_DIR/vmlinuz"
  echo "[void-box] Kernel staged: $DIST_DIR/vmlinuz"
else
  echo "[void-box] WARNING: kernel not found at $KERNEL_FILE"
fi

# ── 5. Build tarball ─────────────────────────────────────────────────────────

TARBALL="$RELEASE_DIR/voidbox-${VERSION}-linux-${ARCH}.tar.gz"
echo "[void-box] Creating tarball: $TARBALL"
tar -czf "$TARBALL" -C "$DIST_DIR" voidbox vmlinuz initramfs.cpio.gz

# ── 6. Generate checksums ────────────────────────────────────────────────────

echo "[void-box] Generating checksums..."
cd "$RELEASE_DIR"
sha256sum \
  "voidbox-${ARCH}" \
  "guest-agent-${ARCH}" \
  "void-box-initramfs-${VERSION}-${ARCH}.cpio.gz" \
  "voidbox-${VERSION}-linux-${ARCH}.tar.gz" \
  > "checksums-${VERSION}-${ARCH}.txt"
cd "$ROOT_DIR"

echo ""
echo "[void-box] Release artifacts complete:"
ls -lh "$RELEASE_DIR"
echo ""
echo "dist/ staging (for nfpm):"
ls -lh "$DIST_DIR"
echo ""
echo "Files ready for distribution:"
echo "  - voidbox-${ARCH}                                   (host binary)"
echo "  - guest-agent-${ARCH}                               (guest agent)"
echo "  - void-box-initramfs-${VERSION}-${ARCH}.cpio.gz     (initramfs)"
echo "  - voidbox-${VERSION}-linux-${ARCH}.tar.gz           (tarball)"
echo "  - checksums-${VERSION}-${ARCH}.txt"
