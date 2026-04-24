# Investigation: guest-side stall around exec ~21 on tight-loop RPC bursts

**Status:** RESOLVED 2026-04-23. One-line fix in `guest-agent/src/main.rs`:
remove the `eprintln!` path from `kmsg()`, keep only the `/dev/kmsg`
write. See "Root cause" below.
**Date opened:** 2026-04-21
**Date updated:** 2026-04-23

---

## TL;DR — Root cause and fix

**Cause.** `kmsg()` in `guest-agent/src/main.rs` used to write the
message on **two** paths:

1. `eprintln!(msg)` → guest-agent's `stderr` (fd 2) → `/dev/console`
   (serial tty), and
2. `open("/dev/kmsg") → writeln! → close` → kernel printk ring buffer
   → serial console

Every `kmsg()` call therefore produced roughly twice the guest serial
traffic of a single-path log. Under the persistent multiplex control
channel, `handle_connection` processes requests back-to-back with no
socket reconnect between iterations, so `kmsg()` is called many times
per iteration on a single thread. After ~20–28 such iterations (exact
count depends on how many `kmsg()` calls per exec) the guest kernel's
n_tty write path runs out of `write_room`, and the next
`eprintln!` call blocks inside `write(2)` in `tty_write` →
`wait_event_interruptible(write_room > 0)`. The guest kernel then
goes idle at `pv_native_safe_halt` (we confirmed this via
`KVM_GET_REGS` — every sample returned RIP exactly in
`pv_native_safe_halt`). The host's `multiplex-reader` blocks in
`libc::read(vsock_fd)` waiting for a response that never comes.

The `/dev/kmsg` path does **not** exhibit this behaviour: kernel
printk uses its own ring buffer with a rate-limiter that **drops**
rather than blocks, and serial drain is handled by a separate kernel
work queue that is not subject to the per-fd write_room gate.

**Fix.** Drop the `eprintln!` line. `/dev/kmsg` alone produces the
same serial-console output (kernel printk already flushes to the
console) without the tty-layer backpressure path. The dual-write
added no visibility, only load.

```diff
 pub(crate) fn kmsg(msg: &str) {
-    // Write to both stderr and /dev/kmsg for maximum visibility
-    eprintln!("{}", msg);
     if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open("/dev/kmsg") {
         use std::io::Write;
         let _ = writeln!(f, "guest-agent: {}", msg);
     }
 }
```

**Validation.**

- `persistent_channel_serial_exec_many` with `SERIAL_EXEC_COUNT = 100`,
  `timeout: None`, `vcpus: 1`, userspace-vsock backend: **passed**
  (100 execs in 7.86 s).
- `persistent_channel_serial_exec_many` + `persistent_channel_concurrent_exec`
  with stock config (16 execs, 30 s timeout, vcpus=2): both passed in
  12.89 s.

**Workaround prior to the fix:** `KvmBackend` cold-boots with
Vhost-vsock when `!enable_snapshots && snapshot.is_none()` (commit
`14b3840`). Vhost masks the stall only coincidentally — vhost's
in-kernel IRQ injection has slightly different per-iter timing that
let the tty write-work queue breathe enough to keep write_room > 0.
Now that the root cause is fixed, the userspace-vsock path is safe
to use under sustained multiplex load too; the vhost default remains
correct for performance but is no longer load-bearing for correctness
in this regime.

---

## Why the earlier hypotheses were wrong

The investigation took multiple wrong turns before landing on the
real cause. Preserving the trail for future investigators of similar
"stall around iter N" patterns:

| Hypothesis | Why it was wrong |
|---|---|
| Userspace-vsock credit-flow bug | All vsock state proven healthy: `fwd_cnt` tracks correctly, peer_buf_alloc has headroom, no descriptor leak. |
| "Stall between Exec start and Exec gate passed" | Wrong kmsg pair comparison: both kmsgs counts matched at stall. Stall is AFTER `Exec gate passed`, not before. |
| Guest-userspace hot-loop in `execute_command` | RIP sampled 6 times via KVM_GET_REGS — all at `pv_native_safe_halt`. Guest is **idle**, not spinning. |
| Guest-kernel IRQ livelock (8259 in virtual-wire mode) | 44 Hz 8259/PIT access is normal scheduler tick, not livelock. Guest kernel watchdog (softlockup + NMI) both stay silent across 5+ min stall. |
| IRQ injection cadence (assert+deassert pulse) | Tested by moving to `KVM_IRQFD` — stall reproduced identically, same iter, same pattern. IRQ plumbing exonerated. |

