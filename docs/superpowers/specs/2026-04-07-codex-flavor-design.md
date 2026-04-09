# Codex flavor — design

Date: 2026-04-07 (revised 2026-04-08)
Status: Draft (multi-PR effort, sequenced)

> **Revision note:** The first draft of this spec assumed Codex could be
> added as a build-script change plus one allowlist line, with
> `agent.program: codex` selecting the binary. Reading the actual codebase
> (`src/llm.rs`, `src/sandbox/mod.rs`, `src/sandbox/local.rs`,
> `src/agent_box.rs`) revealed that the `kind: agent` runner is hardcoded
> to `claude-code` end-to-end: typed `ClaudeExecOpts`, `ClaudeExecResult`,
> stream-json JSONL parser, tool-call observer, Anthropic cost tracking,
> `--settings` / `--mcp-config` provisioning. There is no `agent.program`
> field. This document has been rewritten to reflect that reality and to
> sequence the work into independently landable PRs.

## Goal

Make OpenAI Codex CLI a first-class agent peer to Claude inside void-box.
A user should be able to write:

```yaml
api_version: v1
kind: agent
name: codex_smoke
sandbox:
  memory_mb: 3072
  vcpus: 2
  network: true
llm:
  provider: codex
agent:
  prompt: "Say exactly: Hello from void-box! Then stop."
  timeout_secs: 120
```

…and have void-box bundle the Codex binary, exec it inside the guest with
`OPENAI_API_KEY` injected, capture structured streaming output (tool calls,
tokens, cost), and surface the result through the same `StageResult`
machinery the Claude path uses.

## Non-goals

- pi flavor. Tracked separately; Codex is the first non-Claude agent peer.
- Mounting host `~/.codex` into the guest for ChatGPT-login auth. Deferred;
  first cut is `OPENAI_API_KEY` only.
- Bundling all agents in one polyglot image. Each flavor produces its own
  initramfs.
- Generalizing void-mcp, void-message, or the sidecar. They are already
  agent-agnostic infrastructure (see "What stays the same" below).

## What stays the same

The following void-box subsystems are **already agent-agnostic** and need
no changes to support Codex. They were designed alongside Claude, but the
infrastructure itself is not coupled to Claude:

- **`void-mcp`** — guest-side HTTP MCP server. Any MCP-capable agent
  (Claude, Codex, future) can connect to it via the same
  `127.0.0.1:8222/mcp` endpoint. The server doesn't know or care which
  agent is consuming it.
- **`void-message`** — guest-side CLI for sending sidecar intents. Any
  agent can shell out to it or import its semantics.
- **Sidecar** — host HTTP server reachable from the guest via the SLIRP
  gateway (`10.0.2.2:<port>`). Pure HTTP, agent-agnostic.
- **OCI rootfs / overlay / mount infrastructure**.
- **Snapshot / restore**.
- **Service mode** (`agent.mode: service`).

The only claude-coupled MCP bit is the **discovery mechanism**: how Claude
is told *where* the void-mcp server lives. Today
`agent_box.rs::provision_skills` writes `/workspace/.mcp.json` and
`agent_box.rs::run` passes `--mcp-config /workspace/.mcp.json` to
claude-code. That discovery hop is claude-specific and gets a Codex
counterpart in PR 4 (write `~/.codex/config.toml` pointing at the same
server). The void-mcp server itself, the sidecar, and void-message are
untouched.

## What "first-class peer" requires

The Claude path comprises four distinct claude-coupled concerns. Each
needs a Codex counterpart:

| # | Concern | Claude implementation | Codex equivalent |
|---|---|---|---|
| 1 | Binary in initramfs | `scripts/build_claude_rootfs.sh` + `install_claude_code_binary` in `scripts/lib/guest_common.sh` | `scripts/build_codex_rootfs.sh` + `install_codex_binary` |
| 2 | Guest exec allowlist | `"claude-code"`, `"claude"` in `DEFAULT_COMMAND_ALLOWLIST` (`src/backend/mod.rs:291`) | `"codex"` |
| 3 | Provider model + env injection | `LlmProvider::Claude` in `src/llm.rs` returning `ANTHROPIC_API_KEY`, `HOME` | `LlmProvider::Codex` returning `OPENAI_API_KEY`, `HOME` |
| 4 | Exec invocation + stream observer | `Sandbox::exec_claude_streaming` (`src/sandbox/mod.rs:404`) hardcodes `claude-code -p <prompt> --output-format stream-json --verbose`, parses Claude stream-json JSONL into `ClaudeExecResult`, and `agent_box.rs:702-718` adds claude-only `--settings` / `--mcp-config` flags. | A generic `exec_agent_streaming` that takes the binary name + arg builder from the provider, and a parallel `observe::codex` parser that writes into the same shared `AgentExecResult`. |

