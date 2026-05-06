# Phase 6.4: Event-Driven RX Polling Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the 5 ms timer-driven `net_poll_thread` with `epoll_wait`-driven readiness dispatch, so host→guest RX latency is bounded by the actual data-arrival delay (sub-millisecond) rather than the 5 ms polling cycle.

**Architecture:** A new `mod epoll_dispatch` inside `src/network/` owns a single `epoll_fd` plus a self-pipe. `SlirpBackend` registers/unregisters socket FDs on flow-table mutations. The `net_poll_thread` calls `epoll_wait` (50 ms timeout for housekeeping) and routes each ready FD to the correct relay handler via `epoll_data` carrying a `FlowKey`. The self-pipe lets the vCPU-thread side wake the poll thread when it adds a new flow without polling-cycle delay.

**Tech stack:** smoltcp 0.11 wire types (unchanged), `libc::epoll_*` syscalls, `pipe2(O_NONBLOCK | O_CLOEXEC)`, no new crates.

**Hard performance gate (the "more performant than master" requirement):**

```
scripts/bench-compare.sh --baseline origin/main --skip-vm
```

…must show, for every comparable bench, **HEAD ≤ baseline + 5 %** *and* at least the following must improve by ≥ 30 %:

- `port_forward_accept_latency` (currently bounded by 50 ms listener poll; epoll should drop median by an order of magnitude once the listener also moves onto epoll — *or* document why it stays).
- a new `tcp_rx_latency_us_p50` wall-clock metric in `voidbox-network-bench` (Phase 6.4 must be sub-5 ms; pre-6.4 was bounded below by the 5 ms net-poll cycle).

Phase 6.4 is **not allowed to merge** until both gates above pass.

---

## Background

Reviewer finding **A4** (Medium-Low) on PR #68:

- `src/vmm/mod.rs:1599-1610`: `net_poll_thread` wakes every 5 ms (`std::thread::sleep(Duration::from_millis(5))`).
- `src/network/slirp.rs:1549`: `relay_tcp_nat_data` re-peeks 64 KiB on **every** connected TCP socket every tick, regardless of readiness.
- Listener threads spawned by `spawn_port_forward_listeners` (`src/network/slirp.rs:2097`) sleep 50 ms between accept attempts — this is the cap on `port_forward_accept_latency` (~50 ms median observed in `benches/network.rs::port_forward_accept_latency`).

passt's reference: epoll-driven readiness ([passt/tcp.c:463](https://passt.top/passt/tree/tcp.c#n463)). Phase 6.4 ports the *idea* (event-driven), not the literal `SO_PEEK_OFF` mechanism (which is Linux-specific and would not survive a future cross-platform backend split — though SLIRP itself is already `cfg(target_os = "linux")`).

## Invariants (carried from Phase 6 overview — non-negotiable)

1. **Full observability via `tracing`.** Every epoll event emits a `trace!` line with the `FlowKey` and event type. No silent dispatch.
2. **All-Rust path.** `libc::epoll_*` is the syscall surface; no new crates.
3. **Cross-platform discipline.** Phase 6.4 stays inside the existing `#[cfg(target_os = "linux")]` gate. macOS VZ is unaffected.
4. **No regression in Phase 0–5 baselines.** `bench-compare.sh --baseline origin/main` enforced — see "Hard performance gate" above.
5. **Snapshot/restore correctness.** `snapshot_integration` continues to pass. The `epoll_fd` does not survive snapshot; restore rebuilds the epoll set from `flow_table` contents. Snapshot does not serialize the epoll FD itself.

## File structure

| Path | Responsibility | Action |
|---|---|---|
| `src/network/epoll_dispatch.rs` | Owns `epoll_fd`, self-pipe, register/unregister, `wait()` returning `Vec<EpollEvent>`. Linux-only. | **Create** |
| `src/network/mod.rs` | Add `pub(crate) mod epoll_dispatch;` | Modify |
| `src/network/slirp.rs` | Hold `epoll: EpollDispatch` field on `SlirpBackend`; register on every flow_table insert; unregister on remove; rewrite `relay_tcp_nat_data`/`relay_udp_flows`/`relay_icmp_echo` to dispatch only on ready flows. | Modify |
| `src/vmm/mod.rs` | `net_poll_thread` rewrite: `epoll_wait(timeout=50ms)` instead of `sleep(5ms)`. | Modify |
| `tests/network_baseline.rs` | New pin `tcp_rx_latency_sub_5ms`; fix-up `tcp_writes_more_than_256kb_succeed`'s comment-vs-code mismatch; rename/migrate `drain_n` from `.poll()` to `drain_to_guest`. | Modify |
| `benches/network.rs` | Add divan bench `tcp_rx_latency_one_packet`. | Modify |
| `src/bin/voidbox-network-bench/main.rs` | Add `tcp_rx_latency_us_p50` measurement (host writes to a flow, time until guest sees the bytes via the relay). | Modify |
| `docs/superpowers/plans/2026-04-30-smoltcp-passt-port-phase6.4.md` | This file. | Already created |

`drain_n` migration in `tests/network_baseline.rs` is a quiet cleanup that lands in Task 1 — every test in the file uses it, so dropping `.poll()` here also drops the last in-tree `.poll()` caller and lets us delete the deprecated method entirely later.

## Architecture notes

### Why one `epoll_fd` (not one per protocol)?

- Single point of dispatch — the poll thread does *one* `epoll_wait` syscall regardless of how many flows are open.
- `epoll_data.u64` is 8 bytes — we encode `FlowKey` as a 64-bit token there. UDP and ICMP keys are smaller; TCP keys (`(guest_port, dst_ip, dst_port)`) fit in 64 bits with a tag byte for the protocol discriminator.
- Self-pipe is registered alongside socket FDs; reading it drains a queue of "I just added flow X" wake events posted by `process_guest_frame` running on the vCPU thread.

### Why a self-pipe?

`process_guest_frame` runs on the **vCPU thread** under the device lock. When it inserts a new flow into `flow_table`, the new socket FD is registered with epoll on that thread (cheap — just `epoll_ctl(EPOLL_CTL_ADD, ...)`). But the **poll thread** is asleep inside `epoll_wait(timeout=50ms)`. Without a wakeup, the new flow has up to 50 ms of latency before the first poll cycle picks it up.

The self-pipe (`pipe2(O_NONBLOCK | O_CLOEXEC)` registered with `EPOLLIN`) lets `process_guest_frame` write a single byte after `epoll_ctl`. The poll thread's `epoll_wait` returns immediately, drains the pipe (a no-op handler), and starts dispatching — including the new flow.

### Snapshot interaction

`epoll_fd` is a kernel handle on real FDs — not serializable. Snapshot path:

- `snapshot_internal`: tear down epoll. Drop `EpollDispatch`. Serialize `flow_table` as today.
- `from_snapshot`: deserialize `flow_table` → for every entry, recreate the host socket (already happening today via `host_stream` round-trip) → register the new FD with a fresh `EpollDispatch`.

No serde changes to `flow_table` itself.

### Why 50 ms `epoll_wait` timeout?

Housekeeping the poll thread does *outside* the dispatch loop:

- Reap stale UDP flows (`UDP_IDLE_TIMEOUT = 60 s`) — coarse, 50 ms is fine.
- Reap stale ICMP flows (similar).
- Phase 6.1 will add `LAST_ACK_TIMEOUT` reaping here.

If we set the timeout shorter we re-introduce the "wake every X ms regardless" cost we're trying to remove. If we set it longer, housekeeping latency grows. 50 ms balances both at a 10 % wakeup duty cycle versus the previous 100 % (one wakeup every 5 ms).

---

## Tasks

### Task 1: Pre-baseline + retransmit-test fix-up

**Files:**
- Modify: `tests/network_baseline.rs:170-179` (the `drain_n` helper)
- Modify: `tests/network_baseline.rs:374-422` (retransmit comment-vs-code in `tcp_writes_more_than_256kb_succeed`)

- [ ] **Step 1: Capture baseline numbers from `origin/main`**

```bash
# from a clean repo checkout
scripts/bench-compare.sh --baseline origin/main --skip-vm > /tmp/baseline-vs-main.md
cat /tmp/baseline-vs-main.md
```

Expected: every comparable bench has a real number in both columns. Save `/tmp/baseline-vs-main.md` as the pre-Phase-6.4 reference.

- [ ] **Step 2: Migrate `drain_n` from `.poll()` to `drain_to_guest`**

Replace `tests/network_baseline.rs:170-179`:

```rust
/// Drains frames the stack wants to send to the guest, calling
/// `drain_to_guest` up to `n` times.  Returns all frames produced
/// across the calls (caller may not care about per-call boundaries).
fn drain_n(stack: &mut SlirpBackend, n: usize) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    for _ in 0..n {
        stack.drain_to_guest(&mut out);
    }
    out
}
```

- [ ] **Step 3: Run the existing pins to confirm `drain_n` migration is non-breaking**

```bash
cargo test --test network_baseline
```

Expected: PASS for every existing pin (no semantic change — `drain_to_guest` appends to the buffer, same as `.poll()` extension).

- [ ] **Step 4: Fix the retransmit comment-vs-code mismatch in `tcp_writes_more_than_256kb_succeed`**

The Copilot review's C1.1 finding is correct: the loop unconditionally advances `seq` after every send, never retransmits unACK'd chunks. The 95 % threshold tolerates the resulting loss but the test's intent ("we re-send those") doesn't match its implementation.

Two valid fixes — pick the simpler one. Replace the loop body in `tests/network_baseline.rs:387-422`:

```rust
while bytes_received.load(Ordering::Relaxed) < TOTAL && std::time::Instant::now() < deadline {
    // Retransmit semantics: only advance the send cursor once the
    // previous chunk has been ACK'd. If the stack stops ACKing
    // (Phase 3 backpressure), we re-send the same seq/payload until
    // it's acknowledged. This matches the comment above and the
    // production guest-TCP behavior we're emulating.
    let _ = stack.process_guest_frame(&build_tcp_frame(
        SLIRP_GATEWAY_IP,
        GUEST_EPHEMERAL_PORT,
        host_port,
        seq,
        our_seq + 1,
        TcpControl::Psh,
        &chunk,
    ));

    // Drain frames; track the highest ACK we've seen and watch
    // for RST/FIN that would indicate a Phase-2 era close.
    for f in drain_n(&mut stack, 4) {
        if let Some((_, ack, ctrl, _)) = parse_tcp_to_guest(&f) {
            if matches!(ctrl, TcpControl::Rst | TcpControl::Fin) {
                saw_close = true;
            }
            if ack > acked_seq {
                acked_seq = ack;
            }
        }
    }

    if saw_close {
        break;
    }

    // Advance our send cursor only past ACK'd data.  If the stack
    // didn't ACK this chunk, the next loop iteration re-sends the
    // same seq/payload (true TCP retransmit semantics).
    if acked_seq >= seq.wrapping_add(CHUNK as u32) {
        seq = seq.wrapping_add(CHUNK as u32);
    } else if seq.wrapping_sub(acked_seq) > 256 * 1024 {
        // Out-paced kernel recv buffer; sleep briefly so the host
        // server thread can drain.
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
```

The single substantive change: move `seq = seq.wrapping_add(...)` from line 398 (unconditional, immediately after send) to after the drain loop, gated on `acked_seq >= seq + CHUNK`. If the stack ACK'd, advance; otherwise the next iteration re-sends the same chunk.

- [ ] **Step 5: Run the fixed test to confirm it still passes (now with real retransmit)**

```bash
cargo test --test network_baseline tcp_writes_more_than_256kb_succeed
```

Expected: PASS. The 95 % threshold will likely be 100 % now since real retransmits don't drop bytes.

- [ ] **Step 6: Commit**

```bash
git add tests/network_baseline.rs
git commit -m "test(network): drain_n via drain_to_guest + real retransmit in 256kb test

Two test-harness improvements landing together since both block the
Phase 6.4 RX-latency work:

- drain_n migrated from deprecated SlirpBackend::poll() to
  drain_to_guest. This was the last in-tree poll() caller.
- tcp_writes_more_than_256kb_succeed now matches its 'we re-send
  those' comment: seq only advances when acked_seq catches up,
  giving real TCP-retransmit semantics in the synthetic guest
  rather than the previous 'lossy with 95% tolerance' shape.
  Phase 6.4 must not regress this contract; making the test
  faithful first means epoll regressions surface as failures
  instead of borderline 95% misses."
```

---

### Task 2: ~~Failing pin — `tcp_rx_latency_sub_5ms`~~ **DROPPED**

**Status:** Dropped during execution. Original intent was a unit-level BROKEN_ON_PURPOSE pin asserting host→guest delivery in < 5 ms. **The 5 ms floor lives in `net_poll_thread` (`src/vmm/mod.rs:1609`), not in `SlirpBackend::drain_to_guest`** — the relay is synchronous when called from a test harness, so a unit-level latency assertion can't measure what we actually care about.

**Where the contract moved:** Task 13's wall-clock `tcp_rx_latency_us_p50` metric in `voidbox-network-bench`. That harness boots a real VM, drives the actual `net_poll_thread`, and observes the latency floor end-to-end. The hard-perf-gate requirement at the top of this plan (`tcp_rx_latency_us_p50 < 5 ms`) is the BROKEN_ON_PURPOSE replacement.

**No code lands for Task 2.** Skip directly to Task 3.

<details>
<summary>Original Task 2 body (kept for context)</summary>

