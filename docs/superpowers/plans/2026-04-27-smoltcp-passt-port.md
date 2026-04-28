# SLIRP Refactor: Lift passt Patterns Into Our Stack

**Status:** Spec
**Date:** 2026-04-27
**Supersedes:** [`2026-04-12-network-backend-abstraction.md`](2026-04-12-network-backend-abstraction.md) (design changes — see "Relationship to prior plan" below)

## Required skills during execution

> **Mandatory for every task in every phase.** Each phase plan and
> every individual task assumes the implementer has these loaded.
> Failures here are blocking review comments.

| Skill | When it fires | Why mandatory here |
|---|---|---|
| **`rust-style`** | Any task that writes or modifies Rust code | Project-wide style: for-loops over iterators, `let-else` for early returns, variable shadowing, newtypes, explicit matching, minimal comments. The refactor is high-volume Rust; without this, style drift accumulates. |
| **`rustdoc`** | Any task that adds or changes doc comments on public items (`NetworkBackend` trait, new public methods, new public types) | Public surface gets documented per RFC 1574 — summary sentence, sections, type references. The trait is a long-lived public API; bad rustdoc ages badly. |
| **`rust-analyzer-ssr`** | Any task that does a structural rename or signature change across the workspace (e.g. `SlirpStack → SmoltcpBackend`, `poll → drain_to_guest`, swapping concrete types for trait objects) | LSP-aware rename understands type resolution and path equivalence. Grep-based renames break on shadowed paths and miss trait-method call sites. The plan's renames span `src/network/`, `src/devices/virtio_net.rs`, `src/vmm/mod.rs`, snapshot code, and tests — too wide for safe text-substitution. |
| **`superpowers:test-driven-development`** | Every test/bench task in Phase 0 and every behavior change in Phases 1–5 | The "broken on purpose" pins are TDD by construction: assertion locks current behavior, refactor flips assertion. Skipping the failing-test step destroys that property. |
| **`superpowers:verification-before-completion`** | Before claiming any task complete | The validation gate (`cargo fmt`, `cargo clippy -D warnings`, `cargo test`, `cargo bench`, VM suites where applicable) must produce real green output, not narration. |
| **`verify`** *(repo skill)* | At the end of every phase, before opening the PR | Runs the full project quality gate: format, clippy, tests, security audit, startup bench regression, real-workload smoke. Catches cross-cutting regressions that the network-only gate misses. |
| **`profile`** *(repo skill)* | When a divan or wall-clock bench regresses by >5% | Don't guess at perf regressions — capture eBPF profiles and read them. |

In addition, the project-wide rules from `CLAUDE.md` and `AGENTS.md`
remain in force:

- **Prefer LSP operations** (`goToDefinition`, `findReferences`,
  `hover`, `documentSymbol`, `workspaceSymbol`) over Grep/Glob for
  Rust code navigation. Grep/Glob only for comments, config files,
  non-Rust files.
- **Platform parity:** every change validated on Linux (KVM) and, where
  applicable, macOS (VZ). Phase 0's wall-clock harness is Linux-only
  by design (smoltcp is `cfg(target_os = "linux")`); Phases 1–5
  surface-level changes must not break the macOS build.
- **Imports and constants at module scope.** Never inline `use` /
  `const` inside function bodies.

## Summary

