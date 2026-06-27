# ADR-0005: Model egress as a per-sandbox profile with two-layer, name-based enforcement

- **Status:** Accepted
- **Date:** 2026-06-27
- **Related:** RFC-0002; ADR-0003; ADR-0004; ADR-0006

## Context

Today's network layer is a SLIRP userspace stack with stateless NAT and a default-allow CIDR deny-list: no allow-list, no per-name capability, and no egress audit. Real workflows need a spectrum of reach — open browsing, a bounded set of public services, provider-only, internal-only, fully air-gapped — plus audit, rate-limiting, a kill-switch, and data-exfiltration containment. A domain resolves to many rotating CDN IPs, so a CIDR allow-list cannot keep up and pushes IP-handling onto the guest.

## Decision

We will model egress as a **per-sandbox profile** — a closed enum, set in the spec, defaulting to `open`:

- **`open` (default)** — full internet, direct via NAT (minus a baseline metadata/RFC-1918 deny). Only credentialed flows traverse the proxy, and there is no egress audit. Preserves today's behavior and the cold-boot budget.
- **`monitored`** — full internet, but every flow is CONNECT-tunneled through the proxy so its destination is seen and audited. Content is never decrypted.
- **`allowlist`** — only the listed FQDNs (plus any credentialed endpoints) are reachable, through the proxy.
- **`proxy-only`** — only credentialed endpoints (the LLM providers) are reachable.
- **`none`** — no external egress at all (local providers only).

Enforcement is split across **two layers**, because no single layer can be both unbypassable and name-aware:

- **Network layer (`EgressReach`).** Coarse reach at the IP/CIDR level. `open` allows the internet minus the baseline deny; the restrictive profiles default-deny and pin the guest to its own proxy listener (ADR-0004). It is enforced independently of proxy liveness — if the proxy crashes, a restrictive profile fails closed, never open. This is the bypass-resistant enforcement point: the guest cannot open a socket the host did not allow.
- **Proxy layer.** Fine-grained, name-based policy on the flows routed to it. It matches an FQDN allow-list by hostname (CONNECT host / SNI) using Cilium `toFQDNs` vocabulary — `matchName` (exact) and `matchPattern` (a wildcard `*` that does not cross `.`). It also produces the audit log, applies rate-limits, and is the kill-switch.

The proxy resolves each allowed name per connection and applies a **resolve-once SSRF pin**: it pins the resolved IP for the connection, rejects internal ranges, and never re-resolves at connect time (a re-resolve would reopen a DNS-rebinding gap). So the guest never deals in IPs. `monitored` and `allowlist` introduce **no guest-trusted egress CA** — unlike the credential-injection path (ADR-0002, ADR-0004), the proxy only CONNECT-tunnels these flows, seeing the destination but never the content.

## Consequences

- **Positive:** expresses the full reach spectrum in one model; bypass-resistant (network default-deny) and fail-closed (pin independent of the proxy); name-at-the-proxy follows rotating CDN IPs; a `report`→`enforce` workflow (run `monitored`, harvest destinations with `voidbox egress report`, promote to an `allowlist`) is natural.
- **Negative / cost:** full routing under `monitored`/`allowlist` adds hot-path cost — mitigated by the default `open` keeping selective routing (only credentialed flows traverse the proxy) and by the CONNECT-tunnel avoiding per-byte crypto; ECH / encrypted-SNI degrades hostname inspection, handled by falling back to the set of IPs learned from resolving the allowed names (the DNS-learned-IP allow-set) and failing closed in restrictive profiles.
- **Follow-ups:** content inspection / DLP (TLS-MITM for egress) and arbitrary named TCP via SOCKS5 are deferred non-goals; IPv6 egress is denied this iteration. Platform enforcement of the network layer differs by backend — host-side on KVM, in-guest on VZ (ADR-0006).
