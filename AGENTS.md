# AGENTS.md — VoidBox

VoidBox is a lightweight micro-VM runtime for sandboxed execution. It boots a
guest agent from initramfs, optionally performs an OCI root switch, and exposes
a host↔guest API over vsock. This file covers architecture, testing, validation,
and debugging guidance for agents working on the project.

## Code intelligence

**Always prefer LSP operations** (`goToDefinition`, `findReferences`, `hover`,
`documentSymbol`, `workspaceSymbol`, etc.) over Grep/Glob for code navigation.
LSP provides compiler-aware results that understand types, scopes, and cross-file
relationships. Fall back to Grep/Glob only for pattern-based searches LSP doesn't
cover (comments, config files, non-Rust files).

## Platform parity

**Contributions must work on both Linux (KVM) and macOS (VZ).** Validate on both
where applicable. Key platform differences:

| Concern | Linux (KVM) | macOS (VZ) |
|---------|-------------|------------|
| Kernel | `vmlinuz` (compressed OK) | `vmlinux` (uncompressed); `download_kernel.sh` uses `extract-vmlinux` |
| Network detection | `virtio_mmio.device=512@0xd0000000:10` in cmdline | `voidbox.network=1` in cmdline (VZ uses PCI) |
| Kernel modules | Host or pinned; `build_test_image.sh` | `guest_macos.sh`; `VOID_BOX_KMOD_VERSION` must match `download_kernel.sh` |

## Architecture overview

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

### Host directory mounts

VoidBox supports mounting host directories into the guest VM with explicit
read-only or read-write access. RW mounts write directly to the host directory,
so changes persist across VM restarts.

**Data flow:**

```
YAML spec (MountSpec)
  → runtime (MountConfig)
    → kernel cmdline: voidbox.mount0=mount0:/data:rw
      → guest-agent: mount 9p/virtiofs tag at /data
```

**Spec-level:** `MountSpec` in `src/spec.rs` defines the user-facing YAML
fields: `host` (host directory path), `guest` (guest mount point), `mode`
(`"ro"` default, `"rw"`).

**Backend-level:** `MountConfig` in `src/backend/mod.rs` carries `host_path`,
`guest_path`, and `read_only` (boolean). `VmConfig.mounts` (`src/vmm/config.rs`)
holds the list of mount configs for the VM.

**Linux/KVM transport:** Each mount becomes a virtio-9p device
(`src/backend/kvm.rs`). The kernel cmdline receives
`voidbox.mount<N>=<tag>:<guest_path>:<ro|rw>` parameters
(`src/vmm/config.rs:244-248`).

**macOS/VZ transport:** Each mount becomes a virtiofs share
(`src/backend/vz/backend.rs`). The same kernel cmdline convention is used
(`src/backend/vz/config.rs`).

**Guest-agent:** Parses `voidbox.mount*` params from `/proc/cmdline` and mounts
each tag at the specified guest path with the declared mode.

### Structured logging & observability

VoidBox has a structured logging abstraction that bridges application-level log
calls to the `tracing` ecosystem. All workflow progress messages should go
through this pipeline rather than raw `eprintln!` or bare `tracing::info!`.

**Pipeline:**

```
observer.logger().info(msg, attrs)
  → StructuredLogger::log()
    → tracing::info!()  (when output_to_tracing is true, which is the default)
      → tracing_subscriber (EnvFilter) → stderr
```

**How to use it:** In workflow/scheduler code, obtain the logger from the
`Observer` and call `.info()`, `.warn()`, `.error()`, etc. with structured
key-value attributes:

```rust
self.observer.logger().info(
    &format!("[workflow:{}] step {}/{}: \"{}\" running...", name, i, total, step),
    &[("step", step_name)],
);
```

Do not use `eprintln!` for progress messages — it bypasses the structured
pipeline and won't carry trace context or attributes.

**Log levels:** `LogConfig::default()` sets the minimum level to `INFO`.

