# Auto image resolution — design

Date: 2026-04-12
Status: Draft

## Goal

`voidbox run --file spec.yaml` works with zero env vars on both Linux
and macOS. The CLI auto-resolves the kernel and the right initramfs
based on the spec's `llm.provider`, downloading and caching both on
first run.

## Non-goals

- OCI image distribution (container registry). Initramfs stays a plain
  cpio.gz file on GitHub Releases.
- Auto-updating cached images when a new void-box version is released.
  The user runs `voidbox image clean` or upgrades void-box, which
  changes the version bucket and triggers a fresh download.
- Building images from source inside the CLI. The CLI downloads
  pre-built artifacts; `scripts/build_*_rootfs.sh` stays for dev/CI.

## Artifacts

CI produces 10 artifacts per release (8 images + 2 kernels), each
with a companion `.sha256` checksum file.

### Images (4 flavors × 2 architectures = 8)

| Flavor | x86_64 | aarch64 |
|---|---|---|
| base | `void-box-base-x86_64.cpio.gz` | `void-box-base-aarch64.cpio.gz` |
| claude | `void-box-claude-x86_64.cpio.gz` | `void-box-claude-aarch64.cpio.gz` |
| codex | `void-box-codex-x86_64.cpio.gz` | `void-box-codex-aarch64.cpio.gz` |
| agents | `void-box-agents-x86_64.cpio.gz` | `void-box-agents-aarch64.cpio.gz` |

- **base** — `build_guest_image.sh` output. For `kind: workflow` specs
  with no `llm` section.
- **claude** — `build_claude_rootfs.sh` output. For `provider: claude`,
  `claude-personal`, `ollama`, `lm-studio`, `custom`.
- **codex** — `build_codex_rootfs.sh` output. For `provider: codex`.
- **agents** — new `build_agents_rootfs.sh` output. Both
  `/usr/local/bin/claude-code` and `/usr/local/bin/codex` present. For
  users who want a single image that works with any provider.

### Kernels (2)

| Arch | Artifact | Notes |
|---|---|---|
| x86_64 | `vmlinuz-x86_64` | Compressed, from host or CI |
| aarch64 | `vmlinux-aarch64` | Uncompressed, required by VZ on macOS |

### Checksum files

Each artifact has a `<artifact>.sha256` companion containing its
SHA-256 hex digest. The CLI downloads both, verifies after download,
and rejects corrupted files.

## Hosting

GitHub Releases on `the-void-ia/void-box`. Assets attached to version
tags (e.g. `v0.1.2`). Same hosting pattern already used by the codex
binary download in `build_codex_rootfs.sh`.

**Download URL pattern:**

```
https://github.com/the-void-ia/void-box/releases/download/v{version}/{artifact}
```

## Cache layout

```
~/.void-box/images/
  v0.1.2/
    void-box-claude-x86_64.cpio.gz
    void-box-claude-x86_64.cpio.gz.sha256
    void-box-codex-x86_64.cpio.gz
    void-box-codex-x86_64.cpio.gz.sha256
    vmlinuz-x86_64
    vmlinuz-x86_64.sha256
  v0.1.3/
    void-box-claude-aarch64.cpio.gz
    vmlinux-aarch64
    ...
```

Version-bucketed subdirectories under `~/.void-box/images/`. The
version is the void-box CLI's own `CARGO_PKG_VERSION` (known at
compile time). Each void-box upgrade gets a fresh bucket; old
versions are cleaned with `voidbox image clean`.

## Resolution flow

### Kernel resolution

```
1. --kernel flag or VOID_BOX_KERNEL env var → use it
2. Linux only: /boot/vmlinuz-$(uname -r) exists → use it (no download)
3. Cache hit: ~/.void-box/images/<version>/vmlinuz-<arch> → use it
4. Download from GitHub release → verify checksum → cache → use it
```

Step 2 means most Linux users never download a kernel. macOS users
go to step 3/4.

### Initramfs resolution

```
1. --initramfs flag or VOID_BOX_INITRAMFS env var → use it
2. Cache hit: ~/.void-box/images/<version>/void-box-<flavor>-<arch>.cpio.gz → use it
3. Download from GitHub release → verify checksum → cache → use it
```

### Flavor selection from the spec

```
spec.llm.provider → flavor:
  "codex"                                      → "codex"
  "claude" / "claude-personal" / "ollama"
    / "lm-studio" / "custom"                   → "claude"
  absent (kind: workflow, no llm section)       → "base"
```

Implemented as `LlmProvider::image_flavor(&self) -> &'static str`,
placed next to `binary_name()` and `observer_kind()`.

### Arch detection

```rust
let arch = match std::env::consts::ARCH {
    "x86_64" => "x86_64",
    "aarch64" => "aarch64",
    other => return Err(...)
};
```

Kernel artifact name: `vmlinuz-x86_64` on x86_64, `vmlinux-aarch64`
on aarch64 (VZ requires uncompressed kernel).

## Download behavior

### Progress bar

Show download progress on stderr for each file. Use `indicatif` crate
if already a dependency, otherwise a simple percentage-based progress
line (`\r[download] 45% (42/93 MB)`) that overwrites itself. Both
kernel and initramfs downloads display progress independently.

### Parallel download

When both kernel and initramfs need downloading (cold start), download
them concurrently via `tokio::join!`. Halves the cold-start wait on
first run.

### Retry logic

