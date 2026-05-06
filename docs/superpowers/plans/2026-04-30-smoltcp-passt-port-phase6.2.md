# Phase 6.2: Async Outbound Connect Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development.

**Goal:** Replace the synchronous `TcpStream::connect_timeout(addr, 3s)` on the vCPU thread with an event-driven non-blocking connect — completion is detected on the net-poll thread via `EPOLLOUT` readiness on the connecting socket. The vCPU thread is never blocked on connect again.

**Severity:** Medium-High. A guest opening a connection to an unreachable destination today stalls **all** guest networking for up to 3 seconds (the `connect_timeout`). DNS misconfigurations, transient NAT failures, or one slow destination among many freeze the whole pipeline.

**Architecture:** Phase 6.4 already gave us the `EpollDispatch` primitive with `RegisterMode::Read`/`Write`/`ReadWrite`. We just need to use it. New `TcpNatState::Connecting` state. On guest SYN: create non-blocking socket via `socket2`, call `connect()` (returns `EINPROGRESS`), insert flow with `Connecting` state, register FD with `RegisterMode::Write`. On `EPOLLOUT` readiness: check `getsockopt(SOL_SOCKET, SO_ERROR)` — zero means connected (transition to `SynReceived`, send SYN-ACK to guest, re-register as `Read`); non-zero means failed (RST to guest, reap entry).

**Tech stack:** `socket2 = "0.5"` (already in workspace), `libc::getsockopt`. No new crates.

---

## Background