Refactor `src/network/slirp.rs` to fix correctness and coverage gaps (no
ICMP, UDP-only-on-port-53, fragile hand-rolled TCP relay) by lifting
proven design patterns from [passt](https://passt.top/passt) into our
own all-Rust smoltcp-based stack — instead of adopting passt as an
external backend.

The work is gated behind a benchmark and correctness baseline: every
phase ships with assertions that pin existing behavior (including the
"broken on purpose" parts) so regressions and improvements are both
visible in the diff.

## Motivation

The prior plan (2026-04-12) proposed adding `passt` as an opt-in
Linux-only backend behind a new `NetworkBackend` trait. After deeper
analysis of both codebases, that approach has worse cost/benefit than
keeping the work in-tree:

**Why not passt as a backend:**

- **Observability regression.** passt is an opaque C process behind a
  4-byte-prefixed unix socket. Every bug becomes "did passt do the
  right thing?" instead of "what did our stack do?" with full
  structured logs, tracing spans, and a debugger that works.
- **Cross-platform divergence.** passt is Linux-only. Adding it makes
  guest behavior diverge across host platforms (`ping` works on Linux,
  fails silently on macOS).
- **Operational friction.** passt is not installed by default on
  Fedora, Ubuntu, Arch, or Alpine. Every user wanting the upgrade
  needs a separate install step.
- **Process-lifecycle complexity.** Crash policy, stderr routing,
  `PR_SET_PDEATHSIG`, and snapshot/restore semantics all become real
  problems we don't have today.
- **New attack surface in the data path.** C code in our sandbox
  boundary, even battle-tested C code, is qualitatively new exposure.

**Why lift the design patterns instead:**

- The capability gaps (ICMP, full UDP, IPv6) are tractable in
  Rust+smoltcp. ICMP via `SOCK_DGRAM IPPROTO_ICMP` is ~150 LOC.
  Generalizing UDP off the port-53 fast-path is ~200 LOC.
- The fragile parts of our TCP relay (256 KB `to_host` buffer cliff,
  hand-rolled FIN state machine, `EAGAIN` deferral) can be **deleted**,
  not patched, by adopting passt's "no per-connection packet buffer,
  mirror sequence numbers via `MSG_PEEK`" pattern.
- The all-Rust path keeps structured tracing, sanitizers, and
  profiler-readable call stacks intact.
- The `NetworkBackend` trait abstraction still earns its keep: it
  decouples virtio-net from the stack so a future TAP/vhost-net
  backend (the path that actually moves throughput numbers, per the
  prior plan's appendix) can land cleanly.

## Non-goals

- **Adopting passt as a binary backend.** Explicitly rejected per the
  motivation above.
- **Throughput improvements.** Per the 2026-04-12 plan's appendix, the
  bottleneck is the MMIO exit path, not the network stack. This work
  improves correctness and coverage; throughput wins require
  ioeventfd/irqfd or vhost-net (separately scoped, separately reviewed).
- **IPv6 in the initial phases.** Real lift (~800–1000 LOC). Deferred
  to a later phase with its own plan.
- **macOS feature parity in Phase 0.** The wall-clock e2e harness will
  initially be Linux-only since `smoltcp` is already Linux-gated in
  `Cargo.toml`. macOS (VZ NAT) continues unchanged.

## Relationship to prior plan

The 2026-04-12 plan proposed:

1. Extract `NetworkBackend` trait. **Kept.**
2. Add `PasstBackend` (Linux-only, opt-in). **Replaced** with in-tree
   improvements to the smoltcp-based backend.
3. Cleanup rename `SlirpStack → SlirpBackend`. **Kept**, moved into
   Phase 0 alongside the trait extraction. Role-based name (matches
   future `TapBackend`/`VhostNetBackend`); does not leak the smoltcp
   library dependency.

The trait surface from the prior plan is tightened (`poll` becomes an
out-param to drop the per-call `Vec<Vec<u8>>` allocation; explicit
error type; health/dead signal).

## Design

### Core insight

passt's superpower is a single architectural decision: **don't buffer
per connection — mirror sequence numbers**.

Our current TCP relay (`src/network/slirp.rs:82–1048`, ~625 LOC) does
the opposite: `read()`s from the host socket into a `to_guest: Vec<u8>`,
drains on the next poll, and **closes the connection if `to_host`
exceeds 256 KB** (`slirp.rs:903–910`). passt never has that problem
because it never copies — it `recv(MSG_PEEK)`s, and the host kernel's
socket buffer *is* the buffer. Sequence math
(`seq_to_tap = seq_ack_from_tap + bytes_peeked`) reproduces what we
hand-roll.

That single trick eliminates roughly half of the fragility in our
current code: no `EAGAIN` buffer-overflow path, no manual
`to_host_pending_ack` deferral, no 256 KB cliff.

### Five patterns ported, ranked by ROI

| # | Pattern | passt source | Our target | Approx. LoC | Phase |
|---|---|---|---|---|---|
| 1 | `MSG_PEEK` + sequence mirroring (TCP) | `tcp.c` `tcp_data_from_sock`, `tcp_data_from_tap` | `slirp.rs::relay_tcp_nat_data`, `handle_tcp_frame` | ~400 replaced | 3 |
| 2 | Per-flow connected UDP socket | `udp.c` `udp_flow_from_tap`, `udp_listen_sock_handler` | `slirp.rs::handle_dns_frame` (generalize) | ~200 new | 2 |
| 3 | Unprivileged ICMP echo via `SOCK_DGRAM IPPROTO_ICMP` | `icmp.c` `icmp_ping_handler`, `icmp_sock_handler` | new `slirp.rs::handle_icmp_frame` | ~150 new | 1 |
| 4 | Unified flow table with side indexing | `flow.c`, `flow.h` `union flow` + SipHash table | new `slirp.rs::FlowTable` | ~200 refactor | 4 |
| 5 | Stateless address translation | `fwd.c::nat_inbound` | refactor existing 10.0.2.2→127.0.0.1 rewrite | ~150 refactor | 5 |

### What we keep as-is

- **DNS caching with question-section keying** (`slirp.rs:433–456`) is
  better than passt — passt has no DNS cache. Keep it.
- **Net-poll thread on a 5ms timer** (`vmm/mod.rs:1594–1630`) is
  simpler than passt's epoll/timerfd dance and fits our virtio-mmio
  model. The 5ms floor matters less once we stop dropping connections
  at 256 KB.
- **smoltcp for wire types + ARP via `Interface`** is the right
  division of labor. passt has to hand-roll its packet abstraction
  (`packet.h`); we get checksum and parsing for free.
- **Threading model** (`process_guest_frame` on vCPU, `poll` on
  net-poll, `Arc<Mutex<>>`) is sound. Don't touch it.

### What we throw away from passt

| passt feature | Why skip |
|---|---|
| `TCP_REPAIR` migration | Out of scope; VM snapshots already break TCP |
| `splice()` / vhost-user / pasta zero-copy | Throughput-focused, gated by MMIO exit cost |
| Full IPv6 (DHCPv6, NDP, RA) | Deferred to a later phase |
| AVX2 checksum | smoltcp's checksum is fine; premature optimization |
| Daemon harness, conf parsing, qrap | We're an embedded library, not a daemon |
| C weak-symbol dispatch | Use Rust enum dispatch / trait objects |

### `NetworkBackend` trait

```rust
// src/network/mod.rs

use std::io;

/// A network backend processes raw Ethernet frames between guest and host.
///
/// Implementations must be `Send` so they can be held behind
/// `Arc<Mutex<_>>` and accessed from both the vCPU thread (TX path) and
/// the net-poll thread (RX path).
pub trait NetworkBackend: Send {
    /// Process a raw Ethernet frame sent by the guest (TX path).
    ///
    /// Called from the vCPU thread on MMIO write to the TX virtqueue.
    /// Implementations should not block.
    fn process_guest_frame(&mut self, frame: &[u8]) -> io::Result<()>;

    /// Drain Ethernet frames destined for the guest into `out` (RX path).
    ///
    /// Called every ~5ms from the net-poll thread. Frames are
    /// complete Ethernet payloads — no virtio-net header (the caller
    /// prepends that). The buffer is reused across calls to avoid
    /// per-poll allocation.
    fn drain_to_guest(&mut self, out: &mut Vec<Vec<u8>>);

    /// Backend health. `false` means the backend has entered an
    /// unrecoverable state and should be reconstructed.
    fn is_healthy(&self) -> bool {
        true
    }
}
```

Differences from the prior plan:

- `poll() -> Vec<Vec<u8>>` → `drain_to_guest(&mut self, out: &mut Vec<Vec<u8>>)`.
  Drops the per-poll allocation that would otherwise fire every 5ms.
- Explicit `io::Result<()>` instead of project-wide `Result`.
- `is_healthy()` default-true hook for future backends that have a
  process or socket lifecycle (TAP, vhost-net). Unused by
  `SmoltcpBackend`.

## Phase breakdown

Each phase is **independent** and **landable on its own**. Each phase
will get its own bite-sized plan document under `docs/superpowers/plans/`
when execution starts. Phases 1–5 plan documents are deliberately not
written yet — what we learn from earlier phases will sharpen the
detailed task lists for later ones.

| Phase | Scope | Risk | Plan doc |
|---|---|---|---|
| **0** | Baseline tests + benches + `NetworkBackend` trait extraction + `SlirpStack → SlirpBackend` rename. **Zero user-visible behavior change.** | Low | [`2026-04-27-smoltcp-passt-port-phase0.md`](2026-04-27-smoltcp-passt-port-phase0.md) |
| **1** | ICMP echo via unprivileged `SOCK_DGRAM IPPROTO_ICMP`, with sysctl-fallback to drop. | Low | TBD when 0 lands |
| **2** | Generalize UDP: per-flow connected sockets, drop port-53 limit, keep DNS fast-path/cache. | Low–medium | TBD when 1 lands |
| **3** | TCP relay rewrite using `MSG_PEEK` + sequence mirroring. Drop `to_guest: Vec<u8>` and 256 KB cap. | **High** — gnarliest of the lot. Snapshot integration tests are the gate. | TBD when 2 lands |
| **4** | Unified flow table refactor (no behavior change). Side-indexed entries, SipHash lookup. | Medium | TBD when 3 lands |
| **5** | Stateless NAT translation refactor + port-forwarding configurability. | Low | TBD when 4 lands |
| **6** *(optional)* | IPv6 dual-stack (DHCPv6, NDP, RA, NAT). | High | TBD; may be split further |

## Baseline strategy

Every phase ships with assertions that pin observable behavior. Three
of these assertions deliberately encode **broken** behavior — they are
green lights that flip when the corresponding phase lands.

### Two test layers

**Layer 1 — unit-level (fast, deterministic, no VM):** drive
`SmoltcpBackend` directly. Feed synthetic Ethernet frames via
`process_guest_frame`, drive `drain_to_guest`, inspect emissions.
Sub-millisecond per test, runs on every `cargo test`. Lives in
`tests/network_baseline.rs`.

**Layer 2 — wall-clock e2e (slow, real numbers, comparable to passt):**
boot a VM, run iperf3/netperf-style measurements inside, output JSON.
Mirrors the existing `voidbox-startup-bench` pattern. New binary
`voidbox-network-bench`. Linux-only initially.

### Two benchmark layers

**Layer 1 — divan microbenches:** `benches/network.rs` mirrors
`benches/startup.rs`. `divan::main()`, `#[divan::bench]`, parametric
`args` for NAT-walk scaling. Run with `cargo bench --bench network`.

**Layer 2 — wall-clock harness above** outputs metrics named to match
passt's published table (`tcp_throughput_*`, `tcp_rr_latency`,
`tcp_crr_latency`, `udp_throughput_*`).

### "Broken on purpose" pins

These three tests assert broken behavior today. They are intended to
flip when the corresponding phase lands:

| Test | Today's assertion | Flips in phase |
|---|---|---|
| `tcp_to_host_buffer_drops_at_256kb` | Connection closes when guest writes >256 KB before host reads | 3 |
| `udp_non_dns_silently_dropped` | UDP datagram to port 80 produces no host-side connection | 2 |
| `icmp_echo_silently_dropped` | ICMP echo request produces no echo reply | 1 |

The PR that fixes each behavior is the PR that flips the assertion,
which makes the diff legible to reviewers.

### passt head-to-head methodology

Direct numerical comparison is structurally limited (passt runs in
qemu with its socket back-end; we run our own VMM with virtio-mmio).
The honest plan:

1. **Same hardware, same workload, same metric names.** Run our
   `voidbox-network-bench` and a passt+qemu reference on the same
   host. Two columns in the report.
2. **Track the gap, don't claim parity.** Throughput will lag because
   of MMIO exit overhead; that's known and out-of-scope.
3. **Connect rate (CRR latency) is the most apples-to-apples
   metric** — dominated by NAT-table operations, not MMIO. If passt
   does CRR in 135 µs and we do 600 µs, that's a meaningful "we have
   4× more overhead per connect" signal that this refactor should
   narrow.

Report shape (illustrative, real numbers come from the harness):

```
                          before   after-phase-3   passt
tcp throughput g2h 1500B  4.1 G    5.2 G           5.2 G
tcp RR latency            72 µs    58 µs           58 µs
tcp CRR latency           640 µs   180 µs          135 µs
udp DNS qps               12k      12k             n/a
icmp echo                 dropped  ~110 µs         ~50 µs
allocations per packet    3        0               0
```

## File impact

### Phase 0 (baseline + trait + rename)

| File | Change |
|---|---|
| `src/network/mod.rs` | Add `NetworkBackend` trait |
| `src/network/slirp.rs` | `impl NetworkBackend for SlirpStack`, rename type to `SlirpBackend`, tighten `poll` to `drain_to_guest` |
| `src/devices/virtio_net.rs` | Hold `Arc<Mutex<dyn NetworkBackend>>` instead of concrete `SlirpStack` |
| `src/vmm/mod.rs` | Update construction at cold-boot + snapshot-restore sites |
| `tests/network_baseline.rs` | **New file**: ~14 unit-level pins |
| `benches/network.rs` | **New file**: divan microbenches |
| `src/bin/voidbox-network-bench/main.rs` | **New file**: wall-clock harness |
| `Cargo.toml` | Register new bench, new binary, new test |
| `.github/workflows/startup-bench.yml` | Add `network` bench step (or add a new workflow file) |

### Phases 1–5

Documented in their own plan files when scoped.

## Risks

- **TCP rewrite is the high-risk part.** Phase 3 replaces the most
  battle-tested path in our networking code. The snapshot integration
  suite is the safety gate; if any of `snapshot_integration`,
  `e2e_telemetry`, `e2e_skill_pipeline`, `e2e_mount`, or `e2e_sidecar`
  regress, Phase 3 stays in draft.
- **passt protocol/idiom drift.** We're lifting design patterns, not
  code. The risk is that we hit edge cases passt has already solved
  that we'll re-discover as bugs (e.g. PAWS, fast retransmit
  thresholds). Mitigation: explicit test-case lift from passt's test
  suite (`/home/diego/github/passt/test/`) where applicable.
