#!/usr/bin/env bash
set -euo pipefail

# Build a void-box guest initramfs with networking support.
#
# This script builds an initramfs that includes:
# - guest-agent as init (PID 1)
# - busybox for basic tools (sh, ip, cat, etc.)
# - CA certificates for HTTPS
# - Network configuration tools
#
# Usage:
#   scripts/build-initramfs.sh
#   OUT_DIR=/tmp/initramfs OUT_CPIO=/tmp/initramfs.cpio.gz scripts/build-initramfs.sh
#
# Environment:
#   OUT_DIR   - Directory to build rootfs in (default: /tmp/void-box-initramfs)
#   OUT_CPIO  - Output initramfs path (default: /tmp/void-box-initramfs.cpio.gz)
#   BUSYBOX   - Path to busybox binary (auto-downloaded if not set)
#   MUSL_TARGET - Build with musl for static linking (default: auto-detect)
#
# Requires: cpio, gzip, curl (for busybox download)

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

OUT_DIR="${OUT_DIR:-/tmp/void-box-initramfs}"
OUT_CPIO="${OUT_CPIO:-/tmp/void-box-initramfs.cpio.gz}"
ARTIFACTS_DIR="${ARTIFACTS_DIR:-$ROOT_DIR/artifacts}"

# Busybox URL for static x86_64 binary
BUSYBOX_URL="https://busybox.net/downloads/binaries/1.35.0-x86_64-linux-musl/busybox"

echo "=========================================="
echo "void-box initramfs builder (with SLIRP networking)"
echo "=========================================="
echo ""

# Check for musl target
MUSL_TARGET="x86_64-unknown-linux-musl"
if rustup target list --installed 2>/dev/null | grep -q "$MUSL_TARGET"; then
    echo "[1/7] Building guest-agent with musl (static binary)..."
    cargo build --release -p guest-agent --target "$MUSL_TARGET"
    GUEST_AGENT="target/$MUSL_TARGET/release/guest-agent"
else
    echo "[1/7] Building guest-agent (dynamic binary - musl target not installed)..."
    echo "  Note: For a fully static build, run: rustup target add $MUSL_TARGET"
    cargo build --release -p guest-agent
    GUEST_AGENT="target/release/guest-agent"
fi

echo "[2/7] Preparing rootfs at: $OUT_DIR"
rm -rf "$OUT_DIR"
mkdir -p "$OUT_DIR"/{bin,sbin,etc,proc,sys,dev,tmp,run,var/run,lib,usr/local/bin}
mkdir -p "$OUT_DIR"/etc/ssl/certs

# Install guest-agent as init
echo "[3/7] Installing guest-agent as /init..."
cp "$GUEST_AGENT" "$OUT_DIR/init"
chmod +x "$OUT_DIR/init"

# Also install at /sbin/guest-agent for compatibility
cp "$GUEST_AGENT" "$OUT_DIR/sbin/guest-agent"
chmod +x "$OUT_DIR/sbin/guest-agent"

# Install busybox
echo "[4/7] Installing busybox..."
if [[ -n "${BUSYBOX:-}" && -f "$BUSYBOX" ]]; then
    cp "$BUSYBOX" "$OUT_DIR/bin/busybox"
else
    echo "  Downloading busybox from $BUSYBOX_URL..."
    curl -fsSL -o "$OUT_DIR/bin/busybox" "$BUSYBOX_URL" || {
        echo "Error: Failed to download busybox. Set BUSYBOX=/path/to/busybox manually."
        exit 1
    }
fi
chmod +x "$OUT_DIR/bin/busybox"

# Create busybox symlinks for common commands
BUSYBOX_CMDS="sh ash cat echo ls cp mv rm mkdir rmdir ip ifconfig route ping wget sleep test base64 head tail tr sed awk grep insmod"
for cmd in $BUSYBOX_CMDS; do
    ln -sf busybox "$OUT_DIR/bin/$cmd"
done

# Install kernel modules for virtio devices (needed on distros where these are modules)
echo "[5/7] Installing kernel modules..."
# Detect kernel version from VOID_BOX_KERNEL path or uname -r
if [[ -n "${VOID_BOX_KERNEL:-}" ]]; then
    # Try to extract version from kernel path (e.g., /boot/vmlinuz-6.17.7-300.fc43.x86_64)
    KVER="$(basename "$VOID_BOX_KERNEL" | sed 's/^vmlinuz-//')"
    if [[ ! -d "/lib/modules/$KVER" ]]; then
        KVER="$(uname -r)"
    fi
else
    KVER="$(uname -r)"
fi

MODULES_DIR="/lib/modules/$KVER/kernel"
MODULES_DEST="$OUT_DIR/lib/modules"
mkdir -p "$MODULES_DEST"

# Modules needed (in load order):
#   virtio_mmio      - probe virtio MMIO devices
#   failover         - dependency of net_failover
#   net_failover     - dependency of virtio_net
#   virtio_net       - network device
#   vsock            - vsock core
#   vmw_vsock_virtio_transport_common - vsock virtio common
#   vmw_vsock_virtio_transport        - vsock virtio guest transport
MODULE_NAMES=(
    virtio_mmio
    failover
    net_failover
    virtio_net
    vsock
    vmw_vsock_virtio_transport_common
    vmw_vsock_virtio_transport
)

