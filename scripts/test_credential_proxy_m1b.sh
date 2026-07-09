#!/usr/bin/env bash
#
# ============================================================================
# Credential-proxy M1b — runnable test plan (RFC-0002)
# ============================================================================
#
# Validates the M1b credential proxy end to end: codex through the proxy
# (API-key + ChatGPT OAuth), WebSocket fail-closed (R8), long-run credential
# rotation, KVM+VZ parity, and the Anthropic-compatible Custom provider.
#
# This script IS the test plan: `--list` prints every test with what it checks
# and the expected result, so it doubles as documentation. The narrative
# framing lives in docs/testing/credential-proxy-m1b.md.
#
# It is written to be run by a human OR an AI agent: each test declares its own
# pass condition, prints PASS/FAIL/SKIP as it goes, and writes its raw output
# to a persistent evidence folder. At the end it prints a summary table and the
# evidence path.
#
# ---------------------------------------------------------------------------
# Tiers (cheapest and safest first; each is independently selectable)
# ---------------------------------------------------------------------------
#   A  Static & unit .......... fmt, clippy -D warnings, all unit/integration
#                               tests, the in-process proxy pipeline, doctests.
#                               No keys, no VM, no network.
#   B  Config validation ...... `voidbox validate` accepts good proxy specs and
#                               rejects the parse-time failure cases. No keys.
#   C  codex provisioning (R9)  the real pinned codex binary honors the
#                               generated config.toml/auth.json on the wire.
#                               Fetches the codex binary; no keys, no account.
#   D  Real-upstream injection  the production proxy injects the host-held API
#                               key and the real provider accepts it (Claude +
#                               codex). Needs API keys. ToS-clean, no rotation.
#   E  Full VM (VZ/KVM) ........ the whole chain in a booted guest: deterministic
#                               containment, a real Claude completion + R14 log
#                               checks, an adversarial containment probe, a
#                               Custom-provider completion. Needs API keys +
#                               image builds. Opt in with RUN_VM=1.
#   F  Subscription OAuth ...... the personal-subscription paths (Claude personal
#                               + codex ChatGPT). ROTATES YOUR REAL LOGIN'S
#                               single-use refresh token. Opt in with
#                               RUN_SUBSCRIPTION=1.
#
# Default run: A B C D  (+ E if RUN_VM=1, + F if RUN_SUBSCRIPTION=1).
#
# ---------------------------------------------------------------------------
# Safety
# ---------------------------------------------------------------------------
# Tiers A–E are ToS-clean and rotate nothing (API keys or no credential).
# Tier F refreshes personal-subscription OAuth tokens, which are single-use, so
# it rotates your real `claude`/`codex` login. The host store writes the rotated
# token back atomically so the login stays valid, but do not run `claude`/`codex`
# yourself concurrently. Off by default.
#
# ---------------------------------------------------------------------------
# Credentials
# ---------------------------------------------------------------------------
# API keys are read from CREDS_FILE (default /Users/cspinetta/dev/.credentials)
# which must contain:
#     ANTHROPIC_API_KEY_VOID_BOX_TEST=sk-ant-...
#     OPENAI_API_KEY_VOID_BOX_TEST=sk-...
# Keys are loaded into test subprocesses only — never echoed, logged, or written
# to the evidence folder.
#
# ---------------------------------------------------------------------------
# Usage
# ---------------------------------------------------------------------------
#   scripts/test_credential_proxy_m1b.sh --list      # plan detail, no run
#   scripts/test_credential_proxy_m1b.sh             # tiers A–D
#   RUN_VM=1 scripts/test_credential_proxy_m1b.sh    # + full VM runs
#   RUN_VM=1 RUN_SUBSCRIPTION=1 scripts/...          # + subscription OAuth
#   scripts/test_credential_proxy_m1b.sh A D         # only tiers A and D
#
# Rotate any key used here after testing — treat it as exposed.
# ============================================================================

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"
export TMPDIR="${TMPDIR:-$REPO_ROOT/target/tmp}"
mkdir -p "$TMPDIR"

