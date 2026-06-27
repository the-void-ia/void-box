# ADR-0006: Enforce VZ egress in-guest with an eBPF cgroup connect-filter

- **Status:** Accepted
- **Date:** 2026-06-27
- **Related:** RFC-0002; ADR-0005

## Context

The network-layer reach floor (ADR-0005) must be enforced somewhere the untrusted guest cannot bypass. On KVM, void-box runs its own userspace SLIRP/NAT, so enforcement is host-side and bypass-resistant. VZ (Apple Virtualization.framework) has no such point: it attaches Apple's NAT directly, which exposes no host-side filter hook. That NAT also puts every VM on one shared segment, with a single gateway and no per-sandbox host address. So per-sandbox isolation cannot be done host-side by binding, and source IP is unsafe to authorize on (ARP-spoofable). The existing in-guest primitive, a blackhole-route deny-list, matches on destination prefix only and therefore cannot express the port-aware, default-deny allow-list that pin-to-proxy requires (a sandbox's own listener and a neighbour's differ by port on the shared gateway). The required kernel support (`CONFIG_CGROUP_BPF`, cgroup v2) is already present on both the production VZ kernel and the slim dev/test kernel.

## Decision

We will enforce VZ egress in-guest with a root-attached eBPF cgroup `connect4`/`connect6` program (plus `sendmsg4`/`sendmsg6` for unconnected UDP) that allow-lists egress by `(address, port)`, denies the rest with `EPERM`, and rewrites the destination to redirect allowed-but-uncooperative flows to the proxy — the in-guest equivalent of the KVM DNAT. The root guest-agent attaches the program and places the uid-1000 workload in the filtered cgroup before dropping privileges. No kernel rebuild is required. Alternatives — in-guest nftables, or replacing Apple's NAT with a host userspace stack (gvproxy-style) — were rejected for this iteration (the former is heavier and needs a kernel config flag for the slim kernel; the latter is a networking rework deserving its own RFC).

## Consequences

- **Positive:** a real port-aware, default-deny egress floor on VZ; against the modeled uid-1000 adversary — which holds no `CAP_BPF`/`CAP_NET_ADMIN`/`CAP_NET_RAW` — it is a boundary, not merely defense-in-depth; no kernel rebuild on either platform; the `connect4` destination rewrite supplies the transparent pin-to-proxy that VZ otherwise lacks.
- **Negative / cost:** in-guest enforcement is subvertible by a uid-1000→root escalation (a kernel LPE or a guest-agent exploit) — beyond the primary threat model — which on VZ only leaves a transparent-egress audit-attribution residual (KVM is host-side and unaffected); eBPF coverage is connect/`sendmsg`-only and per-cgroup, and the program is offset-coupled to the pinned kernels.
- **Follow-ups:** a host userspace TCP/IP stack (gvproxy-style) would move VZ enforcement host-side and close the residual, matching KVM — deferred to its own RFC. Validated by a VZ uid-1000 bypass test (raw socket, `sendmsg`, cgroup escape).
