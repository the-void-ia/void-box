# Architecture

## Overview

void-box is a composable agent runtime where each agent runs in a hardware-isolated micro-VM. On Linux this uses KVM; on macOS (Apple Silicon) it uses Virtualization.framework (VZ). The core equation is:

```
VoidBox = Agent(Skills) + Isolation
```

A **VoidBox** binds declared skills (MCP servers, CLI tools, procedural knowledge files, reasoning engines) to an isolated execution environment. Boxes compose into pipelines where output flows between stages, each in a fresh VM.

## Component Diagram

```
┌──────────────────────────────────────────────────────────────────┐
│ User / Daemon / CLI                                              │
│                                                                  │
│  ┌──────────────────────────────────────────────────────────┐    │
│  │ VoidBox (agent_box.rs)                                   │    │
│  │  name: "analyst"                                         │    │
│  │  prompt: "Analyze AAPL..."                               │    │
│  │  skills: [claude-code, financial-data.md, market-mcp]    │    │
│  │  config: memory=1024MB, vcpus=1, network=true            │    │
│  └─────────────────────┬────────────────────────────────────┘    │
│                        │ resolve_guest_image() → .build() → .run()
│  ┌─────────────────────▼───────────────────────────────────┐     │
│  │ OCI Client (voidbox-oci/)                               │     │
│  │  guest image → kernel + initramfs  (auto-pull, cached)  │     │
│  │  base image  → rootfs              (pivot_root)         │     │
│  │  OCI skills  → read-only mounts    (/skills/...)        │     │
│  │  cache: ~/.voidbox/oci/{blobs,rootfs,guest}/            │     │
│  └─────────────────────┬───────────────────────────────────┘     │
│                        │                                         │
│  ┌─────────────────────▼───────────────────────────────────┐     │
│  │ Sandbox (sandbox/)                                      │     │
│  │  ┌─────────────┐  ┌──────────────┐                      │     │
│  │  │ MockSandbox │  │ LocalSandbox │                      │     │
│  │  │ (testing)   │  │ (KVM / VZ)   │                      │     │
│  │  └─────────────┘  └──────┬───────┘                      │     │
│  └──────────────────────────┼──────────────────────────────┘     │
│                             │                                    │
│  ┌──────────────────────────▼──────────────────────────────┐     │
│  │ MicroVm (vmm/)                                          │     │
│  │  ┌────────┐ ┌────────┐ ┌─────────────┐ ┌──────────────┐ │     │
│  │  │ KVM VM │ │ vCPU   │ │ VsockDevice │ │ VirtioNet    │ │     │
│  │  │        │ │ thread │ │ (AF_VSOCK)  │ │ (SLIRP)      │ │     │
│  │  └────────┘ └────────┘ └───────┬─────┘ └───────┬──────┘ │     │
│  │  Linux/KVM: virtio-blk (OCI rootfs)            │        │     │
│  │  9p/virtiofs: skills + host mounts             │        │     │
│  │  Seccomp-BPF on VMM thread    │               │        │     │
│  └────────────────────────────────┼───────────────┼────────┘     │
│                                   │               │              │
└═══════════════════════════════════╪═══════════════╪══════════════┘
              Hardware Isolation    │               │
                                    │ vsock:1234    │ SLIRP NAT
┌───────────────────────────────────▼───────────────▼───────────────┐
│ Guest VM (Linux kernel)                                           │
│                                                                   │
│  ┌──────────────────────────────────────────────────────────────┐ │
│  │ guest-agent (PID 1)                                          │ │
│  │  - Authenticates via session secret (kernel cmdline)         │ │
│  │  - Reads /etc/voidbox/allowed_commands.json                  │ │
│  │  - Reads /etc/voidbox/resource_limits.json                   │ │
│  │  - Applies setrlimit + command allowlist                     │ │
│  │  - Drops privileges to uid:1000                              │ │
│  │  - Listens on vsock port 1234                                │ │
│  │  - pivot_root to OCI rootfs (if sandbox.image set)           │ │
│  └────────────────────────┬─────────────────────────────────────┘ │
│                           │ fork+exec                             │
│  ┌────────────────────────▼─────────────────────────────────────┐ │
│  │ claude-code (or claudio mock)                                │ │
│  │  --output-format stream-json                                 │ │
│  │  --dangerously-skip-permissions                              │ │
│  │  Skills: ~/.claude/skills/*.md                               │ │
│  │  MCP:    ~/.claude/mcp.json                                  │ │
│  │  OCI skills: /skills/{python,go,...} (read-only mounts)      │ │
│  │  LLM:    Claude API / Ollama (via SLIRP → host:11434)        │ │
│  └──────────────────────────────────────────────────────────────┘ │
│                                                                   │
│  eth0: 10.0.2.15/24  gw: 10.0.2.2  dns: 10.0.2.3                  │
└───────────────────────────────────────────────────────────────────┘
```

