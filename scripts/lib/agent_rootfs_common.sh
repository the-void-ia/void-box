#!/usr/bin/env bash
# Shared helpers for agent-flavor rootfs builds (claude, codex, …).
# Sourced by build_<agent>_rootfs.sh — not meant to be run directly.
# Caller owns `set -euo pipefail`.

# ── Architecture & cross-build detection ────────────────────────────────────
# Sets: GUEST_ARCH, IS_CROSS_BUILD

detect_guest_arch() {
  local host_arch
  host_arch="$(uname -m)"
  case "$host_arch" in
    arm64) host_arch="aarch64" ;;
  esac
  GUEST_ARCH="${ARCH:-$host_arch}"

  IS_CROSS_BUILD=false
  if [[ "$(uname -s)" == "Darwin" ]]; then
    IS_CROSS_BUILD=true
  fi
}

# ── Claude binary resolution ───────────────────────────────────────────────
# Locates or downloads a Linux ELF claude-code binary.
#
# Reads: CLAUDE_BIN, CLAUDE_CODE_VERSION (env vars, both optional)
# Requires: GUEST_ARCH, IS_CROSS_BUILD (call detect_guest_arch first)
# Sets: CLAUDE_BIN (absolute path to the resolved binary)
#
# Discovery chain:
#   1. CLAUDE_BIN env var (explicit path)
#   2. Local PATH (~/.local/bin/claude or `which claude`) — Linux only, ELF-checked
#   3. macOS auto-detect: run local `claude --version`, set CLAUDE_CODE_VERSION
#   4. CLAUDE_CODE_VERSION → download Linux build from GCS
#   5. Validate exists + is ELF

