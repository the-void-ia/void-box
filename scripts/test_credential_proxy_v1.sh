#!/usr/bin/env bash
#
# Reproducible test harness for the M0 credential-injection proxy (RFC-0002).
#
# It runs the checks in dependency order — a funded key, then the proxy mechanics
# against real Anthropic, then (on Linux/KVM) the full agent-in-a-VM V1 gate — so a
# failure points at the narrowest broken layer instead of a tangled VM run.
#
# Modes:
#   mechanics  (default) — runs anywhere, including macOS. No VM. Proves the key is
#              funded and that the proxy injects the host-held key such that the real
#              api.anthropic.com accepts the request.
#   full       — Linux/KVM only. Boots the real Claude Code client inside a VM through
#              the proxy (the RFC-0002 "V1" gate) and checks the containment
#              properties in the run log.
#   all        — mechanics, then full.
#
# Usage:
#   export ANTHROPIC_API_KEY=sk-ant-...        # a FUNDED key; never hardcode it here
#   scripts/test_credential_proxy_v1.sh            # mechanics (default)
#   scripts/test_credential_proxy_v1.sh full       # full VM gate (Linux/KVM)
#   scripts/test_credential_proxy_v1.sh all
#
# The key is read from the environment only. Rotate it after testing — anything you
# paste into a shell or a transcript should be treated as exposed.

set -euo pipefail

MODE="${1:-mechanics}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"
export TMPDIR="${TMPDIR:-$REPO_ROOT/target/tmp}"
mkdir -p "$TMPDIR"

ANTHROPIC_VERSION="2023-06-01"
UPSTREAM="https://api.anthropic.com"
CHEAP_MODEL="claude-haiku-4-5"

say()  { printf '\n\033[1m=== %s ===\033[0m\n' "$*"; }
pass() { printf '\033[32m  PASS\033[0m %s\n' "$*"; }
info() { printf '  ..   %s\n' "$*"; }
die()  { printf '\033[31m  FAIL\033[0m %s\n' "$*" >&2; exit 1; }

require_key() {
  [ "${ANTHROPIC_API_KEY:-}" != "" ] || die "ANTHROPIC_API_KEY is not set (needs a funded key)."
}

# ---------------------------------------------------------------------------
# Mechanics — no VM; runs on macOS or Linux.
# ---------------------------------------------------------------------------
run_mechanics() {
  require_key

  say "1/3  Key auth check — GET /v1/models (zero generation cost)"
  # /v1/models validates the key without spending inference tokens.
  local hdrs status org
  hdrs="$(mktemp)"
  status="$(curl -sS -o /dev/null -D "$hdrs" -w '%{http_code}' "$UPSTREAM/v1/models" \
    --header "x-api-key: $ANTHROPIC_API_KEY" \
    --header "anthropic-version: $ANTHROPIC_VERSION")"
  [ "$status" = "200" ] || die "auth check returned HTTP $status (expected 200) — key invalid or revoked."
  org="$(grep -i '^anthropic-organization-id:' "$hdrs" | tr -d '\r' | awk '{print $2}')"
  rm -f "$hdrs"
  pass "key authenticates; organization=${org:-unknown}"

  say "2/3  Inference check — POST /v1/messages ($CHEAP_MODEL)"
  # Confirms the key's organization actually has credit/billing, not just valid auth.
  status="$(curl -sS -o /dev/null -w '%{http_code}' "$UPSTREAM/v1/messages" \
    --header "x-api-key: $ANTHROPIC_API_KEY" \
    --header "anthropic-version: $ANTHROPIC_VERSION" \
    --header "content-type: application/json" \
    --data "{\"model\":\"$CHEAP_MODEL\",\"max_tokens\":16,\"messages\":[{\"role\":\"user\",\"content\":\"Reply with exactly: proxy check ok\"}]}")"
  case "$status" in
    200) pass "key is funded; real inference works" ;;
    400) die  "HTTP 400 — likely 'credit balance too low'. Fund the org's platform credits (platform.claude.com → Billing)." ;;
    *)   die  "inference check returned HTTP $status (expected 200)." ;;
  esac

  say "3/3  Proxy mechanics — inject host key, re-originate to REAL api.anthropic.com"
  # tests/proxy_real_upstream.rs stands up the production proxy (start_proxy):
  # per-sandbox name-constrained CA + token + StaticApiKeyInjector, guest TLS to the
  # loopback listener carrying only the placeholder, re-originated over HTTP/1.1 to
  # the real upstream. Asserts 200 + a real Messages completion.
  cargo test --test proxy_real_upstream -- --ignored --nocapture
  pass "real Anthropic accepted the injected request through the proxy"

  say "Mechanics — what was proven"
  cat <<'EOF'
  - Real TLS to api.anthropic.com from the production upstream client (start_proxy).
  - Guest trusts the per-sandbox name-constrained CA; proxy TLS-terminates.
  - Per-sandbox token authenticates the connection before any upstream call.
  - The guest placeholder x-api-key is replaced with the host-held real key.
  - Request re-originated over HTTP/1.1; real Anthropic returned a completion.
  Not covered here (needs the VM): Claude Code as the real client, guest env/file
  staging, and the R14 no-real-credential-in-guest gate. Run `full` on Linux/KVM.
