#!/usr/bin/env bash
set -euo pipefail

# Build a void-box guest initramfs with the OpenAI Codex CLI binary.
#
# This extends the base build_guest_image.sh by bundling:
#   - The codex CLI binary (Rust musl-static, downloaded from GitHub releases)
#   - SSL CA certificates for HTTPS API calls
#   - /etc/passwd + /etc/group for the sandbox user
#
# Codex is musl-static, so no shared libraries need to be copied — unlike
# claude-code, which is a glibc-linked Bun binary.
#
# Prerequisites (one of):
#   1. CODEX_BIN env var pointing to a Linux ELF codex binary
#   2. codex installed locally on PATH (Linux host only)
#   3. CODEX_VERSION set for automatic download (requires curl)
#
# Usage:
#   scripts/build_codex_rootfs.sh
#   CODEX_VERSION=0.118.0 scripts/build_codex_rootfs.sh
#
# Environment variables (all optional):
#   CODEX_BIN       Path to a pre-downloaded codex binary
#   CODEX_VERSION   Version to download (e.g. "0.118.0"); requires curl
#   BUSYBOX         Path to a static busybox (default: /usr/bin/busybox)
#   OUT_DIR         Rootfs staging directory (default: target/void-box-codex-rootfs)
#   OUT_CPIO        Output initramfs path (default: target/void-box-codex.cpio.gz)

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

source "$ROOT_DIR/scripts/lib/agent_rootfs_common.sh"

OUT_DIR="${OUT_DIR:-target/void-box-codex-rootfs}"
OUT_CPIO="${OUT_CPIO:-target/void-box-codex.cpio.gz}"

# ── Step 1: Locate or download the codex binary ──────────────────────────────
CODEX_BIN="${CODEX_BIN:-}"

# Determine guest architecture (matches build_guest_image.sh logic).
HOST_ARCH="$(uname -m)"
case "$HOST_ARCH" in
  arm64) HOST_ARCH="aarch64" ;;
esac
GUEST_ARCH="${ARCH:-$HOST_ARCH}"

# Map guest arch to codex GitHub release asset suffix.
case "$GUEST_ARCH" in
  x86_64)  CODEX_TARGET="x86_64-unknown-linux-musl" ;;
  aarch64) CODEX_TARGET="aarch64-unknown-linux-musl" ;;
  *)       echo "ERROR: unsupported guest architecture: $GUEST_ARCH" >&2; exit 1 ;;
esac

# On macOS the locally installed codex is a Mach-O binary that won't run in
# the Linux guest. Force CODEX_VERSION-based download in that case.
IS_CROSS_BUILD=false
if [[ "$(uname -s)" == "Darwin" ]]; then
  IS_CROSS_BUILD=true
fi

# PATH probe only runs when the user has NOT explicitly requested a specific
# version via CODEX_VERSION. An explicit CODEX_VERSION should take priority
# so the user always gets the requested build even if a stale/wrapper `codex`
# happens to be on PATH (e.g. the npm package ships a Node.js launcher script
# that is not a valid Linux ELF for the guest).
if [[ -z "$CODEX_BIN" && -z "${CODEX_VERSION:-}" && "$IS_CROSS_BUILD" == "false" ]]; then
  LOCAL_CODEX="$(command -v codex 2>/dev/null || true)"
  if [[ -n "$LOCAL_CODEX" && -f "$LOCAL_CODEX" ]]; then
    # Only accept the PATH hit if it's a real ELF binary — skip npm wrapper
    # scripts (codex.js) and other non-native launchers.
    if file -L "$LOCAL_CODEX" 2>/dev/null | grep -q "ELF.*executable"; then
      CODEX_BIN="$(readlink -f "$LOCAL_CODEX")"
    else
      echo "[codex-rootfs] PATH has a non-ELF codex ($LOCAL_CODEX) — skipping; set CODEX_VERSION to download a native build." >&2
    fi
  fi
fi

