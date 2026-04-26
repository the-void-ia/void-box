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
│  │  - PTY handler: forkpty, up to 4 concurrent sessions         │ │
│  └────────────────────────┬─────────────────────────────────────┘ │
│                           │ fork+exec (headless) or forkpty (PTY) │
│  ┌────────────────────────▼─────────────────────────────────────┐ │
│  │ Agent binary (selected by LlmProvider::binary_name())        │ │
│  │  claude-code: -p <prompt> --output-format stream-json        │ │
│  │  codex:       exec --json <prompt>                           │ │
│  │  (or claudio mock for deterministic tests)                   │ │
│  │  Interactive PTY: raw terminal I/O over vsock                │ │
│  │  Skills: ~/.claude/skills/*.md (Claude)                      │ │
│  │          ~/.codex/config.toml [mcp_servers] (Codex)          │ │
│  │  MCP:    ~/.claude/mcp.json (Claude) or config.toml (Codex) │ │
│  │  OCI skills: /skills/{python,go,...} (read-only mounts)      │ │
│  │  LLM:    Claude API / OpenAI API / Ollama (via SLIRP)        │ │
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
   ├─ sandbox.exec_agent()        Send ExecRequest over vsock (binary from provider)
   │       │
   │   [vsock port 1234]
   │       │
   │   guest-agent receives       Validates session secret
   │       │                      Checks command allowlist
   │       │                      Applies resource limits (setrlimit)
   │       │                      Drops privileges (uid:1000)
   │       │
   │   fork+exec agent binary     claude-code or codex (per LlmProvider)
   │       │
   │   agent executes             Reads skills, calls LLM, uses tools
   │       │
   │   ExecResponse sent          stdout/stderr/exit_code over vsock
   │       │
   ├─ parse agent output          ObserverKind dispatch → AgentExecResult (tokens, cost, tools)
   ├─ read output file            /workspace/output.json
   │
5. StageResult                    box_name, agent_result, file_output
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

### Interactive shell (`voidbox shell`)

```
voidbox shell --mount /project:/workspace:rw --program claude --memory-mb 3024 --vcpus 4 --network
  │
  ├─ Auto-detect LLM provider     claude-personal (OAuth) or claude (API key)
  ├─ Build ephemeral spec          kind: sandbox, synthesized from CLI flags
  │   (or load --file spec.yaml)
  │
  ├─ Build Sandbox                 kernel, initramfs, memory, vcpus, network, mounts
  │   ├─ Stage credentials         Mount ~/.claude as 9p share (claude-personal)
  │   ├─ Write onboarding flag     /home/sandbox/.claude.json (skip login screen)
  │   └─ Restore from snapshot     If --snapshot or --auto-snapshot
  │
  ├─ attach_pty(PtyOpenRequest)    Connect vsock, handshake, send PtyOpen
  │       │
  │   [vsock port 1234]
  │       │
  │   guest-agent receives         Validates allowlist
  │       │                        Acquires session slot (max 4 concurrent)
  │       │                        forkpty: child drops to uid:1000
  │       │                        Interactive mode: no RLIMIT_FSIZE
  │       │
  │   PtyOpened response           Success or error
  │       │
  ├─ RawModeGuard::engage()        Host terminal → raw mode
  │       │
  │   ┌─── I/O loop (two threads) ────────────────────────────┐
  │   │ Writer: stdin → PtyData frames → vsock → guest master │
  │   │ Reader: guest master → PtyData frames → vsock → stdout│
  │   └───────────────────────────────────────────────────────┘
  │       │
  │   PtyClosed { exit_code }      Guest process exited
  │       │
  ├─ drop(RawModeGuard)            Restore terminal
  ├─ sandbox.stop()                Stop VM
  │
  └─ exit(exit_code)               Propagate guest exit code
```

**Spec kinds:**

| Kind | Agent block | PTY | Use case |
|------|-------------|-----|----------|
| `agent` | Required | No (headless exec) | Autonomous task execution |
| `sandbox` | None | Via `voidbox shell` | Interactive development |
| `agent` + `mode: interactive` | Required (empty prompt OK) | Yes | Interactive agent with prompt context |

**Security guarantees (same as headless exec):**

Interactive PTY sessions preserve the full defense-in-depth stack:
- Layer 1: Hardware isolation (KVM/VZ) — separate kernel and memory space
- Layer 2: Seccomp-BPF on VMM thread
- Layer 3: Session secret authentication over vsock
- Layer 4: Command allowlist — only approved binaries can be exec'd via PTY
- Layer 4: Privilege drop to uid:1000 for the PTY child process
- Layer 4: Resource limits (RLIMIT_NOFILE, RLIMIT_NPROC) applied to PTY child
- Layer 5: SLIRP network isolation (rate limiting, deny list)

The only difference: `RLIMIT_FSIZE` (max file size) is skipped for interactive
sessions (`PtyOpenRequest.interactive = true`). Interactive users need to write
files freely (e.g. Claude Code conversation logs exceed 100 MB). Batch exec
retains the 100 MB limit as defense-in-depth.

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
| 0x12 | host → guest | ReadFile | Read file from guest filesystem |
| 0x13 | guest → host | ReadFileResponse | File contents or error |
| 0x14 | host → guest | FileStat | Stat a guest file path |
| 0x15 | guest → host | FileStatResponse | File metadata (size, mode, mtime) |
| 0x16 | host → guest | PtyOpen | Open interactive PTY session (program, args, env, interactive) |
| 0x17 | guest → host | PtyOpened | PTY open result (success/error) |
| 0x18 | both | PtyData | Raw terminal I/O bytes (not JSON-encoded) |
| 0x19 | host → guest | PtyResize | Terminal window size change (cols, rows) |
| 0x1A | host → guest | PtyClose | Request PTY session close (SIGHUP to child) |
| 0x1B | guest → host | PtyClosed | PTY child exited (exit_code) |

**PtyData encoding:** Unlike other messages, `PtyData` payload is raw bytes
(not JSON). This avoids base64 overhead on terminal I/O.

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

### Scope — what the sandbox does and does not defend

void-box defends the host (and the host's other local state) from a compromised agent running inside the guest VM. It does **not** defend the contents of the guest VM from that same agent. The two halves of that boundary are worth spelling out, because at first read the layered defenses above can suggest a stronger in-guest property than they actually deliver.

Once the guest-agent has authenticated, applied resource limits, and dropped privileges to uid:1000, the agent binary it spawns runs with ordinary Linux semantics inside the guest. It can `fork`, `execve` any binary on the rootfs that uid:1000 is allowed to read and execute, and write anywhere uid:1000 has write access (`/tmp`, the home directory, any `rw` host mount). There is no syscall filter on that child, no in-process policy hook between the LLM and the kernel — uid:1000, the SLIRP network policy, and `setrlimit` are the only restrictions that apply to the running agent.

`DEFAULT_COMMAND_ALLOWLIST` (in `src/backend/mod.rs`) is **not** a sandbox in that sense. It is a vsock-side gate: it controls which binary the host can ask the guest to launch as the initial child of the guest-agent. It does not constrain what that child does once it is running, including which other binaries the child invokes via `execve`. If the initial child is `claude-code` and the LLM decides to call out to `bash`, `python`, `curl`, or anything else present on the rootfs, the allowlist is not in the path of that decision.

This matters because **prompt injection is in scope for void-box's threat model**, and inside the guest it translates directly to arbitrary uid:1000-level execution. Anything the agent can read — files mounted in via 9p/virtiofs, host credentials staged for the run, the contents of `/workspace` — should be treated as exfiltratable to the LLM provider or to attacker-controlled outbound traffic that SLIRP allows. The defenses above stop a compromised agent from escaping to the host or to the host's wider local state; they do not stop it from misbehaving inside its own VM.

For the canonical statement of what is and is not in scope as a vulnerability, including the active-work items that are not yet defended, see [`SECURITY.md`](../SECURITY.md). This subsection is intended as the prose explanation; `SECURITY.md` is the policy.

<!-- TODO: link to published threat model once a public summary is released alongside the void-box source repo -->

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

### Performance

Two different latencies matter for snapshot/restore — the **host-side
snapshot/restore phase** (just the function call) and the **end-to-end
user-perceived startup** (from `Sandbox::build()` through first exec
round-trip). The second is what users actually wait for.

**Host-side phase times** — measured on Linux/KVM with 256 MB RAM,
1 vCPU, userspace virtio-vsock:

| Phase | Time | Notes |
|---|---|---|
| Base snapshot | ~420 ms | Full 256 MB memory dump |
| Base restore | ~1.3 ms | COW mmap, lazy page loading |
| Diff snapshot | ~270 ms | Only dirty pages (~1.5 MB, 0.6% of RAM) |
| Diff restore | ~3 ms | Base COW mmap + dirty page overlay |
| **Diff savings** | **99.4%** | Memory file size reduction |

**End-to-end startup (time-to-first-exec)** — measured via
`voidbox-startup-bench --iters 20 --breakdown` on Fedora 43 / KVM,
1 GiB RAM, slim kernel (`scripts/build_slim_kernel.sh`) + test rootfs
(`scripts/build_test_image.sh`):

| Path | p50 | p95 | Notes |
|---|---|---|---|
| Cold boot → first exec | **252 ms** | 259 ms | Kernel boot + vsock handshake + one exec RTT |
| Warm restore → first exec | **138 ms** | 144 ms | `from_snapshot` (sub-ms) + handshake + one exec RTT |

The warm path dwarfs the ~1.3 ms host-side restore because the guest
kernel resumes from HLT/NOHZ-idle and the host-side vsock handshake
retry loop converges only as fast as the guest-agent replies to Ping.

**Startup evolution on this runtime** — where the 19× cold speedup came from:

| Stage | Cold p50 | Warm p50 | What landed |
|-------|----------|----------|-------------|
| Baseline (pre-`feat/perf`) | ~4.9 s | ~607 ms | Blind `sleep(4s)` pre-handshake, blind `sleep(1s)` after module load, 200 ms `epoll_wait` on vsock-irq |
| After blind-wait removal | 3.5 s | 433 ms | Poll `connect()` directly, drop guest-agent module-load sleep, tighten vsock-irq epoll to 20 ms |
| After `initcall_blacklist` | 1.7 s | 140 ms | Skip distro-kernel `cmos_init` / `i8042_init` probe timeouts via default kernel cmdline |
| **After slim kernel** (current) | **252 ms** | **138 ms** | Upstream Linux v6.12.30 + Firecracker microvm config + 9p/virtiofs/overlayfs, uncompressed `vmlinux` |

**Comparison with other microVM runtimes** — approximate published numbers,
measurement methodology varies (Firecracker's published number is
"guest init complete", ours is "first exec RTT over vsock"):

| Runtime | Cold boot | Warm restore | Notes |
|---------|-----------|--------------|-------|
| Firecracker (AWS) | ~125 ms | <50 ms (snapshot) | Minimal microVM, ~5 years of optimization, purpose-built for Lambda |
| libkrun (Red Hat) | ~100–150 ms | — | Container-focused, very lean |
| cloud-hypervisor | ~100–250 ms | 100–200 ms | More features than Firecracker |
| QEMU microvm | ~300–500 ms | — | Best-case QEMU config |
| **VoidBox (this branch)** | **252 ms** | **138 ms** | Kernel boot + vsock handshake + first exec RTT |

Competitive for a general-purpose agent runtime; behind Firecracker's
Lambda-specialized numbers. Further gains depend on PVH direct-kernel
boot (skips `linux-loader` parsing) and smaller-still guest kernel
configs.

### Storage layout

```
~/.void-box/snapshots/
  └── <hash-prefix>/        # first 16 chars of config hash
      ├── state.bin          # postcard: VmSnapshot (vCPU regs, irqchip, PIT, vsock, config)
      ├── memory.mem         # full memory dump (base)
      └── memory.diff        # dirty pages only (diff snapshots)
```

### Restore flow

```
1. VmSnapshot::load(dir)           Read state.bin (vCPU, irqchip, PIT, vsock, config)
2. Vm::new(memory_mb)              Create KVM VM with matching memory size
3. restore_memory(mem, path)       COW mmap(MAP_PRIVATE|MAP_FIXED) — lazy page loading
4. vm.restore_irqchip(state)       Restore PIC master/slave + IOAPIC
5. VirtioVsockMmio::restore()      Restore vsock device registers (userspace backend)
6. create_vcpu_restored(state)     Per-vCPU restore (see register restore order below)
7. vCPU threads resume             Guest continues execution from snapshot point
```

Memory restore uses kernel `MAP_PRIVATE` lazy page loading — pages are demand-faulted from the file, writes create anonymous copies. No userfaultfd required.

#### vCPU register restore order

The restore sequence in `cpu.rs` is order-sensitive. Getting it wrong causes
silent guest crashes (kernel panic → reboot via port 0x64).

```
1. MSRs              KVM_SET_MSRS
2. sregs             KVM_SET_SREGS (segment regs, CR0/CR3/CR4)
3. LAPIC             KVM_SET_LAPIC + periodic timer bootstrap (see below)
4. vcpu_events       KVM_SET_VCPU_EVENTS (exception/interrupt state)
5. XCRs (XCR0)       KVM_SET_XCRS — MUST come before xsave
6. xsave (FPU/SSE)  KVM_SET_XSAVE — depends on XCR0 for feature mask
7. regs              KVM_SET_REGS (GP registers, RIP, RFLAGS)
```

**XCR0 restore is critical.** XCR0 controls which XSAVE features (x87, SSE,
AVX) are active. Without it, the guest's `XRSTORS` instruction triggers a #GP
because the default XCR0 only enables x87, but the guest's XSAVE area
references SSE/AVX features. This manifests as "Bad FPU state detected at
restore_fpregs_from_fpstate" → kernel panic → reboot loop.

#### LAPIC timer bootstrap

When the guest was idle (NO_HZ) at snapshot time, the LAPIC timer is masked
with vector=0 (LVTT=0x10000). After restore, no timer interrupt ever fires,
so the scheduler never runs. The restore code detects this state and
bootstraps a periodic LAPIC timer (mode=periodic, vector=0xEC, TMICT=0x200000,
TDCR=divide-by-1) to kick the scheduler back to life.

#### Vsock backend for snapshot

The **userspace** virtio-vsock backend must be used for VMs that will be
snapshotted. The kernel vhost backend (`/dev/vhost-vsock`) does not expose
internal vring indices, making queue state capture incomplete. The userspace
backend tracks `last_avail_idx`/`last_used_idx` directly, ensuring clean
snapshot/restore of the virtqueue state.

#### CID preservation

The snapshot stores the VM's actual CID (assigned at cold boot). On restore,
the same CID is reused — the guest kernel caches the CID during virtio-vsock
probe and silently drops packets with mismatched `dst_cid`.

### Opt-in plumbing

Every layer has an optional snapshot field that defaults to `None`:

| Layer | Field | Type | Default |
|---|---|---|---|
| `SandboxBuilder` | `.snapshot(path)` | `Option<PathBuf>` | `None` |
| `BoxConfig` | `snapshot` | `Option<PathBuf>` | `None` |
| `SandboxSpec` (YAML) | `sandbox.snapshot` | `Option<String>` | `None` |
| `BoxSandboxOverride` | `sandbox.snapshot` | `Option<String>` | `None` |
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

### macOS / VZ snapshots

The VZ backend uses Apple's native save/restore APIs rather than a custom memory/vCPU capture pipeline. This keeps VoidBox out of the business of serializing Apple's private VM state, but adds a strict constraint: Apple rejects any restore whose `VZVirtualMachineConfiguration` drifts from the one used at save time (platform identifier, device set, memory, vCPUs, kernel cmdline). The sidecar + reconciliation helper exists to cope with that constraint on cold hosts.

#### Save/restore flow

```
1. Pause VM                          VZVirtualMachine.pause
2. saveMachineStateToURL:            Apple writes opaque VM state blob
3. Write vz_meta.json sidecar        VzSnapshotMeta (VoidBox-specific fields)
4. stop() (from paused)              No resume/pause round-trip for auto-snapshot

Cold restore:
1. Read vz_meta.json                 Recover VZGenericMachineIdentifier + config
2. Reconcile with caller config      Override drifting memory/vcpus/network
3. Build VZVirtualMachineConfiguration using the saved identifier
4. restoreMachineStateFromURL:       Apple restores opaque state
5. Resume                            Guest continues execution
```

#### VzSnapshotMeta sidecar

Alongside Apple's opaque save blob (`vm.vzvmsave`), VoidBox persists `vz_meta.json` containing the fields Apple needs to reconstruct an identical configuration plus the VoidBox-specific continuity bits:

| Field | Purpose |
|---|---|
| `session_secret` | Guest-agent auth token baked into kernel cmdline at save time |
| `memory_mb`, `vcpus`, `network` | Reconciliation targets — override caller config if drifting |
| `boot_clock_secs` | Wall-clock at save; cmdline must match at restore so post-restore clock sync happens over the control channel, not in the boot args |
| `config_hash` | Continuity check against the caller's `BackendConfig` |
| `machine_identifier` | `VZGenericMachineIdentifier.dataRepresentation` — Apple refuses to restore without the original identifier |

#### Storage layout (VZ)

```
~/.void-box/snapshots/
  └── <hash-prefix>/
      ├── vm.vzvmsave          # Apple's opaque save blob (saveMachineStateToURL:)
      └── vz_meta.json         # VzSnapshotMeta sidecar (JSON)
```

#### Device-set drift reconciliation

Apple's restore API returns failure as soon as the caller's `VZVirtualMachineConfiguration` differs from the configuration that was active at save time. Most of those fields are user-visible knobs (memory, vCPUs, network), so if the caller supplies a drifting `BackendConfig` we prefer the sidecar values silently rather than surface a validation failure that the caller can't usefully recover from. The reconciliation helper lives in `src/backend/vz/snapshot.rs`; the regression test `snapshot_vz_restore_overrides_drifting_config` guards the contract on real hardware.

#### `enable_snapshots` opt-in

`SandboxBuilder::enable_snapshots(true)` (plumbed through `SandboxConfig` → `BackendConfig`) gates Apple's `validateSaveRestoreSupportWithError` call at cold boot. Some device sets (e.g. virtiofs shares) make Apple reject snapshot capability validation even when the VM runs fine for non-snapshot workloads — cold boots that do not opt in skip the check and keep working.

#### CI limitation

Apple's VZ stack requires access to the bare-metal hypervisor (EL2). GitHub's hosted `macos-14` / `macos-15` arm64 runners themselves run each job inside a VZ guest, so `VZVirtualMachine::validateWithError` fails with "Virtualization is not available on this hardware". The `snapshot_vz_integration` suite uses the skip-on-start-failure pattern to no-op on such runners; real VZ coverage requires a self-hosted bare-metal Mac runner.

#### Key files (VZ)

| File | Role |
|---|---|
| `src/backend/vz/snapshot.rs` | `VzSnapshotMeta`, drift reconciliation helper |
| `src/backend/vz/backend.rs` | `pause()`, `resume()`, `create_snapshot()`, `create_auto_snapshot()`, restore branch of `start()` |
| `tests/snapshot_vz_integration.rs` | Cold boot → snapshot → restore, auto-snapshot, CLI list/delete, device-set drift |

## Developer Notes

For contributor setup, lint/test parity commands, and script usage, see
`CONTRIBUTING.md`.

For runtime setup commands and end-user usage examples, see `README.md`.

## Skill Types

| Type | Constructor | Provisioned as | Example |
|---|---|---|---|
| Agent | `Skill::agent("claude-code")` or `Skill::agent("codex")` | Reasoning engine designation | Selected by `LlmProvider::binary_name()` |
| File | `Skill::file("path/to/SKILL.md")` | `~/.claude/skills/{name}.md` | Domain methodology |
| Remote | `Skill::remote("owner/repo/skill")` | Fetched from GitHub, written to skills/ | `obra/superpowers/brainstorming` |
| MCP | `Skill::mcp("server-name")` | Entry in `~/.claude/mcp.json` | Structured tool server |
| CLI | `Skill::cli("jq")` | Expected in guest initramfs | Binary tool |
