#!/usr/bin/env bash
set -euo pipefail

# Build a void-box guest initramfs with the native claude-code binary.
#
# This extends the base build_guest_image.sh by bundling:
#   - The native claude-code binary (Bun single-executable application)
#   - Its glibc shared libraries (auto-detected via ldd)
#   - SSL CA certificates for HTTPS API calls
#   - /etc/passwd + /etc/group for the sandbox user
#
# The native binary replaces the previous Node.js + npm approach. It uses
# Bun/JavaScriptCore instead of Node.js/V8, has its own HTTP client, and
# is the official distribution method.
#
# Prerequisites:
#   - One of the following (checked in order):
#     1. CLAUDE_BIN env var pointing to a native claude binary
#     2. claude installed locally (~/.local/bin/claude or on PATH)
#     3. CLAUDE_CODE_VERSION set for automatic download (requires curl+jq)
#
# Usage:
#   scripts/build_claude_rootfs.sh
#
# Environment variables (all optional):
#   CLAUDE_BIN            Path to a pre-downloaded native claude binary
#   CLAUDE_CODE_VERSION   Version to download (e.g. "2.1.45"); requires curl
#   BUSYBOX              Path to a static busybox (default: /usr/bin/busybox)
#   OUT_DIR              Rootfs staging directory (default: target/void-box-rootfs)
#   OUT_CPIO             Output initramfs path (default: target/void-box-rootfs.cpio.gz)

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

OUT_DIR="${OUT_DIR:-target/void-box-rootfs}"
OUT_CPIO="${OUT_CPIO:-target/void-box-rootfs.cpio.gz}"

# ── Step 1: Locate or download the native claude binary ──────────────────────
CLAUDE_BIN="${CLAUDE_BIN:-}"

# Determine guest architecture (matches build_guest_image.sh logic).
HOST_ARCH="$(uname -m)"
case "$HOST_ARCH" in
  arm64) HOST_ARCH="aarch64" ;;
esac
GUEST_ARCH="${ARCH:-$HOST_ARCH}"

# Map guest arch to claude-code download platform string.
case "$GUEST_ARCH" in
  x86_64)  CLAUDE_PLATFORM="linux-x64" ;;
  aarch64) CLAUDE_PLATFORM="linux-arm64" ;;
  *)       echo "ERROR: unsupported guest architecture: $GUEST_ARCH" >&2; exit 1 ;;
esac

# On macOS the locally installed claude binary is a Mach-O executable that
# cannot run inside the Linux guest VM. We must obtain a Linux build.
IS_CROSS_BUILD=false
if [[ "$(uname -s)" == "Darwin" ]]; then
  IS_CROSS_BUILD=true
fi

if [[ -z "$CLAUDE_BIN" && "$IS_CROSS_BUILD" == "false" ]]; then
  # Try locally installed binary (only useful when host == guest OS)
  for candidate in \
    "$HOME/.local/bin/claude" \
    "$(command -v claude 2>/dev/null || true)" \
    ; do
    if [[ -n "$candidate" && -f "$candidate" ]]; then
      CLAUDE_BIN="$(readlink -f "$candidate")"
      break
    fi
  done
fi

# Auto-detect version from local install for downloading the Linux build.
if [[ -z "$CLAUDE_BIN" && -z "${CLAUDE_CODE_VERSION:-}" && "$IS_CROSS_BUILD" == "true" ]]; then
  LOCAL_CLAUDE="$(command -v claude 2>/dev/null || true)"
  if [[ -n "$LOCAL_CLAUDE" ]]; then
    # Extract bare version number (e.g. "2.0.76" from "2.0.76 (Claude Code)")
    DETECTED_VER="$("$LOCAL_CLAUDE" --version 2>/dev/null | head -1 | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1 || true)"
    if [[ -n "$DETECTED_VER" ]]; then
      echo "[claude-rootfs] macOS detected — will download Linux build of claude-code v${DETECTED_VER}"
      CLAUDE_CODE_VERSION="$DETECTED_VER"
    fi
  fi
fi

