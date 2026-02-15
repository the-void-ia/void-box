# Getting Started

This guide covers local development, KVM execution, and e2e testing for `void-box`.

## Prerequisites

- Linux host
- Rust toolchain (minimum: `1.83`)
- `/dev/kvm` access for real VM runs

## Build

```bash
cargo build
```

## Run In Mock Mode

```bash
cargo run --example quick_demo
```

Mock mode is useful for fast iteration and CI where KVM is unavailable.

## Run In KVM Mode

Build runtime guest image:

```bash
scripts/build_guest_image.sh
```

Run:

```bash
VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
VOID_BOX_INITRAMFS=/tmp/void-box-rootfs.cpio.gz \
cargo run --example quick_demo
```

## Run With Ollama

```bash
OLLAMA_MODEL=phi4-mini \
VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
VOID_BOX_INITRAMFS=/tmp/void-box-rootfs.cpio.gz \
cargo run --example trading_pipeline
```

If output shows `[llm] Claude (Anthropic API)`, your `OLLAMA_MODEL` env var did not reach the process.

## E2E Tests

E2E uses a dedicated test image.

Build test image:

```bash
scripts/build_test_image.sh
```

Run ignored e2e suites:

```bash
VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
VOID_BOX_INITRAMFS=/tmp/void-box-test-rootfs.cpio.gz \
cargo test --test e2e_skill_pipeline -- --ignored --test-threads=1

VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
VOID_BOX_INITRAMFS=/tmp/void-box-test-rootfs.cpio.gz \
cargo test --test e2e_telemetry -- --ignored --test-threads=1
```

## Grafana Playground

To bring up Grafana + traces + metrics in one command:

```bash
playground/up.sh
```

This uses the OTLP-enabled example:

```bash
cargo run --example playground_pipeline --features opentelemetry
```

At the end of the run, the script prints:
- direct Trace and Metrics Explore URLs
- local log path (`/tmp/void-box-playground-last.log` by default)

## Core Test Commands

```bash
cargo test --lib
cargo test --test integration
cargo test --test skill_pipeline
cargo test --test kvm_integration -- --ignored
```

## Skills Layout

Local skill fixtures used by examples/tests live at:

- `examples/trading_pipeline/skills/financial-data-analysis.md`
- `examples/trading_pipeline/skills/quant-technical-analysis.md`
- `examples/trading_pipeline/skills/portfolio-risk-management.md`

## Troubleshooting

### `Kvm(Error(13))`

The process cannot access `/dev/kvm`.

### `No space left on device` or `Initramfs unpacking failed`

Ensure you are using the correct image for the suite:

- Runtime examples: `/tmp/void-box-rootfs.cpio.gz`
- E2E tests: `/tmp/void-box-test-rootfs.cpio.gz`
