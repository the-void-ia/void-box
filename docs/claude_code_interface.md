# Claude Code interface for void-box

This document defines the guest-side interface for running Claude Code (or a mock) inside a void-box sandbox, so workflows can invoke it via `ctx.exec("claude-code", &[...])` or `ctx.exec_piped(...)`.

## Primary model

- Claude (or a mock) runs **inside the VM** as a CLI, invoked by the guest-agent.
- The host treats it as a normal program: `ctx.exec("claude-code", &["plan", "/workspace"]).await`.

## CLI name and location

- **Binary/script path in guest**: `/usr/local/bin/claude-code` (or `claude-code` on PATH).
- **Name**: `claude-code` so it is distinct from any system `claude` and clearly denotes "Claude Code" style usage.

## Subcommands and arguments

Start with two subcommands that support a canonical workflow (plan → apply):

| Subcommand | Args | Stdin | Description |
|------------|------|-------|-------------|
| `plan` | `[workspace_dir]` (default `.`) | optional context | Produce a plan (e.g. JSON or text) for changes. Output on stdout. |
| `apply` | `[workspace_dir]` (default `.`) | plan from previous step | Consume plan from stdin and apply edits. Output summary on stdout. |

Example workflow usage from host:

```rust
ctx.exec("claude-code", &["plan", "/workspace"]).await?;
ctx.exec_piped("claude-code", &["apply", "/workspace"]).await?;
```

## Environment variables

- `ANTHROPIC_API_KEY`: required for real API calls (not set for mock).
- `WORKSPACE` or workspace passed as argument.
- Optional: `CLAUDE_MODEL`, `CLAUDE_MAX_TOKENS` for real implementation.

## Mock vs real

- **Mock (first implementation)**:
  - A shell or script that:
    - `plan`: echoes a fixed JSON plan or "mock plan" text.
    - `apply`: reads stdin and echoes "Applied N edits" or transforms input (e.g. uppercase) for tests.
  - No network, no API key. Used in integration tests and when building the guest image without Anthropic access.
- **Real (later)**:
  - Wrapper that calls Anthropic API (e.g. Python + `anthropic` SDK, or a small Rust binary).
  - Requires network and `ANTHROPIC_API_KEY` in the guest environment.

## Input/output format (mock)

- **Plan output**: plain text or one-line JSON, e.g. `{"actions":["edit file X"]}`.
- **Apply input**: same format as plan output (piped from previous step).
- **Apply output**: "Applied 1 change(s)" or similar; exit code 0 on success.

## Reproducible build

- The guest rootfs build (see `scripts/run_kvm_tests.sh` and `scripts/build_guest_image.sh`) will install:
  - `guest-agent` at `/sbin/guest-agent`.
  - Optional: `/usr/local/bin/claude-code` as the mock script (or real binary when available).
- Mock script is checked in under `scripts/guest/claude-code-mock.sh` and copied into rootfs during image build.
