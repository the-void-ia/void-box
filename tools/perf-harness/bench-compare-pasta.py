#!/usr/bin/env python3
# bench-compare-pasta.py — produce a markdown side-by-side comparing
# voidbox-network-bench output against bench-pasta.py output.
#
# Both inputs are JSON files with the same field names (the shared
# voidbox-network-bench Report shape).  Either argument can be the
# voidbox or pasta side; the script auto-detects via the `backend`
# field if present, otherwise positional.

from __future__ import annotations

import argparse
import json
import sys
from typing import Any


METRICS = [
    ("tcp_throughput_g2h_mbps", "TCP throughput g2h", "Mbps", False),
    ("tcp_bulk_throughput_g2h_mbps", "TCP bulk g2h (constrained)", "Mbps", False),
    ("tcp_rr_latency_us_p50", "TCP RR latency p50", "µs", True),
    ("tcp_rr_latency_us_p99", "TCP RR latency p99", "µs", True),
    ("tcp_crr_latency_us_p50", "TCP CRR latency p50", "µs", True),
    ("udp_dns_qps", "UDP DNS qps", "qps", False),
    ("icmp_rr_latency_us_p50", "ICMP RR p50", "µs", True),
    ("tcp_rx_latency_us_p50", "TCP RX latency p50", "µs", True),
]


def fmt(value: Any, latency: bool) -> str:
    if value is None:
        return "n/a"
    if isinstance(value, (int, float)):
        if latency:
            if value >= 1000:
                return f"{value / 1000:.2f} ms"
            return f"{value:.1f} µs"
        if value >= 1000:
            return f"{value:.0f}"
        return f"{value:.2f}"
    return str(value)


def fmt_delta(voidbox: Any, pasta: Any, latency: bool) -> str:
    if voidbox is None or pasta is None:
        return "—"
    if pasta == 0:
        return "—"
    ratio = voidbox / pasta
    if latency:
        if ratio >= 1:
            return f"voidbox {ratio:.1f}× slower"
        return f"voidbox {1 / ratio:.2f}× faster"
    if ratio >= 1:
        return f"voidbox {ratio:.2f}× faster"
    return f"voidbox {1 / ratio:.1f}× slower"


def load(path: str) -> dict[str, Any]:
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def detect_role(data: dict[str, Any], default: str) -> str:
    backend = data.get("backend")
    if backend in ("pasta", "voidbox"):
        return backend
    return default


def main() -> int:
    p = argparse.ArgumentParser(description="voidbox vs pasta head-to-head comparison")
    p.add_argument("voidbox_json", help="path to voidbox-network-bench JSON output")
    p.add_argument("pasta_json", help="path to bench-pasta.py JSON output")
    p.add_argument("--output", help="write markdown to file instead of stdout")
    args = p.parse_args()

    voidbox = load(args.voidbox_json)
    pasta = load(args.pasta_json)

    if detect_role(voidbox, "voidbox") == "pasta":
        voidbox, pasta = pasta, voidbox

    lines: list[str] = []
    lines.append("# voidbox vs pasta head-to-head\n")
    lines.append("Methodology per `docs/superpowers/plans/2026-04-27-smoltcp-passt-port.md` §")
    lines.append("\"passt head-to-head methodology\": same host, same workload (`nc`-based g2h /")
    lines.append("RR / CRR), same metric names. **CRR latency is the most apples-to-apples**")
    lines.append("metric — dominated by NAT-table operations on both sides. Throughput numbers")
    lines.append("are not directly comparable: voidbox runs in a real KVM VM (virtio-mmio exit")
    lines.append("overhead); pasta runs in a network namespace (no VM).\n")
    lines.append("| Metric | voidbox (KVM + SLIRP) | pasta (netns) | Δ |")
    lines.append("|---|---:|---:|---|")

    for key, label, _unit, latency in METRICS:
        v = voidbox.get(key)
        pa = pasta.get(key)
        if v is None and pa is None:
            continue
        lines.append(
            f"| {label} | {fmt(v, latency)} | {fmt(pa, latency)} | {fmt_delta(v, pa, latency)} |"
        )

    lines.append("")
    pasta_version = pasta.get("pasta_version")
    if pasta_version:
        lines.append(f"_pasta version: `{pasta_version}`_")
    lines.append("")
    notes = pasta.get("notes")
    if isinstance(notes, list) and notes:
        lines.append("**Notes from pasta side:**")
        for note in notes:
            lines.append(f"- {note}")
        lines.append("")

    md = "\n".join(lines)
    if args.output:
        with open(args.output, "w", encoding="utf-8") as f:
            f.write(md)
        print(f"Report written to {args.output}", file=sys.stderr)
    else:
        print(md)
    return 0


if __name__ == "__main__":
    sys.exit(main())
