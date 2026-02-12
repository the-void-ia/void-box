#!/usr/bin/env bash
set -euo pipefail

# Build a void-box guest rootfs and initramfs.
# Includes: init, guest-agent, optional claude-code mock, optional busybox.
#
# Usage:
#   scripts/build_guest_image.sh
#   OUT_DIR=/tmp/rootfs OUT_CPIO=/tmp/root.cpio.gz scripts/build_guest_image.sh
#   BUSYBOX=/path/to/busybox scripts/build_guest_image.sh
#
# Requires: cpio, gzip. Optional: BUSYBOX for /bin/sh and basic tools.

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

OUT_DIR="${OUT_DIR:-/tmp/void-box-rootfs}"
OUT_CPIO="${OUT_CPIO:-/tmp/void-box-rootfs.cpio.gz}"

# Build guest-agent as a statically-linked musl binary so it runs inside
# a minimal initramfs without any shared libraries.
GUEST_TARGET="x86_64-unknown-linux-musl"
echo "[void-box] Building guest-agent (release, static, target=$GUEST_TARGET)..."
cargo build --release -p guest-agent --target "$GUEST_TARGET"
GUEST_AGENT_BIN="target/$GUEST_TARGET/release/guest-agent"

echo "[void-box] Preparing rootfs at: $OUT_DIR"
rm -rf "$OUT_DIR"
mkdir -p "$OUT_DIR"/{bin,sbin,proc,sys,dev,tmp,usr/local/bin}

# Guest-agent IS the init process (PID 1) — no shell wrapper needed.
# This avoids requiring /bin/sh (busybox) to be present.
echo "[void-box] Installing guest-agent as /init (PID 1)..."
cp "$GUEST_AGENT_BIN" "$OUT_DIR"/init
chmod +x "$OUT_DIR"/init
# Also install in /sbin for convenience
echo "[void-box] Installing guest-agent in /sbin..."
cp "$GUEST_AGENT_BIN" "$OUT_DIR"/sbin/guest-agent

# Claude-code mock (workflow demos and tests)
if [[ -f "$ROOT_DIR/scripts/guest/claude-code-mock.sh" ]]; then
  echo "[void-box] Installing claude-code mock at /usr/local/bin/claude-code..."
  cp "$ROOT_DIR/scripts/guest/claude-code-mock.sh" "$OUT_DIR/usr/local/bin/claude-code"
  chmod +x "$OUT_DIR/usr/local/bin/claude-code"
fi

# Optional: BusyBox for /bin/sh and basic tools (echo, cat, tr, etc.)
if [[ -n "${BUSYBOX:-}" && -f "$BUSYBOX" ]]; then
  echo "[void-box] Installing BusyBox at /bin/sh and /bin/busybox..."
  cp "$BUSYBOX" "$OUT_DIR/bin/busybox"
  chmod +x "$OUT_DIR/bin/busybox"
  ln -sf busybox "$OUT_DIR/bin/sh"
  # Optional links for common commands (so exec("echo", ...) works)
  for cmd in echo cat tr test base64; do
    ln -sf busybox "$OUT_DIR/bin/$cmd" 2>/dev/null || true
  done
else
  echo "[void-box] No BUSYBOX set; guest will have no /bin/sh (set BUSYBOX=/path/to/busybox for full shell support)."
fi

# Copy kernel modules needed for virtio-mmio and vsock
KVER=$(uname -r)
MODDIR="/lib/modules/$KVER/kernel"
DEST_MODDIR="$OUT_DIR/lib/modules"
mkdir -p "$DEST_MODDIR"

echo "[void-box] Adding kernel modules for virtio-mmio and vsock (kernel $KVER)..."
# virtio_mmio: virtio device on MMIO bus
for mod_path in \
  "$MODDIR/drivers/virtio/virtio_mmio.ko.xz" \
  "$MODDIR/net/vmw_vsock/vsock.ko.xz" \
  "$MODDIR/net/vmw_vsock/vmw_vsock_virtio_transport_common.ko.xz" \
  "$MODDIR/net/vmw_vsock/vmw_vsock_virtio_transport.ko.xz" \
  ; do
  if [[ -f "$mod_path" ]]; then
    base=$(basename "$mod_path")
    cp "$mod_path" "$DEST_MODDIR/$base"
    # Decompress .ko.xz -> .ko (finit_module needs raw ELF)
    if [[ "$base" == *.ko.xz ]]; then
      xz -d "$DEST_MODDIR/$base"
      echo "  -> ${base%.xz} (decompressed)"
    else
      echo "  -> $base"
    fi
  else
    echo "  WARNING: $mod_path not found"
  fi
done

echo "[void-box] Creating initramfs at: $OUT_CPIO"
( cd "$OUT_DIR" && find . | cpio -o -H newc | gzip ) > "$OUT_CPIO"

echo "[void-box] Done. Initramfs: $OUT_CPIO"
