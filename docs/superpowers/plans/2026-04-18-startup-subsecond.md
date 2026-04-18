# Startup: remaining path to subsecond cold + sub-100ms warm

**Status:** Draft
**Date:** 2026-04-18
**Context:** follow-up to branch `feat/perf` (commits `fc4abc0` → `079a207`),
which cut cold p50 from ~4.9s to 3.5s and warm p50 from ~607ms to 433ms by
removing three hardcoded waits. Two bottlenecks remain, both beyond
single-session scope.

## Summary

| Path | Current p50 | Target | Remaining blocker |
|------|-------------|--------|-------------------|
| Cold | **3.5s**    | ≤1.0s  | ~2.9s of **kernel-side probe timeouts** (RTC, i8042) |
| Warm | **433ms**   | ≤150ms | ~385ms of **guest kernel wake-from-HLT latency** on restore |

Both are measurable with `voidbox-startup-bench` (this branch) and attributable
via the existing perf-agent + kernel dmesg tooling.

## Bottleneck 1 — cold: host kernel probe timeouts

### Symptoms

Kernel dmesg (captured via `--console-file` with `loglevel=7`) shows four
explicit timeouts during cold boot:

```
gap=1.16s at t=1.347: PM: Unable to read current time from RTC
gap=0.54s at t=2.057: i8042: Can't read CTR while initializing i8042
gap=1.16s at t=3.219: rtc_cmos rtc_cmos: broken or not accessible
gap=1.00s at t=4.451: guest-agent: Modules loaded, waiting 1s...  (FIXED in fc4abc0)
```

The RTC and i8042 drivers are **built into the host's Fedora kernel**
(`CONFIG_RTC_DRV_CMOS=y`, `CONFIG_SERIO_I8042=y`). Kernel cmdline flags
like `rtc=noprobe`, `i8042.noaux` etc. don't prevent the probe — they just
silence some sub-behaviors. The probe itself runs and times out.

### Options

1. **Ship a slim kernel.** Build a microVM-tuned x86_64 kernel (ala Firecracker
   / libkrun) with `CONFIG_RTC_CLASS=n`, `CONFIG_SERIO=n`, no ACPI, no PnP,
   no PM, only virtio drivers. Expected win: cold ~3.5s → ~0.6s.
   - Host-independent: ships as part of VoidBox's release, same artifact
     pipeline as `scripts/download_kernel.sh`.
   - Binary size: ~5 MB vs Fedora's ~15 MB.
   - Delivery: add a `scripts/build_slim_kernel.sh` and wire into
     `.github/workflows/release-images.yml` alongside the initramfs builds.
   - Reference: `microvm.nix`'s kernel config, Firecracker's `microvm-kernel`
     repo, or libkrun's `microvm-kernel-initramfs-hack` patchset.

2. **Stay on host kernel, patch `extra_cmdline`.** Try the most aggressive
   flag combo first: `clocksource=tsc tsc=reliable no_timer_check
   mitigations=off nokaslr nosmap nosmep nopti`. Measure. Likely saves
   <200ms because built-in drivers still probe. Low effort; low ceiling.

3. **Defer the slow drivers to post-user-ready.** Boot with `initcall_blacklist=
   cmos_init,i8042_init`. Only works if the kernel supports initcall
   blacklisting (CONFIG_INITCALL_BLACKLIST=y on Fedora). Untested hypothesis.

### Recommendation

Path (1). It's the only one that delivers the target and cleanly decouples
VoidBox's startup floor from the operator's host distro. Try (2) + (3) in an
afternoon first as cheap diagnostics — the results inform kernel config
priorities for (1).

### Out of scope for this spec

- aarch64 (macOS/VZ uses `scripts/download_kernel.sh` with extract-vmlinux;
  different problem shape).
- Networking subsystem overhead — orthogonal, see
  `2026-04-12-network-backend-abstraction.md`.

## Bottleneck 2 — warm: guest HLT wake-up after snapshot-restore

### Symptoms

Instrumentation in `MicroVm::from_snapshot` (commit `4d8c72d`) shows:

```
restore phases:   load=40µs  vm_new=340µs  mem=20µs  irq=5µs  vcpu=185µs
                  total host-side work: ~670µs
first exec RTT:   ~387ms
second exec RTT:  ~1.5ms   (steady state, identical VM)
guest_wake_est:   ~385ms   (= first - second)
```

The host-side restore is sub-ms. The 385ms floor is **entirely guest
kernel resume latency**: the guest was HLT/NOHZ-idle at snapshot time,
and the LAPIC timer bootstrap in `arch::x86_64::cpu.rs` (which sets LVTT
to periodic/vector 0xEC and TMICT=0x200000) schedules a first tick ~1ms
out, but the scheduler doesn't actually resume for ~385ms.

