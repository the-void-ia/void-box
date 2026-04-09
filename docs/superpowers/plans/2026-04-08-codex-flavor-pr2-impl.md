# Codex Flavor PR 2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire `LlmProvider::Codex` through the existing `kind: agent` exec path so a user can write `llm.provider: codex` in a spec and have void-box exec the bundled codex CLI with `OPENAI_API_KEY`, using passthrough stdout (structured observer deferred to PR 3).

**Architecture:** Flat rename of the claude-coupled types (`ClaudeExecOpts` → `AgentExecOpts`, `ClaudeExecResult` → `AgentExecResult`, `ClaudeStreamEvent` → `AgentStreamEvent`, `StageResult.claude_result` → `agent_result`) — no wrapper enum, both providers populate the same struct fields. The hardcoded `"claude-code"` binary literal in the exec path is replaced with `provider.binary_name()`, and the args construction is delegated to `provider.build_exec_args(prompt, opts)`. Claude-specific flags (`--settings`, `--mcp-config`, the `verify_claude_code_compat` probe) are gated on `provider.supports_claude_settings()` / `provider.binary_name() == "claude-code"`. Observer dispatch in PR 2 is a simple binary-name branch — Claude uses the existing stream-json parser, anything else (Codex) is passthrough. PR 3 replaces that branch with a typed `observer_kind()` selector alongside a new `observe::codex` parser.

**Tech Stack:** Rust (edition 2021), serde, tokio, existing `Sandbox` / `AgentExecOpts` / `Observer` APIs.

**Rust skills:** Apply `rust-style` and `rustdoc` skills to all Rust code. Apply `verify` skill before marking any implementation task complete.

**Spec:** `docs/superpowers/specs/2026-04-07-codex-flavor-design.md` (PR 2 section)

**Prerequisite:** PR 1 must be landed on `feat/codex` before starting PR 2. PR 1 bundles the codex binary and adds `"codex"` to `DEFAULT_COMMAND_ALLOWLIST`, both of which PR 2 depends on for end-to-end validation.

---

## Codex CLI contract (verified against upstream)

Confirmed by reading `codex-rs/exec/src/cli.rs` at the openai/codex repository:

- **Non-interactive subcommand**: `codex exec [OPTIONS] [PROMPT]`
- **Prompt delivery**: positional arg, or stdin if `-` is used as the positional (or if stdin is piped without a positional).
- **JSONL event stream to stdout**: `--json` (alias `--experimental-json`).
- **Bypass approvals in sandbox**: `--dangerously-bypass-approvals-and-sandbox` (alias `--yolo`). This is the Codex analog of Claude's `--dangerously-skip-permissions`. Required inside a void-box guest because there's no human to approve commands.
- **Skip git repo requirement**: `--skip-git-repo-check`. Required because the void-box guest's `/workspace` may not be a git repository.
- **Write final message to file**: `--output-last-message FILE`. Maps directly to void-box's `agent.output_file` pattern.
- **Model override**: `-m <MODEL>` / `--model <MODEL>`. Not wired in PR 2 — `LlmProvider::Codex` uses the upstream default.

**Minimal PR 2 invocation** inside the guest:

```
codex exec --json \
  --dangerously-bypass-approvals-and-sandbox \
  --skip-git-repo-check \
  --output-last-message /workspace/output.json \
  "<prompt text>"
```

(Env: `OPENAI_API_KEY` injected by `LlmProvider::Codex::env_vars()`.)

---

## File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `src/llm.rs` | Modify | Add `LlmProvider::Codex` variant, add methods `binary_name()`, `supports_claude_settings()`, `build_exec_args()`, extend constructors/tests |
| `src/observe/claude.rs` | Modify | Rename `ClaudeExecOpts`/`Result`/`StreamEvent` → `Agent*`; all internal uses and doc comments updated |
| `src/sandbox/mod.rs` | Modify | Rename `exec_claude`/`exec_claude_streaming` → `exec_agent`/`exec_agent_streaming`; take `&LlmProvider` parameter; delegate arg-building to the provider; branch observer dispatch on `binary_name()`; gate `verify_claude_code_compat` on binary name |
| `src/sandbox/local.rs` | Modify | Rename `exec_claude_internal`/`exec_claude_streaming_internal` → `exec_agent_internal`/`exec_agent_streaming_internal`; parameterize the hardcoded `"claude-code"` literal |
| `src/agent_box.rs` | Modify | Rename `StageResult.claude_result` → `agent_result`; gate `--settings`/`--mcp-config` flags on `provider.supports_claude_settings()`; pass `&self.config.llm` to exec_agent_streaming |
| `src/spec.rs` | Modify | Verify + test that `provider: codex` deserializes to `LlmProvider::Codex` |
| `src/runtime.rs` | Modify (likely) | String → `LlmProvider` conversion — add `"codex"` branch if not already string-keyed via serde |
| `examples/specs/codex_smoke.yaml` | Create | `kind: agent`, `provider: codex`, trivial prompt |
| `docs/agents/codex.md` | Modify | Update "When to use" and "Usage" sections — `kind: agent` is now supported |
| `AGENTS.md` | Modify (minor) | Short note that `provider: codex` is a real option in the MCP integration / agent sections |

---

### Task 1: Add `LlmProvider::Codex` variant and supporting methods

**Files:**
- Modify: `src/llm.rs` (enum declaration at line 50, constructors at ~line 108-162, `cli_args()` at ~line 205, `env_vars()` at ~line 222, `is_local()` at ~line 286, `requires_network()` at ~line 298, tests at ~line 346+)

This task is pure addition — no existing code is broken because the new variant isn't wired into any exec path yet. The exec wiring happens in Task 4 after the type renames land in Tasks 2-3.

- [ ] **Step 1: Read the existing `LlmProvider` enum and its methods**

```bash
# Use LSP, not grep — this is Rust code.
```

Use the LSP `documentSymbol` or `hover` on `src/llm.rs` to understand the current shape of `LlmProvider` (enum variants, impl block, methods). Note the existing variants: `Claude`, `Ollama`, `LmStudio`, `ClaudePersonal`, `Custom`. Note the existing methods: `env_vars`, `cli_args`, `is_local`, `requires_network`. Note any trait derives (`Debug`, `Default`, `Clone`).

