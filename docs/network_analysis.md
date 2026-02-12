# Network options: vmm-reference and void-box

## What vmm-reference does for networking

From the [rust-vmm/vmm-reference](https://github.com/rust-vmm/vmm-reference) repo:

- **CLI**: Supports a `--net` option with a `tap` parameter (TAP device name).
- **Implementation**: The README and design docs state that **only the API is in place**; the actual network device configuration and virtio-net backend wiring were left for a **follow-up PR**. So vmm-reference does **not** currently ship a working network path (no TAP attachment to virtio-net, no SLIRP).
- **Purpose**: vmm-reference is a **reference** to validate rust-vmm crates and to serve as a template. Net support is intentionally minimal and not the focus.

**Takeaway**: vmm-reference is **not** a source of a ready-made “best” network implementation for void-box. It only defines a net config shape (e.g. tap name); the data path is not implemented there.

---

## Network backend options (relevant to void-box)

| Option | Pros | Cons | Void-box status |
|--------|------|------|-----------------|
| **SLIRP (user-mode NAT)** | No root, no TAP, no iptables; works in containers/CI; simple. | Single process does TCP/IP in userspace; throughput and latency limits; no raw L2. | **Implemented**: virtio-net MMIO + SLIRP (smoltcp), TX/RX virtqueue path done. |
| **TAP-in-VMM** | Standard model (QEMU, kvmtool); full L2; good for dev and many production setups. | Needs CAP_NET_ADMIN or root to create TAP; couples I/O to VMM process. | **Partial**: `TapDevice` in `src/network/mod.rs` creates a TAP; **not** wired to virtio-net. |
| **vhost-user-net** | Backend in separate process; isolation; used by Cloud Hypervisor. | More moving parts (socket, backend process); heavier to integrate. | **Not implemented**. |

---

## Best option for void-box “network issues”

### 1. **Keep and harden SLIRP (current path)** — best default

- Already implemented and **does not require root or TAP**.
- Fits workflow/sandbox use (curl, API calls, apt in guest).
- **Recommendation**: Treat SLIRP as the default and only path for environments where TAP is not available (e.g. CI, unprivileged). Fix any remaining bugs (e.g. edge cases in NAT, DNS, or virtqueue handling) and add tests.

### 2. **Add optional TAP backend** — best for “real” networking where root is OK

- **vmm-reference** only shows a net *config* (tap name); it does not implement the device. void-box can go further by:
  - Using the existing `TapDevice` in `src/network/mod.rs`.
  - Adding a **second backend** for virtio-net: when `tap_name` is set (and TAP creation succeeds), read/write packets from the TAP fd instead of SLIRP; when not set, keep using SLIRP.
- Gives:
  - **No root**: SLIRP (current).
  - **With root/CAP_NET_ADMIN**: TAP for full L2, better throughput, and bridging if the host is configured for it.

Implementation sketch:

- In the VMM, if `config.network` and `config.tap_name` are set, create `TapDevice`, then create virtio-net with a “TAP backend” that reads/writes the TAP fd (and optionally uses a small buffer/event loop). If TAP creation fails, fall back to SLIRP (or fail fast, depending on policy).
- Reuse the same virtio-net MMIO and virtqueue handling; only the source/sink of packets changes (SLIRP vs TAP).

### 3. **vhost-user-net** — optional later step

- Useful if you want process isolation for the network backend or plan to support multiple backends (e.g. DPDK). Not necessary to fix current “network issues”; consider only if you have a clear need for a separate backend process.

---

## Summary

- **vmm-reference** does not provide a full network implementation; it only reserves a net config (e.g. tap). So the “best option” for void-box is **not** to copy vmm-reference’s net code (there is none), but to choose a backend strategy.
- **Best option for void-box**:
  1. **Default**: Keep **SLIRP** as the main path; fix and test it (address any specific “network issues” you see).
  2. **Optional upgrade**: Add a **TAP backend** for virtio-net (wire existing `TapDevice` to the same virtio-net/virtqueue path), and use it when TAP is available and configured; otherwise keep SLIRP.
  3. **Later**: Consider vhost-user-net only if you need an out-of-process, isolated backend.

This keeps the current design (SLIRP, no root) and adds a clear path to “real” networking (TAP) without depending on vmm-reference’s incomplete net support.
