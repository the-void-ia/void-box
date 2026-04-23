# Codex flavor — `scripts/build_codex_rootfs.sh`

Production OpenAI-Codex-capable rootfs/initramfs.

## What it bundles

- The `codex` CLI binary (Rust musl-static, no shared libraries needed).
- SSL CA certificates for HTTPS API calls.
- `/etc/passwd` + `/etc/group` for the sandbox user (uid 1000).

## When to use

- `kind: workflow` specs that exec `codex` as a workflow step.
- `kind: agent` specs with `llm.provider: codex`. Requires
  `OPENAI_API_KEY` in the host environment or a valid `~/.codex/auth.json`
  (see Auth section below).

## Discovery

The script locates the codex binary in priority order:

1. `CODEX_BIN` env var pointing at a Linux ELF binary — local dev only.
2. `codex` on PATH (Linux host only — the macOS Mach-O binary cannot
   run inside the Linux guest) — local dev only.
3. `CODEX_VERSION` set → explicit override; requires a matching
   `CODEX_SHA256` env var (SHA-256 of the downloaded tarball) so
   developers supply the hash they are asking the build to trust.
4. Default: download the version and URL pinned in
   `scripts/agents/manifest.toml` and verify against the pinned SHA-256
   (computed against the tarball as it lands on disk — the build then
   extracts the binary from the verified tarball).

## Pinning and verification (R-B5c.1)

The default build path is **hash-pinned**. `scripts/agents/manifest.toml`
holds one table per (agent, platform, arch) tuple with a `version`, `url`
template, and `sha256`. `build_codex_rootfs.sh` fetches the URL, computes
SHA-256 of the downloaded tarball, and **fails the build** if the digest
does not match — the expected and observed digests are both printed on
failure.

Parser choice: shell + awk (`scripts/lib/agent_manifest.sh`). No Python or
Rust sidecar is required, which keeps the builder working anywhere bash +
awk + curl + sha256sum/shasum are available.

Hash updates are reviewed PRs, not silent upstream events. The weekly
workflow `.github/workflows/bump-agents.yml` (Mondays 09:00 UTC) checks for
new upstream versions and opens one PR per (agent, platform, arch) with the
computed SHA-256 in the PR body.

Published release metadata: each void-box release uploads a
`RELEASE_DIGESTS.json` alongside the images. It carries the pinned
`version`/`url`/`sha256` for every agent+platform+arch tuple that shipped
with that release.

### Overriding the pinned version for local dev

```bash
# Rejected: override without a matching hash is refused by build scripts.
CODEX_VERSION=0.99.0 scripts/build_codex_rootfs.sh

# Accepted: hash supplied alongside the version.
CODEX_VERSION=0.99.0 \
CODEX_SHA256=abcdef... \
  scripts/build_codex_rootfs.sh
```

### Non-production paths

`CODEX_BIN=/path/to/codex` and local-PATH discovery remain supported for
local dev because they are developer ergonomics. Images built through those
paths **skip the hash check** (the script cannot know which upstream blob the
local file is supposed to be). The build emits a `WARN` line at resolution
time — treat any image whose build log contains
`WARN: using CODEX_BIN … without SHA-256 verification` as **non-production**.

## Usage

```bash
CODEX_VERSION=0.118.0 scripts/build_codex_rootfs.sh

OPENAI_API_KEY=sk-... \
VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
VOID_BOX_INITRAMFS=$PWD/target/void-box-codex.cpio.gz \
cargo run --bin voidbox -- run --file examples/specs/codex_workflow_smoke.yaml
```

For `kind: agent` usage:

```bash
OPENAI_API_KEY=sk-... \
VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
VOID_BOX_INITRAMFS=$PWD/target/void-box-codex.cpio.gz \
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

## Streaming output

Codex's `exec --json` event stream is parsed by
`src/observe/codex.rs::parse_codex_line` and populates the same
`AgentExecResult` struct that the Claude parser produces. The summary
line emitted by `agent_box.rs` reports real token counts, tool calls,
and the final agent message — for example:

```
[vm:my-spec] Agent finished | tokens=22578in/251out | tools=1 | cost=$0.0000 | error=false
```

Tool call tracking covers `file_change` (file edits) and
`command_execution` (shell commands codex runs in the guest).
Unknown item types are recorded as generic tool calls so future codex
event types don't break the parser.
