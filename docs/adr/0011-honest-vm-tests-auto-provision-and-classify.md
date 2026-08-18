# ADR-0011: Honest VM tests by auto-provisioning and classifying failure at the boot boundary

- **Status:** Accepted
- **Date:** 2026-08-13
- **Related:** Supersedes ADR-0009; RFC-0004

## Context

ADR-0009 kept a green VM suite honest with an upfront capability probe: KVM, vsock, a kernel, and an initramfs all had to be present, or the suite skipped. Implementing it surfaced two problems the probe design does not handle.

First, "the kernel or initramfs is unset" was one of its skip conditions, and that is the common local state — a developer who has not exported `VOID_BOX_KERNEL` / `VOID_BOX_INITRAMFS`. So the default local run skipped every VM suite, which is the silent-green outcome the ADR set out to remove.

Second, probing capability upfront can be wrong. ADR-0009 assumed a probe cleanly separates "cannot run VMs" from "a capable machine that failed". But the GitHub-hosted macOS runner has no nested virtualization: it is a real machine where the VZ backend genuinely cannot boot a VM, so the VZ suites there reported `12 passed` in 0.03s — a skip the probe laundered into a green. The backend already reports this at `start()`; a separate probe only duplicates it and adds a second thing to keep correct.

## Decision

Provision artifacts, then classify failure at the boot boundary rather than probing capability before it.

**Auto-provision, so an unset artifact is not a skip.** The harness builds the pinned kernel and the test initramfs into `target/` on first run, cached behind a per-checkout `flock` and a source-fingerprint stamp (`tests/common/test_artifacts.rs`). An explicit `VOID_BOX_KERNEL` / `VOID_BOX_INITRAMFS` still wins. A provisioning failure panics with an actionable message; it does not skip.

**Classify the `start()` result, do not probe before it.** `vm_start` classifies `backend.start()`'s error by *type*: a typed `Error::HypervisorUnavailable` yields a loud skip; every other error is a failure. The backends raise that variant only where a hypervisor is genuinely absent — KVM at the `/dev/kvm` open and the required-extension check, VZ at config validation when Virtualization.framework reports the hardware unavailable — so the test harness matches the variant, never a string. `VOID_BOX_REQUIRE_VM=1` turns even a capability absence into a failure, so a CI runner asserted capable cannot launder a lost hypervisor into a skip.

**Type the capability signal; do not match error strings.** `Error::Kvm` is `#[from]`-raised at every KVM ioctl, and on aarch64 the same errnos arise from real boot regressions (`KVM_ARM_VCPU_INIT` returns ENOENT for an unknown feature bit), so matching errno text would skip such a regression — the gate matches the `HypervisorUnavailable` variant by type instead. VZ has the opposite constraint: Apple returns the same `VZError` code for "no hypervisor" as for a bad config, so only the message distinguishes them. The backend reads that one message at the point the error is created and raises the typed variant there, keeping the message check out of the classifier.

**Non-Linux platforms and the CI lane.** The GitHub macOS runner cannot virtualize, so its VZ suites skip loudly (run with `--nocapture` so the reason prints); running real VZ e2e on capable hardware is tracked in #158. `guest-agent` — the Linux-only PID-1 init — builds as an inert stub on non-Linux, so a workspace build needs no `--exclude guest-agent`. The Linux CI VM lane runs through `cargo nextest` with a bounded `vm` test group, so the suites boot a capped number of VMs at once instead of strictly serially.

## Consequences

- **Positive:** a green VM suite means a VM booted. The default local run provisions and runs instead of skipping. A real boot, config, or RPC failure fails on any capable machine, and the classifier cannot read one as a skip — a meta-test (`tests/vm_gate_honesty.rs`) asserts the classification directly, without a VM.
- **Negative / cost:** the first provisioning run pays a build cost (cached after). The typed signal needs a production `Error::HypervisorUnavailable` variant. A genuine capability absence is only visible as a skip with `--nocapture`; on a machine believed capable, set `VOID_BOX_REQUIRE_VM=1` so nothing hides.
- **Follow-ups:** Sandbox/`MicroVm`-based suites boot via value-returning constructors that `vm_start` cannot wrap, so they still panic (not skip) on incapability — a uniform value-op skip is #159. Real VZ e2e on capable macOS hardware is #158.
