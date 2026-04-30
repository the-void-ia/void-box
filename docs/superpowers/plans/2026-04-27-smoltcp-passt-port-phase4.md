# Phase 4 Implementation Plan: Unified Flow Table

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development.
> Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Mandatory skills for every Rust-touching task:**
> `rust-style`, `rustdoc`, `rust-analyzer-ssr`,
> `superpowers:test-driven-development`,
> `superpowers:verification-before-completion`. Use LSP for navigation.
>
> **Phase 4 is a NO-BEHAVIOR-CHANGE refactor.** Every task ends with
> all 14 baseline pins, all VM suites, and `voidbox-startup-bench`
> warm phase still green. The point is structural cleanup, not new
> capability — temptation to bolt on "while I'm here" features
> should be redirected to Phase 5.

**Spec:** [`2026-04-27-smoltcp-passt-port.md`](2026-04-27-smoltcp-passt-port.md)
**Continues from Phase 3:** [`2026-04-27-smoltcp-passt-port-phase3.md`](2026-04-27-smoltcp-passt-port-phase3.md)

**Goal:** Replace the three per-protocol HashMaps on `SlirpBackend`
(`tcp_nat`, `udp_flows`, `icmp_echo`) with a single `flow_table`
keyed by a `FlowKey` enum, with values held in a `FlowEntry` enum.
Sets up Phase 5 (stateless NAT + port-forwarding) where shared
flow-table operations matter more.

**Architecture:**

```rust
// New types (unified):
enum FlowKey {
    Tcp(TcpNatKey),
    Udp(UdpFlowKey),
    IcmpEcho(IcmpEchoKey),
}

enum FlowEntry {
    Tcp(TcpNatEntry),
    Udp(UdpFlowEntry),
    IcmpEcho(IcmpEchoEntry),
}

// On SlirpBackend:
flow_table: HashMap<FlowKey, FlowEntry>,
```

The per-protocol code paths still match on the variant — this is
"three HashMaps in one wrapper" structurally, not a deep redesign.
The user-visible benefits land later: Phase 5 will reuse
`flow_table` for stateless NAT translation + port-forwarding without
caring which protocol owns each entry.

**Tech Stack:** Rust 1.88, `std::collections::HashMap` (already in
use). No new deps.

**Branch:** `smoltcp-passt-port-phase0` (continuing on the same
branch — user instruction).

## Non-negotiable invariants (carried from Phase 3)

1. **All-Rust** — no opaque process boundary.
2. **Full observability via `tracing`** — every relay continues
   to emit `trace!`/`debug!`/`warn!` at the same observable points.
   The unification must NOT silently drop log lines.
3. **`cargo test`-driveable** — all 14 baseline pins, plus
   `tcp_writes_more_than_256kb_succeed`, must continue passing.
4. **Standard Rust tooling** — LSP, clippy, profiler keep working.

## What this phase explicitly does NOT do

- **No SipHash hasher.** The default `RandomState` already
  randomizes per-process, which is sufficient DoS protection given
  guests can't observe other VMs' hash seeds. SipHash is a Phase 5+
  consideration if and only if profiling shows hash contention,
  which it currently doesn't.
- **No side-indexed entries.** passt's flow table tracks INISIDE
  vs TGTSIDE for each entry; SLIRP is asymmetric (guest is always
  the initiator) so this distinction is moot in our model.
- **No new behavior.** Same RFC compliance, same idle timeouts,
  same packet handling. The pin tests are the contract.

## Task structure

10 tasks across three workstreams. The bench tasks (4.6a–4.6c) land
**after** the migration so they exercise the unified `flow_table`,
not the old per-protocol maps. The validation gate (4.7) compares
the new bench numbers against Phase 3 numbers to verify no
regression from enum dispatch.

