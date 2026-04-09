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

## Rust coding conventions

### Imports and constants

Always declare `use` imports and `const` / `static` items at **module scope**
(top of the file, after the module-level doc comment), never inline inside
function bodies.  Inline declarations hide dependencies and make the file harder
to scan.

```rust
// ✓ correct
use std::io::{Read, Seek, SeekFrom};
const OVERHEAD_BYTES: u64 = 208 * 1024 * 1024;

fn my_fn() { /* uses them */ }

// ✗ avoid
fn my_fn() {
    use std::io::{Read, Seek, SeekFrom};
    const OVERHEAD_BYTES: u64 = 208 * 1024 * 1024;
    // ...
}
```

### Data model and policy design

Prefer **explicit single-source models** over overlapping flags that create
ambiguous or invalid states. If a feature has a small closed set of modes, model
it as an enum rather than separate booleans/options whose combinations must be
interpreted later.

Keep **execution policy** separate from persisted or user-resolved config.
Command-scoped behavior (for example, interactive terminal ownership) should be
derived at the CLI/runtime boundary and passed inward as a narrow policy type,
rather than mutating resolved config structs or threading broad command enums
through lower layers.

Prefer **descriptive local names** and named constants over shorthand locals and
inline literals. Variables like `rc`, `n`, `msg`, `ws`, `k`, and `v` are only
acceptable in tiny, obvious scopes; otherwise use names that describe meaning
directly. Repeated or behavior-defining literals should be lifted to documented
module-scope constants.

### VM pre-flight validation

Operations that can fail silently inside the guest (e.g. the kernel dropping
initramfs files when memory is too low) must surface a clear diagnostic on the
**host**, before the VM starts.  The pattern:

1. **Check before launch** — validate the config in `VmmBackend::start()` before
   any irreversible side effect, using helpers on `BackendConfig`
   (e.g. `initramfs_memory_warning()`).
2. **Log at `WARN`** — use `warn!()` so the message appears without aborting the
   run.  Return `Err` only when a successful boot is impossible regardless of
   retry.
3. **Name the downstream symptom** — mention the observable failure mode
   (e.g. *"kernel may silently drop initramfs files (e.g. vsock.ko)"*) so an
   operator can connect the warning to a later guest error without a separate
   debugging session.

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
- If `agent.mode` is `service`, the agent runs indefinitely; output is published while the process continues.
- If `agent.messaging.enabled` is `true`, a host-side sidecar coordinates inter-agent communication over HTTP; `void-mcp` bridges Claude Code to the sidecar via MCP tools.

## Control channel I/O model

The control channel (`src/backend/control_channel.rs`) uses **synchronous I/O**
(`std::io::Read`/`Write`) for all guest communication, wrapped in
`tokio::task::spawn_blocking` so that blocking syscalls never stall the tokio
async runtime.

### Why not fully async (`AsyncRead`/`AsyncWrite`)?

1. **The VZ connector is fundamentally callback-based.**
   `VZVirtioSocketDevice.connectToPort:completionHandler:` dispatches onto a GCD
   serial queue and fires an ObjC completion handler.  There is no fd to poll
   until *after* the callback delivers one.  A bridge channel between GCD and
   tokio is required regardless.

2. **`AsyncFd` on raw vsock fds is fragile.**
   The fd comes from `dup(connection.fileDescriptor())` inside a GCD callback,
   outside tokio's control.  `AsyncFd` requires `O_NONBLOCK`, which changes
   read/write semantics and demands `WouldBlock` handling.  The current blocking
   fds with `SO_RCVTIMEO` for timeouts are simpler and correct.

3. **Cancellation safety.**
   With async reads, `tokio::time::timeout` cancels the future mid-read.  If
   cancellation happens between a header read and a payload read, the stream is
   left in an inconsistent state.  Avoiding this requires a cancel-safe framing
   layer — added complexity for no practical gain.

