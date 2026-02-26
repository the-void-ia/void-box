<div align="center">
  <img src="assets/logo/void-box.png" alt="void-box" width="400">
  <h1>Void-Box</h1>
  <p><strong>Composable agent runtime with enforced isolation boundaries</strong></p>

  <p>
    <em><strong>Design principle:</strong></em> Skills are declared capabilities.<br>
    Capabilities only exist when bound to an isolated execution boundary.
  </p>

  <p><code>VoidBox = Agent(Skills) + Isolation</code></p>

  <!-- CI badge (official GitHub Actions badge) -->
  <a href="https://github.com/the-void-ia/void-box/actions/workflows/ci.yml">
    <img src="https://github.com/the-void-ia/void-box/actions/workflows/ci.yml/badge.svg?branch=main" alt="CI">
  </a>

  <a href="https://github.com/the-void-ia/void-box/blob/main/LICENSE">
    <img src="https://img.shields.io/badge/license-Apache--2.0-blue?style=flat-square" alt="License">
  </a>
  <img src="https://img.shields.io/badge/rust-1.83%2B-orange?style=flat-square&logo=rust" alt="Rust 1.83+">
</div>

<br>

<p align="center">
  <a href="docs/architecture.md">Architecture</a> ·
  <a href="#quick-start">Quick Start</a> ·
  <a href="#oci-container-support">OCI Support</a> ·
  <a href="#host-mounts">Host Mounts</a> ·
  <a href="#observability">Observability</a>
</p>

<p align="center">
  Local-first. Cloud-ready. Runs on any Linux host with <code>/dev/kvm</code>.
</p>

> **Status:** v0 (early release). Production-ready architecture; APIs are still stabilizing.

<p align="center">
  <img src="site/assets/img/hn_demo.gif" alt="hn_demo — two-stage stock analysis pipeline" width="800">
</p>

---

## What You Get

- **Isolated execution** — Each stage runs inside its own micro-VM boundary (not shared-process containers).
- **Policy-enforced runtime** — Command allowlists, resource limits, seccomp-BPF, and controlled network egress.
- **Skill-native model** — MCP servers, SKILL files, and CLI tools mounted as declared capabilities.
- **Composable pipelines** — Sequential `.pipe()`, parallel `.fan_out()`, with explicit stage-level failure domains.
- **Claude Code native runtime** — Each stage runs `claude-code`, backed by Claude (default) or Ollama via Claude-compatible provider mode.
- **OCI-native** — Auto-pulls guest images (kernel + initramfs) from GHCR on first run. Mount container images as base OS or as skill providers — no local build steps required.
- **Observability native** — OTLP traces, metrics, structured logs, and stage-level telemetry emitted by design.
- **Persistent host mounts** — Share host directories into guest VMs via 9p/virtiofs with explicit read-only or read-write mode. Data in `mode: rw` mounts persists across VM restarts.
- **No root required** — Usermode SLIRP networking via smoltcp (no TAP devices).

> Isolation is the primitive. Pipelines are compositions of bounded execution environments.

## Why Not Containers?

Containers share a host kernel.

For general application isolation, this is often sufficient.
For AI agents executing tools, code, and external integrations, it creates shared failure domains.

In a shared-process model:

- Tool execution and agent runtime share the same kernel.
- Escape surfaces are reduced, but not eliminated.
- Resource isolation depends on cgroups and cooperative enforcement.

VoidBox binds each agent stage to its own micro-VM boundary.

Isolation is enforced by hardware virtualization — not advisory process controls.

---

## Quick Start

### 1. Add dependency

```bash
cargo add void-box
```

### 2. Define skills and build a VoidBox

#### Rust API

```rust
use void_box::agent_box::VoidBox;
use void_box::skill::Skill;
use void_box::llm::LlmProvider;

// Skills = declared capabilities
let hn_api = Skill::file("skills/hackernews-api.md")
    .description("HN API via curl + jq");

let reasoning = Skill::agent("claude-code")
    .description("Autonomous reasoning and code execution");

// VoidBox = Agent(Skills) + Isolation
let researcher = VoidBox::new("hn_researcher")
    .skill(hn_api)
    .skill(reasoning)
    .llm(LlmProvider::ollama("qwen3-coder")) // claude-code runtime using Ollama backend
    .memory_mb(1024)
    .network(true)
    .prompt("Analyze top HN stories for AI engineering trends")
    .build()?;
```

