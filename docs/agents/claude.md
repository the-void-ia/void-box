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
