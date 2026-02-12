#!/usr/bin/env bash
set -euo pipefail

# Build a void-box guest initramfs with real Node.js + @anthropic-ai/claude-code.
#
# This extends the base build_guest_image.sh by bundling:
#   - Node.js binary + all shared libraries (auto-detected via ldd)
#   - @anthropic-ai/claude-code npm package
#   - SSL CA certificates for HTTPS API calls
#   - The dynamic linker (ld-linux)
#
# Prerequisites:
#   - Node.js installed on the host (>= 18)
#   - @anthropic-ai/claude-code installed:
#       npm install --prefix ~/.local @anthropic-ai/claude-code
#     or globally:
#       npm install -g @anthropic-ai/claude-code
#
# Usage:
#   scripts/build_claude_rootfs.sh
#
# Environment variables (all optional):
#   CLAUDE_CODE_DIR   Path to the claude-code package dir
#                     (default: ~/.local/node_modules/@anthropic-ai/claude-code)
#   NODE_BIN          Path to the node binary (default: $(which node))
#   BUSYBOX           Path to a static busybox (default: /usr/bin/busybox)
#   OUT_DIR           Rootfs staging directory (default: target/void-box-rootfs)
#   OUT_CPIO          Output initramfs path (default: target/void-box-rootfs.cpio.gz)

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

OUT_DIR="${OUT_DIR:-target/void-box-rootfs}"
OUT_CPIO="${OUT_CPIO:-target/void-box-rootfs.cpio.gz}"

# ── Step 0: Build base image (guest-agent, busybox, kernel modules) ──────────
# We set BUSYBOX if not already set and the binary exists.
export BUSYBOX="${BUSYBOX:-/usr/bin/busybox}"
if [[ ! -f "$BUSYBOX" ]]; then
  echo "[claude-rootfs] WARNING: busybox not found at $BUSYBOX; guest will have no /bin/sh"
  unset BUSYBOX
fi

# Run the base build (but skip the final cpio step -- we'll do it ourselves)
export OUT_DIR OUT_CPIO
echo "[claude-rootfs] Building base guest image..."
bash "$ROOT_DIR/scripts/build_guest_image.sh"

echo "[claude-rootfs] Extending image with Node.js + claude-code..."

# ── Step 1: Locate Node.js ───────────────────────────────────────────────────
NODE_BIN="${NODE_BIN:-$(which node 2>/dev/null || true)}"
if [[ -z "$NODE_BIN" || ! -f "$NODE_BIN" ]]; then
  echo "ERROR: Node.js not found. Set NODE_BIN=/path/to/node" >&2
  exit 1
fi
# Resolve symlinks
NODE_BIN="$(readlink -f "$NODE_BIN")"
echo "[claude-rootfs] Using Node.js: $NODE_BIN ($(${NODE_BIN} --version))"

# ── Step 2: Locate claude-code package ───────────────────────────────────────
CLAUDE_CODE_DIR="${CLAUDE_CODE_DIR:-}"
if [[ -z "$CLAUDE_CODE_DIR" ]]; then
  # Try common locations
  for candidate in \
    "$HOME/.local/node_modules/@anthropic-ai/claude-code" \
    "/usr/local/lib/node_modules/@anthropic-ai/claude-code" \
    "/usr/lib/node_modules/@anthropic-ai/claude-code" \
    ; do
    if [[ -d "$candidate" && -f "$candidate/cli.js" ]]; then
      CLAUDE_CODE_DIR="$candidate"
      break
    fi
  done
fi

if [[ -z "$CLAUDE_CODE_DIR" || ! -f "$CLAUDE_CODE_DIR/cli.js" ]]; then
  echo "ERROR: @anthropic-ai/claude-code not found." >&2
  echo "Install it:  npm install --prefix ~/.local @anthropic-ai/claude-code" >&2
  echo "Or set CLAUDE_CODE_DIR=/path/to/node_modules/@anthropic-ai/claude-code" >&2
  exit 1
fi
echo "[claude-rootfs] Using claude-code: $CLAUDE_CODE_DIR"

# ── Step 3: Copy Node.js binary ─────────────────────────────────────────────
mkdir -p "$OUT_DIR/usr/bin"
cp "$NODE_BIN" "$OUT_DIR/usr/bin/node"
chmod +x "$OUT_DIR/usr/bin/node"
echo "[claude-rootfs] Installed node at /usr/bin/node"