#### Or use a YAML spec

```yaml
# hackernews_agent.yaml
api_version: v1
kind: agent
name: hn_researcher

sandbox:
  mode: auto
  memory_mb: 1024
  network: true

llm:
  provider: ollama
  model: qwen3-coder

agent:
  prompt: "Analyze top HN stories for AI engineering trends"
  skills:
    - "file:skills/hackernews-api.md"
    - "agent:claude-code"
  timeout_secs: 600
```

### 3. Run

```rust
// Rust API
let result = researcher.run(None).await?;
println!("{}", result.claude_result.result_text);
```

```bash
# Or via CLI with a YAML spec
voidbox run --file hackernews_agent.yaml
```

---
## Architecture

```
┌───────────────────────────────────────────────────────┐
│ Host                                                  │
│  VoidBox Engine / Pipeline Orchestrator               │
│                                                       │
│  ┌─────────────────────────────────────────────────┐  │
│  │ OCI Client (~/.voidbox/oci/)                    │  │
│  │  guest image → kernel + initramfs (auto-pull)   │  │
│  │  base image  → rootfs (pivot_root)              │  │
│  │  OCI skills  → read-only mounts                 │  │
│  └─────────────────────┬───────────────────────────┘  │
│                        │                              │
│  ┌─────────────────────▼───────────────────────────┐  │
│  │ VMM (KVM / Virtualization.framework)            │  │
│  │  vsock ←→ guest-agent (PID 1)                   │  │
│  │  SLIRP ←→ eth0 (10.0.2.15)                      │  │
│  │  Linux/KVM: virtio-blk ←→ OCI base rootfs       │  │
│  │  9p/virtiofs ←→ skills + host mounts            │  │
│  └─────────────────────────────────────────────────┘  │
│                                                       │
│  Seccomp-BPF │ OTLP export                            │
└──────────────┼────────────────────────────────────────┘
     Hardware  │  Isolation
═══════════════╪════════════════════════════════════════
               │
┌──────────────▼──────────────────────────────────────────┐
│ Guest VM (Linux)                                        │
│  guest-agent: auth, allowlist, rlimits                  │
│  claude-code runtime (Claude API or Ollama backend)     │
│  OCI rootfs (pivot_root) + skill mounts (/skills/...)   │
└─────────────────────────────────────────────────────────┘
```

See [docs/architecture.md](docs/architecture.md) for the full component diagram, wire protocol, and security model.

## Observability

Every pipeline run is fully instrumented out of the box. Each VM stage emits
spans and metrics via OTLP, giving you end-to-end visibility across isolated
execution boundaries — from pipeline orchestration down to individual tool calls
inside each micro-VM.

<p align="center">
  <img src="site/assets/img/void-box-tracing-5.png" alt="Pipeline trace waterfall in Grafana Tempo" width="800">
</p>

- **OTLP traces** — Per-box spans, tool call events, pipeline-level trace
- **Metrics** — Token counts, cost, duration per stage
- **Structured logs** — `[vm:NAME]` prefixed, trace-correlated
- **Guest telemetry** — procfs metrics (CPU, memory) exported to host via vsock

Enable with `--features opentelemetry` and set `VOIDBOX_OTLP_ENDPOINT`.
See the [playground](playground/) for a ready-to-run stack with Grafana, Tempo, and Prometheus.

## Running & Testing

### KVM mode (zero-setup)

On a Linux host with `/dev/kvm`, VoidBox auto-pulls a pre-built guest image (kernel + initramfs) from GHCR on first run. No manual build steps required:

```bash
# Just works — guest image is pulled and cached automatically
ANTHROPIC_API_KEY=sk-ant-xxx \
cargo run --bin voidbox -- run --file examples/specs/oci/agent.yaml

# Or with Ollama
cargo run --bin voidbox -- run --file examples/specs/oci/workflow.yaml
```

The guest image (`ghcr.io/the-void-ia/voidbox-guest`) contains the kernel and initramfs with guest-agent, busybox, and common tools. It's cached at `~/.voidbox/oci/guest/` after the first pull.

**Resolution order** — VoidBox resolves the kernel/initramfs using:

1. `sandbox.kernel` / `sandbox.initramfs` in the spec (explicit paths)
2. `VOID_BOX_KERNEL` / `VOID_BOX_INITRAMFS` env vars
3. `sandbox.guest_image` in the spec (explicit OCI ref)
4. Default: `ghcr.io/the-void-ia/voidbox-guest:v{version}` (auto-pull)
5. Mock fallback when `mode: auto`