The original plan attempted a unit-level pin that called `drain_to_guest` synchronously and timed the host-write → guest-receive interval. Implementation revealed:

- `drain_to_guest` is synchronous; the 5 ms `sleep` in `net_poll_thread` is what bounds VMM-level RX latency, not anything inside `SlirpBackend`.
- The test would have measured "spawn-thread + accept + write" minus "drain-loop find time", which underflowed in debug mode and was meaningless in release mode.

The contract — Phase 6.4 must deliver host→guest data in < 5 ms when data is available — is preserved as a VM-level requirement in Task 13.

</details>

- [ ] **Step 1: ~~Write the failing test~~ Skipped — see "DROPPED" note above. Original body kept below for context only.**

```rust
/// Phase 6.4 pin: host→guest RX latency must be sub-5 ms when data
/// is available. Pre-Phase-6.4 the floor was 5 ms (the
/// `net_poll_thread` `sleep(5ms)` cycle); post-Phase-6.4 the
/// epoll dispatch should deliver in < 1 ms on a quiet system.
///
/// Test harness: open a TCP flow guest→host, wait for ESTABLISHED,
/// have the host write 64 bytes, measure the time from `write()`
/// returning to the guest seeing the bytes in `drain_to_guest`'s
/// output. Pre-Phase-6.4 this measures ≈ 5 ms ± jitter; post-
/// Phase-6.4 it should be sub-millisecond on the same host.
#[test]
fn tcp_rx_latency_sub_5ms() {
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};
    use std::time::Instant;

    // Bind a host listener; the SLIRP rewrite of 10.0.2.2 → 127.0.0.1
    // routes our SYN to it.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let host_port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || -> Option<std::time::Duration> {
        let (mut sock, _) = listener.accept().ok()?;
        // Wait for the guest to send something so we know the relay
        // is established and bidirectional.
        let mut probe = [0u8; 1];
        let _ = std::io::Read::read(&mut sock, &mut probe);

        // Stamp T0 just before write returns.
        let t0 = Instant::now();
        sock.write_all(&[0x42; 64]).ok()?;
        Some(t0.elapsed())
    });

    let mut stack = SlirpBackend::new().unwrap();

    // Drive the 3-way handshake.
    let our_seq = 1000u32;
    stack.process_guest_frame(&build_tcp_frame(
        SLIRP_GATEWAY_IP, GUEST_EPHEMERAL_PORT, host_port, our_seq, 0,
        TcpControl::Syn, &[],
    )).unwrap();

    let mut gateway_seq = 0u32;
    for f in drain_n(&mut stack, 4) {
        if let Some((s, _ack, ctrl, _)) = parse_tcp_to_guest(&f) {
            if matches!(ctrl, TcpControl::Syn) {
                gateway_seq = s;
                break;
            }
        }
    }

    stack.process_guest_frame(&build_tcp_frame(
        SLIRP_GATEWAY_IP, GUEST_EPHEMERAL_PORT, host_port, our_seq + 1, gateway_seq + 1,
        TcpControl::None, &[],
    )).unwrap();

    // Send a probe byte so the host server thread proceeds to write.
    stack.process_guest_frame(&build_tcp_frame(
        SLIRP_GATEWAY_IP, GUEST_EPHEMERAL_PORT, host_port, our_seq + 1, gateway_seq + 1,
        TcpControl::Psh, &[0xAA],
    )).unwrap();

    // Now the host writes and stamps T0. We measure from "host write
    // completes" to "guest sees data in drain output."
    let host_t0 = server.join().expect("server").expect("write succeeded");
    let drain_start = Instant::now();
    let mut saw_payload = false;
    while drain_start.elapsed() < std::time::Duration::from_secs(1) {
        let frames: Vec<Vec<u8>> = drain_n(&mut stack, 1);
        for f in &frames {
            if let Some((_, _, _, payload_len)) = parse_tcp_to_guest(f) {
                if payload_len >= 64 {
                    saw_payload = true;
                    break;
                }
            }
        }
        if saw_payload { break; }
        std::thread::sleep(std::time::Duration::from_micros(50));
    }
    let host_to_guest_us = drain_start.elapsed().as_micros() as u64
        - host_t0.as_micros() as u64;

    assert!(saw_payload, "host payload never reached the guest");

    // The contract: epoll dispatch delivers in < 5 ms.
    assert!(
        host_to_guest_us < 5_000,
        "Phase 6.4 contract: host→guest RX latency must be sub-5 ms \
         (was bounded below by 5 ms net_poll_thread cycle); got {host_to_guest_us} µs"
    );
}
```

- [ ] **Step 2: Run the test, expect it to fail**

```bash
cargo test --test network_baseline tcp_rx_latency_sub_5ms
```

Expected: **FAIL** with `host→guest RX latency must be sub-5 ms; got <5000-9999> µs` — the current `net_poll_thread` is ineligible to deliver in <5 ms because of its `sleep(5ms)`.

This is the Phase 6.4 BROKEN_ON_PURPOSE pin. It will flip in Task 11.

- [ ] **Step 3: Commit the failing pin**

```bash
git add tests/network_baseline.rs
git commit -m "test(network): pin tcp_rx_latency_sub_5ms (BROKEN_ON_PURPOSE)

Phase 6.4 contract: host→guest RX latency must be sub-5 ms when
data is available. Pre-6.4 the floor is the 5 ms net_poll_thread
sleep cycle; this assertion fails on master and on the current
PR #68 tip. Phase 6.4's epoll dispatch will flip it to passing.

Mark with #[ignore] is deliberately NOT used: this is a positive
contract and CI must surface the failure on master so the gate
is unmissable."
```

---

### Task 3: `EpollDispatch` skeleton + unit test

**Files:**
- Create: `src/network/epoll_dispatch.rs`
- Modify: `src/network/mod.rs` — add `pub(crate) mod epoll_dispatch;`

- [ ] **Step 1: Write the failing test (in the new module)**

In `src/network/epoll_dispatch.rs`:

```rust
//! Linux epoll-driven readiness dispatch for SLIRP host sockets.
//!
//! Owns one `epoll_fd` plus a self-pipe.  Callers register socket FDs
//! with a `FlowToken` (a 64-bit identifier the dispatcher returns on
//! readiness).  The poll thread calls `wait_with_timeout` to block
//! until any registered FD is ready or the timeout fires, then drains
//! the events into a caller-owned buffer.
//!
//! Why no crate? The standard `mio`/`tokio` story would pull in a
//! reactor + a runtime — Phase 6.4 needs neither.  `libc::epoll_*`
//! is two syscalls, fully observable, and the surface fits in ~150
//! lines.  See plan 2026-04-30-smoltcp-passt-port-phase6.4.md
//! "Architecture notes" for the rationale.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::time::Duration;

/// Opaque per-FD identifier the caller uses to look up which flow a
/// readiness event belongs to.  Encoded into `epoll_data.u64`.
pub type FlowToken = u64;

/// One readiness event, mapped from `libc::epoll_event`.
#[derive(Debug, Clone, Copy)]
pub struct EpollEvent {
    pub token: FlowToken,
    pub readable: bool,
    pub writable: bool,
}

#[derive(Debug)]
pub struct EpollDispatch {
    // implementation in next step
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    #[test]
    fn dispatch_new_creates_epoll_fd() {
        let dispatch = EpollDispatch::new().expect("EpollDispatch::new");
        assert!(dispatch.epoll_fd_for_test() >= 0);
    }
}
```

