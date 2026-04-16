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
#     (on macOS, version is auto-detected from local install)
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

LOG_PREFIX="agents-rootfs"
OUT_DIR="${OUT_DIR:-target/void-box-agents-rootfs}"
OUT_CPIO="${OUT_CPIO:-target/void-box-agents.cpio.gz}"

# ── Step 1: Locate or download both binaries ─────────────────────────────────
detect_guest_arch
resolve_claude_binary "$LOG_PREFIX"
resolve_codex_binary "$LOG_PREFIX"

# ── Step 2: Build base image with both binaries ──────────────────────────────
setup_busybox "$LOG_PREFIX"
setup_pinned_kernel_modules "$LOG_PREFIX"

export CLAUDE_CODE_BIN="$CLAUDE_BIN"
export CODEX_BIN
export OUT_DIR OUT_CPIO
echo "[$LOG_PREFIX] Building base guest image with both claude-code and codex..."
bash "$ROOT_DIR/scripts/build_guest_image.sh"

echo "[$LOG_PREFIX] Extending image with CA certificates and sandbox user..."

# ── Step 3: Create sandbox user (uid 1000) ──────────────────────────────────
install_sandbox_user "$OUT_DIR"

# Create 'claude' symlink (base script installs as 'claude-code')
ln -sf claude-code "$OUT_DIR/usr/local/bin/claude"
echo "[$LOG_PREFIX] Installed /usr/local/bin/claude symlink"

# ── Step 4: Install SSL CA certificates ─────────────────────────────────────
install_ca_certificates "$OUT_DIR"

# ── Step 5: Create final initramfs ──────────────────────────────────────────
finalize_initramfs "$OUT_DIR" "$OUT_CPIO"

echo ""
echo "Combined agents image ready: $OUT_CPIO"
echo "  /usr/local/bin/claude-code  (Claude Code)"
echo "  /usr/local/bin/codex        (OpenAI Codex)"
