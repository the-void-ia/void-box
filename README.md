<h2 align="center">
  <a href="https://voidplatform.ai"><img src="assets/logo/void-box.png" alt="Void-Box" width="400"></a>
  <br><br>
  Hardware-isolated micro-VMs for AI agents — bring any model, run any pipeline, audit every step.
  <br><br>
  <p>
    <a href="https://github.com/the-void-ia/void-box/actions/workflows/ci.yml"><img src="https://github.com/the-void-ia/void-box/actions/workflows/ci.yml/badge.svg?branch=main" alt="CI"></a>
    <a href="https://github.com/the-void-ia/void-box/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue?style=flat-square" alt="License"></a>
    <img src="https://img.shields.io/badge/rust-1.88%2B-orange?style=flat-square&logo=rust" alt="Rust 1.88+">
    <a href="https://voidplatform.ai/docs/"><img src="https://img.shields.io/badge/docs-voidplatform.ai-7c3aed?style=flat-square" alt="Docs"></a>
  </p>
</h2>

<p align="center">
  <b>void-box runs each agent stage inside its own micro-VM — Claude, Codex, Ollama, or any tool you put on PATH — with hardware isolation, OTLP telemetry, and sub-second snapshot/restore.</b>
</p>

<p align="center">
  <a href="https://voidplatform.ai/docs/"><b>Docs</b></a> ·
  <a href="examples/README.md"><b>Examples</b></a> ·
  <a href="https://voidplatform.ai/docs/getting-started/"><b>Getting Started</b></a> ·
  <a href="https://voidplatform.ai/docs/architecture/"><b>Architecture</b></a>
</p>

⭐ **Star us on GitHub — it helps the project a lot!** ⭐

---

<p align="center">
  <img src="assets/hn_demo.gif" alt="hn_demo — two-stage stock analysis pipeline" width="800">
</p>

## What you build with void-box

Real workflows you can run today. Every stage executes inside its own KVM (Linux) or Virtualization.framework (macOS) micro-VM — no shared kernel, no shared blast radius.

### 🔬 Multi-stage research pipelines
- **HackerNews researcher** — autonomous research agent that fetches, ranks, and summarizes top stories. Skills declared as files; one VM per run. ([`examples/hackernews/`](examples/hackernews/))
- **Quant trading pipeline** — four sequential stages (research → analysis → strategy → risk), each in its own VM with its own skill set and resource budget. ([`examples/trading_pipeline.rs`](examples/trading_pipeline.rs))
- **Parallel fan-out** — `.fan_out()` across N isolated VMs, then `.pipe()` results into a reducer stage. ([`examples/parallel_pipeline.rs`](examples/parallel_pipeline.rs))

### 🤖 Code review & PR automation
- **Two-stage review pipeline** — analyzer stage clones the repo and proposes fixes; proposer stage opens a GitHub PR under an explicit command allowlist. The agent can't shell out to anything you didn't pre-declare. ([`examples/code_review/`](examples/code_review/))

### 📡 Long-running agent gateways
- **OpenClaw Telegram bot** — service-mode agent that runs indefinitely, accepts commands over Telegram, and dispatches them to a sandboxed LLM. Swap Claude → Ollama → LM Studio behind one config field. ([`examples/openclaw/openclaw_telegram.yaml`](examples/openclaw/openclaw_telegram.yaml))

### 🏠 Local-first model experimentation
- **Ollama / LM Studio backends** — local models reached through the SLIRP gateway. No API key, no host filesystem access from the guest, no traffic outside the loopback model port. ([`examples/ollama_local.rs`](examples/ollama_local.rs), [`examples/lm_studio_local.rs`](examples/lm_studio_local.rs))

👉 **[Browse all examples →](examples/README.md)**

---

## Why void-box is different

| | |
|---|---|
| 🛡 **Hardware-isolated stages** | KVM (Linux) / Virtualization.framework (macOS) boundary per stage — not shared-process containers, not advisory namespaces. |
| ⚡ **Sub-second snapshot & restore** | Warm restore in ~138 ms, cold in ~252 ms. Fork agents from a snapshot instead of cold-booting per task. |
| 🔌 **Vendor-neutral providers** | Claude, OpenAI Codex, Ollama, LM Studio, OpenRouter, or any Anthropic-compatible endpoint — selected via one config field. |
| 📦 **OCI-native** | Auto-pulls guest images from GHCR; mount container images as base rootfs or as skill providers via overlay. |
| 📊 **OTLP-native observability** | Traces, metrics, structured logs, and stage-level telemetry emitted by design — not bolted on. |
| 🔓 **No root required** | Usermode SLIRP networking via `smoltcp` — no TAP devices, no elevated privileges, no host network reach beyond what you allow. |

