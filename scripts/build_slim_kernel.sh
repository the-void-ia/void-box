#!/usr/bin/env bash
set -euo pipefail

# Build a slim, microVM-tuned Linux kernel for void-box guests.
#
# Starts from upstream kernel.org v6.1.y stable (LTS) and applies Firecracker's
# microvm-kernel-ci config — a defconfig already pruned to the microVM minimum
# (no RTC, no SERIO, no ACPI, no PnP, virtio-only) — plus the three filesystem
# extras void-box's guest-agent requires (9p, virtiofs, overlayfs).
#
# Output: target/vmlinux-slim-${ARCH} (UNCOMPRESSED ELF).
# Unifies artifact shape with macOS/VZ (which already requires uncompressed
# vmlinux) and saves the self-decompressor stage on every cold boot.
#
# Usage:
#   scripts/build_slim_kernel.sh
#   KERNEL_VER=6.1.80 scripts/build_slim_kernel.sh
#   ARCH=x86_64 scripts/build_slim_kernel.sh

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

KERNEL_VER="${KERNEL_VER:-6.12.30}"
KERNEL_MAJMIN="${KERNEL_VER%.*}"                 # e.g. 6.12
KERNEL_SERIES="v${KERNEL_MAJMIN%%.*}.x"          # e.g. v6.x

# Firecracker config tracks their LTS series; pin to a specific tag for
# reproducibility. v1.10.1 ships microvm-kernel-ci-{x86_64,aarch64}-6.1.config
# validated against 6.1.y. We use it as the base for 6.12 too — `olddefconfig`
# fills in symbols added between 6.1 and 6.12.
FC_COMMIT="${FC_COMMIT:-v1.10.1}"
FC_CONFIG_MAJMIN="${FC_CONFIG_MAJMIN:-6.1}"

ARCH="${ARCH:-$(uname -m)}"
case "$ARCH" in
  x86_64)
    MAKE_ARCH="x86_64"
    ;;
  aarch64|arm64)
    ARCH="aarch64"
    MAKE_ARCH="arm64"
    ;;
  *)
    echo "[slim-kernel] ERROR: unsupported architecture: $ARCH"
    exit 1
    ;;
esac

FC_CONFIG_URL="https://raw.githubusercontent.com/firecracker-microvm/firecracker/${FC_COMMIT}/resources/guest_configs/microvm-kernel-ci-${ARCH}-${FC_CONFIG_MAJMIN}.config"
OUT_FILE="target/vmlinux-slim-${ARCH}"
SRC_TARBALL="target/linux-${KERNEL_VER}.tar.xz"
SRC_DIR="target/linux-${KERNEL_VER}"
FC_CONFIG_PATH="target/microvm-kernel-${FC_CONFIG_MAJMIN}-${ARCH}.config"

# ---- Cache check ----
if [[ -f "$OUT_FILE" ]]; then
    echo "[slim-kernel] Cached slim kernel: $OUT_FILE ($(du -h "$OUT_FILE" | cut -f1))"
    echo "[slim-kernel] To rebuild, remove it: rm $OUT_FILE"
    echo ""
    echo "[slim-kernel] Use with:"
    echo "  VOID_BOX_KERNEL=$PWD/$OUT_FILE"
    exit 0
fi

# ---- Build-dep check ----
MISSING=()
for tool in curl tar make gcc bc flex bison; do
    command -v "$tool" >/dev/null 2>&1 || MISSING+=("$tool")
