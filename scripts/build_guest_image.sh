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

# Claude-code binary for workflow demos and tests
# Priority:
#   1. If CLAUDE_CODE_BIN is set and points to an existing file on the host,
#      copy that into the guest as /usr/local/bin/claude-code. This is the
#      \"real\" claude-code CLI (can talk to Anthropic from inside the guest).
#   2. Otherwise, fall back to the built-in shell mock script which simulates
#      plan/apply behaviour without doing real network calls.
if [[ -n "${CLAUDE_CODE_BIN:-}" && -f "$CLAUDE_CODE_BIN" ]]; then
  echo "[void-box] Installing real claude-code from \$CLAUDE_CODE_BIN at /usr/local/bin/claude-code..."
  cp "$CLAUDE_CODE_BIN" "$OUT_DIR/usr/local/bin/claude-code"
  chmod +x "$OUT_DIR/usr/local/bin/claude-code"

  # If the binary is dynamically linked, copy its shared libraries into the
  # initramfs so the kernel's ELF loader can find them at runtime.
  if file -L "$CLAUDE_CODE_BIN" | grep -q "dynamically linked"; then
    echo "[void-box] Detected dynamically linked binary -- copying shared libraries..."
    # Use ldd to discover required libraries and their host paths
    ldd "$CLAUDE_CODE_BIN" 2>/dev/null | while read -r line; do
      # Parse lines like:  libc.so.6 => /lib64/libc.so.6 (0x...)
      #                or: /lib64/ld-linux-x86-64.so.2 (0x...)
      lib_path=""
      if echo "$line" | grep -q "=>"; then
        lib_path=$(echo "$line" | awk '{print $3}')
      elif echo "$line" | grep -q "^[[:space:]]*/"; then
        lib_path=$(echo "$line" | awk '{print $1}')
      fi

      # Skip virtual libraries (linux-vdso) and empty paths
      if [[ -z "$lib_path" || "$lib_path" == "linux-vdso"* || ! -f "$lib_path" ]]; then
        continue
      fi

      # Preserve the original directory structure in the initramfs
      lib_dir=$(dirname "$lib_path")
      mkdir -p "$OUT_DIR$lib_dir"
      if [[ ! -f "$OUT_DIR$lib_path" ]]; then
        cp -L "$lib_path" "$OUT_DIR$lib_path"
        echo "  -> $lib_path"
      fi
    done
  fi
elif [[ -f "$ROOT_DIR/scripts/guest/claude-code-mock.sh" ]]; then
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
  for cmd in echo cat tr test base64 uname ls mkdir rm cp mv pwd id hostname ip sed grep awk env wget nc; do
    ln -sf busybox "$OUT_DIR/bin/$cmd" 2>/dev/null || true
  done
else
  echo "[void-box] No BUSYBOX set; guest will have no /bin/sh (set BUSYBOX=/path/to/busybox for full shell support)."
fi

# Optional: install host curl and jq for HTTP/JSON skills (e.g., HackerNews agent).
# These are dynamically linked, so we copy their shared libraries too.
_install_host_binary() {
  local bin_name="$1"
  local bin_path
  bin_path=$(command -v "$bin_name" 2>/dev/null || true)
  if [[ -z "$bin_path" || ! -f "$bin_path" ]]; then
    echo "[void-box] $bin_name not found on host -- skipping"
    return
  fi
  echo "[void-box] Installing $bin_name from $bin_path..."
  cp -L "$bin_path" "$OUT_DIR/usr/local/bin/$bin_name"
  chmod +x "$OUT_DIR/usr/local/bin/$bin_name"

  # Copy shared libraries if dynamically linked
  if file -L "$bin_path" | grep -q "dynamically linked"; then
    ldd "$bin_path" 2>/dev/null | while read -r line; do
      lib_path=""
      if echo "$line" | grep -q "=>"; then
        lib_path=$(echo "$line" | awk '{print $3}')
      elif echo "$line" | grep -q "^[[:space:]]*/"; then
        lib_path=$(echo "$line" | awk '{print $1}')
      fi
      if [[ -z "$lib_path" || "$lib_path" == "linux-vdso"* || ! -f "$lib_path" ]]; then
        continue
      fi
      lib_dir=$(dirname "$lib_path")
      mkdir -p "$OUT_DIR$lib_dir"
      if [[ ! -f "$OUT_DIR$lib_path" ]]; then
        cp -L "$lib_path" "$OUT_DIR$lib_path"
        echo "  -> $lib_path"
      fi
    done
  fi
}

