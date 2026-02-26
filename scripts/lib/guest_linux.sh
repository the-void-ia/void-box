#!/usr/bin/env bash
# Linux-native guest image build steps.
# Sourced by build_guest_image.sh — not meant to be run directly.
# Expects: guest_common.sh already sourced, OUT_DIR / ARCH / ROOT_DIR set.

# ── Shared libraries for claude-code (via ldd) ───────────────────────────

install_claude_code_libs_linux() {
  local bin="${CLAUDE_CODE_BIN:-}"
  [[ -n "$bin" && -f "$bin" ]] || return 0
  if file -L "$bin" | grep -q "dynamically linked"; then
    echo "[void-box] Detected dynamically linked binary -- copying shared libraries..."
    copy_shared_libs "$bin"
  fi
}

# ── Host binary installation ─────────────────────────────────────────────

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

# ── Kernel modules (from host or downloaded from Ubuntu package) ─────────

# Download kernel modules from Ubuntu package matching the target kernel.
# Used in CI or when host kernel doesn't match the VM kernel.
#
# Env vars:
#   VOID_BOX_KMOD_VERSION  — Target kernel version (e.g. "6.8.0-51")
#   VOID_BOX_KMOD_UPLOAD   — Upload number (e.g. "52"), defaults to KERNEL_UPLOAD
install_kernel_modules_from_deb() {
  local dest="$1"
  local kmod_version="${VOID_BOX_KMOD_VERSION:-${KERNEL_VER:-}}"
  local kmod_upload="${VOID_BOX_KMOD_UPLOAD:-${KERNEL_UPLOAD:-}}"

  if [[ -z "$kmod_version" || -z "$kmod_upload" ]]; then
    echo "[void-box] WARNING: VOID_BOX_KMOD_VERSION not set — cannot download modules"
    return 1
  fi

  local kmod_deb_version="${kmod_version}.${kmod_upload}"
  local deb_arch url_base
  case "$ARCH" in
    x86_64)  deb_arch="amd64"; url_base="https://archive.ubuntu.com/ubuntu/pool/main/l/linux" ;;
    aarch64) deb_arch="arm64";  url_base="https://ports.ubuntu.com/pool/main/l/linux" ;;
    *)       echo "[void-box] ERROR: unsupported arch for module download: $ARCH"; return 1 ;;
  esac

  local deb_name="linux-modules-${kmod_version}-generic_${kmod_deb_version}_${deb_arch}.deb"
  local kmod_url="${url_base}/${deb_name}"
  local tmp
  tmp=$(mktemp -d)

  echo "[void-box] Downloading kernel modules (${kmod_version}-generic, ${deb_arch})..."
  echo "[void-box] URL: ${kmod_url}"
  if ! curl -fsSL "$kmod_url" -o "$tmp/modules.deb"; then
    echo "[void-box] WARNING: failed to download kernel modules"
    rm -rf "$tmp"
    return 1
  fi

  (cd "$tmp" && ar x modules.deb)

  local mod_paths=(
    "lib/modules/${kmod_version}-generic/kernel/drivers/virtio/virtio.ko"
    "lib/modules/${kmod_version}-generic/kernel/drivers/virtio/virtio_ring.ko"
    "lib/modules/${kmod_version}-generic/kernel/drivers/virtio/virtio_mmio.ko"
    "lib/modules/${kmod_version}-generic/kernel/net/vmw_vsock/vsock.ko"
    "lib/modules/${kmod_version}-generic/kernel/net/vmw_vsock/vmw_vsock_virtio_transport_common.ko"
    "lib/modules/${kmod_version}-generic/kernel/net/vmw_vsock/vmw_vsock_virtio_transport.ko"
    "lib/modules/${kmod_version}-generic/kernel/net/core/failover.ko"
    "lib/modules/${kmod_version}-generic/kernel/drivers/net/net_failover.ko"
    "lib/modules/${kmod_version}-generic/kernel/drivers/net/virtio_net.ko"
    "lib/modules/${kmod_version}-generic/kernel/fs/netfs/netfs.ko"
    "lib/modules/${kmod_version}-generic/kernel/fs/9p/9p.ko"
    "lib/modules/${kmod_version}-generic/kernel/net/9p/9pnet.ko"
    "lib/modules/${kmod_version}-generic/kernel/net/9p/9pnet_virtio.ko"
    "lib/modules/${kmod_version}-generic/kernel/fs/overlayfs/overlay.ko"
  )

  # Data tarball may be compressed as .zst, .xz, or .gz
  local data_tar
  data_tar=$(find "$tmp" -maxdepth 1 -name 'data.tar*' -print | head -1)
  if [[ -z "$data_tar" ]]; then
    echo "[void-box] WARNING: no data.tar found in modules package"
    rm -rf "$tmp"
    return 1
  fi

  for mod_path in "${mod_paths[@]}"; do
    local mod_name
    mod_name=$(basename "$mod_path")
    local found=false
    for suffix in ".zst" ".xz" ""; do
      tar xf "$data_tar" -C "$tmp" "./${mod_path}${suffix}" 2>/dev/null || continue
      if [[ -f "$tmp/${mod_path}${suffix}" ]]; then
        case "$suffix" in
          .zst) zstd -d "$tmp/${mod_path}${suffix}" -o "$dest/$mod_name" --force -q ;;
          .xz)  xz -d "$tmp/${mod_path}${suffix}" -c > "$dest/$mod_name" ;;
          "")   cp "$tmp/${mod_path}" "$dest/$mod_name" ;;
        esac
        echo "  -> $mod_name (downloaded${suffix:+, $suffix})"
        found=true
        break
      fi
    done
    if [[ "$found" == "false" ]]; then
      echo "  WARNING: $mod_name not found in modules package"
    fi
  done

  rm -rf "$tmp"
}

