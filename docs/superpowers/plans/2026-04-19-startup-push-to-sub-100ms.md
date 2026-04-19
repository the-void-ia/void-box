# Startup: push to sub-100ms cold, sub-50ms warm

**Status:** Draft
**Date:** 2026-04-19
**Context:** follow-up to the startup-subsecond work (commits `fc4abc0` →
`3fc039f` on `feat/perf`, landing the slim kernel, the `initcall_blacklist`
cmdline flag, and three blind-wait removals). Current numbers on
`feat/perf` (Fedora 43 / KVM / slim kernel + test rootfs):

| Path | Current p50 | Target this spec | Reach target of |
|------|-------------|------------------|-----------------|
| Cold | **252 ms**  | **≤100 ms**      | Firecracker-class (~125 ms), leaner on some hosts |
| Warm | **138 ms**  | **≤50 ms**       | AWS Lambda SnapStart territory (~50–100 ms) |

**Honest framing:** these targets are aggressive. Competitor runtimes
(Firecracker, libkrun, cloud-hypervisor) have years of dedicated
microVM optimization and often measure at a different point in the
timeline ("guest init complete", not "first exec RTT"). VoidBox at
252 ms / 138 ms is already competitive for a general-purpose agent
runtime. This spec describes the **theoretical** remaining budget and
what it costs to claim.

Both numbers are `voidbox-startup-bench --iters 20 --breakdown` with
slim kernel (`scripts/build_slim_kernel.sh`).

## Summary of levers

| Lever | Cold Δ | Warm Δ | Cost / risk |
|-------|--------|--------|-------------|
| Shrink slim kernel further (`tinyconfig`-up) | −40…80 ms | 0 | Config churn; risk of dropping a driver we need |
| PVH boot entry | −15…40 ms | 0 | Loader-level change; ELF note parsing + new entry mode |
| `init_on_alloc=0` / `init_on_free=0` | −10…30 ms | 0 | Security regression — opt-in only, not default |
| Skip guest-agent `setup_network()` when `network=false` | −20 ms | 0 | Already gated but can be tightened |
| Shared-memory control channel (ivshmem-style) | 0 | −80…100 ms | New protocol layer; vsock dropped for RPC |
| Pre-warm handshake during `from_snapshot` | 0 | −60…80 ms | Complexity in restore; concurrency with vCPU resume |
| Persistent control channel across exec | −30 ms | −30 ms | Protocol becomes stateful |
| `KVM_CAP_X86_DISABLE_EXITS_HLT` | 0 | −5…15 ms | Small, KVM-only, defensive |

Combined realistic reach: cold ~130–170 ms, warm ~40–70 ms. Sub-100 ms
cold is only plausible if PVH lands AND the slim kernel loses another
~40 ms of driver init.

## Cold path — where the 252 ms goes

Post-rebase cold p50 decomposes roughly:

```
build  ~10 µs      Sandbox::local().from_env().build()
boot   ~207 ms     VM spawn → kernel entry → guest-agent listens
                   → vsock handshake → first exec RTT
stop   ~50 ms      drop + KVM teardown + thread joins
─────
total  ~257 ms
```

The `boot` phase (207 ms) is where remaining wins live. Rough
decomposition from `--console-file loglevel=7` traces:

```
 0.00–0.15 s   KVM_CREATE_VM + vcpu init + kernel load
 0.15–0.35 s   kernel boot: earlyprintk → setup_arch → init_IRQ
              → page tables + paging_init → sched_init → rcu_init
              → TSC calibration → SMP bring-up (single vCPU still costs ms)
 0.35–0.42 s   driver init (virtio-{mmio,blk,net,vsock,fs,9p} → overlay fs)
 0.42–0.50 s   PID 1 / guest-agent: set_hostname, setup_network,
              mount proc/sys/dev, kmod-load (no-ops under slim kernel),
              vsock_listener.bind(1234)
 0.50–0.60 s   host handshake retry converges (first Pong arrives)
              + first exec round-trip
```

