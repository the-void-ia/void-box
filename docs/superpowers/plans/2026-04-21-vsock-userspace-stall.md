# Investigation: guest-side stall around exec 24 on tight-loop RPC bursts

**Status:** Open — partial workaround shipped (Vhost branching in
commit `14b3840`); root cause located but not fixed.
**Date opened:** 2026-04-21
**Date updated:** 2026-04-22 — re-scoped from "userspace-vsock
flow-control bug" (wrong) to "guest-side stall in `execute_command`
OCI gate" (confirmed by kmsg trace).

---

## tl;dr for the next investigator

**This is NOT a userspace-vsock credit-flow bug.** Earlier hypotheses
in this spec (H1 descriptor leak, H2 missing CreditUpdate, H3 kick
wiring, H4 partial-write accounting) were written before evidence was
gathered and are **wrong**. Don't spend time on vsock plumbing.

**The stall is inside the guest**, in `guest-agent::execute_command`
(`guest-agent/src/main.rs`), between these two kmsg log points:

```
[    N] Exec start: program='sh' args=2 oci_status=not-run     <-- last message seen
[    -] Exec gate passed: oci_status=not-run                   <-- never reaches here
```

That window contains three calls: `trigger_oci_rootfs_setup_async()`,
`wait_for_oci_setup_ready(30s)`, and two `oci_status_str(...)` reads
of an AtomicU8. None of them should block — `oci_rootfs_requested()`
returns `false` on the minimal test initramfs (no `voidbox.oci_rootfs`
in kernel cmdline), so `wait_for_oci_setup_ready` should return
`Ok(())` immediately.

