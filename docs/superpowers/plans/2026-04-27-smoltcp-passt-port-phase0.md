# Phase 0 Implementation Plan: Baseline + Trait Extraction

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Mandatory skills for every Rust-touching task** (from the spec):
> `rust-style`, `rustdoc`, `rust-analyzer-ssr`,
> `superpowers:test-driven-development`,
> `superpowers:verification-before-completion`. Do not skip them.
> Use LSP (`goToDefinition`, `findReferences`, `documentSymbol`,
> `workspaceSymbol`) for Rust navigation; never grep/glob Rust source
> when LSP can answer.

**Spec:** [`2026-04-27-smoltcp-passt-port.md`](2026-04-27-smoltcp-passt-port.md)

**Goal:** Land the test/bench baseline, the `NetworkBackend` trait
abstraction, and the `SlirpStack → SlirpBackend` rename, with zero
user-visible behavior change.

**Naming rationale:** The new name is role-based, not
implementation-based. "Slirp" denotes the user-mode-NAT networking
role (same role libslirp / passt / pasta fill); "smoltcp" is just the
library we use to build it. Future siblings — `TapBackend`,
`VhostNetBackend` — follow the same role-based convention. Renaming
to `SmoltcpBackend` would leak the implementation library into the
public type name and lose this symmetry.

**Architecture:** Three additive workstreams (correctness pins, divan
microbenches, wall-clock e2e harness) followed by a mechanical
trait-extraction refactor. Three "broken on purpose" assertions are
introduced in 0A and stay green — they flip in Phases 1, 2, 3
respectively.

**Tech Stack:** Rust 1.88, `smoltcp` 0.11 (wire types only), `divan`
0.1, `tokio` (existing), `std::net::TcpListener` for the e2e harness
host endpoint, `iperf3`/`netperf` invoked from inside the VM for
throughput numbers.

---

## Task structure

The phase has five workstreams (A–E) totaling **25 tasks**. A, B, C are
**independent and can be executed in parallel**. D depends on A
(baseline tests must exist before refactor). E is the final gate.

```
0A correctness baseline ──┐
0B divan microbenches ────┼──→ 0D trait extraction ──→ 0E validation + PR
0C wall-clock harness ────┘
```

---

## Workstream 0A — Correctness baseline (`tests/network_baseline.rs`)

All Layer-1 unit-level pins. Linux-only because `SlirpStack` is
`#[cfg(target_os = "linux")]`.

### Task 0A.1: Test file scaffolding + frame builder helpers

**Files:**
- Create: `tests/network_baseline.rs`
- Modify: `Cargo.toml` (register `[[test]] name = "network_baseline"`)

- [ ] **Step 1: Create the test file with helpers.**

```rust
//! Layer-1 correctness pins for the smoltcp-based SLIRP stack.
//!
//! These tests drive `SlirpStack` directly with synthetic Ethernet
//! frames — no VM, no kernel, no host sockets to outside hosts. The
//! goal is to lock observable behavior (including deliberately broken
//! behavior) so the passt-pattern refactor's diff is legible to
//! reviewers.
//!
//! Three tests assert *broken* behavior on purpose. Each is marked
//! `BROKEN_ON_PURPOSE` and flips in the phase that fixes it:
//!
//! - `tcp_to_host_buffer_drops_at_256kb` — flips in Phase 3
//! - `udp_non_dns_silently_dropped` — flips in Phase 2
//! - `icmp_echo_silently_dropped` — flips in Phase 1
//!
//! Run with: `cargo test --test network_baseline`

#![cfg(target_os = "linux")]

use smoltcp::wire::{
    ArpOperation, ArpPacket, ArpRepr, EthernetAddress, EthernetFrame, EthernetProtocol,
    EthernetRepr, IpProtocol, Ipv4Address, Ipv4Packet, Ipv4Repr, TcpControl, TcpPacket, TcpRepr,
    UdpPacket, UdpRepr,
};
use std::net::{TcpListener, UdpSocket};
use void_box::network::slirp::{
    SlirpStack, GATEWAY_MAC, GUEST_MAC, SLIRP_GATEWAY_IP, SLIRP_GUEST_IP,
};

const GUEST_EPHEMERAL_PORT: u16 = 49152;
const ETH_HDR_LEN: usize = 14;
const IPV4_MIN_HDR_LEN: usize = 20;
const TCP_MIN_HDR_LEN: usize = 20;
const UDP_HDR_LEN: usize = 8;

/// Build a minimal IPv4-over-Ethernet TCP segment from guest to a
/// pretend external IP. Returns the full Ethernet frame bytes.
fn build_tcp_frame(
    dst_ip: Ipv4Address,
    src_port: u16,
    dst_port: u16,
    seq: u32,
    ack: u32,
    control: TcpControl,
    payload: &[u8],
) -> Vec<u8> {
    let tcp_repr = TcpRepr {
        src_port,
        dst_port,
        control,
        seq_number: smoltcp::wire::TcpSeqNumber(seq as i32),
        ack_number: if ack == 0 {
            None
        } else {
            Some(smoltcp::wire::TcpSeqNumber(ack as i32))
        },
        window_len: 65535,
        window_scale: None,
        max_seg_size: None,
        sack_permitted: false,
        sack_ranges: [None, None, None],
        timestamp: None,
        payload,
    };
    let ip_repr = Ipv4Repr {
        src_addr: SLIRP_GUEST_IP,
        dst_addr: dst_ip,
        next_header: IpProtocol::Tcp,
        payload_len: tcp_repr.buffer_len(),
        hop_limit: 64,
    };
    let eth_repr = EthernetRepr {
        src_addr: EthernetAddress(GUEST_MAC),
        dst_addr: EthernetAddress(GATEWAY_MAC),
        ethertype: EthernetProtocol::Ipv4,
    };
    let total = ETH_HDR_LEN + ip_repr.buffer_len() + tcp_repr.buffer_len();
    let mut buf = vec![0u8; total];
    let mut eth = EthernetFrame::new_unchecked(&mut buf[..]);
    eth_repr.emit(&mut eth);
    let mut ip = Ipv4Packet::new_unchecked(&mut buf[ETH_HDR_LEN..]);
    ip_repr.emit(&mut ip, &Default::default());
    let mut tcp = TcpPacket::new_unchecked(
        &mut buf[ETH_HDR_LEN + ip_repr.buffer_len()..],
    );
    tcp_repr.emit(
        &mut tcp,
        &smoltcp::wire::IpAddress::Ipv4(SLIRP_GUEST_IP),
        &smoltcp::wire::IpAddress::Ipv4(dst_ip),
        &Default::default(),
    );
    buf
}

/// Build a UDP-over-Ethernet datagram from guest.
fn build_udp_frame(dst_ip: Ipv4Address, src_port: u16, dst_port: u16, payload: &[u8]) -> Vec<u8> {
    let udp_repr = UdpRepr { src_port, dst_port };
    let ip_repr = Ipv4Repr {
        src_addr: SLIRP_GUEST_IP,
        dst_addr: dst_ip,
        next_header: IpProtocol::Udp,
        payload_len: UDP_HDR_LEN + payload.len(),
        hop_limit: 64,
    };
    let eth_repr = EthernetRepr {
        src_addr: EthernetAddress(GUEST_MAC),
        dst_addr: EthernetAddress(GATEWAY_MAC),
        ethertype: EthernetProtocol::Ipv4,
    };
    let total = ETH_HDR_LEN + ip_repr.buffer_len() + UDP_HDR_LEN + payload.len();
    let mut buf = vec![0u8; total];
    let mut eth = EthernetFrame::new_unchecked(&mut buf[..]);
    eth_repr.emit(&mut eth);
    let mut ip = Ipv4Packet::new_unchecked(&mut buf[ETH_HDR_LEN..]);
    ip_repr.emit(&mut ip, &Default::default());
    let mut udp = UdpPacket::new_unchecked(
        &mut buf[ETH_HDR_LEN + ip_repr.buffer_len()..],
    );
    udp_repr.emit(
        &mut udp,
        &smoltcp::wire::IpAddress::Ipv4(SLIRP_GUEST_IP),
        &smoltcp::wire::IpAddress::Ipv4(dst_ip),
        UDP_HDR_LEN + payload.len(),
        |b| b.copy_from_slice(payload),
        &Default::default(),
    );
    buf
}

/// Parse one emitted frame as a TCP segment if it matches; return
/// `(seq, ack, control, payload_len)` for the matching direction.
fn parse_tcp_to_guest(frame: &[u8]) -> Option<(u32, u32, TcpControl, usize)> {
    let eth = EthernetFrame::new_checked(frame).ok()?;
    if eth.ethertype() != EthernetProtocol::Ipv4 {
        return None;
    }
    let ip = Ipv4Packet::new_checked(eth.payload()).ok()?;
    if ip.next_header() != IpProtocol::Tcp || ip.dst_addr() != SLIRP_GUEST_IP {
        return None;
    }
    let tcp = TcpPacket::new_checked(ip.payload()).ok()?;
    Some((
        tcp.seq_number().0 as u32,
        tcp.ack_number().0 as u32,
        tcp.control(),
        tcp.payload().len(),
    ))
}

/// Drain frames the stack wants to send to the guest, calling `poll`
/// up to `n` times.
fn drain_n(stack: &mut SlirpStack, n: usize) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for _ in 0..n {
        out.extend(stack.poll());
    }
    out
}
```