_install_host_binary curl
_install_host_binary jq
_install_host_binary bash
# Ensure /bin/bash points to real bash (not busybox applet)
if [[ -f "$OUT_DIR/usr/local/bin/bash" ]]; then
  ln -sf /usr/local/bin/bash "$OUT_DIR/bin/bash"
  echo "[void-box] Symlinked /bin/bash -> /usr/local/bin/bash (real bash)"
fi
_install_host_binary git

# git-core helpers: git-remote-https and git-credential-store are separate
# executables that git shells out to for HTTPS push and credential storage.
_git_exec_dir=$(git --exec-path 2>/dev/null || true)
if [[ -n "$_git_exec_dir" && -d "$_git_exec_dir" ]]; then
  mkdir -p "$OUT_DIR/$_git_exec_dir"
  for helper in git-remote-https git-credential-store; do
    if [[ -f "$_git_exec_dir/$helper" ]]; then
      echo "[void-box] Installing git helper $helper..."
      cp -L "$_git_exec_dir/$helper" "$OUT_DIR/$_git_exec_dir/$helper"
      chmod +x "$OUT_DIR/$_git_exec_dir/$helper"
      # Copy shared libraries for this helper
      if file -L "$_git_exec_dir/$helper" | grep -q "dynamically linked"; then
        ldd "$_git_exec_dir/$helper" 2>/dev/null | while read -r line; do
          lib_path=""
          if echo "$line" | grep -q "=>"; then
            lib_path=$(echo "$line" | awk '{print $3}')
          elif echo "$line" | grep -q "^[[:space:]]*/"; then
            lib_path=$(echo "$line" | awk '{print $1}')
          fi
          if [[ -z "$lib_path" || "$lib_path" == "linux-vdso"* || ! -f "$lib_path" ]]; then
            continue
          fi
          lib_dir=$(dirname "$lib_path")
          mkdir -p "$OUT_DIR$lib_dir"
          if [[ ! -f "$OUT_DIR$lib_path" ]]; then
            cp -L "$lib_path" "$OUT_DIR$lib_path"
            echo "  -> $lib_path"
          fi
        done
      fi
    fi
  done
else
  echo "[void-box] git --exec-path not found; skipping git-core helpers"
fi

# gh CLI: try host binary first, fallback to downloading from GitHub releases.
if command -v gh &>/dev/null; then
  _install_host_binary gh
else
  GH_VERSION="2.65.0"
  GH_TARBALL="gh_${GH_VERSION}_linux_amd64.tar.gz"
  GH_URL="https://github.com/cli/cli/releases/download/v${GH_VERSION}/${GH_TARBALL}"
  GH_TMP=$(mktemp -d)
  echo "[void-box] gh not found on host -- downloading v${GH_VERSION} from GitHub..."
  if curl -fsSL "$GH_URL" -o "$GH_TMP/$GH_TARBALL"; then
    tar -xzf "$GH_TMP/$GH_TARBALL" -C "$GH_TMP"
    cp "$GH_TMP/gh_${GH_VERSION}_linux_amd64/bin/gh" "$OUT_DIR/usr/local/bin/gh"
    chmod +x "$OUT_DIR/usr/local/bin/gh"
    echo "[void-box] Installed gh v${GH_VERSION} (static Go binary)"
  else
    echo "[void-box] WARNING: failed to download gh -- skipping"
  fi
  rm -rf "$GH_TMP"
fi

# Copy kernel modules needed for virtio-mmio and vsock
KVER=$(uname -r)
MODDIR="/lib/modules/$KVER/kernel"
DEST_MODDIR="$OUT_DIR/lib/modules"
mkdir -p "$DEST_MODDIR"

echo "[void-box] Adding kernel modules for virtio-mmio, vsock, and networking (kernel $KVER)..."
# virtio_mmio: virtio device on MMIO bus
# vsock: VM socket communication
# virtio_net + deps: network driver for SLIRP networking
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
