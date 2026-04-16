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
#     3. CLAUDE_CODE_VERSION set for automatic download (requires curl)
#     4. On macOS: version auto-detected from local install
#
# Usage:
#   scripts/build_claude_rootfs.sh
#
# Environment variables (all optional):
#   CLAUDE_BIN            Path to a pre-downloaded native claude binary
#   CLAUDE_CODE_VERSION   Version to download (e.g. "2.1.45"); requires curl
#   BUSYBOX              Path to a static busybox (default: /usr/bin/busybox)
#   OUT_DIR              Rootfs staging directory (default: target/void-box-claude-rootfs)
#   OUT_CPIO             Output initramfs path (default: target/void-box-claude.cpio.gz)
#   VOID_BOX_CA_BUNDLE   Optional host CA bundle override (PEM file)

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"
source "$ROOT_DIR/scripts/lib/agent_rootfs_common.sh"

LOG_PREFIX="claude-rootfs"
OUT_DIR="${OUT_DIR:-target/void-box-claude-rootfs}"
OUT_CPIO="${OUT_CPIO:-target/void-box-claude.cpio.gz}"

# ── Step 1: Locate or download the native claude binary ──────────────────────
detect_guest_arch
resolve_claude_binary "$LOG_PREFIX"

# ── Step 2: Build base image (guest-agent, busybox, kernel modules) ──────────
setup_busybox "$LOG_PREFIX"
setup_pinned_kernel_modules "$LOG_PREFIX"

export CLAUDE_CODE_BIN="$CLAUDE_BIN"
export OUT_DIR OUT_CPIO
echo "[$LOG_PREFIX] Building base guest image..."
bash "$ROOT_DIR/scripts/build_guest_image.sh"

echo "[$LOG_PREFIX] Extending image with CA certificates and sandbox user..."

# ── Step 3: Create sandbox user (uid 1000) ───────────────────────────────────
install_sandbox_user "$OUT_DIR"
echo "[$LOG_PREFIX] Installed sandbox user"

# Create 'claude' symlink (base script installs as 'claude-code')
ln -sf claude-code "$OUT_DIR/usr/local/bin/claude"
echo "[$LOG_PREFIX] Installed /usr/local/bin/claude symlink"

# ── Step 4: Install SSL CA certificates ──────────────────────────────────────
install_ca_certificates "$OUT_DIR"

# ── Step 5: Create final initramfs ───────────────────────────────────────────
finalize_initramfs "$OUT_DIR" "$OUT_CPIO"

echo ""
echo "Usage:"
echo "  ANTHROPIC_API_KEY=sk-ant-... \\"
echo "  VOID_BOX_KERNEL=/boot/vmlinuz-\$(uname -r) \\"
echo "  VOID_BOX_INITRAMFS=$OUT_CPIO \\"
echo "  cargo run --example claude_in_voidbox_example"
