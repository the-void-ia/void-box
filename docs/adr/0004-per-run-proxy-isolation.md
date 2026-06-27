# ADR-0004: Isolate runs with a per-run listener, token, and name-constrained CA

- **Status:** Accepted
- **Date:** 2026-06-27
- **Related:** RFC-0002; ADR-0003

## Context

A single shared proxy (ADR-0003) serving mutually-untrusted runs must attribute each connection to the run that owns it and prevent one run from acting as another. No single mechanism does this. An in-band marker cannot attribute transparently intercepted (DNAT'd) flows, which carry no header the guest could stamp. Terminating TLS to inject a credential (ADR-0002) requires the client to trust a CA, which — if unconstrained — would let a leaked CA key impersonate any site to the guest.

## Decision

We will isolate runs with three per-run mechanisms on three distinct axes:

- **Per-run listener (attribution):** each run gets its own host-side proxy socket; the socket a connection arrives on identifies the run. A network-layer rule denies a run every gateway hop except its own listener (plus DNS and enabled host-service ports), so the arrival socket is a trustworthy identity with no guest cooperation.
- **Per-run token (authentication):** a ≥128-bit CSPRNG value presented as `Proxy-Authorization` on an explicit `CONNECT`, compared in constant time and stripped before forwarding; it proves a connection came from the run's legitimate guest. Never written to a log, an `EgressEvent`, or an error surface.
- **Per-run CA (trust scope):** a short-lived, name-constrained CA (ECDSA P-256) whose public cert is installed in that one guest and whose private key never leaves the host; it bounds what the proxy may impersonate to that run's own upstreams.

The token and CA are re-minted on snapshot restore; the CA private key and the credential store are structurally absent from the guest, the kernel cmdline, and any snapshot.

## Consequences

- **Positive:** attribution works for transparent flows (socket-based, no header needed); the token backstops authentication where the network rule is enforced from a weaker position (VZ, ADR-0006); the name-constrained CA bounds the blast radius of a CA-key leak.
- **Negative / cost:** the token is guest-readable by design, so it guards against neighbouring runs, not the in-guest adversary; per-run CA keygen lands on the cold-start budget; the gateway port space bounds concurrent runs.
- **Follow-ups:** confirm each client *enforces* the CA's name constraints (RFC-0002 V1) — a client that ignores them turns the CA into a universal MITM anchor. Live reload of fresh CA/token into an already-running client after restore is deferred; in-flight credentialed egress fails closed until the client restarts.
