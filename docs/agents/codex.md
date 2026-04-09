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