4. **Concurrency is low.**
   A single VM has one control channel.  Even with concurrent exec calls, the
   blocking thread pool (up to 512 threads, dynamic scaling) handles the workload
   comfortably.  The async advantage (thousands of connections on few threads)
   doesn't apply.

### Pattern

Every public `async fn` on `ControlChannel` follows the same structure:

1. **Serialize** the outgoing message on the caller's async task (cheap, non-blocking).
2. **Clone** the `Arc` fields (`connector`, `boot_wait_done`) and copy `session_secret`.
3. **`spawn_blocking`**: connect → handshake → send → read loop — all synchronous.

The handshake function (`connect_with_handshake_sync`) is a regular `fn` that
uses `std::thread::sleep` for backoff.  It is never called from an async context
directly.

### Key files

| File | Role |
|------|------|
| `src/backend/control_channel.rs` | `ControlChannel`, `GuestStream` trait, `connect_with_handshake_sync` |
| `src/backend/kvm.rs` | AF_VSOCK connector, `GuestStream` impl for `VsockStream` |
| `src/backend/vz/backend.rs` | VZ GCD-based connector (`build_connector`) |
| `src/backend/vz/vsock.rs` | `VzSocketStream` — `GuestStream` impl over dup'd fd |

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

**CLI subscriber:** `src/bin/voidbox/main.rs` initializes `tracing_subscriber` with
`EnvFilter` from the resolved log level (see `src/bin/voidbox/cli_config.rs` for merge order).
Override at runtime with `VOIDBOX_LOG_LEVEL` or `--log-level`, or set `RUST_LOG` when no explicit
CLI/config level is set:

```bash
VOIDBOX_LOG_LEVEL=debug cargo run --bin voidbox -- run --file spec.yaml
# RUST_LOG also applies when VOIDBOX_LOG_LEVEL / config omit log_level
```

**Configuration and routing principles:**

- Keep configuration **opt-in by default** unless a different default is needed
  to prevent a clearly broken UX or unsafe behavior.
- When a runtime limitation or fallback occurs, emit an **explicit warning** with
  actionable remediation rather than silently degrading behavior.
- Prefer making routing and storage locations **user-customizable** (for example,
  via config or CLI overrides) rather than hard-coding one path.
- Interactive PTY/TUI sessions should **own the terminal**. Do not mix runtime
  diagnostics, guest console output, or other non-PTY streams into the same
  terminal once the interactive session begins; route them to tracing/file sinks
  instead.

**Convention:** Workflow progress messages use the `[workflow:<name>]` prefix
pattern, e.g. `[workflow:my-flow] step 1/3: "build" running...`.

**Key files:**

| File | Role |
|------|------|
| `src/observe/logs.rs` | `StructuredLogger`, `LogConfig`, `LogEntry`, `LogLevel` |
| `src/observe/mod.rs` | `Observer` (owns logger), `SpanGuard` (RAII span + logging) |
| `src/workflow/scheduler.rs` | Step progress logging via `observer.logger()` |
| `src/bin/voidbox/main.rs` | CLI `tracing_subscriber` + `EnvFilter` setup |
| `src/bin/voidbox/cli_config.rs` | CLI config merge (`VOIDBOX_*`, YAML, `--log-level`) |

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

## Service mode

VoidBox agents run in one of two modes: **task** (default) or **service**.  Task
mode runs to completion and returns output.  Service mode runs indefinitely —
publishing structured output while the process continues — and is designed for
long-running daemons like API gateways.

### YAML configuration

Agent-level:

```yaml
agent:
  prompt: "Run the gateway"
  mode: service
  output_file: /workspace/output.json
```

Workflow step-level:

```yaml
steps:
  - name: gateway
    mode: service
    run:
      program: sh
      args: [-lc, "exec node /app/server.mjs"]

output_step: gateway
```

### Validation rules