- [ ] **Step 2: Register the test in `Cargo.toml`.**

```toml
[[test]]
name = "network_baseline"
path = "tests/network_baseline.rs"
```

- [ ] **Step 3: Verify it compiles with no tests yet.**

```bash
cargo test --test network_baseline --no-run
```

Expected: builds clean, "0 tests" reported.

- [ ] **Step 4: Commit.**

```bash
git add tests/network_baseline.rs Cargo.toml
git commit -m "test(network): scaffold network_baseline pins with frame helpers"
```

---

### Task 0A.2: Pin TCP handshake (SYN → SYN-ACK)

**Files:**
- Modify: `tests/network_baseline.rs`

- [ ] **Step 1: Write the test using a host listener.**

Append to `tests/network_baseline.rs`:

```rust
#[test]
fn tcp_handshake_emits_synack() {
    // Bind a host listener on 127.0.0.1 so the stack's connect()
    // succeeds. SLIRP rewrites 10.0.2.2 → 127.0.0.1.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let host_port = listener.local_addr().unwrap().port();

    let mut stack = SlirpStack::new().expect("stack");

    // Guest sends SYN to gateway IP at the listener's port.
    let syn = build_tcp_frame(
        SLIRP_GATEWAY_IP,
        GUEST_EPHEMERAL_PORT,
        host_port,
        1000,
        0,
        TcpControl::Syn,
        &[],
    );
    stack.process_guest_frame(&syn).expect("process syn");

    // Drain — SYN-ACK should be queued.
    let frames = drain_n(&mut stack, 4);
    let synack = frames
        .iter()
        .find_map(|f| parse_tcp_to_guest(f))
        .expect("synack emitted");

    let (_seq, ack, ctrl, _len) = synack;
    assert_eq!(ctrl, TcpControl::Syn, "control flags include SYN+ACK");
    assert_eq!(ack, 1001, "ack = guest_seq + 1");
}
```

- [ ] **Step 2: Run.**

```bash
cargo test --test network_baseline tcp_handshake_emits_synack
```

Expected: PASS. (Note: `TcpControl::Syn` in smoltcp's repr also covers
SYN+ACK when ack number is set; assertion above is loose by
construction — sharpen if smoltcp distinguishes.)

- [ ] **Step 3: If the assertion is wrong** (e.g. smoltcp reports
  `TcpControl::None` with the ACK flag in a separate field), open
  `src/network/slirp.rs` `build_tcp_packet_static` (around line 1102)
  via LSP `goToDefinition` and read what it actually emits. Update the
  assertion to match observed behavior. **Do not modify production
  code** — this test pins what we have today.

- [ ] **Step 4: Commit once green.**

```bash
git add tests/network_baseline.rs
git commit -m "test(network): pin TCP handshake SYN-ACK emission"
```

---

### Task 0A.3: Pin TCP data echo (guest send → host receive → host send → guest receive)

**Files:**
- Modify: `tests/network_baseline.rs`

- [ ] **Step 1: Write the round-trip test.**

```rust
#[test]
fn tcp_data_round_trip() {
    use std::io::{Read, Write};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let host_port = listener.local_addr().unwrap().port();

    // Spawn a thread that accepts and echoes one chunk.
    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut buf = [0u8; 16];
        let n = sock.read(&mut buf).unwrap();
        sock.write_all(&buf[..n]).unwrap();
    });

    let mut stack = SlirpStack::new().expect("stack");

    // SYN
    stack
        .process_guest_frame(&build_tcp_frame(
            SLIRP_GATEWAY_IP,
            GUEST_EPHEMERAL_PORT,
            host_port,
            1000,
            0,
            TcpControl::Syn,
            &[],
        ))
        .unwrap();

    // Drain SYN-ACK; capture our_seq.
    let synack_frames = drain_n(&mut stack, 4);
    let (our_seq, _ack, _ctrl, _len) = synack_frames
        .iter()
        .find_map(|f| parse_tcp_to_guest(f))
        .expect("synack");

    // ACK the SYN-ACK (completes handshake).
    stack
        .process_guest_frame(&build_tcp_frame(
            SLIRP_GATEWAY_IP,
            GUEST_EPHEMERAL_PORT,
            host_port,
            1001,
            our_seq + 1,
            TcpControl::None,
            &[],
        ))
        .unwrap();

    // Send 5 bytes of data.
    stack
        .process_guest_frame(&build_tcp_frame(
            SLIRP_GATEWAY_IP,
            GUEST_EPHEMERAL_PORT,
            host_port,
            1001,
            our_seq + 1,
            TcpControl::Psh,
            b"hello",
        ))
        .unwrap();

    // Wait for server to echo and stack to relay back.
    server.join().unwrap();
    let mut total_payload = 0;
    for _ in 0..40 {
        let frames = drain_n(&mut stack, 1);
        for f in frames.iter() {
            if let Some((_, _, _, len)) = parse_tcp_to_guest(f) {
                total_payload += len;
            }
        }
        if total_payload >= 5 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        total_payload >= 5,
        "expected at least 5 bytes echoed back to guest, got {total_payload}"
    );
}
```

- [ ] **Step 2: Run.** `cargo test --test network_baseline tcp_data_round_trip`

- [ ] **Step 3: Commit.**

```bash
git add tests/network_baseline.rs
git commit -m "test(network): pin TCP guest↔host data round-trip"
```

---

### Task 0A.4: Pin "broken on purpose" — TCP `to_host` 256 KB cliff

**Files:**
- Modify: `tests/network_baseline.rs`

- [ ] **Step 1: Write the test that demonstrates the cliff.**

```rust
/// BROKEN_ON_PURPOSE — flips in Phase 3.
///
/// Today: when guest writes >256 KB to host before host reads,
/// `to_host` buffer overflows and the connection is closed
/// (`slirp.rs:903–910`).
///
/// After Phase 3 (MSG_PEEK + sequence mirroring): the host kernel's
/// socket buffer absorbs the write; no userspace cap, no drop.
#[test]
fn tcp_to_host_buffer_drops_at_256kb() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let host_port = listener.local_addr().unwrap().port();

    // Server that accepts but never reads — forces guest writes to
    // accumulate in our `to_host` buffer.
    let _server = std::thread::spawn(move || {
        let (sock, _) = listener.accept().unwrap();
        std::thread::sleep(std::time::Duration::from_secs(2));
        drop(sock);
    });

    let mut stack = SlirpStack::new().expect("stack");

    // Handshake.
    stack
        .process_guest_frame(&build_tcp_frame(
            SLIRP_GATEWAY_IP,
            GUEST_EPHEMERAL_PORT,
            host_port,
            1000,
            0,
            TcpControl::Syn,
            &[],
        ))
        .unwrap();
    let synack = drain_n(&mut stack, 4)
        .into_iter()
        .find_map(|f| parse_tcp_to_guest(&f))
        .expect("synack");
    let (our_seq, _, _, _) = synack;
    stack
        .process_guest_frame(&build_tcp_frame(
            SLIRP_GATEWAY_IP,
            GUEST_EPHEMERAL_PORT,
            host_port,
            1001,
            our_seq + 1,
            TcpControl::None,
            &[],
        ))
        .unwrap();

    // Push ~300 KB in 1 KB segments. Today, somewhere past 256 KB the
    // stack closes the connection (RST or FIN to guest).
    let mut seq = 1001u32;
    let chunk = vec![b'x'; 1024];
    let mut saw_close = false;
    for _ in 0..300 {
        let _ = stack.process_guest_frame(&build_tcp_frame(
            SLIRP_GATEWAY_IP,
            GUEST_EPHEMERAL_PORT,
            host_port,
            seq,
            our_seq + 1,
            TcpControl::Psh,
            &chunk,
        ));
        seq = seq.wrapping_add(1024);
        for f in drain_n(&mut stack, 1) {
            if let Some((_, _, ctrl, _)) = parse_tcp_to_guest(&f) {
                if matches!(ctrl, TcpControl::Rst | TcpControl::Fin) {
                    saw_close = true;
                }
            }
        }
        if saw_close {
            break;
        }
    }
    assert!(
        saw_close,
        "BROKEN_ON_PURPOSE: today the 256 KB to_host cliff closes the \
         connection. If this assertion fails, Phase 3 may have already \
         landed — flip the assertion to `assert!(!saw_close)`."
    );
}
```