(1) and (2) are pure additions. (3) is a small `LlmProvider` enum
extension. (4) is a refactor of the exec layer that **renames**
`ClaudeExecOpts` → `AgentExecOpts`, `ClaudeExecResult` → `AgentExecResult`,
`ClaudeStreamEvent` → `AgentStreamEvent` as flat shared types — both
providers populate the same struct fields.

## Sub-PR sequence

The work is sequenced into **four PRs**, each independently landable and
testable. Each PR has its own implementation plan written separately
after the prior one lands and validates.

### PR 1 — Bundled binary + workflow path (no Rust agent integration)

**Goal:** Codex binary lands in the production initramfs and can be
exec'd from a `kind: workflow` step. No `LlmProvider` changes.

**Why this is a useful standalone PR:** the build-script work, the
discovery/download contract, the cross-build behavior on macOS, and the
allowlist entry are all independent of the LLM provider plumbing.
Landing them first means subsequent PRs can rely on a known-good
initramfs and focus exclusively on Rust changes.

**Files touched:**
- `scripts/lib/agent_rootfs_common.sh` (new) — extracted helpers:
  `install_sandbox_user`, `install_ca_certificates`, `finalize_initramfs`.
  Lifted verbatim from `build_claude_rootfs.sh:179-231`.
- `scripts/lib/guest_common.sh` — add `install_codex_binary()` mirroring
  `install_claude_code_binary()`. Codex is musl-static so no
  `install_codex_libs_*` is needed.
- `scripts/build_guest_image.sh` — call `install_codex_binary` after
  `install_claude_code_binary`. Idempotent: only acts when `CODEX_BIN`
  env var is set, so unrelated builds are unaffected.
- `scripts/build_claude_rootfs.sh` — refactored to source the new
  `agent_rootfs_common.sh` and replace inline sandbox-user / CA-cert /
  finalize blocks with function calls. Behavior unchanged.
- `scripts/build_codex_rootfs.sh` (new) — discovery, download, ELF
  check, base build invocation, common overlays, codex-flavored Usage
  block.
- `src/backend/mod.rs:291` — add `"codex"` to `DEFAULT_COMMAND_ALLOWLIST`.
- `examples/specs/codex_workflow_smoke.yaml` (new) — `kind: workflow`
  spec with one step running `codex --version`. Validates that the
  binary is reachable, executable, and allowlisted.
- `AGENTS.md` — one-paragraph "Codex flavor" subsection under "Guest
  image build scripts".
- `examples/README.md` — line for the new smoke spec.