| ID | Workstream | Scope |
|---|---|---|
| 4.1 | impl | Define `FlowKey` + `FlowEntry` enums; no callers yet |
| 4.2 | impl | Add `flow_table` field to `SlirpBackend`; populate in parallel with existing maps (no migration yet) |
| 4.3 | impl | Migrate ICMP path to `flow_table`; drop `icmp_echo` HashMap |
| 4.4 | impl | Migrate UDP path to `flow_table`; drop `udp_flows` HashMap |
| 4.5 | impl | Migrate TCP path to `flow_table`; drop `tcp_nat` HashMap |
| 4.6 | impl | Cleanup: remove dead helpers, update doc comments |
| **4.6a** | **bench** | **`poll_with_n_mixed_flows` — n/3 TCP + n/3 UDP + n/3 ICMP entries, time `poll()`. Catches enum-dispatch regression at scale.** |
| **4.6b** | **bench** | **`process_udp_frame` + `process_icmp_echo_request` — per-protocol hot-path parity vs the existing `process_syn`.** |
| **4.6c** | **bench** | **`flow_table_insert_remove` — pure-compute HashMap op throughput on the unified table; Phase 4 reference for future Phase 5+ work.** |
| 4.7 | gate | Phase 4 validation gate (incl. new benches no-regression) |

---

## Task 4.1: Define `FlowKey` + `FlowEntry` enums

**Files:**
- Modify: `src/network/slirp.rs`

- [ ] **Step 1: Add the two enums** near the existing `NatKey`,
  `TcpNatEntry`, `UdpFlowKey`, `UdpFlowEntry`, `IcmpEchoKey`,
  `IcmpEchoEntry` definitions (LSP `documentSymbol` to confirm
  placement):

```rust
/// Unified flow-table key. Each variant wraps the protocol-specific
/// key already defined elsewhere in this module — no field changes,
/// just a single type that the unified `flow_table` HashMap can
/// store.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[allow(dead_code)] // consumed in 4.2
enum FlowKey {
    Tcp(NatKey),
    Udp(UdpFlowKey),
    IcmpEcho(IcmpEchoKey),
}

/// Unified flow-table value. Each variant wraps the protocol's
/// existing entry struct.
#[allow(dead_code)] // consumed in 4.2
enum FlowEntry {
    Tcp(TcpNatEntry),
    Udp(UdpFlowEntry),
    IcmpEcho(IcmpEchoEntry),
}
```

`NatKey` already derives `Hash`+`Eq`+`Clone` (the existing TCP key). `UdpFlowKey` and `IcmpEchoKey` already derive the needed traits. The `Copy` constraint is enforced by the variant types — verify they're all `Copy` (they should be — all primitive fields).

- [ ] **Step 2: Verify.**

```bash
cargo check
cargo test --test network_baseline
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
git add src/network/slirp.rs
git commit -m "refactor(slirp): define FlowKey + FlowEntry enums (no callers yet)"
```

---

## Task 4.2: Add `flow_table` field

**Files:**
- Modify: `src/network/slirp.rs`

- [ ] **Step 1: Add the field on `SlirpBackend`.** Place it
  alongside (not replacing) the existing per-protocol HashMaps:

```rust
/// Unified flow table. During Phase 4, populated in parallel with
/// the per-protocol maps (`tcp_nat`, `udp_flows`, `icmp_echo`).
/// Phase 4.3–4.5 migrate each protocol; Phase 4.6 deletes the
/// per-protocol maps.
#[allow(dead_code)] // consumed in 4.3+
flow_table: HashMap<FlowKey, FlowEntry>,
```

Initialize `flow_table: HashMap::new()` in every `SlirpBackend`
construction site (canonical: `with_security`, which `new()` and
`Default::default()` delegate to).

- [ ] **Step 2: Verify.**

```bash
cargo check
cargo test --test network_baseline
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
git add src/network/slirp.rs
git commit -m "refactor(slirp): add flow_table field on SlirpBackend (parallel to existing maps)"
```

---

## Task 4.3: Migrate ICMP path to `flow_table`

**Files:**
- Modify: `src/network/slirp.rs`

ICMP first because it's the smallest path (added in Phase 1, ~150
LOC) and the migration pattern is cleanest there. Once it's right,
4.4 and 4.5 follow the same shape.

