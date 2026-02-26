# AGENT Operations Guide

This document is the operational reference for running VoidBox with OCI images and validating conformance.

## Runtime model

- Host runtime launches a VM backend (`KvmBackend` on Linux, `VzBackend` on macOS).
- Guest boots `guest-agent` from initramfs.
- If `sandbox.image` is set, guest performs OCI root switch with `pivot_root`.
- On Linux/KVM, OCI base rootfs is attached as cached `virtio-blk` disk.
- OCI skills are mounted as read-only tool roots.

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
