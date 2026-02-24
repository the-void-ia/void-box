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
- **Observability native** — OTLP traces, metrics, structured logs, and stage-level telemetry emitted by design.
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
┌──────────────────────────────────────────────┐
│ Host                                         │
│  VoidBox Engine / Pipeline Orchestrator      │
│                                              │
│  ┌─────────────────────────────────────┐     │
│  │ VMM (KVM)                           │     │
│  │  vsock ←→ guest-agent (PID 1)       │     │
│  │  SLIRP ←→ eth0 (10.0.2.15)          │     │
│  └─────────────────────────────────────┘     │
│                                              │
│  Seccomp-BPF │ OTLP export                   │
└──────────────┼───────────────────────────────┘
     Hardware  │  Isolation
═══════════════╪════════════════════════════════
               │
┌──────────────▼──────────────────────────────────────┐
│ Guest VM (Linux)                                    │
│  guest-agent: auth, allowlist, rlimits              │
│  claude-code runtime (Claude API or Ollama backend) │
│  skills provisioned into isolated runtime           │
└─────────────────────────────────────────────────────┘
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

### Mock mode (no KVM required)

```bash
cargo run --example quick_demo
cargo run --example trading_pipeline
cargo run --example parallel_pipeline
```

### KVM mode

```bash
# Build guest initramfs (includes claude-code binary, busybox, CA certs)
scripts/build_claude_rootfs.sh

# Run with Claude API
ANTHROPIC_API_KEY=sk-ant-xxx \
VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
VOID_BOX_INITRAMFS=target/void-box-rootfs.cpio.gz \
cargo run --example trading_pipeline

# Or with Ollama
OLLAMA_MODEL=qwen3-coder \
VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
VOID_BOX_INITRAMFS=target/void-box-rootfs.cpio.gz \
cargo run --example trading_pipeline
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
VOID_BOX_KERNEL=target/vmlinuz-arm64 \
VOID_BOX_INITRAMFS=target/void-box-rootfs.cpio.gz \
target/debug/examples/ollama_local
```

> **Note:** Every `cargo build` invalidates the code signature. Re-run `codesign` after each rebuild.

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

## Roadmap

VoidBox is evolving toward a durable, capability-bound execution platform.

- **Session persistence** — Durable run/session state with pluggable backends (filesystem, SQLite, Valkey).
- **Terminal-native interactive experience** — Panel-based, live-streaming interface powered by the event API.
- **Persistent block devices (virtio-blk)** — Stateful workloads across VM restarts.
- **aarch64 support** — Native ARM64 builds with release pipeline cross-compilation.
- **Codex-style backend support** — Optional execution backend for code-first workflows.
- **Language bindings** — Python and Node.js SDKs for daemon-level integration.

## License

Apache-2.0 · [The Void Platform](LICENSE)