- [ ] **Step 1: Replace `self.icmp_echo` accesses with
  `self.flow_table` accesses where the value is `FlowEntry::IcmpEcho`.**

Two access sites:
- `handle_icmp_frame` (insert/lookup by `IcmpEchoKey`)
- `relay_icmp_echo` (iterate entries, drain socket, build reply)

Pattern for insert:

```rust
// OLD:
match self.icmp_echo.entry(key) {
    std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
    std::collections::hash_map::Entry::Vacant(v) => v.insert(IcmpEchoEntry { ... }),
}

// NEW:
let flow_key = FlowKey::IcmpEcho(key);
match self.flow_table.entry(flow_key) {
    std::collections::hash_map::Entry::Occupied(o) => match o.into_mut() {
        FlowEntry::IcmpEcho(entry) => entry,
        _ => unreachable!("FlowKey::IcmpEcho must map to FlowEntry::IcmpEcho"),
    },
    std::collections::hash_map::Entry::Vacant(v) => match v.insert(FlowEntry::IcmpEcho(IcmpEchoEntry { ... })) {
        FlowEntry::IcmpEcho(entry) => entry,
        _ => unreachable!(),
    },
}
```

Pattern for iterate:

```rust
// OLD:
let keys: Vec<IcmpEchoKey> = self.icmp_echo.keys().copied().collect();
for key in keys {
    let entry = self.icmp_echo.get_mut(&key).unwrap();
    ...
}

// NEW:
let flow_keys: Vec<FlowKey> = self
    .flow_table
    .keys()
    .copied()
    .filter(|k| matches!(k, FlowKey::IcmpEcho(_)))
    .collect();
for flow_key in flow_keys {
    let FlowKey::IcmpEcho(key) = flow_key else { continue; };
    let Some(FlowEntry::IcmpEcho(entry)) = self.flow_table.get_mut(&flow_key) else { continue; };
    ...
}
```

- [ ] **Step 2: Remove the `icmp_echo` field** from `SlirpBackend`
  and its initializer.

- [ ] **Step 3: Verify.** All 14 baseline tests pass, including
  `icmp_echo_returns_reply`.

```bash
cargo check
cargo test --test network_baseline
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
git add src/network/slirp.rs
git commit -m "refactor(slirp): migrate ICMP to flow_table"
```

---

## Task 4.4: Migrate UDP path to `flow_table`

**Files:**
- Modify: `src/network/slirp.rs`

Same shape as 4.3. Access sites:
- `handle_udp_frame` (insert/lookup)
- `relay_udp_flows` (iterate + reap stale)

The reap iteration (`stale: Vec<UdpFlowKey>`) needs the same
`filter(|k| matches!(k, FlowKey::Udp(_)))` pattern as 4.3 used for
ICMP iteration.

- [ ] **Step 1: Migrate accesses to `FlowKey::Udp(...)` /
  `FlowEntry::Udp(...)`.**
- [ ] **Step 2: Remove the `udp_flows` field.**
- [ ] **Step 3: Verify** — `udp_non_dns_round_trips` passes, all
  14 tests green.

```bash
cargo check && cargo test --test network_baseline
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
git add src/network/slirp.rs
git commit -m "refactor(slirp): migrate UDP to flow_table"
```

---

## Task 4.5: Migrate TCP path to `flow_table` (the big one)

**Files:**
- Modify: `src/network/slirp.rs`

TCP is the largest path — `tcp_nat` is touched by `handle_tcp_frame`
(SYN/data/ACK/FIN/RST branches), `relay_tcp_nat_data` (peek + ACK
consume + idle reap + FIN-on-EOF), and a few helpers.

- [ ] **Step 1: Catalog every `self.tcp_nat` access** via LSP
  `findReferences`. Likely 8–12 sites.
- [ ] **Step 2: Migrate each site** to the
  `FlowKey::Tcp(...)` / `FlowEntry::Tcp(...)` pattern from 4.3. The
  ACK-consume and peek-send blocks have nested borrows; the
  `let Some(FlowEntry::Tcp(entry)) = self.flow_table.get_mut(&fk) else { continue; };`
  pattern handles them cleanly.