- [ ] **Step 2: Write failing unit tests for the new variant**

In `src/llm.rs` (the existing `#[cfg(test)] mod tests` block at the end of the file), add these tests. Place them after the existing `test_claude_*` tests so reviewers can compare shapes easily.

```rust
    #[test]
    fn test_codex_env_vars() {
        let provider = LlmProvider::Codex;
        let vars = provider.env_vars();
        let mut keys: Vec<&str> = vars.iter().map(|(k, _)| k.as_str()).collect();
        keys.sort();
        assert_eq!(keys, vec!["HOME", "OPENAI_API_KEY"]);
        assert!(vars.iter().any(|(k, v)| k == "HOME" && v == "/home/sandbox"));
    }

    #[test]
    fn test_codex_binary_name() {
        let provider = LlmProvider::Codex;
        assert_eq!(provider.binary_name(), "codex");
    }

    #[test]
    fn test_claude_binary_name() {
        let provider = LlmProvider::Claude;
        assert_eq!(provider.binary_name(), "claude-code");
        let personal = LlmProvider::ClaudePersonal;
        assert_eq!(personal.binary_name(), "claude-code");
    }

    #[test]
    fn test_codex_supports_claude_settings() {
        let provider = LlmProvider::Codex;
        assert!(!provider.supports_claude_settings());
    }

    #[test]
    fn test_claude_supports_claude_settings() {
        assert!(LlmProvider::Claude.supports_claude_settings());
        assert!(LlmProvider::ClaudePersonal.supports_claude_settings());
    }

    #[test]
    fn test_codex_is_not_local() {
        assert!(!LlmProvider::Codex.is_local());
    }

    #[test]
    fn test_codex_requires_network() {
        assert!(LlmProvider::Codex.requires_network());
    }

    #[test]
    fn test_codex_build_exec_args_contains_prompt() {
        let provider = LlmProvider::Codex;
        let args = provider.build_exec_args("hello world", true, &[]);
        assert_eq!(args[0], "exec");
        assert!(args.contains(&"--json".to_string()));
        assert!(args.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()));
        assert!(args.contains(&"--skip-git-repo-check".to_string()));
        assert_eq!(args.last().unwrap(), "hello world");
    }

    #[test]
    fn test_codex_build_exec_args_without_skip_permissions() {
        let provider = LlmProvider::Codex;
        let args = provider.build_exec_args("prompt", false, &[]);
        assert!(!args.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()));
    }
```

- [ ] **Step 3: Run tests and verify they fail**

```bash
cargo test --package void-box --lib llm:: 2>&1 | tail -30
```

Expected: compile failures (no `LlmProvider::Codex` variant, no `binary_name()` / `supports_claude_settings()` / `build_exec_args()` methods). This is expected — we're doing TDD.

- [ ] **Step 4: Add the `Codex` variant**

In `src/llm.rs`, add `Codex` to the enum. Place it after the `ClaudePersonal` variant so sibling providers stay adjacent:

```rust
    /// Claude using personal OAuth credentials (from `claude auth login`).
    ///
    /// Unlike [`Claude`](LlmProvider::Claude), this does not require
    /// `ANTHROPIC_API_KEY`. Instead, the runtime discovers OAuth credentials
    /// from the host (macOS Keychain or `~/.claude/.credentials.json`) and
    /// mounts them into the guest at `/home/sandbox/.claude`.
    ClaudePersonal,

    /// OpenAI Codex CLI.
    ///
    /// Requires `OPENAI_API_KEY` in the host environment. The guest exec's
    /// the bundled `codex` binary (see `scripts/build_codex_rootfs.sh`) with
    /// `codex exec --json --dangerously-bypass-approvals-and-sandbox
    /// --skip-git-repo-check <prompt>`. Passthrough output — structured
    /// event parsing is deferred to PR 3.
    Codex,

    /// Any Anthropic-compatible API endpoint.
```

(Copy-paste the trailing `Custom` doc comment line — no change to it — so the patch is clean.)

- [ ] **Step 5: Add the `binary_name()` method**

Add this method to the `impl LlmProvider` block, placed near the other getter-style methods (`is_local`, `requires_network`). Match every existing variant explicitly — no wildcard per the rust-style skill:

```rust
    /// Binary name that the guest-agent exec's for this provider.
    ///
    /// Used by `Sandbox::exec_agent_streaming` to resolve which bundled
    /// agent binary to run inside the VM. Each flavor's `build_*_rootfs.sh`
    /// script installs the matching binary into `/usr/local/bin/`.
    pub fn binary_name(&self) -> &'static str {
        match self {
            LlmProvider::Claude => "claude-code",
            LlmProvider::ClaudePersonal => "claude-code",
            LlmProvider::Ollama { .. } => "claude-code",
            LlmProvider::LmStudio { .. } => "claude-code",
            LlmProvider::Custom { .. } => "claude-code",
            LlmProvider::Codex => "codex",
        }
    }
```

Rationale for the Claude-shaped variants all returning `"claude-code"`: `Ollama`, `LmStudio`, `Custom` are Claude-compatible proxies consumed by the `claude-code` binary via `ANTHROPIC_BASE_URL`. They are not separate agent flavors.

- [ ] **Step 6: Add the `supports_claude_settings()` method**

In the same `impl` block, immediately after `binary_name()`:

```rust
    /// Whether this provider understands the Claude-specific `--settings`
    /// and `--mcp-config` CLI flags.
    ///
    /// Claude and Claude-compatible proxies (Ollama, LmStudio, Custom)
    /// return `true`; Codex returns `false`. Used by `agent_box.rs` to gate
    /// flag emission on the exec command line.
    pub fn supports_claude_settings(&self) -> bool {
        match self {
            LlmProvider::Claude
            | LlmProvider::ClaudePersonal
            | LlmProvider::Ollama { .. }
            | LlmProvider::LmStudio { .. }
            | LlmProvider::Custom { .. } => true,
            LlmProvider::Codex => false,
        }
    }
