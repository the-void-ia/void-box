# Getting Started with void-box

This guide will help you get started with void-box, from installation to running your first workflow.

## Choose Your Path

There are three main ways to use void-box, depending on your needs:

### Path 1: Mock Sandbox (Quickest)

**Best for:** Testing, development, CI/CD pipelines where isolation isn't critical

No KVM required, works everywhere including macOS and Windows:

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

**Pros:**
- Instant setup, no dependencies
- Works on any platform
- Perfect for testing workflow logic

**Cons:**
- No actual isolation
- Limited to simulated commands

### Path 2: KVM Sandbox (Real Isolation)

**Best for:** Production use, security-critical workloads, running untrusted code

Requires Linux + KVM + pre-built artifacts:

```bash
# 1. Download pre-built artifacts from GitHub releases
wget https://github.com/the-void-ia/void-box/releases/download/v0.1.0/void-box-initramfs-v0.1.0-x86_64.cpio.gz

# 2. Set environment variables
export VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r)
export VOID_BOX_INITRAMFS=void-box-initramfs-v0.1.0-x86_64.cpio.gz

# 3. Run your application
cargo run --example claude_workflow
```

Or in your code:

```rust
use void_box::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sandbox = Sandbox::local()
        .from_env()? // Load kernel/initramfs from environment
        .memory_mb(512)
        .vcpus(2)
        .network(true)
        .build()?;

    let output = sandbox.exec("echo", &["hello from KVM"]).await?;
    println!("{}", output.stdout_str());

    Ok(())
}
```

**Pros:**
- Real KVM-based isolation
- Secure execution of untrusted code
- Full Linux environment

**Cons:**
- Requires Linux with KVM support
- Needs pre-built artifacts

### Path 3: CLI Tool

**Best for:** Quick testing, command-line workflows, scripting

Install and use the command-line interface:

```bash
# Install
cargo install void-box

# Run commands
voidbox exec echo "hello"
voidbox exec ls -la

# Run workflows
voidbox workflow plan /workspace
voidbox workflow apply /workspace
```

With KVM mode:

```bash
VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
VOID_BOX_INITRAMFS=void-box-initramfs-v0.1.0-x86_64.cpio.gz \
voidbox exec echo "hello from KVM"
```

## Building Pre-built Artifacts Locally

If you prefer to build artifacts yourself instead of downloading from releases:

```bash
# Clone the repository
git clone https://github.com/the-void-ia/void-box
cd void-box

# Build the guest image
./scripts/build_guest_image.sh

# The artifacts will be in /tmp/
# - Kernel: /boot/vmlinuz-$(uname -r) (use host kernel)
# - Initramfs: /tmp/void-box-rootfs.cpio.gz
```

Then use them:

```bash
VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
VOID_BOX_INITRAMFS=/tmp/void-box-rootfs.cpio.gz \
cargo run --example boot_diag
```

## Common Use Cases

### 1. Simple Command Execution

```rust
use void_box::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sandbox = Sandbox::mock().build()?;

    // Run a command
    let output = sandbox.exec("echo", &["Hello, void-box!"]).await?;
    println!("Output: {}", output.stdout_str());

    // Check exit code
    if output.success() {
        println!("Command succeeded!");
    }

    Ok(())
}
```

### 2. Workflow with Multiple Steps

```rust
use void_box::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Define a workflow
    let workflow = Workflow::define("example")
        .step("step1", |ctx| async move {
            ctx.exec("echo", &["first"]).await
        })
        .step("step2", |ctx| async move {
            ctx.exec("echo", &["second"]).await
        })
        .pipe("step1", "step2") // Pipe output from step1 to step2
        .build();

    // Run in sandbox
    let sandbox = Sandbox::mock().build()?;
    let result = workflow
        .observe(ObserveConfig::test())
        .run_in(sandbox)
        .await?;

    println!("Final output: {}", result.result.output_str());

    Ok(())
}
```

### 3. With Observability

```rust
use void_box::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let workflow = Workflow::define("observed-workflow")
        .step("fetch", |ctx| async move {
            ctx.exec("echo", &["data"]).await
        })
        .build();

    let sandbox = Sandbox::mock().build()?;

    // Enable observability
    let observe_config = ObserveConfig::test()
        .with_traces(true)
        .with_metrics(true);

    let result = workflow
        .observe(observe_config)
        .run_in(sandbox)
        .await?;

    // Access observability data
    println!("Traces collected: {}", result.traces().len());
    println!("Metrics: {:?}", result.metrics());

    Ok(())
}
```

## Next Steps

- **Explore Examples**: Check out the [`examples/`](../examples/) directory for more complex use cases
- **Read the Architecture**: Understand how void-box works in [alignment.md](alignment.md)
- **API Documentation**: View the full API docs with `cargo doc --open`
- **Join the Community**: Star the repo and open issues for questions or feature requests

## Troubleshooting

### "Permission denied" when accessing /dev/kvm

Make sure your user is in the `kvm` group:

```bash
sudo usermod -aG kvm $USER
# Log out and log back in
```

### "Cannot find kernel"

Make sure the kernel exists at the specified path:

```bash
ls -la /boot/vmlinuz-$(uname -r)
```

If not found, try:

```bash
# Find your kernel
ls /boot/vmlinuz-*

# Use the correct path
export VOID_BOX_KERNEL=/boot/vmlinuz-6.x.x-xxx
```

### Mock sandbox not behaving as expected

Remember that the mock sandbox simulates command execution. For actual isolation and real command execution, use KVM mode.

## Support

- **Issues**: [GitHub Issues](https://github.com/the-void-ia/void-box/issues)
- **Discussions**: [GitHub Discussions](https://github.com/the-void-ia/void-box/discussions)
