# Phase 5 Implementation Plan: Stateless NAT + Port Forwarding

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development.
> Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Mandatory skills for every Rust-touching task:**
> `rust-style`, `rustdoc`, `rust-analyzer-ssr`,
> `superpowers:test-driven-development`,
> `superpowers:verification-before-completion`. Use LSP for navigation.

**Spec:** [`2026-04-27-smoltcp-passt-port.md`](2026-04-27-smoltcp-passt-port.md)
**Continues from Phase 4:** [`2026-04-27-smoltcp-passt-port-phase4.md`](2026-04-27-smoltcp-passt-port-phase4.md)

**Goal:** Two related changes:

1. **Refactor address translation** into a pure
   `nat::translate_inbound(addr) -> SocketAddr` function.
   Today the `SLIRP_GATEWAY_IP (10.0.2.2)` → `127.0.0.1` rewrite
   is inlined in `handle_tcp_frame` and `handle_udp_frame`. Pulling
   it out of the relay code makes the translation logic reviewable
   on its own, sets the shape for IPv6 dual-stack later, and
   prepares the hook point for #2.

2. **Port forwarding** — first user-visible feature in this refactor
   chain. Today the only translation is `10.0.2.2 → loopback`. After
   Phase 5, an operator can say `host:8080 → guest:80` and a TCP/UDP
   connection from a host process to `127.0.0.1:8080` reaches the
   guest's port 80. Config flows: spec → `NetworkConfig::port_forwards`
   → `nat::Rules` → consulted by `translate_inbound`.

**Architecture:**

```rust
// src/network/nat.rs (new file)
pub struct Rules {
    /// Outbound: when guest connects to gateway, where on the host
    /// kernel does that map to? (`SLIRP_GATEWAY_IP → 127.0.0.1`).
    pub gateway_loopback: bool,
    /// Outbound: drop / redirect rules that the deny-list /
    /// metadata-IP filter currently inlines.
    pub deny_cidrs: Vec<Ipv4Net>,
    /// Inbound: host-port → guest-port forwarding (the new feature).
    pub port_forwards: Vec<PortForward>,
}

pub struct PortForward {
    pub proto: ForwardProto,   // Tcp | Udp
    pub host_port: u16,
    pub guest_port: u16,
}

/// Stateless: pure function of (incoming dst address, rules) → host
/// SocketAddr to connect/bind to.
pub fn translate_outbound(rules: &Rules, dst: Ipv4Address, dst_port: u16)
    -> Option<SocketAddr> { ... }
```

