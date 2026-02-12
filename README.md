# void-box

> Composable workflow sandbox with KVM micro-VMs and native observability

[![CI](https://github.com/the-void-ia/void-box/workflows/CI/badge.svg)](https://github.com/the-void-ia/void-box/actions?query=workflow%3ACI)
[![Docs](https://img.shields.io/badge/docs-latest-blue.svg)](https://docs.rs/void-box)
[![Rust Version](https://img.shields.io/badge/rust-1.70%2B-blue.svg)](https://www.rust-lang.org)

void-box provides isolated execution environments for AI agents and workflows with first-class observability.

## Features

- 🔒 **Isolated Execution**: KVM micro-VMs or mock sandboxes
- 🔄 **Workflow Composition**: Functional-style workflow DAGs with piping
- 📊 **Native Observability**: OpenTelemetry traces, metrics, and logs
- 🚀 **Fast Boot**: Minimal VMs with < 100ms startup
- 🌐 **Networking**: SLIRP user-mode networking (no root required)
- 🛠️ **Flexible**: Library, CLI, or future REST API

## Quick Start

### Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
void-box = "0.1"
```

Or via cargo:

```bash
cargo add void-box
```

### Basic Usage (Mock Sandbox)

```rust
use void_box::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sandbox = Sandbox::mock().build()?;

    let output = sandbox.exec("echo", &["hello"]).await?;
    println!("{}", output.stdout_str());

    Ok(())
}
```

### Workflow Composition

```rust
use void_box::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let workflow = Workflow::define("data-pipeline")
        .step("fetch", |ctx| async move {
            ctx.exec("curl", &["https://api.example.com/data"]).await
        })
        .step("process", |ctx| async move {
            ctx.exec_piped("jq", &[".results[]"]).await
        })
        .pipe("fetch", "process")
        .build();

    let sandbox = Sandbox::mock().build()?;
    let result = workflow
        .observe(ObserveConfig::test())
        .run_in(sandbox)
        .await?;

    println!("Output: {}", result.result.output_str());
    println!("Traces: {}", result.traces().len());

    Ok(())
}
```

### KVM Mode (Real Isolation)

#### Download Pre-built Artifacts

```bash
# Download from GitHub releases
wget https://github.com/the-void-ia/void-box/releases/download/v0.1.0/void-box-initramfs-v0.1.0-x86_64.cpio.gz

# Run with KVM
VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
VOID_BOX_INITRAMFS=void-box-initramfs-v0.1.0-x86_64.cpio.gz \
cargo run --example claude_workflow
```

Or use the CLI:

```bash
cargo install void-box

# Run commands
voidbox exec echo "hello from KVM"
voidbox workflow plan /workspace
```

## Examples

See [`examples/`](examples/) directory:

- `boot_diag.rs` - Basic VM boot and diagnostics
- `claude_workflow.rs` - Claude-style plan → apply workflow
- `claude_in_voidbox_example.rs` - Full Claude integration demo

Run examples:

```bash
# Mock mode (no KVM)
cargo run --example claude_workflow

# KVM mode (real isolation)
VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
VOID_BOX_INITRAMFS=target/void-box-rootfs.cpio.gz \
cargo run --example claude_in_voidbox_example
```

## Architecture

```
┌─────────────────────────────────────────┐
│  Your Application (Rust/Python/Node)   │
├─────────────────────────────────────────┤
│  void-box Library / REST API            │
├─────────────────────────────────────────┤
│  Sandbox Abstraction                    │
│  ├─ Mock (in-process)                   │
│  └─ Local (KVM micro-VM)                │
├─────────────────────────────────────────┤
│  Observability Layer                    │
│  ├─ Traces (OpenTelemetry)              │
│  ├─ Metrics (counters, gauges)          │
│  └─ Logs (structured)                   │
└─────────────────────────────────────────┘
```

## Comparison

| Feature | void-box | BoxLite | Firecracker | Docker |
|---------|----------|---------|-------------|--------|
| Isolation | KVM VMs | Containers | KVM VMs | Containers |
| Startup | ~100ms | ~50ms | ~125ms | ~1s |
| Observability | Native | Basic | None | Basic |
| Workflows | Built-in | None | None | Compose |
| Language | Rust (+API) | Python/Node/Rust | Any (REST) | Any (CLI) |

## Documentation

- [Getting Started Guide](docs/GETTING_STARTED.md)
- [Examples](examples/)
- [Architecture](docs/alignment.md)

## Development

### Build from Source

```bash
# Clone repository
git clone https://github.com/the-void-ia/void-box
cd void-box

# Run tests
cargo test --workspace

# Build guest image
./scripts/build_guest_image.sh

# Run with KVM
VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
VOID_BOX_INITRAMFS=target/void-box-rootfs.cpio.gz \
cargo run --example boot_diag
```

### Build Release Artifacts

```bash
# Build artifacts for distribution
./scripts/build_release_artifacts.sh v0.1.0 x86_64

# Artifacts will be in target/release-artifacts/v0.1.0/
```

## Contributing

Contributions welcome! Please read our [Contributing Guide](CONTRIBUTING.md) for details on:

- Development setup and workflow
- Code quality standards
- Testing requirements
- Pull request process
- Areas looking for contributions

See also:
- [Changelog](CHANGELOG.md) - Notable changes between versions
- [Issue Templates](.github/ISSUE_TEMPLATE/) - Bug reports and feature requests

## Acknowledgments

Built with [rust-vmm](https://github.com/rust-vmm) components.
