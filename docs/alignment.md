# Alignment with BoxLite and VM0

## Compared to BoxLite

- **BoxLite** is a higher-level sandbox API (e.g. `box.run("echo hello")`); you typically don’t own the kernel or guest image.
- **void-box** is lower-level (you supply or build the kernel and rootfs) but adds **workflows** and **observability** as first-class concepts. You run steps in a `Sandbox` (mock or KVM), compose them with `.pipe()`, and get `ObservedResult<WorkflowResult>` with traces, metrics, and logs.

**Parity:** `tests/integration.rs` has `test_boxlite_parity_echo` (and related tests); `tests/kvm_integration.rs` has `kvm_sandbox_echo_parity`, `kvm_sandbox_stdin_pipe`, and `kvm_workflow_pipe_uppercase` so the same patterns work with a real KVM-backed guest.

## Compared to VM0

- **VM0** is a SaaS platform for running agent workloads in VMs.
- **void-box** is not a service; it’s a **building block** in Rust for people who want to build their own VM0-like agent runtime. You get: micro-VM (KVM) + vsock + guest-agent, workflow DAGs, and observability. You integrate Claude (or any CLI) as a workflow step inside the guest (e.g. `ctx.exec("claude-code", &["plan", "/workspace"])`).

**Parity:** The **Claude-in-void** workflow (plan → apply, with mock or real `claude-code` in the guest) is the reference pattern. See `examples/claude_workflow.rs`, `tests/integration.rs` (`test_claude_workflow_plan_apply`), and `tests/kvm_integration.rs` (`kvm_claude_workflow_plan_apply`).