resolve_claude_binary() {
  local log_prefix="${1:-agent-rootfs}"
  CLAUDE_BIN="${CLAUDE_BIN:-}"

  # Map guest arch to claude-code download platform string.
  local claude_platform
  case "$GUEST_ARCH" in
    x86_64)  claude_platform="linux-x64" ;;
    aarch64) claude_platform="linux-arm64" ;;
    *)       echo "ERROR: unsupported guest architecture: $GUEST_ARCH" >&2; exit 1 ;;
  esac

  # 1. Try locally installed binary (Linux only — macOS has Mach-O).
  if [[ -z "$CLAUDE_BIN" && -z "${CLAUDE_CODE_VERSION:-}" && "$IS_CROSS_BUILD" == "false" ]]; then
    local candidate
    for candidate in \
      "$HOME/.local/bin/claude" \
      "$(command -v claude 2>/dev/null || true)" \
      ; do
      if [[ -n "$candidate" && -f "$candidate" ]] \
         && file -L "$candidate" 2>/dev/null | grep -q "ELF.*executable"; then
        CLAUDE_BIN="$(readlink -f "$candidate")"
        break
      fi
    done
  fi

  # 2. macOS auto-detect: derive CLAUDE_CODE_VERSION from local install.
  if [[ -z "$CLAUDE_BIN" && -z "${CLAUDE_CODE_VERSION:-}" && "$IS_CROSS_BUILD" == "true" ]]; then
    local local_claude
    local_claude="$(command -v claude 2>/dev/null || true)"
    if [[ -n "$local_claude" ]]; then
      local detected_ver
      detected_ver="$("$local_claude" --version 2>/dev/null | head -1 | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1 || true)"
      if [[ -n "$detected_ver" ]]; then
        echo "[$log_prefix] macOS detected — will download Linux build of claude-code v${detected_ver}"
        CLAUDE_CODE_VERSION="$detected_ver"
      fi
    fi
  fi

  # 3. Download via CLAUDE_CODE_VERSION.
  if [[ -z "$CLAUDE_BIN" && -n "${CLAUDE_CODE_VERSION:-}" ]]; then
    local gcs_base="https://storage.googleapis.com/claude-code-dist-86c565f3-f756-42ad-8dfa-d59b1c096819/claude-code-releases"
    local download_url="$gcs_base/$CLAUDE_CODE_VERSION/$claude_platform/claude"
    local download_dir="$ROOT_DIR/target/claude-download"
    mkdir -p "$download_dir"
    CLAUDE_BIN="$download_dir/claude-${CLAUDE_CODE_VERSION}-${claude_platform}"

    if [[ ! -f "$CLAUDE_BIN" ]]; then
      echo "[$log_prefix] Downloading claude-code v${CLAUDE_CODE_VERSION} (${claude_platform})..."
      if ! curl -fSL --progress-bar -o "$CLAUDE_BIN" "$download_url"; then
        echo "ERROR: Failed to download claude-code from $download_url" >&2
        echo "  Check that version $CLAUDE_CODE_VERSION exists for $claude_platform." >&2
        rm -f "$CLAUDE_BIN"
        exit 1
      fi
      chmod +x "$CLAUDE_BIN"
    else
      echo "[$log_prefix] Using cached download: $CLAUDE_BIN"
    fi
  fi

  # 4. Validate presence.
  if [[ -z "$CLAUDE_BIN" || ! -f "$CLAUDE_BIN" ]]; then
    echo "ERROR: Native claude binary not found." >&2
    echo "" >&2
    echo "Options:" >&2
    echo "  1. Install claude:  curl -fsSL https://claude.ai/install.sh | sh" >&2
    echo "     (on macOS, the Linux binary will be auto-downloaded)" >&2
    echo "  2. Set CLAUDE_BIN=/path/to/linux/claude (must be a Linux ELF binary)" >&2
    echo "  3. Set CLAUDE_CODE_VERSION=2.1.45 for automatic download" >&2
    exit 1
  fi

  # 5. Validate it's a Linux ELF binary.
  if ! file -L "$CLAUDE_BIN" | grep -q "ELF.*executable"; then
    echo "ERROR: $CLAUDE_BIN is not a native Linux ELF binary." >&2
    echo "  file: $(file -L "$CLAUDE_BIN")" >&2
    if [[ "$IS_CROSS_BUILD" == "true" ]]; then
      echo "  On macOS, set CLAUDE_CODE_VERSION to download the Linux build:" >&2
      echo "    CLAUDE_CODE_VERSION=2.0.76 $0" >&2
    else
      echo "  Make sure you have the native claude-code binary (not the npm wrapper)." >&2
    fi
    exit 1
  fi

  local claude_size
  claude_size="$(du -sh "$CLAUDE_BIN" | awk '{print $1}')"
  echo "[$log_prefix] Claude binary: $CLAUDE_BIN ($claude_size)"
}

# ── Codex binary resolution ───────────────────────────────────────────────
# Locates or downloads a Linux ELF codex binary.
#
# Reads: CODEX_BIN, CODEX_VERSION (env vars, both optional)
# Requires: GUEST_ARCH, IS_CROSS_BUILD (call detect_guest_arch first)
# Sets: CODEX_BIN (absolute path to the resolved binary)
#
# Discovery chain:
#   1. CODEX_BIN env var (explicit path)
#   2. Local PATH (`which codex`) — Linux only, ELF-checked, warns on non-ELF
#   3. macOS auto-detect: run local `codex --version`, set CODEX_VERSION
#   4. CODEX_VERSION → download from GitHub releases
#   5. Validate exists + is ELF