| Field | Task mode | Service mode |
|-------|-----------|--------------|
| `timeout_secs` | Optional | **Not allowed** (rejected at parse time) |
| `output_file` | Optional | **Required** |

Service workflow steps set effective timeout to `0` (infinite).

### Lifecycle differences

| Phase | Task mode | Service mode |
|-------|-----------|--------------|
| Output | Returned on completion | Published via `output_rx` while process runs |
| Daemon status | Terminal on exit | `output_ready=true` on publication, `status=running` until exit |
| Termination | Process exits | Explicit cancel, natural exit, or crash |

The daemon (`spawn_service_run()`) manages three oneshot channels:

- `output_rx` — fires when the output file is ready
- `exit_rx` — fires when the service process terminates
- `stop_tx` — signal to gracefully stop the service

`ServiceExit` maps to `RunStatus`: `Exited { success: true }` → `Succeeded`,
`Exited { success: false }` → `Failed`, `Canceled` → `Cancelled`,
`Crashed(msg)` → `Failed`.

### Key files

| File | Role |
|------|------|
| `src/spec.rs` | `AgentMode`, `StepMode`, `MessagingSpec`, validation rules |
| `src/agent_box.rs` | `ServiceStageHandle`, `ServiceExit`, `run_service()` |
| `src/daemon.rs` | `spawn_service_run()`, `terminalize_service()` |
| `src/runtime.rs` | `run_spec_service()`, timeout override for service steps |

## Messaging and sidecar

VoidBox includes a **sidecar** — a host-side HTTP server that coordinates
inter-agent communication.  Agents exchange **intents** (typed messages with
audience and priority) through the sidecar, enabling multi-agent collaboration
within isolated VM sandboxes.

### Enabling messaging

```yaml
agent:
  prompt: "..."
  messaging:
    enabled: true
    provider_bridge: claude_channels  # optional
```

When `messaging.enabled` is `true`, the daemon starts a sidecar server and
injects `VOID_SIDECAR_URL=http://10.0.2.2:<port>` into the guest environment.

### Architecture

```
Guest VM                              Host
┌──────────────────────┐    HTTP    ┌─────────────┐
│  void-message CLI    │──────────→│  Sidecar     │
│  void-mcp server     │           │  /v1/inbox   │
│  (agent process)     │           │  /v1/intents │
└──────────────────────┘           │  /v1/context │
                                   └─────────────┘
```

The sidecar runs on the host network.  Guest tools reach it via the SLIRP
gateway address (`10.0.2.2`).

### Intent model

Agents communicate through **intents** — structured messages with:

- **kind**: `proposal`, `signal`, or `evaluation`
- **audience**: `broadcast` (all agents) or `leader` (coordinator only)
- **priority**: `high`, `normal`, or `low`
- **payload**: JSON, max 4096 bytes

Constraints: max 3 intents per iteration, default TTL of 2 iterations.
Idempotency keys prevent duplicate submissions.

### Sidecar API endpoints

| Endpoint | Method | Purpose |
|----------|--------|---------|
| `/v1/context` | GET | Execution identity (candidate_id, execution_id, iteration, role, peers) |
| `/v1/inbox?since=<version>` | GET | Read peer messages (incremental polling) |
| `/v1/intents` | POST | Submit intent (with idempotency key) |
| `/v1/health` | GET | Health check |

### void-message CLI

A guest-side CLI for direct sidecar interaction:

```bash
void-message context              # read execution identity
void-message inbox [--since N]    # read peer messages
void-message send --kind signal --audience broadcast --summary "ready"
void-message health               # check sidecar
```

### Key files

| File | Role |
|------|------|
| `src/spec.rs` | `MessagingSpec` (`enabled`, `provider_bridge`) |
| `src/sidecar/mod.rs` | Module exports, `messaging_skill_content()` |
| `src/sidecar/server.rs` | HTTP server, request handlers |
| `src/sidecar/state.rs` | Intent acceptance, dedup, limits |
| `src/sidecar/types.rs` | `SubmittedIntent`, `StampedIntent`, `InboxSnapshot`, `SidecarContext` |
| `src/daemon.rs` | Sidecar startup, env injection |
| `void-message/src/main.rs` | Guest-side CLI |

