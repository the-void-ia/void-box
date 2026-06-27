# ADR-0005: Model egress as a per-sandbox profile with two-layer, name-based enforcement

- **Status:** Accepted
- **Date:** 2026-06-27
- **Related:** RFC-0002; ADR-0003; ADR-0006

## Context

Today's network layer is a SLIRP userspace stack with stateless NAT and a default-allow CIDR deny-list: no allow-list, no per-name capability, and no egress audit. Real workflows need a spectrum of reach — open browsing, a bounded set of public services, provider-only, internal-only, fully air-gapped — plus audit, rate-limiting, a kill-switch, and data-exfiltration containment. A domain resolves to many rotating CDN IPs, so a CIDR allow-list cannot keep up and pushes IP-handling onto the guest.

## Decision

We will model egress as a per-sandbox profile (a closed enum: `open` default, `monitored`, `allowlist`, `proxy-only`, `none`), enforced at two layers. The **network layer** (`EgressReach`) enforces coarse reach: `open` allows the internet minus a baseline metadata/RFC-1918 deny; the restrictive profiles default-deny and pin the guest to its own proxy listener — independently of proxy liveness, so a proxy crash fails closed, never open. The **proxy layer** enforces a fine-grained FQDN allow-list by hostname (CONNECT host / SNI), using Cilium `toFQDNs` vocabulary (`matchName` exact + `matchPattern` wildcard that does not cross `.`), and produces the audit log, rate-limits, and kill-switch. Names are resolved per connection at the proxy with a resolve-once SSRF pin (reject internal ranges, no connect-time re-resolve), so the guest never deals in IPs. `monitored` is destinations-only — a CONNECT-tunnel that sees the destination but never decrypts content and introduces no guest-trusted egress CA.

## Consequences

- **Positive:** expresses the full reach spectrum in one model; bypass-resistant (network default-deny) and fail-closed (pin independent of the proxy); name-at-the-proxy follows rotating CDN IPs; a `report`→`enforce` workflow (run `monitored`, harvest destinations with `voidbox egress report`, promote to an `allowlist`) is natural.
- **Negative / cost:** full routing under `monitored`/`allowlist` adds hot-path cost — mitigated by the default `open` keeping selective routing (only credentialed flows traverse the proxy) and by the CONNECT-tunnel avoiding per-byte crypto; ECH / encrypted-SNI degrades hostname inspection, handled by falling back to the DNS-learned-IP allow-set and failing closed in restrictive profiles.
- **Follow-ups:** content inspection / DLP (TLS-MITM for egress) and arbitrary named TCP via SOCKS5 are deferred non-goals; IPv6 egress is denied this iteration. Platform enforcement of the network layer differs by backend — host-side on KVM, in-guest on VZ (ADR-0006).
