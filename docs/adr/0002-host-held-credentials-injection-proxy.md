# ADR-0002: Keep durable credentials off the guest, injected at a host proxy

- **Status:** Accepted
- **Date:** 2026-06-27
- **Related:** RFC-0002; ADR-0003; ADR-0004

## Context

void-box runs an untrusted agent (uid 1000) inside a micro-VM, with prompt injection or a compromised dependency as the expected adversary. Today the `claude-personal` and `codex` providers stage the OAuth credential — including the single-use refresh token — into the guest via an RW mount or a privileged copy, and the API-key providers forward the key into the guest exec env. A uid-1000 agent can read and exfiltrate any of these for access that outlives the run, and snapshots capture them. Both CLIs also self-refresh and rotate refresh tokens in-process, so a guest-side refresher and a host-side one would invalidate each other's token. The same risk shape covers downstream service secrets (a GitHub token, registry credentials) the workflow consumes. The agent must be able to *use* a credential without *holding* the durable one.

## Decision

We will keep durable credentials off the guest entirely. The host holds each provider's durable secret in a credential store. The host is also the sole owner of OAuth refresh and rotation (serialized, rate-capped, with atomic write-back of the rotated refresh token). It injects the credential at network egress through a host-side, TLS-terminating injection proxy. The guest holds only non-secret placeholders: the client is pointed at the proxy and trusts a per-sandbox CA, and the proxy rewrites the credential header(s) with the host-held secret and re-originates TLS to the real upstream. This single mechanism covers API keys, OAuth, and downstream service secrets alike. TLS termination is acceptable under this feature's single-tenant trust model — the host operator is the data owner and already sees guest plaintext — and is explicitly out of scope where the operator must not read tenant data.

## Consequences

- **Positive:** the durable secret never enters the guest; a guest that ignores the proxy obtains no credential (bypass-safe — a direct upstream call carries no valid token); a single rotation owner removes the refresh-conflict failure mode; the external wire stays encrypted because the proxy re-originates TLS.
- **Negative / cost:** the durable secret now lives in host process memory for the run (mitigated by `mlock`/zeroize and per-sandbox process isolation, ADR-0003); the host decrypts inference traffic (trust-model dependent); credentialed egress gains a hot-path availability coupling to the proxy.
- **Follow-ups:** containment of *personal-subscription* OAuth is gated on validation (RFC-0002 V2) and an unresolved provider-policy question; API keys are the policy-clean programmatic path regardless. Token injection (hand a short-lived token to the genuine client) is a contingency only if proxying a personal subscription proves unviable. A "no real credential in the guest" assertion gates each provider migration.
