# ADR-0007: Bind the VZ credential-proxy listener to the NAT gateway address

- **Status:** Accepted
- **Date:** 2026-07-09
- **Related:** RFC-0002; ADR-0003; ADR-0004; ADR-0006

## Context

The credential proxy's per-sandbox listener fronts a pre-auth TLS/HTTP parser with a real credential behind it, so where it binds decides who can reach that surface. On Linux/KVM it binds host loopback: each VM's own SLIRP instance forwards the guest's `10.0.2.2:<port>` hop onto it, and nothing off-host can route to it. macOS/VZ has no equivalent: Apple's NAT (`VZNATNetworkDeviceAttachment`) exposes no host-side forwarding hook, puts every VZ guest on one shared segment behind a single gateway (`192.168.64.1` on the host-local `bridge100` interface), and offers no per-sandbox host address to bind. The pre-existing shared helper for guest-reachable host services binds `0.0.0.0` on macOS, which also exposes the listener to the host's LAN — an unacceptable surface for a credential-injecting parser, and the reason the proxy was gated to Linux-only through M0/M1a. The RFC's full answer — an in-guest eBPF per-sandbox network rule (ADR-0006) — belongs to the egress track and is not a prerequisite the credential track should block on.

## Decision

We will bind the VZ credential-proxy listener to the VZ NAT gateway address (`192.168.64.1`) via a dedicated `credential_proxy_bind_addr()` helper, and lift the platform gate to admit macOS/VZ. The gateway address is on a host-local interface: guests reach it as their default gateway, host processes can reach it locally, and other LAN hosts cannot route to it. The interface exists only while a VZ NAT VM is running, which the proxy's lifecycle already satisfies (it registers listeners after guest boot). The shared `guest_accessible_bind_addr()` helper and its other users (the messaging sidecar) are unchanged. Because every VZ guest shares the segment, a sandbox can reach a neighbor sandbox's listener at the network layer; the per-sandbox proxy token — a ≥128-bit CSPRNG value checked in constant time before any upstream connect — is therefore the sole cross-sandbox control on VZ, the same recorded posture as M0 on KVM's shared loopback (RFC-0002, M0 deviations) until the per-sandbox network rule lands with the egress track.

## Consequences

- **Positive:** KVM/VZ parity for the credential proxy without pulling the egress-track eBPF work forward; no LAN exposure (unlike `0.0.0.0`); no change to other guest-reachable services; the proxy's existing token/CA isolation model carries over unmodified.
- **Negative / cost:** the listener is reachable by sibling VZ sandboxes and host-local processes, token-gated only — on VZ the token is load-bearing, not defense-in-depth; the `192.168.64.1` gateway address is an observed Virtualization.framework convention (already relied on by `guest_host_gateway()` for the sidecar), not a documented API contract; a bind attempted with no VZ NAT VM running fails, surfaced with a named error.
- **Follow-ups:** the in-guest per-sandbox network rule (ADR-0006, egress track) restores the token to defense-in-depth on VZ; Keychain write-back for `claude-personal` on macOS is unimplemented, so the host OAuth store requires file-based credentials there and fails closed on a Keychain-only login; a host userspace network stack (gvproxy-style) would give VZ true host-side per-sandbox enforcement — deferred to its own RFC.