## MCP integration

The `void-mcp` crate implements an MCP (Model Context Protocol) server that
bridges Claude Code to the sidecar messaging system.  It exposes four
collaboration tools and handles the MCP JSON-RPC wire protocol.

### Tools

| Tool | Purpose | Sidecar endpoint |
|------|---------|-----------------|
| `read_shared_context` | Execution identity (candidate_id, role, peers) | `GET /v1/context` |
| `read_peer_messages` | Observations from sibling agents (incremental `since`) | `GET /v1/inbox` |
| `broadcast_observation` | Share finding with all agents | `POST /v1/intents` (kind=signal, audience=broadcast) |
| `recommend_to_leader` | Send recommendation to coordinator (promote/refine/reject) | `POST /v1/intents` (kind=proposal/evaluation, audience=leader) |

### Transport modes

- **Stdio** (default): Content-Length framed JSON-RPC 2.0 (same as LSP)
- **Streamable HTTP** (`--sse`): Listens on `127.0.0.1:<port>`, handles
  `POST /mcp` — used inside minimal VMs where Claude Code can't spawn child
  processes

### Provisioning flow

When a spec includes an MCP skill, `VoidBox::provision_skills()`:

1. Starts `void-mcp` as a background HTTP server inside the guest (SSE mode,
   port `8222 + index`)
2. Writes `/workspace/.mcp.json`:
   ```json
   {
     "mcpServers": {
       "void-mcp": {
         "type": "http",
         "url": "http://127.0.0.1:8222/mcp"
       }
     }
   }
   ```
3. Passes `--mcp-config /workspace/.mcp.json` to Claude Code

The `void-mcp` binary is in the guest execution allowlist
(`DEFAULT_COMMAND_ALLOWLIST` in `src/backend/mod.rs`).

### Data flow

```
Claude Code → JSON-RPC → void-mcp (:8222) → HTTP → Sidecar (host :N) → Swarm state
```

### Skill configuration

Skills can be declared in YAML specs or via the builder API:

```yaml
agent:
  skills:
    - mcp:void-mcp
```

Or programmatically:

```rust
Skill::mcp("void-mcp")
    .args(vec!["--sse".into()])
    .env("VOID_SIDECAR_URL", url)
```

### Key files

| File | Role |
|------|------|
| `void-mcp/src/main.rs` | MCP server (stdio + HTTP transport) |
| `void-mcp/src/tools.rs` | Tool definitions, sidecar HTTP client |
| `void-mcp/src/jsonrpc.rs` | JSON-RPC 2.0 protocol types |
| `void-mcp/src/http.rs` | HTTP client utilities |
| `src/skill.rs` | `Skill::mcp()` factory, `SkillKind::Mcp` |
| `src/agent_box.rs` | `provision_skills()` — MCP server startup, `.mcp.json` generation |
| `src/backend/mod.rs` | `DEFAULT_COMMAND_ALLOWLIST` (includes `void-mcp`) |

## Testing

Static quality:

```bash
cargo fmt --all -- --check
```

Clippy (Linux):
```bash
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Clippy (macOS — excludes guest-agent, which is Linux-only):
```bash
cargo clippy --workspace --exclude guest-agent --all-targets --all-features -- -D warnings
```

Core tests (Linux):
```bash
cargo test --workspace --all-features
cargo test --doc --workspace --all-features
```

Core tests (macOS — excludes guest-agent):
```bash
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

# Service mode + sidecar + MCP e2e suites:
cargo test --test e2e_service_mode -- --ignored --test-threads=1
cargo test --test e2e_sidecar -- --ignored --test-threads=1
ANTHROPIC_API_KEY=... cargo test --test e2e_agent_mcp -- --ignored --test-threads=1
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