| Level | Use for |
|-------|---------|
| `.info()` | User-visible progress (step start/finish, workflow lifecycle) |
| `.debug()` | Internal detail (durations, stdout/stderr capture) |
| `.warn()` | Partial failures, degraded conditions |
| `.error()` | Step/workflow failures |

**CLI subscriber:** `src/bin/voidbox.rs` initializes `tracing_subscriber` with
`EnvFilter` defaulting to `"info"`. Override at runtime with:

```bash
RUST_LOG=debug cargo run --bin voidbox -- run --file spec.yaml
```

**Convention:** Workflow progress messages use the `[workflow:<name>]` prefix
pattern, e.g. `[workflow:my-flow] step 1/3: "build" running...`.

**Key files:**

| File | Role |
|------|------|
| `src/observe/logs.rs` | `StructuredLogger`, `LogConfig`, `LogEntry`, `LogLevel` |
| `src/observe/mod.rs` | `Observer` (owns logger), `SpanGuard` (RAII span + logging) |
| `src/workflow/scheduler.rs` | Step progress logging via `observer.logger()` |
| `src/bin/voidbox.rs` | CLI `tracing_subscriber` + `EnvFilter` setup |

### Key source files

- `guest-agent/src/main.rs` — `setup_oci_rootfs` (~line 754), `mount_oci_block_lowerdir` (~line 1133)
- `src/devices/virtio_blk.rs` — RO feature flag (line 21), write rejection (line 394)
- `src/runtime.rs` — `build_oci_rootfs_disk` (~line 815, host-side ext4 image creation),
  `resolve_oci_rootfs_plan` (~line 776), `apply_oci_rootfs` (~line 745)
- `src/backend/vz/backend.rs` — VZ virtiofs mount setup

## Snapshot / restore

VoidBox supports two snapshot types: **Base** (full dump) and **Diff** (dirty pages
only). All snapshot features are **explicit opt-in** — no snapshot code runs
unless the user sets a snapshot field.

### Design principle

Every snapshot-related field defaults to `None`. No auto-detection, no env var
fallback, no implicit behavior. The system behaves identically to before if no
snapshot field is set.

### Opt-in layers

Snapshots can be enabled at any layer of the stack:

| Layer | Field | Where |
|-------|-------|-------|
| Builder API | `SandboxBuilder::snapshot(path)` | `src/sandbox/mod.rs` |
| VoidBox API | `VoidBox::snapshot(path)` | `src/agent_box.rs` |
| YAML spec | `sandbox.snapshot` | `src/spec.rs` |
| Per-box override | `sandbox.snapshot` in `BoxSandboxOverride` | `src/spec.rs` |
| Runtime | `resolve_snapshot()`, `resolve_box_snapshot()` | `src/runtime.rs` |
| Daemon API | `CreateRunRequest.snapshot` | `src/daemon.rs` |

### Snapshot resolution

`resolve_snapshot()` in `src/runtime.rs` resolves a snapshot string:

1. Try as hash prefix → `~/.void-box/snapshots/<prefix>/` (if `state.bin` exists)
2. Try as literal directory path (if `state.bin` exists)
3. Neither found → print warning, return `None` (cold boot)

Per-box overrides (`resolve_box_snapshot()`) take priority over top-level spec.

### Wire protocol

`SnapshotReady = 17` in `void-box-protocol/src/lib.rs`. Guest-agent handles
message type 17 as a no-op ack (sends `SnapshotReady` back). Host-side
`ControlChannel::wait_for_snapshot_ready()` in `src/backend/control_channel.rs`
connects and round-trips the message.

### Key source files (snapshot)