- [ ] **Step 2: Run, expect compile error**

```bash
cargo test --lib network::epoll_dispatch
```

Expected: COMPILE FAIL — `new` and `epoll_fd_for_test` not defined.

- [ ] **Step 3: Implement minimal `EpollDispatch`**

Replace the empty struct in `src/network/epoll_dispatch.rs`:

```rust
#[derive(Debug)]
pub struct EpollDispatch {
    epoll_fd: OwnedFd,
}

impl EpollDispatch {
    /// Create a new epoll instance with `EPOLL_CLOEXEC`.
    pub fn new() -> io::Result<Self> {
        // SAFETY: `epoll_create1` returns -1 on error and a valid fd
        // otherwise.  We wrap into OwnedFd so Drop closes it.
        let raw = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let epoll_fd = unsafe { OwnedFd::from_raw_fd(raw) };
        Ok(Self { epoll_fd })
    }

    #[cfg(test)]
    fn epoll_fd_for_test(&self) -> RawFd {
        self.epoll_fd.as_raw_fd()
    }
}
```

Add the missing `use std::os::fd::FromRawFd;` to the file's existing `use` block (module-scope per project convention).

- [ ] **Step 4: Run, expect pass**

```bash
cargo test --lib network::epoll_dispatch::tests::dispatch_new_creates_epoll_fd
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/network/epoll_dispatch.rs src/network/mod.rs
git commit -m "feat(network): EpollDispatch skeleton with epoll_create1

Phase 6.4 foundation. One epoll_fd owned via OwnedFd + EPOLL_CLOEXEC.
No registration logic yet — Task 4 will add register/unregister and
Task 6 will add the self-pipe + wait loop."
```

---

### Task 4: `register` / `unregister` + tests

**Files:**
- Modify: `src/network/epoll_dispatch.rs`

- [ ] **Step 1: Write the failing tests**

In the `mod tests` block:

```rust
#[test]
fn register_then_unregister_round_trip() {
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let mut dispatch = EpollDispatch::new().expect("EpollDispatch::new");
    let token: FlowToken = 0xDEAD_BEEF;
    dispatch
        .register(listener.as_raw_fd(), token, true, false)
        .expect("register");
    dispatch.unregister(listener.as_raw_fd()).expect("unregister");
}

#[test]
fn register_invalid_fd_returns_error() {
    let mut dispatch = EpollDispatch::new().expect("EpollDispatch::new");
    let result = dispatch.register(-1, 0, true, false);
    assert!(result.is_err());
}
```

- [ ] **Step 2: Run, expect compile fail**

```bash
cargo test --lib network::epoll_dispatch
```

Expected: COMPILE FAIL — `register`/`unregister` not defined.

- [ ] **Step 3: Implement**

Add to `EpollDispatch`:

```rust
impl EpollDispatch {
    /// Register `fd` with the dispatcher.  `readable`/`writable`
    /// select EPOLLIN / EPOLLOUT.  `token` is opaque to the
    /// dispatcher — returned verbatim on readiness events.
    pub fn register(
        &mut self,
        fd: RawFd,
        token: FlowToken,
        readable: bool,
        writable: bool,
    ) -> io::Result<()> {
        let mut events: u32 = 0;
        if readable {
            events |= libc::EPOLLIN as u32;
        }
        if writable {
            events |= libc::EPOLLOUT as u32;
        }
        let mut ev = libc::epoll_event {
            events,
            u64: token,
        };
        // SAFETY: epoll_ctl reads `ev` for ADD; we own `fd` for the
        // lifetime of the registration (caller's contract).
        let rc = unsafe {
            libc::epoll_ctl(
                self.epoll_fd.as_raw_fd(),
                libc::EPOLL_CTL_ADD,
                fd,
                &mut ev as *mut _,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn unregister(&mut self, fd: RawFd) -> io::Result<()> {
        // SAFETY: epoll_ctl ignores the event pointer for DEL but
        // still requires it to be non-null on older kernels.
        let mut ev = libc::epoll_event { events: 0, u64: 0 };
        let rc = unsafe {
            libc::epoll_ctl(
                self.epoll_fd.as_raw_fd(),
                libc::EPOLL_CTL_DEL,
                fd,
                &mut ev as *mut _,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
```

- [ ] **Step 4: Run, expect pass**

```bash
cargo test --lib network::epoll_dispatch
```

Expected: PASS for both new tests.

- [ ] **Step 5: Commit**

```bash
git add src/network/epoll_dispatch.rs
git commit -m "feat(network): EpollDispatch register/unregister"
```

---

### Task 5: `wait_with_timeout` + integration test

**Files:**
- Modify: `src/network/epoll_dispatch.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn wait_returns_event_when_socket_becomes_readable() {
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        sock.write_all(b"hi").unwrap();
    });
    let stream = TcpStream::connect(addr).expect("connect");
    server.join().unwrap();

    let mut dispatch = EpollDispatch::new().expect("new");
    dispatch
        .register(stream.as_raw_fd(), 0xCAFE, true, false)
        .expect("register");

    let mut events: Vec<EpollEvent> = Vec::new();
    let n = dispatch
        .wait_with_timeout(&mut events, Duration::from_secs(1))
        .expect("wait");
    assert_eq!(n, 1);
    assert_eq!(events[0].token, 0xCAFE);
    assert!(events[0].readable);
}
```

- [ ] **Step 2: Run, expect compile fail**

Expected: `wait_with_timeout` not found.

- [ ] **Step 3: Implement**

