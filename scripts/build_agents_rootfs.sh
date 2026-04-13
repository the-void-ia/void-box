#!/usr/bin/env bash
set -euo pipefail

# Build a void-box guest initramfs with BOTH claude-code and codex binaries.
#
# This extends the base build_guest_image.sh by bundling:
#   - The native claude-code binary (Bun single-executable, glibc-linked)
#   - Its glibc shared libraries (auto-detected via ldd)
#   - The codex CLI binary (Rust musl-static, no shared libs needed)
#   - SSL CA certificates for HTTPS API calls
#   - /etc/passwd + /etc/group for the sandbox user
#
# For users who want a single image that works with any provider.
#
# Prerequisites:
#   - Claude: CLAUDE_BIN, local install, or CLAUDE_CODE_VERSION
#   - Codex: CODEX_BIN, local install, or CODEX_VERSION
#   (Same discovery as build_claude_rootfs.sh / build_codex_rootfs.sh)
#
# Usage:
#   scripts/build_agents_rootfs.sh
#   CLAUDE_CODE_VERSION=2.1.53 CODEX_VERSION=0.118.0 scripts/build_agents_rootfs.sh
#
# Environment variables (all optional):
#   CLAUDE_BIN            Path to a pre-downloaded native claude binary
#   CLAUDE_CODE_VERSION   Version to download (e.g. "2.1.53"); requires curl
#   CODEX_BIN             Path to a pre-downloaded codex binary
#   CODEX_VERSION         Version to download (e.g. "0.118.0"); requires curl
#   BUSYBOX               Path to a static busybox (default: /usr/bin/busybox)
#   OUT_DIR               Rootfs staging directory (default: target/void-box-agents-rootfs)
#   OUT_CPIO              Output initramfs path (default: target/void-box-agents.cpio.gz)

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"
source "$ROOT_DIR/scripts/lib/agent_rootfs_common.sh"

OUT_DIR="${OUT_DIR:-target/void-box-agents-rootfs}"
OUT_CPIO="${OUT_CPIO:-target/void-box-agents.cpio.gz}"

# ── Architecture detection ──────────────────────────────────────────────────
HOST_ARCH="$(uname -m)"
case "$HOST_ARCH" in
  arm64) HOST_ARCH="aarch64" ;;
esac
GUEST_ARCH="${ARCH:-$HOST_ARCH}"

IS_CROSS_BUILD=false
if [[ "$(uname -s)" == "Darwin" ]]; then
  IS_CROSS_BUILD=true
fi

# ── Step 1a: Locate or download the claude binary ───────────────────────────
CLAUDE_BIN="${CLAUDE_BIN:-}"

case "$GUEST_ARCH" in
  x86_64)  CLAUDE_PLATFORM="linux-x64" ;;
  aarch64) CLAUDE_PLATFORM="linux-arm64" ;;
  *)       echo "ERROR: unsupported guest architecture: $GUEST_ARCH" >&2; exit 1 ;;
esac

if [[ -z "$CLAUDE_BIN" && -z "${CLAUDE_CODE_VERSION:-}" && "$IS_CROSS_BUILD" == "false" ]]; then
  for candidate in "$HOME/.local/bin/claude" "$(command -v claude 2>/dev/null || true)"; do
    if [[ -n "$candidate" && -f "$candidate" ]] && file -L "$candidate" 2>/dev/null | grep -q "ELF.*executable"; then
      CLAUDE_BIN="$(readlink -f "$candidate")"
      break
    fi
  done
fi