| File | Contents |
|------|----------|
| `src/vmm/snapshot.rs` | Types, memory dump/restore, diff support, cache/LRU |
| `src/vmm/mod.rs` | `snapshot()`, `from_snapshot()`, `snapshot_diff()` |
| `src/vmm/cpu.rs` | vCPU state capture/restore |
| `src/sandbox/mod.rs` | `SandboxBuilder::snapshot()` |
| `src/agent_box.rs` | `VoidBox::snapshot()` |
| `src/spec.rs` | `SandboxSpec.snapshot`, `BoxSandboxOverride.snapshot` |
| `src/runtime.rs` | `resolve_snapshot()`, `resolve_box_snapshot()` |
| `src/daemon.rs` | `CreateRunRequest.snapshot` |
| `src/backend/control_channel.rs` | `wait_for_snapshot_ready()` |
| `void-box-protocol/src/lib.rs` | `MessageType::SnapshotReady = 17` |
| `guest-agent/src/main.rs` | Message type 17 dispatch |
| `src/backend/vz/snapshot.rs` | `VzSnapshotMeta` sidecar (JSON) for VZ snapshots |
| `src/backend/vz/backend.rs` | `pause()`, `resume()`, `create_snapshot()`, restore path in `start()` |

### Security considerations

- **RNG entropy**: Cloned VMs share `/dev/urandom` state. Mitigated by fresh CID
  + session secret per restore and hardware RDRAND.
- **ASLR**: Clones share page table layout. Mitigated by short-lived tasks,
  SLIRP NAT isolation, and command allowlists.
- **Session secret**: Each restored VM gets a unique secret via kernel cmdline;
  the snapshot's stored secret is for state continuity, not auth reuse.

## Testing

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

# Snapshot integration suites (required for any snapshot/restore/vCPU changes):
export VOID_BOX_INITRAMFS=/tmp/void-box-test-rootfs.cpio.gz
cargo test --test snapshot_integration -- --ignored --nocapture --test-threads=1

# Claudio-based deterministic E2E suites:
scripts/build_test_image.sh
export VOID_BOX_INITRAMFS=/tmp/void-box-test-rootfs.cpio.gz
cargo test --test e2e_telemetry -- --ignored --test-threads=1
cargo test --test e2e_skill_pipeline -- --ignored --test-threads=1
cargo test --test e2e_mount -- --ignored --test-threads=1
```

### Test initramfs and BusyBox

`scripts/build_test_image.sh` builds a minimal initramfs with `guest-agent`
(as `/init`) and `claudio` (as `/usr/local/bin/claude-code`). The script
**auto-detects** a statically linked BusyBox on the host and includes it if
found. If BusyBox is not auto-detected, set the `BUSYBOX` env var explicitly:

```bash
BUSYBOX=/path/to/busybox-static scripts/build_test_image.sh
```

BusyBox provides `/bin/sh` and common utilities (`echo`, `cat`, `mkdir`, `rm`,
`mv`, `chmod`, `stat`, `dd`, `ls`, `wc`, `test`, `grep`, `sed`, `find`, etc.)
inside the guest. **Without BusyBox**, any test that runs `sh -c "..."` will
fail with `No such file or directory`. The `e2e_mount` tests require BusyBox
because they use shell commands to exercise the mounted filesystem.

### 9p kernel module loading order

The guest-agent loads kernel modules at boot time from `/lib/modules/` inside
the initramfs. For 9p shared mounts, the dependency chain must be loaded in
order:

```
netfs.ko → 9pnet.ko → 9p.ko → 9pnet_virtio.ko
```

This order is enforced in `guest-agent/src/main.rs` (`load_kernel_modules()`
~line 540). The corresponding modules must also be included in the initramfs
by `scripts/build_test_image.sh`. The `overlay.ko` module is also included
for OCI rootfs overlay support. If any module in the chain is missing from
the initramfs, 9p mounts will hang or fail silently.

## Validation contract

Use this contract when shipping VM/OCI/OpenClaw changes. Commands are ordered and
all required gates are explicit.

### Preconditions

- Use repo-local temp dir to avoid `/tmp` pressure:
  ```bash
  export TMPDIR=$PWD/target/tmp
  mkdir -p "$TMPDIR"
  ```
- Set kernel once (Linux: host kernel; macOS: `scripts/download_kernel.sh` then `VOID_BOX_KERNEL=target/vmlinux-arm64`):
  ```bash
  export VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r)   # Linux
  export VOID_BOX_KERNEL=target/vmlinux-arm64        # macOS
  ```
- VM suites require usable KVM/vsock (not only device presence).

### Standard validation sequence

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

### aarch64 cross-check (required when touching `src/vmm/arch/aarch64/` or arch-neutral VMM code)

CI runs on native aarch64 (`ubuntu-24.04-arm`). To catch issues locally from an
x86_64 host without waiting for CI:

```bash
# One-time setup (Fedora):
#   sudo dnf install -y gcc-aarch64-linux-gnu sysroot-aarch64-fc43-glibc
#   rustup target add aarch64-unknown-linux-gnu