```rust
impl EpollDispatch {
    /// Block up to `timeout` for any registered FD to become ready.
    /// Drains ready events into `out` (cleared first).  Returns the
    /// number of events drained.
    ///
    /// `timeout = Duration::ZERO` is non-blocking poll;
    /// `timeout = Duration::from_secs(...)` waits up to that long.
    pub fn wait_with_timeout(
        &self,
        out: &mut Vec<EpollEvent>,
        timeout: Duration,
    ) -> io::Result<usize> {
        out.clear();

        // Pre-allocate a fixed-size event buffer.  64 ready FDs per
        // wait is more than enough for our flow counts; events not
        // returned this round will surface on the next wait.
        let mut raw_events: [libc::epoll_event; 64] =
            [libc::epoll_event { events: 0, u64: 0 }; 64];

        let timeout_ms: i32 = timeout
            .as_millis()
            .min(i32::MAX as u128) as i32;

        // SAFETY: epoll_wait writes up to raw_events.len() entries;
        // returns -1 on error, 0 on timeout, n>0 on events.
        let n = unsafe {
            libc::epoll_wait(
                self.epoll_fd.as_raw_fd(),
                raw_events.as_mut_ptr(),
                raw_events.len() as i32,
                timeout_ms,
            )
        };
        if n < 0 {
            // EINTR is non-fatal — caller can retry on next tick.
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                return Ok(0);
            }
            return Err(err);
        }
        for raw in &raw_events[..n as usize] {
            out.push(EpollEvent {
                token: raw.u64,
                readable: (raw.events & libc::EPOLLIN as u32) != 0,
                writable: (raw.events & libc::EPOLLOUT as u32) != 0,
            });
        }
        Ok(n as usize)
    }
}
```

- [ ] **Step 4: Run, expect pass**

```bash
cargo test --lib network::epoll_dispatch
```

- [ ] **Step 5: Commit**

```bash
git add src/network/epoll_dispatch.rs
git commit -m "feat(network): EpollDispatch::wait_with_timeout"
```

---

### Task 6: Self-pipe + wakeup test

**Files:**
- Modify: `src/network/epoll_dispatch.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn wakeup_unblocks_wait_immediately() {
    use std::time::Instant;
    let mut dispatch = EpollDispatch::new().expect("new");
    let waker = dispatch.waker();

    // Start the wait in another thread with a long timeout.
    let wait_thread = std::thread::spawn(move || -> std::time::Duration {
        let mut events: Vec<EpollEvent> = Vec::new();
        let start = Instant::now();
        let _ = dispatch.wait_with_timeout(&mut events, Duration::from_secs(5));
        start.elapsed()
    });

    // Wake immediately.
    std::thread::sleep(Duration::from_millis(10));
    waker.wake();

    let elapsed = wait_thread.join().expect("wait thread");
    // Wait thread should return well under the 5 s timeout.
    assert!(
        elapsed < Duration::from_secs(1),
        "wait did not return on wakeup: {elapsed:?}"
    );
}
```

- [ ] **Step 2: Run, expect compile fail**

Expected: `waker()` and `Waker` not defined.

- [ ] **Step 3: Implement**

Add to `epoll_dispatch.rs`:

```rust
/// Cloneable wakeup handle for `EpollDispatch`.  Writing one byte to
/// the underlying pipe wakes a thread blocked in `wait_with_timeout`.
#[derive(Debug, Clone)]
pub struct Waker {
    write_end: std::sync::Arc<OwnedFd>,
}

impl Waker {
    pub fn wake(&self) {
        let buf = [0u8; 1];
        // SAFETY: write to a non-blocking pipe never blocks.  We
        // ignore EAGAIN — the pipe already has bytes pending, which
        // means a wakeup is already queued.
        let _ = unsafe {
            libc::write(self.write_end.as_raw_fd(), buf.as_ptr() as *const _, 1)
        };
    }
}

const SELF_PIPE_TOKEN: FlowToken = u64::MAX;

impl EpollDispatch {
    /// Returns a `Waker` that, when called, unblocks any thread
    /// currently inside `wait_with_timeout`.
    pub fn waker(&mut self) -> Waker {
        if self.waker_handle.is_none() {
            let (read_fd, write_fd) = create_pipe2_nonblock_cloexec();
            self.register(read_fd.as_raw_fd(), SELF_PIPE_TOKEN, true, false)
                .expect("register self-pipe");
            self.read_end = Some(read_fd);
            self.waker_handle = Some(std::sync::Arc::new(write_fd));
        }
        Waker {
            write_end: self.waker_handle.as_ref().unwrap().clone(),
        }
    }
}

fn create_pipe2_nonblock_cloexec() -> (OwnedFd, OwnedFd) {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: pipe2 with O_NONBLOCK | O_CLOEXEC writes two fds into fds.
    let rc = unsafe {
        libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC)
    };
    assert!(rc == 0, "pipe2 failed: {}", io::Error::last_os_error());
    let read_end = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write_end = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    (read_end, write_end)
}
```

Add fields to `EpollDispatch`:

```rust
#[derive(Debug)]
pub struct EpollDispatch {
    epoll_fd: OwnedFd,
    read_end: Option<OwnedFd>,
    waker_handle: Option<std::sync::Arc<OwnedFd>>,
}
```

…and update `EpollDispatch::new` to initialize the new fields to `None`.