- **Cross-platform parity for ICMP.** Linux requires the
  `net.ipv4.ping_group_range` sysctl to permit the calling GID.
  macOS allows unprivileged `SOCK_DGRAM IPPROTO_ICMP` unconditionally.
  When sysctl forbids it on Linux, fall back to current behavior
  (drop), with a warn-once log.
- **Engineering time vs. throughput wins.** This work does not move
  throughput numbers. The ioeventfd/vhost-net path that *does* will
  reuse the trait abstraction we land in Phase 0, but won't reuse the
  TCP relay rewrite from Phase 3. If priorities shift toward
  throughput, Phases 0, 1, and 2 still pay off; Phase 3 may be
  deferred.

## Validation gate (per phase)

Every phase ends with:

```bash
# Static
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings

# Tests
cargo test --workspace --all-features
cargo test --doc --workspace --all-features

# Network-specific
cargo test --test network_baseline
cargo bench --bench network         # no >5% regression vs main

# VM suites that exercise networking (Linux/KVM)
export VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r)
export VOID_BOX_INITRAMFS=/tmp/void-box-test-rootfs.cpio.gz
cargo test --test conformance -- --ignored --test-threads=1
cargo test --test snapshot_integration -- --ignored --test-threads=1
cargo test --test e2e_skill_pipeline -- --ignored --test-threads=1
```