To use a custom guest image or disable auto-pull:

```yaml
sandbox:
  # Use a specific guest image
  guest_image: "ghcr.io/the-void-ia/voidbox-guest:latest"

  # Or disable auto-pull (empty string)
  # guest_image: ""
```

### KVM mode (manual build)

If you prefer to build the guest image locally:

```bash
# Build base guest initramfs (guest-agent + tools; no required Claude bundle)
scripts/build_guest_image.sh

# Download a kernel
scripts/download_kernel.sh

# Run with explicit paths
ANTHROPIC_API_KEY=sk-ant-xxx \
VOID_BOX_KERNEL=target/vmlinuz-amd64 \
VOID_BOX_INITRAMFS=/tmp/void-box-rootfs.cpio.gz \
cargo run --example trading_pipeline
```

For a production Claude-capable initramfs, use:

```bash
# Build production rootfs/initramfs with native claude-code + CA certs + sandbox user
scripts/build_claude_rootfs.sh
```

Script intent summary:

- `scripts/build_guest_image.sh`: base runtime image for general VM/OCI work.
- `scripts/build_claude_rootfs.sh`: production image for direct Claude runtime in guest.
- `scripts/build_test_image.sh`: deterministic test image with `claudio` mock.

### Mock mode (no KVM required)

```bash
cargo run --example quick_demo
cargo run --example trading_pipeline
cargo run --example parallel_pipeline
```

### macOS mode (Apple Silicon)

VoidBox runs natively on Apple Silicon Macs using Apple's Virtualization.framework — no Docker or Linux VM required.

**One-time setup:**

```bash
# Install the musl cross-compilation toolchain (compiles from source, ~30 min first time)
brew install filosottile/musl-cross/musl-cross

# Add the Rust target for Linux ARM64
rustup target add aarch64-unknown-linux-musl
```

**Build and run:**

```bash
# Download an ARM64 Linux kernel (cached in target/)
scripts/download_kernel.sh

# Build the guest initramfs (cross-compiles guest-agent, downloads claude-code + busybox)
scripts/build_claude_rootfs.sh

# Build the example and sign it with the virtualization entitlement
cargo build --example ollama_local
codesign --force --sign - --entitlements voidbox.entitlements target/debug/examples/ollama_local

# Run (Ollama must be listening on 0.0.0.0:11434)
OLLAMA_MODEL=qwen3-coder \
VOID_BOX_KERNEL=target/vmlinux-arm64 \
VOID_BOX_INITRAMFS=target/void-box-rootfs.cpio.gz \
target/debug/examples/ollama_local
```

> **Note:** Every `cargo build` invalidates the code signature. Re-run `codesign` after each rebuild.

When using the `voidbox` CLI, `cargo run` automatically codesigns before executing (via `.cargo/config.toml` runner). Just run:

```bash
cargo run --bin voidbox -- run --file examples/specs/oci/guest-image-workflow.yaml
```

If running the binary directly (e.g. `./target/debug/voidbox`), codesign manually first:

```bash
codesign --force --sign - --entitlements voidbox.entitlements target/debug/voidbox
```

### Parallel pipeline with per-box models

```bash
OLLAMA_MODEL=phi4-mini \
OLLAMA_MODEL_QUANT=qwen3-coder \
OLLAMA_MODEL_SENTIMENT=phi4-mini \
VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
VOID_BOX_INITRAMFS=target/void-box-rootfs.cpio.gz \
cargo run --example parallel_pipeline
```

### Tests

```bash
cargo test --lib                      # Unit tests
cargo test --test skill_pipeline      # Integration tests (mock)
cargo test --test integration         # Integration tests

# E2E (requires KVM + test initramfs)
scripts/build_test_image.sh
VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
VOID_BOX_INITRAMFS=/tmp/void-box-test-rootfs.cpio.gz \
cargo test --test e2e_skill_pipeline -- --ignored --test-threads=1
```

---

## OCI Container Support

VoidBox supports OCI container images in three ways:

1. **`sandbox.guest_image`** — Pre-built kernel + initramfs distributed as an OCI image. Auto-pulled from GHCR on first run (no local build needed). See [KVM mode (zero-setup)](#kvm-mode-zero-setup).
2. **`sandbox.image`** — Use a container image as the base OS for the entire sandbox. The guest-agent performs `pivot_root` at boot, replacing the initramfs root with an overlayfs backed by the OCI image. On Linux/KVM, VoidBox builds a cached ext4 disk artifact from the extracted OCI rootfs and attaches it as `virtio-blk` (`/dev/vda` in guest). On macOS/VZ, the OCI rootfs remains directory-mounted via virtiofs.
3. **OCI skills** — Mount additional container images as read-only tool providers at arbitrary guest paths. This lets you compose language runtimes (Python, Go, Java, etc.) without baking them into the initramfs.

Images are pulled from Docker Hub, GHCR, or any OCI-compliant registry and cached locally at `~/.voidbox/oci/`. OCI base rootfs transport is platform-specific (`virtio-blk` on Linux/KVM, virtiofs directory mount on macOS/VZ), while OCI skills and host mounts use `9p/virtiofs` shares.

### Example: OCI skills

Mount Python, Go, and Java into a single agent — no `sandbox.image` needed:

```yaml
# examples/specs/oci/skills.yaml
api_version: v1
kind: agent
name: multi-tool-agent

sandbox:
  mode: auto
  memory_mb: 2048
  vcpus: 2
  network: true

llm:
  provider: ollama
  model: "qwen2.5-coder:7b"

agent:
  prompt: >
    You have Python, Go, and Java available as mounted skills.
    Set up PATH to include the skill binaries:
      export PATH=/skills/python/usr/local/bin:/skills/go/usr/local/go/bin:/skills/java/bin:$PATH

    Write a "Hello from <language>" one-liner in each language and run all three.
    Report which versions are installed.
  skills:
    - "agent:claude-code"
    - image: "python:3.12-slim"
      mount: "/skills/python"
    - image: "golang:1.23-alpine"
      mount: "/skills/go"
    - image: "eclipse-temurin:21-jdk-alpine"
      mount: "/skills/java"
  timeout_secs: 300
```

Run it:

```bash
# Linux (KVM)
VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
VOID_BOX_INITRAMFS=target/void-box-rootfs.cpio.gz \
cargo run --bin voidbox -- run --file examples/specs/oci/skills.yaml

# macOS (Virtualization.framework) — requires initramfs already built (see "macOS mode" above)
VOID_BOX_KERNEL=target/vmlinux-arm64 \
VOID_BOX_INITRAMFS=target/void-box-rootfs.cpio.gz \
cargo run --bin voidbox -- run --file examples/specs/oci/skills.yaml
```

More OCI examples in [`examples/specs/oci/`](examples/specs/oci/):

| Spec | Description |
|------|-------------|
| `agent.yaml` | Single agent with `sandbox.image: python:3.12-slim` |
| `workflow.yaml` | Workflow with `sandbox.image: alpine:3.20` (no LLM) |
| `pipeline.yaml` | Multi-language pipeline: Python base + Go and Java OCI skills |
| `skills.yaml` | OCI skills only (Python, Go, Java) mounted into default initramfs |
| `guest-image-workflow.yaml` | Workflow using `sandbox.guest_image` for auto-pulled kernel + initramfs (on macOS, codesign required; gzip kernel is auto-decompressed for VZ) |

OpenClaw examples and runbook:

- [`examples/openclaw/README.md`](examples/openclaw/README.md)

## Host Mounts

VoidBox can mount host directories into the guest VM using `sandbox.mounts`. Each mount specifies a `host` path, a `guest` mount point, and a `mode` (`"ro"` or `"rw"`, default `"ro"`).

Read-write mounts write directly to the host directory — data persists across VM restarts since the host directory survives. This is the primary mechanism for stateful workloads.

Transport is platform-specific: **9p** (virtio-9p) on Linux/KVM, **virtiofs** on macOS/VZ.

```yaml
sandbox:
  mounts:
    - host: ./data
      guest: /data
      mode: rw        # persistent — host directory survives VM restarts
    - host: ./config
      guest: /config
      mode: ro        # read-only (default)
```

---

## Roadmap

VoidBox is evolving toward a durable, capability-bound execution platform.

- **Session persistence** — Durable run/session state with pluggable backends (filesystem, SQLite, Valkey).
- **Terminal-native interactive experience** — Panel-based, live-streaming interface powered by the event API.
- **Codex-style backend support** — Optional execution backend for code-first workflows.
- **Language bindings** — Python and Node.js SDKs for daemon-level integration.

## License

Apache-2.0 · [The Void Platform](LICENSE)