## Data Flow

### Single VoidBox execution

```
1. VoidBox::new("name")           User declares skills, prompt, config
       │
2. resolve_guest_image()          Resolve kernel + initramfs (5-step chain)
       │                          Pulls from GHCR if no local paths found
       │
3. .build()                       Creates Sandbox (mock or local VM backend: KVM/VZ)
       │                          Mounts OCI rootfs + skill images if configured
       │
4. .run(input)                    Execution begins
       │
   ├─ provision_security()        Write resource limits + allowlist to /etc/voidbox/
   ├─ provision_skills()          Write SKILL.md files to ~/.claude/skills/
   │                              Write mcp.json to ~/.claude/
   ├─ write input                 Write /workspace/input.json (if piped from previous stage)
   │
   ├─ sandbox.exec_claude()       Send ExecRequest over vsock
   │       │
   │   [vsock port 1234]
   │       │
   │   guest-agent receives       Validates session secret
   │       │                      Checks command allowlist
   │       │                      Applies resource limits (setrlimit)
   │       │                      Drops privileges (uid:1000)
   │       │
   │   fork+exec claude-code      Runs with --output-format stream-json
   │       │
   │   claude-code executes       Reads skills, calls LLM, uses tools
   │       │
   │   ExecResponse sent          stdout/stderr/exit_code over vsock
   │       │
   ├─ parse stream-json           Extract ClaudeExecResult (tokens, cost, tools)
   ├─ read output file            /workspace/output.json
   │
5. StageResult                    box_name, claude_result, file_output
```

### Pipeline execution

```
Pipeline::named("analysis", box1)
    .pipe(box2)                    Sequential: box1.output → box2.input
    .fan_out(vec![box3, box4])     Parallel: both receive box2.output
    .pipe(box5)                    Sequential: merged [box3, box4] → box5.input
    .run()

Stage flow:
  box1.run(None)          → carry_data = output bytes
  box2.run(carry_data)    → carry_data = output bytes
  [box3, box4].run(carry) → carry_data = JSON array merge
  box5.run(carry_data)    → PipelineResult
```

For parallel stages (`fan_out`), each box runs in a separate `tokio::task::JoinSet`. Their outputs are merged as a JSON array for the next stage.

## Wire Protocol

Host and guest communicate over AF_VSOCK (port 1234) using the `void-box-protocol` crate.

### Frame format

```
┌──────────────┬───────────┬──────────────────┐
│ length (4 B) │ type (1B) │ payload (N bytes)│
└──────────────┴───────────┴──────────────────┘
```

- **length**: `u32` little-endian, payload size only (excludes the 5-byte header)
- **type**: message type discriminant
- **payload**: JSON-encoded body

### Message types

| Type byte | Direction | Message | Description |
|---|---|---|---|
| 0x01 | host → guest | ExecRequest | Execute a command (program, args, env, timeout) |
| 0x02 | guest → host | ExecResponse | Command result (stdout, stderr, exit_code) |
| 0x03 | both | Ping/Pong | Session authentication handshake |
| 0x04 | guest → host | Pong | Authentication reply with protocol version |
| 0x05 | host → guest | Shutdown | Request guest shutdown |
| 0x0A | host → guest | SubscribeTelemetry | Start telemetry stream |
| 0x0B | host → guest | WriteFile | Write file to guest filesystem |
| 0x0C | guest → host | WriteFileResponse | Write file acknowledgement |
| 0x0D | host → guest | MkdirP | Create directory tree |
| 0x0E | guest → host | MkdirPResponse | Mkdir acknowledgement |
| 0x0F | guest → host | ExecOutputChunk | Streaming output chunk (stream, data, seq) |
| 0x10 | host → guest | ExecOutputAck | Flow control ack (optional) |
| 0x11 | both | SnapshotReady | Guest signals readiness for live snapshot |