# ── Step 4: Copy shared libraries (auto-detected via ldd) ───────────────────
mkdir -p "$OUT_DIR/lib64" "$OUT_DIR/usr/lib64"

# Copy the dynamic linker
LINKER="$(ldd "$NODE_BIN" | grep 'ld-linux' | awk '{print $1}')"
if [[ -n "$LINKER" && -f "$LINKER" ]]; then
  cp "$LINKER" "$OUT_DIR/lib64/$(basename "$LINKER")"
  echo "[claude-rootfs] Installed linker: $LINKER"
fi

# Copy all shared libraries
for lib in $(ldd "$NODE_BIN" | awk '{print $3}' | grep -v "^$" | sort -u); do
  if [[ -f "$lib" ]]; then
    cp "$lib" "$OUT_DIR/lib64/$(basename "$lib")"
  fi
done
echo "[claude-rootfs] Installed $(ls "$OUT_DIR/lib64/"*.so* 2>/dev/null | wc -l) shared libraries"

# ── Step 5: Copy claude-code package ─────────────────────────────────────────
GUEST_CC_DIR="$OUT_DIR/usr/lib/node_modules/@anthropic-ai/claude-code"
mkdir -p "$GUEST_CC_DIR"
# Copy the package contents (cli.js, vendor/, etc.)
cp -a "$CLAUDE_CODE_DIR/"* "$GUEST_CC_DIR/"

# Also copy sibling dependencies if they exist (e.g. @img packages)
PARENT_MODULES="$(dirname "$(dirname "$CLAUDE_CODE_DIR")")"
if [[ -d "$PARENT_MODULES/@img" ]]; then
  cp -a "$PARENT_MODULES/@img" "$OUT_DIR/usr/lib/node_modules/"
  echo "[claude-rootfs] Installed @img dependencies"
fi

echo "[claude-rootfs] Installed claude-code ($(du -sh "$GUEST_CC_DIR" | awk '{print $1}'))"

# ── Step 6: Create claude-code wrapper script ────────────────────────────────
# Claude Code refuses --dangerously-skip-permissions when running as root.
# Since the guest-agent (PID 1) is root, we create a non-root "sandbox" user
# and drop privileges in the wrapper using busybox `su`.
mkdir -p "$OUT_DIR/etc" "$OUT_DIR/home/sandbox"
cat > "$OUT_DIR/etc/passwd" << 'PASSWD'
root:x:0:0:root:/root:/bin/sh
sandbox:x:1000:1000:sandbox:/home/sandbox:/bin/sh
PASSWD
cat > "$OUT_DIR/etc/group" << 'GROUP'
root:x:0:
sandbox:x:1000:
GROUP

# The wrapper is a simple node invocation.
# Privilege dropping (from root to sandbox user) is handled by the guest-agent
# before exec-ing this script, so the wrapper itself just runs node.
cat > "$OUT_DIR/usr/local/bin/claude-code" << 'WRAPPER'
#!/bin/sh
export HOME=/home/sandbox
export NODE_EXTRA_CA_CERTS=/etc/ssl/certs/ca-certificates.crt
export NODE_NO_WARNINGS=1
export PATH=/usr/local/bin:/usr/bin:/bin
exec /usr/bin/node /usr/lib/node_modules/@anthropic-ai/claude-code/cli.js "$@"
WRAPPER
chmod +x "$OUT_DIR/usr/local/bin/claude-code"

# Also create a 'claude' alias
ln -sf claude-code "$OUT_DIR/usr/local/bin/claude"
echo "[claude-rootfs] Installed /usr/local/bin/claude-code wrapper (drops to sandbox user)"

# ── Step 7: Install SSL CA certificates ──────────────────────────────────────
mkdir -p "$OUT_DIR/etc/ssl/certs"
for cert_path in \
  /etc/ssl/certs/ca-certificates.crt \
  /etc/pki/tls/certs/ca-bundle.crt \
  /etc/ssl/certs/ca-bundle.crt \
  ; do
  if [[ -f "$cert_path" ]]; then
    cp "$cert_path" "$OUT_DIR/etc/ssl/certs/ca-certificates.crt"
    echo "[claude-rootfs] Installed CA certificates from $cert_path"
    break
  fi
done

# ── Step 8: Create final initramfs ───────────────────────────────────────────
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