Linux:
```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

macOS (excludes guest-agent, which is Linux-only):
```bash
cargo fmt --all -- --check
cargo clippy --workspace --exclude guest-agent --all-targets --all-features -- -D warnings
cargo test --workspace --exclude guest-agent --all-features
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

# Service mode + sidecar + MCP e2e suites (Linux-only):
cargo test --test e2e_service_mode -- --ignored --test-threads=1
cargo test --test e2e_sidecar -- --ignored --test-threads=1
ANTHROPIC_API_KEY=... cargo test --test e2e_agent_mcp -- --ignored --test-threads=1
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

`e2e_telemetry`, `e2e_skill_pipeline`, `e2e_service_mode`, `e2e_sidecar`, and
`e2e_agent_mcp` are Linux-only (`cfg(target_os = "linux")`) and are not expected
to run on macOS.

### Interactive PTY / shell validation

When touching interactive PTY behavior, terminal handling, guest console
routing, or shell-specific logging, do a manual smoke check on the relevant
host platforms in addition to automated tests.

Linux (KVM):
```bash
export VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r)
export VOID_BOX_INITRAMFS=/tmp/void-box-rootfs.cpio.gz
target/release/voidbox shell --program sh --mount "$PWD:/workspace:rw"
```

macOS (VZ):
```bash
export VOID_BOX_KERNEL=target/vmlinux-arm64
export VOID_BOX_INITRAMFS=/tmp/void-box-rootfs.cpio.gz
target/release/voidbox shell --program sh --mount "$PWD:/workspace:rw"
```

For both platforms, verify:
- the PTY stays interactive and does not exit immediately
- terminal resize is reflected in the guest
- runtime logs and guest console output do not corrupt the interactive terminal
- guest console routing honors the configured host sink

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

### Vsock control channel timeout with large initramfs or missing `ip`

**Symptom:** `control_channel: deadline reached (connect or handshake)` — the
host connects via AF_VSOCK but gets `ECONNRESET` on every attempt.

**Root causes (two independent issues):**

1. **Missing `ip` binary → `Command::new("ip").output()` hangs PID 1.**
   The guest-agent's `setup_network()` calls `run_cmd("ip", ...)` which
   internally does `fork+execvp`. When `ip` is not in PATH (e.g. no busybox
   symlink), `execvp` fails — but in the minimal initramfs PID 1 environment,
   the Rust `Command::output()` call can hang indefinitely instead of returning
   `Err(ENOENT)`. The guest-agent never reaches `create_vsock_listener()`.
   **Fix:** Ensure `ip` (and `which`) are included as busybox symlinks in the
   initramfs (`scripts/build_test_image.sh`).

2. **Large production initramfs (100+ MB) exceeds boot timeout.**
   The control channel allows 4 s boot wait + 30 s connect deadline (34 s total).
   A 100 MB+ initramfs (with real `claude-code`, glibc, git, etc.) takes longer
   to decompress, load modules, and complete network setup — especially with only
   256 MB of guest RAM. **Fix:** Use at least **3 GB** of guest memory for
   production initramfs (`memory_mb(3072)`) so the kernel can decompress and
   boot within the timeout window.

**Debugging tip:** Boot a `MicroVm` directly with `loglevel=7` in the kernel
cmdline and read `vm.read_serial_output()` to see guest-agent progress messages.
With `loglevel=0`, `/dev/kmsg` writes don't reach the serial console. Increase
the serial channel buffer from 4096 to 65536 if kernel messages overflow it.

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