### Security

- **MAX_MESSAGE_SIZE**: 64 MB -- prevents OOM from untrusted length fields
- **Session secret**: 32-byte hex token injected as `voidbox.secret=<hex>` in kernel cmdline. The guest-agent reads it from `/proc/cmdline` at boot and requires it in every ExecRequest. Without the correct secret, the guest-agent rejects the request.
- **ExecRequest Debug impl**: Redacts environment variables matching `KEY`, `SECRET`, `TOKEN`, `PASSWORD` patterns

## Network Layout (SLIRP)

void-box uses smoltcp-based usermode networking (SLIRP) -- no root, no TAP devices, no bridge configuration.

```
Guest VM                                    Host
┌─────────────────────┐                    ┌──────────────────┐
│ eth0: 10.0.2.15/24  │                    │                  │
│ gw:   10.0.2.2      │── virtio-net ──────│ SLIRP stack      │
│ dns:  10.0.2.3      │   (MMIO)           │ (smoltcp)        │
└─────────────────────┘                    │                  │
                                           │ 10.0.2.2 → NAT   │
                                           │   → 127.0.0.1    │
                                           └──────────────────┘
```

- Guest IP: `10.0.2.15/24`
- Gateway: `10.0.2.2` (mapped to host `127.0.0.1`)
- DNS: `10.0.2.3` (forwarded to host resolver)
- Outbound TCP/UDP is NATed through the host
- The guest reaches host services (Ollama on `:11434`) via `10.0.2.2`

### SLIRP security

- Rate limiting on new connections
- Maximum concurrent connection limit
- CIDR deny list (configurable via `ipnet`)

## Security Model

### Defense in depth

```
Layer 1: Hardware isolation (KVM)
  └─ Separate kernel, memory space, devices per VM

Layer 2: Seccomp-BPF (VMM process)
  └─ VMM thread restricted to KVM ioctls + vsock + networking syscalls

Layer 3: Session authentication (vsock)
  └─ 32-byte random secret, per-VM, injected at boot

Layer 4: Guest hardening (guest-agent)
  ├─ Command allowlist (only approved binaries execute)
  ├─ Resource limits via setrlimit (memory, files, processes)
  ├─ Privilege drop to uid:1000 for child processes
  └─ Timeout watchdog with SIGKILL

Layer 5: Network isolation (SLIRP)
  ├─ Rate limiting
  ├─ Max concurrent connections
  └─ CIDR deny list
```

### Session secret flow

```
Host                                    Guest
  │                                       │
  ├─ getrandom(32 bytes)                  │
  ├─ hex-encode → kernel cmdline          │
  │   voidbox.secret=abc123...            │
  │                                       │
  │              boot                     │
  │ ─────────────────────────────────────>│
  │                                       ├─ parse /proc/cmdline
  │                                       ├─ store in OnceLock
  │                                       │
  ├─ ExecRequest { secret: "abc123..." }  │
  │ ─────────────────────────────────────>│
  │                                       ├─ verify secret
  │                                       ├─ execute if match
  │ <─────────────────────────────────────┤
  │  ExecResponse { ... }                 │
```

## Observability

### Trace structure

```
Pipeline span
  └─ Stage 1 span (box_name="data_analyst")
       ├─ tool_call event: Read("input.json")
       ├─ tool_call event: Bash("curl ...")
       └─ attributes: tokens_in, tokens_out, cost_usd, model
  └─ Stage 2 span (box_name="quant_analyst")
       └─ ...
```

### Guest telemetry

The guest-agent periodically reads `/proc/stat`, `/proc/meminfo` and sends `TelemetryBatch` messages over vsock. The host-side `TelemetryAggregator` ingests these and exports as OTLP metrics.

### Configuration