```

- [ ] **Step 7: Add the `build_exec_args()` method**

Add a new `pub fn build_exec_args()` method on `LlmProvider`. The existing `cli_args()` stays in place — it becomes an internal helper called from within `build_exec_args()` for the Claude-shaped variants to append the `--model <name>` args for Ollama / LmStudio / Custom providers.

PR 2's generalization point is `build_exec_args()`: it returns the complete argv (subcommand, flags, prompt) for whichever provider. The exec path in `src/sandbox/mod.rs` calls it and no longer inlines the `[-p, prompt, --output-format, ...]` array.

Add this method to the `impl LlmProvider` block, placed above the existing `cli_args()`:

```rust
    /// Build the full `exec` argument vector for this provider.
    ///
    /// Returns the complete args list (subcommand, flags, prompt) that the
    /// guest-agent passes to the agent binary. The caller pairs this with
    /// `binary_name()` to form the full exec invocation.
    ///
    /// - `prompt`: the user prompt text.
    /// - `dangerously_skip_permissions`: whether to pass the bypass-approvals
    ///   flag (Claude's `--dangerously-skip-permissions` or Codex's
    ///   `--dangerously-bypass-approvals-and-sandbox`).
    /// - `extra_args`: caller-supplied extra args appended at the end.
    pub fn build_exec_args(
        &self,
        prompt: &str,
        dangerously_skip_permissions: bool,
        extra_args: &[String],
    ) -> Vec<String> {
        match self {
            LlmProvider::Claude
            | LlmProvider::ClaudePersonal
            | LlmProvider::Ollama { .. }
            | LlmProvider::LmStudio { .. }
            | LlmProvider::Custom { .. } => {
                let mut args = vec![
                    "-p".to_string(),
                    prompt.to_string(),
                    "--output-format".to_string(),
                    "stream-json".to_string(),
                    "--verbose".to_string(),
                ];
                if dangerously_skip_permissions {
                    args.push("--dangerously-skip-permissions".to_string());
                }
                for provider_arg in self.cli_args() {
                    args.push(provider_arg);
                }
                for extra in extra_args {
                    args.push(extra.clone());
                }
                args
            }
            LlmProvider::Codex => {
                let mut args = vec![
                    "exec".to_string(),
                    "--json".to_string(),
                    "--skip-git-repo-check".to_string(),
                ];
                if dangerously_skip_permissions {
                    args.push("--dangerously-bypass-approvals-and-sandbox".to_string());
                }
                for extra in extra_args {
                    args.push(extra.clone());
                }
                args.push(prompt.to_string());
                args
            }
        }
    }
```

- [ ] **Step 8: Update `env_vars()` to handle the Codex variant**

Find the `env_vars()` method and add a `Codex` arm. Place it alphabetically (after `ClaudePersonal`, before `Ollama`) or at the end — match the order of the enum variant declaration.

```rust
            LlmProvider::Codex => {
                let mut vars = vec![
                    ("HOME".into(), "/home/sandbox".into()),
                ];
                if let Ok(key) = std::env::var("OPENAI_API_KEY") {
                    vars.push(("OPENAI_API_KEY".into(), key));
                }
                vars
            }
```

- [ ] **Step 9: Update `is_local()` and `requires_network()` for `Codex`**

Find both methods. Add the `Codex` variant explicitly per the rust-style skill (no wildcard matches):

```rust
    pub fn is_local(&self) -> bool {
        match self {
            LlmProvider::Claude | LlmProvider::ClaudePersonal | LlmProvider::Codex => false,
            LlmProvider::Ollama { .. } | LlmProvider::LmStudio { .. } => true,
            LlmProvider::Custom { .. } => false,
        }
    }
```

```rust
    pub fn requires_network(&self) -> bool {
        // All providers need network: Claude for api.anthropic.com,
        // Ollama/LmStudio for the SLIRP gateway, Codex for api.openai.com.
        true
    }
```

(If `requires_network()` currently ignores the enum variant and always returns `true`, no code change is needed for this step — just verify.)

- [ ] **Step 10: Update `cli_args()` for the `Codex` variant**

`cli_args()` returns provider-specific `--model`-style args that the claude-compatible code path appends. Codex doesn't consume this — it uses its own `-m` flag and the existing `cli_args()` returns are claude-shape-specific. For `Codex`, return an empty vec:

```rust
    pub(crate) fn cli_args(&self) -> Vec<String> {
        match self {
            LlmProvider::Claude
            | LlmProvider::ClaudePersonal
            | LlmProvider::Codex => Vec::new(),
            LlmProvider::Ollama { model, .. } => vec!["--model".into(), model.clone()],
            LlmProvider::LmStudio { model, .. } => vec!["--model".into(), model.clone()],
            LlmProvider::Custom { model: Some(m), .. } => vec!["--model".into(), m.clone()],
            LlmProvider::Custom { model: None, .. } => Vec::new(),
        }
    }
```

- [ ] **Step 11: Run the unit tests and verify they pass**

```bash
cargo test --package void-box --lib llm:: 2>&1 | tail -30
```

Expected: all new `test_codex_*` and `test_claude_binary_name` / `test_claude_supports_claude_settings` tests pass. No existing tests break.

If existing tests break because they don't match the new arms (e.g. a `match` on `LlmProvider` somewhere was wildcarded and is now unreachable), fix the `match` to include `Codex`. Do NOT add wildcard arms — per the rust-style skill, match all variants explicitly.

- [ ] **Step 12: Run the full workspace test sweep**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Expected: all green. The new variant and methods are additive — no exec path yet consumes them, so nothing should break.

- [ ] **Step 13: Commit Task 1**

```bash
git add src/llm.rs
git commit -m "$(cat <<'EOF'
llm: add LlmProvider::Codex variant

New enum variant for OpenAI Codex CLI. Adds provider-aware methods
binary_name(), supports_claude_settings(), and build_exec_args() used
by PR 2's exec-path generalization. Claude-shaped variants (Claude,
ClaudePersonal, Ollama, LmStudio, Custom) all share the claude-code
binary name because they go through the same Bun binary via
ANTHROPIC_BASE_URL. Only Codex is its own flavor.

The new methods are additive — no caller is wired to binary_name()
or supports_claude_settings() yet. PR 2 Task 4 (exec path rename)
consumes them.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: Flat rename `ClaudeExecOpts`/`Result`/`StreamEvent` → `Agent*`