if [[ -z "$CLAUDE_BIN" && -n "${CLAUDE_CODE_VERSION:-}" ]]; then
  # Download the Linux build from GCS
  GCS_BASE="https://storage.googleapis.com/claude-code-dist-86c565f3-f756-42ad-8dfa-d59b1c096819/claude-code-releases"
  DOWNLOAD_URL="$GCS_BASE/$CLAUDE_CODE_VERSION/$CLAUDE_PLATFORM/claude"
  DOWNLOAD_DIR="$ROOT_DIR/target/claude-download"
  mkdir -p "$DOWNLOAD_DIR"
  CLAUDE_BIN="$DOWNLOAD_DIR/claude-${CLAUDE_CODE_VERSION}-${CLAUDE_PLATFORM}"

  if [[ ! -f "$CLAUDE_BIN" ]]; then
    echo "[claude-rootfs] Downloading claude-code v${CLAUDE_CODE_VERSION} (${CLAUDE_PLATFORM})..."
    if ! curl -fSL --progress-bar -o "$CLAUDE_BIN" "$DOWNLOAD_URL"; then
      echo "ERROR: Failed to download claude-code from $DOWNLOAD_URL" >&2
      echo "  Check that version $CLAUDE_CODE_VERSION exists for $CLAUDE_PLATFORM." >&2
      rm -f "$CLAUDE_BIN"
      exit 1
    fi
    chmod +x "$CLAUDE_BIN"
  else
    echo "[claude-rootfs] Using cached download: $CLAUDE_BIN"
  fi
fi

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

# Verify it's an ELF binary (not a Mach-O or shell script)
if ! file -L "$CLAUDE_BIN" | grep -q "ELF.*executable"; then
  echo "ERROR: $CLAUDE_BIN is not a native Linux ELF binary." >&2
  echo "  file: $(file -L "$CLAUDE_BIN")" >&2
  if [[ "$IS_CROSS_BUILD" == "true" ]]; then
    echo "  On macOS, set CLAUDE_CODE_VERSION to download the Linux build:" >&2
    echo "    CLAUDE_CODE_VERSION=2.0.76 scripts/build_claude_rootfs.sh" >&2
  else
    echo "  Make sure you have the native claude-code binary (not the npm wrapper)." >&2
  fi
  exit 1
fi

CLAUDE_VERSION="$("$CLAUDE_BIN" --version 2>/dev/null | head -1 || echo "unknown")"
CLAUDE_SIZE="$(du -sh "$CLAUDE_BIN" | awk '{print $1}')"
echo "[claude-rootfs] Using native claude binary: $CLAUDE_BIN ($CLAUDE_SIZE, $CLAUDE_VERSION)"

# ── Step 2: Build base image (guest-agent, busybox, kernel modules) ──────────
# On Linux, default to the system busybox. On macOS, build_guest_image.sh
# auto-downloads a static ARM64 busybox via ensure_busybox_macos().
if [[ "$IS_CROSS_BUILD" == "false" ]]; then
  export BUSYBOX="${BUSYBOX:-/usr/bin/busybox}"
  if [[ ! -f "$BUSYBOX" ]]; then
    echo "[claude-rootfs] WARNING: busybox not found at $BUSYBOX; guest will have no /bin/sh"
    unset BUSYBOX
  fi
fi

# Kernel module source policy:
# - Local/dev default: use host modules (VOID_BOX_KMOD_VERSION unset).
# - CI/pinned-kernel flows: opt in via VOID_BOX_PINNED_KMODS=1 (or GITHUB_ACTIONS=true).
if [[ -z "${VOID_BOX_KMOD_VERSION:-}" ]]; then
  if [[ "${VOID_BOX_PINNED_KMODS:-0}" == "1" || "${GITHUB_ACTIONS:-}" == "true" ]]; then
    # Extract pinned KERNEL_VER and KERNEL_UPLOAD from download_kernel.sh.
    _DL_SCRIPT="$ROOT_DIR/scripts/download_kernel.sh"
    _DL_KERNEL_VER=$(grep -oP '(?<=^KERNEL_VER="\$\{KERNEL_VER:-)[^}]+' "$_DL_SCRIPT" 2>/dev/null || true)
    _DL_KERNEL_UPLOAD=$(grep -oP '(?<=^KERNEL_UPLOAD="\$\{KERNEL_UPLOAD:-)[^}]+' "$_DL_SCRIPT" 2>/dev/null || true)
    export VOID_BOX_KMOD_VERSION="${_DL_KERNEL_VER:-6.8.0-51}"
    export VOID_BOX_KMOD_UPLOAD="${_DL_KERNEL_UPLOAD:-52}"
    echo "[claude-rootfs] Using pinned kernel modules: ${VOID_BOX_KMOD_VERSION} (upload ${VOID_BOX_KMOD_UPLOAD})"
  else
    echo "[claude-rootfs] Using host kernel modules for local build (uname -r=$(uname -r))"
  fi
