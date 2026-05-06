# Phase 2 Implementation Plan: Generalize UDP (per-flow connected sockets)

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development.
> Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Mandatory skills for every Rust-touching task:**
> `rust-style`, `rustdoc`, `rust-analyzer-ssr`,
> `superpowers:test-driven-development`,
> `superpowers:verification-before-completion`. Use LSP for navigation.

**Spec:** [`2026-04-27-smoltcp-passt-port.md`](2026-04-27-smoltcp-passt-port.md)
**Continues from Phase 1:** [`2026-04-27-smoltcp-passt-port-phase1.md`](2026-04-27-smoltcp-passt-port-phase1.md)

**Goal:** Replace the port-53-only `handle_dns_frame` fast-path with a
general per-flow UDP NAT, mirroring passt's `udp.c::udp_flow_from_tap`
design. Keep the existing DNS cache as a fast-path within the
generalized handler (the cache is actually better than what passt has,
per the spec). Flip the `udp_non_dns_silently_dropped` BROKEN_ON_PURPOSE
pin to verify arbitrary UDP works.

**Architecture:** New `UdpFlowEntry` per `(guest_src_port, dst_ip, dst_port)`.
Each entry owns one connected `UdpSocket`. `handle_udp_frame` routes:
DNS (`SLIRP_DNS_IP:53`) keeps the existing cached/forward path;
everything else creates/reuses a flow and `send_to`s. `relay_udp_flows`
polls each socket for replies and emits UDP frames back to the guest.
Idle timeout reaps inactive flows.

**Tech Stack:** Rust 1.88, `std::net::UdpSocket` (already used for DNS),
`smoltcp::wire::UdpRepr`/`UdpPacket` (already imported), no new deps.

**Branch:** `smoltcp-passt-port-phase0` (continuing on the same branch
through Phase 0 + 1 + 2 — user instruction).

---

## Task structure

7 tasks across two workstreams.

| ID | Workstream | Scope |
|---|---|---|
| 2.1 | impl | Add `UdpFlowEntry` + key + `icmp_echo`-style HashMap field |
| 2.2 | impl | Generalize dispatch: route non-53 UDP to `handle_udp_frame` |
| 2.3 | impl | Implement `relay_udp_flows` host→guest reply path |
| 2.4 | impl | Idle timeout + flow reaping (60s) |
| 2.5 | test | Flip `udp_non_dns_silently_dropped` BROKEN_ON_PURPOSE pin |
| 2.6 | bench | Replace `measure_dns_qps`'s `nc -w1`-bottlenecked impl with a real UDP socket |
| 2.7 | gate | Phase 2 validation gate |

---

## Workstream 2A — Implementation (`src/network/slirp.rs`)

### Task 2.1: `UdpFlowEntry` + per-flow socket helper

**Files:**
- Modify: `src/network/slirp.rs`

- [ ] **Step 1: Define key + entry types** (mirror `IcmpEchoKey`/`IcmpEchoEntry` from Phase 1):

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct UdpFlowKey {
    guest_src_port: u16,
    dst_ip: Ipv4Address,
    dst_port: u16,
}

