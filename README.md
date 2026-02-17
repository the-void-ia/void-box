# void-box

**Composable agent runtime with hardware isolation**

> A skill is a declared capability. A capability doesn't exist until it has an isolated boundary.
>
> `VoidBox = Agent(Skills) + Environment`

## Status

v0 -- functional, tested (158+ tests), KVM + Ollama E2E verified. API is unstable.

## Why void-box

- **Hardware isolation** -- KVM micro-VMs, not containers. Each agent runs in a fresh, disposable VM with its own kernel.
- **Skills as first-class artifacts** -- Procedural knowledge (SKILL.md), MCP servers, CLI tools, and reasoning engines are declared and provisioned into the sandbox.
- **Composable pipelines** -- Sequential `.pipe()`, parallel `.fan_out()`, and streaming output. Each stage boots a clean VM.
- **Observability-first** -- OTLP traces and metrics, structured logs with trace correlation, guest telemetry via procfs.

## Quick Start -- Rust Library

```rust
use void_box::agent_box::VoidBox;
use void_box::skill::Skill;
use void_box::llm::LlmProvider;

// Skills = declared capabilities
let hn_api = Skill::file("skills/hackernews-api.md")
    .description("HN API via curl + jq");

let reasoning = Skill::agent("claude-code")
    .description("Autonomous reasoning and code execution");

// VoidBox = Agent(Skills) + Environment
let researcher = VoidBox::new("hn_researcher")
    .skill(hn_api)
    .skill(reasoning)
    .llm(LlmProvider::ollama("qwen3-coder"))
    .memory_mb(1024)
    .network(true)
    .prompt("Analyze top HN stories for AI engineering trends")
    .build()?;

let result = researcher.run(None).await?;
println!("{}", result.claude_result.result_text);
```

## Quick Start -- Spec File

```yaml
api_version: v1
kind: agent
name: hn_researcher
sandbox:
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
```

```bash
voidbox run --file hackernews_agent.yaml
```

## Pipeline Example

```rust
use void_box::pipeline::Pipeline;

// Compose: sequential + parallel stages with streaming output
let result = Pipeline::named("trading_analysis", data_box)
    .pipe(quant_box)                                    // sequential
    .fan_out(vec![sentiment_box, risk_box])              // parallel: both get quant output
    .pipe(strategy_box)                                  // sequential: gets merged results
    .run_streaming(|box_name, chunk| {
        eprint!("[{}] {}", box_name, String::from_utf8_lossy(&chunk.data));
    })
    .await?;

println!("Cost: ${:.4} | Tokens: {}in/{}out",
    result.total_cost_usd(),
    result.total_input_tokens(),
    result.total_output_tokens());
```

## Architecture

```
┌─────────────────────────────────────────────┐
│ Host                                         │
│  VoidBox / Pipeline / Daemon                 │
│  ┌─────────────────────────────────────┐    │
│  │ VMM (KVM)                           │    │
│  │  vsock ←→ guest-agent (PID 1)       │    │
│  │  SLIRP ←→ eth0 (10.0.2.15)         │    │
│  └─────────────────────────────────────┘    │
│  Seccomp-BPF │ OTLP export                  │
└──────────────┼───────────────────────────────┘
     Hardware   │  Isolation
═══════════════╪════════════════════════
               │
┌──────────────▼───────────────────────────────┐
│ Guest VM (Linux)                              │
│  guest-agent: auth, allowlist, rlimits        │
│  claude-code → Ollama / Claude API            │
│  skills provisioned at ~/.claude/skills/      │
└───────────────────────────────────────────────┘
```

See [docs/architecture.md](docs/architecture.md) for the full component diagram, wire protocol, and security model.

## Security Model

- **Session secret** -- 32-byte random token (getrandom), injected via kernel cmdline, required for all vsock messages
- **Seccomp-BPF** -- Restricts VMM thread to minimum syscalls for KVM operation
- **Command allowlist** -- Guest-agent only executes approved binaries
- **Resource limits** -- setrlimit on guest processes (memory, open files, processes, file size)
- **SLIRP rate limiting** -- Max concurrent connections, CIDR deny list
- **No root required** -- Usermode networking via smoltcp, no TAP devices

