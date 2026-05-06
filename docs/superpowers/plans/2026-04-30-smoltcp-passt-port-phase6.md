# Phase 6: TCP Lifecycle + Async Connect + Window Mgmt + Event-Driven Polling

> **Status:** Overview (scope + design). Per-subsystem TDD task lists are deferred to dedicated plans (`-phase6.1.md`, `-phase6.2.md`, `-phase6.3.md`, `-phase6.4.md`) written before each is implemented. This document scopes the work, locks invariants, and lists validation gates so each sub-plan can be reviewed against a stable target.

> **For agentic workers:** This is an **overview**, not an executable plan. Do not run subagent-driven-development against this file. When picking up a sub-area, write its own plan first.

**Goal:** Close the four architectural gaps surfaced in the `smoltcp-passt-port-phase0` PR review without regressing any Phase 0–5 baseline.

**Architecture:** Each sub-area imports a specific passt design pattern adapted to our `cfg(target_os = "linux")` SLIRP backend; none requires a backend split. The relay loop in `SlirpBackend::drain_to_guest` stays the single net-poll dispatch point; the changes layer onto its existing flow_table / inject_to_guest pipeline.

**Tech stack:** smoltcp 0.11 wire types, `std::net::TcpStream` (non-blocking), Linux `epoll` (Phase 6.4), no new crates.

---

## Background

Reviewer findings on the smoltcp-passt-port PR (April 2026) — three "Medium" or higher and one "Medium-Low" architectural gap. All four were verified VALID against current code. Quick-fix correctness items (Copilot review) are addressed on the same PR; this Phase 6 plan covers the architecture-shaped follow-ups.

Reference: `docs/superpowers/plans/2026-04-27-smoltcp-passt-port.md` (top-level spec, observability invariant), Phase 0–5 plans (architectural decisions established by prior phases).

## Invariants (carried from earlier phases — non-negotiable)

These are locked from the top-level spec. Phase 6 changes must preserve all of them.

1. **Full observability.** Every TCP/UDP/ICMP frame and every state transition remains traceable through tracing logs. No opaque C-process or kernel-side magic. If a new subsystem hides state inside the kernel (e.g. epoll), tracing must still expose what the host saw and when.
2. **All-Rust path.** No new C dependencies, no FFI beyond what `libc` already provides. `epoll`-via-`libc` is acceptable; a new crate that opaques it is not, unless the crate is already in the workspace.
3. **Cross-platform discipline.** SLIRP itself is Linux-only (`#[cfg(target_os = "linux")]` in `Cargo.toml`). Phase 6 stays inside that gate. macOS uses VZ's built-in NAT; Phase 6 does not affect it.
4. **No regression in Phase 0–5 baselines.** `bench-compare.sh --baseline <phase-5-tip>` must show every existing bench at ±5% or better. New benches added in Phase 6 may legitimately move the baseline, but the existing comparable set holds.
5. **Snapshot/restore correctness.** `snapshot_integration` must continue to pass. Any new state (e.g. half-close timers, async connect futures) added to `TcpNatEntry` must round-trip through serde or be rebuilt from `TcpStream` state on restore — not silently dropped.
6. **No bench-mode-only fixes.** Behavior changes go in production code paths, not behind `#[cfg(test)]` or feature flags. Tests/benches consume the same paths the guest does.

## Sub-areas

Four independent sub-areas, four sub-plans. Order is by reviewer-assigned severity, not by required ordering — they can land in any sequence as long as their individual validation gates hold.

---

### 6.1 — TCP half-close (A1, High)

**Severity:** High (correctness gap, not just performance).

**Current state:**

- `TcpNatState` at `src/network/slirp.rs:131-144` declares `FinWait1`, `FinWait2`, `CloseWait`, `LastAck` variants but they are unused. The enum carries `#[allow(dead_code)]` on line 130 to mute the resulting warnings.
- Guest FIN handler at `src/network/slirp.rs:1483-1500`: on receiving guest FIN, the stack immediately sends a FIN+ACK back to the guest and marks the entry `Closed` in the same call. There is no transition through `FinWait*` or `CloseWait`. The host-side `TcpStream` is dropped at the next `relay_tcp_nat_data` sweep when the entry is reaped.