The kernel-level piece (0.15–0.42 s = ~270 ms of kernel wall time) is
where `tinyconfig`-up, PVH, and `init_on_alloc=0` all attack.

### Lever 1 — Shrink the slim kernel further

Firecracker's 6.1 config is already pruned, but we can go further. The
`tinyconfig`-up approach:

```
make ARCH=x86_64 tinyconfig     # start from near-zero
# Add only what boot + virtio-{mmio,vsock,net,fs,9p,blk} + overlayfs need
```

Candidate drops from the current slim config (audit via
`scripts/kconfig/streamline_config.pl` on a real boot, or manual grep):

- `CONFIG_DEBUG_*` — debug info pulls in symbol tables; keep off for
  release images, keep on for `vmlinux-slim-debug-*`.
- `CONFIG_AUDITSYSCALL`, `CONFIG_AUDIT_WATCH` — no auditd in guest.
- `CONFIG_MAGIC_SYSRQ` — no serial console SysRq path needed.
- `CONFIG_SECURITY_SELINUX`, `CONFIG_SECURITY_APPARMOR` — we run an
  unconstrained guest-agent.
- `CONFIG_SND_*`, `CONFIG_DRM_*`, `CONFIG_INPUT_*` (except what PS/2
  stub needs), `CONFIG_USB_*` — zero device surface in our guest.
- `CONFIG_HW_RANDOM_*` (we use `getrandom` via `CONFIG_RANDOM_TRUST_CPU`).
- `CONFIG_BTRFS_FS`, `CONFIG_XFS_FS`, `CONFIG_JBD2`, `CONFIG_F2FS_FS`
  — we only need ext4 (for OCI rootfs disk) + tmpfs + overlay + 9p + virtiofs.

Expected saving: 40–80 ms of cold, plus ~5 MB of uncompressed ELF size
(from ~30 MB to ~25 MB). Risk: each drop may expose a hidden dependency
discovered only at runtime. Budget 1–2 days of iterate-bench-iterate.

### Lever 2 — PVH boot entry

Linux supports a "Paravirtualized Hardware" (PVH) entry point —
documented in `Documentation/x86/boot.rst`. A PVH-aware loader jumps
directly into 32-bit protected mode with a pre-populated start_info
struct, skipping the real-mode setup code and early boot protocol
negotiation. Both Firecracker and cloud-hypervisor use PVH.

The entry point is advertised in the kernel ELF via a PT_NOTE of type
`Xen_ELFNOTE_PHYS32_ENTRY`. Our `load_elf_kernel` already reads
`e_entry`; extending it to parse the PVH note is ~50 LoC. The VMM
side needs to:

1. Parse the `XEN_ELFNOTE_PHYS32_ENTRY` note from the kernel ELF.
2. Build a `hvm_start_info` struct (memory map + cmdline pointer).
3. Set `%ebx = &hvm_start_info` in vCPU regs.
4. Enter the vCPU in 32-bit protected mode, not 64-bit long mode.

Firecracker's `src/arch/src/x86_64/` has a working reference (MIT
license, permissively reusable).

Expected saving: 15–40 ms. Risk: kernel boot protocol mismatch surfaces
as a silent triple-fault — same failure mode as the ELF-entry-point
bug we already fixed. Budget 2–3 days.

### Lever 3 — Unsafe boot-time inits

```
init_on_alloc=0 init_on_free=0 mitigations=off nokaslr
```

These disable zero-on-alloc (defense against uninitialized memory reads),
zero-on-free (defense against UAF reads), CPU vulnerability mitigations
(Spectre/Meltdown/MDS/Retbleed), and KASLR.

For our threat model (isolated single-purpose guest with no host-shared
memory and one-shot lifecycle), these mitigations cost more than they
protect. But they should be **opt-in**, not default — cold boot spec
should not trade security for 20 ms on every VM.

