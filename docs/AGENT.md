# AGENT Operations Guide

This document is the operational reference for running VoidBox with OCI images and validating conformance.

## Runtime model

- Host runtime launches a VM backend (`KvmBackend` on Linux, `VzBackend` on macOS).
- Guest boots `guest-agent` from initramfs.
- If `sandbox.image` is set, guest performs OCI root switch with `pivot_root`.
- On Linux/KVM, OCI base rootfs is attached as cached `virtio-blk` disk.
- OCI skills are mounted as read-only tool roots.

## OCI root switch internals

### OCI root switch sequence (`setup_oci_rootfs`)

**Linux/KVM (virtio-blk):**

1. Host builds a cached ext4 disk image from the extracted OCI rootfs
   (`build_oci_rootfs_disk` in `src/runtime.rs`, `#[cfg(target_os = "linux")]`).
2. Disk is attached as a **read-only virtio-blk** device (`/dev/vda`).
3. Guest-agent mounts `/dev/vda` as ext4 with `MS_RDONLY` at `/mnt/oci-lower`.

**macOS/VZ (virtiofs):**

1. Host shares the extracted OCI rootfs directory into the guest via virtiofs
   (`read_only: true`), resolved in `apply_oci_rootfs` (`src/runtime.rs`).
2. Guest-agent finds the rootfs at `/mnt/oci-rootfs` (already mounted by
   `mount_shared_dirs`).
3. The virtiofs mount is used directly as the overlay lowerdir.

**Common (both platforms):**

4. A tmpfs is mounted for overlay `upper` + `work` directories.
5. Overlayfs is mounted at `/mnt/newroot` (lower=OCI rootfs RO, upper=tmpfs RW).
6. Essential mounts (`/proc`, `/sys`, `/dev`) are move-mounted into the new root.
7. Mount propagation is set to `MS_REC | MS_PRIVATE` on `/`.
8. `pivot_root(".", "mnt/oldroot")` switches the root.
9. Old root is detached with `umount2(MNT_DETACH)` and removed.
10. `/tmp`, `/workspace`, `/home/sandbox` are recreated; DNS config is restored.

If `pivot_root` returns `EINVAL` (initramfs can't be pivoted), a switch-root
fallback uses `MS_MOVE` + `chroot(".")` and records `OCI_OK_SWITCH_ROOT`.
Status is tracked via `OCI_SETUP_STATUS` (`AtomicU8`), with distinct codes for
each failure point (e.g. `OCI_FAIL_BLOCK_MOUNT`, `OCI_FAIL_OVERLAY_MOUNT`,
`OCI_FAIL_PIVOT_ROOT_*`).

### Read-only strategy (defense-in-depth)

| Layer | Mechanism | Platform | File |
|-------|-----------|----------|------|
| Host file | `File::open()` (read-only) | Linux | `src/devices/virtio_blk.rs` |
| Virtio feature | `VIRTIO_BLK_F_RO` (bit 5); writes rejected with `VIRTIO_BLK_S_UNSUPP` | Linux | `src/devices/virtio_blk.rs` |
| Virtiofs mount | `VZVirtioFileSystemDeviceConfiguration` with `read_only: true` | macOS | `src/backend/vz/backend.rs` |
| Guest mount | `mount(..., MS_RDONLY)` | Both | `guest-agent/src/main.rs` |

On Linux the three-layer block-device strategy applies. On macOS the virtiofs
share is host-enforced RO and the guest adds `MS_RDONLY`. Both platforms use the
overlayfs upper layer (tmpfs) to absorb writes, so the guest has a writable root
without modifying the base rootfs.

### Key source files

- `guest-agent/src/main.rs` — `setup_oci_rootfs` (~line 754), `mount_oci_block_lowerdir` (~line 1133)
- `src/devices/virtio_blk.rs` — RO feature flag (line 21), write rejection (line 394)
- `src/runtime.rs` — `build_oci_rootfs_disk` (~line 815, host-side ext4 image creation),
  `resolve_oci_rootfs_plan` (~line 776), `apply_oci_rootfs` (~line 745)
- `src/backend/vz/backend.rs` — VZ virtiofs mount setup

## Test matrix (CI parity)

Static quality:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo clippy --workspace --exclude guest-agent --all-targets --all-features -- -D warnings
```

Core tests:

```bash
cargo test --workspace --all-features
cargo test --doc --workspace --all-features
cargo test --workspace --exclude guest-agent --all-features
cargo test --doc --workspace --exclude guest-agent --all-features
```

Ignored/VM suites (requires kernel/initramfs and usable KVM/vsock access):

```bash
export VOID_BOX_KERNEL=/path/to/vmlinuz

# Generic guest image suites:
export VOID_BOX_INITRAMFS=/tmp/void-box-rootfs.cpio.gz
cargo test --test conformance -- --ignored --test-threads=1
cargo test --test oci_integration -- --ignored --test-threads=1

# Claudio-based deterministic E2E suites:
scripts/build_test_image.sh
export VOID_BOX_INITRAMFS=/tmp/void-box-test-rootfs.cpio.gz
cargo test --test e2e_telemetry -- --ignored --test-threads=1
cargo test --test e2e_skill_pipeline -- --ignored --test-threads=1
```

## Validation Run Contract

Use this contract when shipping VM/OCI/OpenClaw changes. Commands are ordered and
all required gates are explicit.

### Preconditions

- Use repo-local temp dir to avoid `/tmp` pressure:
  ```bash
  export TMPDIR=$PWD/target/tmp
  mkdir -p "$TMPDIR"
  ```
- Set kernel once:
  ```bash
  export VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r)
  ```
- VM suites require usable KVM/vsock (not only device presence).

### Standard validation sequence

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

### VM suites (required for VM/OCI/OpenClaw changes)

Linux (KVM):

```bash
# Generic VM suites
export VOID_BOX_INITRAMFS=/tmp/void-box-rootfs.cpio.gz
cargo test --test conformance -- --ignored --test-threads=1
cargo test --test oci_integration -- --ignored --test-threads=1

