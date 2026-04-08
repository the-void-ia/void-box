# Codex flavor — design

Date: 2026-04-07
Status: Draft (pending implementation plan)

## Goal

Make OpenAI Codex CLI a first-class agent inside void-box, mirroring how
Claude Code is bundled today by `scripts/build_claude_rootfs.sh`. A user
should be able to:

```bash
CODEX_VERSION=<x.y.z> scripts/build_codex_rootfs.sh
OPENAI_API_KEY=sk-... \
VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
VOID_BOX_INITRAMFS=$PWD/target/void-box-rootfs.cpio.gz \
cargo run --bin voidbox -- run --file examples/specs/codex_smoke.yaml
```

…and have Codex execute inside the guest VM with `OPENAI_API_KEY` injected
from the host environment.

## Non-goals (this PR)

- pi flavor. Tracked separately; Codex is the first non-Claude agent.
- Mounting host `~/.codex` into the guest for ChatGPT-login auth. Deferred
  follow-up; first cut is API key only via env var injection.
- A combined "polyglot" image carrying multiple agents in one rootfs.
- Any change to `agent.program` resolution semantics in `src/spec.rs` beyond
  adding `codex` to the guest command allowlist.
- An OpenClaw-style Codex gateway example. Can come after the base flavor is
  validated end-to-end.
- A new `e2e_codex` ignored integration test. Requires `OPENAI_API_KEY` CI
  plumbing — follow-up after the build script lands.

## Context

Today the repo bundles exactly one agent as a first-class citizen:

- `scripts/build_guest_image.sh` — base initramfs (guest-agent, busybox,
  kernel modules).
- `scripts/build_claude_rootfs.sh` — extends the base with the native
  `claude` binary, sandbox user, CA certs, `/usr/local/bin/claude` symlink.

`build_claude_rootfs.sh` does several things that are not Claude-specific:

- Sandbox user creation (`/etc/passwd`, `/etc/group`, `/home/sandbox`).
- CA certificate installation with multi-distro symlinks.
- Final cpio + gzip + size reporting.

These are the duplication targets for the shared base.

The guest command allowlist lives at `DEFAULT_COMMAND_ALLOWLIST` in
`src/backend/mod.rs` and already lists `claude-code`, `void-mcp`, and others.

## Decisions

| # | Decision | Rationale |
|---|---|---|
| Q1 | Shared base + thin overlays. | Avoids duplicating CA-cert / sandbox-user logic across N agent scripts; matches how `build_claude_rootfs.sh` already layers on `build_guest_image.sh`. |
| Q2 | API key only for first cut; defer `~/.codex` host mount. | Matches how Claude landed (env var first). Keeps PR small; follow-up is purely additive. |
| Q3 | Refactor in this PR (extract `scripts/lib/agent_rootfs_common.sh`) rather than copy-now-refactor-later. | Duplication is small but load-bearing (auth + cert paths). Once duplicated on `main`, the next agent author will copy from whichever file they happened to read first. Claude path is gated by `e2e_claude_mcp` and OpenClaw validation, so regression risk is bounded. `scripts/lib/` directory already exists. |

## Architecture

```
build_guest_image.sh                          ← unchanged base
       │
       ├── scripts/lib/agent_rootfs_common.sh   ← NEW shared helpers
       │       • install_sandbox_user OUT_DIR
       │       • install_ca_certificates OUT_DIR
       │       • finalize_initramfs OUT_DIR OUT_CPIO
       │
       ├── build_claude_rootfs.sh   ← refactored to source the lib
       │       • locates/downloads claude ELF (claude-specific)
       │       • exports binary env var, runs base script
       │       • calls install_sandbox_user / install_ca_certificates
       │       • creates /usr/local/bin/claude symlink (claude-specific)
       │       • calls finalize_initramfs
       │
       └── build_codex_rootfs.sh    ← NEW, same shape
               • locates/downloads codex musl ELF (codex-specific)
               • exports binary env var, runs base script
               • calls install_sandbox_user / install_ca_certificates
               • calls finalize_initramfs
```

A sourced lib (rather than a parent dispatcher) keeps each flavor script
linear and readable. Agent-specific binary discovery, ELF checks, and the
macOS cross-build path stay in their own files. Only truly common steps move
into the lib.

### Wire-up to `build_guest_image.sh`

Today the base script accepts `CLAUDE_CODE_BIN` and copies the binary to
`/usr/local/bin/claude-code`. To support a second agent, the implementation
plan must read `build_guest_image.sh` and choose between two options:

1. **Generalize**: rename to `AGENT_BIN` + `AGENT_INSTALL_NAME` (or similar).
   Both flavor scripts set those. Cleanest end state.
2. **Add a parallel pair**: keep `CLAUDE_CODE_BIN` untouched, add
   `CODEX_BIN` + `CODEX_INSTALL_NAME` alongside. Smaller diff to the base
   script, leaves `CLAUDE_CODE_BIN` as legacy.

Option 1 is preferred unless inspection reveals it touches more than a
handful of lines.

## Components

### `scripts/lib/agent_rootfs_common.sh` (new)

Pure shell, sourced. No `set -e` of its own (caller owns that). Three
idempotent functions:

```
install_sandbox_user OUT_DIR
    Writes OUT_DIR/etc/passwd, OUT_DIR/etc/group with root + sandbox(uid=1000).
    Creates OUT_DIR/home/sandbox.
    Lifted verbatim from build_claude_rootfs.sh lines 179–187.

install_ca_certificates OUT_DIR
    Copies first-found system CA bundle to
    OUT_DIR/etc/ssl/certs/ca-certificates.crt and symlinks the four
    canonical paths. Returns 1 if no host bundle found — both Claude and
    Codex need TLS to reach their APIs.
    Lifted from build_claude_rootfs.sh lines 197–221.

finalize_initramfs OUT_DIR OUT_CPIO
    cpio + gzip, prints final + uncompressed size, prints the standard
    "Done." line. The agent-specific Usage block stays in each caller.
    Lifted from build_claude_rootfs.sh lines 223–231.
```

