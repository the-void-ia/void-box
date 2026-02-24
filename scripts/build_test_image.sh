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
# Supports both x86_64 and aarch64. Auto-detects host architecture.
# On macOS (Apple Silicon), cross-compiles for aarch64-linux-musl using
# a musl cross-compiler (brew install filosottile/musl-cross/musl-cross).
# Handles kernel modules in any compression format (.ko, .ko.xz, .ko.zst).
# Requires: cpio, gzip, musl target.

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

OUT_DIR="${OUT_DIR:-/tmp/void-box-test-rootfs}"
OUT_CPIO="${OUT_CPIO:-/tmp/void-box-test-rootfs.cpio.gz}"

# Detect host OS and architecture
HOST_OS=$(uname -s)
ARCH=$(uname -m)

# Map macOS arm64 → aarch64
if [[ "$ARCH" == "arm64" ]]; then
    ARCH="aarch64"
fi

case "$ARCH" in
  x86_64)  GUEST_TARGET="x86_64-unknown-linux-musl" ;;
  aarch64) GUEST_TARGET="aarch64-unknown-linux-musl" ;;
  *)       echo "[test-image] ERROR: unsupported architecture: $ARCH"; exit 1 ;;
esac

# On macOS, set up the musl cross-compiler for building Linux guest binaries
if [[ "$HOST_OS" == "Darwin" ]]; then
    echo "[test-image] macOS detected — using musl cross-compilation for $GUEST_TARGET"
    CROSS_GCC="${ARCH}-linux-musl-gcc"
    CC_VAR_NAME="CC_${GUEST_TARGET//-/_}"
    export "$CC_VAR_NAME=$CROSS_GCC"
    export "CARGO_TARGET_$(echo "$GUEST_TARGET" | tr '[:lower:]-' '[:upper:]_')_LINKER=$CROSS_GCC"
fi

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

# ---- Kernel modules (Linux only) ----
# On macOS the VZ backend uses PCI auto-discovery — no virtio-mmio kernel
# modules are needed in the initramfs.
if [[ "$HOST_OS" == "Linux" ]]; then
    KVER=$(uname -r)
    MODDIR="/lib/modules/$KVER/kernel"
    DEST_MODDIR="$OUT_DIR/lib/modules"
    mkdir -p "$DEST_MODDIR"

    # copy_module: find a kernel module by name (any compression) and decompress it
    copy_module() {
        local mod_name="$1"
        local found=""

        # Search for .ko, .ko.xz, .ko.zst (covers Ubuntu, Fedora, etc.)
        for ext in ko ko.xz ko.zst; do
            found=$(find "$MODDIR" -name "${mod_name}.${ext}" 2>/dev/null | head -1)
            [[ -n "$found" ]] && break
        done

        if [[ -z "$found" ]]; then
            echo "  WARNING: ${mod_name} not found under $MODDIR"
            return
        fi

        cp "$found" "$DEST_MODDIR/"
        local base
        base=$(basename "$found")

        case "$base" in
            *.ko.xz)
                xz -d "$DEST_MODDIR/$base"
                echo "  -> ${base%.xz}"
                ;;
            *.ko.zst)
                zstd -d --rm "$DEST_MODDIR/$base" 2>/dev/null
                echo "  -> ${base%.zst}"
                ;;
            *.ko)
                echo "  -> $base"
                ;;
        esac
    }

    echo "[test-image] Adding kernel modules (kernel $KVER, arch $ARCH)..."
    for mod_name in \
        virtio_mmio \
        vsock \
        vmw_vsock_virtio_transport_common \
        vmw_vsock_virtio_transport \
        failover \
        net_failover \
        virtio_net \
        ; do
        copy_module "$mod_name"
    done
else
    echo "[test-image] Skipping kernel modules (not needed for VZ backend on macOS)"
fi

# ---- Create initramfs ----
echo "[test-image] Creating test initramfs at: $OUT_CPIO"
( cd "$OUT_DIR" && find . | cpio -o -H newc | gzip ) > "$OUT_CPIO"

echo "[test-image] Done. Test initramfs: $OUT_CPIO"
echo "[test-image] Use with:"
echo "  VOID_BOX_KERNEL=/boot/vmlinuz-\$(uname -r) \\"
echo "  VOID_BOX_INITRAMFS=$OUT_CPIO \\"
echo "  cargo test -- --ignored"
