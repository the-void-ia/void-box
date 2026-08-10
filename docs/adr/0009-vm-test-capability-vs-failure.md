# ADR-0009: Separate capability from failure in the VM test gate

- **Status:** Accepted
- **Date:** 2026-08-07
- **Related:** RFC-0004

## Context

The VM integration suites are the only tests that exercise `src/vmm`, `src/devices`, `src/backend`, and `guest-agent` end to end. They report `ok` when no VM boots. `create_started_backend` (`tests/conformance.rs:113-134`) returns `None` when the backend is unavailable, the kernel or initramfs is unset, or the boot fails, and each test returns early, so the harness records a pass. A real boot failure is then indistinguishable from "this machine cannot run VMs". The checks that cover the most dangerous code can pass without running. One CI step guards against this with `VOID_BOX_DIAGNOSTIC=1`, which panics on a raised boot or RPC error but not on a silent skip; the other suites do not.

## Decision

We will separate capability from failure in the shared VM-test preflight. The harness first detects capability: KVM, vsock, a kernel, and an initramfs must all be present. If any is missing, the test skips and reports the reason. If the machine is capable and the boot or an RPC then fails, the test fails — always, with no flag to set. This logic lives in `tests/common/vm_preflight.rs` and in the per-suite `create_started_backend`, so every suite behaves the same. `VOID_BOX_REQUIRE_VM=1` makes a capable CI runner fail instead of skip when it loses `/dev/kvm`. An artifact and suite-size check is the backstop: a missing initramfs or a sub-second suite means nothing booted.

macOS is a separate case. `tests/conformance.rs:41` reports the VZ backend as always available, and the hosted macOS runner cannot boot nested VZ, so VZ integration runs on a real macOS host, not in CI, and CI treats the VZ suites as advisory.

## Consequences

- **Positive:** a green VM suite means a VM booted. A boot regression turns the suite red instead of green. A test has three honest outcomes — passed, failed, or skipped because the machine cannot run VMs.
- **Negative / cost:** a machine without KVM or vsock now visibly does not run these suites, so the "did not run" state must stay distinct from "failed". The change touches every VM suite. VZ validation on the hosted macOS runner stays advisory, so a VZ-only regression can reach `main` if the local run is skipped and not called out.
- **Follow-ups:** RFC-0004 M0 implements this; M1 wires the CI-absent scenario suites into the gate.