`src/network/slirp.rs:1584` (in `handle_tcp_frame`'s SYN handler):

```rust
match TcpStream::connect_timeout(&dst_addr, Duration::from_secs(3)) {
    Ok(stream) => { ... insert flow, send SYN-ACK ... }
    Err(e) => { ... send RST ... }
}
```

`handle_tcp_frame` is called from `process_guest_frame` on the **vCPU thread under the device lock**. A 3-second blocking syscall here freezes the entire VMM's network handling for that duration.

passt's design ([passt/tcp.c:2785](https://passt.top/passt/tree/tcp.c#n2785)) is fully event-driven — connect dispatches to a worker, completion arrives via epoll on the connecting socket's writability. Phase 6.2 ports the *idea* using our existing `EpollDispatch`.

## State machine (Phase 6.1's diagram + new `Connecting` state)

```
                    guest SYN (translate_outbound)
                        ▼
                    Connecting       (kernel doing 3WHS in background)
                  /             \
                 /               \
   getsockopt SO_ERROR == 0    getsockopt SO_ERROR != 0
                 ▼                       ▼
            SynReceived              Closed (RST to guest)
        (re-register Read)
                 │
                 │ guest's final ACK
                 ▼
            Established
                 │ (Phase 6.1 transitions: FinWait1 / CloseWait / LastAck / Closed)
```

## Invariants (carried)

1. All-Rust path. `socket2` for socket creation; `libc::getsockopt` for SO_ERROR. No new crates.
2. Full observability — every state transition logs at `trace!` or `debug!`.
3. Cross-platform discipline — Linux-only SLIRP unchanged.
4. No regression in Phase 0–5 + 5.5b + 6.4 + listener-on-epoll + 6.1 baselines.
5. Snapshot/restore correctness — the `Connecting` state should NOT be persisted; on snapshot the connecting socket is dropped and the flow is reaped (a half-set-up connection has no useful state to preserve). Document this in `rebuild_epoll_from_flow_table`.
6. Per-flow `CONNECT_TIMEOUT` (3 s, matching today's behavior) is enforced via `last_state_change` + idle-timeout sweep — same machinery Phase 6.1 added.

---

## File impact

| File | Action |
|---|---|
| `src/network/slirp.rs` | Add `TcpNatState::Connecting`. Rewrite `handle_tcp_frame` SYN-flow setup. Add `relay_pending_connects` called from `drain_to_guest` (parallel to `relay_tcp_nat_data`). Reap `Connecting` on `CONNECT_TIMEOUT`. |
| `tests/network_baseline.rs` | Two new pins. |
| `benches/network.rs` | One new bench: `process_syn_during_pending_connects` (parametric on N pending connects). |
| `Cargo.toml` | Add `socket2 = "0.5"` if not already present. (Check first — `nix` may already pull it transitively.) |
| `docs/superpowers/plans/2026-04-30-smoltcp-passt-port-phase6.2.md` | This file. |

---

## Tasks

### Task 1: Verify `socket2` availability + add if needed

`grep -n 'socket2' Cargo.toml`. If absent, add `socket2 = { version = "0.5", features = ["all"] }` under `[target.'cfg(target_os = "linux")'.dependencies]`.

`cargo check` to confirm.

**Commit:** `chore: add socket2 dep for non-blocking connect`

(Skip if already present.)

---

### Task 2: Add `TcpNatState::Connecting` variant + struct field

In `src/network/slirp.rs`, add to `TcpNatState`:

```rust
pub enum TcpNatState {
    /// Non-blocking connect issued; waiting for EPOLLOUT readiness to
    /// arrive on the host socket. On readiness we check
    /// getsockopt(SO_ERROR): zero → transition to SynReceived and send
    /// SYN-ACK to guest; non-zero → send RST to guest and reap.
    Connecting,
    SynReceived,
    // ... existing variants ...
}
```

The state machine doc-comment above the enum needs the new transition added.

In `TcpNatEntry`, optionally add a field to store the guest-side SYN parameters needed to build the SYN-ACK *later* (after async connect completes):

```rust
struct TcpNatEntry {
    // ... existing fields ...
    /// Guest's initial sequence number (`seq` from the original SYN
    /// frame).  Stashed here only for entries in `Connecting` state so
    /// the EPOLLOUT-driven completion path can build SYN-ACK with the
    /// correct ack number (= guest_isn + 1).  Once the entry transitions
    /// to SynReceived this field is no longer read.
    guest_isn: u32,
}
```

Initialize `guest_isn: seq` at every `TcpNatEntry { ... }` site (search for the literal).

Run `cargo check`. Expected: PASS — no consumers of `Connecting` yet.

**Commit:** `feat(slirp): add TcpNatState::Connecting + guest_isn field`

---

### Task 3: Failing pin — `tcp_connect_to_unreachable_does_not_block_other_flows`

In `tests/network_baseline.rs`. The contract: open one flow to a known-good destination, one to a deliberately-unreachable destination (a port nothing is listening on, e.g. 1 — RFC2606 reserves 1 for tcpmux but binding nothing on it gives ECONNREFUSED quickly). The good-destination's SYN-ACK must arrive within 50 ms regardless of the bad destination's connect result.

```rust
#[test]
fn tcp_connect_to_unreachable_does_not_block_other_flows() {
    use std::io::Read;
    use std::net::TcpListener;
    use std::time::Instant;

    // Good destination — bind a listener.
    let good_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let good_port = good_listener.local_addr().unwrap().port();

    // Bad destination — bind then drop, leaving an OS-assigned port that
    // nothing listens on. Connecting to it will get ECONNREFUSED quickly,
    // OR (more reliably for this test) we use a port we know nothing is
    // bound to — pick one in the high range and trust it's empty.
    let bad_port: u16 = 1;  // tcpmux; almost never bound on dev hosts.

    let mut stack = SlirpBackend::new().unwrap();

    let our_seq_bad = 1000u32;
    let our_seq_good = 2000u32;

    let bad_syn_at = Instant::now();
    stack
        .process_guest_frame(&build_tcp_frame(
            SLIRP_GATEWAY_IP, GUEST_EPHEMERAL_PORT, bad_port,
            our_seq_bad, 0, TcpControl::Syn, &[],
        ))
        .unwrap();
    let bad_syn_returned = bad_syn_at.elapsed();

    // process_guest_frame must return quickly — sub-100ms even though
    // the kernel is still issuing SYNs against the dead port.
    assert!(
        bad_syn_returned < std::time::Duration::from_millis(100),
        "process_guest_frame for unreachable dest blocked vCPU for {bad_syn_returned:?}; \
         must return immediately and let the connect complete asynchronously"
    );

    // Now SYN to the good destination.
    let good_syn_at = Instant::now();
    stack
        .process_guest_frame(&build_tcp_frame(
            SLIRP_GATEWAY_IP, GUEST_EPHEMERAL_PORT + 1, good_port,
            our_seq_good, 0, TcpControl::Syn, &[],
        ))
        .unwrap();
    let good_syn_returned = good_syn_at.elapsed();
    assert!(
        good_syn_returned < std::time::Duration::from_millis(100),
        "second process_guest_frame blocked: {good_syn_returned:?}"
    );

    // Drive drain_to_guest until we see the good destination's SYN-ACK.
    // It must arrive well within 1s; if we ever wait the full 3s
    // CONNECT_TIMEOUT, the test fails.
    let deadline = Instant::now() + std::time::Duration::from_secs(1);
    let mut saw_good_synack = false;
    while Instant::now() < deadline {
        for f in drain_n(&mut stack, 1) {
            if let Some((_, _, ctrl, _)) = parse_tcp_to_guest(f.as_slice()) {
                let ip = Ipv4Packet::new_checked(
                    EthernetFrame::new_unchecked(f.as_slice()).payload(),
                ).unwrap();
                let tcp = TcpPacket::new_checked(ip.payload()).unwrap();
                if tcp.dst_port() == GUEST_EPHEMERAL_PORT + 1
                    && matches!(ctrl, TcpControl::Syn)
                {
                    saw_good_synack = true;
                    break;
                }
            }
        }
        if saw_good_synack { break; }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    assert!(saw_good_synack,
        "good-destination SYN-ACK must arrive even while bad destination is still connecting");

    // Accept the good connection so the test cleans up.
    let _ = good_listener.set_nonblocking(true);
    let _ = good_listener.accept();
}
```

Run: `cargo test --test network_baseline tcp_connect_to_unreachable_does_not_block_other_flows`. Expected: **FAIL** — the synchronous `connect_timeout(3s)` on the bad SYN blocks the vCPU thread.

**Commit:** `test(network): pin tcp_connect_to_unreachable_does_not_block_other_flows (BROKEN_ON_PURPOSE)`

---

### Task 4: Replace synchronous connect with non-blocking connect

In `src/network/slirp.rs::handle_tcp_frame`, replace the `TcpStream::connect_timeout` block (~line 1584) with non-blocking connect using `socket2`:

```rust
use socket2::{Domain, Protocol, SockAddr, Socket, Type};

// ... in the SYN handler, after translate_outbound resolved dst_addr ...

let socket = match Socket::new(
    Domain::IPV4,
    Type::STREAM.nonblocking(),
    Some(Protocol::TCP),
) {
    Ok(s) => s,
    Err(e) => {
        warn!("SLIRP TCP: socket() failed for {}:{}: {}", dst_ip, dst_port, e);
        // Send RST to guest — same shape as today.
        let rst = build_tcp_packet_static(...);
        self.inject_to_guest.push(rst);
        return Ok(());
    }
};

let sockaddr = SockAddr::from(dst_addr);
match socket.connect(&sockaddr) {
    Ok(()) => {
        // Connected immediately (loopback, fast path) — promote straight
        // to SynReceived.
        promote_connecting_to_synreceived(...);
    }
    Err(e) if e.raw_os_error() == Some(libc::EINPROGRESS) => {
        // Async connect in progress; insert Connecting entry, register
        // for EPOLLOUT, return.
        let stream: TcpStream = socket.into();
        let host_fd = stream.as_raw_fd();
        let our_seq = rand_seq();
        let token = next_flow_token(PROTO_TAG_TCP);
        let flow_key = FlowKey::Tcp(key);
        let entry = TcpNatEntry {
            host_stream: stream,
            state: TcpNatState::Connecting,
            our_seq,
            guest_ack: seq + 1,
            last_activity: Instant::now(),
            bytes_in_flight: 0,
            flow_token: token,
            last_state_change: Instant::now(),
            our_fin_sent: false,
            guest_isn: seq,
        };
        self.flow_table.insert(flow_key, FlowEntry::Tcp(entry));
        self.token_to_key.insert(token, flow_key);
        if let Err(e) = self.epoll.register(host_fd, token, RegisterMode::Write) {
            warn!(
                guest_src_port = key.guest_src_port,
                dst_ip = %key.dst_ip,
                dst_port = key.dst_port,
                fd = host_fd,
                error = %e,
                "SLIRP TCP: epoll register (Write) failed for connect-in-progress; \
                 flow will time out via Connecting state."
            );
        }
        self.epoll_waker.wake();
        debug!(
            "SLIRP TCP: connect-in-progress for {}:{} (our_seq={our_seq})",
            dst_ip, dst_port
        );
        // Note: NO SYN-ACK sent yet. Sent only after EPOLLOUT confirms connect.
    }
    Err(e) => {
        // Connect failed synchronously (rare for non-blocking; usually
        // address resolution issues). Send RST.
        warn!("SLIRP TCP: connect to {}:{} failed synchronously: {}", dst_ip, dst_port, e);
        let rst = build_tcp_packet_static(...);
        self.inject_to_guest.push(rst);
        return Ok(());
    }
}
```

Factor out a `promote_connecting_to_synreceived(...)` helper that does the SYN-ACK push + state transition + re-register as `Read` — used both for the immediate-success path here AND for the EPOLLOUT-driven path in Task 5.

Run `cargo test --test network_baseline tcp_connect_to_unreachable_does_not_block_other_flows`. Expected: **STILL FAIL** — process_guest_frame returns fast now, but the good destination's SYN-ACK never arrives because no EPOLLOUT handler exists yet.

**Commit:** `feat(slirp): non-blocking connect — Connecting state for in-flight handshakes`

---

### Task 5: `relay_pending_connects` — EPOLLOUT-driven completion

Add a new method in `src/network/slirp.rs`, called from `drain_to_guest` BEFORE `relay_tcp_nat_data`:

```rust
fn relay_pending_connects(&mut self, ready: &[EpollEvent]) {
    let mut connecting_keys: Vec<FlowKey> = Vec::new();
    for event in ready {
        if !event.writable || event.token & PROTO_TAG_MASK != PROTO_TAG_TCP {
            continue;
        }
        let Some(flow_key) = self.token_to_key.get(&event.token).copied() else {
            continue;
        };
        connecting_keys.push(flow_key);
    }

    for flow_key in connecting_keys {
        let FlowKey::Tcp(key) = flow_key else { continue };
        let Some(FlowEntry::Tcp(entry)) = self.flow_table.get_mut(&flow_key) else {
            continue;
        };
        if entry.state != TcpNatState::Connecting {
            continue;
        }

        // Check SO_ERROR to learn the actual connect outcome.
        let host_fd = entry.host_stream.as_raw_fd();
        let mut so_error: libc::c_int = 0;
        let mut len: libc::socklen_t = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(
                host_fd,
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                &mut so_error as *mut _ as *mut libc::c_void,
                &mut len,
            )
        };
        if rc < 0 || so_error != 0 {
            // Connect failed — send RST to guest, reap.
            let connect_err = if rc < 0 {
                io::Error::last_os_error()
            } else {
                io::Error::from_raw_os_error(so_error)
            };
            warn!(
                guest_src_port = key.guest_src_port,
                dst_ip = %key.dst_ip,
                dst_port = key.dst_port,
                error = %connect_err,
                "SLIRP TCP: async connect failed; sending RST to guest"
            );
            let rst = build_tcp_packet_static(
                key.dst_ip, SLIRP_GUEST_IP, key.dst_port, key.guest_src_port,
                0, entry.guest_isn.wrapping_add(1),
                TcpControl::Rst, &[],
            );
            self.inject_to_guest.push(rst);
            entry.state = TcpNatState::Closed;
            self.pending_close.push(flow_key);
            continue;
        }

        // Connected. Promote: transition to SynReceived, send SYN-ACK
        // to guest, re-register epoll for Read.
        entry.state = TcpNatState::SynReceived;
        entry.last_state_change = Instant::now();
        let our_seq = entry.our_seq;
        let guest_isn = entry.guest_isn;
        let flow_token = entry.flow_token;

        // Re-register the FD for read events. The kernel allows
        // EPOLL_CTL_MOD to change the event mask in place; if it fails
        // we fall back to unregister+register.
        let mod_result = self.epoll.modify(host_fd, flow_token, RegisterMode::Read);
        if let Err(e) = mod_result {
            warn!(
                guest_src_port = key.guest_src_port,
                error = %e,
                "SLIRP TCP: epoll modify Write→Read failed; flow may stall"
            );
        }

        let syn_ack = build_tcp_packet_static(
            key.dst_ip, SLIRP_GUEST_IP, key.dst_port, key.guest_src_port,
            our_seq, guest_isn.wrapping_add(1),
            TcpControl::Syn, &[],
        );
        self.inject_to_guest.push(syn_ack);
        debug!(
            "SLIRP TCP: async connect OK for {}:{}, SYN-ACK sent",
            key.dst_ip, key.dst_port
        );
    }
}
```

Add `EpollDispatch::modify(&self, fd, token, mode)` that calls `epoll_ctl(EPOLL_CTL_MOD)`. Pattern mirrors `register` exactly except `EPOLL_CTL_ADD` → `EPOLL_CTL_MOD`. Update tests if needed.

In `SlirpBackend::drain_to_guest`, call `self.relay_pending_connects(&ready)` BEFORE `self.relay_tcp_nat_data(&ready)` so a flow that completes connect AND has data arrive in the same epoll cycle handles both correctly.

Run `cargo test --test network_baseline tcp_connect_to_unreachable_does_not_block_other_flows`. Expected: **PASS** — the good destination's SYN-ACK now arrives via async completion.

**Commit:** `feat(slirp): EPOLLOUT-driven async connect completion (relay_pending_connects)`

---

### Task 6: Failing pin — `tcp_connect_async_eventual_rst_on_failure`

Synthesize a connect to an unreachable address, drive `drain_to_guest` for >100 ms, assert the guest receives RST.

```rust
#[test]
fn tcp_connect_async_eventual_rst_on_failure() {
    use std::time::Instant;

    let mut stack = SlirpBackend::new().unwrap();
    // Bind+drop a listener to claim a port, then close it. The OS may or
    // may not refuse connections on it instantly; we'll just drive
    // drain_to_guest until we see a RST or timeout.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let bad_port = listener.local_addr().unwrap().port();
    drop(listener);

    let our_seq = 1000u32;
    stack.process_guest_frame(&build_tcp_frame(
        SLIRP_GATEWAY_IP, GUEST_EPHEMERAL_PORT, bad_port,
        our_seq, 0, TcpControl::Syn, &[],
    )).unwrap();

    let deadline = Instant::now() + std::time::Duration::from_secs(2);
    let mut saw_rst = false;
    while Instant::now() < deadline {
        for f in drain_n(&mut stack, 1) {
            if let Some((_, _, ctrl, _)) = parse_tcp_to_guest(f.as_slice()) {
                if matches!(ctrl, TcpControl::Rst) {
                    saw_rst = true;
                    break;
                }
            }
        }
        if saw_rst { break; }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    assert!(saw_rst,
        "guest must eventually receive RST when async connect to dropped-listener port fails");
}
```

Should already pass after Task 5 lands (the SO_ERROR check sends RST on failure).

**Commit:** `test(network): pin tcp_connect_async_eventual_rst_on_failure`

---

### Task 7: `CONNECT_TIMEOUT` reaping for stuck `Connecting` entries

If a destination accepts the SYN but never completes the handshake (silent firewall drop), our entry sits in `Connecting` forever. Add a `CONNECT_TIMEOUT` (3 s, matching today's pre-Phase-6.2 behavior) and reap stuck entries.

In `relay_tcp_nat_data`'s existing `to_remove_set` sweep (or in a sibling pass), check for `state == Connecting && last_state_change.elapsed() > CONNECT_TIMEOUT` and:
- Send RST to guest.
- Push to `pending_close`.

```rust
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
```

Place near `LAST_ACK_TIMEOUT` (Phase 6.1 already added that — same pattern).

Run baseline pins: 21/21 default + 22/22 bench-helpers (after Task 7's pin lands).

**Commit:** `feat(slirp): CONNECT_TIMEOUT reaping for stuck Connecting flows`

---

### Task 8: Bench — `process_syn_during_pending_connects`

Validates O(1) cost on guest TX path regardless of pending-connect backlog.

In `benches/network.rs`:

```rust
#[divan::bench(args = [0, 10, 100, 1000])]
fn process_syn_during_pending_connects(bencher: Bencher, n_pending: usize) {
    let mut stack = SlirpBackend::new().unwrap();

    // Pre-populate flow_table with `n_pending` Connecting entries
    // (synthetic, via bench-helpers helper).
    for i in 0..n_pending {
        // A bench-helpers method on SlirpBackend that inserts a
        // synthetic Connecting entry without actually issuing connect().
        // E.g.:
        //   stack.insert_synthetic_connecting_entry(
        //       guest_src_port = 60000 + i,
        //       dst_ip = SLIRP_GATEWAY_IP,
        //       dst_port = 1,
        //   );
    }

    // Time the cost of processing one guest SYN to a fresh dst port.
    let frame = build_syn(49152, 80);

    bencher.bench_local(|| {
        let _ = stack.process_guest_frame(divan::black_box(&frame));
    });
}
```

Add the bench-helpers method `insert_synthetic_connecting_entry` mirroring the existing `insert_synthetic_synsent_entry`.

Expected: each parametric arm produces a similar median (process_guest_frame's cost should be O(1) in n_pending — it just does flow_table.insert + epoll.register, both O(1)).

**Commit:** `bench(network): process_syn_during_pending_connects (Phase 6.2 baseline)`

---

### Task 9: Phase 6.2 validation gate

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --test network_baseline                                          # 22/22
cargo test --test network_baseline --features bench-helpers -- --test-threads=1   # 23/23
cargo test --lib network                                                    # 23/23+
cargo bench --bench network --features bench-helpers --no-run
cargo build --release
```

If the test image is available:
```bash
cargo test --test snapshot_integration -- --ignored --test-threads=1
```

Wall-clock sanity:
```bash
voidbox-network-bench --iterations 3 --bulk-mb 10
# g2h ≥ 6 Gbps, RR/CRR parity, no regression
```

`bench-compare.sh --baseline 47868f0 --skip-vm` should show:
- `process_syn` parity or slight improvement (no longer blocking on connect).
- `process_syn_during_pending_connects/{0,10,100,1000}` all close to baseline `process_syn` (O(1) cost).
- All other benches no regression.

---

## Out of scope (future phases)

- Window management (Phase 6.3).
- IPv6 (Phase 7).
- Refining SYN-ACK timing — Linux TCP supports `TCP_FASTOPEN` and similar; not in 6.2 scope.

## Reviewer pointers

- Verify `process_guest_frame` for an unreachable destination returns within 1 ms in benchmarks. The whole point of the phase.
- Verify the `EPOLL_CTL_MOD` Write→Read path on connect completion doesn't drop events between the modify and the next epoll_wait cycle. Edge cases: what if EPOLLOUT was the only event we registered for and the connect socket has data already buffered (uncommon but possible)? The Read mode picks it up on the next cycle — verify in test.
- Snapshot interaction: `Connecting` flows do NOT survive snapshot (they have no useful state to persist). `rebuild_epoll_from_flow_table` should detect `state == Connecting` and either re-register as Write (treating as still-pending — but the underlying socket is dead post-snapshot) or skip + reap. Pick skip+reap.
- `socket2`'s `Type::STREAM.nonblocking()` requires the `all` feature. Verify `Cargo.toml`.

## Document history

- 2026-05-04: initial plan written.