resolve_codex_binary() {
  local log_prefix="${1:-agent-rootfs}"
  CODEX_BIN="${CODEX_BIN:-}"

  # Map guest arch to codex GitHub release asset suffix.
  local codex_target
  case "$GUEST_ARCH" in
    x86_64)  codex_target="x86_64-unknown-linux-musl" ;;
    aarch64) codex_target="aarch64-unknown-linux-musl" ;;
    *)       echo "ERROR: unsupported guest architecture: $GUEST_ARCH" >&2; exit 1 ;;
  esac

  # 1. Try locally installed binary (Linux only — macOS has Mach-O).
  # An explicit CODEX_VERSION takes priority so the user always gets the
  # requested build even if a stale/wrapper `codex` is on PATH.
  if [[ -z "$CODEX_BIN" && -z "${CODEX_VERSION:-}" && "$IS_CROSS_BUILD" == "false" ]]; then
    local local_codex
    local_codex="$(command -v codex 2>/dev/null || true)"
    if [[ -n "$local_codex" && -f "$local_codex" ]]; then
      if file -L "$local_codex" 2>/dev/null | grep -q "ELF.*executable"; then
        CODEX_BIN="$(readlink -f "$local_codex")"
      else
        echo "[$log_prefix] PATH has a non-ELF codex ($local_codex) — skipping; set CODEX_VERSION to download a native build." >&2
      fi
    fi
  fi

  # 2. macOS auto-detect: derive CODEX_VERSION from local install.
  if [[ -z "$CODEX_BIN" && -z "${CODEX_VERSION:-}" && "$IS_CROSS_BUILD" == "true" ]]; then
    local local_codex
    local_codex="$(command -v codex 2>/dev/null || true)"
    if [[ -n "$local_codex" ]]; then
      # Extract bare version number (e.g. "0.120.0" from "codex-cli 0.120.0")
      local detected_ver
      detected_ver="$("$local_codex" --version 2>/dev/null | head -1 | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1 || true)"
      if [[ -n "$detected_ver" ]]; then
        echo "[$log_prefix] macOS detected — will download Linux build of codex v${detected_ver}"
        CODEX_VERSION="$detected_ver"
      fi
    fi
  fi

  # 3. Download via CODEX_VERSION.
  if [[ -z "$CODEX_BIN" && -n "${CODEX_VERSION:-}" ]]; then
    local release_url="https://github.com/openai/codex/releases/download/rust-v${CODEX_VERSION}/codex-${codex_target}.tar.gz"
    local download_dir="$ROOT_DIR/target/codex-download"
    mkdir -p "$download_dir"
    local cached_bin="$download_dir/codex-${CODEX_VERSION}-${codex_target}"

    if [[ ! -f "$cached_bin" ]]; then
      echo "[$log_prefix] Downloading codex v${CODEX_VERSION} (${codex_target})..."
      local tmp_dir
      tmp_dir="$(mktemp -d)"
      trap 'rm -rf "$tmp_dir"' EXIT
      local tmp_tar="$tmp_dir/codex.tar.gz"
      if ! curl -fSL --progress-bar -o "$tmp_tar" "$release_url"; then
        echo "ERROR: Failed to download codex from $release_url" >&2
        echo "  Check that version $CODEX_VERSION exists for $codex_target." >&2
        exit 1
      fi
      tar -xzf "$tmp_tar" -C "$tmp_dir"
      local extracted_bin
      extracted_bin="$(find_extracted_executable "$tmp_dir" || true)"
      if [[ -z "$extracted_bin" ]]; then
        echo "ERROR: tarball did not contain an executable codex binary" >&2
        ls -laR "$tmp_dir" >&2
        exit 1
      fi
      cp "$extracted_bin" "$cached_bin"
      chmod +x "$cached_bin"
      trap - EXIT
      rm -rf "$tmp_dir"
    else
      echo "[$log_prefix] Using cached download: $cached_bin"
    fi
    CODEX_BIN="$cached_bin"
  fi

  # 4. Validate presence.
  if [[ -z "$CODEX_BIN" || ! -f "$CODEX_BIN" ]]; then
    echo "ERROR: codex binary not found." >&2
    echo "" >&2
    echo "Options:" >&2
    echo "  1. Install codex on your PATH (Linux host only; macOS Mach-O binaries cannot run in the Linux guest)" >&2
    echo "  2. Set CODEX_BIN=/path/to/linux/codex (must be a Linux ELF binary)" >&2
    echo "  3. Set CODEX_VERSION=0.118.0 for automatic download" >&2
    exit 1
  fi

  # 5. Validate it's a Linux ELF binary.
  if ! file -L "$CODEX_BIN" | grep -q "ELF.*executable"; then
    echo "ERROR: $CODEX_BIN is not a native Linux ELF binary." >&2
    echo "  file: $(file -L "$CODEX_BIN")" >&2
    if [[ "$IS_CROSS_BUILD" == "true" ]]; then
      echo "  On macOS, set CODEX_VERSION to download the Linux build:" >&2
      echo "    CODEX_VERSION=0.118.0 $0" >&2
    fi
    exit 1
  fi

  local codex_size
  codex_size="$(du -sh "$CODEX_BIN" | awk '{print $1}')"
  echo "[$log_prefix] Codex binary: $CODEX_BIN ($codex_size)"
}