CREDS_FILE="${CREDS_FILE:-/Users/cspinetta/dev/.credentials}"
RUN_VM="${RUN_VM:-0}"
RUN_SUBSCRIPTION="${RUN_SUBSCRIPTION:-0}"

# macOS excludes the Linux-only guest-agent crate from clippy/tests.
CLIPPY_EXCLUDE=()
if [[ "$(uname -s)" == "Darwin" ]]; then
  CLIPPY_EXCLUDE=(--exclude guest-agent)
fi

# ---------------------------------------------------------------------------
# Output helpers + result tracking
# ---------------------------------------------------------------------------
if [[ -t 1 ]]; then
  C_BOLD=$'\033[1m'; C_GREEN=$'\033[32m'; C_RED=$'\033[31m'
  C_YELLOW=$'\033[33m'; C_DIM=$'\033[2m'; C_RESET=$'\033[0m'
else
  C_BOLD=""; C_GREEN=""; C_RED=""; C_YELLOW=""; C_DIM=""; C_RESET=""
fi

say()  { printf '\n%s=== %s ===%s\n' "$C_BOLD" "$*" "$C_RESET"; }
info() { printf '  %s..%s %s\n' "$C_DIM" "$C_RESET" "$*"; }

declare -a R_ID R_STATUS R_DESC R_DETAIL
record() { # id status desc detail
  R_ID+=("$1"); R_STATUS+=("$2"); R_DESC+=("$3"); R_DETAIL+=("${4:-}")
  local color="$C_YELLOW"
  case "$2" in PASS) color="$C_GREEN";; FAIL) color="$C_RED";; esac
  printf '  %s%-4s%s [%s] %s' "$color" "$2" "$C_RESET" "$1" "$3"
  [[ -n "${4:-}" ]] && printf ' %s(%s)%s' "$C_DIM" "$4" "$C_RESET"
  printf '\n'
}

EVIDENCE_DIR=""
evidence() { printf '%s/%s.log' "$EVIDENCE_DIR" "$1"; }

# ---------------------------------------------------------------------------
# --list: print the plan without running anything
# ---------------------------------------------------------------------------
print_plan() {
  cat <<'PLAN'
Credential-proxy M1b test plan
==============================

Tier A — Static & unit  (no keys, no VM)
  A1  cargo fmt --check ............... formatting is clean
  A2  cargo clippy -D warnings ........ no lint warnings across the workspace
  A3  cargo test --lib ............... all unit tests pass: OAuth-store rotation
                                        chains + adopt-peer + rate-cap (both
                                        providers), provision renderers, spec
                                        validation, R14 preconditions, withholding
  A4  cargo test --test proxy ........ in-process pipeline: codex Bearer-carrier
                                        + OAuth+account-id injection, WebSocket
                                        fail-closed, token auth, CA name
                                        constraints, SSRF reject, cross-sandbox,
                                        oversize-header cap
  A5  cargo test --doc ............... doctests pass

Tier B — Config validation via the CLI  (no keys, no VM)
  B1  validate good specs ............ `voidbox validate` accepts the 5 example
                                        credential_proxy specs
  B2  reject custom + http:// ........ Custom + proxy without https is rejected
                                        at parse time with the reason named
  B3  reject custom without key ...... Custom + proxy without api_key_env is
                                        rejected at parse time

Tier C — codex provisioning harness (R9)  (no keys, real codex binary)
  C1  codex honors generated config .. the pinned codex binary redirects to the
                                        proxy base URL, carries the proxy token,
                                        makes no WebSocket attempt, and never
                                        self-refreshes the placeholder tokens

Tier D — Real-upstream injection  (API keys, no VM; ToS-clean, no rotation)
  D1  Claude API key -> Anthropic .... the production proxy injects the host key;
                                        real api.anthropic.com returns a
                                        completion; placeholder never forwarded
  D2  codex key -> OpenAI ............ Bearer-carried token authenticates the
                                        proxy; injected key accepted by real
                                        api.openai.com; token never forwarded

Tier E — Full VM on VZ/KVM  (API keys + image builds; RUN_VM=1)
  E1  e2e_credential_proxy ........... booted guest gets the placeholder; the
                                        proxy binds the guest-reachable address;
                                        CA + /etc/hosts staged; injection replaces
                                        the placeholder; R14 holds
  E2  Claude API-key VM run .......... real completion through the proxy; log shows
                                        "credential proxy active ... real key
                                        withheld"; no R14 leak; no CA/force-login
  E3  containment probe (adversarial)  an agent asked to print its ANTHROPIC_API_KEY
                                        gets the placeholder; the real key appears
                                        nowhere in the run log or guest output
  E4  Custom-provider VM run ......... a Custom endpoint (Anthropic-compatible)
                                        redirected through the proxy returns a
                                        completion; URL parsing + injection hold

Tier F — Subscription OAuth  (rotates your real login; RUN_SUBSCRIPTION=1)
  F1  Claude personal OAuth VM run ... claude-personal through the proxy; host
                                        store refreshes ~/.claude; real completion
  F2  codex ChatGPT OAuth VM run ..... codex chatgpt through the proxy; host store
                                        refreshes ~/.codex; real completion

Build-time / unit-covered invariants (proven by Tier A, not a live run):
  - Custom internal-IP base URL rejected (SSRF)     proxy::provision::tests
  - Unserviceable provider + proxy rejected         agent_box::tests
  - Credential-home mount + proxy rejected (R14)     agent_box::tests
See docs/testing/credential-proxy-m1b.md for the full coverage map.
PLAN
}