- [ ] **Step 2: Run.** `cargo test --test network_baseline tcp_to_host_buffer_drops_at_256kb`

- [ ] **Step 3: If it doesn't capture the cliff** (e.g. test passes
  300 chunks without close), instrument with `tracing` at `WARN`,
  re-run, and adjust chunk size / count. The cliff is real — the test
  must capture it.

- [ ] **Step 4: Commit.**

```bash
git add tests/network_baseline.rs
git commit -m "test(network): BROKEN_ON_PURPOSE pin — 256 KB to_host cliff"
```

---

### Task 0A.5: Pin TCP rate limit, max concurrent, deny list

**Files:**
- Modify: `tests/network_baseline.rs`

- [ ] **Step 1: Write three clustered tests.**

```rust
#[test]
fn tcp_rate_limit_emits_rst() {
    // 5 conn/s allowance; 10 attempts.
    let mut stack = SlirpStack::with_security(64, 5, vec![]).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let host_port = listener.local_addr().unwrap().port();

    let mut rsts = 0;
    for i in 0..10 {
        stack
            .process_guest_frame(&build_tcp_frame(
                SLIRP_GATEWAY_IP,
                GUEST_EPHEMERAL_PORT + i as u16,
                host_port,
                1000,
                0,
                TcpControl::Syn,
                &[],
            ))
            .unwrap();
        for f in drain_n(&mut stack, 2) {
            if let Some((_, _, ctrl, _)) = parse_tcp_to_guest(&f) {
                if ctrl == TcpControl::Rst {
                    rsts += 1;
                }
            }
        }
    }
    assert!(
        rsts >= 4,
        "expected ≥4 RSTs from rate limit, saw {rsts}"
    );
    drop(listener);
}

#[test]
fn tcp_max_concurrent_emits_rst() {
    let mut stack = SlirpStack::with_security(2, 1000, vec![]).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let host_port = listener.local_addr().unwrap().port();

    // Open 4 distinct connections; cap is 2.
    let mut rsts = 0;
    for i in 0..4 {
        stack
            .process_guest_frame(&build_tcp_frame(
                SLIRP_GATEWAY_IP,
                GUEST_EPHEMERAL_PORT + i,
                host_port,
                1000,
                0,
                TcpControl::Syn,
                &[],
            ))
            .unwrap();
        for f in drain_n(&mut stack, 2) {
            if let Some((_, _, ctrl, _)) = parse_tcp_to_guest(&f) {
                if ctrl == TcpControl::Rst {
                    rsts += 1;
                }
            }
        }
    }
    assert!(rsts >= 1, "expected RST after concurrent limit, saw {rsts}");
    drop(listener);
}

#[test]
fn tcp_deny_list_emits_rst() {
    use ipnet::Ipv4Net;
    let deny: Vec<Ipv4Net> = vec!["169.254.169.254/32".parse().unwrap()];
    let mut stack = SlirpStack::with_security(64, 1000, deny).unwrap();

    stack
        .process_guest_frame(&build_tcp_frame(
            Ipv4Address::new(169, 254, 169, 254),
            GUEST_EPHEMERAL_PORT,
            80,
            1000,
            0,
            TcpControl::Syn,
            &[],
        ))
        .unwrap();
    let rst = drain_n(&mut stack, 2)
        .into_iter()
        .find_map(|f| parse_tcp_to_guest(&f))
        .map(|(_, _, ctrl, _)| ctrl == TcpControl::Rst);
    assert_eq!(rst, Some(true), "deny-list IP must get RST");
}
```

- [ ] **Step 2: Run all three.**

```bash
cargo test --test network_baseline tcp_rate_limit_emits_rst tcp_max_concurrent_emits_rst tcp_deny_list_emits_rst
```

- [ ] **Step 3: Commit.**

```bash
git add tests/network_baseline.rs
git commit -m "test(network): pin TCP rate limit, concurrent cap, deny list"
```

---

### Task 0A.6: Pin ARP behavior

**Files:**
- Modify: `tests/network_baseline.rs`

- [ ] **Step 1: Add ARP frame builder and three tests.**

```rust
fn build_arp_request(target_ip: Ipv4Address) -> Vec<u8> {
    let arp_repr = ArpRepr::EthernetIpv4 {
        operation: ArpOperation::Request,
        source_hardware_addr: EthernetAddress(GUEST_MAC),
        source_protocol_addr: SLIRP_GUEST_IP,
        target_hardware_addr: EthernetAddress([0; 6]),
        target_protocol_addr: target_ip,
    };
    let eth_repr = EthernetRepr {
        src_addr: EthernetAddress(GUEST_MAC),
        dst_addr: EthernetAddress([0xff; 6]),
        ethertype: EthernetProtocol::Arp,
    };
    let total = ETH_HDR_LEN + arp_repr.buffer_len();
    let mut buf = vec![0u8; total];
    let mut eth = EthernetFrame::new_unchecked(&mut buf[..]);
    eth_repr.emit(&mut eth);
    let mut arp = ArpPacket::new_unchecked(&mut buf[ETH_HDR_LEN..]);
    arp_repr.emit(&mut arp);
    buf
}

fn parse_arp_reply(frame: &[u8]) -> Option<(EthernetAddress, Ipv4Address)> {
    let eth = EthernetFrame::new_checked(frame).ok()?;
    if eth.ethertype() != EthernetProtocol::Arp {
        return None;
    }
    let arp = ArpPacket::new_checked(eth.payload()).ok()?;
    let repr = ArpRepr::parse(&arp).ok()?;
    if let ArpRepr::EthernetIpv4 {
        operation: ArpOperation::Reply,
        source_hardware_addr,
        source_protocol_addr,
        ..
    } = repr
    {
        Some((source_hardware_addr, source_protocol_addr))
    } else {
        None
    }
}

#[test]
fn arp_replies_for_gateway() {
    let mut stack = SlirpStack::new().unwrap();
    stack
        .process_guest_frame(&build_arp_request(SLIRP_GATEWAY_IP))
        .unwrap();
    let reply = drain_n(&mut stack, 2)
        .into_iter()
        .find_map(|f| parse_arp_reply(&f))
        .expect("arp reply for gateway");
    assert_eq!(reply.1, SLIRP_GATEWAY_IP);
    assert_eq!(reply.0, EthernetAddress(GATEWAY_MAC));
}

#[test]
fn arp_replies_for_random_subnet_ip() {
    let mut stack = SlirpStack::new().unwrap();
    stack
        .process_guest_frame(&build_arp_request(Ipv4Address::new(10, 0, 2, 99)))
        .unwrap();
    let reply = drain_n(&mut stack, 2)
        .into_iter()
        .find_map(|f| parse_arp_reply(&f))
        .expect("arp reply for in-subnet IP");
    assert_eq!(reply.0, EthernetAddress(GATEWAY_MAC));
}

#[test]
fn arp_does_not_reply_for_guest_ip() {
    let mut stack = SlirpStack::new().unwrap();
    stack
        .process_guest_frame(&build_arp_request(SLIRP_GUEST_IP))
        .unwrap();
    let reply = drain_n(&mut stack, 2)
        .into_iter()
        .find_map(|f| parse_arp_reply(&f));
    assert!(reply.is_none(), "stack must not claim guest's own IP");
}
```

- [ ] **Step 2: Run.** `cargo test --test network_baseline arp_`

- [ ] **Step 3: Commit.**

```bash
git add tests/network_baseline.rs
git commit -m "test(network): pin ARP reply behavior for gateway and subnet"
```