The **correct** decisive evidence:

1. **Guest RIP = `pv_native_safe_halt`** across 6 back-to-back samples.
   The guest is idle. If this is idle, all tasks are blocked.
2. **Stall is between `kmsg("post-wait")` and `kmsg("pre-join-stdout")`**
   — consecutive kmsg calls with almost zero code between them.
3. **Stall iter correlates with kmsg count per iter**
   (~28 → ~21 → ~8 → ~6 as instrumentation added kmsgs).
4. **Removing `eprintln!` from `kmsg()` fixed it.**

The key insight: RIP in the idle loop + stall bracketed by two
`kmsg()` calls + iter count scaling with kmsg count ⇒ the stall is
in a syscall **inside** `kmsg()` that blocks after cumulative load.
The `/dev/kmsg` path had no such history; the only other thing
`kmsg()` did was `eprintln!`. Remove it, test passes.

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

## 2026-04-23 final: guest-side burn-loop, not a host-side deadlock

**Both the "host-side deadlock" H5 and earlier "guest syscall stuck"
framings were wrong.** The correct picture, captured via gdb attach
on a timeout=None test + `top -H`:

| Thread | State during stall |
|---|---|
| vcpu-0 | **99% CPU**, `R` state, inside `KVM_RUN` ioctl. `nonvoluntary_ctxt_switches` growing (5975 in 30s). |
| vcpu-1 | 0% CPU, `S` state, inside `KVM_RUN` ioctl (HLT'd). Voluntary switches stable (≈11). |
| multiplex-reader | Blocked on `libc::read(vsock_fd)` — no bytes arriving. |
| All others | Parked (`epoll_wait`, `nanosleep`, `parking_lot::Condvar`). |

Two captures 30 seconds apart are **bit-for-bit identical stacks**.
No host-side state is changing. And yet the guest is producing
~1455 kvm_exits/sec — i.e. vcpu-0 **is running guest code** and
exiting to host (~every 700 µs), just not producing any response
bytes on the vsock.

The "0 kvm_exits/s" observation from the earlier PMU capture was
**post-panic VM teardown**, not the stall itself. Retract the H5
interpretation.

### What the data forces

- One guest vCPU is in a **CPU-bound loop in guest code**, not
  blocked on anything the host can see.
- The "exit + re-enter" cycle at ~1.4 kHz implies the guest keeps
  returning to host (MMIO, IRQ, PAGE_FAULT…) and re-entering. It's
  not a pure guest-kernel spin on a cpu-local register.
- The host is 100 % idle except for the vcpu-0 thread spinning
  through KVM_RUN/exit/handle/re-enter. There is no host lock to
  chase.

### Eliminated as cause (2026-04-23)

- **Multi-vCPU scheduler interaction.** Repro with `vcpus: 1` —
  stalls at the same iter 21. Not a multi-CPU issue.
- **`KVM_DISABLE_EXITS_HLT|PAUSE` optimization.** Tested with the
  cap disabled; stalls identically. Not the halt-polling change.
- **Double multiplex-reader race** (fixed in `35a5e88`). Kept as a
  correctness improvement; does not fix the stall.
- **`/proc/cmdline` churn** (G1, fixed in `df75fed`). Kept; does not
  fix the stall.
- **Guest-agent resource accumulation (thread/fd/zombie/memory
  leak).** Added per-exec snapshot of
  `/proc/self/{task,fd,statm,stat}` plus a pass over `/proc/*/stat`
  for zombies. Across 15 iters the numbers are totally flat:
  `threads=3, fds=6, rss_kb=748, zombies=0, stat_state=S`. So
  there is **no leak** in guest-agent's resource footprint leading
  up to the stall. Rules out G2 (allocator churn) and G4 (fork/wait
  reaping) as causes.

### Strongest signal so far: stall is entirely inside guest execution

`strace -c -p <vcpu-0>` during the stall shows **zero completed
syscalls in 5 seconds**. `strace -e trace=ioctl -p <vcpu-0>` shows a
single `ioctl(18, KVM_RUN` with no return across a 2-second attach
window.

At the same time, `perf stat -e kvm:kvm_exit -a -I 1000` shows
~1455 vmexits/sec. These facts are consistent: the CPU is leaving
guest mode, but KVM is handling those exits entirely inside the host
kernel fast path and re-entering guest mode without returning to
userspace.

This proves the stall is **not in host userspace**. It lives entirely
inside guest execution while the VMM remains blocked in one long
`KVM_RUN`.

The strongest current hypothesis is a **guest-kernel softlockup or
spin-loop**, but guest userspace hot-looping is not fully ruled out
yet. The remaining question is therefore not host-vs-guest, but
**guest kernel vs guest userspace**.

### Guest-kernel softlockup experiment: weakens kernel-lockup hypothesis

Added `softlockup_panic=1 watchdog_thresh=10` to the guest kernel
cmdline. With `watchdog_thresh=10`, the softlockup detector fires
after a CPU is stuck in kernel mode without scheduling for
approximately `2 × thresh = 20s`.

**Result:** reproduced the stall with `vcpus: 1, timeout: None` and
waited 45+ seconds. No softlockup panic. No `watchdog: BUG`, no
`Kernel panic - not syncing`, no NMI backtrace in the serial log.

What this proves: the guest kernel's watchdog thread IS getting
scheduled, so we are NOT in a classic "kernel thread cannot run"
softlockup. Weakens (but does not fully rule out) the guest-kernel
spin-loop hypothesis — a guest-kernel path that repeatedly yields
or is preempted by timer IRQ between iterations could still spin
without tripping this detector. Hard lockup detection (NMI) would
need `nmi_watchdog=1 hardlockup_panic=1` to rule that path out too.

Leading hypothesis is now **guest userspace hot-loop** — consistent
with:

- Guest-agent (PID 1) snapshotted in state `S` during the stall
  (sleeping, not running). The spinning work is therefore happening
  in *some other* guest process, likely a child spawned by the
  exec loop.
- Host thread `vcpu-0` at 99% CPU while the guest kernel's
  watchdog remains happy.

Not settled yet — still "leading" until we identify the running
process inside the guest.

### 2026-04-23 update: new facts, revised framing

Three independent diagnostic attempts went in today. Summarised so
the next investigator doesn't retrace them:

**1. Guest-agent watchdog extended to enumerate `/proc/*/stat` (see
`dump_all_task_stacks` in `guest-agent/src/main.rs`).** Ran the
`persistent_channel_serial_exec_many` reproducer with `vcpus: 1`,
`timeout: None`, `SERIAL_EXEC_COUNT = 100`, and
`GuestConsoleSink::File`. Stall reproduces at iter ~21 every time.
**The watchdog never emits output** — `/dev/console` froze at
~23.5 KB and did not advance in 5+ minutes of stall. So the
intended next step ("identify the R-state PID") did not produce
evidence via this channel.

**2. Guest kernel lockup detectors armed.** Added
`softlockup_panic=1 watchdog_thresh=5 nmi_watchdog=1` to the kernel
cmdline. Boot log confirmed `NMI watchdog: Enabled. Permanently
consumes one hw-PMU counter.` **Neither softlockup nor NMI
hard-lockup fired** across a 5+ minute stall. So guest-kernel
hard/soft lockup in the classical sense is ruled out. The guest
kernel is still receiving timer IRQs and scheduling its own
watchdog kthread — something between "kernel alive" and "userspace
producing output" is what's stuck.

**3. `perf kvm --guest stat live --event=ioport` during stall.**
Under the stall condition the guest's only PIO traffic is:

| IO Port | Dir | Samples/s | Device |
|---|---|---|---|
| `0x40` | POUT | 118 | i8253 PIT channel-0 data |
| `0x21` | POUT | 88 | i8259 master PIC mask |
| `0x20` | POUT | 44 | i8259 master PIC command (EOI) |
| `0x21` | PIN | 44 | i8259 master PIC mask read |

**Zero writes to the serial port (0x3F8).** The guest does not even
attempt serial output during the stall — which is why the watchdog
thread's writes to `/dev/console` never show up on the host. The
8259/PIT pattern is consistent with the regular guest timer tick
running at ~44 Hz (one EOI + one mask r/w + PIT reprogram per
tick), not with an IRQ storm — so this is **not a PIC livelock** as
originally framed.

### Supporting context: virtual-wire mode

The slim-kernel VM boots with no ACPI MADT and no MP tables. Boot
log shows:

```
APIC: ACPI MADT or MP tables are not detected
APIC: Switch to virtual wire mode setup with no configuration
Not enabling interrupt remapping due to skipped IO-APIC setup
```

All virtio-mmio IRQs (including vsock on IRQ 11) go through the
legacy 8259, not the IO-APIC. This is a valid Linux configuration
but it does mean the guest's IRQ-processing fast path is different
from what a typical ACPI-equipped KVM guest would use — relevant
mostly because it changes what registers the guest accesses when
dispatching device IRQs.

### What the three experiments combined actually prove

- **Guest-kernel is alive.** Timer IRQs deliver, scheduler runs,
  watchdog kthread runs (else NMI watchdog fires).
- **Dispatch thread makes no syscalls.** No serial I/O, no virtio
  queue kicks, no vsock traffic, no MMIO at all — host-visible
  guest syscall activity is effectively zero.
- **Guest-agent's watchdog thread is not producing output** even
  though kernel scheduling is healthy. Two candidates remain:
  1. The watchdog thread is scheduled but its writes to
     `/dev/console` are swallowed by a dead path (e.g. serial line
     discipline flush blocked on kernel state we haven't
     instrumented), or
  2. The dispatch thread is in pure userspace holding some
     userspace lock that the watchdog's `kmsg() → open("/dev/kmsg")`
     transitively waits on.

The "IRQ livelock" interpretation (written earlier in this update)
is retracted — the 44 Hz rate is normal scheduler tick, not a
pathological loop. Apologies to the next reader for the earlier
overclaim.

### Leading hypothesis (revised)

**G6. Guest-agent dispatch thread is in a pure-userspace tight loop
in the `execute_command` OCI gate window, between the "Exec start"
and "Exec gate passed" kmsgs.** The guest kernel is fine; the
dispatch thread never makes another syscall, so nothing reaches the
host. The watchdog thread is either starved (tight-loop wins CPU on
every scheduler slice) or its `kmsg()` path depends on shared
userspace state held by the dispatch thread.

This is the same leading hypothesis as before today — today
strengthens it (guest kernel confirmed healthy, no device I/O from
guest). "G5 IRQ livelock" is retracted.

### Cheapest actionable next steps

1. **Make the watchdog survive a dispatch-thread userspace hang.**
   The current `dump_all_task_stacks` uses
   `std::fs::File::open("/dev/console")`, which drops into the
   guest kernel's serial tty path. A cleaner probe for this
   scenario is a direct `outb(0x3F8)` per-byte from the watchdog
   thread (or the `outb(0x80)` POST debug port), bypassing the tty
   layer entirely. If bytes appear on serial when they currently do
   not, G6 is validated and the culprit is the tty path, not
   scheduling. If still no bytes, the watchdog thread itself is
   starved and G6 narrows to a scheduling/userspace-lock question.
2. **Instrument the dispatch thread's own PID spin.** Add a
   loop-counter atomic incremented from inside
   `execute_command` *after* "Exec start" kmsg but before
   "Exec gate passed" — the watchdog can read it from another
   thread via `load(Relaxed)`, confirming whether the dispatch
   thread is genuinely spinning vs. blocked.
3. **Try with MADT/MP tables provided**, purely as a
   counter-factual. If the stall disappears with IO-APIC routing,
   there's still a guest-userspace bug, but the triggering cadence
   depends on 8259 timing (e.g. specific IRQ dispatch latency).
4. **If the stall persists under every variant above**, rebuild
   the guest-agent with `debug_assertions` and a `trace!` at the
   top of every function inside `execute_command`, routed through
   a lock-free in-memory ringbuffer readable from another thread.
   That localizes the spin to a specific Rust line.

### `sh` is not the spinner

Replaced the test's `sh -c "echo ..."` with a direct `echo ...`
invocation. The stall reproduces identically (same iter ballpark,
same vcpu-0@99% / everything else sleeping pattern). This rules out
busybox `sh` as the source. The bug therefore lies in either
guest-agent's own fork/execvp/wait/stream_pipe lineage, or the
direct `echo` child process, or the interaction of the two.

### Cheapest actionable next step

1. **Enumerate `/proc/*/stat` inside the guest during stall** to
   identify which PID is in state `R` while guest-agent (PID 1) is
   in state `S`. The name behind that PID tells us whether the
   spin is in `echo` / another child, in a guest-agent helper
   thread, or in something unexpected. Add to the guest-agent
   stall watchdog (commit `764535a`): walk `/proc/*/stat`, dump
   any entry with state `R` to `/dev/console` alongside the PID-1
   task stacks it already captures.
2. **Depending on that result, run one of:**
   - If the `R` process is `echo` or another child → `perf kvm
     record` with guest-userspace symbols for that binary.
   - If the `R` process is guest-agent itself → narrow in on
     which thread and which syscall by instrumenting the
     relevant path.
   - If `/proc` shows no `R` process but CPU is burning at 99%
     → reopen the guest-kernel hypothesis with
     `nmi_watchdog=1 hardlockup_panic=1` and rerun; hard-lockup
     detection covers cases softlockup misses.
3. **If `perf kvm record`/`report` file-format errors recur** (we
   hit this earlier), try `perf kvm top -p <voidbox-pid>
   --guestvmlinux=...` — live mode tends to bypass the on-disk
   format mismatch.

(Earlier "Where to look next" speculation about FD/thread
fragmentation and Rust `Command` global state has been folded into
the ruled-out list above — guest-agent's `threads/fds/rss/zombies`
were proven flat across 15 iters, so those specific mechanisms are
not candidates. The `R`-process enumeration described above is the
strictly cheaper path to the actual culprit.)

H5 text below kept for historical reference — superseded.

---

## Remaining hypotheses (guest-side)

### G1. `/proc/cmdline` read stalls under repeated access — RULED OUT 2026-04-23

`oci_rootfs_requested()` was calling `std::fs::read_to_string("/proc/cmdline")`
**per exec**. Hypothesis: repeated `open → read → close` on a procfs
file under tight PID-1 pressure stalls.

**Experiment (branch `experiment/guest-stall-g1-cache`):** wrap the
result in `OnceLock<bool>` so subsequent calls return a cached value
without touching procfs.

```rust
fn oci_rootfs_requested() -> bool {
    static CACHED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| { /* read & parse once */ })
}
```

**Result:** `persistent_channel_serial_exec_many` with
`SERIAL_EXEC_COUNT = 100` still stalls at iter ~21 (previously ~28 —
within noise). `/proc/cmdline` churn is not the cause.

The cache is still a strict correctness/perf improvement (immutable
file, eliminates ~2 syscalls per exec) and is worth keeping
independently, but it does not fix the stall.

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

## 2026-04-23 resolution

**Status:** Fixed in `guest-agent::kmsg()`.

The root cause was not vsock flow control, the OCI gate, or a guest
kernel livelock. It was the extra `eprintln!()` inside `kmsg()`:

```rust
pub(crate) fn kmsg(msg: &str) {
    eprintln!("{}", msg);
    if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open("/dev/kmsg") { ... }
}
```

That dual write sent every milestone log through both:

1. guest-agent stderr -> guest tty/serial path
2. `/dev/kmsg` -> kernel printk -> serial console

Under tight persistent-channel exec loops, the stderr/tty path
accumulated state and eventually blocked forever in `write(2)`.
Because `kmsg()` is called from the dispatch loop, one blocked
`eprintln!` parked PID 1 mid-iteration, the guest went idle
(`pv_native_safe_halt` on the host side), and the host observed it as
a stalled vsock RPC.

Removing the stderr write and keeping only `/dev/kmsg` resolves the
stall. `/dev/kmsg` already provides serial visibility through kernel
printk, so no diagnostic output is lost.

Validated with the persistent-channel repro at `SERIAL_EXEC_COUNT =
100`: all 100 execs completed successfully in 7.86 s with no stall.

### What did not fix it

- `KVM_IRQFD` changes for vsock
- milestone atomic / `mark_milestone`
- `iopl` / `/dev/port` experiments

### What stays

- the stall watchdog / task-stack dump path, which remains useful
  diagnostic tooling for future guest-side hangs

## History / dead ends

- **2026-04-21 spec (this file, original):** theorized userspace-vsock
  flow-control bugs (H1–H4). All ruled out by 2026-04-22 evidence. Kept
  in git history for reference.
- **2026-04-22 attempted fixes (reverted):** proactive CreditUpdate
  after every host-consumed RW + AF_UNIX partial-write buffering with
  `rx_pending`. Both landed briefly on `feat/perf-2-on-main`, both
  reverted after kmsg trace showed the stall is guest-side, not vsock.
- **2026-04-22 workaround (kept):** `14b3840` — Vhost branching.
