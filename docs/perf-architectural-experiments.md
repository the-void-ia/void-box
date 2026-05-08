# SLIRP perf — architectural experiments

Stacked on top of #81.  After the heaptrack-driven user-space alloc
reductions exhausted (-90% allocs/iter, p50 unchanged at ~275 µs), the
remaining wall-clock floor is dominated by:

1. **Kernel ↔ userspace transitions** — per-packet `read()`/`write()` on
   host sockets, one syscall per packet, serial in `net_poll_thread`.
2. **Per-vCPU MMIO exits** for virtio doorbell writes (already partially
   addressed by `KVM_IOEVENTFD` for TX-notify; RX-notify and other
   queues still exit).
3. **Single-queue serialization** through `net_poll_thread`'s single
   epoll loop, even with multi-vCPU guests.

This document tracks the architectural experiments that target those
floors, ranked by risk × payoff.  Each experiment lands as its own
commit with a measurement vs the #81 baseline attached.

## Non-goal: TAP / passt-style host bypass

Dropping SLIRP and routing through TAP + an external passt instance
would close the latency gap to passt itself, but it would move the
DNS interception, port-forwarding, deny-list, and rate-limiting
feature surface out of voidbox into a separate process — and we lose
the in-process observability we currently get from instrumenting
SLIRP directly.  **Full SLIRP-path observability is a hard
requirement**, so passt-style bypass is out of scope.

## Experiments

### 1. `io_uring` for SLIRP host-socket I/O — start here

**Current path:** per-flow `recv()` + `sendto()` on host sockets,
one syscall per packet, called from `net_poll_thread` in serial.
On CRR ~5 syscalls/iter; on bulk transfers it's the dominant cost.

**Proposal:** add an `io_uring` instance to the SLIRP backend,
side-by-side with the existing `EpollDispatch`:

- After each `epoll_wait`, submit a batched `IORING_OP_RECV` SQE
  for every readable host socket — one SQE per flow with new
  data, all submitted in a single syscall.
- Submit `IORING_OP_SEND` SQEs for the outbound frames the SLIRP
  stack builds, again batched into a single submission.
- Drain CQEs in the relay loop instead of calling `recv` /
  `sendto` directly.

**Expected:** ~10–30 µs CRR p50 reduction (5 syscalls per CRR
× ~3–5 µs/syscall × batching savings).  Measurable via
`examples/crr_singleproc_bench`.

**Risk:** lowest — the change is localized to the relay layer's
read/write helpers.  Falls back to the existing path behind a
build feature so we can A/B.

### 2. `splice()` / `sendfile()` zero-copy on bulk paths

**Current path:** guest virtio TX ring → vmm copies into Rust
`Vec<u8>` → SLIRP/smoltcp → kernel send buffer of host socket.
The middle copy is avoidable for direct-pipe flows where guest
payload is destined to a host TCP socket without header rewrites.

**Proposal:** `splice()` between the host-socket fd and a pipe (then
to next stage) eliminates one userspace copy.  Only works for
fd-to-fd, so SLIRP NAT rewriting defeats it for the header path;
applies to the **payload bytes only** if we route header building
through smoltcp metadata and pipe just the bulk payload.

**Expected:** +10–20% throughput on `tcp_throughput_g2h_mbps`.
**Risk:** medium.  Plumbing pipe fds through the relay state
machine is non-trivial; needs care around partial writes and
backpressure.

### 3. MSI-X virtio + multi-queue for vCPU scaling

**Current path:** virtio-net uses a single RX queue + single TX
queue, both serviced by `net_poll_thread`.  With multi-vCPU
guests, the contention is on `net_poll_thread`'s single epoll
loop.

**Proposal:** add MSI-X support to `src/vmm/arch/x86_64/` (currently
INTx only) and expose `VIRTIO_NET_F_MQ` so the guest can spin up
per-CPU queue pairs.  Host side fans out queues to multiple poll
threads, each on its own epoll instance.

**Expected:** +50–100% throughput on multi-vCPU sandboxes.  No
impact on single-vCPU CRR microbenches.
**Risk:** highest of the three.  Touches IRQ delivery, `KVM_IRQFD`
wiring, and the IRQ path is HW-feature-gated; CI workers without
MSI-X support need a fallback.

## Tooling

All experiments measured with the perf-harness from #81:

| Tool | Signal |
|---|---|
| `examples/crr_singleproc_bench` | CRR p50/p99 (real NAT path) |
| `voidbox-network-bench` | g2h throughput, RR p50/p99 |
| `heaptrack` | allocation regression check |
| `tools/perf-harness/bench-pasta.py` | pasta reference number |
| `tools/perf-harness/bench-qemu-slirp.sh` | qemu+libslirp / qemu+passt cross-check |

## Methodology

1. Each experiment is a single commit gated behind a Cargo feature
   (`io-uring`, `splice-zerocopy`, `multi-queue`) so the baseline
   can A/B against it without a revert.
2. Commit message includes the before/after numbers from
   `crr_singleproc_bench --iterations 100` and
   `voidbox-network-bench --iterations 3`.
3. heaptrack run after each commit confirms no alloc regression
   vs the round-2 number from #81 (~41 allocs/iter on CRR).
4. If a commit doesn't move the needle, it's reverted before the
   next experiment lands so the diff stays minimal.