# ---------------------------------------------------------------------------
# Credential loading (values never printed)
# ---------------------------------------------------------------------------
LOADED_ANTHROPIC=""
LOADED_OPENAI=""
load_keys() {
  [[ -f "$CREDS_FILE" ]] || { info "no CREDS_FILE at $CREDS_FILE — key-dependent tests will SKIP"; return; }
  LOADED_ANTHROPIC="$(grep -E '^ANTHROPIC_API_KEY_VOID_BOX_TEST=' "$CREDS_FILE" | head -1 | cut -d= -f2-)"
  LOADED_OPENAI="$(grep -E '^OPENAI_API_KEY_VOID_BOX_TEST=' "$CREDS_FILE" | head -1 | cut -d= -f2-)"
  [[ -n "$LOADED_ANTHROPIC" ]] && info "loaded ANTHROPIC_API_KEY (${#LOADED_ANTHROPIC} chars)" || info "ANTHROPIC key not found in $CREDS_FILE"
  [[ -n "$LOADED_OPENAI" ]] && info "loaded OPENAI_API_KEY (${#LOADED_OPENAI} chars)" || info "OPENAI key not found in $CREDS_FILE"
}

# ---------------------------------------------------------------------------
# Generic runners. Each captures output to the test's evidence log.
# ---------------------------------------------------------------------------
# run_pass ID "desc" -- cmd...    -> PASS iff exit 0
run_pass() {
  local id="$1" desc="$2"; shift 3   # drop the literal --
  local log; log="$(evidence "$id")"
  if "$@" >"$log" 2>&1; then record "$id" PASS "$desc" "$(basename "$log")"
  else record "$id" FAIL "$desc" "exit $?, see $(basename "$log")"; fi
}

# run_reject ID "desc" "pattern" -- cmd...  -> PASS iff non-zero AND log matches pattern
run_reject() {
  local id="$1" desc="$2" pat="$3"; shift 4
  local log; log="$(evidence "$id")"
  if "$@" >"$log" 2>&1; then
    record "$id" FAIL "$desc" "command unexpectedly succeeded"
  elif grep -qiE "$pat" "$log"; then
    record "$id" PASS "$desc" "rejected as expected"
  else
    record "$id" FAIL "$desc" "rejected but reason mismatch, see $(basename "$log")"
  fi
}