| Env var | Description |
|---|---|
| `VOIDBOX_OTLP_ENDPOINT` | OTLP gRPC endpoint (e.g. `http://localhost:4317`) |
| `OTEL_SERVICE_NAME` | Service name for traces (default: `void-box`) |

Enable at compile time: `cargo build --features opentelemetry`

## OCI Image Support

VoidBox uses OCI container images at three levels, all cached at `~/.voidbox/oci/`.

### Guest image (`sandbox.guest_image`)

Pre-built kernel + initramfs distributed as a `FROM scratch` OCI image containing two files: `vmlinuz` and `rootfs.cpio.gz`. Auto-pulled from GHCR on first run — no local toolchain needed.

```
Resolution order:
  1. sandbox.kernel / sandbox.initramfs   (explicit paths in spec)
  2. VOID_BOX_KERNEL / VOID_BOX_INITRAMFS (env vars)
  3. sandbox.guest_image                  (explicit OCI ref)
  4. ghcr.io/the-void-ia/voidbox-guest:v{version}  (default auto-pull)
  5. None → mock fallback (mode: auto)
```

Cache layout: `~/.voidbox/oci/guest/<sha256>/vmlinuz` + `rootfs.cpio.gz` + `<sha256>.done` marker.

### Base image (`sandbox.image`)

Full container image (e.g. `python:3.12-slim`) used as the guest root filesystem.

- Linux/KVM: host builds a cached ext4 disk artifact from the extracted OCI rootfs and attaches it as `virtio-blk` (guest sees `/dev/vda`).
- macOS/VZ: rootfs remains directory-mounted (virtiofs path).
- Guest-agent switches root with overlayfs + `pivot_root` (or secure switch-root fallback when kernel returns `EINVAL` for initramfs root).

Security properties are preserved across both paths:
- OCI root switch is driven only by kernel cmdline flags set by the trusted host.
- command allowlist + authenticated vsock control channel still gate execution.
- writable layer is tmpfs-backed; base OCI lowerdir remains read-only.

Cache layout: `~/.voidbox/oci/rootfs/<sha256>/` (full layer extraction with whiteout handling).

### OCI skills

Container images mounted read-only at arbitrary guest paths (e.g. `/skills/python`). Each skill image is pulled, extracted, and mounted independently — no `sandbox.image` required. Declared in the spec:

```yaml
skills:
  - image: "python:3.12-slim"
    mount: "/skills/python"
  - image: "golang:1.23-alpine"
    mount: "/skills/go"
```

### OCI client internals (`voidbox-oci/`)

| Module | Purpose |
|---|---|
| `registry.rs` | OCI Distribution HTTP client (anonymous + bearer auth, HTTP for localhost) |
| `manifest.rs` | Manifest / image index parsing, platform selection |
| `cache.rs` | Content-addressed blob cache + rootfs/guest done markers |
| `unpack.rs` | Layer extraction (full rootfs with whiteouts, or selective guest file extraction) |
| `lib.rs` | `OciClient`: `pull()`, `resolve_rootfs()`, `resolve_guest_files()` |

## Snapshots

VoidBox supports three types of VM snapshots for sub-second restore. All snapshot features are **explicit opt-in only** — no snapshot code runs unless the user declares a snapshot path.

### Snapshot types

| Type | When created | Contents | Use case |
|---|---|---|---|
| **Base** | After cold boot, VM stopped | Full memory dump + all KVM state | Golden image for repeated boots |
| **Diff** | After dirty tracking enabled, VM stopped | Only modified pages since base | Layered caching (base + delta) |
| **PostInit** | After warmup commands, VM **keeps running** | Full memory + state (live capture) | Pre-warmed environments |

### Storage layout

```
~/.void-box/snapshots/
  └── <hash-prefix>/        # first 16 chars of config hash
      ├── state.bin          # bincode: VmSnapshot (vCPU regs, irqchip, PIT, vsock, config)
      ├── memory.mem         # full memory dump (base/postinit)
      └── memory.diff        # dirty pages only (diff snapshots)
```

### Restore flow

