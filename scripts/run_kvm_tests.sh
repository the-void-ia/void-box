#!/usr/bin/env bash
set -euo pipefail

# Simple helper to:
#  1. Build the guest-agent binary
#  2. Create a minimal initramfs with guest-agent as /init
#  3. Set VOID_BOX_KERNEL / VOID_BOX_INITRAMFS
#  4. Run the KVM-backed integration tests
#
# This is tailored for Fedora-style systems where a bzImage lives in /boot.
#
# Usage:
#   scripts/run_kvm_tests.sh
# or:
#   KERNEL=/path/to/custom/kernel scripts/run_kvm_tests.sh
#
# Requirements:
#   - /dev/kvm accessible
#   - cpio, gzip installed

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

echo "[void-box] Using repo root: $ROOT_DIR"

KERNEL="${KERNEL:-/boot/vmlinuz-$(uname -r)}"
echo "[void-box] Using kernel: $KERNEL"

if [[ ! -f "$KERNEL" ]]; then
  echo "[void-box] ERROR: kernel image not found at: $KERNEL" >&2
  echo "[void-box] Hint: set KERNEL=/path/to/vmlinux-or-bzImage before running." >&2
  exit 1
fi

if [[ ! -e /dev/kvm ]]; then
  echo "[void-box] ERROR: /dev/kvm not found. KVM is not available on this host." >&2
  exit 1
fi

ROOTFS_DIR="${ROOTFS_DIR:-/tmp/void-box-rootfs}"
INITRAMFS="${INITRAMFS:-/tmp/void-box-rootfs.cpio.gz}"
export OUT_DIR="$ROOTFS_DIR"
export OUT_CPIO="$INITRAMFS"

echo "[void-box] Building guest image (guest-agent + init + claude-code mock)..."
"$ROOT_DIR/scripts/build_guest_image.sh"

export VOID_BOX_KERNEL="$KERNEL"
export VOID_BOX_INITRAMFS="$INITRAMFS"

echo "[void-box] VOID_BOX_KERNEL=$VOID_BOX_KERNEL"
echo "[void-box] VOID_BOX_INITRAMFS=$VOID_BOX_INITRAMFS"

echo "[void-box] Running KVM integration tests..."
cargo test --test kvm_integration -- --ignored

echo "[void-box] KVM integration tests completed."