---

### Task 0A.7: Pin DNS cache and forwarding

**Files:**
- Modify: `tests/network_baseline.rs`

- [ ] **Step 1: Add four DNS tests.** A real recursive resolver is
  required; tests skip cleanly if no nameserver is reachable.

```rust
fn build_dns_query(xid: u16, qname: &[u8]) -> Vec<u8> {
    use void_box::network::slirp::SLIRP_DNS_IP;
    // Minimal DNS query: header + QNAME + QTYPE=A + QCLASS=IN
    let mut payload = Vec::new();
    payload.extend_from_slice(&xid.to_be_bytes()); // ID
    payload.extend_from_slice(&[0x01, 0x00]); // standard query, RD=1
    payload.extend_from_slice(&[0x00, 0x01]); // QDCOUNT=1
    payload.extend_from_slice(&[0x00, 0x00]); // ANCOUNT
    payload.extend_from_slice(&[0x00, 0x00]); // NSCOUNT
    payload.extend_from_slice(&[0x00, 0x00]); // ARCOUNT
    payload.extend_from_slice(qname);
    payload.extend_from_slice(&[0x00, 0x01]); // QTYPE=A
    payload.extend_from_slice(&[0x00, 0x01]); // QCLASS=IN
    build_udp_frame(SLIRP_DNS_IP, GUEST_EPHEMERAL_PORT, 53, &payload)
}

fn parse_dns_reply_xid(frame: &[u8]) -> Option<u16> {
    let eth = EthernetFrame::new_checked(frame).ok()?;
    if eth.ethertype() != EthernetProtocol::Ipv4 {
        return None;
    }
    let ip = Ipv4Packet::new_checked(eth.payload()).ok()?;
    if ip.next_header() != IpProtocol::Udp {
        return None;
    }
    let udp = UdpPacket::new_checked(ip.payload()).ok()?;
    if udp.src_port() != 53 {
        return None;
    }
    let p = udp.payload();
    if p.len() < 2 {
        return None;
    }
    Some(u16::from_be_bytes([p[0], p[1]]))
}

// `\x07example\x03com\x00`
const QNAME_EXAMPLE_COM: &[u8] = b"\x07example\x03com\x00";

#[test]
fn dns_query_resolves() {
    let mut stack = match SlirpStack::new() {
        Ok(s) => s,
        Err(_) => return, // no /etc/resolv.conf; skip
    };
    stack
        .process_guest_frame(&build_dns_query(0x1234, QNAME_EXAMPLE_COM))
        .unwrap();
    // Resolution is async on net-poll thread. Drain up to 20× 100ms.
    let mut got = None;
    for _ in 0..20 {
        for f in drain_n(&mut stack, 1) {
            if let Some(xid) = parse_dns_reply_xid(&f) {
                got = Some(xid);
            }
        }
        if got.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if got.is_none() {
        eprintln!("skip: no upstream DNS reachable");
        return;
    }
    assert_eq!(got, Some(0x1234));
}

#[test]
fn dns_cache_keys_by_question_not_xid() {
    let mut stack = match SlirpStack::new() {
        Ok(s) => s,
        Err(_) => return,
    };
    // Warm cache with xid=1.
    stack
        .process_guest_frame(&build_dns_query(0x0001, QNAME_EXAMPLE_COM))
        .unwrap();
    for _ in 0..20 {
        let _ = drain_n(&mut stack, 1);
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    // Query with xid=2 — should hit cache and reply with xid=2.
    stack
        .process_guest_frame(&build_dns_query(0x0002, QNAME_EXAMPLE_COM))
        .unwrap();
    let frames = drain_n(&mut stack, 4);
    let xid = frames.iter().find_map(|f| parse_dns_reply_xid(f));
    if xid.is_none() {
        eprintln!("skip: cache warmup did not complete");
        return;
    }
    assert_eq!(xid, Some(0x0002), "cache must rewrite xid on hit");
}
```

- [ ] **Step 2: Run.**

```bash
cargo test --test network_baseline dns_
```

- [ ] **Step 3: Commit.**

```bash
git add tests/network_baseline.rs
git commit -m "test(network): pin DNS resolution and cache xid-rewrite"
```

---

### Task 0A.8: Pin "broken on purpose" — UDP non-DNS dropped

**Files:**
- Modify: `tests/network_baseline.rs`

- [ ] **Step 1: Write the dropped-on-purpose test.**

```rust
/// BROKEN_ON_PURPOSE — flips in Phase 2.
///
/// Today: UDP datagrams to any port other than 53 are silently
/// dropped (`slirp.rs:637` "drop silently"). A bound host UDP socket
/// receives nothing.
#[test]
fn udp_non_dns_silently_dropped() {
    // Bind a host UDP socket; we'll prove nothing arrives.
    let host_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let host_port = host_sock.local_addr().unwrap().port();
    host_sock
        .set_read_timeout(Some(std::time::Duration::from_millis(200)))
        .unwrap();

    let mut stack = SlirpStack::new().unwrap();
    stack
        .process_guest_frame(&build_udp_frame(
            SLIRP_GATEWAY_IP,
            GUEST_EPHEMERAL_PORT,
            host_port,
            b"hello",
        ))
        .unwrap();
    let _ = drain_n(&mut stack, 4);

    let mut buf = [0u8; 32];
    let received = host_sock.recv(&mut buf).is_ok();
    assert!(
        !received,
        "BROKEN_ON_PURPOSE: today UDP-to-non-53 is dropped. \
         If this fires, Phase 2 likely landed — flip to assert!(received)."
    );
}
```

- [ ] **Step 2: Run.** `cargo test --test network_baseline udp_non_dns_silently_dropped`

- [ ] **Step 3: Commit.**

```bash
git add tests/network_baseline.rs
git commit -m "test(network): BROKEN_ON_PURPOSE pin — UDP non-DNS dropped"
```

---

### Task 0A.9: Pin "broken on purpose" — ICMP echo dropped

**Files:**
- Modify: `tests/network_baseline.rs`

- [ ] **Step 1: Write the dropped-on-purpose test.**

```rust
/// BROKEN_ON_PURPOSE — flips in Phase 1.
///
/// Today: ICMP echo requests are silently dropped at
/// `slirp.rs:637`. Phase 1 adds `IPPROTO_ICMP SOCK_DGRAM` echo
/// translation.
#[test]
fn icmp_echo_silently_dropped() {
    // Build a minimal ICMP echo request as an IPv4 packet inside an
    // Ethernet frame. We don't have an `IcmpRepr` builder set up; do
    // it by hand against smoltcp wire types.
    use smoltcp::wire::{Icmpv4Packet, Icmpv4Repr};

    let icmp_repr = Icmpv4Repr::EchoRequest {
        ident: 0xbeef,
        seq_no: 1,
        data: b"ping",
    };
    let ip_repr = Ipv4Repr {
        src_addr: SLIRP_GUEST_IP,
        dst_addr: Ipv4Address::new(8, 8, 8, 8),
        next_header: IpProtocol::Icmp,
        payload_len: icmp_repr.buffer_len(),
        hop_limit: 64,
    };
    let eth_repr = EthernetRepr {
        src_addr: EthernetAddress(GUEST_MAC),
        dst_addr: EthernetAddress(GATEWAY_MAC),
        ethertype: EthernetProtocol::Ipv4,
    };
    let total = ETH_HDR_LEN + ip_repr.buffer_len() + icmp_repr.buffer_len();
    let mut buf = vec![0u8; total];
    let mut eth = EthernetFrame::new_unchecked(&mut buf[..]);
    eth_repr.emit(&mut eth);
    let mut ip = Ipv4Packet::new_unchecked(&mut buf[ETH_HDR_LEN..]);
    ip_repr.emit(&mut ip, &Default::default());
    let mut icmp = Icmpv4Packet::new_unchecked(
        &mut buf[ETH_HDR_LEN + ip_repr.buffer_len()..],
    );
    icmp_repr.emit(&mut icmp, &Default::default());

    let mut stack = SlirpStack::new().unwrap();
    stack.process_guest_frame(&buf).unwrap();
    let frames = drain_n(&mut stack, 4);

    let saw_icmp_reply = frames.iter().any(|f| {
        EthernetFrame::new_checked(f.as_slice())
            .ok()
            .and_then(|e| {
                if e.ethertype() != EthernetProtocol::Ipv4 {
                    return None;
                }
                Ipv4Packet::new_checked(e.payload()).ok().map(|ip| {
                    ip.next_header() == IpProtocol::Icmp
                        && ip.dst_addr() == SLIRP_GUEST_IP
                })
            })
            .unwrap_or(false)
    });
    assert!(
        !saw_icmp_reply,
        "BROKEN_ON_PURPOSE: today ICMP echo is dropped. \
         Phase 1 should flip this to assert!(saw_icmp_reply)."
    );
}
```

