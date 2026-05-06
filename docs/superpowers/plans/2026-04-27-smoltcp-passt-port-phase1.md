# Phase 1 Implementation Plan: ICMP Echo via Unprivileged SOCK_DGRAM IPPROTO_ICMP

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development.
> Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Mandatory skills for every Rust-touching task:**
> `rust-style`, `rustdoc`, `rust-analyzer-ssr`,
> `superpowers:test-driven-development`,
> `superpowers:verification-before-completion`. Use LSP for navigation.

**Spec:** [`2026-04-27-smoltcp-passt-port.md`](2026-04-27-smoltcp-passt-port.md)
**Continues from Phase 0:** [`2026-04-27-smoltcp-passt-port-phase0.md`](2026-04-27-smoltcp-passt-port-phase0.md)

**Goal:** Make `ping` work inside guest VMs by relaying ICMP echo
through an unprivileged host kernel socket (`SOCK_DGRAM IPPROTO_ICMP`),
in the style of passt's `icmp.c`. Flip the `icmp_echo_silently_dropped`
BROKEN_ON_PURPOSE pin to assert the new behavior.

**Architecture:** New `IcmpEchoEntry` per `(guest_id, dst_ip)` flow.
Each entry owns one `IPPROTO_ICMP` `SOCK_DGRAM` socket. `handle_icmp_frame`
sends echo requests through the socket; `relay_icmp_echo` polls socket
replies and emits ICMP echo reply frames to the guest. The host kernel
rewrites the ICMP id between guest_id and a kernel-assigned id; we
track the mapping per-flow and translate on the way back.

**Tech Stack:** Rust 1.88, `libc` (existing dep) for `socket(2)` with
`IPPROTO_ICMP`, `smoltcp` 0.11 for `Icmpv4Packet`/`Icmpv4Repr` wire
types (already in use), `std::os::fd::FromRawFd` for the wrap.

**Branch:** `smoltcp-passt-port-phase0` (same branch as Phase 0 — user
explicitly continues here, do not branch).

---

## Cross-platform precondition

Linux requires `net.ipv4.ping_group_range` to permit the calling GID
for unprivileged `IPPROTO_ICMP` sockets. The default on Fedora/Ubuntu
since ~2014 is `0 2147483647` (all gids), but it can be tightened by
admins. Approach:

1. Try to open the socket once at `SlirpBackend::new` (or lazily on
   first ICMP frame). If `socket()` returns `EACCES` or `EPERM`, log a
   one-shot warning and **drop** ICMP frames as before.
2. macOS allows the same syscall unconditionally; no sysctl gate.

This is the *exact* compatibility shape passt uses — see `icmp.c`
in `/home/diego/github/passt`.

---

## Task structure

7 tasks across two workstreams.

| ID | Workstream | Scope |
|---|---|---|
| 1.1 | impl | Add `IcmpEchoEntry` + per-flow socket helper |
| 1.2 | impl | Wire `handle_icmp_frame` for guest→host echo path |
| 1.3 | impl | Wire `relay_icmp_echo` for host→guest reply path |
| 1.4 | impl | Sysctl-fallback to drop on `EACCES` / `EPERM` |
| 1.5 | test | Flip `icmp_echo_silently_dropped` to assert reply |
| 1.6 | bench | Populate `icmp_rr_latency_us_p50` in `voidbox-network-bench` |
| 1.7 | gate | Validation + commit summary |

---

## Workstream 1A — Implementation (`src/network/slirp.rs`)

### Task 1.1: `IcmpEchoEntry` + per-flow socket helper

**Files:**
- Modify: `src/network/slirp.rs`

