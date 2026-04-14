# Network Backend Abstraction

**Status:** Draft
**Date:** 2026-04-12

## Summary

Extract a `NetworkBackend` trait from the current smoltcp-based SLIRP stack so
that alternative backends (passt, future others) can be swapped in without
changing the virtio-net device, vCPU loop, or net-poll thread.

## Motivation

The current networking stack is a hand-rolled userspace TCP/UDP NAT built on
smoltcp wire types (~1000 LOC in `src/network/slirp.rs`). It works everywhere
and needs no privileges, but it has real limitations:

- **No ICMP** — `ping` silently fails inside the guest.
- **UDP limited to DNS** — only port 53 is forwarded; all other UDP is dropped.
- **No IPv6** — the stack is IPv4-only.
- **Manual TCP relay** — connection tracking, window management, `EAGAIN`
  buffering, FIN/RST state machine are all hand-written. This is the most
  fragile code in the networking layer.
- **CPU cost** — every `poll()` call on the net-poll thread walks the full NAT
  table, attempts host socket reads, resolves pending DNS, and runs smoltcp's
  ARP state machine.

`passt` (the backend behind podman rootless networking and libkrun) delegates
all of this to the host kernel via a unix socket carrying raw Ethernet frames.
It provides full TCP, UDP, ICMP, and IPv6 — essentially for free.

## Non-goals

- **Improving virtio-net throughput.** The latency/throughput bottleneck is the
  MMIO exit path (see appendix), not the network backend. This work does not
  change that.
- **macOS networking changes.** macOS/VZ uses Apple's built-in NAT; the smoltcp
  backend stays as the portable fallback. passt is Linux-only.
- **pasta support.** pasta (passt's namespace-mode sibling) requires
  `CAP_NET_ADMIN` and adds no benefit over passt for VoidBox's threat model.

## Design

### Trait

```rust
// src/network/mod.rs

/// A network backend processes raw Ethernet frames between guest and host.
///
/// Implementations must be `Send` so they can be held behind `Arc<Mutex<_>>`
/// and accessed from both the vCPU thread (TX path) and the net-poll thread
/// (RX path).
pub trait NetworkBackend: Send {
    /// Process a raw Ethernet frame sent by the guest (TX path).
    ///
    /// Called from the vCPU thread on MMIO write to the TX virtqueue.
    /// Implementations should not block.
    fn process_guest_frame(&mut self, frame: &[u8]) -> Result<()>;

    /// Poll for Ethernet frames destined to the guest (RX path).
    ///
    /// Called every ~5ms from the net-poll thread. Returns zero or more
    /// complete Ethernet frames (no virtio-net header — the caller prepends
    /// that).
    fn poll(&mut self) -> Vec<Vec<u8>>;
}
```

### Wiring

`VirtioNetDevice` changes from:

```rust
pub struct VirtioNetDevice {
    slirp: Arc<Mutex<SlirpStack>>,
    ...
}
```

to:

```rust
pub struct VirtioNetDevice {
    backend: Arc<Mutex<dyn NetworkBackend>>,
    ...
}
```

Construction moves from `VirtioNetDevice::new(slirp)` to
`VirtioNetDevice::new(backend: Arc<Mutex<dyn NetworkBackend>>)`. The two call
sites in `src/vmm/mod.rs` (cold boot + snapshot restore) pick the backend based
on config.

### Backends

#### `SmoltcpBackend` (rename of current `SlirpStack`)

- Default backend on all platforms.
- Zero external dependencies, no root required.
- Existing behavior preserved exactly.
- `impl NetworkBackend for SmoltcpBackend`.

#### `PasstBackend` (new, Linux-only)

- Launches `passt --fd <N>` as a child process at VM start.
- Communicates over a unix socket carrying raw Ethernet frames (4-byte
  length-prefixed).
- `process_guest_frame()`: write length + frame to the socket.
- `poll()`: non-blocking read from the socket, return any complete frames.
- Port forwarding via passt's `-t` (TCP) and `-u` (UDP) flags, mapped from
  `NetworkConfig::port_forwards`.
- On drop: sends SIGTERM to the passt child process.
- `#[cfg(target_os = "linux")]` gated.

### Configuration

Spec-level (`src/spec.rs`):

```yaml
sandbox:
  network:
    backend: smoltcp  # or "passt" (Linux-only, default: smoltcp)
```

Builder API:

```rust
SandboxBuilder::new()
    .network_backend(NetworkBackendKind::Passt)
```

If `passt` is requested but the binary is not found in `$PATH`, emit a clear
error at VM start rather than falling back silently.

### Snapshot/restore

Network backend state is **not** snapshotted. TCP connections break across
snapshot boundaries regardless of backend. On restore:

- smoltcp: fresh `SmoltcpBackend::new()` (current behavior).
- passt: spawn a new passt process.

The `NetSnapshotState` in `src/vmm/snapshot.rs` captures virtio device state
(queues, features, MAC), not backend state. This stays unchanged.

## Implementation plan

### Phase 1: Extract trait (no behavior change)

1. Define `NetworkBackend` trait in `src/network/mod.rs`.
2. `impl NetworkBackend for SlirpStack` (trivial — the methods already match).
3. Change `VirtioNetDevice` to hold `Arc<Mutex<dyn NetworkBackend>>`.
4. Update the two construction sites in `src/vmm/mod.rs`.
5. All existing tests pass unchanged.

### Phase 2: Add passt backend

1. Add `src/network/passt.rs` with `PasstBackend`.
2. Add `NetworkBackendKind` enum to `src/spec.rs`.
3. Wire config through `src/vmm/mod.rs` backend selection.
4. Add integration test: boot VM with passt, verify ICMP + TCP + UDP.

### Phase 3: Cleanup (optional)

1. Rename `SlirpStack` → `SmoltcpBackend` for clarity.
2. Add `GUEST_MAC` to the trait or a config struct instead of importing from
   `slirp` module.

## File impact

| File | Change |
|------|--------|
| `src/network/mod.rs` | Add `NetworkBackend` trait, `NetworkBackendKind` enum |
| `src/network/slirp.rs` | Add `impl NetworkBackend for SlirpStack` |
| `src/network/passt.rs` | New file: `PasstBackend` |
| `src/devices/virtio_net.rs` | `SlirpStack` → `dyn NetworkBackend` |
| `src/vmm/mod.rs` | Backend selection at construction (2 sites) |
| `src/spec.rs` | `network.backend` field |
| `Cargo.toml` | No new deps (passt is an external process) |

## Testing

- Phase 1: existing `cargo test` + VM conformance suites. Zero behavior change.
- Phase 2: new `tests/e2e_passt.rs` (Linux-only, `#[cfg(target_os = "linux")]`):
  - Boot with passt backend, exec `ping -c1 8.8.8.8` (ICMP).
  - TCP: `curl http://example.com` or similar.
  - UDP: DNS resolution (already works, but confirms the path).
  - Port forwarding: bind inside guest, connect from host.
  - Missing `passt` binary → clear error message.

## Risks

- **passt availability.** Not installed by default on most distros. Must be an
  opt-in backend, never the default.
- **passt protocol stability.** The Ethernet-over-unix-socket framing has been
  stable since passt 2022_09_29. Podman and libkrun depend on it.
- **Debugging.** Two backends means two code paths to debug. Mitigated by keeping
  smoltcp as default and making passt opt-in.

---

## Appendix: Why the MMIO exit path is the throughput bottleneck

### The hot path for every packet

Every virtio-net packet — TX or RX — requires multiple MMIO register accesses
by the guest kernel's virtio-mmio driver. Each MMIO access to an unmapped
address triggers a **VM exit**: the vCPU stops executing guest code, KVM
transitions to host userspace, the VMM handles the access, and the vCPU
re-enters guest mode.

A single packet TX typically causes this sequence of exits:

```
Guest driver                           VMM (host)
───────────                            ──────────
1. Write descriptor to desc ring       (guest memory, no exit)
2. Write avail ring index              (guest memory, no exit)
3. MMIO write → QUEUE_NOTIFY (0x050)   ← VM EXIT: process_tx_queue()
4. MMIO read  → INTERRUPT_STATUS       ← VM EXIT: return status
5. MMIO write → INTERRUPT_ACK          ← VM EXIT: clear interrupt
```

RX is similar: the net-poll thread injects an IRQ, the guest handles it,
reads INTERRUPT_STATUS, processes the used ring, ACKs the interrupt — each
MMIO register access is another exit.

### Cost of a VM exit

A KVM VM exit on x86_64 takes roughly **1-3 microseconds** depending on the
host CPU (VMRESUME → VMEXIT → handle → VMRESUME). This includes:

- Saving guest register state
- Switching to host kernel mode
- Transitioning to host userspace (the VMM)
- The VMM's dispatch logic (lock acquisition, address matching, handler call)
- Re-entering KVM and restoring guest state

For a single TCP packet, you're looking at **3-5 exits minimum** (notify +
status + ack, sometimes more for multi-descriptor chains). At 2us per exit,
that's 6-10us of exit overhead per packet — before any actual network
processing happens.

