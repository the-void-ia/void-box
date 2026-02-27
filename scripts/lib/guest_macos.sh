#!/usr/bin/env bash
# macOS (Apple Silicon) cross-build steps for void-box guest images.
# Sourced by build_guest_image.sh — not meant to be run directly.
# Expects: guest_common.sh already sourced, OUT_DIR / ARCH / GUEST_TARGET / ROOT_DIR set.

# ── Static busybox for ARM64 guest (macOS has no native busybox) ──────────────

ensure_busybox_macos() {
  if [[ -n "${BUSYBOX:-}" && -f "$BUSYBOX" ]]; then
    return 0
  fi

  local cached="$ROOT_DIR/target/busybox-aarch64"
  if [[ -f "$cached" ]]; then
    export BUSYBOX="$cached"
    echo "[void-box] Using cached busybox: $cached"
    return 0
  fi

  echo "[void-box] Downloading static busybox (ARM64) for guest..."
  local tmp
  tmp=$(mktemp -d)
  local deb_url="https://launchpad.net/ubuntu/+archive/primary/+files/busybox-static_1.36.1-6ubuntu3.1_arm64.deb"
  if curl -fsSL "$deb_url" -o "$tmp/busybox-static.deb"; then
    (cd "$tmp" && ar x busybox-static.deb && tar xf data.tar* ./usr/bin/busybox 2>/dev/null)
    if [[ -f "$tmp/usr/bin/busybox" ]]; then
      cp "$tmp/usr/bin/busybox" "$cached"
      chmod +x "$cached"
      export BUSYBOX="$cached"
      echo "[void-box] Installed static busybox to $cached"
    else
      echo "[void-box] WARNING: busybox extraction failed"
    fi
  else
    echo "[void-box] WARNING: failed to download busybox"
  fi
  rm -rf "$tmp"
}

# ── Cross-linker detection ────────────────────────────────────────────────────

setup_cross_linker() {
  local cross_prefix="${ARCH}-linux-musl"
  if ! command -v "${cross_prefix}-gcc" &>/dev/null; then
    cross_prefix="${ARCH}-unknown-linux-musl"
    if ! command -v "${cross_prefix}-gcc" &>/dev/null; then
      echo "[void-box] ERROR: musl cross-linker not found for $ARCH."
      echo "  Install one via Homebrew:"
      echo "    brew install filosottile/musl-cross/musl-cross --with-${ARCH}"
      echo "  or:"
      echo "    brew install messense/macos-cross-toolchains/${ARCH}-unknown-linux-musl"
      exit 1
    fi
  fi
  export "CARGO_TARGET_$(echo "$GUEST_TARGET" | tr '[:lower:]-' '[:upper:]_')_LINKER=${cross_prefix}-gcc"
  echo "[void-box] Cross-linker: ${cross_prefix}-gcc"
}

# ── ARM64 glibc + libstdc++ + libgcc download ────────────────────────────────

