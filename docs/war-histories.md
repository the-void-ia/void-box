# VoidBox War Histories

Post-mortem retrospectives of debugging sessions that were expensive
enough to be worth remembering. Each entry follows the same shape:

- **Problem** — user-visible symptom
- **Wrong turns** — hypotheses we chased that turned out to be wrong
- **Decisive evidence** — what finally pinpointed the cause
- **Fix** — the actual change
- **Lessons** — generalizable takeaways

Entries are intentionally retrospective — not a real-time
investigation log. They exist so the next investigator recognises
the pattern instead of rediscovering it.

---

## 2026-04-21 — The `eprintln!` tty-backpressure stall

**Time to find:** ~2 days. **Size of the fix:** 1 line deleted.

### Problem

Under the persistent multiplex control channel, the
`persistent_channel_serial_exec_many` integration test stalled
deterministically around iteration 20–28 of a tight RPC loop:

```
test persistent_channel_serial_exec_many ...
panicked at tests/persistent_channel.rs:135:33:
exec 28 failed: Guest communication error: exec timed out after 30s
```

The guest VM went quiet — no further serial output, no vsock
response — and the host's `multiplex-reader` thread blocked in
`libc::read(vsock_fd)` forever. Only the userspace-vsock backend was
affected; vhost-vsock never reproduced it. Iteration count at stall
scaled **inversely** with kmsg-count-per-iteration: baseline ~28,
instrument-heavier ~12, heaviest ~6.

### Wrong turns

| Hypothesis | Why it was wrong |
|---|---|
| Userspace-vsock credit-flow bug (H1-H4 in the original plan) | Traced `fwd_cnt`, re-queue logic, partial-write accounting. All vsock state stayed healthy at stall. |
| Stall *between* `Exec start` and `Exec gate passed` kmsgs | Miscount — both messages appeared N times; stall was *after* `Exec gate passed`, not before. |
| Guest-userspace hot-loop inside `execute_command` | `KVM_GET_REGS` sampled 6 times during stall — **every** sample returned RIP in `pv_native_safe_halt`. Guest was idle, not spinning. |
| Guest-kernel IRQ livelock (8259 PIC in virtual-wire mode) | Only PIO during stall was the normal 44 Hz scheduler tick. Softlockup + NMI watchdogs both silent across 5+ min. |
| IRQ injection cadence (userspace assert+deassert pulse) | Tested by switching to `KVM_IRQFD` — reproduced identically, same iter, same timing. IRQ plumbing exonerated. |

Two days of incorrect framing. The shipped workaround during that
time (`KvmBackend` cold-boots with vhost-vsock by default, commit
`14b3840`) only masked the bug via timing jitter; it did not fix it.

### Decisive evidence

1. **Guest RIP landed at `pv_native_safe_halt` across 6 back-to-back
   samples.** Guest was idle, meaning *every* guest task was blocked.
2. **The stall was bracketed by two consecutive `kmsg()` calls** —
   `kmsg("post-wait")` emitted on console, `kmsg("pre-join-stdout")`
   never did, with essentially zero user code between them.
3. **Stall iteration scaled with kmsg count per iteration** (28 →
   21 → 8 → 6 as we added instrumentation). That is the signature
   of a cumulative-state bug in the logging path, not in the RPC
   plumbing.

The synthesis: if the guest is idle (1), and the thing blocking all
tasks is bracketed by two `kmsg()` calls (2), and the failure scales
with log volume (3), then the stall is in a syscall `kmsg()` makes.

### Root cause

`kmsg()` historically wrote every message on **two** paths:

1. `eprintln!(msg)` → guest-agent's stderr (fd 2) → `/dev/console`
   (serial tty)
2. `open("/dev/kmsg") → writeln! → close` → kernel printk ring
   buffer → serial console