if [[ -z "$CODEX_BIN" && -n "${CODEX_VERSION:-}" ]]; then
  # Download from openai/codex GitHub releases (Rust line: rust-v<version> tag).
  RELEASE_URL="https://github.com/openai/codex/releases/download/rust-v${CODEX_VERSION}/codex-${CODEX_TARGET}.tar.gz"
  DOWNLOAD_DIR="$ROOT_DIR/target/codex-download"
  mkdir -p "$DOWNLOAD_DIR"
  CACHED_BIN="$DOWNLOAD_DIR/codex-${CODEX_VERSION}-${CODEX_TARGET}"

  if [[ ! -f "$CACHED_BIN" ]]; then
    echo "[codex-rootfs] Downloading codex v${CODEX_VERSION} (${CODEX_TARGET})..."
    TMP_DIR="$(mktemp -d)"
    trap 'rm -rf "$TMP_DIR"' EXIT
    TMP_TAR="$TMP_DIR/codex.tar.gz"
    if ! curl -fSL --progress-bar -o "$TMP_TAR" "$RELEASE_URL"; then
      echo "ERROR: Failed to download codex from $RELEASE_URL" >&2
      echo "  Check that version $CODEX_VERSION exists for $CODEX_TARGET." >&2
      exit 1
    fi
    tar -xzf "$TMP_TAR" -C "$TMP_DIR"
    # The upstream openai/codex release tarball contains a single binary named
    # after the target triple (e.g. codex-x86_64-unknown-linux-musl), not a
    # plain "codex". Match any executable file that isn't the tarball itself
    # or a signature/checksum artifact.
    EXTRACTED_BIN="$(find "$TMP_DIR" -type f -executable \
      ! -name '*.tar.gz' ! -name '*.tgz' ! -name '*.tar' \
      ! -name '*.zst' ! -name '*.sigstore' ! -name '*.sig' \
      ! -name '*.sha256' ! -name '*.txt' \
      | head -1)"
    if [[ -z "$EXTRACTED_BIN" ]]; then
      echo "ERROR: tarball did not contain an executable codex binary" >&2
      ls -laR "$TMP_DIR" >&2
      exit 1
    fi
    cp "$EXTRACTED_BIN" "$CACHED_BIN"
    chmod +x "$CACHED_BIN"
    trap - EXIT
    rm -rf "$TMP_DIR"
  else
    echo "[codex-rootfs] Using cached download: $CACHED_BIN"
  fi
  CODEX_BIN="$CACHED_BIN"
fi

if [[ -z "$CODEX_BIN" || ! -f "$CODEX_BIN" ]]; then
  echo "ERROR: codex binary not found." >&2
  echo "" >&2
  echo "Options:" >&2
  echo "  1. Install codex on your PATH (Linux host only; macOS Mach-O binaries cannot run in the Linux guest)" >&2
  echo "  2. Set CODEX_BIN=/path/to/linux/codex (must be a Linux ELF binary)" >&2
  echo "  3. Set CODEX_VERSION=0.118.0 for automatic download" >&2
  exit 1
fi

# Verify it's an ELF binary (not a Mach-O or shell script).
if ! file -L "$CODEX_BIN" | grep -q "ELF.*executable"; then
  echo "ERROR: $CODEX_BIN is not a native Linux ELF binary." >&2
  echo "  file: $(file -L "$CODEX_BIN")" >&2
  if [[ "$IS_CROSS_BUILD" == "true" ]]; then
    echo "  On macOS, set CODEX_VERSION to download the Linux build:" >&2
    echo "    CODEX_VERSION=0.118.0 scripts/build_codex_rootfs.sh" >&2
  fi
  exit 1
fi

CODEX_SIZE="$(du -sh "$CODEX_BIN" | awk '{print $1}')"
echo "[codex-rootfs] Using codex binary: $CODEX_BIN ($CODEX_SIZE)"

# ── Step 2: Build base image (guest-agent, busybox, kernel modules, codex) ──
if [[ "$IS_CROSS_BUILD" == "false" ]]; then
  export BUSYBOX="${BUSYBOX:-/usr/bin/busybox}"
  if [[ ! -f "$BUSYBOX" ]]; then
    echo "[codex-rootfs] WARNING: busybox not found at $BUSYBOX; guest will have no /bin/sh"
    unset BUSYBOX
  fi
fi

# Pass the codex binary to the base script via CODEX_BIN.
# The base script handles copying it to /usr/local/bin/codex.
export CODEX_BIN
export OUT_DIR OUT_CPIO
echo "[codex-rootfs] Building base guest image..."
bash "$ROOT_DIR/scripts/build_guest_image.sh"

echo "[codex-rootfs] Extending image with CA certificates and sandbox user..."

# ── Step 3: Create sandbox user (uid 1000) ───────────────────────────────────
install_sandbox_user "$OUT_DIR"
echo "[codex-rootfs] Installed sandbox user"

# ── Step 4: Install SSL CA certificates ──────────────────────────────────────
install_ca_certificates "$OUT_DIR"

# ── Step 5: Create final initramfs ───────────────────────────────────────────
finalize_initramfs "$OUT_DIR" "$OUT_CPIO"

echo ""
echo "Usage:"
echo "  OPENAI_API_KEY=sk-... \\"
echo "  VOID_BOX_KERNEL=/boot/vmlinuz-\$(uname -r) \\"
echo "  VOID_BOX_INITRAMFS=$OUT_CPIO \\"
echo "  cargo run --bin voidbox -- run --file examples/specs/codex_workflow_smoke.yaml"