CFLAGS_aarch64_unknown_linux_gnu="--sysroot=/usr/aarch64-redhat-linux/sys-root/fc43" \
  RUSTFLAGS="-D warnings" \
  cargo check --target aarch64-unknown-linux-gnu -p void-box --lib --tests
```

Common aarch64 pitfalls:
- `kvm-bindings` constants use `kvm_device_type_` prefix (e.g.
  `kvm_device_type_KVM_DEV_TYPE_ARM_VGIC_V3`, not `KVM_DEV_TYPE_ARM_VGIC_V3`).
- `kvm-ioctls` `get_preferred_target()` takes `&mut kvm_vcpu_init` out-param.
- `libc::SYS_epoll_wait` and `libc::SYS_poll` don't exist on aarch64 — use
  `SYS_epoll_pwait` and `SYS_ppoll`.
- Unused variables/imports that are only used on x86_64 become errors with
  `-D warnings` on aarch64.

### VM suites (required for VM/OCI/OpenClaw changes)

Linux (KVM):

```bash
# Generic VM suites
export VOID_BOX_INITRAMFS=/tmp/void-box-rootfs.cpio.gz
cargo test --test conformance -- --ignored --test-threads=1
cargo test --test oci_integration -- --ignored --test-threads=1

# Snapshot integration (required for snapshot/restore/vCPU/barrier changes)
scripts/build_test_image.sh
export VOID_BOX_INITRAMFS=/tmp/void-box-test-rootfs.cpio.gz
cargo test --test snapshot_integration -- --ignored --nocapture --test-threads=1

# Linux-only deterministic e2e suites (claudio + busybox)
cargo test --test e2e_telemetry -- --ignored --test-threads=1
cargo test --test e2e_skill_pipeline -- --ignored --test-threads=1
cargo test --test e2e_mount -- --ignored --test-threads=1
```

macOS (VZ):

```bash
scripts/download_kernel.sh
export VOID_BOX_KERNEL=target/vmlinux-arm64
export VOID_BOX_INITRAMFS=/tmp/void-box-rootfs.cpio.gz
cargo test --test conformance -- --ignored --test-threads=1
cargo test --test oci_integration -- --ignored --test-threads=1
cargo test --test e2e_mount -- --ignored --test-threads=1

# VZ snapshot round-trip (requires test initramfs):
scripts/build_test_image.sh
export VOID_BOX_INITRAMFS=/tmp/void-box-test-rootfs.cpio.gz
cargo test --release --test snapshot_vz_integration -- --ignored --test-threads=1
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

## Run examples

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

Deterministic e2e suites (test image with `claudio` + BusyBox):

```bash
scripts/build_test_image.sh
export VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r)
export VOID_BOX_INITRAMFS=/tmp/void-box-test-rootfs.cpio.gz
cargo test --test e2e_telemetry -- --ignored --test-threads=1
cargo test --test e2e_skill_pipeline -- --ignored --test-threads=1
cargo test --test e2e_mount -- --ignored --test-threads=1
```

Do **not** use `target/void-box-rootfs.cpio.gz` for these deterministic e2e suites.
That production image is for real Claude/OpenClaw runtime paths.
`e2e_telemetry` and `e2e_skill_pipeline` are Linux/KVM only.
`e2e_mount` runs on both Linux (KVM, virtio-9p) and macOS (VZ, virtiofs).

## Known issues

