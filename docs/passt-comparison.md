# passt head-to-head comparison harness

Two scripts under `scripts/` produce a side-by-side comparison of voidbox
(real KVM VM + SLIRP) against passt's [`pasta`](https://passt.top/passt/about/)
running in a network namespace.

This is the deferred deliverable from
[`docs/superpowers/plans/2026-04-27-smoltcp-passt-port.md`](superpowers/plans/2026-04-27-smoltcp-passt-port.md)
§ "passt head-to-head methodology".

## What the harness measures

Both sides run the same workload shape — the same fields the
`voidbox-network-bench` `Report` already emits:

| Field | Workload |
|---|---|
| `tcp_throughput_g2h_mbps` | `dd if=/dev/zero bs=1M count=N \| nc HOST PORT` from inside the guest / netns; host TCP server times the drain |
| `tcp_rr_latency_us_p50/p99` | Persistent connection, host-side echo loop bouncing one byte per round trip |
| `tcp_crr_latency_us_p50` | Independent `nc` invocations in a tight loop; host-side timing of the full accept→read→write→close cycle |

The pasta side uses `pasta -- COMMAND` to run the client inside a fresh
network namespace.  Pasta's `--map-host-loopback` (default: the host's
gateway IP) translates to the host's loopback, so the client connects
to `<host-gateway>:PORT` and reaches the host server bound on `127.0.0.1:PORT`.

## What it's good for

**CRR latency is the most apples-to-apples metric** — it's dominated by
NAT-table operations and the round-trip path through the user-mode
networking stack, which is the same code on both sides.  Per the spec:

> Connect rate (CRR latency) is the most apples-to-apples metric —
> dominated by NAT-table operations, not MMIO. If passt does CRR in 135 µs
> and we do 600 µs, that's a meaningful "we have 4× more overhead per
> connect" signal that this refactor should narrow.

## What it's not

**Throughput numbers are not directly comparable.**

- voidbox runs a real KVM VM; every packet incurs `virtio-mmio`
  exits, vCPU IPI overhead, and per-packet copy across the device
  boundary.
- pasta runs in a network namespace; the data path is just user-mode
  socket forwarding, no VM, no MMIO.

The throughput gap is therefore a *sum of the user-mode overhead the
two stacks share* plus *the VM transit cost only voidbox pays*.
Use the throughput numbers as a sanity bound, not a parity target.

A proper VM-vs-VM comparison would run passt under
`qemu-system-x86_64` with a guest image carrying `nc` / `iperf3`.
That is documented as a separate follow-up; the harness here is the
quick, low-friction sibling that exercises the apples-to-apples
metric (CRR) without requiring an extra guest image.

## Usage

```bash
# Generate voidbox numbers (requires VOID_BOX_KERNEL/VOID_BOX_INITRAMFS).
cargo run --release --bin voidbox-network-bench -- \
    --iterations 3 --output /tmp/voidbox-bench.json

# Generate pasta numbers (requires pasta on PATH or via $PASTA).
scripts/bench-pasta.py --output /tmp/pasta-bench.json

# Side-by-side markdown.
scripts/bench-compare-pasta.py /tmp/voidbox-bench.json /tmp/pasta-bench.json \
    --output /tmp/voidbox-vs-pasta.md
```

`scripts/bench-pasta.py --help` lists tunables (iterations, transfer
size, sample counts).

## Reading the report

| Δ column | Meaning |
|---|---|
| `voidbox N× faster`  (throughput) | voidbox has the higher Mbps number |
| `voidbox N× slower`  (throughput) | pasta has the higher Mbps number — expected, since pasta has no VM |
| `voidbox N× faster`  (latency)    | voidbox has the lower µs number |
| `voidbox N× slower`  (latency)    | pasta has the lower µs number — large multiples here mean voidbox spends much of its CRR time outside the NAT path (poll-thread cadence, vCPU exits, virtio handling) |

A useful CRR signal: if `voidbox N× slower on CRR p50` is much larger
than `voidbox N× slower on RR p50`, the per-connection overhead is the
bottleneck, not the data path.  RR p50 captures the data path; CRR
captures the connect path.