# ── Busybox setup ───────────────────────────────────────────────────────────
# On Linux, defaults to /usr/bin/busybox and warns if missing.
# On macOS, build_guest_image.sh auto-downloads via ensure_busybox_macos().

setup_busybox() {
  local log_prefix="${1:-agent-rootfs}"
  if [[ "$IS_CROSS_BUILD" == "false" ]]; then
    export BUSYBOX="${BUSYBOX:-/usr/bin/busybox}"
    if [[ ! -f "$BUSYBOX" ]]; then
      echo "[$log_prefix] WARNING: busybox not found at $BUSYBOX; guest will have no /bin/sh"
      unset BUSYBOX
    fi
  fi
}

# ── Kernel module pinning ──────────────────────────────────────────────────
# In CI or when VOID_BOX_PINNED_KMODS=1, extract the pinned kernel version
# from download_kernel.sh and export VOID_BOX_KMOD_VERSION / _UPLOAD.
# Otherwise fall back to host kernel modules.

setup_pinned_kernel_modules() {
  local log_prefix="${1:-agent-rootfs}"
  if [[ -n "${VOID_BOX_KMOD_VERSION:-}" ]]; then
    return
  fi
  if [[ "${VOID_BOX_PINNED_KMODS:-0}" == "1" || "${GITHUB_ACTIONS:-}" == "true" ]]; then
    local dl_script="$ROOT_DIR/scripts/download_kernel.sh"
    local dl_kernel_ver dl_kernel_upload
    dl_kernel_ver=$(sed -n 's/^KERNEL_VER="\${KERNEL_VER:-\([^}]*\)}"/\1/p' "$dl_script" 2>/dev/null | head -n 1)
    dl_kernel_upload=$(sed -n 's/^KERNEL_UPLOAD="\${KERNEL_UPLOAD:-\([^}]*\)}"/\1/p' "$dl_script" 2>/dev/null | head -n 1)
    export VOID_BOX_KMOD_VERSION="${dl_kernel_ver:-6.8.0-51}"
    export VOID_BOX_KMOD_UPLOAD="${dl_kernel_upload:-52}"
    echo "[$log_prefix] Using pinned kernel modules: ${VOID_BOX_KMOD_VERSION} (upload ${VOID_BOX_KMOD_UPLOAD})"
  else
    echo "[$log_prefix] Using host kernel modules for local build (uname -r=$(uname -r))"
  fi
}

# ── Sandbox user (uid 1000) ──────────────────────────────────────────────────
# Claude Code refuses --dangerously-skip-permissions when running as root, so
# the guest-agent drops privileges before exec-ing the agent binary. The same
# user works for codex and any future agent that doesn't need root.

install_sandbox_user() {
  local out_dir="$1"
  mkdir -p "$out_dir/etc" "$out_dir/home/sandbox"
  cat > "$out_dir/etc/passwd" << 'PASSWD'
root:x:0:0:root:/root:/bin/sh
sandbox:x:1000:1000:sandbox:/home/sandbox:/bin/sh
PASSWD
  cat > "$out_dir/etc/group" << 'GROUP'
root:x:0:
sandbox:x:1000:
GROUP
}

# ── Download artifact helpers ────────────────────────────────────────────────

find_extracted_executable() {
  local search_dir="$1"
  local candidate

  while IFS= read -r candidate; do
    if [[ -x "$candidate" ]]; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done < <(
    find "$search_dir" -type f \
      ! -name '*.tar.gz' ! -name '*.tgz' ! -name '*.tar' \
      ! -name '*.zst' ! -name '*.sigstore' ! -name '*.sig' \
      ! -name '*.sha256' ! -name '*.txt'
  )

  return 1
}