`SlirpBackend` holds `nat: Rules` instead of inlining the gateway
rewrite. The relay code calls `translate_outbound` per packet
(it's pure, fast, no state).

**Tech Stack:** Rust 1.88, `ipnet::Ipv4Net` (already in use). No new
deps.

**Branch:** `smoltcp-passt-port-phase0` (continuing on the same
branch — user instruction).

## Non-negotiable invariants (carried from prior phases)

1. **All-Rust** — no opaque process boundary.
2. **Full observability via `tracing`** — every translation decision
   that diverts a connection (loopback rewrite, deny, port-forward)
   emits a `trace!` event with the (rule, src, dst) context.
3. **`cargo test`-driveable** — every behavior change exercised by
   `tests/network_baseline.rs` (no VM needed).
4. **No regression** — all 14 baseline pins, snapshot suite, e2e
   suites, microbenches, wall-clock baselines stay within 5% of the
   Phase 4 numbers.

## Task structure

8 tasks across three workstreams.

| ID | Workstream | Scope |
|---|---|---|
| 5.1 | impl | New module `src/network/nat.rs` with `Rules`, `PortForward`, `ForwardProto`, `translate_outbound` (no callers yet) |
| 5.2 | impl | `SlirpBackend` holds `nat: Rules`; existing `SLIRP_GATEWAY_IP → 127.0.0.1` rewrite + `deny_list` move into `Rules` |
| 5.3 | impl | TCP path consumes `nat::translate_outbound` (replaces the inline rewrite in `handle_tcp_frame`) |
| 5.4 | impl | UDP path consumes `nat::translate_outbound` |
| 5.5 | impl | Wire `port_forwards` from `NetworkConfig` → `Rules`. Inbound forwarding requires a host listener + per-rule accept loop spawned by `SlirpBackend::new` |
| 5.6 | test | New baseline pins: `nat_translate_outbound_loopback_rewrite`, `nat_translate_outbound_deny_list`, `nat_translate_outbound_unmodified`, `tcp_port_forward_inbound` |
| 5.7 | bench | New divan bench `nat_translate_outbound_hot_path` (pure-compute, ns-scale) |
| 5.8 | gate | Phase 5 validation gate |

---

## Workstream 5A — Stateless translation module

### Task 5.1: New `src/network/nat.rs` module

**Files:**
- Create: `src/network/nat.rs`
- Modify: `src/network/mod.rs` (`pub mod nat;`)

- [ ] **Step 1: Create `src/network/nat.rs`**

```rust
//! Stateless address translation for SLIRP.
//!
//! Pure functions that map (guest-visible address, rules) →
//! (host-side SocketAddr to connect/bind to). No per-flow state
//! lives here — the flow table in `slirp.rs` owns that. Translation
//! itself is a function call.

use std::net::{Ipv4Addr, SocketAddr};

use ipnet::Ipv4Net;
use smoltcp::wire::Ipv4Address;

/// Inbound port-forwarding rule — host listener → guest port.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForwardProto {
    Tcp,
    Udp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortForward {
    pub proto: ForwardProto,
    pub host_port: u16,
    pub guest_port: u16,
}

/// Outbound translation rules, derived once at SlirpBackend construction.
#[derive(Clone, Debug, Default)]
pub struct Rules {
    /// If `true`, guest connects to the SLIRP gateway IP map to
    /// `127.0.0.1` on the host. Today this is always `true`; left
    /// configurable so a future TAP backend can flip it off.
    pub gateway_loopback: bool,
    /// CIDRs the guest is not allowed to connect to. Outbound packets
    /// targeting these get `None` from `translate_outbound`.
    pub deny_cidrs: Vec<Ipv4Net>,
    /// Inbound port forwards. Consulted by `SlirpBackend::new` to spawn
    /// listeners; not used by `translate_outbound`.
    pub port_forwards: Vec<PortForward>,
}

/// Translate an outbound packet's destination address.
///
/// Returns `Some(host_addr)` if the packet should be forwarded —
/// loopback for the gateway IP, otherwise the original IP.
/// Returns `None` if the destination is in the deny list.
pub fn translate_outbound(
    rules: &Rules,
    dst: Ipv4Address,
    dst_port: u16,
    gateway_ip: Ipv4Address,
) -> Option<SocketAddr> {
    let dst_ipv4 = Ipv4Addr::from(dst.0);

    // Deny-list check first — explicit block beats any other rule.
    for cidr in &rules.deny_cidrs {
        if cidr.contains(&dst_ipv4) {
            return None;
        }
    }

    let host_ip = if rules.gateway_loopback && dst == gateway_ip {
        Ipv4Addr::LOCALHOST
    } else {
        dst_ipv4
    };

    Some(SocketAddr::from((host_ip, dst_port)))
}
```

- [ ] **Step 2: Register the module** in `src/network/mod.rs`:

```rust
pub mod nat;
```

- [ ] **Step 3: Verify.**

```bash
cargo check
cargo test --test network_baseline
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
git add src/network/nat.rs src/network/mod.rs
git commit -m "feat(network): add nat.rs with stateless translate_outbound (no callers yet)"
```

---

### Task 5.2: `SlirpBackend` holds `nat: Rules`

**Files:**
- Modify: `src/network/slirp.rs`

- [ ] **Step 1: Add field** on `SlirpBackend`:

```rust
nat: nat::Rules,
```

- [ ] **Step 2: Build it in `with_security`** from the existing
  `deny_list` parameter. Today the deny list lives in two places
  (a `Vec<Ipv4Net>` field on `SlirpBackend` and a CLI arg). The
  refactor: `Rules.deny_cidrs` is the new home. The existing
  `deny_list` field becomes redundant once 5.3 + 5.4 land — remove
  it then.

```rust
let nat = nat::Rules {
    gateway_loopback: true,
    deny_cidrs: deny_list.clone(),
    port_forwards: Vec::new(), // wired in 5.5
};
```

- [ ] **Step 3: Don't migrate any call sites yet.** The existing
  inline rewrites in `handle_tcp_frame` / `handle_udp_frame` keep
  working. 5.3 + 5.4 own the cutover.
- [ ] **Step 4: Verify** — all 14 baseline tests still pass.
- [ ] **Step 5: Commit.**

```bash
git add src/network/slirp.rs
git commit -m "refactor(slirp): add nat::Rules field on SlirpBackend (parallel to existing deny_list)"
```

---

### Task 5.3: TCP path consumes `translate_outbound`

**Files:**
- Modify: `src/network/slirp.rs`

- [ ] **Step 1: Find the existing translation in `handle_tcp_frame`**
  (LSP `documentSymbol` — the SYN branch around the `TcpStream::connect`
  call). It currently does:

```rust
// Inline today:
let dst_ip_for_socket = if key.dst_ip == SLIRP_GATEWAY_IP {
    Ipv4Addr::LOCALHOST
} else {
    Ipv4Addr::from(key.dst_ip.0)
};
let dst_addr = SocketAddr::from((dst_ip_for_socket, key.dst_port));

// Plus a separate deny-list check:
for cidr in &self.deny_list {
    if cidr.contains(&dst_ip_for_socket) {
        // send RST, return
    }
}
```

- [ ] **Step 2: Replace with a single `translate_outbound` call:**

```rust
let dst_addr = match nat::translate_outbound(
    &self.nat,
    key.dst_ip,
    key.dst_port,
    SLIRP_GATEWAY_IP,
) {
    Some(addr) => addr,
    None => {
        // Denied. Send RST and return.
        trace!(
            "SLIRP TCP: deny-list reject dst={}:{} from guest_port={}",
            key.dst_ip, key.dst_port, key.guest_src_port
        );
        let rst = build_tcp_rst_to_guest(/* existing args */);
        self.inject_to_guest.push(rst);
        return Ok(());
    }
};
let host_stream = match TcpStream::connect_timeout(&dst_addr, Duration::from_secs(3)) {
    /* existing match */
};
```

- [ ] **Step 3: Preserve every existing tracing event.**
- [ ] **Step 4: Verify** — `tcp_data_round_trip`,
  `tcp_writes_more_than_256kb_succeed`, `tcp_deny_list_emits_rst`,
  `tcp_handshake_emits_synack` all pass.
- [ ] **Step 5: Commit.**

```bash
git add src/network/slirp.rs
git commit -m "refactor(slirp): TCP path uses nat::translate_outbound"
```

---

### Task 5.4: UDP path consumes `translate_outbound`

**Files:**
- Modify: `src/network/slirp.rs`

- [ ] **Step 1: Find** the inline UDP translation in `handle_udp_frame`
  (Phase 2's `dst_ip_for_socket = if key.dst_ip == SLIRP_GATEWAY_IP { LOCALHOST } else { ... };`).
- [ ] **Step 2: Replace** with `nat::translate_outbound(&self.nat, key.dst_ip, key.dst_port, SLIRP_GATEWAY_IP)`.
  On `None` (deny), drop the datagram silently with a `trace!`.
- [ ] **Step 3: Drop the now-unused `deny_list` field** on `SlirpBackend` — both TCP and UDP go through `Rules.deny_cidrs` now. LSP `findReferences` to confirm zero callers.
- [ ] **Step 4: Verify.**

```bash
cargo check
cargo test --test network_baseline udp_non_dns_round_trips
cargo test --test network_baseline                 # 14/14
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
git add src/network/slirp.rs
git commit -m "refactor(slirp): UDP path uses nat::translate_outbound, drop deny_list field"
```

---

## Workstream 5B — Port forwarding (the user-visible feature)

### Task 5.5: Wire `port_forwards` from spec → host listeners

**Files:**
- Modify: `src/network/mod.rs` (`NetworkConfig::port_forwards: Vec<(u16, u16)>` is already there from earlier work — confirm via LSP and use as the source)
- Modify: `src/network/slirp.rs` (`SlirpBackend::with_security` accepts `port_forwards`, populates `nat.port_forwards`, spawns listeners)

This is the only task that ADDS user-visible behavior. The translation
refactor in 5.1–5.4 was no-behavior-change.

- [ ] **Step 1: Define the listener thread shape.** For each
  `PortForward { proto, host_port, guest_port }`:
  - **TCP:** `TcpListener::bind(("127.0.0.1", host_port))` →
    accept thread → on each accept, **inject a synthetic SYN frame**
    into the guest from `SLIRP_GATEWAY_IP:host_port` → `SLIRP_GUEST_IP:guest_port`,
    then proxy bytes between the host TcpStream and the guest's
    response stream (mirrors the existing outbound path but reversed).
  - **UDP:** `UdpSocket::bind(("127.0.0.1", host_port))` →
    similar pattern with synthetic UDP datagrams.

  This is more involved than the outbound path because we have to
  *initiate* a connection from the host side to the guest. The
  guest's listener at `guest_port` must already be accepting; if
  it's not, the host TCP connect will look like ECONNREFUSED to the
  caller.

- [ ] **Step 2: Smallest viable first commit — just plumb the config**:
  - Pass `port_forwards: Vec<PortForward>` through `with_security`.
  - Populate `nat.port_forwards`.
  - Don't actually spawn listeners yet — just store the rules. A
    next commit can add the listener implementation.

- [ ] **Step 3: Smallest viable second commit — TCP forwarding only**:
  - For each TCP `PortForward`, spawn a thread that binds the host
    listener and on each accept, drives the synthetic SYN injection.
  - Keep UDP forwarding as a TODO comment for a follow-up; the TCP
    path is the high-value case.

- [ ] **Step 4: Verify** — test plan in 5.6 covers this.

This task is the single most user-visible piece of the entire SLIRP
refactor chain. Worth landing carefully; consider splitting into
sub-PRs if the diff balloons.

---

## Workstream 5C — Test + bench

### Task 5.6: Baseline pins for translation + port-forward

**Files:**
- Modify: `tests/network_baseline.rs`

- [ ] **Step 1: Pure-translation pins** — exercise `nat::translate_outbound`
  directly without driving `SlirpBackend`:

```rust
#[test]
fn nat_translate_outbound_loopback_rewrite() { /* ... */ }

#[test]
fn nat_translate_outbound_deny_list() { /* ... */ }

#[test]
fn nat_translate_outbound_unmodified_external_ip() { /* ... */ }
```

- [ ] **Step 2: Port-forward end-to-end pin**:

```rust
#[test]
fn tcp_port_forward_inbound() {
    // Bind a guest-side server (synthesized — drives SlirpBackend
    // directly with a SYN/SYN-ACK/FIN sequence to simulate a guest
    // accepting on guest_port).
    // Build SlirpBackend with port_forwards = [{Tcp, host_port, guest_port}].
    // Connect from host to 127.0.0.1:host_port.
    // Assert the connection succeeds and bytes flow through.
}
```

- [ ] **Step 3: Run.**

```bash
cargo test --test network_baseline nat_ tcp_port_forward
cargo test --test network_baseline       # full suite
git add tests/network_baseline.rs
git commit -m "test(network): pin nat::translate_outbound + tcp_port_forward_inbound"
```

---

### Task 5.7: divan bench for `translate_outbound`

**Files:**
- Modify: `benches/network.rs`

- [ ] **Step 1: Add** a pure-compute bench inside `linux_benches`:

```rust
#[divan::bench]
fn nat_translate_outbound_hot_path(bencher: Bencher) {
    use void_box::network::nat::{self, Rules};
    let rules = Rules {
        gateway_loopback: true,
        deny_cidrs: vec!["169.254.0.0/16".parse().unwrap()],
        port_forwards: Vec::new(),
    };
    let dst = SLIRP_GATEWAY_IP;
    bencher.bench_local(|| {
        divan::black_box(nat::translate_outbound(&rules, dst, 80, SLIRP_GATEWAY_IP));
    });
}
```

Expected order of magnitude: tens of nanoseconds per call. If it's
microseconds, something's wrong (allocation in the hot path, etc.) —
investigate.

- [ ] **Step 2: Commit.**

```bash
cargo bench --bench network nat_translate_outbound_hot_path
git add benches/network.rs
git commit -m "bench(network): nat_translate_outbound_hot_path — Phase 5 baseline"
```

---

### Task 5.8: Phase 5 validation gate

**Files:** none.

- [ ] fmt + clippy clean.
- [ ] `cargo test --test network_baseline` — all baseline pins pass
  (count grew by 4 in 5.6).
- [ ] `cargo bench --bench network` — no regression on existing benches;
  new `nat_translate_outbound_hot_path` reports tens of ns.
- [ ] `cargo test --test snapshot_integration -- --ignored` — 8/8.
- [ ] `cargo test --test e2e_mount -- --ignored` — 11/11.
- [ ] `voidbox-network-bench --iterations 3 --bulk-mb 10` — within 5% of Phase 4 numbers.
- [ ] `voidbox-startup-bench --iters 3 --breakdown` — warm phase exits 0; numbers within noise of Phase 4.

## Risks

- **Port-forwarding is new behavior, not refactor.** 5.5 is the most
  failure-prone task because it injects synthetic frames into the
  flow_table from a different code path than the existing relay. If
  the synthetic SYN doesn't match the existing TCP state-machine's
  expectations, connections break in subtle ways. Strong test
  coverage in 5.6 mitigates.
- **Visibility of `nat` types.** Test files and benches need access
  to `Rules`, `PortForward`, `translate_outbound`. The plan above
  uses `pub` everywhere in `nat.rs` — that's the right surface for
  Phase 6+ users (port-forwarding via spec/CLI). Don't `pub(crate)`
  it.

## File impact

| File | Approximate LOC |
|---|---|
| `src/network/nat.rs` | **+90** (new) |
| `src/network/mod.rs` | +1 (`pub mod nat;`) |
| `src/network/slirp.rs` | **−40 / +25** (deny-list field gone, inline rewrites replaced with `translate_outbound` calls; the +25 is for the port-forwarding spawn) |
| `tests/network_baseline.rs` | +120 (4 new tests) |
| `benches/network.rs` | +20 (one bench) |
| **Total** | **~+220** |
