# Architecture

## Overview

void-box is a composable agent runtime where each agent runs in a hardware-isolated KVM micro-VM. The core equation is:

```
VoidBox = Agent(Skills) + Environment
```

A **VoidBox** binds declared skills (MCP servers, CLI tools, procedural knowledge files, reasoning engines) to an isolated execution environment. Boxes compose into pipelines where output flows between stages, each in a fresh VM.

## Component Diagram

```
┌──────────────────────────────────────────────────────────────────┐
│ User / Daemon / CLI                                               │
│                                                                   │
│  ┌──────────────────────────────────────────────────────────┐    │
│  │ VoidBox (agent_box.rs)                                    │    │
│  │  name: "analyst"                                          │    │
│  │  prompt: "Analyze AAPL..."                                │    │
│  │  skills: [claude-code, financial-data.md, market-mcp]     │    │
│  │  config: memory=1024MB, vcpus=1, network=true             │    │
│  └─────────────────────┬────────────────────────────────────┘    │
│                         │ .build() → .run()                       │
│  ┌──────────────────────▼───────────────────────────────────┐    │
│  │ Sandbox (sandbox/)                                        │    │
│  │  ┌─────────────┐  ┌──────────────┐                       │    │
│  │  │ MockSandbox  │  │ LocalSandbox │                       │    │
│  │  │ (testing)    │  │ (KVM)        │                       │    │
│  │  └─────────────┘  └──────┬───────┘                       │    │
│  └───────────────────────────┼──────────────────────────────┘    │
│                               │                                   │
│  ┌────────────────────────────▼─────────────────────────────┐    │
│  │ MicroVm (vmm/)                                            │    │
│  │  ┌────────┐ ┌────────┐ ┌─────────────┐ ┌──────────────┐ │    │
│  │  │ KVM VM │ │ vCPU   │ │ VsockDevice │ │ VirtioNet    │ │    │
│  │  │        │ │ thread │ │ (AF_VSOCK)  │ │ (SLIRP)      │ │    │
│  │  └────────┘ └────────┘ └──────┬──────┘ └──────┬───────┘ │    │
│  │  Seccomp-BPF on VMM thread    │               │          │    │
│  └────────────────────────────────┼───────────────┼──────────┘    │
│                                   │               │               │
└═══════════════════════════════════╪═══════════════╪═══════════════┘
              Hardware Isolation    │               │
                                    │ vsock:1234    │ SLIRP NAT
┌───────────────────────────────────▼───────────────▼───────────────┐
│ Guest VM (Linux kernel)                                            │
│                                                                    │
│  ┌──────────────────────────────────────────────────────────────┐ │
│  │ guest-agent (PID 1)                                          │ │
│  │  - Authenticates via session secret (kernel cmdline)          │ │
│  │  - Reads /etc/voidbox/allowed_commands.json                   │ │
│  │  - Reads /etc/voidbox/resource_limits.json                    │ │
│  │  - Applies setrlimit + command allowlist                      │ │
│  │  - Drops privileges to uid:1000                               │ │
│  │  - Listens on vsock port 1234                                 │ │
│  └────────────────────────┬─────────────────────────────────────┘ │
│                            │ fork+exec                             │
│  ┌─────────────────────────▼────────────────────────────────────┐ │
│  │ claude-code (or claudio mock)                                 │ │
│  │  --output-format stream-json                                  │ │
│  │  --dangerously-skip-permissions                               │ │
│  │  Skills: ~/.claude/skills/*.md                                │ │
│  │  MCP:    ~/.claude/mcp.json                                   │ │
│  │  LLM:    Claude API / Ollama (via SLIRP → host:11434)        │ │
│  └──────────────────────────────────────────────────────────────┘ │
│                                                                    │
│  eth0: 10.0.2.15/24  gw: 10.0.2.2  dns: 10.0.2.3               │
└────────────────────────────────────────────────────────────────────┘
```

## Data Flow

### Single VoidBox execution

```
1. VoidBox::new("name")           User declares skills, prompt, config
       │
2. .build()                       Creates Sandbox (mock or KVM MicroVm)
       │
3. .run(input)                    Execution begins
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
4. StageResult                    box_name, claude_result, file_output
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
│ length (4 B) │ type (1B) │ payload (N bytes) │
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
| 0x03 | guest → host | ExecOutputChunk | Streaming output chunk (stream, data, seq) |
| 0x04 | host → guest | WriteFileRequest | Write file to guest filesystem |
| 0x05 | guest → host | WriteFileResponse | Write file acknowledgement |
| 0x06 | host → guest | MkdirPRequest | Create directory tree |
| 0x07 | guest → host | MkdirPResponse | Mkdir acknowledgement |

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

## Skill Types

| Type | Constructor | Provisioned as | Example |
|---|---|---|---|
| Agent | `Skill::agent("claude-code")` | Reasoning engine designation | The LLM itself |
| File | `Skill::file("path/to/SKILL.md")` | `~/.claude/skills/{name}.md` | Domain methodology |
| Remote | `Skill::remote("owner/repo/skill")` | Fetched from GitHub, written to skills/ | `obra/superpowers/brainstorming` |
| MCP | `Skill::mcp("server-name")` | Entry in `~/.claude/mcp.json` | Structured tool server |
| CLI | `Skill::cli("jq")` | Expected in guest initramfs | Binary tool |
