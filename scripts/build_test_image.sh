#!/usr/bin/env bash
set -euo pipefail

# Build a test initramfs for void-box E2E tests.
#
# Differs from build_guest_image.sh in one deliberate way: claude-code is
# replaced by `claudio` — a deterministic mock used by the e2e suites
# (e2e_telemetry, e2e_skill_pipeline, e2e_mount, e2e_service_mode,
# e2e_sidecar). Everything else (guest-agent, void-message, void-mcp,
# busybox, kernel modules) uses the shared helpers in scripts/lib/.
#
# Usage:
#   scripts/build_test_image.sh
#   OUT_CPIO=/tmp/test-root.cpio.gz scripts/build_test_image.sh
#   BUSYBOX=/path/to/busybox scripts/build_test_image.sh
#
# Requires: cpio, gzip. On macOS also: a musl cross-compiler
# (brew install filosottile/musl-cross/musl-cross --with-aarch64).

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

SCRIPT_DIR="$ROOT_DIR/scripts/lib"
source "$SCRIPT_DIR/guest_common.sh"

OUT_DIR="${OUT_DIR:-/tmp/void-box-test-rootfs}"
OUT_CPIO="${OUT_CPIO:-/tmp/void-box-test-rootfs.cpio.gz}"

# ── Architecture detection ────────────────────────────────────────────────────

HOST_ARCH="$(uname -m)"
case "$HOST_ARCH" in
  arm64) HOST_ARCH="aarch64" ;;
esac
ARCH="${ARCH:-$HOST_ARCH}"

case "$ARCH" in
  x86_64)  GUEST_TARGET="x86_64-unknown-linux-musl" ;;
  aarch64) GUEST_TARGET="aarch64-unknown-linux-musl" ;;
  *)       echo "[test-image] ERROR: unsupported architecture: $ARCH"; exit 1 ;;
esac

# ── Platform setup ────────────────────────────────────────────────────────────

HOST_OS="$(uname -s)"

if [[ "$HOST_OS" == "Darwin" ]]; then
  source "$SCRIPT_DIR/guest_macos.sh"
  setup_cross_linker
else
  source "$SCRIPT_DIR/guest_linux.sh"
fi

# ── Build guest binaries ──────────────────────────────────────────────────────

echo "[test-image] Building guest-agent (release, static, target=$GUEST_TARGET)..."
cargo build --release -p guest-agent --target "$GUEST_TARGET"
GUEST_AGENT_BIN="target/$GUEST_TARGET/release/guest-agent"

echo "[test-image] Building claudio (mock claude-code, release, static, target=$GUEST_TARGET)..."
cargo build --release -p claudio --target "$GUEST_TARGET"
CLAUDIO_BIN="target/$GUEST_TARGET/release/claudio"

echo "[test-image] Building void-message (release, static, target=$GUEST_TARGET)..."
cargo build --release -p void-message --target "$GUEST_TARGET"
VOID_MESSAGE_BIN="target/$GUEST_TARGET/release/void-message"

echo "[test-image] Building void-mcp (release, static, target=$GUEST_TARGET)..."
cargo build --release -p void-mcp --target "$GUEST_TARGET"
VOID_MCP_BIN="target/$GUEST_TARGET/release/void-mcp"

# ── Assemble rootfs ──────────────────────────────────────────────────────────

prepare_rootfs
install_guest_agent "$GUEST_AGENT_BIN"

# Mock claude-code — this is the one deliberate divergence from the
# production image. claudio gives deterministic output for e2e tests.
echo "[test-image] Installing claudio as /usr/local/bin/claude-code..."
cp "$CLAUDIO_BIN" "$OUT_DIR/usr/local/bin/claude-code"
chmod +x "$OUT_DIR/usr/local/bin/claude-code"

echo "[test-image] Installing void-message CLI at /usr/local/bin/void-message..."
cp "$VOID_MESSAGE_BIN" "$OUT_DIR/usr/local/bin/void-message"
chmod +x "$OUT_DIR/usr/local/bin/void-message"

echo "[test-image] Installing void-mcp MCP bridge at /usr/local/bin/void-mcp..."
cp "$VOID_MCP_BIN" "$OUT_DIR/usr/local/bin/void-mcp"
chmod +x "$OUT_DIR/usr/local/bin/void-mcp"

if [[ "$HOST_OS" == "Darwin" ]]; then
  ensure_busybox_macos
fi
install_busybox

# ── Kernel modules ────────────────────────────────────────────────────────────

if [[ "$HOST_OS" == "Darwin" ]]; then
  install_kernel_modules_macos
else
  install_kernel_modules_linux
fi

# ── Pack ──────────────────────────────────────────────────────────────────────

pack_initramfs

echo "[test-image] Use with:"
echo "  VOID_BOX_KERNEL=/boot/vmlinuz-\$(uname -r) \\"
echo "  VOID_BOX_INITRAMFS=$OUT_CPIO \\"
echo "  cargo test -- --ignored"
