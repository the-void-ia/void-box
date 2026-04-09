# Codex Flavor PR 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bundle the OpenAI Codex CLI binary into the void-box guest initramfs and make it exec-able from a `kind: workflow` step. No `LlmProvider` changes — full agent integration is deferred to PR 2.

**Architecture:** Mirror `build_claude_rootfs.sh` for codex. Extract common rootfs setup (sandbox user, CA certs, finalize cpio) into `scripts/lib/agent_rootfs_common.sh` so the new `build_codex_rootfs.sh` and the refactored `build_claude_rootfs.sh` share it. Add `install_codex_binary()` to `scripts/lib/guest_common.sh` mirroring `install_claude_code_binary()`. Add `codex` to the guest exec allowlist. Codex is musl-static so no shared-library copying is needed.

**Tech Stack:** Bash 4+ (build scripts), Rust (one allowlist constant), YAML (smoke spec).

**Rust skills:** Apply `rust-style` skill to the Rust touchpoint. The skill applies even though the change is a single string literal — keep imports/constants at module scope (already enforced by the existing constant declaration at `src/backend/mod.rs:291`).

**Spec:** `docs/superpowers/specs/2026-04-07-codex-flavor-design.md`

---

## File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `scripts/lib/agent_rootfs_common.sh` | Create | Reusable: sandbox user, CA certs, finalize_initramfs |
| `scripts/build_claude_rootfs.sh` | Modify (line ~33 source line + lines 174-237 block replacement) | Source new lib, replace inline blocks |
| `scripts/lib/guest_common.sh` | Modify (after line 65) | Add `install_codex_binary()` |
| `scripts/build_guest_image.sh` | Modify (after line 84) | Call `install_codex_binary` |
| `scripts/build_codex_rootfs.sh` | Create | Codex flavor entry point |
| `src/backend/mod.rs` | Modify (line 294 area) | Add `"codex"` to `DEFAULT_COMMAND_ALLOWLIST` |
| `examples/specs/codex_workflow_smoke.yaml` | Create | `kind: workflow` smoke spec |
| `docs/agents/claude.md` | Create | Per-agent Claude flavor doc (extracted from AGENTS.md) |
| `docs/agents/codex.md` | Create | Per-agent Codex flavor doc (new) |
| `AGENTS.md` | Modify (Guest image build scripts section) | Replace claude paragraph with `@docs/agents/claude.md` and `@docs/agents/codex.md` discovery imports |
| `examples/README.md` | Modify | Index entry for codex_workflow_smoke.yaml |

**Sequencing rationale:** Task 1 lands the refactor in isolation (verify no claude regression). Task 2 adds the codex install function (no callers yet). Task 3 wires it into the base script. Task 4 creates the codex flavor entry point. Task 5 adds the allowlist. Task 6 adds the smoke spec. Task 7 updates docs. Each task ends in a commit so the branch history reflects independently revertable units.

---

### Task 1: Extract shared rootfs helpers and refactor `build_claude_rootfs.sh`

**Files:**
- Create: `scripts/lib/agent_rootfs_common.sh`
- Modify: `scripts/build_claude_rootfs.sh:33` (add `source` line) and `:174-237` (replace inline block, including the trailing Usage echo)

- [ ] **Step 1: Create the shared lib**

Create `scripts/lib/agent_rootfs_common.sh` with this exact content:

```bash
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
```

- [ ] **Step 2: Refactor `build_claude_rootfs.sh` to source the lib**

In `scripts/build_claude_rootfs.sh`, after the line `cd "$ROOT_DIR"` (~line 33), add:

```bash
source "$ROOT_DIR/scripts/lib/agent_rootfs_common.sh"
```

Then replace lines 174-237 (the `# ── Step 3 ──` block through the trailing `cargo run --example` Usage echo) with this block:

