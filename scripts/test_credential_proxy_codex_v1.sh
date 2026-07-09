#!/usr/bin/env bash
#
# Reproducible test harness for the M1b codex credential-proxy path (RFC-0002).
#
# It runs the checks in dependency order — the provisioning harness against the
# real codex binary first, then the real-upstream proxy mechanics, then (on
# Linux/KVM) the full agent-in-a-VM gate — so a failure points at the narrowest
# broken layer instead of a tangled VM run.
#
# Modes:
#   harness    (default) — runs anywhere, including macOS. No VM, no account, no
#              key. Fetches the pinned codex binary and runs
#              tests/codex_provisioning_harness.rs against it: the R9 gate that
#              the generated config.toml / placeholder auth.json knobs are
#              honored by the actual binary (redirect, token carriers, no
#              WebSocket, no self-refresh). Run this on every codex bump.
#   mechanics  — additionally needs a funded OPENAI_API_KEY. Proves the proxy
#              injects the host-held key such that the real api.openai.com
#              accepts the request.
#   full       — Linux/KVM only. Boots the real codex CLI inside a VM through
#              the proxy and checks the containment properties in the run log.
#   all        — harness, then mechanics, then full.
#
# Usage:
#   scripts/test_credential_proxy_codex_v1.sh                 # harness (default)
#   OPENAI_API_KEY=sk-... scripts/test_credential_proxy_codex_v1.sh mechanics
#   OPENAI_API_KEY=sk-... scripts/test_credential_proxy_codex_v1.sh all
#
# Keys are read from the environment only. Rotate them after testing — anything
# you paste into a shell or a transcript should be treated as exposed.

set -euo pipefail

MODE="${1:-harness}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"
export TMPDIR="${TMPDIR:-$REPO_ROOT/target/tmp}"
mkdir -p "$TMPDIR"

# shellcheck source=lib/agent_manifest.sh
source "$REPO_ROOT/scripts/lib/agent_manifest.sh"

say()  { printf '\n\033[1m=== %s ===\033[0m\n' "$*"; }
pass() { printf '\033[32m  PASS\033[0m %s\n' "$*"; }
info() { printf '  ..   %s\n' "$*"; }
die()  { printf '\033[31m  FAIL\033[0m %s\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# Resolve a codex binary for THIS host OS at the manifest-pinned version.
#
# Linux: the manifest artifact itself, sha256-verified (the production path).
# macOS: the darwin tarball of the same release tag — the manifest pins only
# linux artifacts (only linux ships in guest images), so this is a
# dev-convenience path without a pinned hash; the observed digest is printed so
# a run is reproducible after the fact.
# ---------------------------------------------------------------------------
resolve_codex_bin() {
  if [ "${VOIDBOX_CODEX_BIN:-}" != "" ]; then
    info "using VOIDBOX_CODEX_BIN=$VOIDBOX_CODEX_BIN"
    CODEX_BIN_RESOLVED="$VOIDBOX_CODEX_BIN"
    return
  fi

  local host_os host_arch
  host_os="$(uname -s)"
  host_arch="$(uname -m)"

  local manifest_arch
  case "$host_arch" in
    x86_64)          manifest_arch="x86_64" ;;
    arm64|aarch64)   manifest_arch="aarch64" ;;
    *) die "unsupported host arch '$host_arch'" ;;
  esac

  local version url sha256
  { read -r version; read -r url; read -r sha256; } \
    < <(agent_manifest_require codex linux "$manifest_arch")

  local cache_dir="$REPO_ROOT/target/tmp/codex-harness/$version-$host_os-$host_arch"
  local bin="$cache_dir/codex"
  if [ -x "$bin" ]; then
    info "cached codex $version at $bin"
    CODEX_BIN_RESOLVED="$bin"
    return
  fi
  mkdir -p "$cache_dir"

  local tarball="$cache_dir/codex.tar.gz"
  case "$host_os" in
    Linux)
      url="${url//\{version\}/$version}"
      info "fetching pinned codex $version ($url)"
      curl -fsSL -o "$tarball" "$url"
      agent_manifest_verify "$tarball" "$sha256" \
        || die "sha256 mismatch for $url — manifest and upstream disagree (R-B5c.1)."
      pass "sha256 verified against manifest pin"
      ;;
    Darwin)
      local rust_target="aarch64-apple-darwin"
      [ "$manifest_arch" = "x86_64" ] && rust_target="x86_64-apple-darwin"
      local darwin_url="https://github.com/openai/codex/releases/download/rust-v${version}/codex-${rust_target}.tar.gz"
      info "fetching codex $version for macOS ($darwin_url)"
      info "note: the manifest pins linux artifacts only; recording the observed digest instead"
      curl -fsSL -o "$tarball" "$darwin_url"
      info "observed sha256: $(agent_manifest_sha256 "$tarball")"
      ;;
    *) die "unsupported host OS '$host_os'" ;;
  esac

  tar -xzf "$tarball" -C "$cache_dir"
  # The tarball contains a single binary named codex-<target>; normalize it.
  local extracted
  extracted="$(find "$cache_dir" -maxdepth 1 -type f -name 'codex*' ! -name '*.tar.gz' | head -1)"
  [ -n "$extracted" ] || die "no codex binary found in $tarball"
  mv "$extracted" "$bin"
  chmod +x "$bin"
  pass "codex $version ready at $bin"
  CODEX_BIN_RESOLVED="$bin"
}