Proposal: add an `agent.fast_boot: true` YAML field that appends these
flags. Document the tradeoff explicitly.

Expected saving: 10–30 ms combined.

### Lever 4 — Trim guest-agent init

`guest-agent/src/main.rs` currently runs a sequence at PID 1:

```
set_hostname → mount_proc_sys_dev → setup_network → load_kernel_modules
  → mount_shared_dirs → OCI gate → vsock_listener.bind
```

With the slim kernel, `load_kernel_modules` is a no-op (every module
we need is built-in). It still walks `/lib/modules/` and calls
`finit_module` on each, getting `ENOSYS` on each. That's cheap but
non-zero. Gate on `/proc/modules` containing the needed driver before
trying to load.

`setup_network` without `network=true` is already skipped; verify the
gate is tight (one `/proc/cmdline` read).

Expected saving: ~20 ms cold (network-enabled), ~5 ms cold (no
network). Trivial change — can be taken alongside the slim kernel
shrink.

## Warm path — where the 138 ms goes

Warm p50 decomposes (from the bench's `--breakdown` output):

```
build           ~10 µs     Sandbox::local().snapshot(path).build()
first exec      ~92 ms     from_snapshot (sub-ms) + handshake + exec RTT
second exec     ~1.4 ms    steady-state (already-warm) exec RTT
stop            ~46 ms     Sandbox::stop() — teardown
──────
total           ~139 ms
guest_wake_est  ~90 ms     first_exec - second_exec
```

Host-side `from_snapshot` phases (from `MicroVm::from_snapshot`
instrumentation, commit `4d8c72d`):

```
load_state   ~40 µs
vm_new      ~340 µs
mem         ~20 µs       COW mmap — lazy, doesn't actually load
irqchip      ~5 µs
vcpu       ~185 µs       KVM_SET_* x86_64 state restore
────
host total  ~670 µs       NEGLIGIBLE
```

The 90 ms of "guest_wake_est" is **not** guest kernel HLT wake — that
was disproved in the previous spec. It's the host-side vsock handshake
retry loop converging as the guest-agent's per-connection thread
schedules + replies to Ping. The current retry cadence:

```
first attempt     t=0
handshake_timeout (read Pong)  150 ms
sleep 25 ms  → 2nd attempt t=175 ms   (too late, deadline at 138 ms)
```

So one retry typically occurs. We're at the knee already.

### Lever 5 — Shared-memory control channel

Replace the vsock handshake + first-exec RTT with a pre-established
shared-memory ring buffer (ivshmem or custom memory-mapped region
injected into the VM at snapshot time). Host writes an ExecRequest
into the ring; guest-agent polls the ring and processes it
immediately.

Expected saving: 80–100 ms on warm (eliminates handshake entirely).
Complexity is high — new protocol layer, serialization across
process boundaries, keeping vsock for fallback/telemetry. Budget
1–2 weeks.

### Lever 6 — Pre-warm handshake during `from_snapshot`

Instead of waiting until the first `exec()` call to handshake, start
the handshake as soon as `from_snapshot` begins:

```rust
async fn from_snapshot(path) {
    let vm = spawn_vm(path);
    tokio::spawn(async move {
        // Handshake in background while caller still holds build() result.
        vm.control_channel.warm_handshake().await;
    });
    Ok(vm)
}
```

The first `exec()` call then finds the channel already handshaken —
no retry loop. Expected saving: 60–80 ms. Low complexity, maybe 50
LoC. Budget 1 day.

### Lever 7 — Persistent control channel

The current channel is one-shot: each call in `ControlChannel`
connects, handshakes, sends, reads response, closes. For a running
agent doing 20+ tool calls, that's 20 handshakes.

Making the channel persistent (one connection per `Sandbox`, all
calls multiplexed via request IDs) removes 19 of those 20 handshakes.
The first handshake still costs whatever it costs.

Expected saving: 30 ms on warm start-to-first-exec (the first
handshake is the one that matters there). But **huge** savings on
agent workloads — every subsequent exec/write_file/mkdir_p drops
from ~2 ms to ~0.5 ms RTT. This is a protocol-level change; plan for
1 week of design + implementation + back-compat story.

### Lever 8 — `KVM_CAP_X86_DISABLE_EXITS_HLT`

The in-kernel irqchip default already handles HLT in-kernel, but we
can explicitly request `KVM_CAP_X86_DISABLE_EXITS_HLT` to be sure —
this tells KVM to keep HLT in-kernel across all paths. Measured
saving is usually tiny but the change is 5 LoC and defensive.

Expected saving: 5–15 ms (noise-level, but free).

## Instrumentation additions

Before any of the above land, add these measurement hooks to avoid
flying blind:

1. `voidbox-startup-bench --phases` — emit time spent in each of
   `vm_new / kernel_load / kernel_boot / guest_agent_init /
   handshake / first_exec`. Today the bench only splits on coarse
   `build/boot/stop`.
2. Guest-agent ring-buffer timestamp log at well-known offsets in
   `/dev/kmsg` so the host can correlate its perceived timeline with
   the guest's. Add `TSC` readings at each of the above phase
   boundaries.
3. `voidbox shell --trace-startup` that dumps the full decomposition
   after the first exec RTT completes.

Budget: half a day. Essential before PVH / tinyconfig work so we can
attribute savings to specific levers instead of arguing over noise.

## Prioritized milestones

**Milestone A — free wins (1–2 days, cold ≈ 220–230 ms, warm ≈ 100 ms)**
- Lever 8 (`DISABLE_EXITS_HLT`)
- Lever 4 (gate guest-agent module-load on-demand)
- Lever 6 (pre-warm handshake)
- Lever 7 (persistent control channel) ← biggest single warm win

**Milestone B — kernel surgery (3–5 days, cold ≈ 170–190 ms, warm unchanged)**
- Lever 1 (shrink slim kernel via tinyconfig-up)
- Lever 2 (PVH boot entry)

**Milestone C — opt-in boot flags (1 day, cold ≈ 150–170 ms)**
- Lever 3 (`fast_boot: true` YAML field, documented tradeoff)

**Milestone D — protocol redesign (1–2 weeks, warm ≈ 40–50 ms)**
- Lever 5 (shared-memory control channel) — only if milestones A–C
  don't produce a good-enough warm number. Biggest blast radius.

## Out of scope

- Snapshot creation time (`~420 ms` base / `~270 ms` diff). Users
  create once, restore many times. Acceptable.
- macOS/VZ startup parity. VZ has its own boot path with different
  bottlenecks (`restoreMachineStateFromURL:` has different
  characteristics). Needs its own investigation.
- Host-side PID 1 / daemon startup. Not on the user-perceived path.

## Validation contract

Each milestone must pass before landing:

1. `cargo test --workspace --all-features` — green
2. `scripts/build_slim_kernel.sh` — builds clean
3. `voidbox-startup-bench --iters 20 --breakdown` — regression
   thresholds: cold p95 ≤ 400 ms, warm p95 ≤ 200 ms (matching the
   `verify` skill gate)
4. `conformance`, `oci_integration`, `e2e_mount`, `snapshot_integration`
   — all green
5. HN agent (`examples/hackernews/hackernews_agent.yaml`) runs to
   completion
6. OpenClaw Telegram gateway (`examples/openclaw/openclaw_telegram.yaml`)
   — `smoke_message` step posts to Telegram

## References

- commits `fc4abc0` → `3fc039f` on `feat/perf` — the startup-subsecond
  work that produced the current numbers
- `docs/architecture.md` §"Snapshots / Performance" — current published numbers
- Firecracker kernel config: https://github.com/firecracker-microvm/firecracker/tree/main/resources/guest_configs
- PVH boot protocol: `Documentation/x86/boot.rst` in the Linux source
- cloud-hypervisor PVH loader: https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/arch/src/x86_64/mod.rs
