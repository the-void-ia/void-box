#!/usr/bin/env bash
set -euo pipefail

# Build a void-box guest rootfs and initramfs.
# Includes: init, guest-agent, optional claude-code, optional busybox.
#
# Usage:
#   scripts/build_guest_image.sh
#   OUT_DIR=/tmp/rootfs OUT_CPIO=/tmp/root.cpio.gz scripts/build_guest_image.sh
#   BUSYBOX=/path/to/busybox scripts/build_guest_image.sh
#
# Requires: cpio, gzip. Optional: BUSYBOX for /bin/sh and basic tools.

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

SCRIPT_DIR="$ROOT_DIR/scripts/lib"
source "$SCRIPT_DIR/guest_common.sh"

OUT_DIR="${OUT_DIR:-/tmp/void-box-rootfs}"
OUT_CPIO="${OUT_CPIO:-/tmp/void-box-rootfs.cpio.gz}"

# ── Architecture detection ────────────────────────────────────────────────────

HOST_ARCH="$(uname -m)"
case "$HOST_ARCH" in
  arm64) HOST_ARCH="aarch64" ;;
esac
ARCH="${ARCH:-$HOST_ARCH}"

case "$ARCH" in
  x86_64)  GUEST_TARGET="x86_64-unknown-linux-musl" ;;
  aarch64) GUEST_TARGET="aarch64-unknown-linux-musl" ;;
  *)       echo "[void-box] ERROR: unsupported architecture: $ARCH"; exit 1 ;;
esac

# ── Platform detection & platform-specific setup ──────────────────────────────

HOST_OS="$(uname -s)"

if [[ "$HOST_OS" == "Darwin" ]]; then
  source "$SCRIPT_DIR/guest_macos.sh"
  setup_cross_linker
else
  source "$SCRIPT_DIR/guest_linux.sh"
fi

# ── Build guest binaries ──────────────────────────────────────────────────────

echo "[void-box] Building guest-agent (release, static, target=$GUEST_TARGET, arch=$ARCH)..."
cargo build --release -p guest-agent --target "$GUEST_TARGET"
GUEST_AGENT_BIN="target/$GUEST_TARGET/release/guest-agent"

echo "[void-box] Building void-message (release, static, target=$GUEST_TARGET)..."
cargo build --release -p void-message --target "$GUEST_TARGET"
VOID_MESSAGE_BIN="target/$GUEST_TARGET/release/void-message"

echo "[void-box] Building void-mcp (release, static, target=$GUEST_TARGET)..."
cargo build --release -p void-mcp --target "$GUEST_TARGET"
VOID_MCP_BIN="target/$GUEST_TARGET/release/void-mcp"

# ── Assemble rootfs ──────────────────────────────────────────────────────────

prepare_rootfs
install_dhcp_script
install_guest_agent "$GUEST_AGENT_BIN"

# Sidecar messaging tools
echo "[void-box] Installing void-message CLI at /usr/local/bin/void-message..."
cp "$VOID_MESSAGE_BIN" "$OUT_DIR/usr/local/bin/void-message"
chmod +x "$OUT_DIR/usr/local/bin/void-message"

echo "[void-box] Installing void-mcp MCP bridge at /usr/local/bin/void-mcp..."
cp "$VOID_MCP_BIN" "$OUT_DIR/usr/local/bin/void-mcp"
chmod +x "$OUT_DIR/usr/local/bin/void-mcp"

# Claude-code: install binary, then platform-specific shared libraries
if install_claude_code_binary; then
  if [[ "$HOST_OS" == "Darwin" ]]; then
    install_claude_code_libs_macos
  else
    install_claude_code_libs_linux
  fi
fi

# Codex CLI: install binary if CODEX_BIN is set (musl-static, no libs needed)
install_codex_binary || true

if [[ "$HOST_OS" == "Darwin" ]]; then
  ensure_busybox_macos
fi
install_busybox

# ── Platform-specific extras (host binaries, kernel modules, gh) ──────────────

if [[ "$HOST_OS" == "Darwin" ]]; then
  echo "[void-box] Cross-build detected (macOS → Linux): skipping host binary installation."
  download_gh_cli
  install_kernel_modules_macos
else
  install_host_binaries
  install_kernel_modules_linux
fi

# ── Pack ──────────────────────────────────────────────────────────────────────

pack_initramfs
