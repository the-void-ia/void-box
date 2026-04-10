# Codex flavor — `scripts/build_codex_rootfs.sh`

Production OpenAI-Codex-capable rootfs/initramfs.

## What it bundles

- The `codex` CLI binary (Rust musl-static, no shared libraries needed).
- SSL CA certificates for HTTPS API calls.
- `/etc/passwd` + `/etc/group` for the sandbox user (uid 1000).

## When to use

- `kind: workflow` specs that exec `codex` as a workflow step.
- `kind: agent` specs with `llm.provider: codex`. Requires
  `OPENAI_API_KEY` in the host environment. Streaming output is
  passthrough in PR 2; structured tool-call and token accounting is
  added by PR 3 (see
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

For `kind: agent` usage:

```bash
OPENAI_API_KEY=sk-... \
VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
VOID_BOX_INITRAMFS=$PWD/target/void-box-rootfs.cpio.gz \
cargo run --bin voidbox -- run --file examples/specs/codex_smoke.yaml
```

## Auth

The codex CLI authenticates against the OpenAI Responses API used by
`codex exec`. Two paths are supported, and they coexist:

1. **`codex login` on the host (recommended).** When `provider: codex` is
   used in a spec, void-box discovers `~/.codex/auth.json` on the host,
   stages it into a temp directory with 0600 permissions, and mounts the
   directory at `/home/sandbox/.codex` inside the guest (RW so codex can
   refresh tokens). This works for both `auth_mode: "chatgpt"` (the
   ChatGPT OAuth flow) and `auth_mode: "api_key"`. The temp directory
   auto-cleans on run completion.
2. **`OPENAI_API_KEY` env var.** Forwarded into the guest exec env if set
   on the host. As of codex 0.118, the Responses API endpoint typically
   rejects `sk-proj-...` project-scoped keys with "Missing bearer or
   basic authentication in header". User-level `sk-...` keys may work
   for some endpoints. Treat this as a fallback; prefer `codex login`.

Both can be set together — codex's own auth resolver picks one based on
what's available in `auth.json`.

## Validation

Two smoke specs exercise different entry points:

- `examples/specs/codex_workflow_smoke.yaml` — `kind: workflow` step
  running `codex --version`. Self-contained, no API key or login
  needed. Verifies the bundled binary is present and allowlisted.
- `examples/specs/codex_smoke.yaml` — `kind: agent` with
  `provider: codex`. Requires either `codex login` on the host (so
  `~/.codex/auth.json` exists for void-box to mount) or a working
  `OPENAI_API_KEY`. Verifies the full exec path through
  `LlmProvider::Codex`.