# Linux-only deterministic e2e suites (claudio)
scripts/build_test_image.sh
export VOID_BOX_INITRAMFS=/tmp/void-box-test-rootfs.cpio.gz
cargo test --test e2e_telemetry -- --ignored --test-threads=1
cargo test --test e2e_skill_pipeline -- --ignored --test-threads=1
```

macOS (VZ):

```bash
export VOID_BOX_INITRAMFS=/tmp/void-box-rootfs.cpio.gz
cargo test --test conformance -- --ignored --test-threads=1
cargo test --test oci_integration -- --ignored --test-threads=1
```

`e2e_telemetry` and `e2e_skill_pipeline` are Linux-only (`cfg(target_os = "linux")`)
and are not expected to run on macOS.

### OpenClaw production validation

OpenClaw gateway must run on the production image:

```bash
TMPDIR=$PWD/target/tmp scripts/build_claude_rootfs.sh
export VOID_BOX_INITRAMFS=$PWD/target/void-box-rootfs.cpio.gz
```

Then validate gateway workflow:

```bash
export TELEGRAM_BOT_TOKEN=...
export TELEGRAM_CHAT_ID=...
export ANTHROPIC_API_KEY=...
cargo run --bin voidbox -- run --file examples/openclaw/openclaw_telegram.yaml
```

Or validate the Ollama-backed gateway workflow:

```bash
export TELEGRAM_BOT_TOKEN=...
export TELEGRAM_CHAT_ID=...
export OLLAMA_BASE_URL=http://10.0.2.2:11434
export OLLAMA_API_KEY=ollama-local
export OLLAMA_MODEL=qwen2.5-coder:7b
ollama serve
ollama pull qwen2.5-coder:7b
cargo run --bin voidbox -- run --file examples/openclaw/openclaw_telegram_ollama.yaml
```

Do not use `/tmp/void-box-test-rootfs.cpio.gz` for OpenClaw gateway validation.

### Exit gates

- `fmt`, `clippy`, and workspace tests must pass.
- VM suites must either pass or skip with explicit environment reason.
- OpenClaw validation must use production initramfs and reach startup/interaction.

## Run Examples (Safe Snippets)

Use placeholders for secrets (`...`) and keep real tokens out of docs/commits.

Baseline smoke spec:

```bash
cargo run --bin voidbox -- run --file examples/specs/smoke_test.yaml
```

OCI node sanity (base image mount/exec):

```bash
export VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r)
export VOID_BOX_INITRAMFS=/tmp/void-box-rootfs.cpio.gz
cargo run --bin voidbox -- run --file examples/openclaw/node_version.yaml
```

OpenClaw Telegram gateway (production path):

```bash
TMPDIR=$PWD/target/tmp scripts/build_claude_rootfs.sh
export VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r)
export VOID_BOX_INITRAMFS=$PWD/target/void-box-rootfs.cpio.gz
export TELEGRAM_BOT_TOKEN=...
export TELEGRAM_CHAT_ID=...
export ANTHROPIC_API_KEY=...
cargo run --bin voidbox -- run --file examples/openclaw/openclaw_telegram.yaml
```

OpenClaw Telegram gateway with host Ollama (production path):

```bash
TMPDIR=$PWD/target/tmp scripts/build_claude_rootfs.sh
export VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r)
export VOID_BOX_INITRAMFS=$PWD/target/void-box-rootfs.cpio.gz
export TELEGRAM_BOT_TOKEN=...
export TELEGRAM_CHAT_ID=...
export OLLAMA_BASE_URL=http://10.0.2.2:11434
export OLLAMA_API_KEY=ollama-local
export OLLAMA_MODEL=qwen2.5-coder:7b
ollama serve
ollama pull qwen2.5-coder:7b
cargo run --bin voidbox -- run --file examples/openclaw/openclaw_telegram_ollama.yaml
```

Do **not** use `/tmp/void-box-test-rootfs.cpio.gz` for OpenClaw gateway runs.
That test image is only for deterministic `claudio` suites.

For the full catalog, see `examples/README.md` and `examples/openclaw/README.md`.

Linux-only deterministic e2e suites (test image with `claudio`):

```bash
scripts/build_test_image.sh
export VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r)
export VOID_BOX_INITRAMFS=/tmp/void-box-test-rootfs.cpio.gz
cargo test --test e2e_telemetry -- --ignored --test-threads=1
cargo test --test e2e_skill_pipeline -- --ignored --test-threads=1
```

Do **not** use `target/void-box-rootfs.cpio.gz` for these deterministic e2e suites.
That production image is for real Claude/OpenClaw runtime paths.
These suites are Linux/KVM only.

## Known issues / debugging

### EPERM during OCI layer unpack

Container images (especially multi-layer ones like `alpine/openclaw`) can trigger
`Operation not permitted (os error 1)` during tar extraction. The EPERM-resilient
unpack code in `voidbox-oci/src/unpack.rs` handles files, symlinks, dirs, and
hardlinks — but any bare `?` on tar entry operations (e.g. `entry.path()?`) will
bypass the EPERM handling and surface as an `OciError::Io`. When debugging OCI
unpack failures, check for bare `?` on `entry.path()`, `entry.link_name()`, or
`entry.unpack()` calls that don't go through the EPERM catch-and-skip pattern.

## OCI-focused conformance expectations

- `conformance`: command execution, lifecycle, streaming, filesystem primitives.
- `oci_integration`: image pull/extract, rootfs mounting, readonly invariants.
- `e2e_telemetry`: telemetry flow from guest to host pipeline.
- `e2e_skill_pipeline`: multi-stage skill execution in VM mode.

If the environment lacks usable KVM/vsock or outbound network, VM suites should print skip reasons (for example `failed to create KVM VM: Permission denied`) rather than panic/fail.

## Guest image build scripts

`scripts/build_guest_image.sh`:

- Base/initramfs for normal VM runs and OCI-rootfs workflows.
- Does not require bundling production Claude runtime.
- Preferred for general development and most integration tests.

`scripts/build_claude_rootfs.sh`:

- Production Claude-capable rootfs/initramfs.
- Includes native `claude-code`, CA certs, and sandbox user.
- Use when validating production-like Claude execution paths.
- Required for OpenClaw Telegram gateway example runs.

Recommended default:

- Use `build_guest_image.sh` for broad test cycles.
- Use `build_claude_rootfs.sh` for production gateway/runtime validation.