**The bug this enables:**

When the guest's application closes the write side of a socket but expects to keep reading the host's response (the half-close pattern used by HTTP request bodies, SMTP DATA, anything with `shutdown(SHUT_WR)`), VoidBox slams the connection shut both directions. The host side never gets to flush its remaining response; the guest's read returns EOF prematurely. This is silent data loss for any protocol that uses orderly half-close.

**Reference:** passt's `tcp.c` ([passt/tcp.c:238](https://passt.top/passt/tree/tcp.c#n238), [tcp.c:401](https://passt.top/passt/tree/tcp.c#n401)) tracks the four half-close states explicitly with timer-bounded transitions.

**Target state:**

- Guest FIN sets `state = FinWait1` (we still owe the host a half-close), shuts down the host socket's write side via `TcpStream::shutdown(Shutdown::Write)`, and ACKs the guest's FIN — but **does not** send our own FIN yet.
- When the host returns EOF (zero-byte read on the established connection) and the relay queue is drained, send our FIN to the guest, transition to `LastAck`.
- On guest's final ACK, transition to `Closed` and reap.
- The mirror pattern handles the host-initiated close: host EOF first → state goes to `CloseWait` (we owe the guest a FIN), continue forwarding any guest writes to the host, eventually send FIN to guest → `LastAck` → reap on ACK.
- Add a `LAST_ACK_TIMEOUT` (suggest 60 s, mirroring TCP MSL × 2) so a missing final ACK doesn't leak entries.

**Test requirements:**

- New `tests/network_baseline.rs` pin `tcp_half_close_guest_writes_first`: guest sends data, FIN; host reads data, replies with more data, then FIN. Assert: guest sees the host's post-FIN data **and** its FIN, in that order. Pre-Phase-6.1 this would fail (host data dropped).
- New pin `tcp_half_close_host_writes_first`: symmetric — host sends data, FIN; guest replies, FIN. Assert ordering.
- New pin `tcp_last_ack_timeout_reaps_stale_entry`: synthesize a `LastAck` entry with `last_activity` deep in the past; one `drain_to_guest` cycle later assert the entry is gone.
- `snapshot_integration`: round-trip a connection in `CloseWait` state. Assert post-restore the state is preserved (or, if we choose not to serde the half-close states, that the connection cleanly closes within `LAST_ACK_TIMEOUT`).

**Validation gates (in addition to the global ones below):**

- `cargo test --test network_baseline tcp_half_close_*`
- `cargo test --test snapshot_integration -- --ignored --test-threads=1`

**File impact:**

- `src/network/slirp.rs` — `handle_tcp_frame` FIN/RST arms (~lines 1483–1506), `relay_tcp_nat_data` (~line 1512+), `TcpNatEntry` (add half-close timer field if needed).
- `tests/network_baseline.rs` — three new pins.
- No changes to public API.

---

### 6.2 — Async outbound connect (A2, Medium-High)

**Severity:** Medium-High (correctness + UX gap).

**Current state:**

- `src/network/slirp.rs:1271`: on guest SYN, `handle_tcp_frame` calls `TcpStream::connect_timeout(&dst_addr, Duration::from_secs(3))` **synchronously**.
- `handle_tcp_frame` is called from `process_guest_frame` (~line 664), which is called from the virtio-net TX path (`src/devices/virtio_net.rs:~656`).
- The TX path runs on the vCPU thread under the device lock. A 3 s blocking connect to an unreachable destination stalls **all** guest networking — including unrelated connections — for the duration of the timeout.

**The bug this enables:**

A guest that opens connections to multiple destinations, one of which is slow or unreachable, sees the entire host networking pipeline freeze for 3 s every time it tries that destination. Long-running guests with sporadic dead destinations (DNS misconfigurations, transient NAT failures) suffer noticeable hitches.

**Reference:** passt is fully event-driven — connect dispatches to a worker, completion arrives via epoll on the connecting socket's writability ([passt/tcp.c:2785](https://passt.top/passt/tree/tcp.c#n2785)).

**Target state:**