A phase is not "done" until all gates pass and the wall-clock
`voidbox-network-bench` shows no regression on previously-working
metrics. New metrics (ICMP latency, non-DNS UDP throughput) are
expected to flip from "n/a / dropped" to a number when their
corresponding phase lands.

## References

- **Prior plan** (this supersedes the design, keeps the trait):
  `docs/superpowers/plans/2026-04-12-network-backend-abstraction.md`
- **passt source** (cloned locally):
  `/home/diego/github/passt`
  - `tcp.c` — TCP translation, sequence mirroring (Phase 3 reference)
  - `udp.c` — per-flow UDP NAT (Phase 2 reference)
  - `icmp.c` — `IPPROTO_ICMP SOCK_DGRAM` echo (Phase 1 reference)
  - `flow.c` — unified flow table (Phase 4 reference)
  - `fwd.c::nat_inbound` — stateless address translation (Phase 5 ref)
- **Our networking code:**
  - `src/network/slirp.rs` (1275 LOC) — the file most of this work
    lands in
  - `src/network/mod.rs` (202 LOC) — where `NetworkBackend` trait goes
  - `src/devices/virtio_net.rs` (831 LOC) — virtio-net wiring
  - `src/vmm/mod.rs:1594–1630` — net-poll thread
- **Existing bench/test infrastructure to mirror:**
  - `benches/startup.rs` — divan pattern
  - `src/bin/voidbox-startup-bench/main.rs` — wall-clock harness
    pattern
  - `.github/workflows/startup-bench.yml` — CI regression gate
- **passt project page:** https://passt.top/passt — performance
  table format, metric names
