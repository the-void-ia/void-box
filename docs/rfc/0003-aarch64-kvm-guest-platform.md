# RFC-0003: aarch64/KVM guest platform — memory map, DTB device discovery, and IRQ model

- **Status:** Accepted
- **Authors:** Cristian Spinetta
- **Created:** 2026-07-12
- **Discussion:** #114
- **Related ADRs:** ADR-0007 (aarch64 guest memory map), ADR-0008 (per-platform device discovery)

## Summary

This RFC completes aarch64/KVM guest bring-up (#114) by pinning two cross-cutting decisions: (a) a fixed aarch64 guest-physical memory map that places all device MMIO below guest RAM, mirroring the QEMU `virt` machine layout the existing GIC addresses already follow, and (b) a device-discovery convention where aarch64/KVM guests learn about devices from generated device-tree (DTB) nodes, while x86_64/KVM keeps its `virtio_mmio.device=` kernel-cmdline convention byte-for-byte and macOS/VZ is untouched. It also fixes three defects found while validating the plan: the aarch64 kernel loader cannot boot the compressed `Image` files that Linux distros ship as `/boot/vmlinuz`, IRQ injection uses raw x86 GSI numbers that the arm64 `KVM_IRQ_LINE` ABI misinterprets, and the GICv3→GICv2 fallback can attempt GICv2 after a partially-created GICv3 (a deferred finding from the #112/#113 review).

## Motivation / problem

With #112 and #113 fixed (PR #115), aarch64/KVM vCPUs are created, configured, and run — and then nothing happens: the boot stalls silently and the control channel times out. Issue #114 inventories the gaps: the generated DTB has no GIC, `/cpus`, UART, or virtio-mmio nodes, so the guest kernel cannot route its timer interrupt, bring up CPUs, print anywhere, or find devices; IRQ injection passes raw x86 IRQ numbers to `KVM_IRQ_LINE`, which on arm64 selects the wrong injection type; and the virtio MMIO windows (`0xd000_0000`–`0xd180_0000`) sit inside guest RAM on the aarch64 layout (RAM starts at `0x4000_0000`).

Validating this RFC's plan on the target environment (Ubuntu 6.8 arm64, KVM) surfaced one more blocker upstream of all of the above: `/boot/vmlinuz-6.8.0-90-generic` is a gzip-compressed arm64 `Image` (58 MB uncompressed). Unlike x86 `bzImage`, an arm64 `Image` has no self-decompressor; the current loader writes the raw gzip bytes into guest memory and points PC at them, so the guest kernel never executes at all. `AGENTS.md` already documents "vmlinuz (compressed OK)" for Linux — the loader must actually honor that. The decompressed size also overruns the fixed initramfs slot at RAM+64 MB, so initramfs placement must be computed from the loaded kernel's real footprint rather than a constant.

## Detailed design

### New components

- A **device slot table**: a fixed, arch-independent assignment of virtio-mmio devices to slot indexes (0 = net, 1 = vsock, 2 = 9p, 3 = blk), defined once in the VMM `arch` module. Each arch maps a slot to an MMIO base and an interrupt number. Today those mappings are hardcoded literals scattered across `src/vmm/mod.rs`, `src/vmm/config.rs`, `src/backend/kvm.rs`, and `src/bin/voidbox/snapshot.rs`; the slot table becomes the single source for all of them.
- An **aarch64 FDT builder**: an extension of `generate_dtb` (`src/vmm/arch/aarch64/boot.rs`) that emits GIC, `/cpus`, UART, and per-slot virtio-mmio nodes in addition to the existing memory/chosen/psci/timer nodes.
- A **GIC version probe**: a pure function that asks KVM whether GICv3 is available via `KVM_CREATE_DEVICE` with the `KVM_CREATE_DEVICE_TEST` flag (which tests support without creating a device and is legal before vCPUs exist). Both DTB generation and vGIC creation call it, so the DTB and the creation path always agree on which GIC version to attempt. The flag tests that the device *type* is supported, not that creation in this particular VM will succeed (Documentation/virt/kvm/api.rst); the failure handling for that residual gap is defined in the GIC-creation section below.

### (a) aarch64 guest-physical memory map

All device MMIO sits below RAM. The GIC addresses are unchanged from the current code; the UART and virtio region are new and chosen to match QEMU `virt`, which the GIC addresses already mirror — a layout every arm64 Linux kernel is exercised against.

| Base | Size | Contents |
|---|---|---|
| `0x0800_0000` | 64 KB | GIC distributor (GICv2 uses the first 4 KB) |
| `0x0801_0000` | 8 KB | GICv2 CPU interface (GICv2 fallback only) |
| `0x080A_0000` | 128 KB × vCPUs | GICv3 redistributors |
| `0x0900_0000` | 4 KB | UART (`ns16550a`, byte registers, `reg-shift = 0`) |
| `0x0A00_0000` | 4 KB stride × 4 slots | virtio-mmio slots (each device window is 0x200 bytes) |
| `0x4000_0000` | — | RAM start |
| RAM + 0 | ≤ 2 MB | DTB |
| RAM + 2 MB | kernel size | kernel `Image` load base (2 MB aligned) + `text_offset` from the Image header |
| after kernel | initramfs size | initramfs, placed past the kernel's runtime footprint (`image_size` header field, which includes BSS), 2 MB aligned |

The redistributor region bounds the vCPU count: the gap between `0x080A_0000` and the UART at `0x0900_0000` holds exactly 123 redistributor frames of 128 KB, and KVM silently accepts a redistributor region that overruns userspace MMIO windows (the vGIC knows nothing about them), so aarch64 config validation rejects more than 123 vCPUs instead of letting redistributors alias the UART and virtio windows. The GICv2 fallback is stricter still: the architecture (and KVM's vGICv2, which fails vGIC creation with `E2BIG`) caps it at 8 vCPUs, and the VMM turns that combination into a clear pre-flight error. Both ceilings are recorded in the memory-map ADR. The x86_64 limit (256) is unchanged.

x86_64 keeps its current layout (RAM at 0, virtio windows in the `0xd000_0000` MMIO gap, IRQs 10–13) unchanged.

The kernel loader gains three fixes required by this map. It detects gzip magic bytes and decompresses the `Image` before loading (distro `/boot/vmlinuz` on arm64 is gzip-compressed and has no self-decompressor); decompression is bounded — it streams through a reader capped at the guest's RAM size and errors past the cap, so a malformed or hostile file cannot balloon host memory before any later size check — and the output lands directly in the guest-RAM slice rather than a transient host buffer. It reads `text_offset` and `image_size` from the arm64 Image header instead of assuming the pre-5.8 `0x8_0000` text offset — the current `0x4008_0000` constant violates the boot protocol's "2 MB aligned base + text_offset" rule for modern kernels whose `text_offset` is 0; the header fields are untrusted input, so placement arithmetic is checked (no wrap-around) and the computed kernel, initramfs, and DTB extents must each lie inside guest RAM and be mutually disjoint, mirroring the placement-window checks the x86 loader already performs. And it places the initramfs after the loaded kernel's `image_size` (which includes BSS) instead of at a fixed RAM+64 MB, erroring out (rather than silently corrupting) if kernel + initramfs exceed guest memory. The generated DTB is length-checked against its 2 MB slot before being written — it now grows per-vCPU and per-device nodes, and `bootargs`, today copied unbounded, gets the same 4096-byte cap x86 enforces on its cmdline.

### DTB contents

`generate_dtb` grows the following nodes; all `interrupts` specifiers become resolvable because the root node gains `interrupt-parent` pointing at the GIC's phandle.

- **GIC** — for GICv3: `compatible = "arm,gic-v3"`, `reg` = distributor + redistributor region sized `vcpus × 128 KB`, `interrupt-controller`, `#interrupt-cells = <3>`. For the GICv2 fallback: `compatible = "arm,gic-400"`, `reg` = distributor (4 KB) + CPU interface (8 KB). Which variant is emitted follows the GIC version probe.
- **`/cpus`** — one `cpu@N` node per vCPU with `device_type = "cpu"`, `compatible = "arm,armv8"` (the string the CPU binding documents; QEMU and Firecracker ship an undocumented `arm,arm-v8` variant, but the kernel identifies CPUs by MIDR and matches on neither, so the documented spelling costs nothing), `enable-method = "psci"`, and `reg` set to the MPIDR affinity KVM assigns (Aff0 = id & 0xf, Aff1 = (id >> 4) & 0xff — the kernel's documented vcpu-id mapping). PSCI-based bring-up additionally requires each vCPU to be initialized with the `KVM_ARM_VCPU_PSCI_0_2` feature flag in `configure_vcpu`; without it KVM exposes the legacy PSCI 0.1 interface and the DTB's `arm,psci-1.0` contract is a lie — secondary CPUs and guest-initiated shutdown would break.
- **timer** — the existing node, with the third interrupt cell corrected per GIC version: the GICv2 binding encodes a CPU mask in bits 15:8 (one bit per CPU, at most the 8 CPUs GICv2 can architecturally address — exact under the GICv2 vCPU ceiling above), while the GICv3 binding requires those bits to be zero (the current hardcoded `0xf08` is a GICv2-with-4-CPUs value).
- **UART** — `serial@9000000` with `compatible = "ns16550a"`, `clock-frequency = <1843200>` (required by the 8250 OF binding; the value only feeds baud-divisor math our emulation ignores), and a level-triggered SPI 1 interrupt specifier. `/chosen` gains `stdout-path = "/serial@9000000"` so a bare `earlycon` works for early-boot debugging.
- **virtio-mmio** — one `virtio_mmio@...` node per populated slot with `compatible = "virtio,mmio"`, `reg` from the slot table, and an edge-triggered SPI interrupt specifier. Only configured devices get nodes, matching the conditional cmdline args on x86.

The UART reuses the existing 16550 `SerialDevice` register model: the target kernels build the 8250 driver with device-tree probing in (`CONFIG_SERIAL_8250=y`, `CONFIG_SERIAL_OF_PLATFORM=y`, verified on Ubuntu 6.8 arm64), the device enumerates as `ttyS0` so the existing `console=ttyS0` cmdline works unchanged on both arches, and the vCPU loop only needs an address-range dispatch in its existing `MmioRead`/`MmioWrite` arms (aarch64 has no port I/O); the dispatch bounds-checks the offset against the 8-register file and ignores the rest of the 4 KB window, rather than truncating the offset into an aliased register index. The host never injects the UART interrupt — kernel console writes poll the line-status register, which matches the x86 status quo where no serial IRQ is injected either; the serial console is output-only on both arches.

### (b) Device discovery: DTB on aarch64/KVM, cmdline on x86_64/KVM

The kernel discovers devices differently per platform, and the guest-agent needs a platform-neutral signal for whether networking is configured:

| Concern | Linux x86_64 (KVM) | Linux aarch64 (KVM) | macOS (VZ) |
|---|---|---|---|
| Kernel device discovery | `virtio_mmio.device=512@0xd0000000:10` … cmdline args (unchanged, byte-identical) | virtio-mmio DTB nodes | PCI |
| guest-agent network detection | `virtio_mmio.device=512@0xd0000000:10` token (unchanged) | `voidbox.network=1` | `voidbox.network=1` (unchanged) |

On aarch64/KVM the host emits **no** `virtio_mmio.device=` args — emitting both DTB nodes and cmdline args would register two platform devices over the same window. Instead, when networking is enabled the host adds `voidbox.network=1`, the marker VZ already uses and the guest-agent already matches; `voidbox.network=1` thereby becomes the generic "host configured a NIC" signal rather than a VZ-ism, and the exact `virtio_mmio.device=…:10` match stays as the x86 legacy form. The guest-agent's other cmdline contracts (`voidbox.mount<N>=`, `voidbox.secret=`, OCI params) are already platform-neutral and unchanged.

Two guest-agent adjustments, both compile-time arch-gated so x86 images are bit-for-bit unaffected: the hardcoded x86 fallback device list in `virtio_mmio_params_from_cmdline()` (used as module parameters when `virtio_mmio.ko` loads with no cmdline args) becomes x86_64-only — on aarch64 the devices come from the DTB and the fallback would register bogus windows; and nothing else — device-tree platform devices bind to `virtio_mmio` whether it is built in (`=y` on Ubuntu arm64) or loaded later as a module.

The x86 kernel cmdline also carries x86-only hardware quirks (`i8042.*`, `reboot=k`, `pci=off`, `initcall_blacklist=cmos_init,i8042_init`); these are gated to x86_64 rather than emitted as noise on aarch64. The x86_64 cmdline string remains byte-identical.

### IRQ model

Interrupt numbers join the slot table: each arch maps a slot to its native interrupt identity, and every injection site goes through one helper instead of hardcoding numbers.

| Slot | Device | x86_64 GSI | aarch64 GIC SPI (INTID) |
|---|---|---|---|
| — | UART | — (no serial IRQ injected) | SPI 1 (33), declared in DTB only |
| 0 | virtio-net | 10 | SPI 16 (48) |
| 1 | virtio-vsock | 11 | SPI 17 (49) |
| 2 | virtio-9p | 12 | SPI 18 (50) |
| 3 | virtio-blk | 13 | SPI 19 (51) |

`inject_irq` (`src/vmm/cpu.rs`) becomes arch-aware: on x86_64 the `KVM_IRQ_LINE` `irq` field is the raw GSI as today; on aarch64 it is packed as `(KVM_ARM_IRQ_TYPE_SPI << KVM_ARM_IRQ_TYPE_SHIFT) | intid` per the arm64 uapi (`irq_type` occupies bits 27:24 — `KVM_ARM_IRQ_TYPE_SHIFT` is 24, with bits 31:28 being `vcpu2_index`; the shift constants come from `kvm-bindings`, so the encoding cannot drift). `KVM_IRQFD` (the fast path in `net_poll_thread`) keeps working on arm64 through the vGIC's default GSI routing: when the vGIC initializes, KVM installs an identity routing table (`kvm_vgic_setup_default_irq_routing` — gsi *n* → irqchip pin *n*, delivered as INTID *n* + 32), so the `gsi` argument is the SPI index. That routing exists only once the vGIC is initialized, which makes ordering a stated invariant: irqfd registration must happen after `setup_vm_post_vcpus` — an irqfd registered earlier would report success and then never deliver. The current thread-spawn order satisfies this, and milestone 3 verifies delivery empirically rather than trusting `KVM_IRQFD` returning `Ok`. The same applies to the TX-notify `KVM_IOEVENTFD`, whose hardcoded `0xd000_0000` base moves to the slot table. The DTB declares virtio interrupts edge-triggered (rising), which both the assert/deassert `KVM_IRQ_LINE` pulse and an irqfd write deliver correctly.

Three constants per device must agree — the MMIO base, the irqfd `gsi`, and the ioeventfd datamatch address (base + the queue-notify offset `0x050`) — and today they are set from independent hardcoded sites; drift between them fails silently (a mismatched TX ioeventfd simply stops matching and every TX packet quietly returns to the slow MMIO-exit path). All three therefore read from the slot table, the arch split in `inject_irq` is compile-time (`#[cfg(target_arch)]`), the hand-rolled `KVM_IRQ_LINE` fallback inside `net_poll_thread` — a separate injection site from `inject_irq`, carrying the raw x86 encoding today — is routed through the same arch-aware helper, and a unit test pins the x86 slot-0 tuple (base `0xd000_0000`, gsi 10, notify `0xd000_0050`, TX queue index 1) so the byte-identical-x86 claim is checked, not asserted.

The vCPU run loop additionally handles `VcpuExit::SystemEvent`: on arm64, guest shutdown and reboot arrive as PSCI `SYSTEM_OFF`/`SYSTEM_RESET` system events rather than the x86 port-0x64 write, and an unhandled system event would spin the vCPU loop on a dead guest.

### GIC creation: probe first, no post-creation fallback

`setup_vm_post_vcpus` currently tries the full GICv3 sequence (create device → set addresses → `CTRL_INIT`) and falls back to GICv2 on any error. KVM allows one vGIC per VM, so if creation succeeds and a later step fails, the GICv2 attempt is doomed — the deferred finding from the #112/#113 review. With the version probe, the sequence becomes: probe GICv3 support (`KVM_CREATE_DEVICE_TEST`) → create exactly the probed version → any subsequent failure is a hard error, never a fallback. The probe is also what the DTB generator consults, so the emitted GIC node always names the version the VMM attempts. The probe is not a guarantee — it checks that the device type is supported, not that creation in this VM will succeed — so a host where the probe passes and creation still fails aborts the boot with a clear error instead of booting against a DTB that describes a GIC that does not exist. That trades a hypothetical recovery (falling back to GICv2 on a host whose GICv3 breaks only at creation time) for the invariant that DTB and device never diverge.

### Snapshot/restore boundary (out of scope, made explicit)

aarch64 snapshot/restore remains stubbed and is not extended here; a separate issue will track its known gaps (GIC state capture/restore are stubs; `prepare_vcpu_restored` never calls `KVM_ARM_VCPU_INIT` on aarch64; and the snapshot header carries no architecture discriminator, so a cross-arch restore is rejected only by deserialization luck — it needs an explicit arch field once two architectures can produce snapshots). Two touch-points land now because this work owns the adjacent lines: the arch-neutral restore path in `src/vmm/mod.rs` calls `restore_irqchip` before vCPUs exist, which is fine for the x86 in-kernel irqchip but wrong for a future aarch64 GIC restore (the vGIC only exists after `setup_vm_post_vcpus`) — the call site gets a comment pinning that ordering constraint with a reference to the tracking issue; and the `vsock_mmio_base` literals (`0xd080_0000`) recorded in snapshot configs move to the slot table so snapshots taken on each arch record that arch's real base.

## Alternatives considered

**PL011 UART instead of 16550.** PL011 is the canonical arm64 console (QEMU `virt` uses it) but requires a new ~150-line register model, changes the console name to `ttyAMA0` (forking the cmdline per arch), and buys nothing: the 8250 OF binding is equally first-class in distro kernels (`CONFIG_SERIAL_OF_PLATFORM=y`) and the 16550 model already exists and is tested. Rejected; PL011 remains the fallback if a target kernel ever lacks 8250 OF support.

**Custom compact memory map instead of mirroring QEMU `virt`.** Any region below `0x4000_0000` works. Mirroring `virt` was chosen because the GIC constants already match it, every arm64 kernel is routinely booted on that exact layout, and identical addresses make kernel logs and upstream references directly comparable during debugging. No technical cost.

**Keep `virtio_mmio.device=` cmdline on aarch64 with corrected addresses.** The parameter's trailing IRQ field is a raw Linux IRQ number; on device-tree arm64 systems Linux IRQ numbers are dynamically assigned, so there is no number the host could write that reliably maps to a chosen GIC SPI. Device interrupts would be undeliverable — this is precisely the #114 defect. Cmdline discovery on arm64 is a dead end, not a style choice.

**Generate the DTB after vGIC creation instead of probing first.** Restructuring boot so the DTB is written after `setup_vm_post_vcpus` (reading real MPIDRs and the created GIC's type) is the Firecracker structure and describes reality by construction. It was set aside because it reshapes the `Arch` trait and the `MicroVm::new` sequencing for no behavioral difference: the probe is deterministic, the MPIDR formula is kernel ABI, and a probe/create divergence is impossible when both consult the same function. If a future KVM breaks probe determinism, this alternative is the escape hatch.

**Emit `voidbox.network=1` on all platforms including x86_64.** Harmonizes the marker, but changes the x86 cmdline for zero functional gain and the x86/KVM lane is under a strict no-regression constraint here. Deferred as a possible follow-up.

## Risks & trade-offs

- **Host↔guest contract drift.** The guest-agent's network detection and module-param fallback are keyed to exact cmdline strings. Mitigation: x86_64 emissions stay byte-identical (asserted by existing unit tests plus a new cmdline test), guest-agent changes are compile-time arch-gated, and aarch64 had no working boots to regress.
- **MPIDR formula coupling.** `/cpus` `reg` values are computed from KVM's documented vcpu-id→MPIDR mapping rather than read back from created vCPUs (which don't exist yet at DTB time). The mapping is guest-visible ABI and stable; if it ever changes, secondary CPU bring-up fails loudly. The DTB-after-vGIC alternative is the recorded escape hatch.
- **Snapshot format.** `SnapshotConfig.vsock_mmio_base` starts recording `0x0a00_1000` on aarch64. x86 snapshots are unaffected; aarch64 snapshots don't work yet, so nothing existing can break.
- **Decompression cost.** Gunzipping the ~18 MB `/boot/vmlinuz` into a 58 MB Image costs roughly 200–500 ms of single-threaded inflate per cold boot (the in-tree `miniz_oxide` backend inflates at ~100–300 MB/s) — a large fraction of the ~250 ms cold-boot reference, not noise. The cost lands only on the host-kernel path (`VOID_BOX_KERNEL=/boot/vmlinuz-…`), i.e. arm64 dev and validation loops; release auto-resolution ships an uncompressed `vmlinux-aarch64`, so production cold boot is unaffected. The startup-bench gate boots an uncompressed slim kernel and is blind to this path, so no automated gate will flag it. Mitigation now: bounded decompression written directly into the guest-RAM slice (no transient 58 MB allocation). Follow-up, tracked as an issue when this lands: a decompressed-kernel cache keyed on the source's path/mtime/size, which removes the cost from the dev loop entirely.
- **Edge-triggered virtio interrupts.** The DTB declares edge; injection is pulse-based. A missed edge (pulse while the guest hasn't yet acked a prior one) is prevented by the existing `INTERRUPT_STATUS`-guarded re-pulse logic in the device threads, same as x86 today.

## Unresolved questions

- Whether `/dev/vhost-vsock` (the default vsock backend) behaves identically on the arm64 validation host — determined empirically in milestone 3; the userspace backend is the fallback and is exercised by the snapshot suites anyway.
- Timer PPI trigger polarity: the node currently declares level-low (8); QEMU uses level-high (4) on `virt`. The GIC driver reconfigures PPI triggers from hardware state regardless; whichever value validates on the target kernel is kept and pinned in the implementation.

## Rollout / implementation plan

Single PR against `main` (branched off #115, which it requires), implemented in boot-verified milestones on the arm64 validation host:

1. **Make the kernel actually run**: loader fixes (bounded gunzip, Image header `text_offset`/`image_size` with checked placement, dynamic initramfs placement, DTB length check), vCPU-ceiling validation, `KVM_ARM_VCPU_PSCI_0_2` feature, GIC + `/cpus` DTB nodes with the version probe, timer-cell fix, `SystemEvent` handling. Verified by the kernel reaching userspace (indirectly, until the next milestone lands console output).
2. **Serial console**: UART MMIO dispatch + DTB node + `stdout-path`. Verified by `voidbox-startup-bench --console-file` capturing kernel output with `loglevel=7`; unblocks debugging everything after it.
3. **IRQ model**: slot table, arch-aware `inject_irq`, irqfd/ioeventfd GSI mapping, vsock/9p/blk/net injection sites including the `net_poll_thread` fallback. Verified by vsock handshake (control channel connects) and by asserting the net irqfd path actually delivers into the guest, not merely that registration succeeds.
4. **Virtio device nodes + discovery convention**: DTB virtio nodes, aarch64 cmdline changes, guest-agent arch gates, `AGENTS.md` platform-table update. Verified by the full gates: smoke spec, `conformance`, `e2e_mount` (+ `e2e_telemetry`, `e2e_skill_pipeline`) on arm64/KVM; fmt/clippy/tests on arm64 Linux and macOS; x86_64 covered by CI.

On acceptance, the two load-bearing decisions are recorded as ADRs (aarch64 guest memory map, including the vCPU ceilings it implies; per-platform device discovery convention). The aarch64 snapshot/restore gaps and the decompressed-kernel cache each get a tracking issue filed when this lands.
