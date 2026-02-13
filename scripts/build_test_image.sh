#!/usr/bin/env bash
set -euo pipefail

# Build a test initramfs for void-box E2E tests.
# Includes: guest-agent as /init, claudio (mock claude-code) as /usr/local/bin/claude-code.
#
# Usage:
#   scripts/build_test_image.sh
#   OUT_CPIO=/tmp/test-root.cpio.gz scripts/build_test_image.sh
#
# The resulting initramfs can be used with:
#   VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
#   VOID_BOX_INITRAMFS=/tmp/void-box-test-rootfs.cpio.gz \
#   cargo test -- --ignored
#
# Requires: cpio, gzip, musl target (rustup target add x86_64-unknown-linux-musl).

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

OUT_DIR="${OUT_DIR:-/tmp/void-box-test-rootfs}"
OUT_CPIO="${OUT_CPIO:-/tmp/void-box-test-rootfs.cpio.gz}"
GUEST_TARGET="x86_64-unknown-linux-musl"

# ---- Build guest-agent (static musl) ----
echo "[test-image] Building guest-agent (release, static, target=$GUEST_TARGET)..."
cargo build --release -p guest-agent --target "$GUEST_TARGET"
GUEST_AGENT_BIN="target/$GUEST_TARGET/release/guest-agent"

# ---- Build claudio (mock claude-code, static musl) ----
echo "[test-image] Building claudio (release, static, target=$GUEST_TARGET)..."
cargo build --release -p claudio --target "$GUEST_TARGET"
CLAUDIO_BIN="target/$GUEST_TARGET/release/claudio"

# ---- Assemble rootfs ----
echo "[test-image] Preparing rootfs at: $OUT_DIR"
rm -rf "$OUT_DIR"
mkdir -p "$OUT_DIR"/{bin,sbin,proc,sys,dev,tmp,usr/local/bin}

# Guest-agent IS the init process (PID 1)
echo "[test-image] Installing guest-agent as /init..."
cp "$GUEST_AGENT_BIN" "$OUT_DIR/init"
chmod +x "$OUT_DIR/init"
cp "$GUEST_AGENT_BIN" "$OUT_DIR/sbin/guest-agent"

# Claudio (mock claude-code) replaces real claude-code for deterministic testing
echo "[test-image] Installing claudio as /usr/local/bin/claude-code..."
cp "$CLAUDIO_BIN" "$OUT_DIR/usr/local/bin/claude-code"
chmod +x "$OUT_DIR/usr/local/bin/claude-code"

# ---- Optional: BusyBox for /bin/sh ----
if [[ -n "${BUSYBOX:-}" && -f "$BUSYBOX" ]]; then
    echo "[test-image] Installing BusyBox at /bin/sh..."
    cp "$BUSYBOX" "$OUT_DIR/bin/busybox"
    chmod +x "$OUT_DIR/bin/busybox"
    ln -sf busybox "$OUT_DIR/bin/sh"
    for cmd in echo cat env tr test ls mkdir rm cp mv pwd id hostname; do
        ln -sf busybox "$OUT_DIR/bin/$cmd" 2>/dev/null || true
    done
fi

# ---- Kernel modules for virtio-mmio and vsock ----
KVER=$(uname -r)
MODDIR="/lib/modules/$KVER/kernel"
DEST_MODDIR="$OUT_DIR/lib/modules"
mkdir -p "$DEST_MODDIR"

echo "[test-image] Adding kernel modules (kernel $KVER)..."
for mod_path in \
    "$MODDIR/drivers/virtio/virtio_mmio.ko.xz" \
    "$MODDIR/net/vmw_vsock/vsock.ko.xz" \
    "$MODDIR/net/vmw_vsock/vmw_vsock_virtio_transport_common.ko.xz" \
    "$MODDIR/net/vmw_vsock/vmw_vsock_virtio_transport.ko.xz" \
    "$MODDIR/net/core/failover.ko.xz" \
    "$MODDIR/drivers/net/net_failover.ko.xz" \
    "$MODDIR/drivers/net/virtio_net.ko.xz" \
    ; do
    if [[ -f "$mod_path" ]]; then
        base=$(basename "$mod_path")
        cp "$mod_path" "$DEST_MODDIR/$base"
        if [[ "$base" == *.ko.xz ]]; then
            xz -d "$DEST_MODDIR/$base"
            echo "  -> ${base%.xz}"
        else
            echo "  -> $base"
        fi
    else
        echo "  WARNING: $mod_path not found"
    fi
done

# ---- Create initramfs ----
echo "[test-image] Creating test initramfs at: $OUT_CPIO"
( cd "$OUT_DIR" && find . | cpio -o -H newc | gzip ) > "$OUT_CPIO"

echo "[test-image] Done. Test initramfs: $OUT_CPIO"
echo "[test-image] Use with:"
echo "  VOID_BOX_KERNEL=/boot/vmlinuz-\$(uname -r) \\"
echo "  VOID_BOX_INITRAMFS=$OUT_CPIO \\"
echo "  cargo test -- --ignored"