- [ ] **Step 2: Run.** `cargo test --test network_baseline icmp_echo_silently_dropped`

- [ ] **Step 3: Commit.**

```bash
git add tests/network_baseline.rs
git commit -m "test(network): BROKEN_ON_PURPOSE pin — ICMP echo dropped"
```

---

## Workstream 0B — divan microbenches (`benches/network.rs`)

### Task 0B.1: Bench file scaffolding + first three benches

**Files:**
- Create: `benches/network.rs`
- Modify: `Cargo.toml` (register `[[bench]] name = "network"`)

- [ ] **Step 1: Create the bench file.**

```rust
//! Divan micro-benchmarks for SLIRP hot paths.
//!
//! Mirrors `benches/startup.rs` in shape. Job: regression detection
//! for the per-packet hot path on the vCPU and net-poll threads.
//!
//! Run with: `cargo bench --bench network`

#![cfg(target_os = "linux")]

use divan::Bencher;
use smoltcp::wire::{
    EthernetAddress, EthernetFrame, EthernetProtocol, EthernetRepr, IpProtocol, Ipv4Address,
    Ipv4Packet, Ipv4Repr, TcpControl, TcpPacket, TcpRepr,
};
use void_box::network::slirp::{
    SlirpStack, GATEWAY_MAC, GUEST_MAC, SLIRP_GATEWAY_IP, SLIRP_GUEST_IP,
};

fn main() {
    divan::main();
}

fn build_syn(src_port: u16, dst_port: u16) -> Vec<u8> {
    let tcp = TcpRepr {
        src_port,
        dst_port,
        control: TcpControl::Syn,
        seq_number: smoltcp::wire::TcpSeqNumber(1000),
        ack_number: None,
        window_len: 65535,
        window_scale: None,
        max_seg_size: None,
        sack_permitted: false,
        sack_ranges: [None, None, None],
        timestamp: None,
        payload: &[],
    };
    let ip = Ipv4Repr {
        src_addr: SLIRP_GUEST_IP,
        dst_addr: SLIRP_GATEWAY_IP,
        next_header: IpProtocol::Tcp,
        payload_len: tcp.buffer_len(),
        hop_limit: 64,
    };
    let eth = EthernetRepr {
        src_addr: EthernetAddress(GUEST_MAC),
        dst_addr: EthernetAddress(GATEWAY_MAC),
        ethertype: EthernetProtocol::Ipv4,
    };
    let total = 14 + ip.buffer_len() + tcp.buffer_len();
    let mut buf = vec![0u8; total];
    let mut e = EthernetFrame::new_unchecked(&mut buf[..]);
    eth.emit(&mut e);
    let mut ipp = Ipv4Packet::new_unchecked(&mut buf[14..]);
    ip.emit(&mut ipp, &Default::default());
    let mut tcpp = TcpPacket::new_unchecked(&mut buf[14 + ip.buffer_len()..]);
    tcp.emit(
        &mut tcpp,
        &smoltcp::wire::IpAddress::Ipv4(SLIRP_GUEST_IP),
        &smoltcp::wire::IpAddress::Ipv4(SLIRP_GATEWAY_IP),
        &Default::default(),
    );
    buf
}

#[divan::bench]
fn process_syn(bencher: Bencher) {
    let frame = build_syn(49152, 1);
    bencher.bench_local(|| {
        let mut stack = SlirpStack::new().unwrap();
        let _ = stack.process_guest_frame(divan::black_box(&frame));
    });
}

#[divan::bench]
fn poll_idle(bencher: Bencher) {
    let mut stack = SlirpStack::new().unwrap();
    bencher.bench_local(|| {
        let _ = divan::black_box(&mut stack).poll();
    });
}

#[divan::bench]
fn process_arp_request(bencher: Bencher) {
    use smoltcp::wire::{ArpOperation, ArpPacket, ArpRepr};
    let arp_repr = ArpRepr::EthernetIpv4 {
        operation: ArpOperation::Request,
        source_hardware_addr: EthernetAddress(GUEST_MAC),
        source_protocol_addr: SLIRP_GUEST_IP,
        target_hardware_addr: EthernetAddress([0; 6]),
        target_protocol_addr: SLIRP_GATEWAY_IP,
    };
    let eth = EthernetRepr {
        src_addr: EthernetAddress(GUEST_MAC),
        dst_addr: EthernetAddress([0xff; 6]),
        ethertype: EthernetProtocol::Arp,
    };
    let total = 14 + arp_repr.buffer_len();
    let mut buf = vec![0u8; total];
    let mut e = EthernetFrame::new_unchecked(&mut buf[..]);
    eth.emit(&mut e);
    let mut a = ArpPacket::new_unchecked(&mut buf[14..]);
    arp_repr.emit(&mut a);

    bencher.bench_local(|| {
        let mut stack = SlirpStack::new().unwrap();
        let _ = stack.process_guest_frame(divan::black_box(&buf));
    });
}
```

- [ ] **Step 2: Register in `Cargo.toml`.**

```toml
[[bench]]
name = "network"
path = "benches/network.rs"
harness = false
```

- [ ] **Step 3: Build and run.**

```bash
cargo bench --bench network --no-run
cargo bench --bench network process_syn
```

Expected: divan prints timing, e.g. `process_syn  fastest=…us`.

- [ ] **Step 4: Commit.**

```bash
git add benches/network.rs Cargo.toml
git commit -m "bench(network): divan microbenches for SLIRP hot paths"
```

---

### Task 0B.2: Parametric NAT-walk scaling bench

**Files:**
- Modify: `benches/network.rs`

- [ ] **Step 1: Add the parametric bench.** Append:

```rust
/// Open `n` distinct guest→gateway flows, then time `poll()`.
/// This walks the NAT table — `O(n)` today; the unified flow table
/// in Phase 4 should keep it `O(n)` but with smaller constants.
#[divan::bench(args = [1, 100, 1000])]
fn poll_with_n_flows(bencher: Bencher, n: usize) {
    let mut stack = SlirpStack::new().unwrap();
    for i in 0..n {
        let frame = build_syn(49152u16.wrapping_add(i as u16), 1);
        let _ = stack.process_guest_frame(&frame);
    }
    bencher.bench_local(|| {
        let _ = divan::black_box(&mut stack).poll();
    });
}
```

- [ ] **Step 2: Run.**

```bash
cargo bench --bench network poll_with_n_flows
```

- [ ] **Step 3: Commit.**

```bash
git add benches/network.rs
git commit -m "bench(network): parametric NAT-walk scaling at 1/100/1000 flows"
```

---

### Task 0B.3: DNS cache hit/miss benches

**Files:**
- Modify: `benches/network.rs`

- [ ] **Step 1: Append DNS benches.**