**Files:**
- Modify: `src/observe/claude.rs` — type declarations, their internal uses, doc comments
- Modify: `src/sandbox/mod.rs` — imports and method signatures using the renamed types
- Modify: `src/sandbox/local.rs` — imports and internal method signatures
- Modify: `src/agent_box.rs` — imports and call sites
- Modify: any test file that imports these types — find via rust-analyzer `findReferences`

This is a single-commit mechanical rename. The whole workspace must compile at the end of this commit (intermediate states may not, so the rename happens in one editing session before the build is re-run).

- [ ] **Step 1: Find all references via LSP**

Open `src/observe/claude.rs` and use `findReferences` on each of the three type declarations: `ClaudeExecOpts`, `ClaudeExecResult`, `ClaudeStreamEvent`. Collect the file:line list of every reference — callers, imports, doc comments, test assertions. Expect ~15-25 sites across the workspace.

Also `findReferences` on `parse_stream_json` and `parse_jsonl_line` — these keep their names, but their return types change, so any caller that binds the return value by explicit type annotation needs updating.

- [ ] **Step 2: Rename the type declarations in `src/observe/claude.rs`**

In `src/observe/claude.rs`, rename the three struct/enum declarations:

- `pub struct ClaudeExecOpts { ... }` → `pub struct AgentExecOpts { ... }`
- `pub struct ClaudeExecResult { ... }` → `pub struct AgentExecResult { ... }`
- `pub enum ClaudeStreamEvent { ... }` → `pub enum AgentStreamEvent { ... }`

Also rename any internal uses within the same file (the `parse_stream_json` and `parse_jsonl_line` signatures, any tests).

Any doc comments inside this file that mention `ClaudeExec*` should be updated to `AgentExec*` for consistency — but keep the **words** "Claude" and "claude-code" where they refer to the actual agent being parsed (e.g. "parses Claude's stream-json output format"). Only rename the **type identifiers**.

- [ ] **Step 3: Update `src/sandbox/mod.rs`**

Find every `ClaudeExec*` / `ClaudeStreamEvent` reference in `src/sandbox/mod.rs` and rename to the new type names. Common locations:

- `use crate::observe::claude::{...}` imports
- `pub async fn exec_claude(..., opts: ClaudeExecOpts) -> Result<ClaudeExecResult>` method signature
- `pub async fn exec_claude_streaming(..., opts: ClaudeExecOpts, on_event: F) -> Result<ClaudeExecResult>` method signature
- `F: FnMut(ClaudeStreamEvent)` bound
- Any `let opts: ClaudeExecOpts = ...` bindings
- Doc comments on the public methods

Leave the method names (`exec_claude` / `exec_claude_streaming`) alone for this task — they are renamed in Task 4.

- [ ] **Step 4: Update `src/sandbox/local.rs`**

Same treatment for `src/sandbox/local.rs`. The internal methods `exec_claude_internal` and `exec_claude_streaming_internal` use the renamed types in their return channels. Update imports and signatures.

- [ ] **Step 5: Update `src/agent_box.rs`**

In `src/agent_box.rs`, rename:

- `use crate::observe::claude::{ClaudeExec...};` imports
- The `ClaudeExecOpts { ... }` struct literal used at ~line 724 when calling `exec_claude_streaming`
- The `ClaudeStreamEvent::ToolUse(ref tc)` pattern in the `on_event` callback at ~line 731
- The `claude_result: ClaudeExecResult` field in `StageResult` → **`agent_result: AgentExecResult`**. Update the struct definition and all constructors/field accesses. Grep for `.claude_result` and `claude_result:` to find the construction sites and any downstream readers (likely in `src/daemon.rs`, `src/pipeline.rs`, tests).
- Any local bindings like `let mut claude_result = ...` → `let mut agent_result = ...`
- The `claude_result.is_error`, `claude_result.tool_calls.len()`, `claude_result.total_cost_usd` field accesses in the `eprintln!` at ~line 749

- [ ] **Step 6: Update `src/daemon.rs`, `src/pipeline.rs`, and any test files**

Grep for remaining `ClaudeExecOpts`, `ClaudeExecResult`, `ClaudeStreamEvent`, `claude_result:` references:

```bash
# Use Grep tool, not raw rg. Per CLAUDE.md: "Prefer LSP operations over Grep/Glob for Rust code navigation; fall back to Grep/Glob only for comments, config files, and non-Rust files."
# For a rename cleanup pass after LSP findReferences, grep is appropriate to verify zero remaining hits.
```

Use the Grep tool with pattern `ClaudeExecOpts|ClaudeExecResult|ClaudeStreamEvent|claude_result` across `src/` and `tests/`. Update every remaining reference to the new names. Re-run grep and confirm zero hits.

**Important exceptions that should NOT be renamed:**
- The `observe::claude` module path stays — it's the Claude-specific parser, not a generic type.
- References to `claude-code` (the binary) stay.
- References to `Claude` the LLM provider (e.g. `LlmProvider::Claude`, `"claude-code"` string literals) stay.
- Doc comments that describe claude-specific behavior stay (e.g. "parses Claude's stream-json format").

- [ ] **Step 7: Run `cargo check` repeatedly until the workspace compiles**

```bash
cargo check --workspace --all-targets --all-features 2>&1 | head -80
```

Expected: may take multiple iterations. Each error names a missing rename or an incompatible type. Fix and re-run until the output is `Finished` with no errors.

- [ ] **Step 8: Run the full test sweep**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Expected: all green. No behavior change — the rename is type-only.

- [ ] **Step 9: Commit Task 2**