Control-channel debug logs (`RUST_LOG=void_box::backend::control_channel=
debug`) show the guest-agent accepts each connect attempt immediately,
then closes (`ECONNRESET`) or never responds (`EAGAIN`) for ~8–10 retries,
each burning a 150ms `HANDSHAKE_READ_TIMEOUT`, until one succeeds. The
guest-agent's accept loop *is* running, but something between `accept()`
returning and `send_raw_message(Pong)` completing is stalled.

### Open questions

1. **Is the LAPIC timer bootstrap actually firing?** Add a
   `KVM_IRQ_LINE`-injected check or instrument the guest to log
   `local_apic_timer_interrupt` counts in the first 500ms post-restore.
   If the first tick doesn't arrive until ~385ms, the TMICT=0x200000
   assumption about bus-clock frequency is wrong on this host.
2. **Is `handle_connection` blocked on `read_exact` waiting for Ping
   payload that's already in the socket?** kmsg-trace each step of
   `handle_connection` to see where wall time goes. If `read_exact`
   returns immediately but `send_raw_message(Pong)` stalls, the vsock
   TX path is suspect.
3. **Does pre-touching the guest address space help?** The LAPIC bootstrap
   uses `KVM_SET_LAPIC` but the guest thread only wakes on its own timer
   interrupt. Testing whether `inject_irq(vm_fd, 11)` (vsock) right after
   vCPU thread start breaks the stall would disambiguate timer-vs-vsock
   wake.

### Options

1. **Proactive wake-up IRQ.** After `from_snapshot` spawns vCPU threads,
   synchronously `inject_irq(0x30)` (LAPIC timer vector) to force the
   guest out of HLT immediately rather than waiting for the periodic
   timer hardware to fire. Experimental IRQ 0 injection was tried
   (commit `4d8c72d` preamble) with no effect — IRQ 0 goes to the legacy
   PIT; LAPIC vector is separate.

2. **Drain pending vsock events at restore.** Before returning from
   `from_snapshot`, have the userspace vsock worker thread inject IRQ 11
   once unconditionally so the guest processes any queued connections
   without waiting for a timer-driven scheduler tick.

3. **Guest-agent: don't HLT.** Add a second thread in guest-agent that
   spins on `sched_yield()` for the first 50ms after boot. Ugly and
   burns CPU, but closes the wake latency window.

4. **Accept the floor, warm-warm the VM.** After `from_snapshot`, the
   harness/runtime could issue a throwaway `sh -c :` exec before
   returning control to the user. This pushes the 385ms into the
   idle path where it doesn't block the user's first action.
   Zero guest changes. Hackiest, easiest.

### Recommendation

Investigate (1) and (2) in parallel — they disambiguate which wake path
matters. If neither drops the 385ms materially, fall back to (4) as a
pragmatic no-code-change mitigation while (3) is evaluated.

### Out of scope for this spec

- Diff snapshots (`snapshot_diff`). The wake-up problem applies equally;
  fixing base-snapshot restore first and reusing the mechanism for diffs.
- macOS/VZ snapshot restore (`backend/vz/snapshot.rs`). Different wake
  semantics via Apple's `restoreMachineStateFromURL:`; needs its own dig.
- Snapshot *creation* cost (orthogonal; users create once, restore many).

## Validation

Both investigations should re-run the same harness:

```bash
cargo build --release --bin voidbox-startup-bench
export VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r)      # or slim kernel
export VOID_BOX_INITRAMFS=/tmp/void-box-test-rootfs.cpio.gz
./target/release/voidbox-startup-bench --iters 20 --breakdown
```

Acceptance thresholds when the work lands:
- Cold p95 ≤ 1.0s (from 3.5s).
- Warm p95 ≤ 150ms (from 434ms).
- `cargo bench --bench startup` regression fence unchanged (no micro-bench
  blow-ups from the kernel/guest-agent changes).

## References

- `src/vmm/mod.rs` — `MicroVm::from_snapshot` (instrumentation landed in `4d8c72d`)
- `src/vmm/arch/x86_64/cpu.rs` — LAPIC timer bootstrap (lines 183–268)
- `src/backend/control_channel.rs` — handshake polling (post-`fc4abc0`)
- `guest-agent/src/main.rs` — `handle_connection`, `create_vsock_listener`
- `examples/specs/startup_cross_check.yaml` — single-run cross-check spec
- Kernel dmesg gap analysis: `awk` pattern in commit-message anecdote; reproduce
  with `VOIDBOX_LOGLEVEL=7` (would need cmdline plumbing; for now patch
  `src/vmm/config.rs:232` to `loglevel=7` locally for the measurement run).