3 retries with exponential backoff (1s, 2s, 4s) on network errors and
5xx server responses. 4xx responses (404, 403) fail immediately — the
release artifact doesn't exist, retrying won't help. The error message
names the URL and suggests `VOID_BOX_INITRAMFS` as the manual
fallback.

### Checksum verification

After downloading an artifact, download its `.sha256` companion, parse
the hex digest, and compare against `sha2::Sha256::digest()` of the
downloaded file. On mismatch: delete the corrupt file, emit an error,
and do NOT cache. The retry loop treats checksum failure as a
retryable error (re-downloads from scratch).

## Error handling

| Failure | Behavior |
|---|---|
| No `llm.provider` on a `kind: agent` spec | Error: "kind: agent requires an llm.provider field" |
| Download fails after retries | Error naming the URL + "Set VOID_BOX_INITRAMFS manually or run `voidbox image pull <flavor>`" |
| Release doesn't exist (404) | Error: "No release v{version} found. Check that void-box {version} has published release artifacts." |
| Checksum mismatch after retries | Error: "Checksum verification failed for {artifact}. The file may be corrupted in transit." |
| Cache dir not writable | Error: "Cannot write to ~/.void-box/images/. Set VOID_BOX_INITRAMFS to skip caching." |
| Linux host kernel missing | Fall through to download (step 3/4) instead of erroring |

## `voidbox image` subcommand

```
voidbox image pull <flavor>     Download a specific image
voidbox image pull kernel       Download the kernel
voidbox image pull all          Download all 4 images + kernel
voidbox image list              Show cached images
voidbox image clean             Remove old versions (keep current)
voidbox image clean --all       Remove everything
```

### `pull`

Downloads the artifact for the current void-box version and host arch.
If already cached and checksum matches, prints "already cached" and
exits. Flavors: `base`, `claude`, `codex`, `agents`, `kernel`, `all`.

### `list`

```
$ voidbox image list
Version   Flavor   Arch      Size    Path
v0.1.2    claude   x86_64    96 MB   ~/.void-box/images/v0.1.2/void-box-claude-x86_64.cpio.gz
v0.1.2    codex    x86_64    91 MB   ~/.void-box/images/v0.1.2/void-box-codex-x86_64.cpio.gz
v0.1.2    kernel   x86_64    18 MB   ~/.void-box/images/v0.1.2/vmlinuz-x86_64
v0.1.1    claude   x86_64    94 MB   ~/.void-box/images/v0.1.1/void-box-claude-x86_64.cpio.gz
```

Scans `~/.void-box/images/*/` for known artifact names. Prints version,
flavor, arch, file size, and full path. Current version is marked or
listed first.

### `clean`

- `voidbox image clean` — removes all version directories except the
  current CLI version. Prints what it removed and how much space was
  freed.
- `voidbox image clean --all` — removes the entire
  `~/.void-box/images/` directory. Prints total space freed.

Both are safe — worst case the CLI re-downloads on next run.

## Files

### New files

| File | Responsibility |
|---|---|
| `src/image.rs` | `resolve_kernel()`, `resolve_initramfs()`, `download_and_cache()`, checksum verification, arch detection, retry logic |
| `src/bin/voidbox/image.rs` | `voidbox image` subcommand (pull, list, clean) |
| `scripts/build_agents_rootfs.sh` | Combined image builder (sources `agent_rootfs_common.sh`, sets both `CLAUDE_CODE_BIN` + `CODEX_BIN`) |
| `.github/workflows/release-images.yml` | CI workflow for building + uploading artifacts on release |

### Modified files

| File | Change |
|---|---|
| `src/bin/voidbox/main.rs` | Add `Image` subcommand variant |
| `src/runtime.rs` | Call `image::resolve_kernel()` / `image::resolve_initramfs()` when env vars are unset |
| `src/llm.rs` | Add `image_flavor(&self) -> &'static str` method on `LlmProvider` |
| `Cargo.toml` | Add `sha2` crate dependency (for checksum verification); `indicatif` if used for progress bar |

## Testing

- Unit tests for `image::resolve_kernel()` / `resolve_initramfs()` with
  a mock cache dir (no network). Test the fallback chain: env var →
  host kernel → cache → download.
- Unit test for `LlmProvider::image_flavor()` mapping — every variant
  maps to the expected flavor string.
- Unit test for checksum verification (valid + invalid digest).
- Unit test for retry logic (mock HTTP server returning 500 then 200).
- Integration test: `voidbox image list` on an empty cache returns
  empty table.
- Integration test: `voidbox image pull codex` with a mock HTTP server
  creates the expected cache file.
- The CI workflow itself is validated by the release pipeline producing
  the expected artifact set.

## Existing behavior preserved

- `VOID_BOX_KERNEL` and `VOID_BOX_INITRAMFS` env vars still work as
  overrides. They are checked first and bypass the entire resolution
  flow.
- `--kernel` and `--initramfs` CLI flags still work (they set the
  same values as the env vars).
- `scripts/build_guest_image.sh`, `build_claude_rootfs.sh`,
  `build_codex_rootfs.sh`, `build_test_image.sh` all continue to
  work for local development. The CLI is a consumer of pre-built
  artifacts; these scripts are the producers.
- `download_kernel.sh` continues to work as a standalone script.
  The Rust resolver reimplements the same logic for the auto-download
  path but the shell script is not removed.