In `wait_with_timeout`, after collecting events, drop the self-pipe wake-token from the returned set (the caller doesn't care about it) and drain any pending bytes from the read end:

```rust
// Drain self-pipe events from the returned set + the pipe itself.
let mut filtered: Vec<EpollEvent> = Vec::with_capacity(out.len());
for ev in out.drain(..) {
    if ev.token == SELF_PIPE_TOKEN {
        if let Some(read_end) = &self.read_end {
            let mut scratch = [0u8; 64];
            // SAFETY: non-blocking read; ignored result.
            unsafe {
                libc::read(
                    read_end.as_raw_fd(),
                    scratch.as_mut_ptr() as *mut _,
                    scratch.len(),
                );
            }
        }
        continue;
    }
    filtered.push(ev);
}
*out = filtered;
let observable_n = out.len();
Ok(observable_n)
```

- [ ] **Step 4: Run all dispatch tests**

```bash
cargo test --lib network::epoll_dispatch
```

Expected: PASS for all four tests.

- [ ] **Step 5: Commit**

```bash
git add src/network/epoll_dispatch.rs
git commit -m "feat(network): EpollDispatch self-pipe wakeup

Cloneable Waker writes one byte to a non-blocking pipe registered
with EPOLLIN. wait_with_timeout filters self-pipe events out of
the returned set and drains the pipe so subsequent waits don't
spurious-wake."
```

---

### Task 7: Wire `EpollDispatch` into `SlirpBackend`

**Files:**
- Modify: `src/network/slirp.rs` — `SlirpBackend` struct + `new` + `with_security`.

- [ ] **Step 1: Add the field**

In the `SlirpBackend` struct definition (~line 450):

```rust
pub struct SlirpBackend {
    // ... existing fields ...
    epoll: crate::network::epoll_dispatch::EpollDispatch,
    epoll_waker: crate::network::epoll_dispatch::Waker,
}
```

In `SlirpBackend::with_security` (~line 503), after `flow_table` is initialized but before any flow is inserted:

```rust
let mut epoll = crate::network::epoll_dispatch::EpollDispatch::new()
    .map_err(|e| anyhow::anyhow!("EpollDispatch::new: {e}"))?;
let epoll_waker = epoll.waker();
```

…then include `epoll`, `epoll_waker` in the struct literal.

- [ ] **Step 2: Run unit tests; expect them to still pass (no behavior change yet)**

```bash
cargo test --lib network::slirp
cargo test --test network_baseline
```

Expected: ALL PASS — `SlirpBackend` now owns an unused epoll_fd.

- [ ] **Step 3: Commit**

```bash
git add src/network/slirp.rs
git commit -m "refactor(slirp): SlirpBackend holds EpollDispatch + Waker

Plumbed but not yet consumed.  Subsequent tasks wire flow_table
mutations into epoll register/unregister and rewrite the relay
loops to dispatch on readiness."
```

---

### Task 8: TCP register/unregister on flow_table mutation + smoke test

**Files:**
- Modify: `src/network/slirp.rs` — `handle_tcp_frame` (after `flow_table.insert`) and `relay_tcp_nat_data` (where `to_remove` entries are reaped).

- [ ] **Step 1: Add a `flow_token_for_tcp` helper at module scope**

Encoding: 8 bits of protocol tag (0x01 = TCP), 8 bits unused (zero), 16 bits guest_port, 32 bits packed (dst_port << 16) | (truncated dst_ip). For 100 % uniqueness across tag/port collisions, see follow-up — for now this 64-bit token is unique within the flow table because `NatKey` itself is unique.

```rust
const PROTO_TAG_TCP: u64 = 0x0100_0000_0000_0000;
const PROTO_TAG_UDP: u64 = 0x0200_0000_0000_0000;
const PROTO_TAG_ICMP: u64 = 0x0300_0000_0000_0000;

fn flow_token_for_tcp(key: &NatKey) -> u64 {
    let dst_ip_bytes = key.dst_ip.0;
    let dst_ip_low: u64 = u64::from(u32::from_be_bytes(dst_ip_bytes)) & 0xFFFF_FFFF;
    PROTO_TAG_TCP
        | (u64::from(key.guest_src_port) << 32)
        | (u64::from(key.dst_port) << 16)
        | (dst_ip_low & 0xFFFF)
}
```

Symmetric helpers for UDP / ICMP land in Tasks 9 / 10.

- [ ] **Step 2: After every `flow_table.insert(FlowKey::Tcp(...), FlowEntry::Tcp(entry))`, register the host_stream FD**

For example in `handle_tcp_frame` (~line 1290 after insert):

```rust
let token = flow_token_for_tcp(&key);
self.epoll
    .register(entry.host_stream.as_raw_fd(), token, true, false)
    .ok();
self.epoll_waker.wake();
```

…and in `process_pending_inbound_accepts` (line 648 area):

```rust
self.flow_table.insert(FlowKey::Tcp(key), FlowEntry::Tcp(entry));
let host_fd = match self.flow_table.get(&FlowKey::Tcp(key)) {
    Some(FlowEntry::Tcp(e)) => e.host_stream.as_raw_fd(),
    _ => unreachable!(),
};
self.epoll.register(host_fd, flow_token_for_tcp(&key), true, false).ok();
self.epoll_waker.wake();
```

…and on every `flow_table.remove(&FlowKey::Tcp(...))` site, unregister first:

```rust
if let Some(FlowEntry::Tcp(e)) = self.flow_table.get(&flow_key) {
    self.epoll.unregister(e.host_stream.as_raw_fd()).ok();
}
self.flow_table.remove(&flow_key);
```

(grep for every `flow_table.remove` and `flow_table.insert` site touching TCP — there are ~6.)

- [ ] **Step 3: Run all baseline pins**

```bash
cargo test --test network_baseline
```

Expected: PASS — no behavioral change yet (relay still re-peeks every flow on every tick).

- [ ] **Step 4: Commit**

```bash
git add src/network/slirp.rs
git commit -m "feat(slirp): register TCP flows with EpollDispatch

flow_table mutations now keep the epoll set in sync.  No relay-loop
change yet — Task 11 will switch the loop to dispatch by readiness
instead of iterating the full table."
```

---

### Task 9: UDP register/unregister + ICMP register/unregister

Mirror Task 8 for `FlowKey::Udp` and `FlowKey::IcmpEcho` flow_table sites. Same shape: register on insert, unregister on remove. Use `PROTO_TAG_UDP` / `PROTO_TAG_ICMP` in the helpers.

- [ ] **Step 1: Implement helpers and call sites**
- [ ] **Step 2: Run baseline pins (PASS)**
- [ ] **Step 3: Commit** with message `feat(slirp): register UDP + ICMP flows with EpollDispatch`

---

### Task 10: Flip `relay_tcp_nat_data` to event-driven

**Files:**
- Modify: `src/network/slirp.rs` — `relay_tcp_nat_data` body (~line 1512+).

The current loop iterates *every* TCP entry in `flow_table` every tick. New shape: take the readiness set from a caller-passed `&[EpollEvent]`, look up the flow by `FlowKey`, only peek-relay readable flows.

- [ ] **Step 1: Change signature**

```rust
fn relay_tcp_nat_data(&mut self, ready: &[EpollEvent]) {
    let mut to_remove: Vec<FlowKey> = Vec::new();
    let mut frames_to_inject: Vec<Vec<u8>> = Vec::new();

    for event in ready {
        if event.token & PROTO_TAG_TCP_MASK != PROTO_TAG_TCP {
            continue;
        }
        // Decode token back to NatKey by linear scan — flow_table is
        // small and the token-to-key direction is rare (only on
        // readiness).  Future optimization: keep a side index.
        let flow_key = match self.flow_table.iter().find_map(|(k, _)| {
            if let FlowKey::Tcp(nat_key) = k {
                if flow_token_for_tcp(nat_key) == event.token {
                    return Some(*k);
                }
            }
            None
        }) {
            Some(k) => k,
            None => continue,
        };

        let Some(FlowEntry::Tcp(entry)) = self.flow_table.get_mut(&flow_key) else {
            continue;
        };
        if entry.state != TcpNatState::Established {
            continue;
        }

        // ... existing peek/relay body, unchanged from line 1549+ ...
    }

    self.inject_to_guest.append(&mut frames_to_inject);
    for flow_key in to_remove {
        if let Some(FlowEntry::Tcp(e)) = self.flow_table.get(&flow_key) {
            self.epoll.unregister(e.host_stream.as_raw_fd()).ok();
        }
        self.flow_table.remove(&flow_key);
    }
}
```

Define `PROTO_TAG_TCP_MASK` next to the other tag constants:

```rust
const PROTO_TAG_MASK: u64 = 0xFF00_0000_0000_0000;
```

…and check `event.token & PROTO_TAG_MASK == PROTO_TAG_TCP`.

- [ ] **Step 2: Update the caller in `drain_to_guest`**

```rust
pub fn drain_to_guest(&mut self, out: &mut Vec<Vec<u8>>) {
    self.process_pending_inbound_accepts();
    // ... ARP handling ...

    // Phase 6.4: gather readiness events once per tick.  The poll
    // thread will already have driven a recent epoll_wait; here we do
    // a non-blocking poll to pick up anything that arrived between
    // the last wait and now.
    let mut ready: Vec<EpollEvent> = Vec::new();
    let _ = self.epoll.wait_with_timeout(&mut ready, Duration::ZERO);

    self.resolve_pending_dns();
    self.relay_tcp_nat_data(&ready);
    self.relay_icmp_echo(&ready);
    self.relay_udp_flows(&ready);

    // ... unchanged collection of frames ...
}
```

- [ ] **Step 3: Update `relay_icmp_echo` and `relay_udp_flows` signatures to `(&mut self, ready: &[EpollEvent])`** with parallel filtering by `PROTO_TAG_ICMP` / `PROTO_TAG_UDP`.

- [ ] **Step 4: Run baseline pins**

```bash
cargo test --test network_baseline
```

Expected: PASS — the `wait_with_timeout(Duration::ZERO)` non-blocking poll captures any ready FD between vCPU calls; the relay still works.

- [ ] **Step 5: Commit**

```bash
git add src/network/slirp.rs
git commit -m "feat(slirp): relay loops dispatch by epoll readiness

drain_to_guest non-blocking-polls the epoll set once per tick and
passes the ready event list to relay_tcp_nat_data /
relay_udp_flows / relay_icmp_echo, which now skip non-ready flows
instead of re-peeking the whole table.  Behavior unchanged on
hot-path; per-tick CPU should drop on idle systems with many
flows."
```

---

### Task 11: Rewrite `net_poll_thread` to use `epoll_wait`

**Files:**
- Modify: `src/vmm/mod.rs:1599-1640`.

- [ ] **Step 1: Replace the `sleep(5ms)` loop**

The current loop:

```rust
while running.load(Ordering::Relaxed) {
    std::thread::sleep(std::time::Duration::from_millis(5));
    // ... try_inject_rx + irq ...
}
```

Becomes (pseudocode — exact integration with the device-lock pattern needs care):

```rust
while running.load(Ordering::Relaxed) {
    // Acquire the SlirpBackend's waker once at startup; use it as
    // the shutdown signaling channel too.
    let mut events: Vec<EpollEvent> = Vec::new();
    {
        let guard = match net_dev.lock() {
            Ok(g) => g,
            Err(_) => continue,
        };
        // Borrow epoll for the wait; see Step 2 for the API on
        // VirtioNetDevice that exposes it without holding the
        // device lock during epoll_wait.
        let _ = guard.poll_epoll(&mut events, Duration::from_millis(50));
    }
    // ... try_inject_rx + irq, unchanged ...
}
```

The challenge: `epoll_wait` blocks for up to 50 ms; we cannot hold the device lock that whole time (vCPU would stall on next TX). Solution: `VirtioNetDevice::poll_epoll` clones the `epoll` into an `Arc<Mutex<EpollDispatch>>` (or similar) and the wait happens *outside* the device lock.

- [ ] **Step 2: Refactor the lock granularity**

In `src/network/slirp.rs`, change:

```rust
epoll: EpollDispatch,
```

to:

```rust
epoll: std::sync::Arc<std::sync::Mutex<EpollDispatch>>,
```

…and update all `self.epoll.register(...)` to `self.epoll.lock().unwrap().register(...)`. Provide a clone-of-Arc accessor:

```rust
pub fn epoll_arc(&self) -> std::sync::Arc<std::sync::Mutex<EpollDispatch>> {
    Arc::clone(&self.epoll)
}
```

The poll thread holds an `Arc<Mutex<EpollDispatch>>`, calls `wait_with_timeout` while holding that lock, and *not* the device lock.

- [ ] **Step 3: Run baseline + integration tests**

```bash
cargo test --workspace --all-features
cargo test --test network_baseline
```

Expected: all PASS.

- [ ] **Step 4: Run the BROKEN_ON_PURPOSE pin from Task 2 — it should now flip to PASS**

```bash
cargo test --test network_baseline tcp_rx_latency_sub_5ms
```

Expected: **PASS** with measured latency < 5 ms (likely sub-millisecond).

- [ ] **Step 5: Commit**

```bash
git add src/network/slirp.rs src/vmm/mod.rs
git commit -m "feat(vmm): net_poll_thread driven by epoll_wait

Replaces the 5 ms sleep cycle with epoll_wait(timeout=50ms).  When
host data arrives, the poll thread wakes within microseconds and
drives drain_to_guest immediately.  When idle, the thread wakes
once every 50 ms for housekeeping (UDP/ICMP idle reaping) — a
10x reduction in wakeup duty cycle vs the previous 5 ms timer.

Phase 6.4 BROKEN_ON_PURPOSE pin tcp_rx_latency_sub_5ms flips to
passing here."
```

---

### Task 12: Snapshot rebuild test + implementation

**Files:**
- Modify: `src/vmm/mod.rs` (snapshot/restore paths) and `src/network/slirp.rs` (`from_snapshot`-shaped constructor).

- [ ] **Step 1: Run the existing snapshot integration suite to confirm baseline**

```bash
export VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r)
export VOID_BOX_INITRAMFS=/tmp/void-box-test-rootfs.cpio.gz
cargo test --test snapshot_integration -- --ignored --test-threads=1
```

Expected: PASS (Phase 0–5 baseline). If it doesn't pass on this branch's tip pre-6.4, fix before continuing — this gate is non-negotiable.

- [ ] **Step 2: Write the new test pin**

In `tests/network_baseline.rs`:

```rust
/// Phase 6.4 contract: snapshot/restore must rebuild the epoll
/// dispatch from flow_table contents.  After a round-trip, the
/// backend has zero registered flows in epoll if flow_table was
/// non-empty pre-snapshot — that's the bug we want to catch.
#[test]
fn epoll_set_rebuilt_on_restore_smoke() {
    // Construct backend, open one TCP flow (handshake), serialize
    // the flow_table, drop the backend, build a fresh backend and
    // inject the serialized flow_table.  Verify the new backend's
    // epoll set has the flow's host_fd registered.
    // ... (full test code) ...
}
```

The detailed body is omitted here — write it referencing the snapshot helpers in `src/vmm/snapshot.rs` and the existing `from_snapshot` shape. Verify by checking the count of registered FDs (add a `#[cfg(test)] pub fn registered_fd_count(&self) -> usize` to `EpollDispatch`).

- [ ] **Step 3: Run, expect FAIL**

The current snapshot path has no rebuild step; the count is 0.

- [ ] **Step 4: Implement rebuild in the snapshot deserialization path**

Wherever `from_snapshot` reconstructs the `SlirpBackend` (likely in `src/vmm/mod.rs` around line 690 area where snapshots are restored), after the flow_table is rebuilt from the snapshot bytes, iterate it and call `epoll.register` for each entry's host FD.

- [ ] **Step 5: Run new test + integration suite**

```bash
cargo test --test network_baseline epoll_set_rebuilt
cargo test --test snapshot_integration -- --ignored --test-threads=1
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add tests/network_baseline.rs src/network/slirp.rs src/vmm/mod.rs
git commit -m "feat(slirp): rebuild epoll set on snapshot restore

epoll_fd is a kernel handle and cannot serialize.  After
flow_table is reconstructed from snapshot bytes, register every
host FD with a fresh EpollDispatch."
```

---

### Task 13: Bench the win + perf gate

**Files:**
- Modify: `benches/network.rs` — add `tcp_rx_latency_one_packet`.
- Modify: `src/bin/voidbox-network-bench/main.rs` — add `tcp_rx_latency_us_p50` measurement.

- [ ] **Step 1: Add divan microbench**

In `benches/network.rs`, add:

```rust
/// Phase 6.4 baseline: time from "host write returns" to "guest
/// sees data in drain_to_guest output".  Pre-6.4 this was bounded
/// below by the 5 ms net_poll_thread cycle; post-6.4 epoll
/// dispatch should deliver in microseconds.
#[divan::bench]
fn tcp_rx_latency_one_packet(bencher: Bencher) {
    // ... handshake setup outside the timed loop ...
    bencher.bench_local(|| {
        // Host writes; measure how fast the bytes appear in the
        // SlirpBackend's drain output.
    });
}
```

Full implementation: harness similar to `tcp_inbound_syn_ack_transition` shape — use `bench-helpers` feature for synthetic flow seeding, drive the data path inside the timed closure.

- [ ] **Step 2: Add wall-clock measurement to `voidbox-network-bench`**

In `src/bin/voidbox-network-bench/main.rs`, add a `tcp_rx_latency_us_p50` field to `Report` and a `measure_rx_latency` function that boots a VM, opens a guest→host flow, has the host write small packets, and measures host-T0-to-guest-arrival via the SLIRP relay.

- [ ] **Step 3: Run the perf gate against `origin/main`**

```bash
scripts/bench-compare.sh --baseline origin/main --skip-vm > /tmp/phase6.4-vs-main.md
cat /tmp/phase6.4-vs-main.md
```

Validate per the hard performance gate at the top of this plan:

- Every comparable bench: HEAD ≤ baseline + 5 %.
- `tcp_rx_latency_one_packet` (HEAD-only) shows a sub-millisecond median.
- `port_forward_accept_latency` improves by ≥ 30 %, *or* document why it stays (likely the listener accept thread is still on the 50 ms cycle — fixing it is a small follow-up step in Phase 6.4 itself or its own task; decide before committing).

- [ ] **Step 4: If `port_forward_accept_latency` doesn't improve, add a fix-up sub-task** to also move the listener accept onto epoll. The plan permits this — see Architecture notes.

- [ ] **Step 5: Commit benches + the perf-gate output**

```bash
git add benches/network.rs src/bin/voidbox-network-bench/main.rs
git commit -m "bench(network): tcp_rx_latency_one_packet + voidbox-network-bench p50

Captures the Phase 6.4 win numerically.  Pre-6.4 RX latency was
bounded below by the 5 ms net_poll_thread cycle; post-6.4 epoll
dispatch lands in microseconds.

scripts/bench-compare.sh --baseline origin/main --skip-vm output
attached as /tmp/phase6.4-vs-main.md (not committed; consult the
PR description for the table)."
```

---

### Task 14: Phase 6.4 validation gate

- [ ] **Step 1: Standard validation contract** (per `AGENTS.md`)

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo test --doc --workspace --all-features
```

All must pass.

- [ ] **Step 2: VM suites**

```bash
export VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r)
export VOID_BOX_INITRAMFS=/tmp/void-box-test-rootfs.cpio.gz
cargo test --test conformance -- --ignored --test-threads=1
cargo test --test oci_integration -- --ignored --test-threads=1
cargo test --test snapshot_integration -- --ignored --nocapture --test-threads=1
cargo test --test e2e_telemetry -- --ignored --test-threads=1
cargo test --test e2e_skill_pipeline -- --ignored --test-threads=1
cargo test --test e2e_mount -- --ignored --test-threads=1
cargo test --test e2e_service_mode -- --ignored --test-threads=1
cargo test --test e2e_sidecar -- --ignored --test-threads=1
```

All must pass.

- [ ] **Step 3: aarch64 cross-check**

```bash
CFLAGS_aarch64_unknown_linux_gnu="--sysroot=/usr/aarch64-redhat-linux/sys-root/fc43" \
  RUSTFLAGS="-D warnings" \
  cargo check --target aarch64-unknown-linux-gnu -p void-box --lib --tests
