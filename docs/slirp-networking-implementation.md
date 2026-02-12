# SLIRP Networking: Implementation and Bug Fixes

## Overview

void-box provides user-mode NAT networking for guest VMs via a custom SLIRP
stack, allowing guests to reach external services (e.g. the Anthropic API)
without requiring root privileges, TAP devices, or iptables rules.

### Network Layout

| Role    | IP Address  | Notes                          |
|---------|-------------|--------------------------------|
| Guest   | 10.0.2.15   | Static, configured by guest-agent |
| Gateway | 10.0.2.2    | Virtual, handled by SLIRP      |
| DNS     | 10.0.2.3    | Forwarded to host resolvers    |

Guest MAC: `52:54:00:12:34:56`, Gateway MAC: `52:54:00:12:34:01`.

### Architecture

```
Guest (virtio-net driver)
  │
  ├── TX: guest → host
  │     virtio TX queue → VirtioNetDevice::process_tx_queue()
  │       → strip virtio-net header → SlirpStack::process_guest_frame()
  │         → dispatch by EtherType: ARP / IPv4
  │
  └── RX: host → guest
        SlirpStack::poll() → collect inject_to_guest frames
          → VirtioNetDevice::get_rx_frames() → prepend virtio-net header
            → try_inject_rx() → write to RX queue → IRQ 10
```

Packet handling:

- **ARP**: Custom handler responds to all ARP requests for 10.0.2.x IPs
  (except the guest's own IP) with the gateway MAC address.
- **TCP**: NAT proxy — guest TCP SYN triggers a host `TcpStream::connect()`,
  then data is relayed bidirectionally with sequence number tracking.
- **DNS (UDP:53)**: Queries to 10.0.2.3 are forwarded to host resolvers
  (8.8.8.8, 1.1.1.1) and responses are injected back as UDP frames.
- **Other**: Silently dropped.

## Key Files

| File | Purpose |
|------|---------|
| `src/network/slirp.rs` | SLIRP stack: ARP, TCP NAT, DNS forwarding |
| `src/devices/virtio_net.rs` | Virtio-net MMIO device emulation |
| `src/vmm/cpu.rs` | vCPU run loop with virtio-net RX polling and IRQ injection |
| `src/vmm/config.rs` | Kernel command line (virtio-mmio device params, ipv6.disable) |
| `guest-agent/src/main.rs` | Guest-side network setup (ip addr/route, resolv.conf) |
| `scripts/build_guest_image.sh` | Initramfs with virtio_net.ko and dependencies |
| `scripts/build_claude_rootfs.sh` | Extended initramfs with Node.js + claude-code |

## Bugs Fixed

### Bug 1: Missing `VIRTIO_F_VERSION_1` feature flag

**File:** `src/devices/virtio_net.rs`

The virtio-net device was not advertising `VIRTIO_F_VERSION_1` (bit 32), which
is **required** for virtio-mmio v2 devices. The Linux `virtio_mmio` driver
rejected the device during feature negotiation, so `eth0` never appeared.

**Fix:** Added `features::VIRTIO_F_VERSION_1` to the device's advertised
features.

### Bug 2: Network setup before driver loading

**File:** `guest-agent/src/main.rs`

`setup_network()` (which runs `ip addr add`, `ip route add`) was called inside
`init_system()`, which runs before `load_kernel_modules()`. Since `virtio_net.ko`
creates the `eth0` interface, the network commands had no interface to configure.

**Fix:** Moved `setup_network()` to `main()`, after `load_kernel_modules()`.

### Bug 3: IPv6 traffic bypassing IPv4-only SLIRP

**File:** `src/vmm/config.rs`

The guest kernel defaulted to IPv6, causing `nslookup` and other tools to send
AAAA queries via IPv6 sockets. The SLIRP stack only handles IPv4, so all
IPv6 traffic was silently dropped.

**Fix:** Added `ipv6.disable=1` to the kernel command line when networking is
enabled.

### Bug 4: ARP reply field offset error (2-byte shift)

**File:** `src/network/slirp.rs`

The custom ARP reply builder placed `hw_addr_len` (6) and `proto_addr_len` (4)
at byte offsets 20-21 of the Ethernet frame instead of the correct 18-19. This
shifted all subsequent ARP fields by 2 bytes, producing a malformed reply that
the guest kernel could not parse.

**Symptoms:** Guest sent repeated ARP requests for 10.0.2.3 (one per second)
but never learned the MAC, so no IPv4 traffic followed.

**Fix:** Corrected the ARP reply layout:

```
Offset  Field              Size
 0..6   Dst MAC            6
 6..12  Src MAC            6
12..14  EtherType (0x0806) 2
14..16  HW type (1)        2
16..18  Proto type (0x0800)2
18      HW addr len (6)    1  ← was at 20
19      Proto addr len (4) 1  ← was at 21
20..22  Opcode (2=reply)   2  ← was at 22
22..28  Sender HW addr     6  ← was at 24
28..32  Sender Proto addr  4  ← was at 30
32..38  Target HW addr     6  ← was at 34
38..42  Target Proto addr  4  ← was at 40
```

### Bug 5: Virtio queue ring index wrapping

**File:** `src/devices/virtio_net.rs`

The virtio TX and RX queues use circular rings with `queue_size` entries (256).
The `avail_idx` and `used_idx` counters are `u16` values that wrap at 65536,
but the ring arrays only have 256 entries. The ring offset calculations were
not applying `% queue_size`:

```rust
// BEFORE (bug): reads past end of ring when avail_idx >= 256
let ring_offset = 4 + (self.tx_avail_idx as usize) * 2;

// AFTER (fix): wraps correctly
let ring_offset = 4 + ((self.tx_avail_idx as usize) % queue_size) * 2;
```

**Symptoms:** After ~256 packets, the kernel logged
`virtio_net virtio0: output.0:id 0 is not a head!` and networking degraded.

**Fix:** Applied `% queue_size` to all four ring offset calculations (TX avail,
TX used, RX avail, RX used).

### Bug 6: Missing virtio-net IRQ injection in vCPU loop

**File:** `src/vmm/cpu.rs`

The virtio-net device sets `interrupt_status |= 1` when there are RX frames to
deliver, but nothing was injecting the corresponding IRQ 10 into the guest. The
guest driver never saw the interrupt and never processed the RX queue.

**Fix:** Added a pre-`vcpu.run()` poll in the vCPU loop that calls
`try_inject_rx()` and, if `has_pending_interrupt()` is true, injects IRQ 10 via
`KVM_IRQ_LINE` ioctl (assert then deassert).

### Bug 7: Guest-agent privilege dropping for claude-code

**File:** `guest-agent/src/main.rs`

The `claude-code` CLI refuses `--dangerously-skip-permissions` when running as
root. Since the guest-agent is PID 1 (root), child processes inherited root
privileges.

**Fix:** Added a `pre_exec` hook in `execute_command()` that drops to
uid/gid 1000 (`sandbox` user) before exec-ing the child process. Created
`/workspace` and `/home/sandbox` directories owned by uid 1000.

## Verification

End-to-end test with the real Claude Code CLI:

```
$ ANTHROPIC_API_KEY=sk-ant-... \
  VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
  VOID_BOX_INITRAMFS=target/void-box-rootfs.cpio.gz \
  cargo run --example claude_in_voidbox_example

Network: eth0 10.0.2.15/24, gw 10.0.2.2, dns 10.0.2.3
DNS: api.anthropic.com → 160.79.104.10 ✓
TCP: SYN → 160.79.104.10:443 → SYN-ACK → Established ✓
Claude (plan): [API response received successfully]
```