- On guest SYN: create a non-blocking socket (`TcpStream::connect` with `O_NONBLOCK`, or `socket2::Socket::new` + `connect_with_timeout` driven by us), insert a new state `Connecting` into `TcpNatState`, queue an entry in `flow_table` with the connecting socket. Return immediately to the vCPU thread.
- The net-poll thread polls the connecting socket on each tick (writability-check via `poll`/`select`/`epoll` — coordinate with 6.4). On readiness:
  - Check `getsockopt(SOL_SOCKET, SO_ERROR)` — zero means connected, non-zero means failed.
  - On success: transition `Connecting → SynReceived`, send SYN-ACK to the guest.
  - On failure: send RST to the guest, reap the entry.
  - On still-pending after `CONNECT_TIMEOUT` (3 s, matching today's behavior): treat as failure.
- vCPU thread is now never blocked on `connect`.

**Test requirements:**

- New pin `tcp_connect_to_unreachable_does_not_block_other_flows`: open one flow to a known-good destination, one to a deliberately-unreachable destination, both in quick succession. Measure time from guest SYN to host accepting the good-destination flow. Pre-6.2 this would be ~3 s (waiting for the bad one); post-6.2 it should be sub-millisecond.
- New pin `tcp_connect_async_eventual_rst_on_failure`: synthesize a connect to an unreachable address; drive `drain_to_guest` for >3 s; assert the guest receives RST.
- Bench: `bench/network.rs` add `process_syn_during_pending_connects` parametric on N pending connecting flows. Validates O(1) cost on guest TX path regardless of pending-connect backlog.

**Validation gates:**

- `cargo test --test network_baseline tcp_connect_*`
- `cargo bench --bench network process_syn_during_pending_connects`

**File impact:**

- `src/network/slirp.rs` — `TcpNatState` (add `Connecting`), `handle_tcp_frame` SYN arm (lines ~1267–1290), new `relay_pending_connects` method called from `drain_to_guest` (parallel to `relay_tcp_nat_data`).
- `tests/network_baseline.rs` — two new pins.
- `benches/network.rs` — one new bench.
- Snapshot interaction: `Connecting` state must serde correctly; restore should drop `Connecting` flows (reconnect from scratch is acceptable, deferred to Phase 6.1's MSL-bounded timer).

---

### 6.3 — TCP window management (A3, Medium)

**Severity:** Medium (perf gap, throughput left on the table).

**Current state:**

- `src/network/slirp.rs:1927`: `build_tcp_packet_static` always emits `window_len: TCP_WINDOW (65535)`, `window_scale: None`.
- No code reads `tcp.window_len()` from incoming guest frames. The guest's advertised window is ignored entirely.

**Why this matters:**

- The guest's TCP stack negotiates a window with us. We send "always 65535" regardless of what the guest can actually buffer. This is wrong both directions:
  - Inbound (host→guest): we relay host data into our `inject_to_guest` queue without ever asking whether the guest still has receive buffer. If the guest is slow, our queue grows unbounded — Phase 3 partially mitigated this with peek-based reads, but window-aware backpressure would be cleaner.
  - Outbound (guest→host): the guest sends respecting our advertised window (always 65535). On modern guests with `tcp_window_scaling=1` (the default), this caps effective throughput at 64 KB / RTT regardless of available bandwidth.
- The `window_scale: None` means we never negotiate scaling on SYN. Even if we tracked windows, we'd be capped at 64 KB.

**Reference:** passt's `tcp_conn` ([passt/tcp_conn.h:21](https://passt.top/passt/tree/tcp_conn.h#n21)) tracks `wnd_from_tap`, `wnd_to_tap`, scale factors, and updates ACK/window per [tcp.c:1021](https://passt.top/passt/tree/tcp.c#n1021), [tcp.c:1426](https://passt.top/passt/tree/tcp.c#n1426).

**Target state:**

- On SYN/SYN-ACK exchange, negotiate `window_scale: Some(7)` (128× scale factor — passt's default). `TcpNatEntry` records the negotiated scale.
- On every guest packet, read `tcp.window_len()` and update `entry.guest_window` (after applying scale). Use this to bound the host→guest send rate: never push more bytes through `inject_to_guest` than the guest's effective receive window allows.
- On every host-side relay, set our outgoing `window_len` based on host kernel state — `getsockopt(TCP_INFO).tcpi_rcv_space` gives kernel-side receive buffer headroom; advertise that, scaled.
- Drop the hardcoded `TCP_WINDOW = 65535` constant.

**Test requirements:**

- New pin `tcp_advertised_window_tracks_guest_buffer`: synthesize a guest with a small advertised window (say 4096); push 64 KB of data from host; assert that `inject_to_guest` never holds more than ~`window` unacknowledged bytes.
- New pin `tcp_window_scale_negotiated_in_syn`: parse the SYN-ACK we send to the guest; assert it includes `window_scale: Some(7)`.
- Bench: extend `tcp_bulk_throughput_1mb` to also run with a constrained-window receiver (`SO_RCVBUF=16384`); pre-6.3 throughput will be 64 KB / RTT bound; post-6.3 should be substantially higher because we'll let the guest send larger bursts when host kernel space allows.

**Validation gates:**

- `cargo test --test network_baseline tcp_advertised_window_*`
- `cargo bench --bench network tcp_bulk_throughput_*` — assert no regression, and ideally improvement at small `SO_RCVBUF`.

**File impact:**

- `src/network/slirp.rs` — `TcpNatEntry` (add `guest_window`, `guest_window_scale`), `build_tcp_packet_static` signature (take advertised window from caller), `handle_tcp_frame` (read incoming window), `relay_tcp_nat_data` (gate sends on guest window).
- `tests/network_baseline.rs` — two new pins.
- `benches/network.rs` — one new bench arm.

---

### 6.4 — Event-driven RX polling (A4, Medium-Low)

**Severity:** Medium-Low (efficiency, not correctness).

**Current state:**

- `src/vmm/mod.rs:1599` — `net_poll_thread` wakes every 5 ms (`std::thread::sleep(Duration::from_millis(5))` at line 1609).
- `src/network/slirp.rs:1549` — `relay_tcp_nat_data` re-peeks a 64 KiB buffer on every connected TCP socket every tick, regardless of whether new data has arrived.

**Why this matters:**

- 200 polls/second on every connected flow, even when idle. With many flows this is wasted CPU.
- 5 ms granularity means tail latency for any RX event is bounded below by ~5 ms even if data arrived microseconds after the last poll. For latency-sensitive workloads this is the floor.

**Reference:** passt uses epoll-driven socket readiness ([passt/tcp.c:463](https://passt.top/passt/tree/tcp.c#n463)) with optional `SO_PEEK_OFF` — the syscall returns the readable list, no polling needed.

**Target state:**

- Replace the 5 ms timer with `epoll_wait` on a Linux `epoll_fd` that owns all of:
  - the connected `TcpStream`s in `flow_table` (registered with `EPOLLIN`)
  - the connecting sockets from Phase 6.2 (registered with `EPOLLOUT`)
  - the UDP flow sockets (Phase 2)
  - the ICMP echo socket (Phase 1)
  - a `pipe(2)` self-pipe for inter-thread wakeup (so `process_guest_frame` can request an out-of-band poll cycle when it adds a new flow).
- `epoll_wait` timeout: short (say 50 ms) just as a safety net for periodic housekeeping (LAST_ACK_TIMEOUT sweeps, idle UDP flow reaping). The hot path is event-driven.
- Each socket's `epoll_data` carries its `FlowKey` so the readiness handler can dispatch directly without iterating the full table.

**Caveats:**

- This sub-area is **Linux-specific** (`epoll`). The SLIRP backend itself is already Linux-only, so this fits, but the implementation should isolate epoll inside a `mod epoll_dispatch` so a future portable backend (e.g. BSD `kqueue`) can plug in a different reactor.
- Snapshot/restore: an `epoll_fd` does not survive snapshot (it's a kernel-side handle on real fds). Restore must rebuild the epoll set from scratch from `flow_table` contents — no serde required for the `epoll_fd` itself.

**Test requirements:**

- New pin `tcp_rx_latency_sub_5ms_when_data_available`: send data from host to a connected guest flow; measure host→guest delivery latency. Pre-6.4 this is bounded below by 5 ms (the timer cycle); post-6.4 it should be sub-millisecond on a quiet system.
- Bench: existing `port_forward_accept_latency` should *improve* — it's currently bounded by a 50 ms listener-poll cycle, but if 6.4 also moves the listener accept onto epoll, the median should drop substantially.
- `snapshot_integration`: verify rebuild-on-restore works (no FD leak, all flows still relay).

**Validation gates:**

- `cargo test --test network_baseline tcp_rx_latency_*`
- `cargo bench --bench network port_forward_accept_latency` — should regress *favorably* (faster).
- `cargo test --test snapshot_integration -- --ignored`

**File impact:**

- `src/vmm/mod.rs` — `net_poll_thread` rewrite to use `epoll_wait` (~lines 1599–1640).
- `src/network/slirp.rs` — new `mod epoll_dispatch`, `SlirpBackend` holds the `epoll_fd`, `flow_table` insertions/removals add/remove from epoll.
- New constants for the epoll wakeup pipe.

---

## Cross-cutting concerns

### Bench discipline

Every sub-area must add at least one bench (microbench in `benches/network.rs` and/or wall-clock metric in `voidbox-network-bench`) that captures the win or proves no regression. `bench-compare.sh --baseline <phase-5-tip>` must run cleanly before each sub-area's PR is merged. Shared protocol: each sub-area's PR description includes the bench-compare table.

### Observability

Every state transition added (Connecting, FinWait*, CloseWait, LastAck, window updates, epoll readiness) emits a `tracing::trace!` or `tracing::debug!` line keyed on the relevant `FlowKey`. No silent state changes. This matches the observability invariant.

### Test image

No new test-image requirements expected. All new e2e pins should be expressible against the existing initramfs (BusyBox + claudio).

### Phase ordering

Logically sensible order is **6.4 → 6.2 → 6.1 → 6.3** (epoll first to give 6.2 its readiness primitive, async connect next to remove vCPU stalls, half-close once we have proper per-flow event handling, window mgmt last as the polish layer). However, the validation gates per sub-area are independent; any order that passes all gates is acceptable.

## Validation gates (global, every sub-area)

The standard validation contract from `AGENTS.md` applies. In addition:

```
# 1. Phase 0–5 baselines hold.
scripts/bench-compare.sh --baseline <phase-5-tip-sha> --skip-vm

# 2. All Phase 6.X test pins pass.
cargo test --test network_baseline -- --ignored --test-threads=1

# 3. Snapshot integration intact.
cargo test --test snapshot_integration -- --ignored --test-threads=1

# 4. Cross-platform compile.
cargo check --workspace --exclude guest-agent --all-targets --all-features  # macOS shape

# 5. aarch64 cross-check (per AGENTS.md "aarch64 cross-check" section).
```

## Out of scope

- IPv6 (deferred from earlier phases; would be its own Phase 7).
- TCP options beyond MSS and window-scale (SACK, timestamps, ECN). Possible future work but not Phase 6.
- vsock-over-SLIRP (orthogonal subsystem).
- A passt head-to-head benchmark suite (deferred separate task — needs passt+qemu reference env).

## Reviewer pointers

When a sub-area's plan and PR land, the review focus per area:

- **6.1**: half-close transitions and `LAST_ACK_TIMEOUT` reaping. Verify no FD leaks under repeated open-close-open patterns. Verify snapshot interaction.
- **6.2**: vCPU thread is never blocked on connect under any input. Verify timing of the "unreachable destination doesn't stall good destination" pin.
- **6.3**: window scale negotiation in SYN/SYN-ACK frames. Verify advertised window tracks guest buffer state on tracing logs.
- **6.4**: epoll FD lifecycle (register/unregister on flow_table mutation), wakeup-pipe correctness, snapshot rebuild path.

## Open questions

- **6.3:** what window-scale factor to advertise? passt uses 7 (128×). We could be more conservative (say 5 = 32×) initially. Decide in 6.3's plan.
- **6.4:** should the epoll wakeup pipe also carry the new-flow `FlowKey` so the poll thread can `epoll_ctl(EPOLL_CTL_ADD, ...)` itself, vs. doing it under the SlirpBackend lock from the vCPU thread? Tradeoff is lock granularity vs. message-passing complexity. Decide in 6.4's plan.

---

## Document history

- 2026-04-30: initial overview written, scope locked from PR review on `smoltcp-passt-port-phase0` branch.