## LLM Providers

| Provider | Config | Notes |
|---|---|---|
| **Claude** (default) | `ANTHROPIC_API_KEY` | Production quality |
| **Ollama** | `LlmProvider::ollama("model")` | Local, any model. Guest reaches host via SLIRP gateway (10.0.2.2) |
| **Custom** | `LlmProvider::custom(url)` | OpenRouter, vLLM, any OpenAI-compatible endpoint |

## Observability

- **OTLP traces** -- Per-box spans, tool call events, pipeline-level trace
- **Metrics** -- Token counts, cost, duration per stage
- **Structured logs** -- `[vm:NAME]` prefixed, trace-correlated
- **Guest telemetry** -- procfs metrics (CPU, memory) exported to host via vsock

Enable with `--features opentelemetry` and set `VOIDBOX_OTLP_ENDPOINT`.

## Project Structure

```
void-box/              Main crate (VMM, sandbox, pipeline, skills, observability)
  src/
    agent_box.rs       VoidBox: Agent(Skills) + Environment
    pipeline.rs        Sequential + parallel composition
    skill.rs           Skill types (MCP, CLI, file, remote, agent)
    llm.rs             LLM provider configuration
    runtime.rs         Spec file → execution
    vmm/               KVM micro-VM monitor
    sandbox/           Mock + local sandbox abstraction
    observe/           OpenTelemetry integration
    network/           SLIRP usermode networking (smoltcp)
guest-agent/           PID 1 inside guest VMs (vsock, auth, rlimits)
void-box-protocol/     Wire format types (host ↔ guest)
claudio/               Mock claude-code for testing
```

## Running

### Mock mode (no KVM required)

```bash
cargo run --example quick_demo
cargo run --example trading_pipeline
cargo run --example parallel_pipeline
```

### KVM mode

```bash
# Build guest initramfs
scripts/build_guest_image.sh

# Run with Claude API
ANTHROPIC_API_KEY=sk-ant-xxx \
VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
VOID_BOX_INITRAMFS=/tmp/void-box-rootfs.cpio.gz \
cargo run --example trading_pipeline

# Or with Ollama
OLLAMA_MODEL=qwen3-coder \
VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
VOID_BOX_INITRAMFS=/tmp/void-box-rootfs.cpio.gz \
cargo run --example trading_pipeline
```

### Parallel pipeline with per-box models

```bash
OLLAMA_MODEL=phi4-mini \
OLLAMA_MODEL_QUANT=qwen3-coder \
OLLAMA_MODEL_SENTIMENT=phi4-mini \
VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
VOID_BOX_INITRAMFS=/tmp/void-box-rootfs.cpio.gz \
cargo run --example parallel_pipeline
```

## Examples

| Example | Description |
|---|---|
| `quick_demo` | Two-stage analyst/strategist pipeline |
| `trading_pipeline` | Four-stage sequential financial pipeline with local skills |
| `parallel_pipeline` | Diamond topology with `fan_out`, per-box models, streaming |
| `ollama_local` | Single box with local Ollama model |
| `remote_skills` | Pulls skills from skills.sh repositories |
| `claude_workflow` | Workflow plan/apply pattern |
| `claude_in_voidbox_example` | Interactive Claude session in sandbox |
| `boot_diag` | VM boot diagnostics |

## Testing

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

## Troubleshooting

**`/dev/kvm` permission denied** -- Add your user to the `kvm` group and re-login.

**`Not logged in`** -- Use `OLLAMA_MODEL=...` for local inference or set `ANTHROPIC_API_KEY`.

**Parallel stages timeout** -- Use the same Ollama model for all parallel boxes, or increase `STAGE_TIMEOUT_SECS=600`.

## License

Apache-2.0