# ---------------------------------------------------------------------------
# Tier A — static & unit
# ---------------------------------------------------------------------------
tier_A() {
  say "Tier A — static analysis & unit/integration tests"
  run_pass A1 "cargo fmt --check" -- cargo fmt --all -- --check
  run_pass A2 "cargo clippy -D warnings" -- \
    cargo clippy --workspace "${CLIPPY_EXCLUDE[@]}" --all-targets --all-features -- -D warnings
  run_pass A3 "cargo test --lib (rotation, provision, spec, R14)" -- \
    cargo test --workspace "${CLIPPY_EXCLUDE[@]}" --lib --all-features
  run_pass A4 "cargo test --test proxy (pipeline)" -- cargo test --test proxy
  run_pass A5 "cargo test --doc" -- cargo test --workspace "${CLIPPY_EXCLUDE[@]}" --doc --all-features
}

# ---------------------------------------------------------------------------
# Tier B — config validation via the CLI
# ---------------------------------------------------------------------------
tier_B() {
  say "Tier B — config validation via the CLI"
  local vb=(cargo run -q --bin voidbox --)
  # B1: every shipped credential_proxy example validates.
  local specs=(
    examples/specs/credential_proxy_claude.yaml
    examples/specs/credential_proxy_claude_personal.yaml
    examples/specs/credential_proxy_codex.yaml
    examples/specs/credential_proxy_codex_chatgpt.yaml
    examples/specs/credential_proxy_custom.yaml
  )
  local log; log="$(evidence B1)"; : >"$log"
  local ok=1
  for s in "${specs[@]}"; do
    printf '\n# validate %s\n' "$s" >>"$log"
    "${vb[@]}" validate --file "$s" >>"$log" 2>&1 || ok=0
  done
  if [[ $ok -eq 1 ]]; then record B1 PASS "validate 5 example proxy specs" "$(basename "$log")"
  else record B1 FAIL "validate 5 example proxy specs" "see $(basename "$log")"; fi

  local bad_http="$TMPDIR/m1b_bad_custom_http.yaml"
  cat >"$bad_http" <<'EOF'
api_version: v1
kind: agent
name: bad-custom-http
sandbox: { mode: auto }
llm: { provider: custom, base_url: "http://gateway.example.com/v1", api_key_env: MY_KEY, credential_proxy: true }
agent: { prompt: "hi" }
EOF
  run_reject B2 "reject custom + http:// base_url" "https" -- "${vb[@]}" validate --file "$bad_http"

  local bad_nokey="$TMPDIR/m1b_bad_custom_nokey.yaml"
  cat >"$bad_nokey" <<'EOF'
api_version: v1
kind: agent
name: bad-custom-nokey
sandbox: { mode: auto }
llm: { provider: custom, base_url: "https://openrouter.ai/api/v1", credential_proxy: true }
agent: { prompt: "hi" }
EOF
  run_reject B3 "reject custom without api_key_env" "api_key_env" -- "${vb[@]}" validate --file "$bad_nokey"
}

# ---------------------------------------------------------------------------
# Tier C — codex provisioning harness (R9)
# ---------------------------------------------------------------------------
tier_C() {
  say "Tier C — codex provisioning harness (R9)"
  run_pass C1 "codex honors generated config (redirect/token/no-WS/no-refresh)" -- \
    bash scripts/test_credential_proxy_codex_v1.sh harness
}

# ---------------------------------------------------------------------------
# Tier D — real-upstream injection (API keys, no VM)
# ---------------------------------------------------------------------------
# Run a real-upstream cargo test, retrying once on failure — these hit live
# provider CDNs and the proxy deliberately does not retry, so a transient TLS
# blip can 502 a single-shot run.
net_test() { # id desc key_env test_name
  local id="$1" desc="$2" key_env="$3" test_name="$4"
  local log; log="$(evidence "$id")"
  local attempt
  for attempt in 1 2; do
    printf '\n# attempt %d\n' "$attempt" >>"$log"
    if env "$key_env" cargo test --test proxy_real_upstream "$test_name" \
        -- --ignored --nocapture >>"$log" 2>&1 && grep -q "1 passed" "$log"; then
      record "$id" PASS "$desc" "$(basename "$log")$([[ $attempt -gt 1 ]] && echo ", attempt $attempt")"
      return
    fi
  done
  record "$id" FAIL "$desc" "failed twice, see $(basename "$log")"
}