**Codex binary discovery contract** (mirrors `CLAUDE_BIN`):
1. `CODEX_BIN` env var pointing at a Linux ELF → use it.
2. `command -v codex` (Linux host only; macOS skips because the Mach-O
   binary won't run inside the guest).
3. `CODEX_VERSION` set → download from GitHub:

   `https://github.com/openai/codex/releases/download/rust-v${CODEX_VERSION}/codex-${arch}-unknown-linux-musl.tar.gz`

   where `${arch}` is `x86_64` or `aarch64`. Confirmed against
   `gh api repos/openai/codex/releases/latest` (current latest tag at
   spec write time: `rust-v0.118.0`; asset names follow the pattern
   above). Cache extracted binary under `target/codex-download/`.
4. None of the above → error listing the three options.

**ELF check:** same `file -L | grep "ELF.*executable"` guard as Claude.
Codex musl binaries are statically linked, so no `ldd`/library copying
is needed; the dynamic-link path in `guest_linux.sh` stays claude-only.

**Validation gates for PR 1:**
- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- `cargo test --workspace --all-features` (one allowlist line — should
  be noise-free)
- `CODEX_VERSION=0.118.0 scripts/build_codex_rootfs.sh` produces an
  initramfs containing `/usr/local/bin/codex`. Verify with
  `gzip -dc target/void-box-rootfs.cpio.gz | cpio -t | grep codex`.
- `cargo run --bin voidbox -- run --file examples/specs/codex_workflow_smoke.yaml`
  on Linux/KVM produces the expected `codex --version` output.
- Re-run `build_claude_rootfs.sh` to confirm the refactor didn't change
  the output. Diff the file list of the produced cpio against
  pre-refactor (`gzip -dc … | cpio -t | sort`).
- `e2e_agent_mcp` still passes (closest existing gate for the claude
  path).

### PR 2 — `LlmProvider::Codex` + provider-aware exec (rename to AgentExec*)

**Goal:** A `kind: agent` spec with `provider: codex` exec's the codex
binary inside the guest with `OPENAI_API_KEY` injected. The streaming
observer is **passthrough** in this PR (raw stdout chunks forwarded
without parsing). Token / cost / tool-call accounting comes in PR 3.

**Files touched:**
- `src/llm.rs` — new `LlmProvider::Codex` enum variant. Implements:
  - `env_vars()` → `[("HOME", "/home/sandbox"), ("OPENAI_API_KEY", <host>)]`
  - `cli_args()` → codex-specific subcommand args (resolved during PR 2
    by reading `codex --help`)
  - `is_local()` → `false`
  - `requires_network()` → `true`
  - `binary_name()` → `"codex"` (new method on the enum; Claude variants
    return `"claude-code"`)
  - `supports_claude_settings()` → `false` for Codex, `true` for Claude
    variants. Used by `agent_box.rs` to gate the `--settings` flag.
  - Constructor `LlmProvider::codex()`
  - Serde rename so YAML `provider: codex` deserializes to
    `LlmProvider::Codex`.
- **Type renames** (flat shared types — no wrapper enum):
  - `ClaudeExecOpts` → `AgentExecOpts` (struct fields unchanged)
  - `ClaudeExecResult` → `AgentExecResult` (struct fields unchanged)
  - `ClaudeStreamEvent` → `AgentStreamEvent`
  - `StageResult.claude_result` field → `agent_result: AgentExecResult`
    (the struct that wraps the per-stage exec result in `agent_box.rs`).
  - All callers across `src/sandbox/mod.rs`, `src/sandbox/local.rs`,
    `src/agent_box.rs`, tests, doc comments.
  - `parse_stream_json` and `parse_jsonl_line` in `src/observe/claude.rs`
    keep their names; their return types change to `AgentExecResult` /
    `AgentStreamEvent`. The `observe::claude` module stays — it's still
    the Claude-specific parser.
- **Compat probe handling.** `Sandbox::verify_claude_code_compat` (called
  from `src/sandbox/mod.rs:286, 417` before exec) is a claude-specific
  pre-flight that asserts the bundled `claude-code` binary speaks the
  expected protocol version. PR 2 gates this call on
  `provider.binary_name() == "claude-code"` rather than running it
  unconditionally. The function itself is not renamed in PR 2 — it stays
  `verify_claude_code_compat` because it remains claude-specific. A
  future PR can introduce a parallel `verify_codex_compat` if needed.
- `src/sandbox/mod.rs` — rename `exec_claude` / `exec_claude_streaming`
  to `exec_agent` / `exec_agent_streaming`. The `prompt: &str` parameter
  stays. Add `binary_name: &str` (or read from `AgentExecOpts.binary` —
  picked during implementation).
- `src/sandbox/local.rs` — rename `exec_claude_internal` /
  `exec_claude_streaming_internal`. Parameterize the hardcoded
  `"claude-code"` literals at lines 285, 293, 362, 389.
- `src/agent_box.rs:702-718` — read binary name from
  `self.config.llm.binary_name()`. Gate the claude-only `--settings` /
  `--mcp-config` flags on `provider.supports_claude_settings()`. The
  `.mcp.json` file is still written by `provision_skills` (shared
  infra); only the *flag passed to the agent binary* is gated. PR 4
  adds the codex equivalent (`~/.codex/config.toml`).
- `src/spec.rs` — confirm `LlmSpec.provider: String` round-trips
  `"codex"` correctly through to `LlmProvider::Codex`.
- `examples/specs/codex_smoke.yaml` (new) — `kind: agent`,
  `provider: codex`, trivial prompt.

**Stream output handling in PR 2:** for Codex, `exec_agent_streaming`
forwards stdout chunks line-by-line to the structured logger
(`Observer::logger().info(...)`) without invoking `parse_jsonl_line`.
The returned `AgentExecResult` has empty `tool_calls`, zero
`input_tokens`/`output_tokens`/`total_cost_usd`, and `result_text`
populated from joined stdout. PR 3 fills in the structured fields.
The user-visible behavior in PR 2 — agent runs, output file is
written, exit code propagates — is preserved.

**Observer dispatch in PR 2:** the dispatch is a simple branch on
`provider.binary_name()` — `"claude-code"` invokes `parse_jsonl_line`,
anything else falls through to passthrough. PR 3 replaces this branch
with a dedicated `provider.observer_kind() -> ObserverKind` method that
returns a typed selector. Introducing the method in PR 2 would be dead
code; introducing it in PR 3 alongside the Codex parser keeps each PR
self-contained.

**Validation gates for PR 2:**
- Standard fmt/clippy/test sequence.
- New unit tests in `src/llm.rs::tests`: `test_codex_env_vars`,
  `test_codex_cli_args`, `test_codex_binary_name`, mirroring the
  Claude tests.
- New unit test in `src/spec.rs`: deserializing
  `llm:\n  provider: codex` produces `LlmProvider::Codex`.
- `e2e_agent_mcp` still passes — the rename touches every call site
  and this is the most important regression gate.
- Manual smoke: `examples/specs/codex_smoke.yaml` runs end-to-end on
  Linux/KVM with `OPENAI_API_KEY` set. Output file contains a
  recognizable response.

### PR 3 — `observe::codex` structured stream parser

**Goal:** Codex stdout is parsed into structured events (tool calls,
tokens, cost) so the streaming UI matches Claude's behavior.

**Files touched:**
- `src/observe/codex.rs` (new) — `parse_codex_stream` function and the
  Codex-specific event taxonomy. Mirrors `src/observe/claude.rs` but
  speaks Codex's `exec --json` event format. Resolved during PR 3
  implementation by capturing real event streams.
- `src/observe/mod.rs` — re-export.
- `src/sandbox/mod.rs::exec_agent_streaming` — dispatches to either
  `parse_jsonl_line` (Claude) or `parse_codex_event` (Codex) based on
  `provider.observer_kind()`.
- `src/agent_box.rs` — no signature change; the cost/token/tool-call
  summary line already reads from `AgentExecResult` after PR 2's
  rename, so it now shows real numbers for Codex.

**Validation gates for PR 3:**
- Standard fmt/clippy/test sequence.
- Unit tests for `parse_codex_stream` against captured fixture event
  streams (committed under `tests/fixtures/codex_events/`).
- Manual e2e: `examples/specs/codex_smoke.yaml` shows tool calls in
  the console summary line (`tools=N`), nonzero token counts, and a
  reasonable cost estimate.

### PR 4 — Codex MCP server discovery (small)

**Goal:** When a `kind: agent` spec uses `provider: codex` and includes
any `SkillKind::Mcp` skill, void-box writes a Codex-format pointer file
so Codex finds the existing void-mcp HTTP server.

**Note:** void-mcp itself is unchanged. Only the per-agent *discovery*
of the server is added. The shared `.mcp.json` written by
`provision_skills` still exists; this PR adds a parallel
`~/.codex/config.toml` `mcp_servers` section pointing at the same
server URL.

**Files touched:**
- `src/agent_box.rs::provision_skills` — when `provider == Codex` and
  any MCP skill is registered, also write `/home/sandbox/.codex/config.toml`
  with an `mcp_servers` table referencing `http://127.0.0.1:8222/mcp`
  (or whatever port `provision_skills` chose).
- New helper module or inline function for the per-server TOML
  serialization. JSON↔TOML translation is trivial (one nested table
  per server) and doesn't merit a generic library.

**Validation gates for PR 4:**
- Standard fmt/clippy/test sequence.
- Unit test: given a list of provisioned MCP servers, the function
  produces expected TOML.
- Manual e2e: a `provider: codex` spec with an MCP skill exec's a
  prompt that references an MCP tool and observes it being called.
  Not added to CI in PR 4.

## Decisions

| # | Decision | Rationale |
|---|---|---|
| Q1 | Shared base + thin overlays via `scripts/lib/agent_rootfs_common.sh`. | Avoids duplicating CA-cert / sandbox-user logic across N agent scripts. |
| Q2 | API key only for first cut; defer `~/.codex` host mount. | Matches how Claude landed. Pure additive follow-up. |
| Q3 | Refactor `build_claude_rootfs.sh` in PR 1, not later. | Duplication is small but load-bearing; once on `main`, the next agent author copies from whichever file they read first. |
| Q4 | Sequence as four PRs, not one atomic PR. | Each PR is independently testable and reviewable. PR 1 produces working software (codex via workflow steps). PRs 2–4 layer agent integration on top. |
| Q5 | PR 2 ships passthrough output, PR 3 adds the structured observer. | Avoids blocking the provider plumbing on parser work. The user-visible result file path is preserved by passthrough; only the streaming summary line is degraded in PR 2. |
| Q6 | Rename `exec_claude_*` → `exec_agent_*` in PR 2. | Per rust-style skill: descriptive names. Avoids creating a permanent split that pi would force a third copy of. |
| Q7 | **Flat rename** `ClaudeExecOpts` / `ClaudeExecResult` / `ClaudeStreamEvent` → `AgentExecOpts` / `AgentExecResult` / `AgentStreamEvent`. No wrapper enum. Both providers populate the same struct fields. | Shared flat types are simpler to consume and avoid pushing match arms onto every caller. Provider-specific behavior lives in the parser and arg-builder, not in the result type. |
| Q8 | void-mcp, void-message, sidecar are **untouched**. Only the per-agent discovery flag/file is added. | These are already agent-agnostic infrastructure. The only claude-coupled bit is the `--mcp-config /workspace/.mcp.json` discovery hop, which gets a `~/.codex/config.toml` counterpart in PR 4. |

## Open questions to resolve during PR 1 implementation

1. Exact non-interactive subcommand for Codex (`codex exec --json
   <prompt>` on stdin? positional? via file?). Confirmed during PR 1
   only matters for the smoke spec — the build script itself only
   needs `codex --version`.
2. Whether `build_guest_image.sh` should always attempt
   `install_codex_binary` (gated on `CODEX_BIN` being set), or whether
   the install should live only in `build_codex_rootfs.sh`. Decision:
   always attempt, gated on env var, so default builds are unaffected.

## Open questions to resolve during PR 2 implementation

1. Whether Codex consumes the prompt via stdin, a `-p` flag, a
   positional arg, or a file. Drives `exec_agent_streaming`'s
   arg-builder shape.
2. Whether `OPENAI_BASE_URL` should be propagated for users running
   against Azure OpenAI / proxies. Likely yes; mirror how Ollama's
   `ANTHROPIC_BASE_URL` works.
3. Whether `serde(rename = "codex")` is sufficient or whether the YAML
   parser at `src/spec.rs:87` (`pub provider: String`) needs a
   separate string-to-enum mapping. Confirm by reading how
   `claude-personal` currently round-trips.

## Open questions to resolve during PR 3 implementation

1. Codex `exec --json` event taxonomy. Resolved by capturing a real
   run.
2. How Codex reports token counts and cost (does it emit a final
   summary event like Claude's `result` event?). If not, void-box
   estimates from model + token counts using OpenAI's published
   pricing table — same shape as the Ollama zero-cost handling.

## Open questions to resolve during PR 4 implementation

1. Exact Codex `config.toml` schema for `mcp_servers`. Resolved by
   reading upstream Codex docs / source.

## Documentation

- `AGENTS.md` — adds Codex flavor subsection in PR 1, expands with
  provider documentation in PR 2.
- `examples/README.md` — adds entries for `codex_workflow_smoke.yaml`
  (PR 1) and `codex_smoke.yaml` (PR 2).
- No new top-level doc file.

## Implementation plans

This spec is the design. Each PR gets its own implementation plan
written by the writing-plans skill *after the previous PR has landed
and validated*. The plan for PR 1 is written immediately following
spec approval; plans for PRs 2–4 are written incrementally.

- PR 1 plan: `docs/superpowers/plans/2026-04-08-codex-flavor-pr1-impl.md`
- PR 2 plan: written after PR 1 lands.
- PR 3 plan: written after PR 2 lands.
- PR 4 plan: written after PR 3 lands.