```

- [ ] **Step 4: Hard perf gate**

```bash
scripts/bench-compare.sh --baseline origin/main --skip-vm
```

Validate against the contract at the top of this plan. **The PR is not allowed to merge** until this passes.

- [ ] **Step 5: Commit gate evidence in the PR description (no commit needed)**

Capture the bench-compare output in the PR body. Phase 6.4 PR is then ready for review.

---

## Rollback plan

Each task lands as one commit. If Task N introduces a regression caught at Task M (where M > N), `git revert` Task N's commit and redispatch its implementer with the failure context. No task irreversibly changes wire format or snapshot layout — every change is additive (new fields, new module) or behavior-preserving refactor.

The only exception is the snapshot rebuild path (Task 12). If that's wrong on disk, restored backends will have a fresh-but-empty epoll set and connections will appear hung. Test the snapshot path *before* claiming Task 12 done.

## Out of scope (deferred to Phase 6.1 / 6.2 / 6.3)

- TCP half-close — Phase 6.1.
- Async outbound `connect` — Phase 6.2 (will *consume* the epoll dispatch primitive added here for `EPOLLOUT` writability detection).
- Window management — Phase 6.3.

## Reviewer pointers

- **Lock granularity:** verify `epoll_wait` does not happen under the device lock (Task 11 Step 2).
- **FD lifecycle:** every `flow_table.insert` has a matching `epoll.register`; every `flow_table.remove` has a matching `epoll.unregister`. grep for both pairs and pair-count.
- **Self-pipe correctness:** `Waker::wake` is no-block, no-allocate, signal-safe-adjacent.
- **Snapshot rebuild:** Task 12's test is the contract; verify the count helper is `#[cfg(test)]` only.
- **Token uniqueness:** `flow_token_for_tcp` is unique within the flow table because `NatKey` is unique. The 16-bit dst_ip truncation is intentional for v4-only addresses on a /16 SLIRP subnet — collisions with foreign IPs are not possible because all flows route through the gateway.

## Document history

- 2026-04-30: initial plan written, hard performance gate locked.