4. **Missing IA32_XSS restore → XRSTORS #GP → stack overflow → kernel panic.**
   On kernels with CET (Control-flow Enforcement Technology, kernel ≥ 6.x on
   Fedora/Ubuntu), `IA32_XSS` (MSR 0x0DA0) has bits 11-12 set (CET_U/CET_S).
   The guest kernel's `XRSTORS` instruction uses the combined mask
   `XCR0 ∪ IA32_XSS` to restore process FPU state in compacted format.  After
   snapshot restore, `IA32_XSS` defaulted to 0 because the MSR was not
   captured.  `XRSTORS` then #GP'd on the CET features referenced by the
   fpstate's `xcomp_bv` but not enabled in `XCR0 ∪ IA32_XSS`.  The #GP
   triggered the kernel's `fixup_exception` handler, which itself tried to
   context-switch (calling `restore_fpregs_from_fpstate` again), causing a
   recursive #GP that overflowed the TASK stack guard page → kernel panic →
   `VcpuExit::Shutdown`.
   **Fix:** Added `IA32_XSS` (0x0DA0) to `SNAPSHOT_MSR_INDICES`.  Safe on
   older CPUs: the per-MSR capture loop silently skips unsupported MSRs, and
   restore only writes MSRs present in the snapshot.

**Debugging tip:** Guest reboots (port 0x64 write of 0xFE) indicate kernel
panic, not a hang. Add `panic=-1 loglevel=7 earlyprintk=serial` to kernel
cmdline and read serial output via `vm.read_serial_output()` to capture the
actual panic message.  Increase the serial channel buffer from 4096 to 65536
if kernel messages overflow it.

**Key files:** `src/vmm/cpu.rs` (capture/restore order), `src/vmm/mod.rs`
(CID preservation), `src/vmm/snapshot.rs` (VcpuState with xcrs field),
`src/vmm/arch/x86_64/cpu.rs` (`SNAPSHOT_MSR_INDICES`, `capture_vcpu_state`,
`restore_vcpu_state`).

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
- `e2e_service_mode`: service lifecycle — output publication while running,
  graceful stop, exit status mapping. **Must pass** for changes to service mode,
  daemon service lifecycle, or `ServiceStageHandle`.
- `e2e_sidecar`: sidecar intent flow — submit, inbox polling, context identity,
  idempotency. **Must pass** for changes to sidecar state, types, or server.
- `e2e_agent_mcp`: end-to-end agent (currently Claude Code) with void-mcp
  tools inside a real VM. void-mcp itself is agent-agnostic; the test
  uses Claude as the consumer because it's the only LlmProvider wired to
  consume MCP today. Requires `ANTHROPIC_API_KEY`. **Must pass** for
  changes to void-mcp tools or MCP provisioning.

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

## VM memory sizing

The VM's `memory_mb` must comfortably exceed the peak physical footprint during
early-boot initramfs extraction.  During that window the kernel holds **both**
the compressed initramfs (loaded by the bootloader into guest RAM) **and** the
decompressed tmpfs contents simultaneously, before releasing the compressed
copy.  If memory is exhausted mid-extraction the kernel silently stops creating
files — the symptom surfaces as "file not found: /lib/modules/vsock.ko" inside
the guest, with no obvious memory-pressure indicator.

**Formula** (`INITRAMFS_OVERHEAD_BYTES = 208 MB` covers kernel + runtime slack):

```
minimum_memory_mb = compressed_mb + uncompressed_mb + 208
```

Quick reference for the production image (`build_claude_rootfs.sh`, ~96 MB compressed / ~278 MB uncompressed):

| compressed | uncompressed | minimum | recommended |
|------------|-------------|---------|-------------|
| ~96 MB | ~278 MB | ~582 MB | 1024 MB |

The default spec `memory_mb` (`src/spec.rs : default_memory()`) must be set
high enough to accommodate the largest supported initramfs.  The current default
is **1024 MB**.

**Pre-flight check**: `BackendConfig::initramfs_memory_warning()` reads the
gzip ISIZE field at VM start and emits a `WARN` log if `memory_mb` is below the
computed minimum.  When adding a new backend or changing the default memory,
verify this check still fires correctly for undersized configs.
