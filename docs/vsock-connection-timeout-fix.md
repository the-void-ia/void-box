# Vsock Connection Timeout Fix

## Problem

The `claude_in_voidbox_example` (and any host-to-guest vsock communication) failed
with **Connection timed out (os error 110)**. The guest-agent was listening on
vsock port 1234, the vhost-vsock backend was configured and running, yet host
`connect(AF_VSOCK, CID, 1234)` never completed.

### Symptoms

```
[vsock] attempt 1 connect failed: Connection timed out (os error 110)
[vsock] attempt 2 connect failed: Connection timed out (os error 110)
...
[vsock] deadline reached after 12 connect/handshake attempts
Error: Guest("vsock: deadline reached (connect or handshake)")
```

The guest serial output confirmed the agent was up and listening:

```
guest-agent: vsock listener created on attempt 1
guest-agent: Listening on vsock port 1234
```

## Root Cause

Two independent bugs prevented end-to-end vsock communication.

### Bug 1: No interrupt injection for virtio-mmio vhost-vsock

**File:** `src/vmm/mod.rs`, `src/devices/virtio_vsock_mmio.rs`

When the vhost-vsock kernel backend has data for the guest (e.g. a connection
request), it writes to a **call eventfd**. The VMM must then:

1. Set `INTERRUPT_STATUS |= 1` on the virtio-mmio device registers.
2. Inject the corresponding IRQ (GSI 11) into the guest via the in-kernel
   irqchip.

Neither step was happening. The guest kernel's virtio-mmio ISR
(`vm_interrupt` in `drivers/virtio/virtio_mmio.c`) reads `INTERRUPT_STATUS`:
if it is zero the ISR returns `IRQ_NONE` and ignores the interrupt entirely.

**Why KVM_IRQFD alone is not enough:** `KVM_IRQFD` injects an IRQ when an
eventfd fires, but it does **not** update the virtio-mmio `INTERRUPT_STATUS`
register. The guest ISR sees `INTERRUPT_STATUS == 0`, returns `IRQ_NONE`, and
never processes the vring — so connection requests from the host are silently
dropped.

### Bug 2: Guest-agent closed the connection after one message

**File:** `guest-agent/src/main.rs`

The host-side protocol sends a **Ping** message first (handshake), waits for
**Pong**, and then sends the **ExecRequest** on the same TCP-like vsock
connection.

The guest-agent's `handle_connection()` only processed **one message** per
accepted connection. After replying with Pong it returned, and the main loop
immediately closed the socket. When the host then wrote the ExecRequest it
received **EPIPE (Broken pipe, os error 32)**.

## Fix

### Fix 1: vsock-irq handler thread

Added a dedicated `vsock-irq` thread in `src/vmm/mod.rs` that:

1. Uses `epoll` to watch all vhost-vsock **call eventfds**.
2. When a call eventfd fires:
   - Reads the eventfd to consume the signal.
   - Locks the `VirtioVsockMmio` mutex and sets `interrupt_status |= 1`.
   - Injects IRQ 11 via `KVM_IRQ_LINE` ioctl (assert then deassert).

This ensures the guest ISR sees `INTERRUPT_STATUS != 0` and processes the
vring, completing the host-to-guest data path.

A new public method `set_interrupt_status()` was added to `VirtioVsockMmio`
for the IRQ thread to update the register.

### Fix 2: Multi-message connection handling in guest-agent

Changed `handle_connection()` in `guest-agent/src/main.rs` to **loop** over
messages until the peer disconnects or a terminal message (Shutdown) is
received. This allows the Ping/Pong handshake and subsequent ExecRequest to
happen on the same connection.

## Verification

After the fix, both turns of the demo complete on the first attempt:

```
[vsock] attempt 1 connect OK (cid=..., port=1234)
vsock: handshake OK (Pong received)
vsock: ExecResponse received exit_code=0
Claude (plan): {"actions":["mock edit"],"summary":"mock plan for /workspace"}

[vsock] attempt 1 connect OK (cid=..., port=1234)
vsock: handshake OK (Pong received)
vsock: ExecResponse received exit_code=0
Claude (apply): Mock applied 1 plan line(s) in /workspace

✓ Demo completed.
```

## Files Changed

| File | Change |
|------|--------|
| `src/vmm/mod.rs` | Added `vsock_irq_thread()` function and thread spawn logic |
| `src/vmm/kvm.rs` | (cleanup only — removed unused `register_irqfd` method) |
| `src/devices/virtio_vsock_mmio.rs` | Added `set_interrupt_status()` and `call_eventfds()` public methods |
| `guest-agent/src/main.rs` | Changed `handle_connection()` to loop over multiple messages |
