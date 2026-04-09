#!/usr/bin/env bash
# Shared helpers for agent-flavor rootfs builds (claude, codex, …).
# Sourced by build_<agent>_rootfs.sh — not meant to be run directly.
# Caller owns `set -euo pipefail`.

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

# ── SSL CA certificates ──────────────────────────────────────────────────────
# Install the host CA bundle at the canonical path and create symlinks for
# every common location so that curl, OpenSSL, Bun, etc. all find it
# regardless of which distro compiled them.
# Returns 1 if no host CA bundle is found — both Claude and Codex need TLS.

install_ca_certificates() {
  local out_dir="$1"
  local canonical="$out_dir/etc/ssl/certs/ca-certificates.crt"
  mkdir -p "$(dirname "$canonical")"

  local found=""
  for cert_path in \
    /etc/ssl/certs/ca-certificates.crt \
    /etc/pki/tls/certs/ca-bundle.crt \
    /etc/ssl/certs/ca-bundle.crt \
    /etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem \
    ; do
    if [[ -f "$cert_path" ]]; then
      cp "$cert_path" "$canonical"
      echo "[agent-rootfs] Installed CA certificates from $cert_path"
      found="$cert_path"
      break
    fi
  done

  if [[ -z "$found" ]]; then
    echo "ERROR: no host CA bundle found in any standard location" >&2
    return 1
  fi

  for link_path in \
    /etc/pki/tls/certs/ca-bundle.crt \
    /etc/ssl/certs/ca-bundle.crt \
    /etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem \
    ; do
    local link_dir="$out_dir$(dirname "$link_path")"
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
  local uncompressed_mb=$(( (uncompressed_bytes + 1048575) / 1048576 ))
  echo "[agent-rootfs] Done. Initramfs: $out_cpio ($final_size)"
  echo "[agent-rootfs] Uncompressed size: ~${uncompressed_mb} MB — guest RAM must be larger."
}