if [[ -z "$CLAUDE_BIN" && -n "${CLAUDE_CODE_VERSION:-}" ]]; then
  DOWNLOAD_URL="https://storage.googleapis.com/claude-code-dist-86c565f3-f756-42ad-8dfa-d59b1c096819/claude-code-releases/${CLAUDE_CODE_VERSION}/${CLAUDE_PLATFORM}/claude"
  CLAUDE_BIN="$ROOT_DIR/target/claude-code-${CLAUDE_CODE_VERSION}-${CLAUDE_PLATFORM}"
  if [[ ! -f "$CLAUDE_BIN" ]]; then
    echo "[agents-rootfs] Downloading claude-code ${CLAUDE_CODE_VERSION} (${CLAUDE_PLATFORM})..."
    if ! curl -fSL --progress-bar -o "$CLAUDE_BIN" "$DOWNLOAD_URL"; then
      echo "ERROR: Failed to download claude-code from $DOWNLOAD_URL" >&2
      rm -f "$CLAUDE_BIN"
      exit 1
    fi
    chmod +x "$CLAUDE_BIN"
  else
    echo "[agents-rootfs] Using cached claude download: $CLAUDE_BIN"
  fi
fi

if [[ -z "$CLAUDE_BIN" || ! -f "$CLAUDE_BIN" ]]; then
  echo "ERROR: Native claude binary not found." >&2
  echo "  Set CLAUDE_BIN or CLAUDE_CODE_VERSION." >&2
  exit 1
fi

if ! file -L "$CLAUDE_BIN" | grep -q "ELF.*executable"; then
  echo "ERROR: $CLAUDE_BIN is not a native Linux ELF binary." >&2
  exit 1
fi

echo "[agents-rootfs] Claude binary: $CLAUDE_BIN ($(du -sh "$CLAUDE_BIN" | awk '{print $1}'))"

# ── Step 1b: Locate or download the codex binary ───────────────────────────
CODEX_BIN="${CODEX_BIN:-}"

case "$GUEST_ARCH" in
  x86_64)  CODEX_TARGET="x86_64-unknown-linux-musl" ;;
  aarch64) CODEX_TARGET="aarch64-unknown-linux-musl" ;;
esac

if [[ -z "$CODEX_BIN" && -z "${CODEX_VERSION:-}" && "$IS_CROSS_BUILD" == "false" ]]; then
  LOCAL_CODEX="$(command -v codex 2>/dev/null || true)"
  if [[ -n "$LOCAL_CODEX" && -f "$LOCAL_CODEX" ]] && file -L "$LOCAL_CODEX" 2>/dev/null | grep -q "ELF.*executable"; then
    CODEX_BIN="$(readlink -f "$LOCAL_CODEX")"
  fi
fi

if [[ -z "$CODEX_BIN" && -n "${CODEX_VERSION:-}" ]]; then
  RELEASE_URL="https://github.com/openai/codex/releases/download/rust-v${CODEX_VERSION}/codex-${CODEX_TARGET}.tar.gz"
  DOWNLOAD_DIR="$ROOT_DIR/target/codex-download"
  mkdir -p "$DOWNLOAD_DIR"
  CACHED_BIN="$DOWNLOAD_DIR/codex-${CODEX_VERSION}-${CODEX_TARGET}"

  if [[ ! -f "$CACHED_BIN" ]]; then
    echo "[agents-rootfs] Downloading codex v${CODEX_VERSION} (${CODEX_TARGET})..."
    TMP_DIR="$(mktemp -d)"
    trap 'rm -rf "$TMP_DIR"' EXIT
    if ! curl -fSL --progress-bar -o "$TMP_DIR/codex.tar.gz" "$RELEASE_URL"; then
      echo "ERROR: Failed to download codex from $RELEASE_URL" >&2
      exit 1
    fi
    tar -xzf "$TMP_DIR/codex.tar.gz" -C "$TMP_DIR"
    EXTRACTED_BIN="$(find "$TMP_DIR" -type f -executable \
      ! -name '*.tar.gz' ! -name '*.tgz' ! -name '*.tar' \
      ! -name '*.zst' ! -name '*.sigstore' ! -name '*.sig' \
      ! -name '*.sha256' ! -name '*.txt' \
      | head -1)"
    if [[ -z "$EXTRACTED_BIN" ]]; then
      echo "ERROR: tarball did not contain an executable codex binary" >&2
      exit 1
    fi
    cp "$EXTRACTED_BIN" "$CACHED_BIN"
    chmod +x "$CACHED_BIN"
    trap - EXIT
    rm -rf "$TMP_DIR"
  else
    echo "[agents-rootfs] Using cached codex download: $CACHED_BIN"
  fi
  CODEX_BIN="$CACHED_BIN"