No new logic. Pure extraction.

### `scripts/build_claude_rootfs.sh` (refactor)

Sources the lib at the top, replaces lines 179–225 with three function
calls. Claude binary discovery, ELF check, macOS cross-build path, and the
`claude` symlink stay where they are — those are claude-specific. Net diff:
~50 lines removed, ~5 added, behavior unchanged.

### `scripts/build_codex_rootfs.sh` (new)

Mirrors `build_claude_rootfs.sh` exactly:

1. **Discovery**, in priority order:
   - `CODEX_BIN` env var → use it.
   - `command -v codex` → use it (Linux host only; macOS skips because the
     Mach-O binary won't run inside the Linux guest).
   - `CODEX_VERSION` → download the Linux musl release tarball from the
     OpenAI Codex GitHub releases and extract to
     `target/codex-download/`. Exact URL pattern to be confirmed against
     the upstream releases page during implementation.
2. **ELF check** — same `file -L | grep ELF.*executable` guard as Claude.
3. **Base build** — export the agent binary env var(s) (per the wire-up
   choice above), invoke `build_guest_image.sh`.
4. **Common overlays** — call `install_sandbox_user`,
   `install_ca_certificates` from the lib.
5. **Codex-specific** — symlink `/usr/local/bin/codex` only if the install
   name differs from `codex`. Likely a no-op.
6. **Finalize** — `finalize_initramfs`, then print a Codex-flavored Usage
   block (`OPENAI_API_KEY=...`).

### `src/backend/mod.rs` — allowlist

One string literal added to `DEFAULT_COMMAND_ALLOWLIST` for `codex`. No
other Rust changes. Use rust-analyzer LSP (`workspaceSymbol`) to locate the
constant; do not Grep.

### Spec / runtime

No changes to `src/spec.rs`, `src/runtime.rs`, or `src/agent_box.rs`. A
user runs Codex by writing:

```yaml
agent:
  program: codex
  args: ["exec", "--json", "..."]   # exact args TBD by Codex CLI semantics
  env:
    OPENAI_API_KEY: "${OPENAI_API_KEY}"
```

The existing env-injection and exec path handles it.

### `examples/specs/codex_smoke.yaml` (new)

The one new YAML in the PR. Mirrors `examples/specs/smoke_test.yaml` but
invokes `codex` with a trivial prompt to validate the flavor end-to-end.

### Documentation updates

- `AGENTS.md` — add a one-paragraph "Codex flavor" subsection under "Guest
  image build scripts", mirroring the existing Claude paragraph.
- `examples/README.md` — add a line for `examples/specs/codex_smoke.yaml`.

No new top-level doc file.

## Error handling

Build script posture: `set -euo pipefail`, fail loud with actionable hints.

| Failure | Behavior |
|---|---|
| `CODEX_BIN` set but file missing | Error, exit 1, name the var. |
| No `CODEX_BIN`, no `CODEX_VERSION`, no `codex` on PATH | Error listing the three options, mirroring `build_claude_rootfs.sh` lines 110–119. |
| macOS host with no `CODEX_VERSION` and no Linux build cached | Error pointing user at `CODEX_VERSION=...`, mirroring the Claude cross-build path. |
| Binary is not Linux ELF | `file -L` check, error with the actual `file` output. |
| GitHub release download fails | curl `-fSL`, `rm -f` partial, error with the URL. |
| No host CA bundle found | `install_ca_certificates` returns 1; caller exits. |

No silent fallbacks. Every diagnostic names the env var or path involved.

## Testing and validation

The Rust touchpoint is one allowlist line, so the standard validation
sequence from `AGENTS.md` is sufficient on Linux:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Plus a build-script smoke run on a Linux host:

```bash
CODEX_VERSION=<known-good> scripts/build_codex_rootfs.sh
# expect: target/void-box-rootfs.cpio.gz exists, contains /usr/local/bin/codex
```

And a manual end-to-end on Linux/KVM:

```bash
OPENAI_API_KEY=sk-... \
VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
VOID_BOX_INITRAMFS=$PWD/target/void-box-rootfs.cpio.gz \
cargo run --bin voidbox -- run --file examples/specs/codex_smoke.yaml
```

### Claude regression check

The claude-rootfs refactor is verified by:

1. Re-running `e2e_claude_mcp` (closest existing gate).
2. Manually running `build_claude_rootfs.sh` and diffing the resulting
   initramfs file list (`gzip -dc … | cpio -t`) against pre-refactor to
   confirm nothing dropped.

### Explicitly out of scope for this PR

- No new `e2e_codex` ignored test. Requires `OPENAI_API_KEY` CI plumbing.
  Follow-up.
- No `verify` skill run on this design doc — `verify` gates the
  implementation PR, not the spec.

## Open questions to resolve during planning

1. Exact GitHub release URL pattern for the Codex CLI musl tarball
   (`rust-v*` tag prefix? asset naming? checksum file?). Confirm against
   the upstream releases page before writing the discovery loop.
2. Whether `build_guest_image.sh` accepts a generic `AGENT_BIN` env var or
   is hardcoded to `CLAUDE_CODE_BIN`. Drives the "generalize vs parallel
   pair" decision in §Architecture/Wire-up.
3. Codex CLI exec arg conventions for non-interactive runs (`exec`,
   `--json`, model flag, etc.) — needed only for the smoke spec, not the
   build script.

These are implementation details, not design decisions. They will be
resolved by reading upstream sources during plan execution.