### What this means in practice

At high packet rates (e.g., 10k packets/sec during `npm install`), the vCPU
spends a significant fraction of its time bouncing between guest and host on
MMIO exits. The network backend's processing time (smoltcp NAT lookup,
passt socket write) is **tens of nanoseconds to low microseconds** — dwarfed
by the exit overhead.

This is why:

1. **Switching from smoltcp to passt won't measurably improve throughput.**
   The per-packet backend cost is noise compared to the MMIO exit cost.

2. **The real throughput wins require reducing exits**, not speeding up what
   happens between exits. Possible approaches (out of scope for this work):
   - **virtio-net with ioeventfd/irqfd**: KVM handles notify writes in-kernel
     without exiting to userspace. Requires `KVM_IOEVENTFD` + `KVM_IRQFD`
     setup. This is what Firecracker and cloud-hypervisor do.
   - **vhost-net**: Kernel-side virtqueue processing. Eliminates the VMM from
     the data path entirely. Requires TAP devices (root or `CAP_NET_ADMIN`).
   - **Batch processing**: Process multiple descriptors per exit instead of
     one-at-a-time. Reduces the exit-to-packet ratio.

3. **passt's value is correctness and coverage** (ICMP, full UDP, IPv6,
   kernel-grade TCP), not speed.

### Where the net-poll thread fits

The net-poll thread (`src/vmm/mod.rs:1495`) runs independently of the vCPU,
polling every 5ms. It:

1. Locks the `VirtioNetDevice` mutex
2. Calls `try_inject_rx()` — walks the RX virtqueue in guest memory
3. If frames were injected, pulses IRQ 10 via `KVM_IRQ_LINE` ioctl

This thread exists because `KVM_RUN` doesn't exit during pure guest
computation — without it, RX frames would sit in the buffer until the next
TX-triggered exit. The 5ms poll interval means RX latency floors at ~5ms
regardless of backend speed. This is another reason the backend isn't the
bottleneck — the poll interval dominates RX latency.
