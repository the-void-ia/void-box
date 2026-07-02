# ADR-0004: Isolate sandboxes with a per-sandbox listener, token, and name-constrained CA

- **Status:** Accepted
- **Date:** 2026-06-27
- **Related:** RFC-0002; ADR-0003

> **Amendment (RFC-0002 §A is the reconciled source).** The decision below records the per-sandbox token as presented on `Proxy-Authorization` on an explicit `CONNECT`. That carrier holds for the egress path, which reaches the proxy through a `CONNECT`. The M0 credential path instead redirects the client by base URL and issues no `CONNECT`, so it carries the token in an `x-voidbox-proxy-token` request header on the redirected request. RFC-0002 §A describes both carriers. This ADR is left unedited per the superseded-not-edited convention; consult RFC-0002 §A for the current wire contract.

## Context

A single shared proxy (ADR-0003) serving mutually-untrusted sandboxes must attribute each connection to the sandbox that owns it and prevent one sandbox from acting as another. No single mechanism does this. An in-band marker cannot attribute transparently intercepted (DNAT'd) flows, which carry no header the guest could stamp. Terminating TLS to inject a credential (ADR-0002) requires the client to trust a CA, which — if unconstrained — would let a leaked CA key impersonate any site to the guest.

## Decision

We will isolate sandboxes with three per-sandbox mechanisms on three **orthogonal** axes — attribution, authentication, trust scope — each closing a gap the others cannot, so they are complementary rather than redundant:

| Axis | What | Why it is needed | How it works |
|------|------|------------------|--------------|
| **Per-sandbox listener** — attribution ("which sandbox is this?") | a distinct host-side proxy socket per sandbox | a shared proxy must tell whose connection this is, and transparently intercepted (DNAT'd) flows carry no header to read | a network-layer rule denies a sandbox every gateway hop but its own listener (plus DNS and enabled host-service ports), so the **arrival socket is the sandbox's identity** — no guest cooperation |
| **Per-sandbox token** — authentication ("is it really that sandbox's guest?") | a ≥128-bit CSPRNG secret, unique per sandbox | Socket-attribution is only as reliable as the rule that keeps a sandbox off another's listener. That rule is strong on KVM but weaker on VZ, so the token adds an independent check (unpacked below). | presented as `Proxy-Authorization` on an explicit `CONNECT`, constant-time compared, stripped before forwarding; never logged or placed in an `EgressEvent` |
| **Per-sandbox CA** — trust scope ("what may the proxy impersonate?") | a short-lived, name-constrained CA (ECDSA P-256); public cert in that one guest, private key host-only | injecting a credential (ADR-0002) means terminating TLS *as the upstream*, which requires the client to trust a CA — an unconstrained one would be a skeleton key that could impersonate any site the guest reaches | X.509 **Name Constraints** limit the CA to the sandbox's declared upstreams; a compliant client rejects any leaf outside them, so even a leaked key impersonates only those upstreams, only that sandbox, only until teardown |

**Why the token, on top of the listener.** The listener decides which sandbox a connection belongs to from the socket it arrives on. That works only as long as the network rule actually stops one sandbox from reaching another sandbox's listener. On KVM that rule runs on the host, where a guest cannot tamper with it, so the listener alone is enough and the token is just extra insurance. On VZ, where Apple's NAT offers no host-side enforcement point, the rule is enforced inside the guest by an eBPF filter (ADR-0006). That filter holds against the modeled uid-1000 adversary, but a sandbox that escalated to root could defeat it and reach a neighbour's listener — and then the arrival socket would point at the wrong sandbox. The token is the check that still holds in that case: every connection must carry its own sandbox's secret, and a neighbour neither has that secret nor can read it across the VM boundary. So on VZ the token is the primary defense against one sandbox impersonating another, and on KVM it is defense-in-depth. Closing the VZ gap entirely would require moving enforcement host-side (a gvproxy-style stack), which is deferred (ADR-0006 follow-up; RFC-0002 Unresolved questions).

**Worked example (the CA).** Claude Code is pointed at the proxy expecting `api.anthropic.com`. The proxy mints a certificate for `api.anthropic.com` signed by the sandbox's CA; the client accepts it — trusted signer, matching name, and within the CA's name constraints — so the proxy terminates TLS, swaps the guest's placeholder for the real key, and re-originates TLS to the real upstream. If that same CA were used to present a cert for `github.com`, a compliant client **rejects** it: `github.com` is outside the CA's name constraints, even though the signature is valid. So the CA can impersonate only the sandbox's own upstreams, never an arbitrary site — which holds only as long as the client enforces name constraints (RFC-0002 V1).

The token and CA are re-minted on snapshot restore; the CA private key and the credential store are structurally absent from the guest, the kernel cmdline, and any snapshot.

## Consequences

- **Positive:** attribution works for transparent flows (socket-based, no header needed); the token backstops authentication where the network rule is enforced from a weaker position (VZ, ADR-0006); the name-constrained CA bounds the blast radius of a CA-key leak.
- **Negative / cost:** the token is guest-readable by design, so it guards against neighbouring sandboxes, not the in-guest adversary; per-sandbox CA keygen lands on the cold-start budget; the gateway port space bounds concurrent sandboxes.
- **Follow-ups:** confirm each client *enforces* the CA's name constraints (RFC-0002 V1) — a client that ignores them turns the CA into a universal MITM anchor. Live reload of fresh CA/token into an already-running client after restore is deferred; in-flight credentialed egress fails closed until the client restarts.
