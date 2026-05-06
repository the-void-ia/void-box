#!/usr/bin/env python3
# bench-pasta.py — passt/pasta side of the head-to-head comparison.
#
# Drives the same workload shape as `voidbox-network-bench`:
#   - tcp_throughput_g2h_mbps     (sustained guest→host throughput)
#   - tcp_rr_latency_us_p50/p99   (persistent-connection round-trip)
#   - tcp_crr_latency_us_p50      (connect-request-response latency)
#
# The "guest" is a process running inside a pasta-managed network
# namespace.  Pasta forwards the host's gateway address into the netns
# as a translation for the host's loopback (its --map-host-loopback
# default), so connecting to the host gateway IP from inside the netns
# reaches the host's 127.0.0.1.  This mirrors voidbox's SLIRP
# convention (10.0.2.2 → 127.0.0.1) closely enough for the metric
# comparison to be apples-to-apples on the NAT path.
#
# Methodology aligns with docs/superpowers/plans/2026-04-27-smoltcp-passt-port.md
# § "passt head-to-head methodology": same host, same workload, same
# metric names, focus on CRR latency (dominated by NAT-table ops, not
# MMIO exit overhead).

from __future__ import annotations

import argparse
import json
import os
import socket
import statistics
import subprocess
import sys
import threading
import time
from dataclasses import asdict, dataclass, field
from typing import Optional


@dataclass
class Report:
    tcp_bulk_throughput_g2h_mbps: Optional[float] = None
    tcp_throughput_g2h_mbps: Optional[float] = None
    tcp_throughput_h2g_mbps: Optional[float] = None
    tcp_rr_latency_us_p50: Optional[float] = None
    tcp_rr_latency_us_p99: Optional[float] = None
    tcp_crr_latency_us_p50: Optional[float] = None
    udp_dns_qps: Optional[float] = None
    icmp_rr_latency_us_p50: Optional[float] = None
    tcp_rx_latency_us_p50: Optional[float] = None
    backend: str = "pasta"
    pasta_version: Optional[str] = None
    notes: list[str] = field(default_factory=list)


def _resolve_pasta() -> str:
    """Find a pasta binary in $PATH or fall back to /usr/bin/pasta."""
    import shutil
    found = shutil.which("pasta")
    if found:
        return found
    return "/usr/bin/pasta"


def detect_host_gateway() -> str:
    """Return the host's IPv4 default-route gateway address.

    Parses ``ip -4 route show default`` for ``default via <GW> ...`` lines
    and returns the address after ``via``.  Routes of the form
    ``default dev <IFACE> ...`` (no ``via``) are skipped — they don't
    name a usable IP for pasta's ``--map-host-loopback`` translation.
    """
    out = subprocess.check_output(["ip", "-4", "route", "show", "default"], text=True)
    for line in out.splitlines():
        parts = line.split()
        if not parts or parts[0] != "default":
            continue
        try:
            via_index = parts.index("via")
        except ValueError:
            continue
        if via_index + 1 < len(parts):
            return parts[via_index + 1]
    raise RuntimeError(
        "no IPv4 default gateway with a 'via' field found in `ip route show default` output"
    )


def pasta_version(pasta: str) -> str:
    out = subprocess.run([pasta, "--version"], capture_output=True, text=True, check=False)
    first = out.stdout.splitlines() or [""]
    return first[0].strip()


def free_port() -> int:
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def run_in_netns(pasta: str, cmd: str, *, timeout: float) -> subprocess.CompletedProcess[str]:
    """Run `cmd` inside a fresh pasta-managed network namespace."""
    return subprocess.run(
        [pasta, "-q", "--config-net", "--", "bash", "-c", cmd],
        capture_output=True,
        text=True,
        timeout=timeout,
        check=False,
    )


