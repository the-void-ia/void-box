# void-box

Composable sandbox runtime for agent workflows with KVM micro-VMs, vsock command execution, skill provisioning, and observability.

## What It Provides

- `Sandbox`: mock and KVM-backed execution
- `AgentBox`: skill + prompt + isolated runtime unit
- `Pipeline`: multi-stage box composition
- `observe`: traces, metrics, and structured logs

## Repository Layout

- `src/`: core runtime
- `examples/`: runnable demos
- `examples/trading_pipeline/skills/`: local skill files used by trading examples/tests
- `tests/`: integration and e2e suites
- `tests/e2e/`: ignored KVM e2e suites
- `scripts/`: image builders and helpers

## Quickstart

### 1. Mock mode (no KVM required)

```bash
cargo run --example quick_demo
```

### 2. KVM mode with runtime guest image

Build runtime initramfs:

```bash
scripts/build_guest_image.sh
```

Run an example in KVM mode:

```bash
VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
VOID_BOX_INITRAMFS=/tmp/void-box-rootfs.cpio.gz \
cargo run --example quick_demo
```

### 3. KVM mode with Ollama provider

```bash
OLLAMA_MODEL=phi4-mini \
VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
VOID_BOX_INITRAMFS=/tmp/void-box-rootfs.cpio.gz \
cargo run --example trading_pipeline
```

## E2E Tests

E2E tests require the **test initramfs**, not the runtime one.

Build test initramfs:

```bash
scripts/build_test_image.sh
```

Run e2e suites:

```bash
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
Playground logs are also written locally to `/tmp/void-box-playground-last.log` by default.
It also asks for provider mode (`Anthropic`, `Ollama`, `Mock`) and prepares initramfs automatically:
- `Mock` -> `scripts/build_test_image.sh` (`/tmp/void-box-test-rootfs.cpio.gz`, claudio mock)
- `Anthropic` / `Ollama` -> `scripts/build_guest_image.sh` (`/tmp/void-box-rootfs.cpio.gz`)

## Examples

- `boot_diag`: VM boot diagnostics
- `quick_demo`: two-stage analyst/strategist pipeline
- `trading_pipeline`: four-stage financial pipeline with local skills
- `ollama_local`: single box configured for Ollama
- `remote_skills`: pulls skills from remote repositories
- `claude_workflow`: workflow plan/apply pattern in sandbox
- `claude_in_voidbox_example`: interactive Claude-style session
- `playground_pipeline`: observability-first pipeline demo for Grafana

See `examples/README.md` for per-example notes.

## Development

Run core test suites:

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

This indicates the guest-side `claude-code` auth flow is not configured for the selected provider.
Use `OLLAMA_MODEL=...` or configure Claude auth/API key for your guest image.