struct UdpFlowEntry {
    /// Connected `UdpSocket`. The host kernel handles source-port
    /// preservation and reply demux; we just `send_to` and
    /// `recv_from`. Set non-blocking.
    sock: std::net::UdpSocket,
    last_activity: Instant,
}
```

- [ ] **Step 2: Add helper `open_udp_flow_socket(dst: SocketAddr) -> io::Result<UdpSocket>`**

```rust
fn open_udp_flow_socket(dst: std::net::SocketAddr) -> io::Result<std::net::UdpSocket> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0")?;
    sock.set_nonblocking(true)?;
    sock.connect(dst)?;
    Ok(sock)
}
```

`connect()` on a `UdpSocket` doesn't open a TCP-style connection — it
sets the default destination and filters incoming datagrams to that
peer only. This is what passt's per-flow design relies on.

- [ ] **Step 3: Add `udp_flows: HashMap<UdpFlowKey, UdpFlowEntry>` field on `SlirpBackend`.**

Initialize in `with_security` (the canonical constructor) — `new()` and `Default::default()` delegate to it.

- [ ] **Step 4: cargo check** — should compile clean. No behavior wired yet.

- [ ] **Step 5: Commit.**

```bash
git add src/network/slirp.rs
git commit -m "feat(slirp): add UdpFlowEntry + per-flow connected socket helper"
```

---

### Task 2.2: Dispatch non-DNS UDP to `handle_udp_frame`

**Files:**
- Modify: `src/network/slirp.rs`

- [ ] **Step 1: Update `handle_ipv4_frame` to route UDP.** Currently
  (around line 642):

```rust
if dst_ip == SLIRP_DNS_IP && protocol == IpProtocol::Udp {
    return self.handle_dns_frame(&ipv4);
}
```

Change to:

```rust
if protocol == IpProtocol::Udp {
    if dst_ip == SLIRP_DNS_IP {
        return self.handle_dns_frame(&ipv4);
    }
    return self.handle_udp_frame(&ipv4);
}
```

DNS keeps its dedicated handler (cache + upstream forward). Everything else flows through the new path.

- [ ] **Step 2: Add `handle_udp_frame`** as a sibling of `handle_dns_frame`:

```rust
fn handle_udp_frame(&mut self, ipv4: &Ipv4Packet<&[u8]>) -> Result<()> {
    let udp = match UdpPacket::new_checked(ipv4.payload()) {
        Ok(u) => u,
        Err(_) => return Ok(()),
    };
    let payload = udp.payload().to_vec(); // own; mutable borrow of self below
    let key = UdpFlowKey {
        guest_src_port: udp.src_port(),
        dst_ip: ipv4.dst_addr(),
        dst_port: udp.dst_port(),
    };

    // SLIRP gateway translation: 10.0.2.2 → 127.0.0.1 (same trick as TCP).
    let dst_ip_for_socket = if key.dst_ip == SLIRP_GATEWAY_IP {
        std::net::Ipv4Addr::LOCALHOST
    } else {
        std::net::Ipv4Addr::from(key.dst_ip.0)
    };
    let dst = std::net::SocketAddr::from((dst_ip_for_socket, key.dst_port));

    let entry = match self.udp_flows.entry(key) {
        std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
        std::collections::hash_map::Entry::Vacant(v) => {
            let sock = match open_udp_flow_socket(dst) {
                Ok(s) => s,
                Err(e) => {
                    trace!("SLIRP UDP: open flow socket failed: {e}");
                    return Ok(());
                }
            };
            v.insert(UdpFlowEntry { sock, last_activity: Instant::now() })
        }
    };
    entry.last_activity = Instant::now();

    if let Err(e) = entry.sock.send(&payload) {
        trace!("SLIRP UDP: send failed: {e}");
    }
    Ok(())
}
```

- [ ] **Step 3: cargo check + tests.** All 14 baseline tests still pass.
  `udp_non_dns_silently_dropped` continues to pass (no reply path yet).

- [ ] **Step 4: Commit.**

```bash
git add src/network/slirp.rs
git commit -m "feat(slirp): forward non-DNS UDP via per-flow connected sockets"
```

---

### Task 2.3: `relay_udp_flows` host→guest reply path

**Files:**
- Modify: `src/network/slirp.rs`

- [ ] **Step 1: Add `relay_udp_flows`** alongside `relay_icmp_echo`:

```rust
fn relay_udp_flows(&mut self) {
    let now = Instant::now();
    let keys: Vec<UdpFlowKey> = self.udp_flows.keys().copied().collect();
    for key in keys {
        let frame = {
            let Some(entry) = self.udp_flows.get_mut(&key) else { continue; };
            let mut buf = [0u8; 1500];
            match entry.sock.recv(&mut buf) {
                Ok(n) => {
                    entry.last_activity = now;
                    Self::build_udp_reply_to_guest(
                        key.dst_ip, key.dst_port, key.guest_src_port, &buf[..n],
                    )
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(_) => continue,
            }
        };
        if let Some(f) = frame {
            self.inject_to_guest.push(f);
        }
    }
}

fn build_udp_reply_to_guest(
    src_ip: Ipv4Address,
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
) -> Option<Vec<u8>> {
    let udp_repr = UdpRepr { src_port, dst_port };
    let ip_repr = Ipv4Repr {
        src_addr: src_ip,
        dst_addr: SLIRP_GUEST_IP,
        next_header: IpProtocol::Udp,
        payload_len: 8 + payload.len(),
        hop_limit: 64,
    };
    let eth_repr = EthernetRepr {
        src_addr: EthernetAddress(GATEWAY_MAC),
        dst_addr: EthernetAddress(GUEST_MAC),
        ethertype: EthernetProtocol::Ipv4,
    };
    let total = 14 + ip_repr.buffer_len() + 8 + payload.len();
    let mut buf = vec![0u8; total];
    let mut eth = EthernetFrame::new_unchecked(&mut buf[..]);
    eth_repr.emit(&mut eth);
    let mut ip = Ipv4Packet::new_unchecked(&mut buf[14..]);
    ip_repr.emit(&mut ip, &Default::default());
    let mut udp = UdpPacket::new_unchecked(&mut buf[14 + ip_repr.buffer_len()..]);
    udp_repr.emit(
        &mut udp,
        &IpAddress::Ipv4(src_ip),
        &IpAddress::Ipv4(SLIRP_GUEST_IP),
        payload.len(),
        |b| b.copy_from_slice(payload),
        &Default::default(),
    );
    Some(buf)
}
```

Note `payload.len()` (NOT `8 + payload.len()`) for `udp_repr.emit`'s
4th arg — matches the bug we fixed in 0A.7.

- [ ] **Step 2: Wire into `drain_to_guest`.** Find the existing chain:
  `self.relay_tcp_nat_data();` → `self.relay_icmp_echo();` and append
  `self.relay_udp_flows();` after the ICMP relay.

- [ ] **Step 3: cargo check + tests.** Note: `udp_non_dns_silently_dropped`
  is now expected to FAIL — UDP replies actually flow. Don't flip the
  test in this task (Task 2.5 owns that). Run with `--no-fail-fast` to
  confirm only that one test fails.

- [ ] **Step 4: Commit.**

```bash
git add src/network/slirp.rs
git commit -m "feat(slirp): relay UDP flow replies back to guest"
```

---

### Task 2.4: UDP idle timeout + flow reaping

**Files:**
- Modify: `src/network/slirp.rs`

- [ ] **Step 1: Add idle reap to `relay_udp_flows`.** At the start (or
  end) of the function, walk entries and remove those past
  `UDP_IDLE_TIMEOUT`:

```rust
const UDP_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

// At top of relay_udp_flows:
let stale: Vec<UdpFlowKey> = self
    .udp_flows
    .iter()
    .filter(|(_, e)| now.duration_since(e.last_activity) > UDP_IDLE_TIMEOUT)
    .map(|(k, _)| *k)
    .collect();
for k in stale {
    self.udp_flows.remove(&k);
}
```

passt uses `/proc/sys/net/netfilter/nf_conntrack_udp_timeout` for this; we hardcode 60s (the kernel default). Don't read from /proc.

- [ ] **Step 2: cargo check + tests.** No new test for the timeout
  (the test would need to wait 60s; integration cost not worth it).

- [ ] **Step 3: Commit.**

```bash
git add src/network/slirp.rs
git commit -m "feat(slirp): UDP flow idle reap (60s)"
```

---

## Workstream 2B — Test + bench

### Task 2.5: Flip `udp_non_dns_silently_dropped` BROKEN_ON_PURPOSE pin

**Files:**
- Modify: `tests/network_baseline.rs`

- [ ] **Step 1: Find the test** (introduced in 0A.8). Rename to
  `udp_non_dns_round_trips` and rewrite to assert the host receives
  the datagram, then sends a reply that the guest receives.

```rust
/// Phase 2 flipped the BROKEN_ON_PURPOSE assertion: arbitrary UDP
/// (any destination port, not just 53) now round-trips through the
/// per-flow connected-socket NAT.
#[test]
fn udp_non_dns_round_trips() {
    let host_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let host_port = host_sock.local_addr().unwrap().port();
    host_sock
        .set_read_timeout(Some(std::time::Duration::from_millis(500)))
        .unwrap();

    let mut stack = SlirpBackend::new().unwrap();

    // Guest sends "hello" to gateway:host_port (which SLIRP rewrites
    // to 127.0.0.1:host_port).
    stack
        .process_guest_frame(&build_udp_frame(
            SLIRP_GATEWAY_IP,
            GUEST_EPHEMERAL_PORT,
            host_port,
            b"hello",
        ))
        .unwrap();
    let _ = drain_n(&mut stack, 4);

    // Host receives the datagram.
    let mut buf = [0u8; 32];
    let (n, peer) = host_sock.recv_from(&mut buf).expect("host receives guest UDP");
    assert_eq!(&buf[..n], b"hello");

    // Host echoes back.
    host_sock.send_to(&buf[..n], peer).unwrap();

    // Drain — guest should see the reply on its source port.
    let mut saw_reply = false;
    for _ in 0..20 {
        for f in drain_n(&mut stack, 1) {
            let Some(eth) = EthernetFrame::new_checked(f.as_slice()).ok() else { continue; };
            if eth.ethertype() != EthernetProtocol::Ipv4 { continue; }
            let Some(ip) = Ipv4Packet::new_checked(eth.payload()).ok() else { continue; };
            if ip.next_header() != IpProtocol::Udp { continue; }
            let Some(udp_pkt) = UdpPacket::new_checked(ip.payload()).ok() else { continue; };
            if udp_pkt.dst_port() == GUEST_EPHEMERAL_PORT && udp_pkt.payload() == b"hello" {
                saw_reply = true;
                break;
            }
        }
        if saw_reply { break; }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(saw_reply, "guest must receive UDP reply via per-flow NAT");
}
```

- [ ] **Step 2: Run.**

```bash
cargo test --test network_baseline udp_
cargo test --test network_baseline    # confirm 14 pass total
```

- [ ] **Step 3: Commit.**

```bash
git add tests/network_baseline.rs
git commit -m "test(network): flip UDP pin — assert non-DNS round-trips (was BROKEN_ON_PURPOSE)"
```

---

### Task 2.6: Replace `measure_dns_qps` busybox-`nc`-bottlenecked impl

**Files:**
- Modify: `src/bin/voidbox-network-bench/main.rs`

- [ ] **Step 1: Read the current `measure_dns_qps`** to understand the
  existing flow. It currently runs busybox `nc -u -w1` per query in the
  guest, which caps qps at ~1/s (0.5 qps observed) regardless of SLIRP
  speed. With Phase 2's general UDP, we can do something faster.

- [ ] **Step 2: Replace the inner shell loop with a tighter pattern**
  using busybox `dd`-style raw UDP via `/dev/udp/`. busybox `nc` opens
  one connection per invocation and sleeps for the timeout. A loop in
  shell using `awk` to bound iterations:

```sh
end=$(($(date +%s) + 5))
count=0
while [ "$(date +%s)" -lt "$end" ]; do
  printf '\x12\x34\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00\x07example\x03com\x00\x00\x01\x00\x01' \
    | nc -u -w0 -q0 10.0.2.3 53 >/dev/null 2>&1 && count=$((count + 1))
done
echo "qps=$((count / 5))"
```

`-w0` (no idle wait) and `-q0` (close immediately on EOF) prevent the
1s-per-query stall. busybox `nc` may not honor both; if so, accept
that DNS qps stays approximate and remove `measure_dns_qps` entirely
(replacing it with a host-driven measurement that sends UDP through
SLIRP from outside the guest — a smaller, cleaner change).

If neither works reliably: leave the metric `null` with a `WARN`.
The Phase 2 win is correctness (DNS isn't blocked anymore), not
this specific number.

- [ ] **Step 3: Smoke run** with `--iterations 1` and confirm the qps
  metric is non-null and >> 0.5.

- [ ] **Step 4: Commit.**

```bash
git add src/bin/voidbox-network-bench/main.rs
git commit -m "bench(network): use tighter busybox-nc loop for DNS qps"
```

If Step 2 doesn't yield a reliable improvement, commit a smaller
change documenting the limit and move on.

---

## Workstream 2C — Validation

### Task 2.7: Validation gate

**Files:** none (gate only)

- [ ] fmt + clippy clean
- [ ] `cargo test --workspace` clean (modulo the pre-existing
  guest-agent flake we tracked earlier)
- [ ] `cargo test --test network_baseline` 14 pass (the renamed test
  is one of them)
- [ ] `cargo bench --bench network` no regression
- [ ] `cargo test --test snapshot_integration -- --ignored` 8/8 pass
- [ ] Wall-clock smoke run produces non-null `udp_dns_qps` >= Phase 0
  baseline (or stays `null` with documented WARN if Step 2.6 didn't
  improve it)

No PR opened — paused per user instruction. Branch will keep
accumulating phases.

---

## File impact

| File | Approximate LOC |
|---|---|
| `src/network/slirp.rs` | +200 |
| `tests/network_baseline.rs` | +30 / -25 (renamed test) |
| `src/bin/voidbox-network-bench/main.rs` | +30 / -10 |
| **Total** | **~+225** |

## Risks

- **Per-flow socket creation can leak fds** if the idle timeout is
  too long under burst traffic. 60s is generous; consider tightening
  to 30s if memory pressure becomes an issue. Out of scope for this
  phase; default 60s matches kernel conntrack.
- **No port-forwarding configurability yet.** Phase 2 only handles
  outbound UDP from guest. Inbound UDP forwarding (host → guest port
  X) is part of Phase 5 (stateless NAT translation refactor).
- **DNS cache stays.** Some users may expect Phase 2 to invalidate
  it; we don't. Cache only fires on `dst == 10.0.2.3:53`; everything
  else takes the per-flow path.