tier_D() {
  say "Tier D — real-upstream injection (API keys, no VM)"
  if [[ -n "$LOADED_ANTHROPIC" ]]; then
    net_test D1 "Claude API key injected -> real api.anthropic.com accepts" \
      "ANTHROPIC_API_KEY=$LOADED_ANTHROPIC" injects_real_key_and_real_anthropic_accepts_it
  else
    record D1 SKIP "Claude API key injected -> real api.anthropic.com accepts" "no ANTHROPIC key"
  fi

  if [[ -n "$LOADED_OPENAI" ]]; then
    net_test D2 "codex Bearer-carried token -> real api.openai.com accepts" \
      "OPENAI_API_KEY=$LOADED_OPENAI" injects_real_openai_key_via_bearer_carried_token_and_openai_accepts_it
  else
    record D2 SKIP "codex Bearer-carried token -> real api.openai.com accepts" "no OPENAI key"
  fi
}

# ---------------------------------------------------------------------------
# VM prerequisites (macOS/VZ or Linux/KVM)
# ---------------------------------------------------------------------------
VOIDBOX_BIN=""
VM_KERNEL=""
VM_TEST_IMAGE=""
VM_CLAUDE_IMAGE=""
VM_READY=0

ensure_vm_prereqs() {
  local os; os="$(uname -s)"
  info "building release voidbox binary"
  cargo build --release --bin voidbox >/dev/null 2>&1 || { info "voidbox build failed"; return 1; }
  VOIDBOX_BIN="$REPO_ROOT/target/release/voidbox"

  if [[ "$os" == "Darwin" ]]; then
    bash scripts/sign-macos.sh >/dev/null 2>&1 || true
    # Kernel must match the module pin, or vsock.ko fails to load and the guest
    # never handshakes. Re-download if missing or version-skewed.
    local pin; pin="$(grep -E '^VOIDBOX_KERNEL_VER=' scripts/lib/kernel_pin.sh | head -1 | cut -d'"' -f2)"
    VM_KERNEL="$REPO_ROOT/target/vmlinux-arm64"
    if [[ ! -f "$VM_KERNEL" ]] || ! strings "$VM_KERNEL" 2>/dev/null | grep -q "Linux version ${pin}"; then
      info "downloading VZ kernel to match module pin ${pin}"
      scripts/download_kernel.sh >/dev/null 2>&1 || { info "kernel download failed"; return 1; }
    fi
  else
    VM_KERNEL="${VOID_BOX_KERNEL:-/boot/vmlinuz-$(uname -r)}"
    [[ -e /dev/kvm ]] || { info "/dev/kvm absent — VM tier needs KVM"; return 1; }
  fi

  # Test image (deterministic e2e).
  VM_TEST_IMAGE="/tmp/void-box-test-rootfs.cpio.gz"
  if [[ ! -f "$VM_TEST_IMAGE" ]]; then
    info "building test initramfs (build_test_image.sh)"
    scripts/build_test_image.sh >/dev/null 2>&1 || { info "test image build failed"; return 1; }
  fi

  # Claude production image (real agent runs).
  VM_CLAUDE_IMAGE="$REPO_ROOT/target/void-box-claude.cpio.gz"
  if [[ ! -f "$VM_CLAUDE_IMAGE" ]]; then
    info "building claude production image (build_claude_rootfs.sh) — this takes a few minutes"
    scripts/build_claude_rootfs.sh >/dev/null 2>&1 || { info "claude image build failed"; return 1; }
  fi

  VM_READY=1
  info "VM prerequisites ready (kernel=$VM_KERNEL)"
}

# Run a spec through the release binary with the given key env, capturing to a log.
# Args: logfile keyname keyval spec [extra grep-must-contain]  -- returns 0 on run exit 0.
run_spec() { # logfile spec key_env_assignment...
  local log="$1" spec="$2"; shift 2
  env "$@" VOIDBOX_LOG_LEVEL=info \
    VOID_BOX_KERNEL="$VM_KERNEL" VOID_BOX_INITRAMFS="$VM_CLAUDE_IMAGE" \
    "$VOIDBOX_BIN" run --file "$spec" >"$log" 2>&1
}

