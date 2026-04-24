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

1. `CLAUDE_BIN` env var pointing at a Linux ELF binary — local dev only.
2. `~/.local/bin/claude` or `claude` on PATH (Linux host only) — local dev only.
3. `CLAUDE_CODE_VERSION` set → explicit override; requires a matching
   `CLAUDE_CODE_SHA256` env var so developers supply the hash they are
   asking the build to trust.
4. Default: download the version and URL pinned in
   `scripts/agents/manifest.toml` and verify against the pinned SHA-256.

## Pinning and verification (R-B5c.1)

The default build path is **hash-pinned**. `scripts/agents/manifest.toml`
holds one table per (agent, platform, arch) tuple with a `version`, `url`
template, and `sha256`. `build_claude_rootfs.sh` fetches the URL, computes
SHA-256, and **fails the build** if the digest does not match — the expected
and observed digests are both printed on failure.

Parser choice: shell + awk (`scripts/lib/agent_manifest.sh`). No Python or
Rust sidecar is required, which keeps the builder working anywhere bash +
awk + curl + sha256sum/shasum are available.

Hash updates are reviewed PRs, not silent upstream events. The weekly
workflow `.github/workflows/bump-agents.yml` (Mondays 09:00 UTC) checks for
new upstream versions and opens one PR per agent containing the bumped
arches, with per-arch SHA-256s listed in the PR body.

Batching note (pragmatic deviation from R-B5c.1): the spec text says
"one PR per (agent, platform, arch) bump — not batched" alongside a
`bump-agents/<agent>-<version>` branch name that cannot disambiguate two
arches sharing a version. We honour the branch-name pattern and combine
both arches of the same agent version into a single PR, because the
arches ship in lockstep upstream and splitting them produces two PRs that
must be merged atomically for the release pipeline to stay coherent. The
PR body surfaces each arch's URL and SHA-256 on its own row so review is
still granular. If one arch legitimately lags (partial-merge of a prior
PR, security patch, or upstream arch skew), the weekly run bumps only the
lagging arch and leaves the matching one alone — the job no longer
asserts arch parity.

Published release metadata: each void-box release uploads a
`RELEASE_DIGESTS.json` alongside the images. It carries the pinned
`version`/`url`/`sha256` for every agent+platform+arch tuple that shipped
with that release.

### Overriding the pinned version for local dev

```bash
# Rejected: override without a matching hash is refused by build scripts.
CLAUDE_CODE_VERSION=2.1.99 scripts/build_claude_rootfs.sh

# Accepted: hash supplied alongside the version.
CLAUDE_CODE_VERSION=2.1.99 \
CLAUDE_CODE_SHA256=abcdef... \
  scripts/build_claude_rootfs.sh
```

### Non-production paths

`CLAUDE_BIN=/path/to/claude` and local-PATH discovery remain supported for
local dev because they are developer ergonomics. Images built through those
paths **skip the hash check** (the script cannot know which upstream blob the
local file is supposed to be). The build emits a `WARN` line at resolution
time — treat any image whose build log contains
`WARN: using CLAUDE_BIN … without SHA-256 verification` as **non-production**.

## Usage

```bash
scripts/build_claude_rootfs.sh

ANTHROPIC_API_KEY=sk-ant-... \
VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
VOID_BOX_INITRAMFS=$PWD/target/void-box-claude.cpio.gz \
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