# ── SSL CA certificates ──────────────────────────────────────────────────────
# Install the host CA bundle at the canonical path and create symlinks for
# every common location so that curl, OpenSSL, Bun, etc. all find it
# regardless of which distro compiled them.
# Supports Linux + macOS host paths and optional override via VOID_BOX_CA_BUNDLE
# (or SSL_CERT_FILE).
# Returns 1 if no host CA bundle is found — both Claude and Codex need TLS.

install_ca_certificates() {
  local out_dir="$1"
  local canonical="$out_dir/etc/ssl/certs/ca-certificates.crt"
  mkdir -p "$(dirname "$canonical")"

  local cert_candidates=()
  if [[ -n "${VOID_BOX_CA_BUNDLE:-}" ]]; then
    cert_candidates+=("${VOID_BOX_CA_BUNDLE}")
  fi
  if [[ -n "${SSL_CERT_FILE:-}" ]]; then
    cert_candidates+=("${SSL_CERT_FILE}")
  fi

  # Ordered by precedence:
  # 1. Common Linux distro locations
  # 2. macOS system bundle
  # 3. Homebrew-managed cert bundle paths
  cert_candidates+=(
    /etc/ssl/certs/ca-certificates.crt
    /etc/pki/tls/certs/ca-bundle.crt
    /etc/ssl/certs/ca-bundle.crt
    /etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem
    /etc/ssl/cert.pem
    /private/etc/ssl/cert.pem
    /opt/homebrew/etc/ca-certificates/cert.pem
    /usr/local/etc/ca-certificates/cert.pem
    /opt/homebrew/etc/openssl@3/cert.pem
    /usr/local/etc/openssl@3/cert.pem
  )

  local found=""
  local searched=""
  local cert_path
  for cert_path in "${cert_candidates[@]}"; do
    if [[ -n "$searched" ]]; then
      searched="${searched}, "
    fi
    searched="${searched}${cert_path}"
    if [[ -f "$cert_path" ]]; then
      cp "$cert_path" "$canonical"
      echo "[agent-rootfs] Installed CA certificates from $cert_path"
      found="$cert_path"
      break
    fi
  done

  if [[ -z "$found" ]]; then
    echo "ERROR: no host CA bundle found in any known location" >&2
    echo "  Checked: $searched" >&2
    echo "  Set VOID_BOX_CA_BUNDLE=/path/to/ca-bundle.pem and retry." >&2
    return 1
  fi

  local link_dir
  for link_path in \
    /etc/pki/tls/certs/ca-bundle.crt \
    /etc/ssl/certs/ca-bundle.crt \
    /etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem \
    /etc/ssl/cert.pem \
    ; do
    link_dir="$out_dir$(dirname "$link_path")"
    mkdir -p "$link_dir"
    ln -sf /etc/ssl/certs/ca-certificates.crt "$out_dir$link_path"
  done
}

# ── Initramfs packing ────────────────────────────────────────────────────────

finalize_initramfs() {
  local out_dir="$1"
  local out_cpio="$2"
  echo "[agent-rootfs] Creating initramfs at: $out_cpio"
  ( cd "$out_dir" && find . | cpio -o -H newc | gzip ) > "$out_cpio"

  local final_size
  final_size="$(du -sh "$out_cpio" | awk '{print $1}')"
  local uncompressed_bytes
  uncompressed_bytes="$(gzip -dc "$out_cpio" | wc -c | tr -d ' ')"
  local uncompressed_mb
  uncompressed_mb=$(( (uncompressed_bytes + 1048575) / 1048576 ))
  echo "[agent-rootfs] Done. Initramfs: $out_cpio ($final_size)"
  echo "[agent-rootfs] Uncompressed size: ~${uncompressed_mb} MB — guest RAM must be larger (e.g. voidbox snapshot create --memory 512)."
}
