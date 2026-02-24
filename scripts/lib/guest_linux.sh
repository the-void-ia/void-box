#!/usr/bin/env bash
# Linux-native guest image build steps.
# Sourced by build_guest_image.sh — not meant to be run directly.
# Expects: guest_common.sh already sourced, OUT_DIR / ARCH / ROOT_DIR set.

# ── Shared libraries for claude-code (via ldd) ───────────────────────────────

install_claude_code_libs_linux() {
  local bin="${CLAUDE_CODE_BIN:-}"
  [[ -n "$bin" && -f "$bin" ]] || return 0
  if file -L "$bin" | grep -q "dynamically linked"; then
    echo "[void-box] Detected dynamically linked binary -- copying shared libraries..."
    copy_shared_libs "$bin"
  fi
}

# ── Host binary installation ─────────────────────────────────────────────────

install_host_binary() {
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
  copy_shared_libs "$bin_path"
}

install_host_binaries() {
  install_host_binary curl
  install_host_binary jq
  install_host_binary bash
  if [[ -f "$OUT_DIR/usr/local/bin/bash" ]]; then
    ln -sf /usr/local/bin/bash "$OUT_DIR/bin/bash"
    echo "[void-box] Symlinked /bin/bash -> /usr/local/bin/bash (real bash)"
  fi
  install_host_binary git

  # git-core helpers
  local git_exec_dir
  git_exec_dir=$(git --exec-path 2>/dev/null || true)
  if [[ -n "$git_exec_dir" && -d "$git_exec_dir" ]]; then
    mkdir -p "$OUT_DIR/$git_exec_dir"
    for helper in git-remote-https git-credential-store; do
      if [[ -f "$git_exec_dir/$helper" ]]; then
        echo "[void-box] Installing git helper $helper..."
        cp -L "$git_exec_dir/$helper" "$OUT_DIR/$git_exec_dir/$helper"
        chmod +x "$OUT_DIR/$git_exec_dir/$helper"
        copy_shared_libs "$git_exec_dir/$helper"
      fi
    done
  else
    echo "[void-box] git --exec-path not found; skipping git-core helpers"
  fi

  # gh CLI: prefer host binary, fallback to download
  if command -v gh &>/dev/null; then
    install_host_binary gh
  else
    download_gh_cli
  fi
}

# ── Kernel modules (from host /lib/modules) ──────────────────────────────────

install_kernel_modules_linux() {
  local kver
  kver=$(uname -r)
  local moddir="/lib/modules/$kver/kernel"
  local dest="$OUT_DIR/lib/modules"
  mkdir -p "$dest"

  _install_kmod() {
    local mod_base="$1"
    local dest_dir="$2"
    local mod_name
    mod_name=$(basename "$mod_base")

    if [[ -f "${mod_base}.ko.xz" ]]; then
      cp "${mod_base}.ko.xz" "$dest_dir/${mod_name}.ko.xz"
      xz -d "$dest_dir/${mod_name}.ko.xz"
      echo "  -> ${mod_name}.ko (from .ko.xz)"
    elif [[ -f "${mod_base}.ko.zst" ]]; then
      zstd -d "${mod_base}.ko.zst" -o "$dest_dir/${mod_name}.ko" --force -q
      echo "  -> ${mod_name}.ko (from .ko.zst)"
    elif [[ -f "${mod_base}.ko" ]]; then
      cp "${mod_base}.ko" "$dest_dir/${mod_name}.ko"
      echo "  -> ${mod_name}.ko (uncompressed)"
    else
      local config_key
      config_key="CONFIG_$(echo "${mod_name}" | tr '[:lower:]' '[:upper:]' | tr '-' '_')"
      local kconfig="/boot/config-${kver}"
      if [[ -f "$kconfig" ]] && grep -q "^${config_key}=y" "$kconfig" 2>/dev/null; then
        echo "  -> ${mod_name} built-in (${config_key}=y)"
      else
        echo "  WARNING: ${mod_name} not found as module (may be built-in or missing)"
      fi
    fi
  }

  echo "[void-box] Adding kernel modules for virtio-mmio, vsock, and networking (kernel $kver)..."
  _install_kmod "$moddir/drivers/virtio/virtio_mmio"                      "$dest"
  _install_kmod "$moddir/net/vmw_vsock/vsock"                             "$dest"
  _install_kmod "$moddir/net/vmw_vsock/vmw_vsock_virtio_transport_common" "$dest"
  _install_kmod "$moddir/net/vmw_vsock/vmw_vsock_virtio_transport"        "$dest"
  _install_kmod "$moddir/net/core/failover"                               "$dest"
  _install_kmod "$moddir/drivers/net/net_failover"                        "$dest"
  _install_kmod "$moddir/drivers/net/virtio_net"                          "$dest"
}
