# ADR-0008: Per-platform virtio device discovery — DTB on aarch64/KVM, cmdline on x86_64/KVM, with a shared slot table

- **Status:** Accepted
- **Date:** 2026-07-14
- **Related:** RFC-0003; ADR-0007

## Context

The guest kernel must be told which virtio-mmio devices exist, where, and on which interrupt. x86_64/KVM does this with `virtio_mmio.device=512@<base>:<irq>` kernel parameters, whose trailing field is a raw Linux IRQ number. On device-tree arm64 systems Linux IRQ numbers are assigned dynamically, so no cmdline value can reliably name a chosen GIC SPI — device interrupts declared that way are undeliverable, which is the #114 boot failure. Separately, the guest-agent keys platform behavior (network setup) off exact cmdline tokens, and device MMIO bases and interrupt numbers were hardcoded as independent literals across four host files, where a drift fails silently.

## Decision

We will discover devices per platform: **aarch64/KVM declares one virtio-mmio DTB node per populated device slot** (with a GIC SPI interrupt specifier, edge-triggered) and emits **no** `virtio_mmio.device=` args — emitting both would register two platform devices over one window; **x86_64/KVM keeps its cmdline convention byte-identical**, pinned by a unit test; macOS/VZ (PCI) is unchanged. `voidbox.network=1`, previously VZ-only, becomes the platform-neutral marker the guest-agent reads for "the host configured a NIC"; the exact `virtio_mmio.device=…:10` token match remains as the x86 legacy form.

All per-device constants derive from a single **slot table** (`src/vmm/arch/mod.rs::VirtioSlot`; slots 0–3 = net, vsock, 9p, blk): the MMIO window base, the `KVM_IRQFD` GSI (raw IOAPIC GSI on x86_64; the SPI index on aarch64, resolved through the vGIC's default identity routing installed at vGIC init — so irqfd registration must follow `setup_vm_post_vcpus`), the `KVM_IRQ_LINE` value (raw GSI on x86_64; the packed irq_type/INTID form from the arm64 uapi, using `kvm-bindings` constants), the ioeventfd doorbell address, the DTB interrupt specifier, and the x86 cmdline numbers. Every injection site — including the net-poll fallback and the vsock-irq thread — goes through one arch-aware helper.

## Consequences

- **Positive:** interrupts are deliverable on aarch64 (the defect this fixes); one source of truth makes base/GSI/doorbell drift structurally impossible where it previously failed silently (a mismatched TX ioeventfd quietly pushed every packet back to the MMIO-exit path); the guest-agent contract is explicit per platform and its x86 surface is bit-for-bit unchanged.
- **Negative / cost:** the DTB, the device construction, and the slot table must stay in sync on which slots are populated (single derivation via `VoidBoxConfig::populated_virtio_slots`); aarch64 guests cannot be booted with cmdline-only device discovery, so non-DTB boot paths are unsupported there; `voidbox.network=1` now means "network present" generically, not "VZ".
- **Follow-ups:** deriving the x86 cmdline strings from the slot table (today they are pinned literals asserted equal by test); extending the slot table if a fifth device class ever ships.