Something in that window (`/proc/cmdline` file I/O, memory allocation
churn from repeated `String` allocs, PID-1-specific scheduler
interaction, or a race against the guest-agent's signal handlers) is
silently blocking after ~12–28 iterations of tight-loop exec.

The number is **timing-sensitive**: baseline config stalls at iter
~28; adding more kmsg calls per exec shifts it to iter ~12. That
rules out a static descriptor/FD/thread limit and points at a
timing-dependent resource that accumulates and gets cleared given
wall-clock headroom.

**Workaround shipped in `14b3840`:** `KvmBackend` cold-boots with
Vhost-vsock when `!enable_snapshots && snapshot.is_none()`. Vhost
doesn't trigger the stall (different kernel-side IRQ/wakeup path).
Real agent workloads (HN, service-mode, skill pipeline) take the
Vhost path and work. Userspace-vsock is used only when snapshot
compatibility is actually needed.

---

## What the user-visible failure looks like

```
$ cargo test --test persistent_channel -- --ignored --test-threads=1
# with SERIAL_EXEC_COUNT bumped from 16 to 100:
test persistent_channel_serial_exec_many ...
thread '...' panicked at tests/persistent_channel.rs:135:33:
exec 28 failed: Guest communication error: exec timed out after 30s
```

30 s is the host-side exec timeout. Exec 28 is sent, guest-agent
kmsgs "Exec start" for the corresponding `request_id`, then: silence.
No "Exec done", no "Exec response sent", no response ever reaches
the host's multiplex reader.

Affects the **userspace vsock** backend only (AF_UNIX bridge). The
Vhost backend (AF_VSOCK through the kernel) does **not** exhibit the
stall — same guest-agent code, same kmsg trace, just keeps going.
That's why the 14b3840 workaround works.

`snapshot_integration::auto_snapshot_round_trip` passes cleanly with
the slim kernel — it does one snapshot + one restore + one exec, well
below the ceiling. Earlier claim in this doc that it "hangs under
the same condition" was measurement error and is retracted.

---

## Evidence captured on 2026-04-22

### Host side (gstack during hang)

Every thread parked, nobody doing work:

| Thread | State |
|---|---|
| `vcpu-0` | `KVM_RUN` ioctl (guest HLT'd) |
| `vsock-irq` | `epoll_wait` on call eventfds |
| `vsock-userspace` worker | `epoll_wait` (50 ms timeout, no events) |
| `multiplex-reader` | `libc::read` on vsock fd — **waiting for bytes** |
| `tokio-runtime-w` × N | `epoll_wait` |
| main | `parking_lot::condvar::wait` |

The host is healthy and idle. It's blocked exclusively because the
guest never sends more bytes.

### Guest side (kmsg trace with `loglevel=7`)

Added `kmsg()` at each phase of `handle_connection` dispatch and
`execute_command`. Output for the stalling iteration:

```
[  1.948566] Exec start: program='sh' args=2 oci_status=not-run
[NO FURTHER OUTPUT]
```

Earlier iterations (1–11) all show the full sequence:

```
Exec start: ... oci_status=not-run
Exec gate passed: oci_status=not-run
DBG rid=N pre-spawn
DBG rid=N post-spawn pid=...
DBG rid=N pre-wait
DBG rid=N post-wait exit=0
DBG rid=N joined stdout
DBG rid=N joined stderr
```

**The stall is between "Exec start" and "Exec gate passed".** That's
this block in `guest-agent/src/main.rs::execute_command`:

```rust
// "Exec start" kmsg fires here
kmsg(&format!("Exec start: program='{}' ...", ...));

// ↓↓↓ STALL HAPPENS SOMEWHERE IN THIS WINDOW ↓↓↓
trigger_oci_rootfs_setup_async();    // reads /proc/cmdline
if let Err(e) = wait_for_oci_setup_ready(Duration::from_secs(30)) {
    // unreachable on minimal test image — no voidbox.oci_rootfs on cmdline
}
let status = oci_status_str(OCI_SETUP_STATUS.load(Ordering::Acquire));
// ↑↑↑ STALL HAPPENS SOMEWHERE IN THIS WINDOW ↑↑↑

// "Exec gate passed" kmsg fires here — never reached
kmsg(&format!("Exec gate passed: oci_status={}", status));
```

### Timing-sensitivity observation

Baseline (no extra kmsg):           stalls around iter 28.
With ~6 extra kmsg per exec:        stalls around iter 12.
With `loglevel=0` (kmsg suppressed from console): stalls around iter ~28.

More work per exec → stall earlier. Not a fixed quota. Some kind of
scheduler / allocator / IO-queue pressure that takes time to clear.

### What was ruled out (spend no more time here)

| Hypothesis | Evidence against |
|---|---|
| Virtio-vsock descriptor leak | Traced `pop_avail` / `push_used` balance, added re-queue on RX-full — stall persists |
| Missing CreditUpdate from host → guest peer_free hits 0 | Instrumented `queue_credit_update` firing after every RW, `fwd_cnt` advances correctly to ~5 KB at stall (peer_buf_alloc is 256 KB) — stall persists |
| AF_UNIX partial-write drops bytes | Added `rx_pending` buffering with retry loop — stall persists, and "partial write" eprintln never fires in repro |
| RX virtqueue full → packets dropped | Added re-queue on `pop_avail() == None` — "RX queue full" trace never fires in repro |
| Guest watchdog thread leak | Already fixed in commit `d6c974f` (condvar wakeup + join). `fds=6 threads=2` stays flat across all execs |
| Guest fds/thread limit | Per-process count stable per exec, no monotonic growth |
| Control channel handshake races | Single long-lived multiplex channel; handshake only on first exec |

**Time to stop investigating on the host side.** The host is idle,
waiting for bytes that never arrive. The guest is the black box.

---

## Remaining hypotheses (guest-side)

### G1. `/proc/cmdline` read stalls under repeated access

`oci_rootfs_requested()` calls `std::fs::read_to_string("/proc/cmdline")`
**per exec**. The Rust path does `open` → `read_to_end` (growing
Vec) → `close`. On a minimal guest running as PID 1, something about
repeated opens of a proc file might stall under pressure.

**To check:**
1. Cache the `oci_rootfs_requested()` result in a `OnceCell<bool>` —
   kernel cmdline doesn't change after boot.
2. Repro with the cache in place. If stall still happens, rule out.

This is the cheapest thing to try first.

### G2. Memory allocation churn in PID 1

`format!` in the kmsg calls, `String` in `read_to_string`, Vec growth
in body decoding all go through the same allocator. PID 1 in a
minimal Linux has idiosyncratic OOM and fault behavior. Tight-loop
allocs/frees may hit an allocator slowpath.

**To check:** run with `MALLOC_ARENA_MAX=1` env (guest-agent env),
or switch guest-agent to use `jemalloc` — see if the stall shifts.

### G3. Guest kernel scheduler starving PID 1

Single-vCPU VM. PID 1 is running the handler loop, plus SIGCHLD from
each spawned `sh`, plus the shell's own context switches. If the
kernel's scheduler picks a pathological rebalance moment, PID 1 may
get preempted for longer than expected. Combined with the
`KVM_DISABLE_EXITS_HLT|PAUSE` setting, the host can't even observe
that pause — vCPU just stays in KVM_RUN.

**To check:**
1. Force `vcpus=2` in the bench and see if the stall ceiling changes.
2. Disable the `KVM_DISABLE_EXITS_HLT` setting temporarily and profile.

### G4. `std::process::Command::spawn` + reaper interaction

Each exec does `fork + execvp` (spawning `sh`). PID 1 has special
responsibilities for child reaping (SIGCHLD handling). If a child
escapes our `child.wait()` tracking — e.g. the shell spawns its own
subprocess that gets reparented to PID 1 on exit — it could clog
the PID 1 SIGCHLD queue and serialize wait() calls.

**To check:** `ls /proc/1/task/` before/after the stall, check for
zombies with `grep "Z " /proc/*/stat`. Probably the most likely
candidate for the ~28 ceiling: exec spawns `sh -c "echo N"` → sh
execs echo (same PID, no reparenting) → echo exits. But if sh does
internal forks, reparenting happens.

---

## Reproduction

```bash
# Build test image (requires the watchdog fix, in commit d6c974f+).
BUSYBOX=/usr/bin/busybox scripts/build_test_image.sh

export VOID_BOX_KERNEL=$PWD/target/vmlinux-slim-x86_64
export VOID_BOX_INITRAMFS=/tmp/void-box-test-rootfs.cpio.gz

# Fastest repro — rpc-bench against Userspace vsock:
cargo run --release --bin voidbox-rpc-bench -- --seq-iters 50 --conc 4
# Hangs indefinitely at iter ~23.

# Via integration test:
# Edit tests/persistent_channel.rs:
#   const SERIAL_EXEC_COUNT: usize = 100;
# Then:
cargo test --test persistent_channel -- --ignored --test-threads=1
# Fails with: exec 28 failed: exec timed out after 30s
```

### Gotcha: the bench may hide the stall under Vhost branching

Commit `14b3840` makes `KvmBackend` pick Vhost-vsock for
non-snapshot cold boot. The rpc-bench builds a `Sandbox` without
`enable_snapshots(true)`, so it gets Vhost and does **not** stall.
To reproduce the userspace-vsock stall, either:

- use the old `fe8cceb` path (pre-14b3840), or
- add `.enable_snapshots(true)` to the bench's sandbox builder, or
- use `snapshot_integration`-style tests that capture + restore.

---

## Investigation plan

1. **Try G1 first (cache `/proc/cmdline`).** One-line change to
   `oci_rootfs_requested()`. Re-run bench at 100 iters. 10 minutes.
2. If G1 doesn't resolve: **get a gstack of the guest at stall**.
   This requires patching guest-agent to self-dump `/proc/self/stack`
   into `/dev/kmsg` on a trigger (e.g. on receiving a specific env
   var or via a timer). The dump will show the EXACT syscall/library
   function where PID 1 is blocked.
3. If that shows a blocking syscall: fix the specific call site.
4. If that shows PID 1 running: it's G3 (scheduler) — profile with
   `perf stat` on the vCPU thread during the stall window.
5. **Re-test at 100 iters** after each attempted fix.
6. **Bump the cap** in `tests/persistent_channel.rs` to 100 / 64
   only after 100-iter bench is green.

## Relevant files

| Path | Role |
|---|---|
| `guest-agent/src/main.rs::execute_command` | **where the stall lives** — between "Exec start" and "Exec gate passed" kmsg points |
| `guest-agent/src/main.rs::oci_rootfs_requested` | repeated `/proc/cmdline` reads per exec — first suspect (G1) |
| `guest-agent/src/main.rs::wait_for_oci_setup_ready` | bounded wait; should return Ok(()) immediately on no-oci path |
| `src/devices/virtio_vsock_userspace.rs` | host-side vsock backend (investigated and **ruled out** as cause) |
| `src/devices/vsock_connection.rs` | credit flow / write path (investigated and **ruled out** as cause) |
| `src/backend/kvm.rs::start` | the Vhost-branching workaround (`14b3840`) |
| `tests/persistent_channel.rs` | regression test (capped at 16 pending fix) |
| `src/bin/voidbox-rpc-bench/main.rs` | repro harness |

## Impact

- **Not a regression for real agent workloads.** HN, service mode,
  skill pipeline, mount, telemetry, snapshot — all pass. These either
  space RPCs out in wall-clock time or take the Vhost path via the
  14b3840 workaround.
- **Blocks** raising the `persistent_channel` cap past 16 and any
  workload that wants to burst > ~20 RPCs through the userspace
  vsock backend.
- Blocks the `rpc-bench --seq-iters 200` CI-regression-guard goal.
- **No user-facing symptom** as long as workloads stay on the Vhost
  path (which is the default after `14b3840` for all non-snapshot
  cold boots).

## Out-of-scope / notes

- macOS VZ uses Apple's vsock transport (not this userspace backend),
  not affected.
- The 14b3840 workaround is snapshot-aware: any run that opts into
  snapshots (`enable_snapshots: true` or `snapshot: Some(...)`) uses
  Userspace vsock and is potentially affected. Today that path is
  only exercised by `snapshot_integration` tests, which don't do
  tight-loop bursts, and by `create_auto_snapshot` flows, same.
- When the real fix lands, `14b3840` can be reverted and cold-boot
  will go back to Userspace always — recovering the fast pre-bound
  listener handshake path.

## History / dead ends

- **2026-04-21 spec (this file, original):** theorized userspace-vsock
  flow-control bugs (H1–H4). All ruled out by 2026-04-22 evidence. Kept
  in git history for reference.
- **2026-04-22 attempted fixes (reverted):** proactive CreditUpdate
  after every host-consumed RW + AF_UNIX partial-write buffering with
  `rx_pending`. Both landed briefly on `feat/perf-2-on-main`, both
  reverted after kmsg trace showed the stall is guest-side, not vsock.
- **2026-04-22 workaround (kept):** `14b3840` — Vhost branching.