```
1. VmSnapshot::load(dir)           Read state.bin (vCPU, irqchip, PIT, vsock state)
2. Vm::new(memory_mb)              Create KVM VM with matching memory size
3. restore_memory(mem, path)       COW mmap(MAP_PRIVATE|MAP_FIXED) — lazy page loading
4. vm.restore_irqchip(state)       Restore PIC master/slave + IOAPIC
5. vm.restore_pit(state)           Restore PIT timer
6. VirtioVsockMmio::restore()      Restore vsock device registers
7. create_vcpu_restored(state)     Set CPUID + restore full register state per vCPU
8. vCPU threads resume             Guest continues execution from snapshot point
```

Memory restore uses kernel `MAP_PRIVATE` lazy page loading — pages are demand-faulted from the file, writes create anonymous copies. No userfaultfd required.

### Live snapshot (PostInit)

For PostInit snapshots, the VM stays running:

```
1. Host sets snapshot_requested flag
2. Each vCPU, after its current KVM_RUN exit:
   a. Captures its own register state
   b. Waits on barrier (all vCPUs paused)
3. Host thread waits on barrier (all vCPUs paused)
4. Host captures: irqchip, PIT, vsock state, full memory dump
5. Host clears flag, releases barrier
6. vCPUs resume execution
```

### Opt-in plumbing

Every layer has an optional snapshot field that defaults to `None`:

| Layer | Field | Type | Default |
|---|---|---|---|
| `SandboxBuilder` | `.snapshot(path)` | `Option<PathBuf>` | `None` |
| `BoxConfig` | `snapshot` | `Option<PathBuf>` | `None` |
| `BoxConfig` | `warmup` | `Option<WarmupSpec>` | `None` |
| `SandboxSpec` (YAML) | `sandbox.snapshot` | `Option<String>` | `None` |
| `BoxSandboxOverride` | `sandbox.snapshot` | `Option<String>` | `None` |
| `PipelineBoxSpec` | `warmup` | `Option<WarmupSpec>` | `None` |
| `CreateRunRequest` (API) | `snapshot` | `Option<String>` | `None` |

Resolution chain: per-box override → top-level spec → `None` (cold boot).

### Snapshot resolution

When a snapshot string is provided, the runtime resolves it as:

1. **Hash prefix** → `~/.void-box/snapshots/<prefix>/` (if `state.bin` exists)
2. **Literal path** → treat as directory path (if `state.bin` exists)
3. **Neither** → warning printed, cold boot

No env var fallback, no auto-detection.

### Cache management

- **LRU eviction**: `evict_lru(max_bytes)` removes oldest snapshots first
- **Layer hashing**: `compute_layer_hash(base, layer, content)` for deterministic cache keys
- **Listing**: `list_snapshots()` / `voidbox snapshot list`
- **Deletion**: `delete_snapshot(prefix)` / `voidbox snapshot delete <prefix>`

### Security considerations

Snapshot cloning shares identical VM state across restored instances:

- **RNG entropy**: Restored VMs inherit the same `/dev/urandom` pool. Mitigated by: fresh CID per restore, hardware `RDRAND` re-seeding on `rdtsc`
- **ASLR**: Clones share guest page table layout. Mitigated by: short-lived tasks, no direct network addressability (SLIRP NAT), command allowlist limiting attack surface
- **Session isolation**: Restored VMs reuse the snapshot's stored session secret for vsock authentication (the secret is baked into the guest's kernel cmdline in snapshot memory). Per-restore secret rotation would require guest-side support

## Developer Notes

For contributor setup, lint/test parity commands, and script usage, see
`CONTRIBUTING.md`.

For runtime setup commands and end-user usage examples, see `README.md`.

## Skill Types

| Type | Constructor | Provisioned as | Example |
|---|---|---|---|
| Agent | `Skill::agent("claude-code")` | Reasoning engine designation | The LLM itself |
| File | `Skill::file("path/to/SKILL.md")` | `~/.claude/skills/{name}.md` | Domain methodology |
| Remote | `Skill::remote("owner/repo/skill")` | Fetched from GitHub, written to skills/ | `obra/superpowers/brainstorming` |
| MCP | `Skill::mcp("server-name")` | Entry in `~/.claude/mcp.json` | Structured tool server |
| CLI | `Skill::cli("jq")` | Expected in guest initramfs | Binary tool |
