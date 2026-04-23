#!/usr/bin/env bash
# Shared helpers for agent-flavor rootfs builds (claude, codex, …).
# Sourced by build_<agent>_rootfs.sh — not meant to be run directly.
# Caller owns `set -euo pipefail`.

# Load the pinned-manifest reader (R-B5c.1). This is the only source of truth
# for `version`/`url`/`sha256` tuples consumed by the resolve_* helpers below.
_agent_rootfs_common_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./agent_manifest.sh
source "$_agent_rootfs_common_dir/agent_manifest.sh"

# ── Architecture & cross-build detection ────────────────────────────────────
# Sets: GUEST_ARCH, IS_CROSS_BUILD

detect_guest_arch() {
  local host_arch
  host_arch="$(uname -m)"
  case "$host_arch" in
    arm64) host_arch="aarch64" ;;
  esac
  GUEST_ARCH="${ARCH:-$host_arch}"

  IS_CROSS_BUILD=false
  if [[ "$(uname -s)" == "Darwin" ]]; then
    IS_CROSS_BUILD=true
  fi
}

# ── Claude binary resolution ───────────────────────────────────────────────
# Locates or downloads a Linux ELF claude-code binary, with SHA-256 pinning
# per R-B5c.1.
#
# Reads: CLAUDE_BIN, CLAUDE_CODE_VERSION, CLAUDE_CODE_SHA256 (all optional)
# Requires: GUEST_ARCH, IS_CROSS_BUILD (call detect_guest_arch first)
# Sets: CLAUDE_BIN (absolute path to the resolved binary),
#       CLAUDE_BUILD_PROVENANCE (one of: manifest, override, local-path)
#
# Discovery chain:
#   1. CLAUDE_BIN env var → local dev path; **non-production** (no verify)
#   2. PATH (Linux only) → local dev path; **non-production** (no verify)
#   3. CLAUDE_CODE_VERSION → explicit override; CLAUDE_CODE_SHA256 is required
#      so developers supply the hash they are asking the build to trust
#   4. Default → pull version+url+sha256 from scripts/agents/manifest.toml

