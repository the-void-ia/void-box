# Phase 6.1: TCP Half-Close Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Wire `TcpNatState`'s currently-unused `FinWait1`, `CloseWait`, `LastAck` variants into the SLIRP relay so guest `shutdown(SHUT_WR)` mid-write doesn't lose tail data.

**Severity:** High — silent data loss on every protocol that uses orderly half-close (HTTP request bodies, SMTP DATA, anything with `shutdown(SHUT_WR)`).

**Architecture:** Replace the current "guest FIN → immediate close" short-circuit with a five-state machine driven by guest FIN events and host-side EOF on `host_stream`. Add a `LAST_ACK_TIMEOUT` of 60 s (TCP MSL×2) so missing final ACKs don't leak entries.

**Tech stack:** Existing `TcpNatEntry` + `TcpNatState` types in `src/network/slirp.rs`. No new dependencies.

---

## Background

`TcpNatState` (`src/network/slirp.rs:131-144`) declares `FinWait1`, `FinWait2`, `CloseWait`, `LastAck` but the implementation only ever uses `Syn*`, `Established`, `Closed`. The enum carries `#[allow(dead_code)]` (line 130) to mute warnings.

Guest FIN handler (`src/network/slirp.rs:1657-1676`):

```rust
if tcp.fin() {
    entry.guest_ack = seq.wrapping_add(1);
    let fin_ack_frame = build_tcp_packet_static(..., TcpControl::Fin, &[]);
    self.inject_to_guest.push(fin_ack_frame);
    entry.our_seq = entry.our_seq.wrapping_add(1);
    entry.state = TcpNatState::Closed;     // ← LOSES IN-FLIGHT HOST DATA
    self.pending_close.push(flow_key);     // ← FORCES IMMEDIATE REAP
    return Ok(());
}
```

This pushes the host-side `TcpStream` into `pending_close`, which `relay_tcp_nat_data` reaps the same tick. The host's pending write data never makes it back to the guest.

## Target state machine

```
                           guest FIN (we ACK + shutdown(Write))
   Established  ──────────────────────────────────────────►  FinWait1
       │                                                         │
       │ host EOF (we send FIN)                                  │ host EOF (we send FIN)
       ▼                                                         ▼
   CloseWait                                                  LastAck
       │ guest FIN (we ACK)                                      │ guest's final ACK
       ▼                                                         │ — or —
   LastAck ◄──────────────────────────────────────────────────── │ LAST_ACK_TIMEOUT (60 s)
       │                                                         │
       │ guest's final ACK / LAST_ACK_TIMEOUT                    ▼
       └─────────────────────────────────► Closed ◄──────────────┘
                                              │
                                              ▼  unregister from epoll
                                          flow_table
                                          .remove()
```

We collapse `FinWait2` (in real TCP, "we sent FIN, peer ACKed our FIN, peer hasn't sent its own FIN yet"). Since we don't observe per-segment ACKs from the kernel — only data + EOF — we treat `FinWait1` as continuing until host EOF arrives, then jump straight to `LastAck`.

## Invariants (carried)

1. All-Rust path. No new crate deps.
2. Full observability — every state transition logs at `trace!` or `debug!`.
3. Cross-platform discipline (Linux-only SLIRP unchanged).
4. No regression in Phase 0–5 + 5.5b + 6.4 baselines. `bench-compare.sh --baseline 47868f0 --skip-vm` enforced.
5. Snapshot/restore correctness — `snapshot_integration` continues to pass. New states must round-trip OR cleanly degrade to `Closed` within `LAST_ACK_TIMEOUT`.

---

## File impact

| File | Action |
|---|---|
| `src/network/slirp.rs` | Modify `TcpNatEntry` (add `last_state_change: Instant`), rewrite FIN handler, add CloseWait/LastAck transitions in `relay_tcp_nat_data`, add `LAST_ACK_TIMEOUT` reaping. Drop `#[allow(dead_code)]` on `TcpNatState`. |
| `tests/network_baseline.rs` | Three new pins. |
| Snapshot serde | Verify the new state transitions round-trip; if not, document the cleanup-on-restore behavior. |

No public API change.

---

## Tasks

### Task 1: Failing pin — `tcp_half_close_guest_writes_first`

