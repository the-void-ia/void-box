#!/usr/bin/env bash
# Shared helpers for building void-box guest images.
# Sourced by build_guest_image.sh — not meant to be run directly.

# ── Rootfs skeleton ───────────────────────────────────────────────────────────

prepare_rootfs() {
  echo "[void-box] Preparing rootfs at: $OUT_DIR"
  rm -rf "$OUT_DIR"
  mkdir -p "$OUT_DIR"/{bin,sbin,proc,sys,dev,tmp,usr/local/bin,etc,etc/udhcpc,usr/share/udhcpc,lib/modules}
}

# ── DHCP client script (for VZ NAT on macOS and virtio-net on Linux) ──────────

install_dhcp_script() {
  local script='#!/bin/sh
case "$1" in
  bound|renew)
    /bin/ip addr flush dev "$interface" 2>/dev/null
    /bin/ip addr add "$ip/$mask" dev "$interface"
    if [ -n "$router" ]; then
      /bin/ip route add default via "$router" dev "$interface"
    fi
    if [ -n "$dns" ]; then
      : > /etc/resolv.conf
      for d in $dns; do
        echo "nameserver $d" >> /etc/resolv.conf
      done
    fi
    ;;
esac'
  echo "$script" > "$OUT_DIR/usr/share/udhcpc/default.script"
  echo "$script" > "$OUT_DIR/etc/udhcpc/default.script"
  chmod +x "$OUT_DIR/usr/share/udhcpc/default.script"
  chmod +x "$OUT_DIR/etc/udhcpc/default.script"
}

# ── Guest-agent (init) ────────────────────────────────────────────────────────

install_guest_agent() {
  local bin="$1"
  echo "[void-box] Installing guest-agent as /init (PID 1)..."
  cp "$bin" "$OUT_DIR/init"
  chmod +x "$OUT_DIR/init"
  echo "[void-box] Installing guest-agent in /sbin..."
  cp "$bin" "$OUT_DIR/sbin/guest-agent"
}

# ── Claude-code binary ────────────────────────────────────────────────────────

install_claude_code_binary() {
  local bin="${CLAUDE_CODE_BIN:-}"
  if [[ -n "$bin" && -f "$bin" ]]; then
    echo "[void-box] Installing real claude-code from \$CLAUDE_CODE_BIN at /usr/local/bin/claude-code..."
    cp "$bin" "$OUT_DIR/usr/local/bin/claude-code"
    chmod +x "$OUT_DIR/usr/local/bin/claude-code"
    return 0
  elif [[ -f "$ROOT_DIR/scripts/guest/claude-code-mock.sh" ]]; then
    echo "[void-box] Installing claude-code mock at /usr/local/bin/claude-code..."
    cp "$ROOT_DIR/scripts/guest/claude-code-mock.sh" "$OUT_DIR/usr/local/bin/claude-code"
    chmod +x "$OUT_DIR/usr/local/bin/claude-code"
    return 1  # signal: mock installed, no libs needed
  fi
  return 1
}

# ── Shared-library copying (for dynamically linked ELF binaries on Linux) ─────

copy_shared_libs() {
  local bin_path="$1"
  if ! file -L "$bin_path" | grep -q "dynamically linked"; then
    return
  fi
  ldd "$bin_path" 2>/dev/null | while read -r line; do
    local lib_path=""
    if echo "$line" | grep -q "=>"; then
      lib_path=$(echo "$line" | awk '{print $3}')
    elif echo "$line" | grep -q "^[[:space:]]*/"; then
      lib_path=$(echo "$line" | awk '{print $1}')
    fi
    if [[ -z "$lib_path" || "$lib_path" == "linux-vdso"* || ! -f "$lib_path" ]]; then
      continue
    fi
    local lib_dir
    lib_dir=$(dirname "$lib_path")
    mkdir -p "$OUT_DIR$lib_dir"
    if [[ ! -f "$OUT_DIR$lib_path" ]]; then
      cp -L "$lib_path" "$OUT_DIR$lib_path"
      echo "  -> $lib_path"
    fi
  done
}

# ── BusyBox ───────────────────────────────────────────────────────────────────

install_busybox() {
  if [[ -n "${BUSYBOX:-}" && -f "$BUSYBOX" ]]; then
    echo "[void-box] Installing BusyBox at /bin/sh and /bin/busybox..."
    cp "$BUSYBOX" "$OUT_DIR/bin/busybox"
    chmod +x "$OUT_DIR/bin/busybox"
    ln -sf busybox "$OUT_DIR/bin/sh"
    for cmd in echo cat tr test base64 uname ls mkdir rm cp mv pwd id hostname ip sed grep awk env wget nc udhcpc; do
      ln -sf busybox "$OUT_DIR/bin/$cmd" 2>/dev/null || true
    done
  else
    echo "[void-box] No BUSYBOX set; guest will have no /bin/sh (set BUSYBOX=/path/to/busybox for full shell support)."
  fi
}

# ── gh CLI download (used by both platforms when host binary is unavailable) ──

download_gh_cli() {
  local gh_version="2.65.0"
  local gh_arch
  case "$ARCH" in
    x86_64)  gh_arch="amd64" ;;
    aarch64) gh_arch="arm64" ;;
  esac
  local tarball="gh_${gh_version}_linux_${gh_arch}.tar.gz"
  local url="https://github.com/cli/cli/releases/download/v${gh_version}/${tarball}"
  local tmp
  tmp=$(mktemp -d)
  echo "[void-box] Downloading gh v${gh_version} (linux/${gh_arch}) from GitHub..."
  if curl -fsSL "$url" -o "$tmp/$tarball"; then
    tar -xzf "$tmp/$tarball" -C "$tmp"
    cp "$tmp/gh_${gh_version}_linux_${gh_arch}/bin/gh" "$OUT_DIR/usr/local/bin/gh"
    chmod +x "$OUT_DIR/usr/local/bin/gh"
    echo "[void-box] Installed gh v${gh_version} (static Go binary)"
  else
    echo "[void-box] WARNING: failed to download gh -- skipping"
  fi
  rm -rf "$tmp"
}

# ── Initramfs packing ────────────────────────────────────────────────────────

pack_initramfs() {
  echo "[void-box] Creating initramfs at: $OUT_CPIO"
  ( cd "$OUT_DIR" && find . | cpio -o -H newc | gzip ) > "$OUT_CPIO"
  echo "[void-box] Done. Initramfs: $OUT_CPIO"
}