resolve_claude_binary() {
  local log_prefix="${1:-agent-rootfs}"
  CLAUDE_BIN="${CLAUDE_BIN:-}"
  CLAUDE_BUILD_PROVENANCE=""

  # Ambiguity guard: CLAUDE_BIN and CLAUDE_CODE_VERSION both claim ownership
  # of the resolution. Refuse rather than silently preferring one.
  if [[ -n "$CLAUDE_BIN" && -n "${CLAUDE_CODE_VERSION:-}" ]]; then
    echo "ERROR: CLAUDE_BIN and CLAUDE_CODE_VERSION are both set — ambiguous resolution." >&2
    echo "  Unset one: CLAUDE_BIN picks a local file (non-production) and" >&2
    echo "  CLAUDE_CODE_VERSION+CLAUDE_CODE_SHA256 picks a verified download." >&2
    return 1
  fi

  local claude_platform
  case "$GUEST_ARCH" in
    x86_64)  claude_platform="linux-x64" ;;
    aarch64) claude_platform="linux-arm64" ;;
    *)       echo "ERROR: unsupported guest architecture: $GUEST_ARCH" >&2; return 1 ;;
  esac

  # 1. CLAUDE_BIN env var → explicit local-dev path.
  if [[ -n "$CLAUDE_BIN" ]]; then
    CLAUDE_BUILD_PROVENANCE="local-path"
    echo "[$log_prefix] WARN: using CLAUDE_BIN=$CLAUDE_BIN without SHA-256 verification — non-production image" >&2
  fi

  # 2. PATH (Linux only, no env-var override in play).
  if [[ -z "$CLAUDE_BIN" && -z "${CLAUDE_CODE_VERSION:-}" && "$IS_CROSS_BUILD" == "false" ]]; then
    local candidate
    for candidate in \
      "$HOME/.local/bin/claude" \
      "$(command -v claude 2>/dev/null || true)" \
      ; do
      if [[ -n "$candidate" && -f "$candidate" ]] \
         && file -L "$candidate" 2>/dev/null | grep -q "ELF.*executable"; then
        CLAUDE_BIN="$(readlink -f "$candidate")"
        CLAUDE_BUILD_PROVENANCE="local-path"
        echo "[$log_prefix] WARN: using local PATH claude ($CLAUDE_BIN) without SHA-256 verification — non-production image" >&2
        break
      fi
    done
  fi

  # 3. CLAUDE_CODE_VERSION override path: must be accompanied by a matching
  #    CLAUDE_CODE_SHA256. This is the R-B5c.1 guard: no silent unverified
  #    fetch. The URL template still comes from the manifest so an override
  #    can't silently point at an attacker-controlled origin.
  if [[ -z "$CLAUDE_BIN" && -n "${CLAUDE_CODE_VERSION:-}" ]]; then
    if [[ -z "${CLAUDE_CODE_SHA256:-}" ]]; then
      echo "ERROR: CLAUDE_CODE_VERSION=$CLAUDE_CODE_VERSION is set without a matching CLAUDE_CODE_SHA256." >&2
      echo "  R-B5c.1 forbids unverified overrides." >&2
      echo "  Supply the SHA-256 of the artifact you want the build to trust:" >&2
      echo "    CLAUDE_CODE_VERSION=$CLAUDE_CODE_VERSION CLAUDE_CODE_SHA256=<hex> $0" >&2
      return 1
    fi

    local manifest_entry pinned_version manifest_url_template
    manifest_entry="$(agent_manifest_require claude-code linux "$GUEST_ARCH")" || return 1
    pinned_version="$(printf '%s\n' "$manifest_entry" | sed -n '1p')"
    manifest_url_template="$(printf '%s\n' "$manifest_entry" | sed -n '2p')"
    if [[ "$CLAUDE_CODE_VERSION" != "$pinned_version" ]]; then
      # The override version is re-templated through the manifest's URL
      # pattern. If upstream changes the URL shape between pins, the
      # override will 404 — make that possibility visible up front.
      echo "[$log_prefix] WARN: CLAUDE_CODE_VERSION=$CLAUDE_CODE_VERSION differs from manifest pin $pinned_version — reusing manifest URL template, which may be stale for the override version." >&2
    fi
    local download_url="${manifest_url_template//\{version\}/$CLAUDE_CODE_VERSION}"
    local download_dir="$ROOT_DIR/target/claude-download"
    mkdir -p "$download_dir"
    CLAUDE_BIN="$download_dir/claude-${CLAUDE_CODE_VERSION}-${claude_platform}"
    _agent_fetch_and_verify "$log_prefix" "$download_url" "$CLAUDE_BIN" "$CLAUDE_CODE_SHA256" || return 1
    chmod +x "$CLAUDE_BIN"
    CLAUDE_BUILD_PROVENANCE="override"
  fi

  # 4. Manifest default: no env-var override, no local binary.
  if [[ -z "$CLAUDE_BIN" ]]; then
    local manifest_entry
    manifest_entry="$(agent_manifest_require claude-code linux "$GUEST_ARCH")" || return 1
    local pinned_version pinned_url_template pinned_sha
    pinned_version="$(printf '%s\n' "$manifest_entry" | sed -n '1p')"
    pinned_url_template="$(printf '%s\n' "$manifest_entry" | sed -n '2p')"
    pinned_sha="$(printf '%s\n' "$manifest_entry" | sed -n '3p')"
    local download_url="${pinned_url_template//\{version\}/$pinned_version}"
    local download_dir="$ROOT_DIR/target/claude-download"
    mkdir -p "$download_dir"
    CLAUDE_BIN="$download_dir/claude-${pinned_version}-${claude_platform}"
    _agent_fetch_and_verify "$log_prefix" "$download_url" "$CLAUDE_BIN" "$pinned_sha" || return 1
    chmod +x "$CLAUDE_BIN"
    CLAUDE_CODE_VERSION="$pinned_version"
    CLAUDE_BUILD_PROVENANCE="manifest"
  fi

  if ! file -L "$CLAUDE_BIN" | grep -q "ELF.*executable"; then
    echo "ERROR: $CLAUDE_BIN is not a native Linux ELF binary." >&2
    echo "  file: $(file -L "$CLAUDE_BIN")" >&2
    return 1
  fi

  local claude_size
  claude_size="$(du -sh "$CLAUDE_BIN" | awk '{print $1}')"
  echo "[$log_prefix] Claude binary: $CLAUDE_BIN ($claude_size) [provenance=${CLAUDE_BUILD_PROVENANCE}]"
}