```rust
fn build_dns_query_for_bench(xid: u16) -> Vec<u8> {
    use smoltcp::wire::{UdpPacket, UdpRepr};
    use void_box::network::slirp::SLIRP_DNS_IP;
    let mut payload = Vec::new();
    payload.extend_from_slice(&xid.to_be_bytes());
    payload.extend_from_slice(&[0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
    payload.extend_from_slice(b"\x07example\x03com\x00");
    payload.extend_from_slice(&[0x00, 0x01, 0x00, 0x01]);

    let udp_repr = UdpRepr {
        src_port: 49152,
        dst_port: 53,
    };
    let ip_repr = Ipv4Repr {
        src_addr: SLIRP_GUEST_IP,
        dst_addr: SLIRP_DNS_IP,
        next_header: IpProtocol::Udp,
        payload_len: 8 + payload.len(),
        hop_limit: 64,
    };
    let eth = EthernetRepr {
        src_addr: EthernetAddress(GUEST_MAC),
        dst_addr: EthernetAddress(GATEWAY_MAC),
        ethertype: EthernetProtocol::Ipv4,
    };
    let total = 14 + ip_repr.buffer_len() + 8 + payload.len();
    let mut buf = vec![0u8; total];
    let mut e = EthernetFrame::new_unchecked(&mut buf[..]);
    eth.emit(&mut e);
    let mut ip = Ipv4Packet::new_unchecked(&mut buf[14..]);
    ip_repr.emit(&mut ip, &Default::default());
    let mut udp = UdpPacket::new_unchecked(&mut buf[14 + ip_repr.buffer_len()..]);
    udp_repr.emit(
        &mut udp,
        &smoltcp::wire::IpAddress::Ipv4(SLIRP_GUEST_IP),
        &smoltcp::wire::IpAddress::Ipv4(SLIRP_DNS_IP),
        8 + payload.len(),
        |b| b.copy_from_slice(&payload),
        &Default::default(),
    );
    buf
}

#[divan::bench]
fn dns_cache_miss(bencher: Bencher) {
    let frame = build_dns_query_for_bench(1);
    bencher.bench_local(|| {
        let mut stack = SlirpStack::new().unwrap();
        let _ = stack.process_guest_frame(divan::black_box(&frame));
    });
}

#[divan::bench]
fn dns_cache_hit(bencher: Bencher) {
    // Warm cache by injecting one query and polling resolution.
    let mut stack = SlirpStack::new().unwrap();
    let warm = build_dns_query_for_bench(1);
    let _ = stack.process_guest_frame(&warm);
    for _ in 0..20 {
        let _ = stack.poll();
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let hit = build_dns_query_for_bench(2);
    bencher.bench_local(|| {
        let _ = divan::black_box(&mut stack).process_guest_frame(divan::black_box(&hit));
    });
}
```

- [ ] **Step 2: Run.** `cargo bench --bench network dns_`

- [ ] **Step 3: Commit.**

```bash
git add benches/network.rs
git commit -m "bench(network): DNS cache hit and miss paths"
```

---

### Task 0B.4: Wire CI extension

**Files:**
- Modify: `.github/workflows/startup-bench.yml` (add a `network` step)

- [ ] **Step 1: Read the existing workflow** to learn the regression
  threshold mechanism.

```bash
cat .github/workflows/startup-bench.yml
```

- [ ] **Step 2: Add a parallel job/step** that runs
  `cargo bench --bench network` and compares against `main` baseline
  using the same mechanism the startup bench uses. Concrete diff
  depends on what's already there — match the pattern; do not
  duplicate infrastructure.

- [ ] **Step 3: Push to a feature branch and verify the workflow
  runs.** If the divan output format the existing workflow expects
  doesn't match, adjust the workflow rather than divan output (divan
  has a single canonical JSON format; rely on it).

- [ ] **Step 4: Commit.**

```bash
git add .github/workflows/startup-bench.yml
git commit -m "ci(bench): include network microbenches in regression gate"
```

---

## Workstream 0C — Wall-clock e2e harness (`voidbox-network-bench`)

### Task 0C.1: Binary scaffold

**Files:**
- Create: `src/bin/voidbox-network-bench/main.rs`
- Modify: `Cargo.toml` (register `[[bin]] name = "voidbox-network-bench"`)

- [ ] **Step 1: Create the binary scaffold.**

```rust
//! Wall-clock end-to-end network benchmark harness.
//!
//! Boots a real VM and measures TCP throughput, RR/CRR latency, and
//! UDP DNS qps inside the guest. Output is JSON for diffing against
//! a baseline.
//!
//! Mirrors `voidbox-startup-bench` in CLI shape and lifecycle.
//!
//! Linux-only because the smoltcp-based SLIRP stack is Linux-only.

#![cfg(target_os = "linux")]

use clap::Parser;
use serde::Serialize;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(version, about = "VoidBox network benchmark harness")]
struct Cli {
    /// Number of iterations per metric.
    #[arg(long, default_value_t = 5)]
    iterations: u32,

    /// Output JSON file. If omitted, prints to stdout.
    #[arg(long)]
    output: Option<PathBuf>,

    /// Skip throughput measurements (useful for fast smoke runs).
    #[arg(long, default_value_t = false)]
    no_throughput: bool,
}

#[derive(Serialize, Debug, Default)]
struct Report {
    tcp_throughput_g2h_mbps: Option<f64>,
    tcp_throughput_h2g_mbps: Option<f64>,
    tcp_rr_latency_us_p50: Option<f64>,
    tcp_rr_latency_us_p99: Option<f64>,
    tcp_crr_latency_us_p50: Option<f64>,
    udp_dns_qps: Option<f64>,
    icmp_rr_latency_us_p50: Option<f64>, // None today; populated post-Phase-1
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let mut report = Report::default();

    eprintln!("voidbox-network-bench: scaffold (no measurements yet)");
    let _ = (cli.iterations, &cli.output, cli.no_throughput, &mut report);

    let json = serde_json::to_string_pretty(&report)?;
    match cli.output {
        Some(path) => std::fs::write(path, json)?,
        None => println!("{json}"),
    }
    Ok(())
}

#[allow(dead_code)]
fn percentile(samples: &mut [Duration], p: f64) -> Duration {
    samples.sort();
    let idx = ((samples.len() as f64) * p).clamp(0.0, samples.len() as f64 - 1.0) as usize;
    samples[idx]
}
```

- [ ] **Step 2: Register in `Cargo.toml`.**

```toml
[[bin]]
name = "voidbox-network-bench"
path = "src/bin/voidbox-network-bench/main.rs"
```

- [ ] **Step 3: Build.**

```bash
cargo build --bin voidbox-network-bench
```

- [ ] **Step 4: Smoke run.**

```bash
cargo run --bin voidbox-network-bench
```

Expected: prints JSON with all `null` fields.

- [ ] **Step 5: Commit.**

```bash
git add src/bin/voidbox-network-bench Cargo.toml
git commit -m "bench(network): voidbox-network-bench binary scaffold"
```

---

### Task 0C.2: TCP throughput measurement

**Files:**
- Modify: `src/bin/voidbox-network-bench/main.rs`

- [ ] **Step 1: Read the existing startup-bench harness** to learn
  the VM lifecycle pattern.

```bash
# Use LSP `documentSymbol` on src/bin/voidbox-startup-bench/main.rs
# to map its functions, then read the run loop.
```

- [ ] **Step 2: Implement `measure_tcp_throughput`** that:
  1. Starts a host-side iperf3 server (or a Rust echo loop on a
     TCP socket).
  2. Boots a VM whose initramfs includes `iperf3`.
  3. Execs `iperf3 -c 10.0.2.2 -t 5 -p <port> --json` inside the
     guest via the existing `ControlChannel::exec`.
  4. Parses the JSON, extracts bits-per-second, returns Mbps.
  5. Stops the VM.
- [ ] **Step 3:** Wire the function into `main` for both directions
  (g2h, h2g) and populate `report.tcp_throughput_*`.
- [ ] **Step 4: Smoke run.**

```bash
cargo run --bin voidbox-network-bench -- --iterations 1
```

- [ ] **Step 5: Commit.**

```bash
git add src/bin/voidbox-network-bench/main.rs
git commit -m "bench(network): TCP throughput via iperf3 inside VM"
```

> **Note for the implementer:** the test image
> (`/tmp/void-box-test-rootfs.cpio.gz`) does not include `iperf3` by
> default. Either extend `scripts/build_test_image.sh` to include it,
> or write a hand-rolled echo loop in Rust that ships with the
> harness. The latter is simpler and recommended — see passt's
> `test/perf/` for the methodology to copy.

---

### Task 0C.3: RR / CRR latency

**Files:**
- Modify: `src/bin/voidbox-network-bench/main.rs`

- [ ] **Step 1: Implement `measure_rr_latency`** — open a TCP echo
  socket on the host, run a guest-side loop that does
  `connect+send+recv+close` (CRR) or `send+recv` on a kept-open
  connection (RR), record `iterations` samples, return p50/p99 in µs.
- [ ] **Step 2:** Wire into `main`. Populate
  `report.tcp_rr_latency_us_*` and `report.tcp_crr_latency_us_p50`.
- [ ] **Step 3: Run.**

```bash
cargo run --bin voidbox-network-bench -- --iterations 100 --no-throughput
```

- [ ] **Step 4: Commit.**

```bash
git add src/bin/voidbox-network-bench/main.rs
git commit -m "bench(network): TCP RR/CRR latency p50/p99"
```

---

### Task 0C.4: UDP DNS qps + JSON baseline

**Files:**
- Modify: `src/bin/voidbox-network-bench/main.rs`

