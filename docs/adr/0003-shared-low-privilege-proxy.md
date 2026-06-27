# ADR-0003: Serve every sandbox from one shared low-privilege proxy

- **Status:** Accepted
- **Date:** 2026-06-27
- **Related:** RFC-0002; ADR-0002; ADR-0004

## Context

The credential injector (ADR-0002) and the egress policy (ADR-0005) share one per-connection handler pipeline. That pipeline can run as one process per sandbox or as a single shared process. A process per sandbox would isolate sandboxes by the OS boundary between one proxy and the next, but each would carry its own `mlock`ed credential store, so memory cost grows linearly with VM count and fights VM density (pressure on the host's memory-reclaim mechanisms — kernel same-page merging (KSM) and the balloon driver). The proxy also parses attacker-controlled input (HTTP, CONNECT, TLS ClientHello) on the host, in the hot path before any auth gate — a wider surface than today's narrow authenticated vsock protocol.

## Decision

We will serve every sandbox from a single shared host proxy process — built fresh in Rust (`rustls`/`hyper`) — rather than one process per VM. It runs as a distinct, **low-privilege uid**, separate from the **void-box daemon** (the higher-privilege host runtime that launches and manages the sandboxes). That isolation is what contains the parser surface from Context: it faces attacker-controlled input before any auth gate, so a compromise of the proxy must not be a compromise of the daemon or the host runtime. The only process boundary is therefore daemon↔proxy; sandboxes are kept apart *inside* the shared process by per-sandbox mechanisms (ADR-0004), not by a per-sandbox process boundary. Egress policy and credential injection plug into the same per-connection pipeline.

## Consequences

- **Positive:** memory cost is fixed rather than linear in VM count; a parser-surface compromise is contained to the low-privilege proxy and is not a host-runtime compromise; one pipeline and one lifecycle serve both egress and injection.
- **Negative / cost:** there is no proxy-vs-proxy process isolation, so cross-sandbox separation rests entirely on the per-sandbox mechanisms of ADR-0004; the shared proxy is a single availability dependency for credentialed (and, under restrictive profiles, all) egress.
- **Follow-ups:** revisit a per-sandbox process only if a tenant-isolation requirement forces it. Building fresh in Rust (rather than forking Smokescreen or mitmproxy) was chosen so the egress proxy and credential injector keep one pipeline, lifecycle, and snapshot/restore story; the conventions of those tools are adopted without their runtime.
