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
#   VOID_BOX_CA_BUNDLE   Optional host CA bundle override (PEM file)

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

source "$ROOT_DIR/scripts/lib/agent_rootfs_common.sh"

LOG_PREFIX="codex-rootfs"
OUT_DIR="${OUT_DIR:-target/void-box-codex-rootfs}"
OUT_CPIO="${OUT_CPIO:-target/void-box-codex.cpio.gz}"

# ── Step 1: Locate or download the codex binary ──────────────────────────────
detect_guest_arch
resolve_codex_binary "$LOG_PREFIX"

# ── Step 2: Build base image (guest-agent, busybox, kernel modules, codex) ──
setup_busybox "$LOG_PREFIX"
setup_pinned_kernel_modules "$LOG_PREFIX"

export CODEX_BIN
export OUT_DIR OUT_CPIO
echo "[$LOG_PREFIX] Building base guest image..."
bash "$ROOT_DIR/scripts/build_guest_image.sh"

echo "[$LOG_PREFIX] Extending image with CA certificates and sandbox user..."

# ── Step 3: Create sandbox user (uid 1000) ───────────────────────────────────
install_sandbox_user "$OUT_DIR"
echo "[$LOG_PREFIX] Installed sandbox user"

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