install_kernel_modules_linux() {
  local dest="$OUT_DIR/lib/modules"
  mkdir -p "$dest"

  # If VOID_BOX_MODULES_DIR is set, use pre-extracted modules.
  if [[ -n "${VOID_BOX_MODULES_DIR:-}" && -d "$VOID_BOX_MODULES_DIR" ]]; then
    echo "[void-box] Installing kernel modules from VOID_BOX_MODULES_DIR=$VOID_BOX_MODULES_DIR"
    cp "$VOID_BOX_MODULES_DIR"/*.ko "$dest/" 2>/dev/null || true
    return
  fi

  # If VOID_BOX_KMOD_VERSION is set, download modules for that kernel (CI / cross-build).
  if [[ -n "${VOID_BOX_KMOD_VERSION:-}" ]]; then
    echo "[void-box] Target kernel differs from host — downloading matching modules..."
    install_kernel_modules_from_deb "$dest"
    return
  fi

  # Default: copy from host /lib/modules (works when host kernel == VM kernel).
  local kver
  kver=$(uname -r)
  local moddir="/lib/modules/$kver/kernel"

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
  _install_kmod "$moddir/drivers/virtio/virtio"                           "$dest"
  _install_kmod "$moddir/drivers/virtio/virtio_ring"                      "$dest"
  _install_kmod "$moddir/drivers/virtio/virtio_mmio"                      "$dest"
  _install_kmod "$moddir/net/vmw_vsock/vsock"                             "$dest"
  _install_kmod "$moddir/net/vmw_vsock/vmw_vsock_virtio_transport_common" "$dest"
  _install_kmod "$moddir/net/vmw_vsock/vmw_vsock_virtio_transport"        "$dest"
  _install_kmod "$moddir/net/core/failover"                               "$dest"
  _install_kmod "$moddir/drivers/net/net_failover"                        "$dest"
  _install_kmod "$moddir/drivers/net/virtio_net"                          "$dest"
  _install_kmod "$moddir/fs/netfs/netfs"                                 "$dest"
  _install_kmod "$moddir/fs/9p/9p"                                       "$dest"
  _install_kmod "$moddir/net/9p/9pnet"                                   "$dest"
  _install_kmod "$moddir/net/9p/9pnet_virtio"                            "$dest"
  _install_kmod "$moddir/fs/overlayfs/overlay"                           "$dest"
}
