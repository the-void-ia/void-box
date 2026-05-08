#!/usr/bin/env python3
# bench-pasta-concurrent.py — multi-flow companion to bench-pasta.py.
#
# Mirrors examples/crr_concurrent_bench.rs's measurement shape so the
# voidbox-vs-pasta comparison is apples-to-apples at M>1: same C
# crr-client binary, same per-flow N iterations, same aggregation
# (median-of-p50s, max p99, mean-of-means, aggregate qps).
#
# Used to answer "is voidbox's serial-net_poll_thread the
# bottleneck, or is the host-kernel TCP RTT the floor that pasta
# also hits?" — see docs/perf-architectural-experiments.md.
#
# Usage:
#   bench-pasta-concurrent.py --concurrency 4 --iterations 2000
#
# Requires:
#   - pasta on PATH (or via $PASST)
#   - /tmp/crr-client compiled from tools/perf-harness/crr-client.c

from __future__ import annotations

import argparse
import shutil
import socket
import statistics
import subprocess
import sys
import threading
import time
from pathlib import Path

HOST_LISTEN_BACKLOG = 1024


def positive_int(value: str) -> int:
    """Argparse type that rejects non-positive integers.

    `--concurrency 0` would expand the shell `for i in {flow_ids}`
    to an empty body and the harness would silently report zero
    flows; better to fail at parse time.
    """
    parsed = int(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError(
            f"must be a positive integer (got {value!r})"
        )
    return parsed


def resolve_pasta() -> str:
    found = shutil.which("pasta")
    if found:
        return found
    return "/usr/bin/pasta"


def detect_host_gateway() -> str:
    """Returns the netns→host gateway address pasta installs.

    Pasta defaults to the host's default-route gateway; the
    `--map-host-loopback` flag also makes that address proxy to
    127.0.0.1 inside the netns, which is what we connect to.
    """
    out = subprocess.check_output(
        ["ip", "-4", "route", "show", "default"], text=True
    )
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
    raise RuntimeError("no default gateway found in `ip -4 route show default`")


def host_handler(conn: socket.socket) -> None:
    try:
        conn.recv(1)
        conn.sendall(b"x")
    except OSError:
        pass
    finally:
        conn.close()


def host_listener_thread(
    sock: socket.socket, total_expected: int, accepts_done: list[int]
) -> None:
    """Accepts up to `total_expected` connections, dispatching each
    to a per-connection handler thread so M concurrent flows make
    progress simultaneously.

    `accepts_done` is a one-element list used as a mutable counter
    (avoids importing threading primitives for a single counter).
    """
    sock.settimeout(0.5)
    deadline = time.monotonic() + 120.0
    while accepts_done[0] < total_expected and time.monotonic() < deadline:
        try:
            conn, _ = sock.accept()
        except socket.timeout:
            continue
        except OSError:
            return
        accepts_done[0] += 1
        threading.Thread(target=host_handler, args=(conn,), daemon=True).start()


def run_concurrent_in_netns(
    pasta: str,
    crr_client: Path,
    gateway: str,
    port: int,
    concurrency: int,
    iterations: int,
) -> str:
    """Runs `concurrency` parallel crr-client processes inside a
    pasta-managed netns, each doing `iterations` CRRs.  Returns the
    concatenated stdout (one summary line per flow:
    `flow_id n p50_ns p99_ns mean_ns`).
    """
    flow_ids = " ".join(str(flow_id) for flow_id in range(1, concurrency + 1))
    cmd = (
        f"set -eu; rm -rf /tmp/crr_results; mkdir -p /tmp/crr_results; "
        f"for i in {flow_ids}; do "
        f"  {crr_client} {gateway} {port} {iterations} > /tmp/crr_results/$i.txt & "
        f"done; "
        f"wait; "
        f'for i in {flow_ids}; do echo "$i $(cat /tmp/crr_results/$i.txt)"; done'
    )
    completed = subprocess.run(
        [pasta, "-q", "--config-net", "--", "bash", "-c", cmd],
        capture_output=True,
        text=True,
        timeout=600,
        check=False,
    )
    if completed.returncode != 0:
        raise RuntimeError(
            f"pasta netns command failed (rc={completed.returncode}): "
            f"{completed.stderr.strip()[:500]}"
        )
    return completed.stdout


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Multi-flow concurrent CRR microbench against pasta."
    )
    parser.add_argument("--concurrency", type=positive_int, default=4)
    parser.add_argument("--iterations", type=positive_int, default=2000)
    parser.add_argument(
        "--crr-client",
        default="/tmp/crr-client",
        help="Path to static crr-client binary (compiled from "
        "tools/perf-harness/crr-client.c).",
    )
    parser.add_argument(
        "--gateway",
        default=None,
        help="Override the netns→host gateway address.  Defaults to the "
        "host's default-route gateway, which pasta `--map-host-loopback` "
        "proxies to 127.0.0.1.",
    )
    args = parser.parse_args()

    pasta = resolve_pasta()
    if not Path(pasta).exists():
        print(f"ERROR: pasta not found at {pasta}", file=sys.stderr)
        return 2

    crr_client = Path(args.crr_client).resolve()
    if not crr_client.is_file():
        print(
            f"ERROR: crr-client not found at {crr_client}; compile with "
            "`gcc -O2 -static -o /tmp/crr-client tools/perf-harness/crr-client.c`",
            file=sys.stderr,
        )
        return 2

    gateway = args.gateway or detect_host_gateway()
    total_expected = args.concurrency * args.iterations

    sock = socket.socket()
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.bind(("0.0.0.0", 0))
    port = sock.getsockname()[1]
    sock.listen(HOST_LISTEN_BACKLOG)

    accepts_done = [0]
    listener_thread = threading.Thread(
        target=host_listener_thread,
        args=(sock, total_expected, accepts_done),
        daemon=True,
    )
    listener_thread.start()

    print(
        f"Running pasta concurrent CRR: {args.concurrency} flows × "
        f"{args.iterations} CRRs each ({total_expected} total)...",
        file=sys.stderr,
    )

    wall_start = time.perf_counter()
    try:
        stdout = run_concurrent_in_netns(
            pasta, crr_client, gateway, port, args.concurrency, args.iterations
        )
    finally:
        sock.close()
    wall_elapsed = time.perf_counter() - wall_start

    listener_thread.join(timeout=5)
    print(
        f"host accepts: {accepts_done[0]}/{total_expected}",
        file=sys.stderr,
    )

    flows: list[tuple[int, int, int, int, int]] = []
    for line in stdout.strip().splitlines():
        parts = line.split()
        if len(parts) != 5:
            print(f"warning: ignoring malformed line: {line!r}", file=sys.stderr)
            continue
        flow_id = int(parts[0])
        iterations_seen = int(parts[1])
        p50_ns = int(parts[2])
        p99_ns = int(parts[3])
        mean_ns = int(parts[4])
        flows.append((flow_id, iterations_seen, p50_ns, p99_ns, mean_ns))

    if len(flows) != args.concurrency:
        print(
            f"ERROR: expected {args.concurrency} flow summaries, got {len(flows)}",
            file=sys.stderr,
        )
        return 1

    p50s_us = sorted(p50_ns // 1000 for _, _, p50_ns, _, _ in flows)
    p99s_us = sorted(p99_ns // 1000 for _, _, _, p99_ns, _ in flows)
    means_us = sorted(mean_ns // 1000 for _, _, _, _, mean_ns in flows)

    median_of_p50s = p50s_us[len(p50s_us) // 2]
    max_p99 = p99s_us[-1]
    mean_of_means = statistics.mean(means_us)
    aggregate_qps = total_expected / wall_elapsed

    print()
    print(
        f"pasta concurrent CRR: {args.concurrency} flows × {args.iterations} "
        f"iterations ({wall_elapsed:.3f}s wall):"
    )
    for flow_id, iters_seen, p50_ns, p99_ns, mean_ns in flows:
        print(
            f"  flow {flow_id} ({iters_seen} iters): p50={p50_ns // 1000} µs  "
            f"p99={p99_ns // 1000} µs  mean={mean_ns // 1000} µs"
        )
    print()
    print(f"  median-of-p50s:  {median_of_p50s} µs")
    print(f"  max p99:         {max_p99} µs")
    print(f"  mean-of-means:   {mean_of_means:.0f} µs")
    print(f"  aggregate qps:   {aggregate_qps:.0f} CRRs/s")
    return 0


if __name__ == "__main__":
    sys.exit(main())