# ── Codex binary resolution ───────────────────────────────────────────────
# Locates or downloads a Linux ELF codex binary, with SHA-256 pinning per
# R-B5c.1. The pinned sha256 is computed against the *tarball* as it lands on
# disk (the downloaded blob), not against the extracted binary.
#
# Reads: CODEX_BIN, CODEX_VERSION, CODEX_SHA256 (all optional)
# Requires: GUEST_ARCH, IS_CROSS_BUILD (call detect_guest_arch first)
# Sets: CODEX_BIN (absolute path to the resolved binary),
#       CODEX_BUILD_PROVENANCE (one of: manifest, override, local-path)
#
# Discovery chain:
#   1. CODEX_BIN env var → local dev path; **non-production** (no verify)
#   2. PATH (Linux only) → local dev path; **non-production** (no verify)
#   3. CODEX_VERSION → explicit override; CODEX_SHA256 is required so
#      developers supply the hash they are asking the build to trust
#   4. Default → pull version+url+sha256 from scripts/agents/manifest.toml

resolve_codex_binary() {
  local log_prefix="${1:-agent-rootfs}"
  CODEX_BIN="${CODEX_BIN:-}"
  CODEX_BUILD_PROVENANCE=""

  if [[ -n "$CODEX_BIN" && -n "${CODEX_VERSION:-}" ]]; then
    echo "ERROR: CODEX_BIN and CODEX_VERSION are both set — ambiguous resolution." >&2
    echo "  Unset one: CODEX_BIN picks a local file (non-production) and" >&2
    echo "  CODEX_VERSION+CODEX_SHA256 picks a verified download." >&2
    return 1
  fi

  local codex_target
  case "$GUEST_ARCH" in
    x86_64)  codex_target="x86_64-unknown-linux-musl" ;;
    aarch64) codex_target="aarch64-unknown-linux-musl" ;;
    *)       echo "ERROR: unsupported guest architecture: $GUEST_ARCH" >&2; return 1 ;;
  esac

  # 1. CODEX_BIN env var → explicit local-dev path.
  if [[ -n "$CODEX_BIN" ]]; then
    CODEX_BUILD_PROVENANCE="local-path"
    echo "[$log_prefix] WARN: using CODEX_BIN=$CODEX_BIN without SHA-256 verification — non-production image" >&2
  fi

  # 2. PATH (Linux only, no env-var override in play).
  if [[ -z "$CODEX_BIN" && -z "${CODEX_VERSION:-}" && "$IS_CROSS_BUILD" == "false" ]]; then
    local local_codex
    local_codex="$(command -v codex 2>/dev/null || true)"
    if [[ -n "$local_codex" && -f "$local_codex" ]]; then
      if file -L "$local_codex" 2>/dev/null | grep -q "ELF.*executable"; then
        CODEX_BIN="$(readlink -f "$local_codex")"
        CODEX_BUILD_PROVENANCE="local-path"
        echo "[$log_prefix] WARN: using local PATH codex ($CODEX_BIN) without SHA-256 verification — non-production image" >&2
      else
        echo "[$log_prefix] PATH has a non-ELF codex ($local_codex) — skipping; set CODEX_VERSION + CODEX_SHA256 to download a native build." >&2
      fi
    fi
  fi

  # 3. CODEX_VERSION override path: must be accompanied by a matching
  #    CODEX_SHA256 (R-B5c.1 — no silent unverified fetch).
  if [[ -z "$CODEX_BIN" && -n "${CODEX_VERSION:-}" ]]; then
    if [[ -z "${CODEX_SHA256:-}" ]]; then
      echo "ERROR: CODEX_VERSION=$CODEX_VERSION is set without a matching CODEX_SHA256." >&2
      echo "  R-B5c.1 forbids unverified overrides." >&2
      echo "  Supply the SHA-256 of the downloaded tarball you want the build to trust:" >&2
      echo "    CODEX_VERSION=$CODEX_VERSION CODEX_SHA256=<hex> $0" >&2
      return 1
    fi

    local manifest_entry pinned_version manifest_url_template
    manifest_entry="$(agent_manifest_require codex linux "$GUEST_ARCH")" || return 1
    pinned_version="$(printf '%s\n' "$manifest_entry" | sed -n '1p')"
    manifest_url_template="$(printf '%s\n' "$manifest_entry" | sed -n '2p')"
    if [[ "$CODEX_VERSION" != "$pinned_version" ]]; then
      echo "[$log_prefix] WARN: CODEX_VERSION=$CODEX_VERSION differs from manifest pin $pinned_version — reusing manifest URL template, which may be stale for the override version." >&2
    fi
    local download_url="${manifest_url_template//\{version\}/$CODEX_VERSION}"
    _codex_fetch_verify_extract "$log_prefix" "$CODEX_VERSION" "$codex_target" "$download_url" "$CODEX_SHA256" || return 1
    CODEX_BUILD_PROVENANCE="override"
  fi

  # 4. Manifest default.
  if [[ -z "$CODEX_BIN" ]]; then
    local manifest_entry
    manifest_entry="$(agent_manifest_require codex linux "$GUEST_ARCH")" || return 1
    local pinned_version pinned_url_template pinned_sha
    pinned_version="$(printf '%s\n' "$manifest_entry" | sed -n '1p')"
    pinned_url_template="$(printf '%s\n' "$manifest_entry" | sed -n '2p')"
    pinned_sha="$(printf '%s\n' "$manifest_entry" | sed -n '3p')"
    local download_url="${pinned_url_template//\{version\}/$pinned_version}"
    _codex_fetch_verify_extract "$log_prefix" "$pinned_version" "$codex_target" "$download_url" "$pinned_sha" || return 1
    CODEX_VERSION="$pinned_version"
    CODEX_BUILD_PROVENANCE="manifest"
  fi

  if ! file -L "$CODEX_BIN" | grep -q "ELF.*executable"; then
    echo "ERROR: $CODEX_BIN is not a native Linux ELF binary." >&2
    echo "  file: $(file -L "$CODEX_BIN")" >&2
    return 1
  fi

  local codex_size
  codex_size="$(du -sh "$CODEX_BIN" | awk '{print $1}')"
  echo "[$log_prefix] Codex binary: $CODEX_BIN ($codex_size) [provenance=${CODEX_BUILD_PROVENANCE}]"
}