done
if [[ ${#MISSING[@]} -ne 0 ]]; then
    echo "[slim-kernel] ERROR: missing build tools: ${MISSING[*]}"
    echo "[slim-kernel] Fedora: sudo dnf install -y flex bison bc openssl-devel elfutils-libelf-devel perl-ExtUtils-MakeMaker"
    echo "[slim-kernel] Debian: sudo apt install -y flex bison bc libssl-dev libelf-dev"
    exit 1
fi
# libssl header check (needed for module signing even when disabled)
if [[ ! -f /usr/include/openssl/opensslv.h ]] && [[ ! -f /usr/include/x86_64-linux-gnu/openssl/opensslv.h ]]; then
    echo "[slim-kernel] ERROR: missing OpenSSL headers"
    echo "[slim-kernel] Fedora: sudo dnf install -y openssl-devel"
    echo "[slim-kernel] Debian: sudo apt install -y libssl-dev"
    exit 1
fi

mkdir -p target

# ---- Download source ----
if [[ ! -d "$SRC_DIR" ]]; then
    if [[ ! -f "$SRC_TARBALL" ]]; then
        KERNEL_URL="https://cdn.kernel.org/pub/linux/kernel/${KERNEL_SERIES}/linux-${KERNEL_VER}.tar.xz"
        echo "[slim-kernel] Downloading ${KERNEL_VER} from kernel.org..."
        curl -fSL -o "$SRC_TARBALL" "$KERNEL_URL"
    fi
    echo "[slim-kernel] Extracting source..."
    tar -xf "$SRC_TARBALL" -C target
fi

# ---- Download Firecracker config ----
if [[ ! -f "$FC_CONFIG_PATH" ]]; then
    echo "[slim-kernel] Downloading Firecracker config (${FC_COMMIT}) for kernel ${KERNEL_MAJMIN}..."
    echo "[slim-kernel] URL: ${FC_CONFIG_URL}"
    curl -fSL -o "$FC_CONFIG_PATH" "$FC_CONFIG_URL"
fi

# ---- Apply config + void-box additions ----
cp "$FC_CONFIG_PATH" "$SRC_DIR/.config"

# void-box guest-agent needs these filesystems; Firecracker's microvm config
# doesn't ship them. Built-in (=y) not modular (=m) so no initcall cost at boot.
echo "[slim-kernel] Applying void-box additions to config..."
(
    cd "$SRC_DIR"
    # void-box guest-agent needs these filesystems; Firecracker's microvm
    # config doesn't ship them. Built-in (=y), not modular, so no initcall
    # cost at boot.
    scripts/config --enable CONFIG_NET_9P
    scripts/config --enable CONFIG_NET_9P_VIRTIO
    scripts/config --enable CONFIG_9P_FS
    scripts/config --enable CONFIG_9P_FSCACHE
    scripts/config --enable CONFIG_FUSE_FS
    scripts/config --enable CONFIG_VIRTIO_FS
    scripts/config --enable CONFIG_OVERLAY_FS
    # Required for our VoidBoxConfig::kernel_cmdline to register virtio-mmio
    # devices (vsock, net, 9p, OCI rootfs) via `virtio_mmio.device=...` args.
    # Firecracker's config leaves this off because they use virtio-pci.
    scripts/config --enable CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES
    # Drop module signing — we only build built-in modules and the
    # openssl/engine.h dependency breaks on OpenSSL 3 (Fedora 40+).
    scripts/config --disable CONFIG_MODULE_SIG
    scripts/config --disable CONFIG_MODULE_SIG_ALL
    scripts/config --disable CONFIG_SYSTEM_TRUSTED_KEYRING
    scripts/config --disable CONFIG_SYSTEM_REVOCATION_LIST
    scripts/config --set-str CONFIG_SYSTEM_TRUSTED_KEYS ""
    scripts/config --set-str CONFIG_SYSTEM_REVOCATION_KEYS ""
    # olddefconfig fills in any new symbols introduced since the Firecracker
    # config was written (older config vs newer kernel tree).
    make ARCH="$MAKE_ARCH" olddefconfig >/dev/null
)

# ---- Build ----
JOBS="$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 4)"
echo "[slim-kernel] Building vmlinux with -j${JOBS} (this takes ~5-10 min)..."
(
    cd "$SRC_DIR"
    make ARCH="$MAKE_ARCH" -j"$JOBS" vmlinux
)

cp "$SRC_DIR/vmlinux" "$OUT_FILE"
chmod 644 "$OUT_FILE"

echo ""
echo "[slim-kernel] Built: $OUT_FILE ($(du -h "$OUT_FILE" | cut -f1))"
echo "[slim-kernel] Use with:"
echo "  VOID_BOX_KERNEL=$PWD/$OUT_FILE"