fi

if [[ -z "$CODEX_BIN" || ! -f "$CODEX_BIN" ]]; then
  echo "ERROR: codex binary not found." >&2
  echo "  Set CODEX_BIN or CODEX_VERSION." >&2
  exit 1
fi

if ! file -L "$CODEX_BIN" | grep -q "ELF.*executable"; then
  echo "ERROR: $CODEX_BIN is not a native Linux ELF binary." >&2
  exit 1
fi

echo "[agents-rootfs] Codex binary: $CODEX_BIN ($(du -sh "$CODEX_BIN" | awk '{print $1}'))"

# ── Step 2: Build base image with both binaries ────────────────────────────
if [[ "$IS_CROSS_BUILD" == "false" ]]; then
  export BUSYBOX="${BUSYBOX:-/usr/bin/busybox}"
  if [[ ! -f "$BUSYBOX" ]]; then
    echo "[agents-rootfs] WARNING: busybox not found at $BUSYBOX; guest will have no /bin/sh"
    unset BUSYBOX
  fi
fi

# Kernel module source policy (same as claude rootfs).
if [[ -z "${VOID_BOX_KMOD_VERSION:-}" ]]; then
  if [[ "${VOID_BOX_PINNED_KMODS:-0}" == "1" || "${GITHUB_ACTIONS:-}" == "true" ]]; then
    _DL_SCRIPT="$ROOT_DIR/scripts/download_kernel.sh"
    _DL_KERNEL_VER=$(grep -oP '(?<=^KERNEL_VER="\$\{KERNEL_VER:-)[^}]+' "$_DL_SCRIPT" 2>/dev/null || true)
    _DL_KERNEL_UPLOAD=$(grep -oP '(?<=^KERNEL_UPLOAD="\$\{KERNEL_UPLOAD:-)[^}]+' "$_DL_SCRIPT" 2>/dev/null || true)
    export VOID_BOX_KMOD_VERSION="${_DL_KERNEL_VER:-6.8.0-51}"
    export VOID_BOX_KMOD_UPLOAD="${_DL_KERNEL_UPLOAD:-52}"
    echo "[agents-rootfs] Using pinned kernel modules: ${VOID_BOX_KMOD_VERSION}"
  fi
fi

export CLAUDE_CODE_BIN="$CLAUDE_BIN"
export CODEX_BIN
export OUT_DIR OUT_CPIO
echo "[agents-rootfs] Building base guest image with both claude-code and codex..."
bash "$ROOT_DIR/scripts/build_guest_image.sh"

echo "[agents-rootfs] Extending image with CA certificates and sandbox user..."

# ── Step 3: Create sandbox user (uid 1000) ──────────────────────────────────
install_sandbox_user "$OUT_DIR"

# Create 'claude' symlink (base script installs as 'claude-code')
ln -sf claude-code "$OUT_DIR/usr/local/bin/claude"
echo "[agents-rootfs] Installed /usr/local/bin/claude symlink"

# ── Step 4: Install SSL CA certificates ─────────────────────────────────────
install_ca_certificates "$OUT_DIR"

# ── Step 5: Create final initramfs ──────────────────────────────────────────
finalize_initramfs "$OUT_DIR" "$OUT_CPIO"

echo ""
echo "Combined agents image ready: $OUT_CPIO"
echo "  /usr/local/bin/claude-code  (Claude Code)"
echo "  /usr/local/bin/codex        (OpenAI Codex)"