install_claude_code_libs_macos() {
  local bin="${CLAUDE_CODE_BIN:-}"
  [[ -n "$bin" && -f "$bin" ]] || return 0
  if ! file -L "$bin" | grep -q "dynamically linked"; then
    return 0
  fi

  echo "[void-box] Dynamically linked binary detected (cross-build)."
  echo "[void-box] Downloading ARM64 glibc + libstdc++ + libgcc for the guest..."

  local tmp
  tmp=$(mktemp -d)
  local guest_libdir="$OUT_DIR/usr/lib/aarch64-linux-gnu"
  mkdir -p "$guest_libdir" "$OUT_DIR/lib"

  # libc6
  local libc6_url="https://launchpad.net/ubuntu/+archive/primary/+files/libc6_2.39-0ubuntu8.4_arm64.deb"
  if curl -fsSL "$libc6_url" -o "$tmp/libc6.deb"; then
    (cd "$tmp" && ar x libc6.deb && tar xf data.tar* \
      ./usr/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1 \
      ./usr/lib/aarch64-linux-gnu/libc.so.6 \
      ./usr/lib/aarch64-linux-gnu/libdl.so.2 \
      ./usr/lib/aarch64-linux-gnu/libm.so.6 \
      ./usr/lib/aarch64-linux-gnu/libpthread.so.0 \
      ./usr/lib/aarch64-linux-gnu/librt.so.1 \
      ./usr/lib/ld-linux-aarch64.so.1 2>/dev/null)
    cp "$tmp"/usr/lib/aarch64-linux-gnu/*.so.* "$guest_libdir/" 2>/dev/null || true
    ln -sf /usr/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1 "$OUT_DIR/lib/ld-linux-aarch64.so.1"
    echo "[void-box] Installed glibc (libc6) ARM64 shared libraries"
  else
    echo "[void-box] WARNING: failed to download libc6 -- claude-code may not run"
  fi

  # libstdc++6
  local stdcpp_url="https://launchpad.net/ubuntu/+archive/primary/+files/libstdc++6_14.2.0-4ubuntu2~24.04_arm64.deb"
  if curl -fsSL "$stdcpp_url" -o "$tmp/libstdcpp.deb"; then
    (cd "$tmp" && ar x libstdcpp.deb && tar xf data.tar* \
      ./usr/lib/aarch64-linux-gnu/libstdc++.so.6 \
      ./usr/lib/aarch64-linux-gnu/libstdc++.so.6.0.33 2>/dev/null)
    cp "$tmp"/usr/lib/aarch64-linux-gnu/libstdc++.so.6* "$guest_libdir/" 2>/dev/null || true
    echo "[void-box] Installed libstdc++6 ARM64"
  else
    echo "[void-box] WARNING: failed to download libstdc++6"
  fi

  # libgcc-s1
  local libgcc_url="https://launchpad.net/ubuntu/+archive/primary/+files/libgcc-s1_14.2.0-4ubuntu2~24.04_arm64.deb"
  if curl -fsSL "$libgcc_url" -o "$tmp/libgcc.deb"; then
    (cd "$tmp" && ar x libgcc.deb && tar xf data.tar* \
      ./usr/lib/aarch64-linux-gnu/libgcc_s.so.1 2>/dev/null)
    cp "$tmp"/usr/lib/aarch64-linux-gnu/libgcc_s.so.1 "$guest_libdir/" 2>/dev/null || true
    echo "[void-box] Installed libgcc_s1 ARM64"
  else
    echo "[void-box] WARNING: failed to download libgcc-s1"
  fi

  rm -rf "$tmp"
}

# ── Kernel modules (downloaded from Ubuntu ARM64 packages) ────────────────────

install_kernel_modules_macos() {
  local dest="$OUT_DIR/lib/modules"
  mkdir -p "$dest"

  if [[ -n "${VOID_BOX_MODULES_DIR:-}" && -d "$VOID_BOX_MODULES_DIR" ]]; then
    echo "[void-box] Installing kernel modules from VOID_BOX_MODULES_DIR=$VOID_BOX_MODULES_DIR"
    cp "$VOID_BOX_MODULES_DIR"/*.ko "$dest/" 2>/dev/null || true
    return
  fi

  [[ "$ARCH" == "aarch64" ]] || return 0

  # Must match download_kernel.sh (KERNEL_VER) so modules are compatible with the VM kernel
  local kmod_version="${VOID_BOX_KMOD_VERSION:-6.8.0-51}"
  local kmod_upload="${VOID_BOX_KMOD_UPLOAD:-52}"
  local kmod_deb_version="${kmod_version}.${kmod_upload}"
  local kmod_url="https://launchpad.net/ubuntu/+archive/primary/+files/linux-modules-${kmod_version}-generic_${kmod_deb_version}_arm64.deb"
  local tmp
  tmp=$(mktemp -d)

  echo "[void-box] Downloading Ubuntu ARM64 kernel modules (${kmod_version}-generic)..."
  if curl -fsSL "$kmod_url" -o "$tmp/modules.deb"; then
    (cd "$tmp" && ar x modules.deb)
    local vsock_modules=(
      "lib/modules/${kmod_version}-generic/kernel/net/vmw_vsock/vsock.ko.zst"
      "lib/modules/${kmod_version}-generic/kernel/net/vmw_vsock/vmw_vsock_virtio_transport_common.ko.zst"
      "lib/modules/${kmod_version}-generic/kernel/net/vmw_vsock/vmw_vsock_virtio_transport.ko.zst"
    )
    local virtiofs_modules=(
      "lib/modules/${kmod_version}-generic/kernel/fs/fuse/virtiofs.ko.zst"
    )
    local overlay_modules=(
      "lib/modules/${kmod_version}-generic/kernel/fs/overlayfs/overlay.ko.zst"
    )
    for mod_path in "${vsock_modules[@]}" "${virtiofs_modules[@]}" "${overlay_modules[@]}"; do
      local mod_name
      mod_name=$(basename "$mod_path" .zst)
      tar xf "$tmp/data.tar" -C "$tmp" "./$mod_path" 2>/dev/null || true
      if [[ -f "$tmp/$mod_path" ]]; then
        zstd -d "$tmp/$mod_path" -o "$dest/$mod_name" --force -q
        echo "[void-box] Installed kernel module: $mod_name"
      else
        echo "[void-box] WARNING: $mod_name not found in modules package"
      fi
    done
  else
    echo "[void-box] WARNING: failed to download kernel modules -- vsock may not work"
  fi
  rm -rf "$tmp"
}