Pure synthetic harness. Drives `SlirpBackend` directly without a VM.

```rust
#[test]
fn tcp_half_close_guest_writes_first() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let host_port = listener.local_addr().unwrap().port();

    let server = std::thread::spawn(move || -> Vec<u8> {
        let (mut sock, _) = listener.accept().unwrap();
        // Read until EOF (guest will send data + FIN).
        let mut request = Vec::new();
        let _ = sock.read_to_end(&mut request);
        // After guest's shutdown(WRITE), we still want to send response.
        sock.write_all(b"HTTP/1.1 200 OK\r\n\r\nBODY").unwrap();
        // Then close.
        drop(sock);
        request
    });

    let mut stack = SlirpBackend::new().unwrap();

    // Guest 3-way handshake.
    let our_seq = 1000u32;
    stack.process_guest_frame(&build_tcp_frame(
        SLIRP_GATEWAY_IP, GUEST_EPHEMERAL_PORT, host_port, our_seq, 0,
        TcpControl::Syn, &[],
    )).unwrap();
    let mut gateway_seq = 0u32;
    for f in drain_n(&mut stack, 4) {
        if let Some((s, _, ctrl, _)) = parse_tcp_to_guest(&f) {
            if matches!(ctrl, TcpControl::Syn) { gateway_seq = s; break; }
        }
    }
    stack.process_guest_frame(&build_tcp_frame(
        SLIRP_GATEWAY_IP, GUEST_EPHEMERAL_PORT, host_port,
        our_seq + 1, gateway_seq + 1, TcpControl::None, &[],
    )).unwrap();

    // Guest sends "HELLO" data + FIN.
    let request = b"HELLO";
    stack.process_guest_frame(&build_tcp_frame(
        SLIRP_GATEWAY_IP, GUEST_EPHEMERAL_PORT, host_port,
        our_seq + 1, gateway_seq + 1, TcpControl::Psh, request,
    )).unwrap();
    stack.process_guest_frame(&build_tcp_frame(
        SLIRP_GATEWAY_IP, GUEST_EPHEMERAL_PORT, host_port,
        our_seq + 1 + request.len() as u32, gateway_seq + 1,
        TcpControl::Fin, &[],
    )).unwrap();

    // Drive drain_to_guest until we see host's response data AND its FIN.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut response_bytes: Vec<u8> = Vec::new();
    let mut saw_host_fin = false;
    while std::time::Instant::now() < deadline {
        for f in drain_n(&mut stack, 1) {
            if let Some((_, _, ctrl, payload_len)) = parse_tcp_to_guest(&f) {
                // Extract payload bytes.
                let eth = EthernetFrame::new_unchecked(f.as_slice());
                let ip = Ipv4Packet::new_unchecked(eth.payload());
                let tcp = TcpPacket::new_unchecked(ip.payload());
                if payload_len > 0 {
                    response_bytes.extend_from_slice(tcp.payload());
                }
                if matches!(ctrl, TcpControl::Fin) {
                    saw_host_fin = true;
                }
            }
        }
        if !response_bytes.is_empty() && saw_host_fin {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    let _ = server.join();

    assert_eq!(&response_bytes[..], b"HTTP/1.1 200 OK\r\n\r\nBODY",
        "guest must receive ALL host response data after sending FIN");
    assert!(saw_host_fin, "guest must receive host's FIN");
}
```

Run: `cargo test --test network_baseline tcp_half_close_guest_writes_first`. Expected: **FAIL** — current code marks state=Closed on guest FIN; host's response data is dropped.

**Commit:** `test(network): pin tcp_half_close_guest_writes_first (BROKEN_ON_PURPOSE)`

---

### Task 2: Wire `FinWait1` transition + `shutdown(Write)` on guest FIN

In `src/network/slirp.rs::handle_tcp_frame`, replace the FIN handler:

```rust
// FIN from guest
if tcp.fin() {
    debug!("SLIRP TCP: FIN from guest for {}:{}", dst_ip, dst_port);
    entry.guest_ack = seq.wrapping_add(1);

    // ACK the guest's FIN — but don't send our own FIN yet. Host
    // application may have data still to send. We transition to
    // FinWait1 and shut down the host socket's write side so the
    // host knows no more data is coming from the guest.
    let ack_frame = build_tcp_packet_static(
        dst_ip, SLIRP_GUEST_IP, dst_port, src_port,
        entry.our_seq, entry.guest_ack,
        TcpControl::None, &[],
    );
    self.inject_to_guest.push(ack_frame);

    if let Err(e) = entry.host_stream.shutdown(std::net::Shutdown::Write) {
        warn!("SLIRP TCP: shutdown(Write) failed on guest FIN, falling back \
               to immediate close: {}", e);
        entry.state = TcpNatState::Closed;
        self.pending_close.push(flow_key);
        return Ok(());
    }

    entry.state = TcpNatState::FinWait1;
    entry.last_state_change = Instant::now();
    trace!("SLIRP TCP: state Established → FinWait1 for {}:{}", dst_ip, dst_port);
    return Ok(());
}
```

Add `last_state_change: Instant` field to `TcpNatEntry` (used for `LAST_ACK_TIMEOUT` reaping):

```rust
struct TcpNatEntry {
    // ... existing fields ...
    /// Wall clock when the entry's state last changed. Used by
    /// LAST_ACK_TIMEOUT reaping in relay_tcp_nat_data so a missing
    /// final ACK doesn't leak the entry forever.
    last_state_change: Instant,
}
```

Initialize at every existing `TcpNatEntry { ... }` literal site (search for `TcpNatEntry {` in slirp.rs). Set to `Instant::now()` everywhere.