### Snapshot restore: XCR0 and LAPIC timer

**Root causes of guest crash after snapshot restore** (2-day debugging effort):

1. **Missing XCR0 restore → FPU #GP → kernel panic → reboot loop.**
   After restore, XCR0 defaults to x87-only (bit 0). The guest kernel's
   `XRSTORS` instruction references SSE/AVX features in the XSAVE compact
   format, but those features aren't enabled in XCR0 → General Protection
   fault → "Bad FPU state detected at restore_fpregs_from_fpstate" → panic →
   `emergency_restart` writes 0xFE to port 0x64 (keyboard controller reset).
   **Fix:** Capture `kvm_xcrs` via `KVM_GET_XCRS` and restore via
   `KVM_SET_XCRS` *before* `KVM_SET_XSAVE`. Added `xcrs` field to
   `VcpuState` (backward-compatible via `#[serde(default)]`).

2. **LAPIC timer masked in NO_HZ idle → scheduler never runs.**
   When the guest is idle at snapshot time, the kernel disables the LAPIC
   timer (LVTT=0x10000, masked, vector=0). After restore the timer stays
   masked — no tick ever fires, scheduler stalls, vsock never processes
   packets. **Fix:** Detect masked+vector=0 state and bootstrap a periodic
   LAPIC timer (mode=periodic, vector=0xEC, TMICT=0x200000, TDCR=divide-by-1).

3. **CID mismatch → guest silently drops vsock packets.**
   Guest kernel caches CID during virtio-vsock probe at cold boot. If
   restore assigns a new random CID, the guest drops all incoming packets
   (dst_cid doesn't match cached value). **Fix:** `snapshot_internal()`
   stores `self.cid` in the snapshot; `from_snapshot()` reuses that CID.

**Debugging tip:** Guest reboots (port 0x64 write of 0xFE) indicate kernel
panic, not a hang. Add `panic=-1 loglevel=7 earlyprintk=serial` to kernel
cmdline and read serial output via `vm.read_serial_output()` to capture the
actual panic message.

**Key files:** `src/vmm/cpu.rs` (capture/restore order), `src/vmm/mod.rs`
(CID preservation), `src/vmm/snapshot.rs` (VcpuState with xcrs field).

### EPERM during OCI layer unpack

Container images (especially multi-layer ones like `alpine/openclaw`) can trigger
`Operation not permitted (os error 1)` during tar extraction. The EPERM-resilient
unpack code in `voidbox-oci/src/unpack.rs` handles files, symlinks, dirs, and
hardlinks — but any bare `?` on tar entry operations (e.g. `entry.path()?`) will
bypass the EPERM handling and surface as an `OciError::Io`. When debugging OCI
unpack failures, check for bare `?` on `entry.path()`, `entry.link_name()`, or
`entry.unpack()` calls that don't go through the EPERM catch-and-skip pattern.

## Conformance expectations

- `conformance`: command execution, lifecycle, streaming, filesystem primitives.
- `oci_integration`: image pull/extract, rootfs mounting, readonly invariants.
- `snapshot_integration`: cold snapshot capture, vCPU state persistence
  (including XCR0/xsave), restore pipeline, exec on restored VM, VM stop
  after restore. **Must pass** for any changes to snapshot, restore, vCPU,
  vsock, or `stop()` code paths. Uses userspace virtio-vsock backend.
- `snapshot_vz_integration` (macOS only): VZ snapshot round-trip using Apple's
  `saveMachineStateToURL:`/`restoreMachineStateFromURL:` APIs. Cold boot →
  exec → snapshot → stop → restore → exec. **Must pass** for VZ backend
  snapshot/restore changes.
- `e2e_telemetry`: telemetry flow from guest to host pipeline.
- `e2e_skill_pipeline`: multi-stage skill execution in VM mode.
- `e2e_mount`: host↔guest directory sharing via virtio-9p (Linux) / virtiofs
  (macOS) — RW/RO, write, read, mkdir, rename, delete, chmod, large files,
  pre-existing content, empty dirs.

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