- [ ] **Step 1: Define a NatKey-style key for ICMP echo.**

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct IcmpEchoKey {
    guest_id: u16,
    dst_ip: Ipv4Address,
}
```

- [ ] **Step 2: Define `IcmpEchoEntry`.**

```rust
struct IcmpEchoEntry {
    /// Host-side socket, `socket(AF_INET, SOCK_DGRAM, IPPROTO_ICMP)`.
    /// Set non-blocking; the kernel handles the ICMP framing.
    sock: std::net::UdpSocket,
    /// The guest's original ICMP id from the echo request. The kernel
    /// assigns its own id when we send via the SOCK_DGRAM ICMP socket;
    /// on reply we translate the kernel id back to `guest_id`.
    guest_id: u16,
    last_activity: std::time::Instant,
}
```

`std::net::UdpSocket` is the wrapper we use — see Step 3 for why.

- [ ] **Step 3: Add a helper `open_icmp_socket() -> io::Result<UdpSocket>`** at module scope:

```rust
fn open_icmp_socket() -> io::Result<std::net::UdpSocket> {
    use std::os::fd::FromRawFd;

    // SAFETY: socket(2) returns -1 on error; we check before wrapping.
    // IPPROTO_ICMP + SOCK_DGRAM is the unprivileged ICMP path: kernel
    // handles ICMP framing, no CAP_NET_RAW required.
    let raw = unsafe {
        libc::socket(
            libc::AF_INET,
            libc::SOCK_DGRAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            libc::IPPROTO_ICMP,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is a valid fd from socket(2); UdpSocket adopts
    // ownership and closes on drop.
    Ok(unsafe { std::net::UdpSocket::from_raw_fd(raw) })
}
```

Rationale: `std::net::UdpSocket` uses the SOCK_DGRAM I/O surface
(`recv_from`, `send_to`); it doesn't care that the underlying protocol
is ICMP rather than UDP. This is the same pattern passt uses (just
with raw fds).

- [ ] **Step 4: Add `icmp_echo: HashMap<IcmpEchoKey, IcmpEchoEntry>` field to `SlirpBackend`.**

Initialize in `SlirpBackend::with_security(...)` and `SlirpBackend::new()`.

- [ ] **Step 5: `cargo check`** — should compile clean. No behavior wired yet.

- [ ] **Step 6: Commit.**

```bash
git add src/network/slirp.rs
git commit -m "feat(slirp): add IcmpEchoEntry + IPPROTO_ICMP socket helper"
```

---

### Task 1.2: `handle_icmp_frame` (guest → host)

**Files:**
- Modify: `src/network/slirp.rs`

- [ ] **Step 1: Update `handle_ipv4_frame` to dispatch ICMP.** Around
  line 654 (the "drop silently" branch), insert before it:

```rust
if protocol == IpProtocol::Icmp {
    return self.handle_icmp_frame(&ipv4);
}
```

- [ ] **Step 2: Add `handle_icmp_frame`** as a sibling of
  `handle_dns_frame`. Body:

```rust
fn handle_icmp_frame(&mut self, ipv4: &Ipv4Packet<&[u8]>) -> Result<()> {
    let icmp = match smoltcp::wire::Icmpv4Packet::new_checked(ipv4.payload()) {
        Ok(p) => p,
        Err(_) => return Ok(()),
    };
    let repr = match smoltcp::wire::Icmpv4Repr::parse(&icmp, &Default::default()) {
        Ok(r) => r,
        Err(_) => return Ok(()),
    };
    let (ident, seq_no, data) = match repr {
        smoltcp::wire::Icmpv4Repr::EchoRequest { ident, seq_no, data } => {
            (ident, seq_no, data)
        }
        _ => return Ok(()), // only echo request handled today
    };

    let key = IcmpEchoKey { guest_id: ident, dst_ip: ipv4.dst_addr() };
    let entry = match self.icmp_echo.entry(key) {
        std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
        std::collections::hash_map::Entry::Vacant(v) => {
            let sock = match open_icmp_socket() {
                Ok(s) => s,
                Err(e) => {
                    // Sysctl-driven fallback handled in Task 1.4.
                    trace!("SLIRP ICMP: open socket failed: {e}");
                    return Ok(());
                }
            };
            v.insert(IcmpEchoEntry {
                sock,
                guest_id: ident,
                last_activity: Instant::now(),
            })
        }
    };
    entry.last_activity = Instant::now();

    // Build a wire ICMP echo packet with seq + data; the kernel will
    // rewrite the ident on send_to.
    let req = smoltcp::wire::Icmpv4Repr::EchoRequest {
        ident: 0, // kernel rewrites
        seq_no,
        data,
    };
    let mut buf = vec![0u8; req.buffer_len()];
    let mut pkt = smoltcp::wire::Icmpv4Packet::new_unchecked(&mut buf);
    req.emit(&mut pkt, &Default::default());

    let dst = std::net::SocketAddr::from((
        std::net::Ipv4Addr::from(ipv4.dst_addr().0),
        0u16, // port ignored for ICMP
    ));
    if let Err(e) = entry.sock.send_to(&buf, dst) {
        trace!("SLIRP ICMP: send_to failed: {e}");
    }
    Ok(())
}
```

- [ ] **Step 3: cargo check + cargo test --test network_baseline.** The
  ICMP test still passes today (assertion is `assert!(!saw_icmp_reply)` —
  no reply yet because reply path is in Task 1.3).

- [ ] **Step 4: Commit.**

```bash
git add src/network/slirp.rs
git commit -m "feat(slirp): forward guest ICMP echo via SOCK_DGRAM IPPROTO_ICMP"
```

---

### Task 1.3: `relay_icmp_echo` (host → guest reply path)

**Files:**
- Modify: `src/network/slirp.rs`

- [ ] **Step 1: Add a `relay_icmp_echo` method** alongside
  `relay_tcp_nat_data`. Body:

```rust
fn relay_icmp_echo(&mut self) {
    // Drain replies from each active ICMP socket and emit echo-reply
    // frames to the guest.
    let now = Instant::now();
    const ICMP_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

    let keys: Vec<IcmpEchoKey> = self.icmp_echo.keys().copied().collect();
    for key in keys {
        let frame = {
            let Some(entry) = self.icmp_echo.get_mut(&key) else { continue; };
            if now.duration_since(entry.last_activity) > ICMP_IDLE_TIMEOUT {
                None // mark for removal below
            } else {
                let mut buf = [0u8; 1500];
                match entry.sock.recv_from(&mut buf) {
                    Ok((n, _addr)) => {
                        entry.last_activity = now;
                        Self::build_icmp_echo_reply_to_guest(
                            key.dst_ip,
                            entry.guest_id,
                            &buf[..n],
                        )
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                    Err(_) => continue,
                }
            }
        };
        match frame {
            None => {
                self.icmp_echo.remove(&key);
            }
            Some(Some(f)) => self.inject_to_guest.push(f),
            Some(None) => {} // build failed; drop silently
        }
    }
}

fn build_icmp_echo_reply_to_guest(
    src_ip: Ipv4Address,
    guest_id: u16,
    raw_icmp: &[u8],
) -> Option<Vec<u8>> {
    use smoltcp::wire::*;
    let icmp = Icmpv4Packet::new_checked(raw_icmp).ok()?;
    let parsed = Icmpv4Repr::parse(&icmp, &Default::default()).ok()?;
    let (seq_no, data) = match parsed {
        Icmpv4Repr::EchoReply { seq_no, data, .. } => (seq_no, data),
        _ => return None,
    };
    let reply = Icmpv4Repr::EchoReply {
        ident: guest_id,
        seq_no,
        data,
    };
    let ip_repr = Ipv4Repr {
        src_addr: src_ip,
        dst_addr: SLIRP_GUEST_IP,
        next_header: IpProtocol::Icmp,
        payload_len: reply.buffer_len(),
        hop_limit: 64,
    };
    let eth_repr = EthernetRepr {
        src_addr: EthernetAddress(GATEWAY_MAC),
        dst_addr: EthernetAddress(GUEST_MAC),
        ethertype: EthernetProtocol::Ipv4,
    };
    let total = 14 + ip_repr.buffer_len() + reply.buffer_len();
    let mut buf = vec![0u8; total];
    let mut eth = EthernetFrame::new_unchecked(&mut buf[..]);
    eth_repr.emit(&mut eth);
    let mut ip = Ipv4Packet::new_unchecked(&mut buf[14..]);
    ip_repr.emit(&mut ip, &Default::default());
    let mut icmp_out = Icmpv4Packet::new_unchecked(&mut buf[14 + ip_repr.buffer_len()..]);
    reply.emit(&mut icmp_out, &Default::default());
    Some(buf)
}
```

- [ ] **Step 2: Wire `relay_icmp_echo` into `drain_to_guest`.** Around
  the existing `self.relay_tcp_nat_data();` call (find via LSP), add
  `self.relay_icmp_echo();` immediately after.

- [ ] **Step 3: cargo check + cargo test --test network_baseline.** All
  13 tests still pass; the broken-on-purpose assertion remains green
  because Task 1.5 hasn't flipped it yet (Task 1.5 will demonstrate the
  reply path actually works).

- [ ] **Step 4: Commit.**

```bash
git add src/network/slirp.rs
git commit -m "feat(slirp): relay ICMP echo replies back to guest"
```

---

### Task 1.4: Sysctl fallback (graceful degrade)

**Files:**
- Modify: `src/network/slirp.rs`

- [ ] **Step 1: Add a once-cell `static`** at module scope to track
  whether ICMP support is available:

```rust
use std::sync::atomic::{AtomicU8, Ordering};

/// Tristate: 0 = unknown, 1 = available, 2 = unavailable.
static ICMP_PROBE: AtomicU8 = AtomicU8::new(0);
```

- [ ] **Step 2: Probe in `open_icmp_socket`** — on the first call, try
  the syscall; if it fails with `EACCES`/`EPERM`, set `ICMP_PROBE = 2`,
  log a one-shot warning, and return `Err`. Subsequent calls short-circuit
  on `2`.

```rust
fn open_icmp_socket() -> io::Result<std::net::UdpSocket> {
    if ICMP_PROBE.load(Ordering::Relaxed) == 2 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "ICMP unprivileged probe previously failed",
        ));
    }
    use std::os::fd::FromRawFd;
    let raw = unsafe {
        libc::socket(
            libc::AF_INET,
            libc::SOCK_DGRAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            libc::IPPROTO_ICMP,
        )
    };
    if raw < 0 {
        let err = io::Error::last_os_error();
        if matches!(err.raw_os_error(), Some(libc::EACCES) | Some(libc::EPERM)) {
            if ICMP_PROBE.swap(2, Ordering::Relaxed) != 2 {
                tracing::warn!(
                    "SLIRP: unprivileged ICMP unavailable on this host \
                     (sysctl net.ipv4.ping_group_range likely restricts \
                     it); ICMP echo from guests will be dropped."
                );
            }
        }
        return Err(err);
    }
    ICMP_PROBE.store(1, Ordering::Relaxed);
    Ok(unsafe { std::net::UdpSocket::from_raw_fd(raw) })
}
```

- [ ] **Step 3: cargo check + tests.** Behavior on Linux/macOS where
  the syscall is permitted is unchanged. On a host with restrictive
  sysctl, the warning fires once and ICMP frames are silently dropped
  (the same behavior as before Phase 1 — the BROKEN_ON_PURPOSE pin
  becomes the steady state for that environment).

- [ ] **Step 4: Commit.**

```bash
git add src/network/slirp.rs
git commit -m "feat(slirp): warn-once + fallback when unprivileged ICMP forbidden"
```

---

## Workstream 1B — Test + bench

### Task 1.5: Flip `icmp_echo_silently_dropped` BROKEN_ON_PURPOSE pin

**Files:**
- Modify: `tests/network_baseline.rs`

- [ ] **Step 1: Find the test** (introduced in Phase 0 task 0A.9).
  Rename it to `icmp_echo_returns_reply` and rewrite the body to
  assert a reply IS observed:

```rust
/// Phase 1 flipped the BROKEN_ON_PURPOSE assertion: the guest now
/// receives an ICMP echo reply via the host's unprivileged
/// `IPPROTO_ICMP SOCK_DGRAM` socket.
#[test]
fn icmp_echo_returns_reply() {
    use smoltcp::wire::{Icmpv4Packet, Icmpv4Repr};

    let icmp_repr = Icmpv4Repr::EchoRequest {
        ident: 0xbeef,
        seq_no: 1,
        data: b"ping",
    };
    let ip_repr = Ipv4Repr {
        src_addr: SLIRP_GUEST_IP,
        // 127.0.0.1 — guaranteed to respond on most hosts via the host
        // kernel's loopback; macOS and Linux both reply to ICMP echo.
        dst_addr: Ipv4Address::new(127, 0, 0, 1),
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

    let mut stack = match SlirpBackend::new() {
        Ok(s) => s,
        Err(_) => {
            eprintln!("skip: SlirpBackend::new failed");
            return;
        }
    };
    if stack.process_guest_frame(&buf).is_err() {
        eprintln!("skip: process_guest_frame failed (likely no ICMP support)");
        return;
    }

    // Poll up to 20 × 50ms for the reply.
    let mut saw_reply = false;
    for _ in 0..20 {
        for f in drain_n(&mut stack, 1) {
            let Some(eth) = EthernetFrame::new_checked(f.as_slice()).ok() else { continue; };
            if eth.ethertype() != EthernetProtocol::Ipv4 { continue; }
            let Some(ip) = Ipv4Packet::new_checked(eth.payload()).ok() else { continue; };
            if ip.next_header() == IpProtocol::Icmp && ip.dst_addr() == SLIRP_GUEST_IP {
                saw_reply = true;
                break;
            }
        }
        if saw_reply { break; }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    if !saw_reply {
        // Sysctl may forbid unprivileged ICMP on some hosts. Skip
        // rather than fail — the warn-once log explains why.
        eprintln!(
            "skip: no ICMP reply received within 1s; \
             sysctl net.ipv4.ping_group_range may forbid unprivileged ICMP"
        );
    }
}
```

- [ ] **Step 2: Run.**

```bash
cargo test --test network_baseline icmp_echo_returns_reply
```

Expected: PASS (or SKIP with the sysctl message on a restrictive host).

- [ ] **Step 3: Run the full suite** to confirm no regression:

```bash
cargo test --test network_baseline
```

Expected: 14 tests pass (the renamed test is one of them).

- [ ] **Step 4: Commit.**

```bash
git add tests/network_baseline.rs
git commit -m "test(network): flip ICMP pin — assert echo reply (was BROKEN_ON_PURPOSE)"
```

---

### Task 1.6: Populate `icmp_rr_latency_us_p50` in `voidbox-network-bench`

**Files:**
- Modify: `src/bin/voidbox-network-bench/main.rs`

- [ ] **Step 1: Add `measure_icmp_rr_latency`** alongside the existing
  measurement functions. Use busybox `ping` (which is in the test
  initramfs) inside the guest:

```bash
ping -c <iters * samples_per_iter> -W 1 -i 0.05 8.8.8.8 \
  | awk '/time=/ { sub(/^.*time=/, ""); sub(/ ms.*/, ""); print }'
```

Each line of output is one RTT in milliseconds; multiply by 1000 for
microseconds, collect, percentile.

The guest exec returns the joined output via the existing
`ControlChannel::exec` API. Parse the lines, build a `Vec<Duration>`,
call `percentile(&mut samples, 0.5)`.

If the guest's ICMP echo fails (sysctl, host kernel, etc.), `ping`
returns a non-zero exit. Treat that as "leave the metric `None`" with
a `WARN` log, same fallback shape as the other measurements.

- [ ] **Step 2: Wire into `main`** — call after the existing TCP/UDP
  measurements; populate `report.icmp_rr_latency_us_p50`.

- [ ] **Step 3: Smoke run.**

```bash
VOID_BOX_KERNEL=$PWD/target/vmlinux-slim-x86_64 \
VOID_BOX_INITRAMFS=/tmp/void-box-test-rootfs.cpio.gz \
  cargo run --release --bin voidbox-network-bench -- --iterations 1 \
  | python3 -m json.tool
```

`icmp_rr_latency_us_p50` should be a non-null number now.

- [ ] **Step 4: Commit.**

```bash
git add src/bin/voidbox-network-bench/main.rs
git commit -m "bench(network): populate ICMP RR latency p50"
```

---

## Workstream 1C — Validation

### Task 1.7: Validation gate + summary commit

**Files:** none (gate only)

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

Expected: 14 tests pass (previously-broken `icmp_echo_silently_dropped`
is now `icmp_echo_returns_reply` and asserts a reply).

- [ ] **Step 4: Microbenches no-regression.**

```bash
cargo bench --bench network
```

Compared to the Phase 0 baseline.

- [ ] **Step 5: VM suites that touch networking** (Linux/KVM):

```bash
export VOID_BOX_KERNEL=$PWD/target/vmlinux-slim-x86_64
export VOID_BOX_INITRAMFS=/tmp/void-box-test-rootfs.cpio.gz
cargo test --test conformance -- --ignored --test-threads=1
cargo test --test snapshot_integration -- --ignored --test-threads=1
cargo test --test e2e_skill_pipeline -- --ignored --test-threads=1
cargo test --test e2e_mount -- --ignored --test-threads=1
```

- [ ] **Step 6: New ICMP RR metric** captured:

```bash
cargo run --release --bin voidbox-network-bench -- --iterations 3 \
  --output /tmp/baseline-network-phase1.json
cat /tmp/baseline-network-phase1.json
```

`icmp_rr_latency_us_p50` should be a non-null number; the other
metrics should be statistically equivalent to Phase 0's baseline.

- [ ] **Step 7: aarch64 cross-check** if available.

- [ ] **Step 8:** No commit needed for validation alone. PR opens
  later when the user is ready (across multiple phases on the same
  branch).

---

## Risks

- **Sysctl-restricted hosts.** If `net.ipv4.ping_group_range` is `1 0`
  (default on some hardened environments), `socket()` returns `EACCES`
  and we silently degrade. The warn-once log + the test's skip path
  handle this. Document in the PR description.
- **macOS portability.** macOS's `IPPROTO_ICMP SOCK_DGRAM` works
  unconditionally, but the rest of `slirp.rs` is already
  `#[cfg(target_os = "linux")]`-gated, so this isn't a practical
  concern in Phase 1 — macOS uses VZ NAT, not SLIRP.
- **ICMP id collision.** Two guest processes pinging different hosts
  with the same id won't collide because the key is
  `(guest_id, dst_ip)`. Two guest processes pinging the *same* host
  with the same id will share an entry — which is correct: replies
  belong to whichever guest sent the matching seq.

## File impact

| File | Change | Approximate LOC |
|---|---|---|
| `src/network/slirp.rs` | `IcmpEchoEntry`, `handle_icmp_frame`, `relay_icmp_echo`, sysctl fallback | +180 |
| `tests/network_baseline.rs` | flip `icmp_echo_silently_dropped` → `icmp_echo_returns_reply` | ~+15/-15 |
| `src/bin/voidbox-network-bench/main.rs` | `measure_icmp_rr_latency` | +50 |
| **Total** | | **~+230** (within the spec's ~150-LOC estimate plus test/bench wiring) |