# Internal: download the codex tarball, verify its SHA-256, extract the
# binary, and set CODEX_BIN to the cached extracted path.
_codex_fetch_verify_extract() {
  local log_prefix="$1"
  local version="$2"
  local target="$3"
  local url="$4"
  local expected_sha="$5"

  local download_dir="$ROOT_DIR/target/codex-download"
  mkdir -p "$download_dir"
  local cached_tar="$download_dir/codex-${version}-${target}.tar.gz"
  local cached_bin="$download_dir/codex-${version}-${target}"

  _agent_fetch_and_verify "$log_prefix" "$url" "$cached_tar" "$expected_sha" || return 1

  if [[ ! -x "$cached_bin" ]]; then
    (
      set -euo pipefail
      tmp_dir="$(mktemp -d)"
      trap 'rm -rf "$tmp_dir"' EXIT
      tar -xzf "$cached_tar" -C "$tmp_dir"
      extracted_bin="$(find_extracted_executable "$tmp_dir" codex || true)"
      if [[ -z "$extracted_bin" ]]; then
        echo "ERROR: tarball did not contain an executable codex binary" >&2
        ls -laR "$tmp_dir" >&2
        exit 1
      fi
      cp "$extracted_bin" "$cached_bin"
      chmod +x "$cached_bin"
    ) || return 1
  fi

  CODEX_BIN="$cached_bin"
}

