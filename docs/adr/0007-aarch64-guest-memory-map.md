# ADR-0007: Fix the aarch64/KVM guest-physical memory map to the QEMU `virt` layout

- **Status:** Accepted
- **Date:** 2026-07-14
- **Related:** RFC-0003; ADR-0008

## Context

The aarch64 guest address space had no designed device region: virtio-mmio windows reused the x86 constants (`0xd000_0000`–`0xd180_0000`), which sit inside guest RAM on aarch64 (RAM starts at `0x4000_0000`), and there was no UART at all. The kernel load address assumed the pre-5.8 Image `text_offset` of `0x8_0000`, violating the boot protocol's "2 MB-aligned base + header `text_offset`" rule for modern kernels, and the initramfs sat at a fixed RAM+64 MB that a decompressed distro kernel (~58 MB plus BSS) overruns. Device MMIO, RAM, and boot-artifact placement must come from one internally consistent map, and the DTB must describe exactly that map.

## Decision

We will use the following fixed aarch64 guest-physical layout (`src/vmm/arch/aarch64/kvm.rs::layout`), mirroring the QEMU `virt` machine, whose GIC addresses the code already used:

| Base | Size | Contents |
|---|---|---|
| `0x0800_0000` | 64 KB | GIC distributor (GICv2 uses the first 4 KB) |
| `0x0801_0000` | 8 KB | GICv2 CPU interface (fallback only) |
| `0x080A_0000` | 128 KB × vCPUs | GICv3 redistributors |
| `0x0900_0000` | 4 KB | UART (`ns16550a`) |
| `0x0A00_0000` | 4 KB stride × 4 slots | virtio-mmio slots (0x200-byte windows) |
| `0x4000_0000` | — | RAM: DTB (2 MB slot), then the kernel Image at a 2 MB-aligned base + header `text_offset`, then the initramfs past the kernel's `image_size`, 2 MB-aligned |

The map bounds the vCPU count: the redistributor region must not reach the UART, capping aarch64 at **123 vCPUs** (KVM accepts an overlapping redistributor region silently — it knows nothing about userspace MMIO windows — so config validation enforces the ceiling). The GICv2 fallback is architecturally capped at **8 vCPUs**. The loader inflates gzip-compressed Images (bounded at guest-RAM size; an arm64 Image has no self-decompressor), treats the header fields as untrusted (checked arithmetic, kernel/initramfs/DTB extents inside RAM and mutually disjoint), and length-checks the DTB against its slot.

## Consequences

- **Positive:** every arm64 distro kernel is routinely exercised against this exact layout; addresses in guest logs are directly comparable to QEMU/upstream references; devices can never alias RAM by construction; boot-artifact placement adapts to real kernel sizes instead of silently corrupting.
- **Negative / cost:** aarch64 VMs are capped at 123 vCPUs (8 under GICv2) — acceptable for a micro-VM runtime; ~200–500 ms of gunzip on the host-kernel dev path (release kernels ship uncompressed); the layout is ABI for aarch64 snapshots once those exist.
- **Follow-ups:** a decompressed-kernel cache to remove the gunzip cost from the dev loop (tracked as an issue); aarch64 snapshot support must record and validate against this map.