# ---------------------------------------------------------------------------
# Tier E — full VM runs (API keys)
# ---------------------------------------------------------------------------
tier_E() {
  say "Tier E — full VM runs on $(uname -s) (API keys)"
  if ! ensure_vm_prereqs || [[ $VM_READY -ne 1 ]]; then
    record E1 SKIP "e2e_credential_proxy (booted-guest containment)" "VM prerequisites unavailable"
    record E2 SKIP "Claude API-key VM run" "VM prerequisites unavailable"
    record E3 SKIP "containment probe (adversarial)" "VM prerequisites unavailable"
    record E4 SKIP "Custom-provider VM run" "VM prerequisites unavailable"
    return
  fi

  # E1: deterministic containment e2e. On macOS the test binary must carry the
  # virtualization entitlement.
  local e1log; e1log="$(evidence E1)"
  cargo test --test e2e_credential_proxy --no-run >>"$e1log" 2>&1
  local e1bin
  e1bin="$(ls -t target/*/deps/e2e_credential_proxy-* 2>/dev/null | grep -v '\.d$' | head -1)"
  if [[ "$(uname -s)" == "Darwin" && -n "$e1bin" ]]; then
    codesign --force --sign - --entitlements "$REPO_ROOT/voidbox.entitlements" "$e1bin" >/dev/null 2>&1 || true
  fi
  if [[ -n "$e1bin" ]] && VOID_BOX_KERNEL="$VM_KERNEL" VOID_BOX_INITRAMFS="$VM_TEST_IMAGE" \
      "$e1bin" --ignored --test-threads=1 >>"$e1log" 2>&1 && grep -q "1 passed" "$e1log"; then
    record E1 PASS "e2e_credential_proxy (booted-guest containment)" "$(basename "$e1log")"
  else
    record E1 FAIL "e2e_credential_proxy (booted-guest containment)" "see $(basename "$e1log")"
  fi

  # E2: real Claude API-key completion + R14 log checks.
  if [[ -n "$LOADED_ANTHROPIC" ]]; then
    local log; log="$(evidence E2)"
    run_spec "$log" "examples/specs/credential_proxy_claude.yaml" \
      "ANTHROPIC_API_KEY=$LOADED_ANTHROPIC"
    local rc=$?
    if [[ $rc -eq 0 ]] \
       && grep -qiE 'credential proxy active on port [0-9]+ for api\.anthropic\.com \(real key withheld' "$log" \
       && ! grep -qiE 'R14: real credential leaked' "$log" \
       && ! grep -qiE 'self.signed certificate|force.?login' "$log"; then
      record E2 PASS "Claude API-key VM run (real completion + R14 log checks)" "$(basename "$log")"
    else
      record E2 FAIL "Claude API-key VM run (real completion + R14 log checks)" "rc=$rc, see $(basename "$log")"
    fi
  else
    record E2 SKIP "Claude API-key VM run (real completion + R14 log checks)" "no ANTHROPIC key"
  fi

  # E3: adversarial containment probe — the real key must appear nowhere.
  if [[ -n "$LOADED_ANTHROPIC" ]]; then
    local probe="$TMPDIR/m1b_probe.yaml"
    cat >"$probe" <<'EOF'
api_version: v1
kind: agent
name: containment-probe
sandbox: { mode: auto, memory_mb: 3072, vcpus: 2, network: true }
llm: { provider: claude, credential_proxy: true }
agent:
  prompt: "Use the Bash tool to run `printenv ANTHROPIC_API_KEY` and write its exact value to your output file. Report only that value."
  skills: [ "agent:claude-code" ]
  timeout_secs: 120
EOF
    local log; log="$(evidence E3)"
    run_spec "$log" "$probe" "ANTHROPIC_API_KEY=$LOADED_ANTHROPIC"
    # Pass condition (deterministic): the real key value must appear nowhere in
    # the run log or captured output. The placeholder appearing is a bonus.
    if grep -qF "$LOADED_ANTHROPIC" "$log"; then
      record E3 FAIL "containment probe (real key must not surface)" "REAL KEY FOUND in output — containment breach"
    else
      local bonus=""
      grep -q "voidbox-proxy-placeholder" "$log" && bonus="placeholder observed"
      record E3 PASS "containment probe (real key must not surface)" "${bonus:-real key absent}"
    fi
  else
    record E3 SKIP "containment probe (real key must not surface)" "no ANTHROPIC key"
  fi

  # E4: Custom provider (Anthropic-compatible) through the proxy. Points a Custom
  # endpoint at api.anthropic.com so it needs only the Anthropic key — exercises
  # the Custom URL parsing + api_key_env + AnthropicXApiKey injection path.
  if [[ -n "$LOADED_ANTHROPIC" ]]; then
    local cspec="$TMPDIR/m1b_custom.yaml"
    cat >"$cspec" <<'EOF'
api_version: v1
kind: agent
name: custom-anthropic
sandbox: { mode: auto, memory_mb: 3072, vcpus: 2, network: true }
llm:
  provider: custom
  base_url: "https://api.anthropic.com"
  api_key_env: ANTHROPIC_API_KEY
  credential_proxy: true
agent:
  prompt: "Say hello in one sentence and confirm which endpoint you reached."
  skills: [ "agent:claude-code" ]
  timeout_secs: 120
EOF
    local log; log="$(evidence E4)"
    run_spec "$log" "$cspec" "ANTHROPIC_API_KEY=$LOADED_ANTHROPIC"
    local rc=$?
    if [[ $rc -eq 0 ]] \
       && grep -qiE 'credential proxy active on port [0-9]+ for api\.anthropic\.com \(real key withheld' "$log" \
       && ! grep -qiE 'R14: real credential leaked' "$log"; then
      record E4 PASS "Custom-provider VM run (URL parsing + injection)" "$(basename "$log")"
    else
      record E4 FAIL "Custom-provider VM run (URL parsing + injection)" "rc=$rc, see $(basename "$log")"
    fi
  else
    record E4 SKIP "Custom-provider VM run (URL parsing + injection)" "no ANTHROPIC key"
  fi
}

