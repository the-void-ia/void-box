# void-box examples

## codebox_example (real code execution)

BoxLite-style example: run commands inside a sandbox and print stdout/stderr/exit code.

- `cargo run --example codebox_example`

**Modes**

| Mode | How | When |
|------|-----|------|
| Mock | No env, or omit `VOID_BOX_KERNEL` | Default. Execution is simulated in-process; always works. |
| KVM | Set `VOID_BOX_KERNEL` (+ optional `VOID_BOX_INITRAMFS`) | Real micro-VM. Requires virtio-vsock to be wired in the VMM (see [docs/sandbox_capabilities.md](../docs/sandbox_capabilities.md)). |

The example does **not** fall back to mock if KVM fails; it exits with an error so the execution mode is explicit.

## claude_in_voidbox_example (Claude in the box)

Rust port of [BoxLite’s claude_in_boxlite_example.py](https://github.com/boxlite-ai/boxlite/blob/main/examples/python/claude_in_boxlite_example.py): run a Claude-style CLI inside the sandbox and interact via plan/apply (void-box uses `claude-code plan` / `claude-code apply`; the guest image includes a mock by default).

- `cargo run --example claude_in_voidbox_example` — then choose demo (automated multi-turn) or interactive (type messages, type `apply` to run apply step).

For real Claude API you would set `ANTHROPIC_API_KEY` in the sandbox env and use a guest image with the real Claude Code CLI.

## claude_workflow

Canonical plan → apply workflow using the mock sandbox (and optional KVM when vsock is available). See [docs/workflows.md](../docs/workflows.md).

- `cargo run --example claude_workflow`
