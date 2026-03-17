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
  <a href="https://the-void-ia.github.io/void-box/docs/architecture/">Architecture</a> ·
  <a href="#quick-start">Quick Start</a> ·
  <a href="https://the-void-ia.github.io/void-box/docs/oci-containers/">OCI Support</a> ·
  <a href="https://the-void-ia.github.io/void-box/docs/host-mounts/">Host Mounts</a> ·
  <a href="https://the-void-ia.github.io/void-box/docs/snapshots/">Snapshots</a> ·
  <a href="https://the-void-ia.github.io/void-box/guides/observability-setup/">Observability</a>
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

- **Isolated execution** — Each stage runs inside its own micro-VM boundary, not shared-process containers.
- **Policy-enforced runtime** — Command allowlists, resource limits, seccomp-BPF, and controlled network egress.
- **Skill-native model** — MCP servers, SKILL files, and CLI tools mounted as declared capabilities.
- **Composable pipelines** — Sequential `.pipe()`, parallel `.fan_out()`, with explicit stage-level failure domains.
- **Claude Code native runtime** — Each stage runs `claude-code`, backed by Claude or Ollama via provider mode.
- **OCI-native** — Auto-pulls guest images from GHCR; mount container images as base OS or skill providers.
- **Observability native** — OTLP traces, metrics, structured logs, and stage-level telemetry emitted by design.
- **Persistent host mounts** — Share host directories into guest VMs via 9p/virtiofs with read-only or read-write mode.
- **No root required** — Usermode SLIRP networking via smoltcp (no TAP devices).

> Isolation is the primitive. Pipelines are compositions of bounded execution environments.

## Why Not Containers?

Containers share a host kernel — sufficient for general isolation, but AI agents executing tools, code, and external integrations create shared failure domains. VoidBox binds each agent stage to its own micro-VM boundary, enforced by hardware virtualization rather than advisory process controls. See [Architecture](https://the-void-ia.github.io/void-box/docs/architecture/) ([source](docs/architecture.md)) for the full security model.

---

## Quick Start

```bash
cargo add void-box
```

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
    .llm(LlmProvider::ollama("qwen3-coder"))
    .memory_mb(1024)
    .network(true)
    .prompt("Analyze top HN stories for AI engineering trends")
    .build()?;
```

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

```bash
voidbox run --file hackernews_agent.yaml
```

---

## Documentation

| | |
|---|---|
| **[Architecture](https://the-void-ia.github.io/void-box/docs/architecture/)** | Component diagram, data flow, security model |
| **[Runtime Model](https://the-void-ia.github.io/void-box/docs/runtime/)** | Claude Code runtime, LLM providers, skill types |
| **[CLI + TUI](https://the-void-ia.github.io/void-box/docs/cli-tui/)** | Command reference, daemon API endpoints |
| **[Events + Observability](https://the-void-ia.github.io/void-box/docs/events-observability/)** | Event types, OTLP traces, metrics |
| **[OCI Containers](https://the-void-ia.github.io/void-box/docs/oci-containers/)** | Guest images, base images, OCI skills |
| **[Snapshots](https://the-void-ia.github.io/void-box/docs/snapshots/)** | Sub-second VM restore, snapshot types |
| **[Host Mounts](https://the-void-ia.github.io/void-box/docs/host-mounts/)** | 9p/virtiofs host directory sharing |
| **[Security](https://the-void-ia.github.io/void-box/docs/security/)** | Defense in depth, session auth, seccomp |
| **[Wire Protocol](https://the-void-ia.github.io/void-box/docs/wire-protocol/)** | vsock framing, message types |

### Guides

| | |
|---|---|
| **[Getting Started](https://the-void-ia.github.io/void-box/guides/getting-started/)** | Install, first agent, first run |
| **[Running on Linux](https://the-void-ia.github.io/void-box/guides/running-on-linux/)** | KVM setup, manual build, mock mode, tests |
| **[Running on macOS](https://the-void-ia.github.io/void-box/guides/running-on-macos/)** | Apple Silicon, Virtualization.framework |
| **[Observability Setup](https://the-void-ia.github.io/void-box/guides/observability-setup/)** | OTLP config, Grafana playground |
| **[AI Agent Sandboxing](https://the-void-ia.github.io/void-box/guides/ai-agent-sandboxing/)** | Isolated micro-VM agent execution |
| **[Pipeline Composition](https://the-void-ia.github.io/void-box/guides/pipeline-composition/)** | Multi-stage pipelines with .pipe() and .fan_out() |
| **[YAML Specs](https://the-void-ia.github.io/void-box/guides/yaml-specs/)** | Declarative agent/pipeline definitions |
| **[Local LLMs](https://the-void-ia.github.io/void-box/guides/ollama-local/)** | Ollama integration via SLIRP networking |

---

## Roadmap

VoidBox is evolving toward a durable, capability-bound execution platform.

- **Session persistence** — Durable run/session state with pluggable backends (filesystem, SQLite, Valkey).
- **Terminal-native interactive experience** — Panel-based, live-streaming interface powered by the event API.
- **Codex-style backend support** — Optional execution backend for code-first workflows.
- **Language bindings** — Python and Node.js SDKs for daemon-level integration.

## License

Apache-2.0 · [The Void Platform](LICENSE)