# ── Busybox setup ───────────────────────────────────────────────────────────
# On Linux, defaults to /usr/bin/busybox and warns if missing.
# On macOS, build_guest_image.sh auto-downloads via ensure_busybox_macos().

setup_busybox() {
  local log_prefix="${1:-agent-rootfs}"
  if [[ "$IS_CROSS_BUILD" == "false" ]]; then
    export BUSYBOX="${BUSYBOX:-/usr/bin/busybox}"
    if [[ ! -f "$BUSYBOX" ]]; then
      echo "[$log_prefix] WARNING: busybox not found at $BUSYBOX; guest will have no /bin/sh"
      unset BUSYBOX
    fi
  fi
}

# ── Kernel module pinning ──────────────────────────────────────────────────
# In CI or when VOID_BOX_PINNED_KMODS=1, extract the pinned kernel version
# from download_kernel.sh and export VOID_BOX_KMOD_VERSION / _UPLOAD.
# Otherwise fall back to host kernel modules.

setup_pinned_kernel_modules() {
  local log_prefix="${1:-agent-rootfs}"
  if [[ -n "${VOID_BOX_KMOD_VERSION:-}" ]]; then
    return
  fi
  if [[ "${VOID_BOX_PINNED_KMODS:-0}" == "1" || "${GITHUB_ACTIONS:-}" == "true" ]]; then
    local dl_script="$ROOT_DIR/scripts/download_kernel.sh"
    local dl_kernel_ver dl_kernel_upload
    dl_kernel_ver=$(sed -n 's/^KERNEL_VER="\${KERNEL_VER:-\([^}]*\)}"/\1/p' "$dl_script" 2>/dev/null | head -n 1)
    dl_kernel_upload=$(sed -n 's/^KERNEL_UPLOAD="\${KERNEL_UPLOAD:-\([^}]*\)}"/\1/p' "$dl_script" 2>/dev/null | head -n 1)
    export VOID_BOX_KMOD_VERSION="${dl_kernel_ver:-6.8.0-51}"
    export VOID_BOX_KMOD_UPLOAD="${dl_kernel_upload:-52}"
    echo "[$log_prefix] Using pinned kernel modules: ${VOID_BOX_KMOD_VERSION} (upload ${VOID_BOX_KMOD_UPLOAD})"
  else
    echo "[$log_prefix] Using host kernel modules for local build (uname -r=$(uname -r))"
  fi
}

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

# ── Download artifact helpers ────────────────────────────────────────────────

# Fetch `url` to `dest` (idempotent: skip if the file already matches the
# expected SHA-256), then verify.
#
# Atomicity: the fresh download is written to a sibling `.tmp.$$` file,
# verified *there*, and only then renamed into place. A concurrent builder
# cannot observe a half-written or unverified `dest`. A mismatching cached
# file (from a prior run whose manifest hash has since changed) is deleted
# and refetched. Poisoned cache files never persist past a verification
# failure.
_agent_fetch_and_verify() {
  local log_prefix="$1"
  local url="$2"
  local dest="$3"
  local expected_sha="$4"
  local label
  label="$(basename "$dest")"

  if [[ -f "$dest" ]]; then
    if agent_manifest_verify "$dest" "$expected_sha" "$label" >/dev/null 2>&1; then
      echo "[$log_prefix] Using cached (verified) download: $dest"
      return 0
    fi
    echo "[$log_prefix] Cached download failed verification, refetching: $dest" >&2
    rm -f "$dest"
  fi

  local staging="${dest}.tmp.$$"
  echo "[$log_prefix] Downloading $url"
  if ! curl -fSL --progress-bar -o "$staging" "$url"; then
    echo "ERROR: download failed: $url" >&2
    rm -f "$staging"
    return 1
  fi

  if ! agent_manifest_verify "$staging" "$expected_sha" "$label"; then
    rm -f "$staging"
    return 1
  fi

  if ! mv -f "$staging" "$dest"; then
    echo "ERROR: failed to atomically install $dest" >&2
    rm -f "$staging"
    return 1
  fi
  echo "[$log_prefix] Verified SHA-256 for $label"
  return 0
}