EOF
}

# ---------------------------------------------------------------------------
# Full V1 gate — Claude Code inside a VM through the proxy. Linux/KVM only.
# ---------------------------------------------------------------------------
run_full() {
  require_key

  say "Preconditions (Linux/KVM)"
  [ "$(uname -s)" = "Linux" ] || die "full gate is Linux/KVM only. The M0 proxy fails closed on macOS/VZ by design (guest_accessible_bind_addr binds 0.0.0.0). Use a Linux host."
  [ -e /dev/kvm ] || die "/dev/kvm not present — the VM suites need usable KVM."
  pass "Linux host with /dev/kvm"

  say "Build the production Claude image"
  info "scripts/build_claude_rootfs.sh  (hash-pinned claude-code; see docs/agents/claude.md)"
  scripts/build_claude_rootfs.sh
  local initramfs="$REPO_ROOT/target/void-box-claude.cpio.gz"
  [ -f "$initramfs" ] || die "expected image at $initramfs"
  export VOID_BOX_KERNEL="${VOID_BOX_KERNEL:-/boot/vmlinuz-$(uname -r)}"
  export VOID_BOX_INITRAMFS="$initramfs"
  pass "image built; VOID_BOX_KERNEL=$VOID_BOX_KERNEL"

  say "Run the V1 spec through the proxy"
  local log="$REPO_ROOT/target/credential_proxy_v1.log"
  info "log: $log"
  # VOIDBOX_LOG_LEVEL=info surfaces the 'credential proxy active' line.
  VOIDBOX_LOG_LEVEL=info \
    cargo run --bin voidbox -- run --file examples/specs/credential_proxy_claude.yaml 2>&1 | tee "$log"

  say "Observation checklist (V1)"
  # Each property below maps to the mechanism that enforces it.

  # 1. Proxy stood up and the real key is withheld (agent_box::maybe_setup_credential_proxy).
  if grep -qE 'credential proxy active on port [0-9]+ for api\.anthropic\.com \(real key withheld from guest\)' "$log"; then
    pass "proxy active; real key withheld from guest (R14 structural withholding)"
  else
    die "did not see the 'credential proxy active … real key withheld' line — proxy did not start or provider was not served."
  fi

  # 2. No R14 leak abort (assert_no_real_credential): the run would have errored if
  #    the real key reached the staged guest env or files.
  if grep -qiE 'R14: real credential leaked' "$log"; then
    die "R14 gate tripped — a real credential reached the guest. This is a containment failure."
  fi
  pass "R14 gate did not trip (no real credential in staged guest env/files)"

  # 3. Claude Code trusted the CA and did not force interactive login / retry-storm.
  if grep -qiE 'self.signed certificate|unable to (get|verify)|CERT_|force.?login|/login' "$log"; then
    die "saw a CA-trust or force-login symptom in the log — inspect $log (NODE_EXTRA_CA_CERTS / name constraints / login suppression)."
  fi
  pass "no CA-trust error and no force-login symptom"

  # 4. The agent actually reached Anthropic and answered (proves end-to-end injection).
  if grep -qiE 'Agent finished|reached the Anthropic API|hello' "$log"; then
    pass "agent produced output — injected request reached real Anthropic and returned a completion"
  else
    info "no obvious completion marker; inspect $log for the agent's answer."
  fi

  say "Full gate — manual follow-ups worth a look in the run"
  cat <<'EOF'
  - HTTP/2 -> HTTP/1.1: the proxy is HTTP/1.1-only on the client hop
    (tests/proxy.rs::proxy_does_not_negotiate_h2_and_serves_http1). A Node/undici
    client that prefers h2 must fall back cleanly — no ALPN/h2 protocol error.
  - Guest env: ANTHROPIC_BASE_URL points at the proxy, ANTHROPIC_API_KEY is the
    placeholder (never the real key), ANTHROPIC_CUSTOM_HEADERS carries the token.
  - /etc/hosts inside the guest aliases api.anthropic.com to the SLIRP gateway.
EOF
}

case "$MODE" in
  mechanics) run_mechanics ;;
  full)      run_full ;;
  all)       run_mechanics; run_full ;;
  *) die "unknown mode '$MODE' (expected: mechanics | full | all)" ;;
esac

say "Done"
printf '  Reminder: rotate ANTHROPIC_API_KEY after testing — treat it as exposed.\n'