- [ ] **Step 3: Remove the `tcp_nat` field.**
- [ ] **Step 4: Verify — full baseline + the headline pin
  `tcp_writes_more_than_256kb_succeed`.**

```bash
cargo check
cargo test --test network_baseline
cargo bench --bench network tcp_bulk_throughput_1mb
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
git add src/network/slirp.rs
git commit -m "refactor(slirp): migrate TCP to flow_table"
```

---

## Task 4.6: Cleanup — drop `#[allow(dead_code)]`, update docs

**Files:**
- Modify: `src/network/slirp.rs`

- [ ] **Step 1: Remove all `#[allow(dead_code)]`** added in 4.1
  and 4.2 — the items are now consumed.
- [ ] **Step 2: Update file-level doc** at the top of `slirp.rs`
  to reflect the unified flow table:

```
//! Architecture:
//! - ARP: custom handler for 10.0.2.x
//! - All TCP/UDP/ICMP flows live in a unified flow_table:
//!   HashMap<FlowKey, FlowEntry>. Per-protocol relay logic dispatches
//!   on the FlowEntry variant.
//! - DNS to 10.0.2.3:53 takes a cached fast-path
//! - Other: silently dropped
```

- [ ] **Step 3: Verify.**

```bash
cargo check
cargo test --test network_baseline
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
git add src/network/slirp.rs
git commit -m "refactor(slirp): drop allow(dead_code) + update Phase 4 docs"
```

---

## Task 4.7: Phase 4 validation gate

**Files:** none.

- [ ] **Static checks**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

- [ ] **Unit + baseline + bench**

```bash
cargo test --workspace --all-features
cargo test --test network_baseline                 # 14/14
cargo bench --bench network                        # no regression
```

- [ ] **VM suites — the safety net**

```bash
export VOID_BOX_KERNEL=$PWD/target/vmlinux-slim-x86_64
export VOID_BOX_INITRAMFS=/tmp/void-box-test-rootfs.cpio.gz
cargo test --test snapshot_integration -- --ignored --test-threads=1
cargo test --test e2e_skill_pipeline -- --ignored --test-threads=1
cargo test --test e2e_mount -- --ignored --test-threads=1
cargo test --test conformance -- --ignored --test-threads=1
# (3 conformance tests pre-existing fail; same as before — verify same set fails)
```

- [ ] **Wall-clock — no regression**

```bash
./target/release/voidbox-network-bench --iterations 3 --bulk-mb 10
./target/release/voidbox-startup-bench --iters 3 --breakdown   # warm phase exits 0
```

Numbers should be statistically equivalent to Phase 3:
- `tcp_throughput_g2h_mbps` ≈ 1885 Mbps
- `tcp_bulk_throughput_g2h_mbps` ≈ 1565 Mbps
- `tcp_rr_latency_us_p50` = 2 µs
- `tcp_crr_latency_us_p50` ≈ 10 ms

Any movement >10% on these is a regression.

## Risks

- **Borrow checker friction.** Nested `match` on enum variants
  with `&mut self` borrows can be awkward — the `let Some(...) else
  { continue; }` pattern keeps each access scoped. If you hit a
  multi-variant borrow conflict, revisit by keeping the lookup and
  the mutation in separate scopes (one to find the variant, one to
  mutate).
- **Hashing.** `FlowKey` derives `Hash` from variant + inner key.
  Collision probability is fine; the default `RandomState` is
  per-process, so guests can't observe seeds.
- **No behavior change is the contract.** If any task changes a
  `tracing` event's level or a fields shape, that violates the
  observability invariant. Preserve message text and structured
  fields.

## File impact

| File | Approximate LOC |
|---|---|
| `src/network/slirp.rs` | **~+50 / −30** (net positive — enum dispatch adds boilerplate) |
| **Total** | **~+20** |

Net LOC goes UP slightly. The win is that Phase 5 can reuse
`flow_table` instead of cloning each per-protocol map's
boilerplate.