find_extracted_executable() {
  local search_dir="$1"
  local preferred_name="${2:-}"
  local candidate

  # Pass 1: a matching bare-name binary, if the caller named one.
  if [[ -n "$preferred_name" ]]; then
    while IFS= read -r candidate; do
      if [[ -x "$candidate" ]]; then
        printf '%s\n' "$candidate"
        return 0
      fi
    done < <(find "$search_dir" -type f -name "$preferred_name" | LC_ALL=C sort)
  fi

  # Pass 2: first executable we find, sorted for determinism. `find` path
  # ordering is filesystem-defined; LC_ALL=C sort pins the choice so two runs
  # on the same tarball always pick the same binary.
  while IFS= read -r candidate; do
    if [[ -x "$candidate" ]]; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done < <(
    find "$search_dir" -type f \
      ! -name '*.tar.gz' ! -name '*.tgz' ! -name '*.tar' \
      ! -name '*.zst' ! -name '*.sigstore' ! -name '*.sig' \
      ! -name '*.sha256' ! -name '*.txt' \
      | LC_ALL=C sort
  )

  return 1
}

# ── SSL CA certificates ──────────────────────────────────────────────────────
# Install the host CA bundle at the canonical path and create symlinks for
# every common location so that curl, OpenSSL, Bun, etc. all find it
# regardless of which distro compiled them.
# Supports Linux + macOS host paths and optional override via VOID_BOX_CA_BUNDLE
# (or SSL_CERT_FILE).
# Returns 1 if no host CA bundle is found — both Claude and Codex need TLS.

install_ca_certificates() {
  local out_dir="$1"
  local canonical="$out_dir/etc/ssl/certs/ca-certificates.crt"
  mkdir -p "$(dirname "$canonical")"

  local cert_candidates=()
  if [[ -n "${VOID_BOX_CA_BUNDLE:-}" ]]; then
    cert_candidates+=("${VOID_BOX_CA_BUNDLE}")
  fi
  if [[ -n "${SSL_CERT_FILE:-}" ]]; then
    cert_candidates+=("${SSL_CERT_FILE}")
  fi

  # Ordered by precedence:
  # 1. Common Linux distro locations
  # 2. macOS system bundle
  # 3. Homebrew-managed cert bundle paths
  cert_candidates+=(
    /etc/ssl/certs/ca-certificates.crt
    /etc/pki/tls/certs/ca-bundle.crt
    /etc/ssl/certs/ca-bundle.crt
    /etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem
    /etc/ssl/cert.pem
    /private/etc/ssl/cert.pem
    /opt/homebrew/etc/ca-certificates/cert.pem
    /usr/local/etc/ca-certificates/cert.pem
    /opt/homebrew/etc/openssl@3/cert.pem
    /usr/local/etc/openssl@3/cert.pem
  )

  local found=""
  local searched=""
  local cert_path
  for cert_path in "${cert_candidates[@]}"; do
    if [[ -n "$searched" ]]; then
      searched="${searched}, "
    fi
    searched="${searched}${cert_path}"
    if [[ -f "$cert_path" ]]; then
      cp "$cert_path" "$canonical"
      echo "[agent-rootfs] Installed CA certificates from $cert_path"
      found="$cert_path"
      break
    fi
  done

  if [[ -z "$found" ]]; then
    echo "ERROR: no host CA bundle found in any known location" >&2
    echo "  Checked: $searched" >&2
    echo "  Set VOID_BOX_CA_BUNDLE=/path/to/ca-bundle.pem and retry." >&2
    return 1
  fi

  local link_dir
  for link_path in \
    /etc/pki/tls/certs/ca-bundle.crt \
    /etc/ssl/certs/ca-bundle.crt \
    /etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem \
    /etc/ssl/cert.pem \
    ; do
    link_dir="$out_dir$(dirname "$link_path")"
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
  local uncompressed_mb
  uncompressed_mb=$(( (uncompressed_bytes + 1048575) / 1048576 ))
  echo "[agent-rootfs] Done. Initramfs: $out_cpio ($final_size)"
  echo "[agent-rootfs] Uncompressed size: ~${uncompressed_mb} MB — guest RAM must be larger (e.g. voidbox snapshot create --memory 512)."
}
