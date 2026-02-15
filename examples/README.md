# Examples

## boot_diag

Boot diagnostics for VM bring-up.

```bash
cargo run --example boot_diag
```

## quick_demo

Two-stage pipeline (`analyst -> strategist`) with mock or KVM.

```bash
cargo run --example quick_demo
```

## trading_pipeline

Four-stage trading pipeline using local skills under `examples/trading_pipeline/skills/`.

```bash
cargo run --example trading_pipeline
```

With Ollama:

```bash
OLLAMA_MODEL=phi4-mini \
VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
VOID_BOX_INITRAMFS=/tmp/void-box-rootfs.cpio.gz \
cargo run --example trading_pipeline
```

## ollama_local

Single AgentBox configured to use Ollama.

```bash
cargo run --example ollama_local
```

## remote_skills

Fetch and preview remote skills from public repositories.

```bash
cargo run --example remote_skills
```

## claude_workflow

Plan/apply workflow-style example in sandbox.

```bash
cargo run --example claude_workflow
```

## claude_in_voidbox_example

Interactive/demo style Claude-compatible session.

```bash
cargo run --example claude_in_voidbox_example
```