The stderr path goes through `tty_write` →
`wait_event_interruptible(write_room > 0)` inside the guest kernel's
n_tty line discipline. Under the persistent multiplex channel,
`handle_connection` processes requests back-to-back with no
reconnect — the dispatch thread logs many messages in rapid
succession on a single thread. After ~20–28 iterations the guest
kernel's n_tty write path runs out of `write_room`, and the next
`eprintln!` call blocks inside `write(2)` indefinitely. With only
1 vCPU and all other guest tasks sleeping, the scheduler halts the
CPU at `pv_native_safe_halt` — which is exactly what we saw.

The `/dev/kmsg` path does **not** have this property: kernel printk
uses its own ring buffer with a drop-based rate-limiter, and serial
drain runs on a separate work queue not subject to the per-fd
`write_room` gate.

### Fix

One line removed:

```diff
 pub(crate) fn kmsg(msg: &str) {
-    eprintln!("{}", msg);
     if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open("/dev/kmsg") {
         use std::io::Write;
         let _ = writeln!(f, "guest-agent: {}", msg);
     }
 }
```

`/dev/kmsg` alone puts bytes on the serial console via kernel
printk — the stderr path added no extra visibility, only load.
Commit: `fix(guest-agent): drop eprintln from kmsg() — tty
deadlock under multiplex`.

Validation:
- `persistent_channel_serial_exec_many` with `SERIAL_EXEC_COUNT = 100`,
  `timeout: None`, vcpus 1, userspace vsock: **100 execs in 7.86 s**.
- `persistent_channel_serial_exec_many` + `persistent_channel_concurrent_exec`
  at stock config (16 execs, 30 s timeout, vcpus 2): passed in 12.89 s.

### Lessons

1. **Never dual-write the same log message to stderr + /dev/kmsg
   from hot paths.** `/dev/kmsg` is the correct sink for PID-1 /
   long-running-service log output because printk has built-in
   drop-based rate-limiting. The stderr path goes through an n_tty
   line discipline that *blocks* when its buffer fills. Codified as
   a `rust-style` skill rule against `println!` / `eprintln!` in
   library and service code.

2. **"Iteration-at-stall scales with work-per-iteration" is the
   signature of a cumulative-state bug.** If halving the log volume
   doubles `N`, the bug is in the logging path, not in the RPC
   plumbing. Notice this signature early; it saves days.

3. **`KVM_GET_REGS` via an attached gdb is the fastest way to find
   out what a "stuck" guest is actually doing.** Host-side `perf
   kvm` hit multiple dead ends on Fedora + AMD. A gdb script that
   calls `ioctl(vcpu_fd, KVM_GET_REGS, &buf)` on the VMM inferior
   and prints `rip` — six samples in under a minute — is worth
   more than an afternoon of `perf` format archaeology. Captured
   as a runbook entry in
   `docs/superpowers/plans/2026-04-23-guest-observability-alternatives.md`.

4. **`perf kvm --guest stat live --event=ioport`** reveals what a
   seemingly-stuck guest's vmexit pattern is. No file format
   headaches — runs continuously, prints per-port histograms. If
   the only PIO activity is 8259/PIT at tick rate, the guest is
   idle, not livelocked. Same runbook.

5. **Workarounds can mask the cause so thoroughly you stop looking
   for it.** The vhost-vsock default (commit `14b3840`) made every
   real agent workload pass by accident — vhost's in-kernel IRQ
   injection has slightly different per-iter timing that let the
   tty work-queue breathe. For six weeks that workaround kept the
   product shipping and the bug unsearched. When shipping a
   workaround, leave *exactly one* test configured to exercise the
   un-workaround'd path, as an early warning that the underlying
   bug still exists.

### See also

- `docs/superpowers/plans/2026-04-23-guest-observability-alternatives.md`
  — debugging-tools runbook section distilled from this
  investigation (KVM_GET_REGS via gdb, perf kvm stat ioport,
  /dev/port POST debug, outb milestone markers, log-count-vs-iter
  scaling heuristic).
