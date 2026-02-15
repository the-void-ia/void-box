# void-box

Composable sandbox runtime for AI agent workflows. Each agent runs in an isolated KVM micro-VM with domain-specific skills, communicates via vsock, and produces structured output for the next stage.

Inspired by [Ed Huang's "Box" concept](https://me.0xffff.me/agent_infra.html): *"A Box exposes no execution details, has no external dependencies, has no side effects, and encapsulates Skill-guided Actions + a reproducible, disposable environment."*

## Core Abstractions

| Abstraction | What it does |
|---|---|
| **`Sandbox`** | Mock or KVM-backed execution environment with vsock I/O |
| **`AgentBox`** | Skill + Prompt + Isolated VM = one autonomous agent unit |
| **`Pipeline`** | Compose boxes sequentially (`.pipe()`) or in parallel (`.fan_out()`) |
| **`Skill`** | Domain knowledge: local files, MCP servers, CLI tools, remote skills |
| **`LlmProvider`** | Per-box LLM backend: Claude API, Ollama, or any compatible endpoint |
| **`Workflow`** | DAG-based step execution with automatic parallelism |
| **`Observer`** | Traces, metrics, and structured logs (OpenTelemetry) |

## Repository Layout

```
src/                    Core runtime (sandbox, vmm, pipeline, workflow, observe)
examples/               Runnable demos
  common/               Shared helpers for examples (make_box, detect_llm_provider, etc.)
  trading_pipeline/     Skills and config for the trading examples
tests/                  Integration and e2e suites
tests/e2e/              KVM-dependent e2e suites (ignored by default)
scripts/                Image builders and helpers
guest-agent/            Rust binary that runs inside the guest VM
void-box-protocol/      Wire-format protocol for host<->guest vsock communication
```

## Quickstart

Minimum supported Rust version: `1.83`.

### 1. Mock mode (no KVM required)

```bash
cargo run --example quick_demo
cargo run --example trading_pipeline
cargo run --example parallel_pipeline
```

### 2. KVM mode

Build the guest initramfs:

```bash
scripts/build_guest_image.sh
```

Run with Claude API:

```bash
ANTHROPIC_API_KEY=sk-ant-xxx \
VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
VOID_BOX_INITRAMFS=/tmp/void-box-rootfs.cpio.gz \
cargo run --example trading_pipeline
```

### 3. KVM mode with Ollama (local LLM)

```bash
ollama pull phi4-mini
OLLAMA_MODEL=phi4-mini \
VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
VOID_BOX_INITRAMFS=/tmp/void-box-rootfs.cpio.gz \
cargo run --example trading_pipeline
```

### 4. Parallel pipeline with different models per box

Each `AgentBox` can use a different Ollama model. The parallel pipeline example supports per-box overrides via environment variables:

```bash
ollama pull phi4-mini && ollama pull qwen2.5-coder:7b

OLLAMA_MODEL=phi4-mini \
OLLAMA_MODEL_QUANT=qwen2.5-coder:7b \
OLLAMA_MODEL_SENTIMENT=phi4-mini \
VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
VOID_BOX_INITRAMFS=/tmp/void-box-rootfs.cpio.gz \
STAGE_TIMEOUT_SECS=600 \
cargo run --example parallel_pipeline
```

## Pipeline Topologies

### Sequential (`pipe`)

```
data_analyst -> quant_analyst -> research_analyst -> portfolio_strategist
```

Each box receives the previous box's output as input. See `examples/trading_pipeline.rs`.

### Parallel fan-out / fan-in (`fan_out`)

```
                    ┌── quant_analyst ──┐
data_analyst ──────>│                   │──> portfolio_strategist
                    └── sentiment_analyst┘
```

Parallel boxes run in separate VMs concurrently. Their outputs are merged as a JSON array for the next stage. See `examples/parallel_pipeline.rs`.

### Streaming output

Pipelines can use `run_streaming()` instead of `run()` to receive real-time output from each agent as it executes:

```rust
Pipeline::named("my_pipeline", first_box)
    .fan_out(vec![box_a, box_b])
    .pipe(final_box)
    .run_streaming(|box_name, chunk| {
        let text = String::from_utf8_lossy(&chunk.data);
        println!("[vm:{}/{}] {}", box_name, chunk.stream, text);
    })
    .await?;
```

All logs are prefixed with `[vm:NAME]` to identify which VM produced each line.

## Examples

| Example | Description |
|---|---|
| `boot_diag` | VM boot diagnostics |
| `quick_demo` | Two-stage analyst/strategist pipeline |
| `trading_pipeline` | Four-stage sequential financial pipeline with local skills |
| `parallel_pipeline` | Diamond topology with `fan_out`, per-box models, streaming output |
| `ollama_local` | Single box configured for Ollama |
| `remote_skills` | Pulls skills from remote repositories |
| `claude_workflow` | Workflow plan/apply pattern in sandbox |
| `claude_in_voidbox_example` | Interactive Claude-style session |
| `playground_pipeline` | Observability-first pipeline demo for Grafana |

See `examples/README.md` for per-example notes.

## E2E Tests

E2E tests require the **test initramfs**, not the runtime one.

```bash
scripts/build_test_image.sh

VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
VOID_BOX_INITRAMFS=/tmp/void-box-test-rootfs.cpio.gz \
cargo test --test e2e_skill_pipeline -- --ignored --test-threads=1

VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
VOID_BOX_INITRAMFS=/tmp/void-box-test-rootfs.cpio.gz \
cargo test --test e2e_telemetry -- --ignored --test-threads=1
```

## Observability Playground

One command boots Grafana LGTM and runs a pipeline that exports traces and metrics over OTLP.

```bash
playground/up.sh
```

The script starts Docker Compose services, runs:

```bash
cargo run --example playground_pipeline --features opentelemetry
```

Then prints direct Grafana Explore links for traces and metrics.
Playground logs are written locally to `/tmp/void-box-playground-last.log`.
It asks for provider mode (`Anthropic`, `Ollama`, `Mock`) and prepares initramfs automatically:
- `Mock` -> `scripts/build_test_image.sh` (claudio mock)
- `Anthropic` / `Ollama` -> `scripts/build_guest_image.sh`

## Development

```bash
cargo test --lib
cargo test --test skill_pipeline
cargo test --test integration
```

## Documentation

- `docs/GETTING_STARTED.md`
- `docs/workflows.md`
- `docs/observability.md`
- `docs/guest_image.md`
- `docs/sandbox_capabilities.md`

## Troubleshooting

### `/dev/kvm` permission denied

Ensure your user can access `/dev/kvm` (often via `kvm` group) and re-login.

### Stage fails with `Not logged in · Please run /login`

The guest-side `claude-code` auth is not configured. Use `OLLAMA_MODEL=...` for local inference or set `ANTHROPIC_API_KEY` for Claude API.

### Parallel stages timeout with Ollama

When running multiple VMs in parallel against a single Ollama instance, model swapping can cause timeouts. Options:
- Use the same model for all parallel boxes to avoid swaps
- Increase timeout: `STAGE_TIMEOUT_SECS=600`
- Run Ollama with enough VRAM to hold multiple models simultaneously