---

## Works with the agents and tools you already use

Claude Code · OpenAI Codex · Ollama · LM Studio · OpenRouter · Together AI · any Anthropic-compatible endpoint · MCP servers · OCI base images (GHCR) · OpenTelemetry · Grafana Tempo · Prometheus · 9p / virtiofs host mounts · …and any CLI you can put on PATH.

---

## Your data stays yours

- **Hardware boundary per stage** — KVM/VZ isolation enforced by the CPU, not by the kernel or by process controls.
- **Defense-in-depth** — seccomp-BPF on the VMM thread, command allowlists, session-secret auth on the vsock control channel, uid:1000 privilege drop, and SLIRP NAT isolation.
- **Credentials never persist** — host OAuth tokens are mounted read-only; API keys are injected as session-scoped env vars and never written to disk inside the guest.
- **Fully auditable** — every stage emits OTLP traces, metrics, and structured logs. Nothing in the run is a black box.
- **Open source · self-hostable** — Apache-2.0. This repo. Inspect, fork, run on your own metal.

Read the [Security overview](https://voidplatform.ai/docs/security-model/).

---

## Get started

```bash
curl -fsSL https://raw.githubusercontent.com/the-void-ia/void-box/main/scripts/install.sh | sh
voidbox run --file examples/hackernews/hackernews_agent.yaml
```

Other ways to install:

- **Homebrew (macOS):** `brew install the-void-ia/tap/voidbox`
- **Rust (CLI only):** `cargo install void-box`
- **Debian / Fedora / tarballs:** [voidplatform.ai/docs/installation](https://voidplatform.ai/docs/installation/)

First run, env vars, and provider auth → [Getting Started](https://voidplatform.ai/docs/getting-started/).

---

## Documentation

| | |
|---|---|
| **[Architecture](https://voidplatform.ai/docs/architecture/)** | Component diagram, data flow, security model |
| **[Runtime Model](https://voidplatform.ai/docs/runtime-model/)** | LLM providers, skill types, agent binaries |
| **[CLI + TUI](https://voidplatform.ai/docs/cli-tui/)** | Command reference, daemon API |
| **[YAML Specs](https://voidplatform.ai/docs/yaml-specs/)** | Declarative agent and pipeline definitions |
| **[Pipeline Composition](https://voidplatform.ai/docs/pipeline-composition/)** | `.pipe()`, `.fan_out()`, failure domains |
| **[OCI Containers](https://voidplatform.ai/docs/oci-containers/)** | Guest images, base images, OCI skills |
| **[Snapshots](https://voidplatform.ai/docs/snapshots/)** | Sub-second VM restore, snapshot types |
| **[Host Mounts](https://voidplatform.ai/docs/host-mounts/)** | 9p / virtiofs host directory sharing |
| **[Events + Observability](https://voidplatform.ai/docs/events-observability/)** | OTLP traces, metrics, event types |
| **[Security Model](https://voidplatform.ai/docs/security-model/)** | Defense-in-depth, seccomp, session auth |
| **[Wire Protocol](https://voidplatform.ai/docs/wire-protocol/)** | vsock framing, message types |

Platform setup: [Linux](https://voidplatform.ai/docs/running-on-linux/) · [macOS](https://voidplatform.ai/docs/running-on-macos/) · [Local LLMs](https://voidplatform.ai/docs/ollama-local/) · [Observability stack](https://voidplatform.ai/docs/observability-setup/)

---

## Roadmap

> **Where we're headed.** Current focus is hardening the security boundary and squeezing more out of the snapshot/restore path. We'll be sharing the work as it lands — follow along on **[voidplatform.ai/updates](https://voidplatform.ai/updates/)**.

Up next, after the security and performance push:

- **Session persistence** — Durable run/session state with pluggable backends (filesystem, SQLite, Valkey).
- **Terminal-native interactive experience** — Panel-based, live-streaming TUI powered by the event API.
- **Language bindings** — Python and Node.js SDKs for daemon-level integration.

## License

Apache-2.0 · [The Void Platform](LICENSE)
