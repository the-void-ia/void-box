# ADR-0004: Isolate runs with a per-run listener, token, and name-constrained CA

- **Status:** Accepted
- **Date:** 2026-06-27
- **Related:** RFC-0002; ADR-0003

## Context

A single shared proxy (ADR-0003) serving mutually-untrusted runs must attribute each connection to the run that owns it and prevent one run from acting as another. No single mechanism does this. An in-band marker cannot attribute transparently intercepted (DNAT'd) flows, which carry no header the guest could stamp. Terminating TLS to inject a credential (ADR-0002) requires the client to trust a CA, which — if unconstrained — would let a leaked CA key impersonate any site to the guest.

## Decision

We will isolate runs with three per-run mechanisms on three **orthogonal** axes — attribution, authentication, trust scope — each closing a gap the others cannot, so they are complementary rather than redundant:

| Axis | What | Why it is needed | How it works |
|------|------|------------------|--------------|
| **Per-run listener** — attribution ("which run is this?") | a distinct host-side proxy socket per run | a shared proxy must tell whose connection this is, and transparently intercepted (DNAT'd) flows carry no header to read | a network-layer rule denies a run every gateway hop but its own listener (plus DNS and enabled host-service ports), so the **arrival socket is the run's identity** — no guest cooperation |
| **Per-run token** — authentication ("is it really that run's guest?") | a ≥128-bit CSPRNG secret, unique per run | socket-attribution is only trustworthy while that network rule holds — airtight host-side on KVM, but in-guest and subvertible by a root escalation on VZ (ADR-0006); the token re-binds a connection to its run when the floor can be breached | presented as `Proxy-Authorization` on an explicit `CONNECT`, constant-time compared, stripped before forwarding; a neighbour cannot present a token it cannot read across the VM boundary. **Primary cross-run control on VZ; defense-in-depth on KVM.** Never logged or placed in an `EgressEvent` |
| **Per-run CA** — trust scope ("what may the proxy impersonate?") | a short-lived, name-constrained CA (ECDSA P-256); public cert in that one guest, private key host-only | injecting a credential (ADR-0002) means terminating TLS *as the upstream*, which requires the client to trust a CA — an unconstrained one would be a skeleton key that could impersonate any site the guest reaches | X.509 **Name Constraints** limit the CA to the run's declared upstreams; a compliant client rejects any leaf outside them, so even a leaked key impersonates only those upstreams, only that run, only until teardown |

**Worked example (the CA).** Claude Code is pointed at the proxy expecting `api.anthropic.com`. The proxy mints a certificate for `api.anthropic.com` signed by the run's CA; the client accepts it — trusted signer, matching name, and within the CA's name constraints — so the proxy terminates TLS, swaps the guest's placeholder for the real key, and re-originates TLS to the real upstream. If that same CA were used to present a cert for `github.com`, a compliant client **rejects** it: `github.com` is outside the CA's name constraints, even though the signature is valid. So the CA can impersonate only the run's own upstreams, never an arbitrary site — which holds only as long as the client enforces name constraints (RFC-0002 V1).

The token and CA are re-minted on snapshot restore; the CA private key and the credential store are structurally absent from the guest, the kernel cmdline, and any snapshot.

## Consequences

- **Positive:** attribution works for transparent flows (socket-based, no header needed); the token backstops authentication where the network rule is enforced from a weaker position (VZ, ADR-0006); the name-constrained CA bounds the blast radius of a CA-key leak.
- **Negative / cost:** the token is guest-readable by design, so it guards against neighbouring runs, not the in-guest adversary; per-run CA keygen lands on the cold-start budget; the gateway port space bounds concurrent runs.
- **Follow-ups:** confirm each client *enforces* the CA's name constraints (RFC-0002 V1) — a client that ignores them turns the CA into a universal MITM anchor. Live reload of fresh CA/token into an already-running client after restore is deferred; in-flight credentialed egress fails closed until the client restarts.
