#!/usr/bin/env bash
set -euo pipefail

# Build a slim, microVM-tuned Linux kernel for void-box guests.
#
# Default build: upstream Linux kernel.org v6.12.30 + Firecracker's
# microvm-kernel-ci config (their 6.1 LTS flavor by default) as the base
# — a defconfig already pruned to the microVM minimum (no RTC, no SERIO,
# no ACPI, no PnP, virtio-only) — plus the three filesystem extras
# void-box's guest-agent requires (9p, virtiofs, overlayfs) and
# VIRTIO_MMIO_CMDLINE_DEVICES so virtio devices declared on the kernel
# cmdline are registered.
#
# When the kernel and Firecracker config are on different series (e.g.
# 6.12 kernel with 6.1 FC config), `make olddefconfig` fills in any
# symbols the newer tree added. Override `KERNEL_VER` to pick a
# different kernel, and `FC_CONFIG_MAJMIN` to pick a matching FC config
# series if you want to stay in lockstep.
#
# Output:
#   x86_64   → target/vmlinux-slim-x86_64  (uncompressed ELF)
#   aarch64  → target/vmlinux-slim-aarch64 (uncompressed Linux Image)
#
# The x86_64 output is a raw ELF that VoidBox's linux-loader-based x86
# boot path parses via `e_entry`. The aarch64 output is the Linux ARM64
# `Image` binary (raw kernel with its 64-byte header at offset 0) that
# VoidBox's aarch64 boot path loads at KERNEL_LOAD_ADDR and jumps to.
# Both are uncompressed, unifying artifact shape with macOS/VZ (which
# already requires an uncompressed kernel) and saving the
# self-decompressor stage on every cold boot.
#
# Usage:
#   scripts/build_slim_kernel.sh
#   KERNEL_VER=6.12.30 scripts/build_slim_kernel.sh
#   KERNEL_VER=6.1.80 FC_CONFIG_MAJMIN=6.1 scripts/build_slim_kernel.sh
#   ARCH=aarch64 scripts/build_slim_kernel.sh

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
for tool in curl tar xz make gcc bc flex bison; do
    command -v "$tool" >/dev/null 2>&1 || MISSING+=("$tool")
done
if [[ ${#MISSING[@]} -ne 0 ]]; then
    echo "[slim-kernel] ERROR: missing build tools: ${MISSING[*]}"
    echo "[slim-kernel] Fedora: sudo dnf install -y xz flex bison bc openssl-devel elfutils-libelf-devel perl-ExtUtils-MakeMaker"
    echo "[slim-kernel] Debian: sudo apt install -y xz-utils flex bison bc libssl-dev libelf-dev"
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

    # Lever 1: trim boot-path kernel features we never use.
    # Target ~40–80 ms cold-boot reduction (initcalls + image size).
    #
    # Note on debug info: Firecracker's microvm base config ships
    # `CONFIG_DEBUG_INFO_NONE=y` — no DWARF, no BTF in the guest kernel
    # ELF. We leave that as-is. Host-side profiling of the `voidbox`
    # process is unaffected (perf-agent resolves Rust release symbols
    # from the voidbox binary itself, not the guest kernel). We only
    # lose the ability to profile *inside* the guest kernel, which we
    # don't do — smaller image and faster ELF load are worth more to
    # us than `perf` on guest-kernel symbols.

    # Audit: no auditd in our guest. Trims initcalls and syscall hooks.
    scripts/config --disable CONFIG_AUDIT
    scripts/config --disable CONFIG_AUDITSYSCALL
    scripts/config --disable CONFIG_AUDIT_WATCH
    scripts/config --disable CONFIG_AUDIT_TREE

    # Hardware RNG drivers: keep CONFIG_RANDOM_TRUST_CPU so RDRAND seeds
    # the pool; drop the virtio_rng / intel_rng / amd_rng / etc. probes
    # that would otherwise hang briefly at boot on missing hardware.
    scripts/config --disable CONFIG_HW_RANDOM
    scripts/config --disable CONFIG_HW_RANDOM_VIRTIO
    scripts/config --disable CONFIG_HW_RANDOM_INTEL
    scripts/config --disable CONFIG_HW_RANDOM_AMD
    scripts/config --enable CONFIG_RANDOM_TRUST_CPU

    # No audio, no video, no USB, no non-virtio input in a microVM.
    # Firecracker's base config already drops most of these, but a few
    # survive via `olddefconfig` when the kernel version moves forward.
    scripts/config --disable CONFIG_SOUND
    scripts/config --disable CONFIG_SND
    scripts/config --disable CONFIG_DRM
    scripts/config --disable CONFIG_DRM_VIRTIO_GPU
    scripts/config --disable CONFIG_USB
    scripts/config --disable CONFIG_USB_SUPPORT
    scripts/config --disable CONFIG_INPUT_JOYDEV
    scripts/config --disable CONFIG_INPUT_TABLET
    scripts/config --disable CONFIG_INPUT_TOUCHSCREEN
    scripts/config --disable CONFIG_INPUT_MISC

    # Filesystems we never mount in the guest. Our guest uses ext4
    # (OCI block lowerdir) + tmpfs + overlayfs + 9p/virtiofs.
    scripts/config --disable CONFIG_BTRFS_FS
    scripts/config --disable CONFIG_XFS_FS
    scripts/config --disable CONFIG_F2FS_FS
    scripts/config --disable CONFIG_JBD2
    scripts/config --disable CONFIG_REISERFS_FS
    scripts/config --disable CONFIG_NFS_FS
    scripts/config --disable CONFIG_NFSD

    # SysRq is a debug convenience. No host reaches the guest's serial
    # console interactively in production.
    scripts/config --disable CONFIG_MAGIC_SYSRQ

    # olddefconfig fills in any new symbols introduced since the Firecracker
    # config was written (older config vs newer kernel tree).
    make ARCH="$MAKE_ARCH" olddefconfig >/dev/null
)

# ---- Build ----
#
# x86_64: `make vmlinux` → uncompressed ELF, suitable for VoidBox's
#         linux-loader-based ELF boot path (reads `e_entry`).
# aarch64: `make Image` → raw Linux ARM64 `Image` binary at
#         `arch/arm64/boot/Image`. VoidBox's aarch64 backend reads this
#         raw into guest memory and sets PC to the load address; an
#         ELF `vmlinux` wouldn't boot because the CPU would execute the
#         ELF header as instructions.
JOBS="$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 4)"
case "$ARCH" in
  x86_64)
    MAKE_TARGET="vmlinux"
    BUILT_ARTIFACT="$SRC_DIR/vmlinux"
    ;;
  aarch64)
    MAKE_TARGET="Image"
    BUILT_ARTIFACT="$SRC_DIR/arch/arm64/boot/Image"
    ;;
esac

echo "[slim-kernel] Building ${MAKE_TARGET} with -j${JOBS} (this takes ~5-10 min)..."
(
    cd "$SRC_DIR"
    make ARCH="$MAKE_ARCH" -j"$JOBS" "$MAKE_TARGET"
)

cp "$BUILT_ARTIFACT" "$OUT_FILE"
chmod 644 "$OUT_FILE"

echo ""
echo "[slim-kernel] Built: $OUT_FILE ($(du -h "$OUT_FILE" | cut -f1))"
echo "[slim-kernel] Use with:"
echo "  VOID_BOX_KERNEL=$PWD/$OUT_FILE"