```bash
echo "[claude-rootfs] Extending image with CA certificates and sandbox user..."

# ── Step 3: Create sandbox user (uid 1000) ───────────────────────────────────
install_sandbox_user "$OUT_DIR"
echo "[claude-rootfs] Installed sandbox user"

# Create 'claude' symlink (base script installs as 'claude-code')
ln -sf claude-code "$OUT_DIR/usr/local/bin/claude"
echo "[claude-rootfs] Installed /usr/local/bin/claude symlink"

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
```

- [ ] **Step 3: Verify the refactor doesn't change the output**

Capture pre-refactor and post-refactor file lists (the cpio file list is the source of truth — file content shouldn't change because the lib functions are line-for-line equivalents).

Run before checking out the change:
```bash
git stash
scripts/build_claude_rootfs.sh
gzip -dc target/void-box-rootfs.cpio.gz | cpio -t 2>/dev/null | sort > /tmp/cpio.before
git stash pop
```

Then after applying Step 1 + Step 2:
```bash
scripts/build_claude_rootfs.sh
gzip -dc target/void-box-rootfs.cpio.gz | cpio -t 2>/dev/null | sort > /tmp/cpio.after
diff /tmp/cpio.before /tmp/cpio.after
```

Expected: empty diff. If the diff is non-empty, the refactor changed something — investigate before proceeding.

**Note for the implementer:** if `scripts/build_claude_rootfs.sh` cannot be run in your environment (e.g. no `claude` binary available, no internet for download), skip Step 3 and rely on Task 1's commit landing on a branch that CI runs `e2e_agent_mcp` against. Document the skip in the commit message.

- [ ] **Step 4: Commit Task 1**

```bash
git add scripts/lib/agent_rootfs_common.sh scripts/build_claude_rootfs.sh
git commit -m "$(cat <<'EOF'
scripts: extract shared agent_rootfs_common.sh

Lifts sandbox-user, CA-certs, and finalize-initramfs blocks from
build_claude_rootfs.sh into a sourced lib so future agent flavors
(codex, pi) don't duplicate them. Behavior unchanged — verified by
diffing the output cpio file list pre/post refactor.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: Add `install_codex_binary()` to `guest_common.sh`

**Files:**
- Modify: `scripts/lib/guest_common.sh:65` (insert after the `install_claude_code_binary` function, before the `# ── Shared-library copying` section)

- [ ] **Step 1: Add the function**

In `scripts/lib/guest_common.sh`, after the closing `}` of `install_claude_code_binary` (line 65) and before `# ── Shared-library copying`, insert:

```bash
# ── Codex CLI binary ──────────────────────────────────────────────────────────
# Codex is musl-static (no glibc shipping needed). Idempotent: only installs
# when CODEX_BIN env var is set, so default builds are unaffected.

install_codex_binary() {
  local bin="${CODEX_BIN:-}"
  if [[ -n "$bin" && -f "$bin" ]]; then
    echo "[void-box] Installing codex from \$CODEX_BIN at /usr/local/bin/codex..."
    cp "$bin" "$OUT_DIR/usr/local/bin/codex"
    chmod +x "$OUT_DIR/usr/local/bin/codex"
    return 0
  fi
  return 1
}
```

- [ ] **Step 2: Verify the function loads cleanly**

Run a syntax check and a smoke source:
```bash
bash -n scripts/lib/guest_common.sh
( source scripts/lib/guest_common.sh && type install_codex_binary )
```
Expected: no syntax errors; `install_codex_binary is a function`.

- [ ] **Step 3: Commit Task 2**

```bash
git add scripts/lib/guest_common.sh
git commit -m "$(cat <<'EOF'
scripts: add install_codex_binary helper

Mirrors install_claude_code_binary but skips the shared-library
copying step — codex Linux releases are musl-static. Idempotent:
gated on CODEX_BIN being set, so unrelated builds are unaffected.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: Wire `install_codex_binary` into `build_guest_image.sh`

**Files:**
- Modify: `scripts/build_guest_image.sh:84` (after the `install_claude_code_binary` block)

- [ ] **Step 1: Add the call**

In `scripts/build_guest_image.sh`, after the closing `fi` of the claude-code install block (line 84), add:

```bash
# Codex CLI: install binary if CODEX_BIN is set (musl-static, no libs needed)
install_codex_binary || true
```

The `|| true` is intentional — `install_codex_binary` returns 1 when `CODEX_BIN` is unset, which is the normal path for non-codex builds.

- [ ] **Step 2: Verify default builds are unaffected**

```bash
unset CODEX_BIN
bash -n scripts/build_guest_image.sh
```
Expected: no syntax errors. The function call is reachable but inert when `CODEX_BIN` is unset.

- [ ] **Step 3: Verify codex install path with a fake binary**

Create a fake ELF for the smoke check (uses `/bin/true` which is guaranteed to be a Linux ELF on Linux hosts):

```bash
fake_codex=$(mktemp)
cp /bin/true "$fake_codex"
CODEX_BIN="$fake_codex" \
  OUT_DIR=/tmp/void-box-codex-test \
  OUT_CPIO=/tmp/void-box-codex-test.cpio.gz \
  scripts/build_guest_image.sh 2>&1 | grep -E "(codex|claude)" | head -5
gzip -dc /tmp/void-box-codex-test.cpio.gz 2>/dev/null | cpio -t 2>/dev/null | grep codex
rm -rf /tmp/void-box-codex-test /tmp/void-box-codex-test.cpio.gz "$fake_codex"
```
Expected: stdout contains `Installing codex from $CODEX_BIN at /usr/local/bin/codex...` and the cpio listing contains `./usr/local/bin/codex`.

**Note for the implementer:** if `build_guest_image.sh` requires building `guest-agent`, `void-message`, `void-mcp` first and that's slow in your environment, run the verification once with a clean target dir and rely on the cached binaries for subsequent task verification.

- [ ] **Step 4: Commit Task 3**

```bash
git add scripts/build_guest_image.sh
git commit -m "$(cat <<'EOF'
scripts: install codex binary when CODEX_BIN is set

build_guest_image.sh now optionally installs the codex CLI alongside
claude-code. Inert when CODEX_BIN is unset, so default and existing
flavor builds are unaffected.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: Create `scripts/build_codex_rootfs.sh`

**Files:**
- Create: `scripts/build_codex_rootfs.sh`

- [ ] **Step 1: Write the script**

Create `scripts/build_codex_rootfs.sh` with this exact content:

```bash
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
#   OUT_DIR         Rootfs staging directory (default: target/void-box-rootfs)
#   OUT_CPIO        Output initramfs path (default: target/void-box-rootfs.cpio.gz)

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

source "$ROOT_DIR/scripts/lib/agent_rootfs_common.sh"

OUT_DIR="${OUT_DIR:-target/void-box-rootfs}"
OUT_CPIO="${OUT_CPIO:-target/void-box-rootfs.cpio.gz}"

# ── Step 1: Locate or download the codex binary ──────────────────────────────
CODEX_BIN="${CODEX_BIN:-}"

# Determine guest architecture (matches build_guest_image.sh logic).
HOST_ARCH="$(uname -m)"
case "$HOST_ARCH" in
  arm64) HOST_ARCH="aarch64" ;;
esac
GUEST_ARCH="${ARCH:-$HOST_ARCH}"

# Map guest arch to codex GitHub release asset suffix.
case "$GUEST_ARCH" in
  x86_64)  CODEX_TARGET="x86_64-unknown-linux-musl" ;;
  aarch64) CODEX_TARGET="aarch64-unknown-linux-musl" ;;
  *)       echo "ERROR: unsupported guest architecture: $GUEST_ARCH" >&2; exit 1 ;;
esac

# On macOS the locally installed codex is a Mach-O binary that won't run in
# the Linux guest. Force CODEX_VERSION-based download in that case.
IS_CROSS_BUILD=false
if [[ "$(uname -s)" == "Darwin" ]]; then
  IS_CROSS_BUILD=true
fi

if [[ -z "$CODEX_BIN" && "$IS_CROSS_BUILD" == "false" ]]; then
  LOCAL_CODEX="$(command -v codex 2>/dev/null || true)"
  if [[ -n "$LOCAL_CODEX" && -f "$LOCAL_CODEX" ]]; then
    CODEX_BIN="$(readlink -f "$LOCAL_CODEX")"
  fi
fi

if [[ -z "$CODEX_BIN" && -n "${CODEX_VERSION:-}" ]]; then
  # Download from openai/codex GitHub releases (Rust line: rust-v<version> tag).
  RELEASE_URL="https://github.com/openai/codex/releases/download/rust-v${CODEX_VERSION}/codex-${CODEX_TARGET}.tar.gz"
  DOWNLOAD_DIR="$ROOT_DIR/target/codex-download"
  mkdir -p "$DOWNLOAD_DIR"
  CACHED_BIN="$DOWNLOAD_DIR/codex-${CODEX_VERSION}-${CODEX_TARGET}"

  if [[ ! -f "$CACHED_BIN" ]]; then
    echo "[codex-rootfs] Downloading codex v${CODEX_VERSION} (${CODEX_TARGET})..."
    TMP_TAR="$(mktemp --suffix=.tar.gz)"
    if ! curl -fSL --progress-bar -o "$TMP_TAR" "$RELEASE_URL"; then
      echo "ERROR: Failed to download codex from $RELEASE_URL" >&2
      echo "  Check that version $CODEX_VERSION exists for $CODEX_TARGET." >&2
      rm -f "$TMP_TAR"
      exit 1
    fi
    TMP_EXTRACT="$(mktemp -d)"
    tar -xzf "$TMP_TAR" -C "$TMP_EXTRACT"
    EXTRACTED_BIN="$(find "$TMP_EXTRACT" -name codex -type f -executable | head -1)"
    if [[ -z "$EXTRACTED_BIN" ]]; then
      echo "ERROR: tarball did not contain a 'codex' executable" >&2
      ls -laR "$TMP_EXTRACT" >&2
      rm -rf "$TMP_TAR" "$TMP_EXTRACT"
      exit 1
    fi
    cp "$EXTRACTED_BIN" "$CACHED_BIN"
    chmod +x "$CACHED_BIN"
    rm -rf "$TMP_TAR" "$TMP_EXTRACT"
  else
    echo "[codex-rootfs] Using cached download: $CACHED_BIN"
  fi
  CODEX_BIN="$CACHED_BIN"
fi

if [[ -z "$CODEX_BIN" || ! -f "$CODEX_BIN" ]]; then
  echo "ERROR: codex binary not found." >&2
  echo "" >&2
  echo "Options:" >&2
  echo "  1. Install codex on your PATH (Linux host only)" >&2
  echo "  2. Set CODEX_BIN=/path/to/linux/codex (must be a Linux ELF binary)" >&2
  echo "  3. Set CODEX_VERSION=0.118.0 for automatic download" >&2
  exit 1
fi

# Verify it's an ELF binary (not a Mach-O or shell script).
if ! file -L "$CODEX_BIN" | grep -q "ELF.*executable"; then
  echo "ERROR: $CODEX_BIN is not a native Linux ELF binary." >&2
  echo "  file: $(file -L "$CODEX_BIN")" >&2
  if [[ "$IS_CROSS_BUILD" == "true" ]]; then
    echo "  On macOS, set CODEX_VERSION to download the Linux build:" >&2
    echo "    CODEX_VERSION=0.118.0 scripts/build_codex_rootfs.sh" >&2
  fi
  exit 1
fi

CODEX_SIZE="$(du -sh "$CODEX_BIN" | awk '{print $1}')"
echo "[codex-rootfs] Using codex binary: $CODEX_BIN ($CODEX_SIZE)"

# ── Step 2: Build base image (guest-agent, busybox, kernel modules, codex) ──
if [[ "$IS_CROSS_BUILD" == "false" ]]; then
  export BUSYBOX="${BUSYBOX:-/usr/bin/busybox}"
  if [[ ! -f "$BUSYBOX" ]]; then
    echo "[codex-rootfs] WARNING: busybox not found at $BUSYBOX; guest will have no /bin/sh"
    unset BUSYBOX
  fi
fi

# Pass the codex binary to the base script via CODEX_BIN.
# The base script handles copying it to /usr/local/bin/codex.
export CODEX_BIN
export OUT_DIR OUT_CPIO
echo "[codex-rootfs] Building base guest image..."
bash "$ROOT_DIR/scripts/build_guest_image.sh"

echo "[codex-rootfs] Extending image with CA certificates and sandbox user..."

# ── Step 3: Create sandbox user (uid 1000) ───────────────────────────────────
install_sandbox_user "$OUT_DIR"
echo "[codex-rootfs] Installed sandbox user"

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
```

- [ ] **Step 2: Make it executable and syntax-check**

```bash
chmod +x scripts/build_codex_rootfs.sh
bash -n scripts/build_codex_rootfs.sh
```
Expected: no output, no errors.

- [ ] **Step 3: Verify the discovery error message**

Run with no codex source available — expected to fail with the actionable error:
```bash
unset CODEX_BIN CODEX_VERSION
PATH="/usr/bin:/bin" scripts/build_codex_rootfs.sh 2>&1 | tail -10 || true
```
Expected: stderr contains "ERROR: codex binary not found." and the three numbered options.

- [ ] **Step 4: End-to-end build with a real codex binary**

If you have `CODEX_VERSION` access:
```bash
CODEX_VERSION=0.118.0 scripts/build_codex_rootfs.sh
gzip -dc target/void-box-rootfs.cpio.gz | cpio -t 2>/dev/null | grep -E "codex|claude" | sort
```
Expected listing contains `./usr/local/bin/codex`.

If running on macOS, this should still work via the GitHub download path.

**If `CODEX_VERSION=0.118.0` no longer exists when this task runs**, run `gh api repos/openai/codex/releases/latest --jq .tag_name` to find the current latest tag (the prefix is `rust-v`), then strip the `rust-v` prefix and use that value.

- [ ] **Step 5: Commit Task 4**

```bash
git add scripts/build_codex_rootfs.sh
git commit -m "$(cat <<'EOF'
scripts: add build_codex_rootfs.sh

Bundles the OpenAI Codex CLI into a void-box guest initramfs,
mirroring build_claude_rootfs.sh. Codex is musl-static so no glibc
shipping is needed. Discovery: CODEX_BIN env var, then PATH (Linux
host only), then CODEX_VERSION-based GitHub download.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 5: Add `codex` to the guest exec allowlist

**Files:**
- Modify: `src/backend/mod.rs:294-295` area (`DEFAULT_COMMAND_ALLOWLIST`)

- [ ] **Step 1: Locate the constant via LSP**

Per repo convention, prefer LSP over Grep for Rust navigation. Use `workspaceSymbol` to find `DEFAULT_COMMAND_ALLOWLIST` and `goToDefinition` to land on `src/backend/mod.rs`. The constant declaration is at line 291; the entries `"claude-code"` and `"claude"` are at lines 294-295.

- [ ] **Step 2: Insert `"codex"`**

In `src/backend/mod.rs`, find:

```rust
    "claude-code",
    "claude",
    "python3",
```

Replace with:

```rust
    "claude-code",
    "claude",
    "codex",
    "python3",
```

This places codex next to its sibling agent binaries, before the language-runtime entries.

- [ ] **Step 3: Verify the build**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```
Expected: all three pass with no new warnings or test failures. The change is a single string-literal addition to a `&[&str]` slice — no surprises possible.

- [ ] **Step 4: Commit Task 5**

```bash
git add src/backend/mod.rs
git commit -m "$(cat <<'EOF'
backend: allowlist codex CLI for guest exec

Adds "codex" to DEFAULT_COMMAND_ALLOWLIST so guest workflow steps
(and future kind: agent runs via LlmProvider::Codex) can exec the
bundled binary.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 6: Create the workflow smoke spec

**Files:**
- Create: `examples/specs/codex_workflow_smoke.yaml`

- [ ] **Step 1: Write the spec**

Create `examples/specs/codex_workflow_smoke.yaml` with this exact content:

```yaml
api_version: v1
kind: workflow
name: codex_workflow_smoke

# Smoke test: verify the bundled codex CLI binary is present, executable,
# and allowlisted. Does not exercise the OpenAI API — `codex --version`
# is a self-contained subcommand that prints the version and exits 0.
#
# Build the initramfs first:
#   CODEX_VERSION=0.118.0 scripts/build_codex_rootfs.sh
#
# Then run:
#   VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
#   VOID_BOX_INITRAMFS=$PWD/target/void-box-rootfs.cpio.gz \
#   cargo run --bin voidbox -- run --file examples/specs/codex_workflow_smoke.yaml

sandbox:
  memory_mb: 1024
  vcpus: 1
  network: false

workflow:
  steps:
    - name: codex_version
      run:
        program: codex
        args: ["--version"]

  output_step: codex_version
```

- [ ] **Step 2: Read the file back**

Re-read `examples/specs/codex_workflow_smoke.yaml` to verify the structure matches the existing examples (compare against `examples/specs/workflow.yaml` for shape: `kind: workflow`, `sandbox`, `workflow.steps`, `workflow.output_step`). The actual YAML parsing is exercised by Step 3 — if the spec doesn't parse, the `voidbox run` invocation will fail with a clear deserialization error before the VM boots.

- [ ] **Step 3: End-to-end run on Linux/KVM (requires built initramfs)**

```bash
CODEX_VERSION=0.118.0 scripts/build_codex_rootfs.sh
VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
VOID_BOX_INITRAMFS=$PWD/target/void-box-rootfs.cpio.gz \
cargo run --bin voidbox -- run --file examples/specs/codex_workflow_smoke.yaml
```
Expected: exit 0, output of the `codex_version` step contains a version string like `codex 0.118.0` or similar (exact format depends on the upstream `codex --version` output). If the output is empty or the run fails, investigate before committing.

**If you cannot run this end-to-end** (no KVM, no host kernel, no codex release binary), document that in the commit message and rely on PR review + CI to gate. Do NOT mark the task complete without at least Step 1 + Step 2 passing locally.

- [ ] **Step 4: Commit Task 6**

```bash
git add examples/specs/codex_workflow_smoke.yaml
git commit -m "$(cat <<'EOF'
examples: add codex workflow smoke spec

Single-step workflow that runs 'codex --version' inside the guest VM
to verify the bundled binary is present, executable, and allowlisted.
Self-contained — does not require OPENAI_API_KEY.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 7: Split per-agent docs and wire `@` discovery imports

**Files:**
- Create: `docs/agents/claude.md`
- Create: `docs/agents/codex.md`
- Modify: `AGENTS.md` (Guest image build scripts section, lines ~1072-1082)
- Modify: `examples/README.md`

**Why split:** future agent flavors (codex, pi, …) deserve their own doc files rather than accreting paragraphs in `AGENTS.md`. Using `@docs/agents/<name>.md` discovery imports lets the loader pull each per-agent doc on demand. This task lands the split for claude and codex; the existing inline `build_claude_rootfs.sh` paragraph in `AGENTS.md` is replaced by a pair of `@` imports.

- [ ] **Step 1: Create `docs/agents/claude.md`**

Create the file `docs/agents/claude.md` with this exact content:

```markdown
# Claude flavor — `scripts/build_claude_rootfs.sh`

Production Claude-capable rootfs/initramfs.

## What it bundles

- Native `claude-code` binary (Bun single-executable, glibc-linked).
- Glibc shared libraries auto-detected via `ldd`.
- SSL CA certificates for HTTPS API calls.
- `/etc/passwd` + `/etc/group` for the sandbox user (uid 1000).
- `/usr/local/bin/claude` symlink to `claude-code`.

## When to use

- Validating production-like Claude execution paths.
- OpenClaw Telegram gateway example runs.

## Discovery

The script locates the claude binary in priority order:

1. `CLAUDE_BIN` env var pointing at a Linux ELF binary.
2. `~/.local/bin/claude` or `claude` on PATH (Linux host only).
3. `CLAUDE_CODE_VERSION` set → automatic download of the Linux build
   from the official GCS bucket. On macOS, the version is
   auto-detected from the local install.

## Usage

```bash
scripts/build_claude_rootfs.sh

ANTHROPIC_API_KEY=sk-ant-... \
VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
VOID_BOX_INITRAMFS=$PWD/target/void-box-rootfs.cpio.gz \
cargo run --example claude_in_voidbox_example
```

## Validation

Required when changing the claude flavor or the shared
`scripts/lib/agent_rootfs_common.sh` helpers:

- Run `e2e_agent_mcp` (the agent-agnostic MCP integration test that
  uses Claude as the consumer):
  ```bash
  ANTHROPIC_API_KEY=... cargo test --test e2e_agent_mcp -- --ignored --test-threads=1
  ```
```

- [ ] **Step 2: Create `docs/agents/codex.md`**

Create the file `docs/agents/codex.md` with this exact content:

```markdown
# Codex flavor — `scripts/build_codex_rootfs.sh`

Production OpenAI-Codex-capable rootfs/initramfs.

## What it bundles

- The `codex` CLI binary (Rust musl-static, no shared libraries needed).
- SSL CA certificates for HTTPS API calls.
- `/etc/passwd` + `/etc/group` for the sandbox user (uid 1000).

## When to use

- Validating workflows that exec `codex` from a `kind: workflow` step.
- Future `kind: agent` runs with `provider: codex` (added in PR 2 of
  the Codex flavor effort — see
  `docs/superpowers/specs/2026-04-07-codex-flavor-design.md`).

## Discovery

The script locates the codex binary in priority order:

1. `CODEX_BIN` env var pointing at a Linux ELF binary.
2. `codex` on PATH (Linux host only — the macOS Mach-O binary cannot
   run inside the Linux guest).
3. `CODEX_VERSION` set → automatic download of the musl-static Linux
   build from the openai/codex GitHub releases (`rust-v<version>` tag).

## Usage

```bash
CODEX_VERSION=0.118.0 scripts/build_codex_rootfs.sh

OPENAI_API_KEY=sk-... \
VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
VOID_BOX_INITRAMFS=$PWD/target/void-box-rootfs.cpio.gz \
cargo run --bin voidbox -- run --file examples/specs/codex_workflow_smoke.yaml
```

## Validation

The smoke spec at `examples/specs/codex_workflow_smoke.yaml` runs
`codex --version` inside the guest VM, which is self-contained and
does not require `OPENAI_API_KEY`. This verifies the bundled binary
is present, executable, and allowlisted.
```

- [ ] **Step 3: Replace the `build_claude_rootfs.sh` paragraph in `AGENTS.md`**

In `AGENTS.md`, find the "## Guest image build scripts" section. Locate the `\`scripts/build_claude_rootfs.sh\`:` paragraph (currently around lines 1072-1077, ending with "Required for OpenClaw Telegram gateway example runs."). Replace that paragraph with this `@` import block:

```markdown
@docs/agents/claude.md

@docs/agents/codex.md
```

The `build_guest_image.sh` paragraph above (the base image) stays unchanged. The "Recommended default:" list at the end of the section also stays, but update its content to mention codex alongside claude:

Find:
```markdown
Recommended default:

- Use `build_guest_image.sh` for broad test cycles.
- Use `build_claude_rootfs.sh` for production gateway/runtime validation.
```

Replace with:
```markdown
Recommended default:

- Use `build_guest_image.sh` for broad test cycles.
- Use `build_claude_rootfs.sh` for production Claude gateway/runtime validation.
- Use `build_codex_rootfs.sh` for Codex CLI workflows.
```

- [ ] **Step 4: Add the examples/README.md entry**

In `examples/README.md`, find the section that lists `examples/specs/*.yaml` entries (look for `smoke_test.yaml` or `workflow.yaml` references). Add a new line for `codex_workflow_smoke.yaml`:

```markdown
- `specs/codex_workflow_smoke.yaml` — Single-step workflow that runs
  `codex --version` inside the guest VM. Verifies the bundled codex CLI
  binary is reachable and allowlisted. Build the initramfs with
  `CODEX_VERSION=0.118.0 scripts/build_codex_rootfs.sh`.
```

- [ ] **Step 5: Verify the markdown**

Read all four files back to confirm:
- Both new `docs/agents/*.md` files exist with the exact content above.
- The `AGENTS.md` section has the `@` imports replacing the inline claude paragraph, and the recommended-defaults list mentions codex.
- The `examples/README.md` entry matches the format of surrounding entries.

No tooling check needed — these are docs, not code.

- [ ] **Step 6: Commit Task 7**

```bash
git add docs/agents/claude.md docs/agents/codex.md AGENTS.md examples/README.md
git commit -m "$(cat <<'EOF'
docs: split per-agent flavor docs and wire @ discovery imports

Extracts the build_claude_rootfs.sh paragraph from AGENTS.md into a
new docs/agents/claude.md, adds a parallel docs/agents/codex.md for
the new flavor, and replaces the inline claude paragraph with @
discovery imports for both. Future agent flavors (pi, ...) follow
the same template.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Final validation

After all seven tasks have committed cleanly, run the full validation gate from `AGENTS.md`:

- [ ] **Step 1: Workspace fmt + clippy + tests**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```
Expected: all green. The only Rust change in PR 1 is the allowlist string literal, so test runtime should be dominated by compilation.

- [ ] **Step 2: Claude regression check**

Re-run `build_claude_rootfs.sh` and confirm the file list still matches pre-PR baseline (Task 1 Step 3 already did this once; a second pass after all other tasks confirms nothing else regressed):

```bash
scripts/build_claude_rootfs.sh
gzip -dc target/void-box-rootfs.cpio.gz | cpio -t 2>/dev/null | sort > /tmp/cpio.claude-after-pr1
# Compare against the saved baseline if available, or manually inspect.
```

- [ ] **Step 3: `e2e_agent_mcp` integration test**

If your environment supports it (Linux/KVM, ANTHROPIC_API_KEY available):
```bash
scripts/build_test_image.sh
export VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r)
export VOID_BOX_INITRAMFS=/tmp/void-box-test-rootfs.cpio.gz
ANTHROPIC_API_KEY=... cargo test --test e2e_agent_mcp -- --ignored --test-threads=1
```
Expected: passes. This is the gate that catches any accidental claude regression from the shared-lib refactor in Task 1.

- [ ] **Step 4: Codex workflow smoke**

```bash
CODEX_VERSION=0.118.0 scripts/build_codex_rootfs.sh
VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
VOID_BOX_INITRAMFS=$PWD/target/void-box-rootfs.cpio.gz \
cargo run --bin voidbox -- run --file examples/specs/codex_workflow_smoke.yaml
```
Expected: exit 0, output contains codex version string.

- [ ] **Step 5: Open PR**

Push the branch and open a PR titled something like `Codex flavor PR 1: bundled binary + workflow path`. Reference the spec in the PR body:

```
Implements PR 1 of the Codex flavor design:
docs/superpowers/specs/2026-04-07-codex-flavor-design.md

Scope:
- New scripts/lib/agent_rootfs_common.sh extracts shared rootfs setup.
- New scripts/build_codex_rootfs.sh bundles the codex musl binary.
- One allowlist line in src/backend/mod.rs.
- New examples/specs/codex_workflow_smoke.yaml.

Out of scope (deferred to PR 2):
- LlmProvider::Codex variant
- exec_claude_* → exec_agent_* rename
- kind: agent with provider: codex
```