# ---------------------------------------------------------------------------
# Harness — the R9 gate. No VM, no account, no key.
# ---------------------------------------------------------------------------
run_harness() {
  say "R9 provisioning harness — real codex binary vs generated provisioning"
  resolve_codex_bin
  "$CODEX_BIN_RESOLVED" --version || die "codex binary does not run on this host"

  VOIDBOX_CODEX_BIN="$CODEX_BIN_RESOLVED" \
    cargo test --test codex_provisioning_harness -- --ignored --nocapture --test-threads=1
  pass "codex honored the generated config.toml/auth.json (redirect, token carriers, no WS, no self-refresh)"
}

# ---------------------------------------------------------------------------
# Mechanics — real api.openai.com through the production proxy. Needs a key.
# ---------------------------------------------------------------------------
run_mechanics() {
  [ "${OPENAI_API_KEY:-}" != "" ] || die "OPENAI_API_KEY is not set (needs a funded key)."

  say "1/2  Key auth check — GET /v1/models (zero generation cost)"
  local status
  status="$(curl -sS -o /dev/null -w '%{http_code}' "https://api.openai.com/v1/models" \
    --header "authorization: Bearer $OPENAI_API_KEY")"
  [ "$status" = "200" ] || die "auth check returned HTTP $status (expected 200) — key invalid or revoked."
  pass "key authenticates"

  say "2/2  Proxy mechanics — inject host key, re-originate to REAL api.openai.com"
  cargo test --test proxy_real_upstream \
    injects_real_openai_key_via_bearer_carried_token_and_openai_accepts_it \
    -- --ignored --nocapture
  pass "real OpenAI accepted the injected request through the proxy"
}

# ---------------------------------------------------------------------------
# Full gate — codex CLI inside a VM through the proxy. Linux/KVM only.
# ---------------------------------------------------------------------------
run_full() {
  [ "${OPENAI_API_KEY:-}" != "" ] || die "OPENAI_API_KEY is not set (needs a funded key)."

  say "Preconditions (Linux/KVM)"
  [ "$(uname -s)" = "Linux" ] || die "full gate is Linux/KVM only (the VM suites need KVM). Use a Linux host."
  [ -e /dev/kvm ] || die "/dev/kvm not present — the VM suites need usable KVM."
  pass "Linux host with /dev/kvm"

  say "Build the production codex image"
  info "scripts/build_codex_rootfs.sh  (hash-pinned codex; see docs/agents/codex.md)"
  scripts/build_codex_rootfs.sh
  local initramfs="$REPO_ROOT/target/void-box-codex.cpio.gz"
  [ -f "$initramfs" ] || die "expected image at $initramfs"
  export VOID_BOX_KERNEL="${VOID_BOX_KERNEL:-/boot/vmlinuz-$(uname -r)}"
  export VOID_BOX_INITRAMFS="$initramfs"
  pass "image built; VOID_BOX_KERNEL=$VOID_BOX_KERNEL"

  say "Run the codex API-key spec through the proxy"
  local log="$REPO_ROOT/target/credential_proxy_codex_v1.log"
  info "log: $log"
  VOIDBOX_LOG_LEVEL=info \
    cargo run --bin voidbox -- run --file examples/specs/credential_proxy_codex.yaml 2>&1 | tee "$log"

  say "Observation checklist"
  if grep -qE 'credential proxy active on port [0-9]+ for api\.openai\.com \(real key withheld from guest\)' "$log"; then
    pass "proxy active; real key withheld from guest (R14 structural withholding)"
  else
    die "did not see the 'credential proxy active … real key withheld' line."
  fi
  if grep -qiE 'R14: real credential leaked' "$log"; then
    die "R14 gate tripped — a real credential reached the guest."
  fi
  pass "R14 gate did not trip"
  if grep -qiE 'websocket-upgrade-refused' "$log"; then
    die "the proxy refused a WebSocket upgrade — supports_websockets=false was not honored (R8/R9)."
  fi
  pass "no WebSocket upgrade attempt (supports_websockets=false honored)"
  if grep -qiE 'certificate|force.?login' "$log"; then
    info "certificate/login mention in log — inspect $log to rule out a CA-trust symptom."
  fi
  if grep -qiE 'Agent finished' "$log"; then
    pass "agent finished — injected request reached real OpenAI"
  else
    info "no completion marker; inspect $log for the agent's answer."
  fi
}

case "$MODE" in
  harness)   run_harness ;;
  mechanics) run_mechanics ;;
  full)      run_full ;;
  all)       run_harness; run_mechanics; run_full ;;
  *) die "unknown mode '$MODE' (expected: harness | mechanics | full | all)" ;;
esac

say "Done"
printf '  Reminder: rotate any key used here after testing — treat it as exposed.\n'