def measure_g2h_throughput(
    pasta: str,
    gw: str,
    iterations: int,
    transfer_mb: int,
) -> Optional[float]:
    samples_mbps: list[float] = []
    for i in range(iterations):
        port = free_port()
        result_box: dict[str, object] = {}

        srv = socket.socket()
        srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        srv.bind(("127.0.0.1", port))
        srv.listen(1)
        srv.settimeout(30.0)

        def host_drain() -> None:
            try:
                conn, _ = srv.accept()
            except socket.timeout:
                result_box["error"] = "accept timeout"
                return
            start = time.perf_counter()
            total = 0
            with conn:
                while True:
                    buf = conn.recv(1 << 16)
                    if not buf:
                        break
                    total += len(buf)
            result_box["bytes"] = total
            result_box["elapsed"] = time.perf_counter() - start

        worker = threading.Thread(target=host_drain, daemon=True)
        worker.start()
        time.sleep(0.2)

        cmd = f"dd if=/dev/zero bs=1M count={transfer_mb} 2>/dev/null | nc {gw} {port}"
        try:
            run_in_netns(pasta, cmd, timeout=60)
        except subprocess.TimeoutExpired:
            print(f"g2h[{i:>2}]: client timeout; skipping", file=sys.stderr)
            srv.close()
            continue

        worker.join(timeout=10)
        srv.close()

        if "error" in result_box:
            print(f"g2h[{i:>2}]: {result_box['error']}; skipping", file=sys.stderr)
            continue
        bytes_received = int(result_box.get("bytes", 0))
        elapsed = float(result_box.get("elapsed", 0.0))
        if bytes_received <= 0 or elapsed < 1e-4:
            print(f"g2h[{i:>2}]: bytes={bytes_received} elapsed={elapsed}s; skipping", file=sys.stderr)
            continue
        mbps = bytes_received * 8 / elapsed / 1_000_000
        print(
            f"g2h[{i:>2}]: {bytes_received} B in {elapsed:.3f}s = {mbps:.1f} Mbps",
            file=sys.stderr,
        )
        samples_mbps.append(mbps)

    if not samples_mbps:
        return None
    return sum(samples_mbps) / len(samples_mbps)