Run: `cargo test --test network_baseline tcp_half_close_guest_writes_first`. Expected: **still FAIL** — host's FIN doesn't reach the guest yet (relay loop doesn't know about FinWait1).

**Commit:** `feat(slirp): FinWait1 transition on guest FIN, host write-side shutdown`

---

### Task 3: Wire `FinWait1 → LastAck` on host EOF + send our FIN to guest

In `src/network/slirp.rs::relay_tcp_nat_data`, find the recv_peek `Ok(0)` arm (host EOF, ~line 1773 area):

```rust
Ok(0) => {
    // Host closed the connection.
    debug!("SLIRP TCP: host EOF on flow guest_port={}", key.guest_src_port);
    match entry.state {
        TcpNatState::Established => {
            // Host closed first → CloseWait. We send FIN to guest;
            // guest may still send data which we'll forward to host
            // (but host's write side may be closed — that's a guest
            // write failure, not our concern).
            entry.state = TcpNatState::CloseWait;
            entry.last_state_change = Instant::now();
            became_closed = false;  // no longer immediately reap
            // Build FIN to guest below via the existing fin_frame branch.
        }
        TcpNatState::FinWait1 => {
            // Guest closed first; now host has finished writing.
            // Send FIN to guest, transition to LastAck.
            entry.state = TcpNatState::LastAck;
            entry.last_state_change = Instant::now();
            became_closed = false;
        }
        _ => {
            // Already in a closing state or Closed.
            became_closed = false;
        }
    }
}
```

Then update the FIN-emit logic just below (~line 1824):

```rust
// Send FIN if we just transitioned to a state that demands one.
let needs_fin = matches!(
    entry.state,
    TcpNatState::CloseWait | TcpNatState::LastAck
);
if needs_fin && !entry.our_fin_sent {
    fin_frame = Some(build_tcp_packet_static(
        key.dst_ip, SLIRP_GUEST_IP, key.dst_port, key.guest_src_port,
        entry.our_seq, entry.guest_ack,
        TcpControl::Fin, &[],
    ));
    entry.our_seq = entry.our_seq.wrapping_add(1);
    entry.our_fin_sent = true;
    trace!("SLIRP TCP: sent FIN to guest, state={:?}", entry.state);
}
```

Add `our_fin_sent: bool` field to `TcpNatEntry` so we don't re-send FIN on repeat polls (every Established-side recv_peek would otherwise queue another FIN).

Run: `cargo test --test network_baseline tcp_half_close_guest_writes_first`. Expected: **PASS** — host's response data + FIN now flow back through.

**Commit:** `feat(slirp): FinWait1 → LastAck and Established → CloseWait on host EOF`

---

### Task 4: Wire `CloseWait → LastAck` on guest FIN; `LastAck → Closed` on guest's final ACK

Update `handle_tcp_frame` to handle FIN from a CloseWait entry:

```rust
if tcp.fin() {
    match entry.state {
        TcpNatState::Established => {
            // ... Task 2 path: → FinWait1 ...
        }
        TcpNatState::CloseWait => {
            // Host already closed; guest just closed too. ACK the
            // guest's FIN and transition to LastAck.
            entry.guest_ack = seq.wrapping_add(1);
            let ack_frame = build_tcp_packet_static(
                dst_ip, SLIRP_GUEST_IP, dst_port, src_port,
                entry.our_seq, entry.guest_ack,
                TcpControl::None, &[],
            );
            self.inject_to_guest.push(ack_frame);
            entry.state = TcpNatState::LastAck;
            entry.last_state_change = Instant::now();
            return Ok(());
        }
        _ => {
            // Repeat FIN or unexpected — ACK and stay where we are.
        }
    }
}
```

Update the ACK handler to handle the guest's final ACK in LastAck:

```rust
// ACK while in LastAck — guest acknowledged our FIN. Reap.
if tcp.ack() && entry.state == TcpNatState::LastAck {
    debug!("SLIRP TCP: LastAck → Closed for {}:{}", dst_ip, dst_port);
    entry.state = TcpNatState::Closed;
    self.pending_close.push(flow_key);
    return Ok(());
}
```

Place this branch BEFORE the existing "ACK transitions SynReceived → Established" block (otherwise the SynReceived branch would catch ACKs in LastAck — they're mutually exclusive but explicit ordering is clearer).

Run all baseline pins: `cargo test --test network_baseline`. Expected: 18/18 pass.

**Commit:** `feat(slirp): CloseWait → LastAck on guest FIN; LastAck → Closed on final ACK`

---

### Task 5: Failing pin — `tcp_half_close_host_writes_first`

Symmetric mirror of Task 1's pin. Host writes first, then closes; guest replies with data + FIN.

```rust
#[test]
fn tcp_half_close_host_writes_first() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let host_port = listener.local_addr().unwrap().port();

    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        sock.write_all(b"GREETING").unwrap();
        sock.shutdown(std::net::Shutdown::Write).unwrap();
        let mut buf = Vec::new();
        let _ = sock.read_to_end(&mut buf);
        // Mirror back what the guest sent.
        buf
    });

    let mut stack = SlirpBackend::new().unwrap();
    // ... handshake (same shape as Task 1) ...

    // Drive drain_to_guest until we see GREETING + host's FIN.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut received: Vec<u8> = Vec::new();
    let mut saw_host_fin = false;
    while std::time::Instant::now() < deadline {
        for f in drain_n(&mut stack, 1) {
            // Same parsing as Task 1.
        }
        if &received[..] == b"GREETING" && saw_host_fin {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    assert_eq!(&received[..], b"GREETING");
    assert!(saw_host_fin);

    // Guest sends reply data + FIN. Host must receive both.
    let reply = b"REPLY";
    // ACK host's FIN first to advance gateway_seq, then send reply data
    // ... (synthesized guest frames) ...

    let host_received = server.join().unwrap();
    assert_eq!(&host_received[..], b"REPLY", "host must receive guest's reply post-FIN");
}
```

The full test body should mirror Task 1's structure. Run: should PASS post-Task-3 (host EOF triggers CloseWait + FIN to guest correctly).

**Commit:** `test(network): pin tcp_half_close_host_writes_first`

---

### Task 6: `LAST_ACK_TIMEOUT` reaping

In `src/network/slirp.rs`, near the existing `TCP_IDLE_TIMEOUT`:

```rust
const LAST_ACK_TIMEOUT: Duration = Duration::from_secs(60);  // TCP MSL × 2
```

In `relay_tcp_nat_data`'s idle-timeout sweep (after the existing `TCP_IDLE_TIMEOUT` check):

```rust
for (flow_key, entry) in &self.flow_table {
    if let FlowEntry::Tcp(tcp_entry) = entry {
        if tcp_entry.state == TcpNatState::LastAck
            && tcp_entry.last_state_change.elapsed() > LAST_ACK_TIMEOUT
            && !to_remove.contains(flow_key)
        {
            warn!("SLIRP TCP: LastAck timeout for guest_port={}, reaping",
                  // ... key.guest_src_port from match ...);
            to_remove.push(*flow_key);
        }
    }
}
```

(merge with the existing `TCP_IDLE_TIMEOUT` sweep into one pass for cache-friendliness.)

Run baseline pins: 18/18 + 2 new = 20 pass.

**Commit:** `feat(slirp): LAST_ACK_TIMEOUT reaping prevents LastAck entry leak`

---

### Task 7: Failing pin — `tcp_last_ack_timeout_reaps_stale_entry`

Uses the existing `#[cfg(any(test, feature = "bench-helpers"))]` synthetic-injection pattern:

```rust
#[cfg(feature = "bench-helpers")]
#[test]
fn tcp_last_ack_timeout_reaps_stale_entry() {
    let mut stack = SlirpBackend::new().unwrap();
    // ... open a flow, drive it into LastAck via a helper ...
    // Synthesize last_state_change = Instant::now() - 70 seconds (>LAST_ACK_TIMEOUT).
    stack.set_synthetic_last_state_change(guest_port, high_port, /* 70s ago */);
    // One drain_to_guest cycle.
    let mut out = Vec::new();
    stack.drain_to_guest(&mut out);
    // Entry should be gone.
    assert!(stack.tcp_flow_state(guest_port, high_port).is_none(),
            "LastAck entry past LAST_ACK_TIMEOUT must be reaped");
}
```

May need a new bench-helpers helper `set_synthetic_last_state_change` on `SlirpBackend`.

Gate the test with `#[cfg(feature = "bench-helpers")]` per the existing snapshot-rebuild smoke pin pattern (`tests/network_baseline.rs::epoll_set_rebuilt_from_flow_table_smoke`). Default `cargo test` skips it; `cargo test --features bench-helpers -- --test-threads=1` runs it.

**Commit:** `test(network): pin tcp_last_ack_timeout_reaps_stale_entry (bench-helpers)`

---

### Task 8: Drop `#[allow(dead_code)]` on `TcpNatState`

Now that all variants are wired. Verify clippy doesn't complain about any remaining unused variant.

Verify `FinWait2` is actually unused — it should be safe to remove the variant entirely (or document why we keep it for future symmetry). Suggest: remove `FinWait2` variant; if anyone needs the distinction later they can re-add it.

Update the doc comment on `TcpNatState` to reflect the new state machine.

**Commit:** `refactor(slirp): drop allow(dead_code) on TcpNatState; remove unused FinWait2`

---

### Task 9: Phase 6.1 validation gate

Standard contract per `AGENTS.md` plus the new pins.

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features -- --test-threads=1
cargo test --test network_baseline                                    # 20 pass
cargo test --test network_baseline --features bench-helpers -- --test-threads=1   # 21 pass
cargo build --release
scripts/bench-compare.sh --baseline 47868f0 --skip-vm                 # no regressions
```

If `VOID_BOX_KERNEL` and `VOID_BOX_INITRAMFS` are set:

```bash
cargo test --test snapshot_integration -- --ignored --test-threads=1  # passes
cargo test --test conformance -- --ignored --test-threads=1
```

Wall-clock sanity:

```bash
voidbox-network-bench --iterations 3 --bulk-mb 10                     # g2h ≥ 6 Gbps, CRR ~10 ms
```

---

## Out of scope

- TCP window management (Phase 6.3).
- Async outbound connect (Phase 6.2).
- IPv6 (Phase 7).
- Real `FinWait2` distinction — would require kernel-side ACK observation we don't currently have.

## Reviewer pointers

- Verify FIN is sent EXACTLY ONCE per state transition (search for `our_fin_sent` checks).
- Verify the ACK path correctly transitions LastAck → Closed without consuming a SYN-state ACK by mistake.
- Verify `last_state_change` is updated on every state transition, not just initial.
- Verify guest writes during CloseWait are still relayed to host (host has full read side; only its write side is closed, which it learns from our shutdown — but writes from us to it are valid).
- Snapshot interaction: `TcpNatState` serde already exists; new fields (`last_state_change`, `our_fin_sent`) need `#[serde(default)]` for backward compatibility with pre-6.1 snapshots.

## Document history

- 2026-05-04: initial plan written.
