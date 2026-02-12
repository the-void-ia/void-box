# Onboarding: agents in the void

This doc describes how to see void-box run a Claude-style workflow and inspect observability, without needing a real VM or API.

## 1. Run the example (mock sandbox)

From the repo root:

```bash
cargo run --example claude_workflow
```

This runs the canonical **plan → apply** workflow inside the **mock** sandbox (no KVM, no network). You’ll see:

- Workflow result (success, output, duration)
- Per-step outputs (plan stdout, apply stdout)
- Observability summary (traces, logs)

## 2. Run the integration test

Same workflow, exercised as a test:

```bash
cargo test --test integration test_claude_workflow_plan_apply --no-fail-fast
```

## 3. Optional: VHS demo and GIF

If you have [VHS](https://github.com/charmbracelet/vhs) installed:

```bash
vhs docs/vhs/onboard.tape
```

This records a short terminal session (example + test) and writes **`docs/vhs/onboard.gif`**. Use it for docs or a quick “agents in the void” onboarding clip.

## 4. Real KVM + guest image (opt-in)

To run the same workflow inside a real micro-VM:

1. Build the guest image (includes mock `claude-code` and guest-agent):

   ```bash
   ./scripts/build_guest_image.sh
   ```

2. Set kernel and initramfs (see `scripts/run_kvm_tests.sh` and `docs/guest_image.md`), then:

   ```bash
   cargo test --test kvm_integration kvm_claude_workflow_plan_apply -- --ignored
   ```

See **`docs/workflows.md`** for the workflow shape and **`docs/observability.md`** for what gets captured per run.