- [ ] **Step 1: Implement `measure_dns_qps`** — guest-side loop
  resolving `example.com` against the SLIRP DNS at 10.0.2.3, count
  successful replies in a fixed window, divide.
- [ ] **Step 2:** Wire into `main`, populate `report.udp_dns_qps`.
- [ ] **Step 3: Run** with `--output baseline.json` and inspect:

```bash
cargo run --bin voidbox-network-bench -- --output baseline.json
cat baseline.json
```

- [ ] **Step 4: Commit and stash a `baseline.json`** as a build
  artifact (do **not** commit it — it's machine-specific). Document
  in the binary's `--help` output how to use it for diffing.

```bash
git add src/bin/voidbox-network-bench/main.rs
git commit -m "bench(network): UDP DNS qps and JSON report output"
```

---

## Workstream 0D — Trait extraction + rename

### Task 0D.1: Define `NetworkBackend` trait

**Files:**
- Modify: `src/network/mod.rs`

- [ ] **Step 1: Use LSP `documentSymbol`** on `src/network/mod.rs` to
  confirm where to insert the trait (after `NetworkConfig`, before
  `TapDevice`).
- [ ] **Step 2: Add the trait.**

```rust
use std::io;

/// A network backend processes raw Ethernet frames between guest and
/// host.
///
/// Implementations must be `Send` so they can be held behind
/// `Arc<Mutex<_>>` and accessed from both the vCPU thread (TX path)
/// and the net-poll thread (RX path).
pub trait NetworkBackend: Send {
    /// Process a raw Ethernet frame sent by the guest.
    ///
    /// Called from the vCPU thread on MMIO write to the TX virtqueue.
    /// Implementations must not block.
    fn process_guest_frame(&mut self, frame: &[u8]) -> io::Result<()>;

    /// Drain Ethernet frames destined for the guest into `out`.
    ///
    /// Called every ~5ms from the net-poll thread. Frames are
    /// complete Ethernet payloads — no virtio-net header (the caller
    /// prepends that). The buffer is reused across calls to avoid
    /// per-poll allocation.
    fn drain_to_guest(&mut self, out: &mut Vec<Vec<u8>>);

    /// Backend health.
    ///
    /// `false` means the backend has entered an unrecoverable state
    /// and should be reconstructed by the caller. The default
    /// implementation always returns `true`.
    fn is_healthy(&self) -> bool {
        true
    }
}
```

> **Apply `rustdoc` skill:** confirm the doc comment style — summary
> sentence first, no leading "This trait …", `# Errors` /
> `# Panics` if applicable. The above complies.

- [ ] **Step 3: Build.** `cargo check --target-dir target/check`
- [ ] **Step 4: Commit.**

```bash
git add src/network/mod.rs
git commit -m "feat(network): introduce NetworkBackend trait"
```

---

### Task 0D.2: Tighten `SlirpStack::poll` to `drain_to_guest` signature

**Files:**
- Modify: `src/network/slirp.rs`

- [ ] **Step 1: Use LSP `findReferences`** on `SlirpStack::poll` to
  list every call site — these all need to switch to
  `drain_to_guest(&mut out)`.

```bash
# Inside the IDE / via LSP:
#   goToDefinition on `poll` → 392
#   findReferences  on `poll` → list all callers
```

- [ ] **Step 2: Add the new method on `SlirpStack`** (do not yet
  remove `poll` — keep both during the rename to keep the build
  green).

```rust
/// Drain frames destined to the guest into `out`. Reuses the buffer
/// across calls. See `NetworkBackend::drain_to_guest`.
pub fn drain_to_guest(&mut self, out: &mut Vec<Vec<u8>>) {
    out.append(&mut self.poll());
}
```

This is a thin wrapper for now — the real allocation drop happens in
**Task 0D.3** when the `poll` body moves into `drain_to_guest`.

- [ ] **Step 3: Build.** `cargo check`
- [ ] **Step 4: Commit.**

```bash
git add src/network/slirp.rs
git commit -m "refactor(slirp): add drain_to_guest wrapper for trait fit"
```

---

### Task 0D.3: Move `poll` body into `drain_to_guest`, drop the per-call alloc

**Files:**
- Modify: `src/network/slirp.rs`

- [ ] **Step 1: Use LSP `goToDefinition`** on
  `SlirpStack::poll` (around line 392) to land on its body.
- [ ] **Step 2: Refactor.** Move the body of `poll` into
  `drain_to_guest`, replacing every `self.inject_to_guest.drain(..)`
  / `Vec::new()` allocation with appends to `out`.

Before:

```rust
pub fn poll(&mut self) -> Vec<Vec<u8>> {
    // ... existing body that builds and returns Vec<Vec<u8>>
}

pub fn drain_to_guest(&mut self, out: &mut Vec<Vec<u8>>) {
    out.append(&mut self.poll());
}
```

After:

```rust
pub fn drain_to_guest(&mut self, out: &mut Vec<Vec<u8>>) {
    // ... body that pushes into `out` directly
}

#[deprecated(note = "use drain_to_guest")]
pub fn poll(&mut self) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    self.drain_to_guest(&mut out);
    out
}
```

The deprecated `poll` keeps the existing tests/benches working while
0D.4 migrates callers.

- [ ] **Step 3: Build and run baseline tests.**

```bash
cargo check
cargo test --test network_baseline
```

Expected: all baseline pins still green. The deprecation warning
fires from the test file — that's intended; tests migrate in 0D.6.

- [ ] **Step 4: Commit.**

```bash
git add src/network/slirp.rs
git commit -m "refactor(slirp): move poll body into drain_to_guest, drop alloc"
```

---

### Task 0D.4: `impl NetworkBackend for SlirpStack`

**Files:**
- Modify: `src/network/slirp.rs`

- [ ] **Step 1: Add the impl.** Use the existing methods (return type
  for `process_guest_frame` is `Result` — the trait wants
  `io::Result`; bridge in the impl).

```rust
use crate::network::NetworkBackend;
use std::io;

impl NetworkBackend for SlirpStack {
    fn process_guest_frame(&mut self, frame: &[u8]) -> io::Result<()> {
        SlirpStack::process_guest_frame(self, frame)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
    }

    fn drain_to_guest(&mut self, out: &mut Vec<Vec<u8>>) {
        SlirpStack::drain_to_guest(self, out)
    }
}
```

> **Apply `rust-style` skill:** the closure can be a function-pointer
> reference if `e.to_string()` works without arguments — but
> `Error::to_string` takes `&self`, so the closure form is correct.
> The trait method names shadow the inherent names; explicit
> `SlirpStack::method(self, …)` disambiguates per project convention.

- [ ] **Step 2: Build.** `cargo check`
- [ ] **Step 3: Sanity test.**

```rust
// In tests/network_baseline.rs, behind the existing module, append:
#[test]
fn smoltcp_backend_implements_network_backend() {
    fn assert_send<T: Send>() {}
    fn assert_backend<T: NetworkBackend>() {}
    assert_send::<SlirpStack>();
    assert_backend::<SlirpStack>();
}
```

```bash
cargo test --test network_baseline smoltcp_backend_implements_network_backend
```

- [ ] **Step 4: Commit.**

```bash
git add src/network/slirp.rs tests/network_baseline.rs
git commit -m "feat(slirp): impl NetworkBackend for SlirpStack"
```

---

### Task 0D.5: Switch `VirtioNetDevice` to hold `Arc<Mutex<dyn NetworkBackend>>`

**Files:**
- Modify: `src/devices/virtio_net.rs`

- [ ] **Step 1: Use LSP `documentSymbol`** on
  `src/devices/virtio_net.rs` to map its struct + methods.
- [ ] **Step 2: Use LSP `findReferences`** on the field that today
  holds `Arc<Mutex<SlirpStack>>` to know all the access sites.
- [ ] **Step 3: Apply `rust-analyzer-ssr`** to change
  `Arc<Mutex<SlirpStack>>` → `Arc<Mutex<dyn NetworkBackend>>`
  workspace-wide. SSR pattern (run from project root):

```bash
# From the LSP shell or via the `rust-analyzer-ssr` skill:
#   pattern: Arc<Mutex<SlirpStack>>
#   replace: Arc<Mutex<dyn NetworkBackend>>
```

- [ ] **Step 4: Update method bodies that called `poll()`** to call
  `drain_to_guest(&mut buf)` against a reused buffer field.

Before:

```rust
let frames = self.slirp.lock().unwrap().poll();
for frame in frames { /* ... */ }
```

After:

```rust
self.rx_scratch.clear();
self.slirp.lock().unwrap().drain_to_guest(&mut self.rx_scratch);
for frame in self.rx_scratch.drain(..) { /* ... */ }
```

Add `rx_scratch: Vec<Vec<u8>>` to the struct, default-initialized.

- [ ] **Step 5: Build + tests.**

```bash
cargo check
cargo test --test network_baseline
```

- [ ] **Step 6: Commit.**

```bash
git add src/devices/virtio_net.rs
git commit -m "refactor(virtio_net): hold dyn NetworkBackend, reuse rx buffer"
```

---

### Task 0D.6: Update VMM construction sites (cold-boot + snapshot-restore)

**Files:**
- Modify: `src/vmm/mod.rs`

- [ ] **Step 1: Use LSP `findReferences`** on `SlirpStack::new` and
  `SlirpStack::with_security` to find every construction site.
  Expect two: cold boot (around `Vm::new`) and snapshot restore
  (around `restore`). Confirm via the file's `documentSymbol`.

- [ ] **Step 2: Wrap each construction in `Arc<Mutex<…>>`** and bind
  the variable type as `Arc<Mutex<dyn NetworkBackend>>`:

```rust
let backend: Arc<Mutex<dyn NetworkBackend>> = Arc::new(Mutex::new(
    SlirpStack::with_security(max_conn, max_rate, deny.clone())?,
));
```

- [ ] **Step 3: Build + tests.**

```bash
cargo check
cargo test --workspace --all-features
```

- [ ] **Step 4: Run the LSP `workspaceSymbol`** lookup for any
  remaining `SlirpStack` references that should now be hidden behind
  the trait. Anything outside `src/network/` and the construction
  sites is suspect.

- [ ] **Step 5: Commit.**

```bash
git add src/vmm/mod.rs
git commit -m "refactor(vmm): construct network backend behind dyn trait"
```

---

### Task 0D.7: Rename `SlirpStack → SlirpBackend`

**Files:**
- Modify: `src/network/slirp.rs`, `tests/network_baseline.rs`,
  `benches/network.rs`, `src/devices/virtio_net.rs`,
  `src/vmm/mod.rs`, any other references LSP turns up.

The module file `src/network/slirp.rs` keeps its name — only the
type is renamed. (The current filename already aligns with the new
type name, and matches the convention used elsewhere in the repo:
`src/devices/virtio_net.rs` holds `VirtioNetDevice`, not a
`virtio_net_device.rs` file.)

- [ ] **Step 1: Use LSP rename** (`rust-analyzer` rename refactor) on
  `SlirpStack` → `SlirpBackend`. **Do not text-substitute** — the
  rename also touches `tests/network_baseline.rs` imports, the
  `benches/network.rs` imports, and any `pub use` re-exports.

- [ ] **Step 2: Build + run all tests.**

```bash
cargo check
cargo test --workspace --all-features
cargo test --test network_baseline
```

- [ ] **Step 3: Final build.** `cargo check`

- [ ] **Step 4: Commit.**

```bash
git add -A
git commit -m "refactor(network): rename SlirpStack to SlirpBackend"
```

---

## Workstream 0E — Validation + ship

### Task 0E.1: Full validation gate

**Files:** none

- [ ] **Step 1: Format + clippy.**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

- [ ] **Step 2: Workspace tests.**

```bash
cargo test --workspace --all-features
cargo test --doc --workspace --all-features
```

- [ ] **Step 3: Network baseline.**

```bash
cargo test --test network_baseline
```

Expected: all tests pass, including the three `BROKEN_ON_PURPOSE`
pins (they assert *broken* behavior — green is correct).

- [ ] **Step 4: Microbenches no-regression.**

```bash
cargo bench --bench network
```

Compare against `main` baseline (CI does this automatically; do it
locally first).

- [ ] **Step 5: VM suites that touch networking.**

```bash
export VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r)
scripts/build_test_image.sh
export VOID_BOX_INITRAMFS=/tmp/void-box-test-rootfs.cpio.gz
cargo test --test conformance -- --ignored --test-threads=1
cargo test --test snapshot_integration -- --ignored --test-threads=1
cargo test --test e2e_skill_pipeline -- --ignored --test-threads=1
cargo test --test e2e_mount -- --ignored --test-threads=1
```

- [ ] **Step 6: Repo `verify` skill.** Run the project's quality
  gate (`/verify`) — format, clippy, tests, security audit, startup
  bench regression, real-workload smoke.

- [ ] **Step 7: aarch64 cross-check** (per `AGENTS.md`).

```bash
CFLAGS_aarch64_unknown_linux_gnu="--sysroot=/usr/aarch64-redhat-linux/sys-root/fc43" \
  RUSTFLAGS="-D warnings" \
  cargo check --target aarch64-unknown-linux-gnu -p void-box --lib --tests
```

- [ ] **Step 8: macOS build smoke** (if a macOS box is available, or
  via CI). The trait extraction must not break the macOS build —
  `NetworkBackend` lives in `src/network/mod.rs` (cross-platform);
  the `SmoltcpBackend` impl is gated `#[cfg(target_os = "linux")]`.

- [ ] **Step 9:** If any gate fails, fix in place and re-run from
  Step 1. Do not proceed to PR until all gates green.

---

### Task 0E.2: Open the PR

**Files:** none

- [ ] **Step 1: Push the branch.**

```bash
git push -u origin smoltcp-passt-port-phase0
```

- [ ] **Step 2: Open the PR** with body:

```markdown
## Phase 0: baseline + NetworkBackend trait

Implements Phase 0 of `docs/superpowers/plans/2026-04-27-smoltcp-passt-port.md`.

**Zero user-visible behavior change.** This PR lands:

- `tests/network_baseline.rs` — 13 unit-level pins for the smoltcp-based
  SLIRP stack, including three deliberately-broken assertions that
  flip in Phases 1, 2, 3.
- `benches/network.rs` — divan microbenches for SLIRP hot paths
  (process_syn, poll_idle, NAT-walk scaling, DNS cache hit/miss).
- `voidbox-network-bench` — wall-clock e2e harness with metric names
  matching passt's published table.
- `NetworkBackend` trait in `src/network/mod.rs`.
- `SlirpStack` renamed to `SlirpBackend` (role-based name,
  symmetric with future `TapBackend`/`VhostNetBackend`); `poll`
  replaced by `drain_to_guest(&mut Vec<Vec<u8>>)` to drop the
  per-poll allocation.

## Test plan

- [x] cargo fmt / clippy clean
- [x] cargo test --workspace --all-features
- [x] cargo test --test network_baseline
- [x] cargo bench --bench network — no regression
- [x] conformance, snapshot_integration, e2e_skill_pipeline,
      e2e_mount green
- [x] aarch64 cross-check green
- [x] macOS build smoke green
- [x] /verify clean

## Broken on purpose

These three baseline pins assert today's broken behavior. They flip
in subsequent phases — do not "fix" them in this PR:

- `tcp_to_host_buffer_drops_at_256kb` (flips in Phase 3)
- `udp_non_dns_silently_dropped` (flips in Phase 2)
- `icmp_echo_silently_dropped` (flips in Phase 1)
```

- [ ] **Step 3: Tag for review.** Phase 0 is mechanical; the trait
  shape is the only design decision worth a second pair of eyes.

---

## Self-review checklist (run before handing off)

- [ ] Every task has explicit file paths, exact commands, expected
  output.
- [ ] No `TBD`, no "implement appropriately", no "similar to Task N"
  without repeating the code.
- [ ] Three `BROKEN_ON_PURPOSE` pins are present (Tasks 0A.4, 0A.8,
  0A.9) and each names the phase that flips it.
- [ ] Trait surface in 0D.1 matches the spec doc exactly
  (`drain_to_guest` out-param, `is_healthy` default-true).
- [ ] Rename in 0D.7 uses LSP rename (rust-analyzer-ssr), not text
  substitution. Type renames to `SlirpBackend` (role-based, not
  `SmoltcpBackend`).
- [ ] Validation gate in 0E.1 covers fmt, clippy, workspace tests,
  baseline tests, microbenches, VM suites, aarch64 cross-check,
  macOS smoke.
- [ ] All Rust-touching tasks reference `rust-style` / `rustdoc` /
  `rust-analyzer-ssr` where they apply.