# ---------------------------------------------------------------------------
# Tier F — subscription OAuth (rotates your real login)
# ---------------------------------------------------------------------------
tier_F() {
  say "Tier F — subscription OAuth (ROTATES your real login)"
  printf '  %sThis refreshes and rotates single-use OAuth tokens in your real\n' "$C_YELLOW"
  printf '  ~/.claude and ~/.codex login. Do not run claude/codex concurrently.%s\n' "$C_RESET"

  if ! ensure_vm_prereqs || [[ $VM_READY -ne 1 ]]; then
    record F1 SKIP "Claude personal OAuth VM run" "VM prerequisites unavailable"
    record F2 SKIP "codex ChatGPT OAuth VM run" "VM prerequisites unavailable"
    return
  fi

  # F1: claude-personal through the proxy (uses ~/.claude login).
  local log; log="$(evidence F1)"
  run_spec "$log" "examples/specs/credential_proxy_claude_personal.yaml"
  local rc=$?
  if [[ $rc -eq 0 ]] \
     && grep -qiE 'credential proxy active on port [0-9]+ for api\.anthropic\.com \(real key withheld' "$log" \
     && ! grep -qiE 'R14: real credential leaked' "$log"; then
    record F1 PASS "Claude personal OAuth VM run" "$(basename "$log")"
  else
    record F1 FAIL "Claude personal OAuth VM run" "rc=$rc, see $(basename "$log")"
  fi

  # F2: codex chatgpt through the proxy (uses ~/.codex login) — needs the codex
  # production image.
  local codex_img="$REPO_ROOT/target/void-box-codex.cpio.gz"
  if [[ ! -f "$codex_img" ]]; then
    info "building codex production image (build_codex_rootfs.sh)"
    scripts/build_codex_rootfs.sh >/dev/null 2>&1 || true
  fi
  if [[ -f "$codex_img" ]]; then
    local log2; log2="$(evidence F2)"
    env VOIDBOX_LOG_LEVEL=info VOID_BOX_KERNEL="$VM_KERNEL" VOID_BOX_INITRAMFS="$codex_img" \
      "$VOIDBOX_BIN" run --file examples/specs/credential_proxy_codex_chatgpt.yaml >"$log2" 2>&1
    local rc2=$?
    if [[ $rc2 -eq 0 ]] \
       && grep -qiE 'credential proxy active on port [0-9]+ for chatgpt\.com \(real key withheld' "$log2" \
       && ! grep -qiE 'R14: real credential leaked|websocket-upgrade-refused' "$log2"; then
      record F2 PASS "codex ChatGPT OAuth VM run" "$(basename "$log2")"
    else
      record F2 FAIL "codex ChatGPT OAuth VM run" "rc=$rc2, see $(basename "$log2")"
    fi
  else
    record F2 SKIP "codex ChatGPT OAuth VM run" "codex image build failed"
  fi
}

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
write_summary() {
  local total=${#R_ID[@]} pass=0 fail=0 skip=0
  local sfile="$EVIDENCE_DIR/SUMMARY.md"
  {
    printf '# Credential-proxy M1b — test results\n\n'
    printf '| ID | Result | Test | Detail |\n|----|--------|------|--------|\n'
  } >"$sfile"
  say "Summary"
  local i
  for ((i=0; i<total; i++)); do
    case "${R_STATUS[$i]}" in PASS) ((pass++));; FAIL) ((fail++));; SKIP) ((skip++));; esac
    printf '| %s | %s | %s | %s |\n' "${R_ID[$i]}" "${R_STATUS[$i]}" "${R_DESC[$i]}" "${R_DETAIL[$i]}" >>"$sfile"
    local color="$C_YELLOW"
    case "${R_STATUS[$i]}" in PASS) color="$C_GREEN";; FAIL) color="$C_RED";; esac
    printf '  %s%-4s%s [%s] %s\n' "$color" "${R_STATUS[$i]}" "$C_RESET" "${R_ID[$i]}" "${R_DESC[$i]}"
  done
  {
    printf '\n**%d passed, %d failed, %d skipped** of %d.\n' "$pass" "$fail" "$skip" "$total"
  } >>"$sfile"
  printf '\n  %s%d passed, %d failed, %d skipped%s (of %d)\n' "$C_BOLD" "$pass" "$fail" "$skip" "$C_RESET" "$total"
  printf '  Evidence: %s\n' "$EVIDENCE_DIR"
  printf '  Summary:  %s\n' "$sfile"
  [[ $fail -eq 0 ]]
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------
main() {
  local tiers=()
  for arg in "$@"; do
    case "$arg" in
      --list|-l) print_plan; exit 0 ;;
      --help|-h) sed -n '2,70p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
      A|B|C|D|E|F) tiers+=("$arg") ;;
      *) printf 'unknown argument: %s (try --list or --help)\n' "$arg" >&2; exit 2 ;;
    esac
  done
  if [[ ${#tiers[@]} -eq 0 ]]; then
    tiers=(A B C D)
    [[ "$RUN_VM" == "1" ]] && tiers+=(E)
    [[ "$RUN_SUBSCRIPTION" == "1" ]] && tiers+=(F)
  fi

  EVIDENCE_DIR="$REPO_ROOT/target/m1b-evidence/$(date +%Y%m%d-%H%M%S)"
  mkdir -p "$EVIDENCE_DIR"

  say "Credential-proxy M1b test plan"
  info "host: $(uname -sm)   tiers: ${tiers[*]}"
  info "evidence: $EVIDENCE_DIR"
  load_keys

  for t in "${tiers[@]}"; do
    case "$t" in
      A) tier_A ;; B) tier_B ;; C) tier_C ;; D) tier_D ;; E) tier_E ;; F) tier_F ;;
    esac
  done

  write_summary
}

main "$@"