MODULES_FOUND=0
for mod_name in "${MODULE_NAMES[@]}"; do
    # Search for the module file (.ko, .ko.xz, .ko.zst, .ko.gz)
    mod_file="$(find "$MODULES_DIR" -name "${mod_name}.ko*" 2>/dev/null | head -1 || true)"
    if [[ -z "$mod_file" ]]; then
        echo "  Warning: module $mod_name not found under $MODULES_DIR"
        continue
    fi
    cp "$mod_file" "$MODULES_DEST/"
    base="$(basename "$mod_file")"
    # Decompress if needed (busybox insmod can't handle compressed modules)
    case "$base" in
        *.ko.xz)
            xz -d "$MODULES_DEST/$base"
            echo "  Installed $mod_name (decompressed from .ko.xz)"
            ;;
        *.ko.zst)
            zstd -d --rm "$MODULES_DEST/$base" 2>/dev/null
            echo "  Installed $mod_name (decompressed from .ko.zst)"
            ;;
        *.ko.gz)
            gzip -d "$MODULES_DEST/$base"
            echo "  Installed $mod_name (decompressed from .ko.gz)"
            ;;
        *.ko)
            echo "  Installed $mod_name"
            ;;
    esac
    MODULES_FOUND=$((MODULES_FOUND + 1))
done

if [[ $MODULES_FOUND -eq 0 ]]; then
    echo "  Warning: No kernel modules found. Virtio devices may not work if not built-in."
else
    echo "  Installed $MODULES_FOUND kernel modules for kernel $KVER"
fi

# Install CA certificates for HTTPS
echo "[6/7] Installing CA certificates..."
CA_BUNDLE=""
# Try common locations for CA bundle
for ca_path in \
    /etc/ssl/certs/ca-certificates.crt \
    /etc/ssl/certs/ca-bundle.crt \
    /etc/pki/tls/certs/ca-bundle.crt \
    /usr/share/ca-certificates/mozilla/ca-bundle.crt \
    /etc/ssl/cert.pem; do
    if [[ -f "$ca_path" ]]; then
        CA_BUNDLE="$ca_path"
        break
    fi
done

if [[ -n "$CA_BUNDLE" ]]; then
    cp "$CA_BUNDLE" "$OUT_DIR/etc/ssl/certs/ca-certificates.crt"
    # Create symlinks for common names
    ln -sf ca-certificates.crt "$OUT_DIR/etc/ssl/certs/ca-bundle.crt"
    echo "  Installed CA bundle from $CA_BUNDLE"
else
    echo "  Warning: No CA bundle found. HTTPS may not work."
fi

# Install claude-code mock if available
if [[ -f "$ROOT_DIR/scripts/guest/claude-code-mock.sh" ]]; then
    echo "  Installing claude-code mock..."
    cp "$ROOT_DIR/scripts/guest/claude-code-mock.sh" "$OUT_DIR/usr/local/bin/claude-code"
    chmod +x "$OUT_DIR/usr/local/bin/claude-code"
fi

# Create basic /etc files
cat > "$OUT_DIR/etc/passwd" << 'EOF'
root:x:0:0:root:/root:/bin/sh
nobody:x:65534:65534:nobody:/:/bin/false
EOF

cat > "$OUT_DIR/etc/group" << 'EOF'
root:x:0:
nobody:x:65534:
EOF

cat > "$OUT_DIR/etc/hosts" << 'EOF'
127.0.0.1   localhost
10.0.2.2    gateway
10.0.2.3    dns
EOF

# Pre-configure DNS for SLIRP networking
cat > "$OUT_DIR/etc/resolv.conf" << 'EOF'
nameserver 10.0.2.3
nameserver 8.8.8.8
EOF

# Create /etc/nsswitch.conf for name resolution
cat > "$OUT_DIR/etc/nsswitch.conf" << 'EOF'
passwd:     files
group:      files
hosts:      files dns
EOF

# Create initramfs
echo "[7/7] Creating initramfs at: $OUT_CPIO"
( cd "$OUT_DIR" && find . | cpio -o -H newc 2>/dev/null | gzip ) > "$OUT_CPIO"

# Copy to artifacts directory
mkdir -p "$ARTIFACTS_DIR"
cp "$OUT_CPIO" "$ARTIFACTS_DIR/initramfs.cpio.gz"

echo ""
echo "=========================================="
echo "Build complete!"
echo "=========================================="
echo "Initramfs: $OUT_CPIO"
echo "Artifacts: $ARTIFACTS_DIR/initramfs.cpio.gz"
echo ""
echo "Rootfs contents:"
ls -la "$OUT_DIR"
echo ""
echo "To run with void-box:"
echo "  export VOID_BOX_KERNEL=/path/to/vmlinux"
echo "  export VOID_BOX_INITRAMFS=$ARTIFACTS_DIR/initramfs.cpio.gz"
echo "  cargo run --example codebox_example"