```bash
git add -u
git commit -m "$(cat <<'EOF'
observe,sandbox,agent_box: rename ClaudeExec* types to AgentExec*

Flat rename of the three claude-coupled types produced by the
observe::claude parser (ClaudeExecOpts → AgentExecOpts,
ClaudeExecResult → AgentExecResult, ClaudeStreamEvent →
AgentStreamEvent) plus the StageResult.claude_result field →
agent_result. No wrapper enum — both providers will populate the
same struct fields. The observe::claude module stays; only the
type identifiers it produces change.

This is a pure rename: no behavior, no public API change beyond the
type names.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: Rename `exec_claude*` methods → `exec_agent*`, parameterize binary name

**Files:**
- Modify: `src/sandbox/mod.rs` — rename `exec_claude` → `exec_agent`, `exec_claude_streaming` → `exec_agent_streaming`; take `&LlmProvider` parameter; delegate arg building to `provider.build_exec_args()`
- Modify: `src/sandbox/local.rs` — rename `exec_claude_internal` → `exec_agent_internal`, `exec_claude_streaming_internal` → `exec_agent_streaming_internal`; replace hardcoded `"claude-code"` with `binary: &str` parameter
- Modify: `src/agent_box.rs` — update call sites to pass `&self.config.llm`
- Modify: any test files

- [ ] **Step 1: Rename `exec_claude_internal` in `src/sandbox/local.rs`**

Find the method around line 278. Current signature:

```rust
    pub(crate) async fn exec_claude_internal(
        &self,
        args: &[&str],
        extra_env: &[(String, String)],
        timeout_secs: Option<u64>,
    ) -> Result<ExecOutput> {
```

Change to:

```rust
    pub(crate) async fn exec_agent_internal(
        &self,
        binary: &str,
        args: &[&str],
        extra_env: &[(String, String)],
        timeout_secs: Option<u64>,
    ) -> Result<ExecOutput> {
```

Inside the function body, replace every hardcoded `"claude-code"` literal with `binary`. Check lines ~285, ~293 per the committed code in `src/sandbox/local.rs`. After the rename:

- Line ~285: `return self.simulate_exec(binary, args, &[]);`
- Line ~293: `.exec(binary, args, &[], &env, None, timeout_secs)`

- [ ] **Step 2: Rename `exec_claude_streaming_internal` in `src/sandbox/local.rs`**

Find the method around line 349. Change the name to `exec_agent_streaming_internal`, add `binary: &str` as the first parameter after `&self`, and replace the hardcoded `"claude-code"` literals in the function body (around lines ~362, ~389) with `binary`.

- [ ] **Step 3: Rename `exec_claude` in `src/sandbox/mod.rs`**

Find `pub async fn exec_claude(` around line 280. Current signature:

```rust
    pub async fn exec_claude(
        &self,
        prompt: &str,
        opts: crate::observe::claude::AgentExecOpts,
    ) -> Result<crate::observe::claude::AgentExecResult> {
```

(After Task 2, the types are already renamed to `AgentExecOpts`/`AgentExecResult`.)

Rename the method to `exec_agent` and add a `provider: &crate::llm::LlmProvider` parameter. The new signature:

```rust
    pub async fn exec_agent(
        &self,
        provider: &crate::llm::LlmProvider,
        prompt: &str,
        opts: crate::observe::claude::AgentExecOpts,
    ) -> Result<crate::observe::claude::AgentExecResult> {
```

Inside the body (around lines 285-395):

- Replace the claude-specific compat probe `self.verify_claude_code_compat(local, &opts.env).await?;` with a gated version:
  ```rust
  if provider.binary_name() == "claude-code" {
      self.verify_claude_code_compat(local, &opts.env).await?;
  }
  ```
- Replace the inline `let mut args = vec![...]` construction (lines 289-295) with delegation to the provider:
  ```rust
  let args: Vec<String> = provider.build_exec_args(
      prompt,
      opts.dangerously_skip_permissions,
      &opts.extra_args,
  );
  ```
  Remove the subsequent `if opts.dangerously_skip_permissions { args.push(...); }` block and the `for extra in &opts.extra_args { args.push(...); }` loop — both are now handled inside `build_exec_args`.
- Replace the call to `exec_claude_internal` (around line 312) with `exec_agent_internal(provider.binary_name(), &args_refs, ...)`.
- Replace the call to `mock.exec_with_stdin("claude-code", &args_refs, &[])` (around line 316) with `mock.exec_with_stdin(provider.binary_name(), &args_refs, &[])`.
- **Observer dispatch**: the non-streaming path at the end of the function (lines ~361-395) calls `parse_stream_json(&output.stdout)` unconditionally. Gate this:
  ```rust
  let result = if provider.binary_name() == "claude-code" {
      crate::observe::claude::parse_stream_json(&output.stdout)
  } else {
      // Passthrough: Codex or any non-Claude provider. PR 3 adds the
      // structured parser; for now, populate only result_text from stdout.
      let mut result = crate::observe::claude::AgentExecResult::default();
      result.result_text = String::from_utf8_lossy(&output.stdout).into_owned();
      result.is_error = output.exit_code != 0;
      result
  };
  ```
  (`AgentExecResult` must implement `Default`. If it doesn't, add `#[derive(Default)]` in `src/observe/claude.rs` — which is safe because all its fields are owned and already default-able, or add explicit `Default` impl if any field is non-default.)

- [ ] **Step 4: Rename `exec_claude_streaming` in `src/sandbox/mod.rs`**

Find `pub async fn exec_claude_streaming<F>(` around line 404. Apply the same treatment:

- Rename to `exec_agent_streaming`.
- Add `provider: &crate::llm::LlmProvider` parameter (first after `&self`).
- Gate the compat probe on `provider.binary_name() == "claude-code"`.
- Delegate arg building to `provider.build_exec_args(...)`.
- Inside the match arm that calls `local.exec_claude_streaming_internal(&args_refs, &opts.env, opts.timeout_secs)`, update to `local.exec_agent_streaming_internal(provider.binary_name(), &args_refs, &opts.env, opts.timeout_secs)`.
- **Observer dispatch**: the streaming path loops over JSONL lines and calls `parse_jsonl_line`. Gate this:
  ```rust
  if provider.binary_name() == "claude-code" {
      // existing parse_jsonl_line + on_event loop
  } else {
      // Passthrough: forward stdout lines to the structured logger.
      while let Some(chunk) = chunk_rx.recv().await {
          if chunk.stream == "stdout" {
              for line in String::from_utf8_lossy(&chunk.data).lines() {
                  // Minimal forward — PR 3 adds a codex observer.
                  tracing::info!(target: "agent_stdout", "{}", line);
              }
          }
      }
      // Wait for the final ExecResponse to populate result_text / is_error.
      let response = resp_rx.await??;
      let mut result = crate::observe::claude::AgentExecResult::default();
      result.result_text = String::from_utf8_lossy(&response.stdout).into_owned();
      result.is_error = response.exit_code != 0;
      Ok(result)
  }
  ```
  (Adapt field names to what the actual streaming path produces — read the current code carefully; the `chunk_rx` / `resp_rx` names may differ.)

- [ ] **Step 5: Update call sites in `src/agent_box.rs`**

Find the two call sites (previously noted at ~line 722 and ~line 886 in `src/agent_box.rs`). Each calls `sandbox.exec_claude_streaming(&full_prompt, ClaudeExecOpts { ... }, |event| ...)`.

After Task 2's rename, it's `AgentExecOpts`. Now also:
- Change method name from `exec_claude_streaming` to `exec_agent_streaming`.
- Add `&self.config.llm` as the first positional arg.
- No change to the event callback — it still receives `AgentStreamEvent::ToolUse(...)` (Claude path) or never fires (Codex passthrough path).

Example:
```rust
let mut agent_result = sandbox
    .exec_agent_streaming(
        &self.config.llm,
        &full_prompt,
        AgentExecOpts {
            dangerously_skip_permissions: true,
            extra_args,
            timeout_secs: self.config.timeout_secs,
            ..Default::default()
        },
        |event| match event {
            crate::observe::claude::AgentStreamEvent::ToolUse(ref tc) => {
                // existing logging
            }
        },
    )
    .await?;
```

- [ ] **Step 6: Run `cargo check` until the workspace compiles**

```bash
cargo check --workspace --all-targets --all-features 2>&1 | head -50
```

Fix errors iteratively. Common ones:
- `AgentExecResult` needs `#[derive(Default)]` — add it in `src/observe/claude.rs`.
- Match arms on `LlmProvider` that previously wildcarded may now need the `Codex` variant.
- `Mock` sandbox variant may still hardcode `"claude-code"` in `exec_with_stdin` — thread the provider through.

- [ ] **Step 7: Run the full validation sweep**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Expected: all green. The observer branch for Codex is reachable but not yet exercised by any test; the Claude path still runs through the existing parser unchanged.

- [ ] **Step 8: Commit Task 3**

```bash
git add -u
git commit -m "$(cat <<'EOF'
sandbox,agent_box: rename exec_claude* to exec_agent*, thread provider

Renames Sandbox::exec_claude / exec_claude_streaming to exec_agent /
exec_agent_streaming and adds a &LlmProvider parameter. The hardcoded
"claude-code" literal in the internal exec path is replaced with
provider.binary_name(). Arg construction (previously inlined with
claude-specific flags) is delegated to provider.build_exec_args().

The claude compat probe (verify_claude_code_compat) and the
stream-json observer (parse_jsonl_line / parse_stream_json) are both
gated on provider.binary_name() == "claude-code". Non-Claude
providers get a passthrough observer that populates
AgentExecResult.result_text from stdout and forwards stdout lines to
the tracing subscriber. PR 3 replaces this branch with a typed
observer_kind() selector alongside a new observe::codex parser.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: Gate Claude-only flags in `agent_box.rs`

**Files:**
- Modify: `src/agent_box.rs` (lines ~702-718)

`agent_box.rs` currently appends `--settings` and `--mcp-config` flags unconditionally. These are claude-code CLI flags — Codex rejects them. Gate them on `provider.supports_claude_settings()`.

- [ ] **Step 1: Read the current flag-emission block**

Use LSP on `src/agent_box.rs` to find the block that builds `extra_args`. Around line 705:

```rust
let mut extra_args = self.config.llm.cli_args();
extra_args.extend([
    "--settings".to_string(),
    r#"{"skipWebFetchPreflight":true}"#.to_string(),
]);

// If MCP servers were provisioned, explicitly point claude-code to the config
let has_mcp = self
    .skills
    .iter()
    .any(|s| matches!(s.kind, SkillKind::Mcp { .. }));
if has_mcp {
    extra_args.extend(["--mcp-config".to_string(), MCP_CONFIG_PATH.to_string()]);
}
```

- [ ] **Step 2: Gate both flag additions**

Wrap both the `--settings` extension and the `--mcp-config` extension in a single guard:

```rust
let mut extra_args = self.config.llm.cli_args();
if self.config.llm.supports_claude_settings() {
    extra_args.extend([
        "--settings".to_string(),
        r#"{"skipWebFetchPreflight":true}"#.to_string(),
    ]);

    // If MCP servers were provisioned, explicitly point claude-code to the config
    let has_mcp = self
        .skills
        .iter()
        .any(|s| matches!(s.kind, SkillKind::Mcp { .. }));
    if has_mcp {
        extra_args.extend(["--mcp-config".to_string(), MCP_CONFIG_PATH.to_string()]);
    }
}
```

Codex's MCP server discovery is deferred to PR 4 (the codex MCP config.toml patch). For PR 2, a `provider: codex` spec with an MCP skill skips the flag emission — the void-mcp HTTP server still gets provisioned in the guest (that's in `provision_skills`, unrelated), but codex doesn't know about it. Add a comment explaining this:

```rust
// Claude-only: the --settings flag (for skipWebFetchPreflight) and the
// --mcp-config flag are Claude CLI conventions. Codex reads its MCP servers
// from ~/.codex/config.toml instead (added in PR 4 of the Codex flavor
// effort). The void-mcp HTTP server is still provisioned via
// provision_skills regardless of provider.
if self.config.llm.supports_claude_settings() {
    ...
}
```

- [ ] **Step 3: Also check `matches!(s.kind, SkillKind::Mcp { .. })`**

The `matches!` macro is flagged by the `rust-style` skill. Replace with a full match:

```rust
    let has_mcp = self.skills.iter().any(|s| match &s.kind {
        SkillKind::Mcp { .. } => true,
        SkillKind::Cli { .. }
        | SkillKind::Agent { .. }
        | SkillKind::Oci { .. }
        | SkillKind::Inline { .. } => false,
    });
```

Confirm the `SkillKind` variants match the current declaration in `src/skill.rs` — if they differ, update the match arms accordingly. Per the rust-style skill, no wildcard arms.

- [ ] **Step 4: Validate**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Expected: all green. No test change required yet; the gating is covered by end-to-end validation in Task 7.

- [ ] **Step 5: Commit Task 4**

```bash
git add src/agent_box.rs
git commit -m "$(cat <<'EOF'
agent_box: gate claude-only exec flags on supports_claude_settings

The --settings and --mcp-config flags are Claude CLI conventions;
Codex rejects them. Wraps both flag additions in a
provider.supports_claude_settings() guard. Codex MCP discovery is
deferred to PR 4 (which writes ~/.codex/config.toml pointing at the
existing void-mcp HTTP server).

Also replaces the matches!(s.kind, SkillKind::Mcp { .. }) shorthand
with a full match per the rust-style skill.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 5: Verify YAML `provider: codex` round-trip

**Files:**
- Modify: `src/spec.rs` — add a test
- Modify: `src/runtime.rs` — add a `"codex"` → `LlmProvider::Codex` branch IF the conversion lives there (verify during implementation)

The spec says `LlmSpec.provider: String` is a string field (at `src/spec.rs:87`). Somewhere the runtime converts that string to `LlmProvider`. PR 2 needs to confirm "codex" deserializes correctly.

- [ ] **Step 1: Locate the string → enum conversion**

Use LSP `findReferences` on `LlmSpec.provider` to find every read site. At least one of them converts the string to `LlmProvider`. Likely locations: `src/runtime.rs`, `src/pipeline.rs`, or an `impl TryFrom<LlmSpec> for LlmProvider`.

If the conversion uses a match on the string:
```rust
match spec.provider.as_str() {
    "claude" => LlmProvider::Claude,
    "claude-personal" => LlmProvider::ClaudePersonal,
    "ollama" => LlmProvider::ollama(...),
    "lm-studio" | "lmstudio" => LlmProvider::lm_studio(...),
    "custom" => LlmProvider::custom(...),
    other => return Err(...),
}
```

Add a `"codex" => LlmProvider::Codex` arm before the fallback.

If the conversion uses serde (via `#[serde(rename_all = "...")]` on the enum), the variant name drives the YAML key. In that case, verify `LlmProvider::Codex` serializes to `"codex"` by checking the serde attributes on the enum declaration.

- [ ] **Step 2: Add a unit test**

In `src/spec.rs::tests` (or wherever other provider-parsing tests live), add:

```rust
    #[test]
    fn test_provider_codex_deserializes() {
        let yaml = r#"
api_version: v1
kind: agent
name: codex-test
sandbox:
  memory_mb: 1024
  vcpus: 1
  network: true
llm:
  provider: codex
agent:
  prompt: "hello"
  timeout_secs: 60
"#;
        let spec: Spec = serde_yaml::from_str(yaml).expect("spec should parse");
        assert_eq!(spec.llm.provider, "codex");
    }
```

If there's an existing resolver test that converts `LlmSpec` to `LlmProvider`, add a parallel assertion that `"codex"` resolves to `LlmProvider::Codex`:

```rust
    #[test]
    fn test_resolve_codex_provider() {
        let llm_spec = LlmSpec { provider: "codex".into(), /* fill in other required fields with defaults */ };
        let provider = resolve_llm_provider(&llm_spec).expect("codex should resolve");
        assert!(matches!(provider, LlmProvider::Codex));
    }
```

(Match on `LlmProvider::Codex` is acceptable here because we're asserting a specific variant — this is a test, not production matching. The rust-style skill's "no wildcards" rule applies to production code.)

- [ ] **Step 3: Run the tests**

```bash
cargo test --workspace --all-features -- codex 2>&1 | tail -20
```

Expected: all pass. If the resolver rejects `"codex"`, add the match arm from Step 1.

- [ ] **Step 4: Commit Task 5**

```bash
git add -u
git commit -m "$(cat <<'EOF'
spec,runtime: accept provider: codex in YAML and resolve to LlmProvider::Codex

Confirms the string → LlmProvider conversion recognizes "codex" and
returns LlmProvider::Codex. Adds unit tests for YAML deserialization
and provider resolution.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 6: Create `examples/specs/codex_smoke.yaml`

**Files:**
- Create: `examples/specs/codex_smoke.yaml`

- [ ] **Step 1: Write the spec**

Create `examples/specs/codex_smoke.yaml` with this exact content:

```yaml
api_version: v1
kind: agent
name: codex_smoke

# End-to-end smoke test: verify `provider: codex` wires through the
# agent exec path and produces output. Requires OPENAI_API_KEY.
#
# Build the initramfs first:
#   CODEX_VERSION=0.118.0 scripts/build_codex_rootfs.sh
#
# Then run:
#   OPENAI_API_KEY=sk-... \
#   VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
#   VOID_BOX_INITRAMFS=$PWD/target/void-box-rootfs.cpio.gz \
#   cargo run --bin voidbox -- run --file examples/specs/codex_smoke.yaml

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

- [ ] **Step 2: Verify it parses**

```bash
cargo run --bin voidbox -- run --file examples/specs/codex_smoke.yaml 2>&1 | head -5
```

Expected: the spec parses successfully. The actual VM run will fail unless `VOID_BOX_KERNEL`, `VOID_BOX_INITRAMFS`, and `OPENAI_API_KEY` are set — the point of this step is to catch YAML deserialization errors before runtime. If the spec fails to parse, fix it; if it fails with "kernel not set" or similar runtime errors, that's the expected state and the test passes.

- [ ] **Step 3: Commit Task 6**

```bash
git add examples/specs/codex_smoke.yaml
git commit -m "$(cat <<'EOF'
examples: add codex_smoke.yaml (kind: agent with provider: codex)

End-to-end smoke spec: provider: codex inside a kind: agent run.
Requires OPENAI_API_KEY and the codex flavor initramfs from
scripts/build_codex_rootfs.sh.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 7: Update `docs/agents/codex.md`

**Files:**
- Modify: `docs/agents/codex.md` (the PR 1 file, now that `kind: agent` is supported)

- [ ] **Step 1: Update the "When to use" section**

Replace the current bullet list:

```markdown
## When to use

- Validating workflows that exec `codex` from a `kind: workflow` step.
- Future `kind: agent` runs with `provider: codex` (added in PR 2 of
  the Codex flavor effort — see
  `docs/superpowers/specs/2026-04-07-codex-flavor-design.md`).
```

With:

```markdown
## When to use

- `kind: workflow` specs that exec `codex` as a workflow step.
- `kind: agent` specs with `llm.provider: codex`. Requires
  `OPENAI_API_KEY` in the host environment. Streaming output is
  passthrough in PR 2; structured tool-call and token accounting is
  added by PR 3 (see
  `docs/superpowers/specs/2026-04-07-codex-flavor-design.md`).
```

- [ ] **Step 2: Add a `kind: agent` usage example**

In the "Usage" section, after the existing `codex_workflow_smoke.yaml` block, add:

````markdown
For `kind: agent` usage:

```bash
OPENAI_API_KEY=sk-... \
VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
VOID_BOX_INITRAMFS=$PWD/target/void-box-rootfs.cpio.gz \
cargo run --bin voidbox -- run --file examples/specs/codex_smoke.yaml
```
````

- [ ] **Step 3: Update the "Validation" section**

Extend it to cover both smoke specs:

```markdown
## Validation

Two smoke specs exercise different entry points:

- `examples/specs/codex_workflow_smoke.yaml` — `kind: workflow` step
  running `codex --version`. Self-contained, no API key needed.
  Verifies the bundled binary is present and allowlisted.
- `examples/specs/codex_smoke.yaml` — `kind: agent` with
  `provider: codex`. Requires `OPENAI_API_KEY`. Verifies the full
  exec path through `LlmProvider::Codex`.
```

- [ ] **Step 4: Commit Task 7**

```bash
git add docs/agents/codex.md
git commit -m "$(cat <<'EOF'
docs: update codex.md for kind: agent support landing in PR 2

Updates When to use, Usage, and Validation sections to reflect that
provider: codex is now usable in kind: agent specs (passthrough
output; PR 3 adds the structured observer).

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Final validation

- [ ] **Step 1: Workspace fmt + clippy + tests**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Expected: all green. The new `llm.rs` tests from Task 1 pass, and the existing suite is unaffected by the rename because the new types are drop-in replacements.

- [ ] **Step 2: `e2e_agent_mcp` regression check**

This is the most important regression gate — it exercises the full Claude exec path that Tasks 2-4 modified. If this passes, the rename + provider-parameterization did not break claude.

```bash
scripts/build_test_image.sh
export VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r)
export VOID_BOX_INITRAMFS=/tmp/void-box-test-rootfs.cpio.gz
ANTHROPIC_API_KEY=... cargo test --test e2e_agent_mcp -- --ignored --test-threads=1
```

Expected: passes. If it fails, the most likely causes are:
- Something in the exec path lost the `--settings` or `--mcp-config` flag for Claude (Task 4's gate is backwards).
- `AgentExecResult::default()` produces a value that the Claude code path interprets as "no stream output" (see `src/sandbox/mod.rs` around line 361-392 — the "no_stream_output" heuristic).

Debug both by reading the actual error message and stepping through the provider dispatch.

- [ ] **Step 3: Codex end-to-end smoke**

```bash
CODEX_VERSION=0.118.0 scripts/build_codex_rootfs.sh
export VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r)
export VOID_BOX_INITRAMFS=$PWD/target/void-box-rootfs.cpio.gz
OPENAI_API_KEY=sk-... cargo run --bin voidbox -- run --file examples/specs/codex_smoke.yaml
```

Expected: exit 0, output file at `/workspace/output.json` (or whatever `agent.output_file` defaults to) contains a response starting with "Hello from void-box!".

If the run fails with "command not found: codex", PR 1's allowlist entry didn't land — check `src/backend/mod.rs:296`.

If the run fails with a codex CLI argument error, the `build_exec_args` implementation is wrong — capture the actual codex stderr and adjust.

If the run times out, the SLIRP network may not be reaching `api.openai.com` — check with `codex --version` (which is offline) via the Task-1 workflow smoke first.

- [ ] **Step 4: Open PR 2**

Push the branch and open a PR titled something like `Codex flavor PR 2: LlmProvider::Codex + AgentExec* rename`. Reference the spec and the PR 1 that landed before it:

```
Implements PR 2 of the Codex flavor design:
docs/superpowers/specs/2026-04-07-codex-flavor-design.md

Depends on PR 1 (bundled binary + allowlist).

Scope:
- New LlmProvider::Codex variant with binary_name(),
  supports_claude_settings(), build_exec_args() methods.
- Flat rename ClaudeExecOpts/Result/StreamEvent → AgentExec*,
  StageResult.claude_result → agent_result.
- Rename Sandbox::exec_claude* → exec_agent*, thread &LlmProvider.
- Parameterize hardcoded "claude-code" literal in sandbox/local.rs.
- Gate --settings / --mcp-config flags on supports_claude_settings().
- Gate verify_claude_code_compat on binary_name() == "claude-code".
- Passthrough observer for non-Claude providers (structured parser in PR 3).
- New examples/specs/codex_smoke.yaml.

Out of scope (deferred to PR 3):
- observe::codex structured stream parser.
- Typed ObserverKind selector.

Out of scope (deferred to PR 4):
- ~/.codex/config.toml MCP server discovery.
```

---

## Open implementation notes

- **`AgentExecResult::default()` safety**: the passthrough path relies on `AgentExecResult` implementing `Default`. Verify during Task 3 Step 6 — if the struct doesn't already derive it, add `#[derive(Default)]` in `src/observe/claude.rs`. The existing fields (session_id, model, result_text, tool_calls, input_tokens, output_tokens, total_cost_usd, is_error) are all `Default`-able.
- **`no_stream_output` heuristic regression risk**: `exec_agent` in `src/sandbox/mod.rs` has a heuristic (around lines 363-392) that returns `Err(Error::Guest(...))` if the parsed result has empty session_id / model / result_text / tool_calls / tokens. For the passthrough path, `result_text` is populated from stdout, so the heuristic will NOT trip for Codex — but verify this during Task 3 Step 6 by reading the heuristic carefully.
- **Mock sandbox**: `src/sandbox/mod.rs` has a `Mock` branch that calls `mock.exec_with_stdin("claude-code", ...)`. Update this to use `provider.binary_name()`. The Mock sandbox is used by unit tests that don't have a real VM; without this update, the Codex variant's tests will fail because the mock only knows claude-code.