fi

# Pass the claude binary to the base script via CLAUDE_CODE_BIN.
# The base script handles copying it to /usr/local/bin/claude-code and
# running ldd to install shared libraries into the initramfs.
export CLAUDE_CODE_BIN="$CLAUDE_BIN"
export OUT_DIR OUT_CPIO
echo "[claude-rootfs] Building base guest image..."
bash "$ROOT_DIR/scripts/build_guest_image.sh"

echo "[claude-rootfs] Extending image with CA certificates and sandbox user..."

# ── Step 3: Create sandbox user (passwd + group) ─────────────────────────────
# Claude Code refuses --dangerously-skip-permissions when running as root.
# The guest-agent drops privileges to uid 1000 before exec-ing claude-code.
mkdir -p "$OUT_DIR/etc" "$OUT_DIR/home/sandbox"
cat > "$OUT_DIR/etc/passwd" << 'PASSWD'
root:x:0:0:root:/root:/bin/sh
sandbox:x:1000:1000:sandbox:/home/sandbox:/bin/sh
PASSWD
cat > "$OUT_DIR/etc/group" << 'GROUP'
root:x:0:
sandbox:x:1000:
GROUP

# Create 'claude' symlink (base script installs as 'claude-code')
ln -sf claude-code "$OUT_DIR/usr/local/bin/claude"
echo "[claude-rootfs] Installed sandbox user and /usr/local/bin/claude symlink"

# ── Step 4: Install SSL CA certificates ──────────────────────────────────────
# Install the CA bundle at the canonical path and create symlinks for every
# common location so that curl, OpenSSL, Bun, etc. all find it regardless
# of which distro compiled them.
CANONICAL_CERT="$OUT_DIR/etc/ssl/certs/ca-certificates.crt"
mkdir -p "$(dirname "$CANONICAL_CERT")"
for cert_path in \
  /etc/ssl/certs/ca-certificates.crt \
  /etc/pki/tls/certs/ca-bundle.crt \
  /etc/ssl/certs/ca-bundle.crt \
  /etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem \
  ; do
  if [[ -f "$cert_path" ]]; then
    cp "$cert_path" "$CANONICAL_CERT"
    echo "[claude-rootfs] Installed CA certificates from $cert_path"
    break
  fi
done

# Symlinks so all common paths resolve to the same bundle
for link_path in \
  /etc/pki/tls/certs/ca-bundle.crt \
  /etc/ssl/certs/ca-bundle.crt \
  /etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem \
  ; do
  link_dir="$OUT_DIR$(dirname "$link_path")"
  mkdir -p "$link_dir"
  ln -sf /etc/ssl/certs/ca-certificates.crt "$OUT_DIR$link_path"
done

# ── Step 5: Create final initramfs ───────────────────────────────────────────
echo "[claude-rootfs] Creating initramfs at: $OUT_CPIO"
( cd "$OUT_DIR" && find . | cpio -o -H newc | gzip ) > "$OUT_CPIO"

FINAL_SIZE="$(du -sh "$OUT_CPIO" | awk '{print $1}')"
echo "[claude-rootfs] Done. Initramfs: $OUT_CPIO ($FINAL_SIZE)"
echo ""
echo "Usage:"
echo "  ANTHROPIC_API_KEY=sk-ant-... \\"
echo "  VOID_BOX_KERNEL=/boot/vmlinuz-\$(uname -r) \\"
echo "  VOID_BOX_INITRAMFS=$OUT_CPIO \\"
echo "  cargo run --example claude_in_voidbox_example"