def measure_rr_latency(
    pasta: str,
    gw: str,
    iterations: int,
    samples_per_iter: int,
) -> tuple[Optional[float], Optional[float]]:
    all_samples_us: list[float] = []
    for i in range(iterations):
        port = free_port()
        srv = socket.socket()
        srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        srv.bind(("127.0.0.1", port))
        srv.listen(1)
        srv.settimeout(30.0)

        result_box: dict[str, object] = {}

        def host_echo() -> None:
            try:
                conn, _ = srv.accept()
            except socket.timeout:
                result_box["error"] = "accept timeout"
                return
            samples: list[float] = []
            with conn:
                buf = bytearray(1)
                for _ in range(samples_per_iter):
                    start = time.perf_counter_ns()
                    nrecv = conn.recv_into(buf, 1)
                    if nrecv == 0:
                        break
                    conn.sendall(bytes(buf[:1]))
                    samples.append((time.perf_counter_ns() - start) / 1000.0)
            result_box["samples"] = samples

        worker = threading.Thread(target=host_echo, daemon=True)
        worker.start()
        time.sleep(0.2)

        # Send `samples_per_iter` zero bytes.  The guest doesn't read
        # the echoed bytes back; host-side timing is the ground truth.
        cmd = f"dd if=/dev/zero bs=1 count={samples_per_iter} 2>/dev/null | nc {gw} {port} >/dev/null"
        try:
            run_in_netns(pasta, cmd, timeout=60)
        except subprocess.TimeoutExpired:
            print(f"rr[{i:>2}]: client timeout; skipping", file=sys.stderr)
            srv.close()
            continue

        worker.join(timeout=10)
        srv.close()

        if "error" in result_box:
            print(f"rr[{i:>2}]: {result_box['error']}; skipping", file=sys.stderr)
            continue
        iter_samples = list(result_box.get("samples", []))
        if len(iter_samples) > 1:
            iter_samples.pop(0)
        if not iter_samples:
            print(f"rr[{i:>2}]: no samples; skipping", file=sys.stderr)
            continue
        p50 = statistics.median(iter_samples)
        print(f"rr[{i:>2}]: {len(iter_samples)} samples, p50={p50:.1f} µs", file=sys.stderr)
        all_samples_us.extend(iter_samples)

    if not all_samples_us:
        return None, None
    sorted_s = sorted(all_samples_us)
    n = len(sorted_s)
    p50 = sorted_s[n // 2]
    p99_idx = max(0, int(round(0.99 * (n - 1))))
    p99 = sorted_s[p99_idx]
    return p50, p99


def measure_crr_latency(
    pasta: str,
    gw: str,
    iterations: int,
    samples_per_iter: int,
) -> Optional[float]:
    all_samples_us: list[float] = []
    for i in range(iterations):
        port = free_port()
        srv = socket.socket()
        srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        srv.bind(("127.0.0.1", port))
        srv.listen(64)
        srv.settimeout(30.0)

        result_box: dict[str, object] = {}

        def host_accept_loop() -> None:
            samples: list[float] = []
            for _ in range(samples_per_iter):
                # Start the timer BEFORE accept() so each sample includes
                # the TCP connect + accept latency, matching
                # voidbox-network-bench's measure_crr_latency semantics
                # (its crr_echo_server starts the timer before
                # accept_with_deadline).  Without this, the two
                # harnesses report different metrics under the same
                # name and the side-by-side comparison becomes
                # meaningless.
                start = time.perf_counter_ns()
                try:
                    conn, _ = srv.accept()
                except socket.timeout:
                    break
                with conn:
                    # one read + one write keeps it a true CRR round-trip
                    try:
                        conn.recv(1)
                        conn.sendall(b"x")
                    except OSError:
                        pass
                samples.append((time.perf_counter_ns() - start) / 1000.0)
            result_box["samples"] = samples

        worker = threading.Thread(target=host_accept_loop, daemon=True)
        worker.start()
        time.sleep(0.2)

        # Guest: a tight loop of independent nc invocations
        cmd = (
            f"for _ in $(seq 1 {samples_per_iter}); do "
            f"echo y | nc {gw} {port} >/dev/null; done"
        )
        try:
            run_in_netns(pasta, cmd, timeout=120)
        except subprocess.TimeoutExpired:
            print(f"crr[{i:>2}]: client timeout; skipping", file=sys.stderr)
            srv.close()
            continue

        worker.join(timeout=15)
        srv.close()

        iter_samples = list(result_box.get("samples", []))
        if not iter_samples:
            print(f"crr[{i:>2}]: no samples; skipping", file=sys.stderr)
            continue
        p50 = statistics.median(iter_samples)
        print(f"crr[{i:>2}]: {len(iter_samples)} samples, p50={p50:.0f} µs", file=sys.stderr)
        all_samples_us.extend(iter_samples)

    if not all_samples_us:
        return None
    sorted_s = sorted(all_samples_us)
    return sorted_s[len(sorted_s) // 2]


def main() -> int:
    parser = argparse.ArgumentParser(description="passt/pasta head-to-head bench harness")
    parser.add_argument(
        "--pasta",
        default=os.environ.get("PASTA") or _resolve_pasta(),
        help="path to the pasta binary; default $PASTA, or `pasta` on PATH, or system /usr/bin/pasta",
    )
    parser.add_argument("--iterations", type=int, default=3)
    parser.add_argument("--transfer-mb", type=int, default=50)
    parser.add_argument("--rr-samples", type=int, default=100)
    parser.add_argument("--crr-samples", type=int, default=30)
    parser.add_argument("--output", default=None, help="path to write JSON; default stdout")
    args = parser.parse_args()

    if not os.access(args.pasta, os.X_OK):
        print(f"pasta not executable: {args.pasta}", file=sys.stderr)
        return 2

    gw = detect_host_gateway()
    version = pasta_version(args.pasta)
    print(f"pasta: {version}", file=sys.stderr)
    print(f"host gateway (acts as host-loopback inside netns): {gw}", file=sys.stderr)

    report = Report(backend="pasta", pasta_version=version)
    report.notes.append(
        "pasta runs in a network namespace (no VM); excludes the MMIO/virtio-mmio overhead "
        "that voidbox-network-bench includes.  CRR latency is the most apples-to-apples metric "
        "because it is dominated by NAT-table operations on both sides."
    )

    print("\n--- TCP throughput g2h ---", file=sys.stderr)
    report.tcp_throughput_g2h_mbps = measure_g2h_throughput(
        args.pasta, gw, args.iterations, args.transfer_mb
    )

    print("\n--- TCP RR latency ---", file=sys.stderr)
    p50, p99 = measure_rr_latency(args.pasta, gw, args.iterations, args.rr_samples)
    report.tcp_rr_latency_us_p50 = p50
    report.tcp_rr_latency_us_p99 = p99

    print("\n--- TCP CRR latency ---", file=sys.stderr)
    report.tcp_crr_latency_us_p50 = measure_crr_latency(
        args.pasta, gw, args.iterations, args.crr_samples
    )

    payload = json.dumps(asdict(report), indent=2)
    if args.output:
        with open(args.output, "w", encoding="utf-8") as f:
            f.write(payload)
            f.write("\n")
        print(f"\nReport written to {args.output}", file=sys.stderr)
    else:
        print()
        print(payload)
    return 0


if __name__ == "__main__":
    sys.exit(main())
