# Design: host-mediated credential injection for the guest agent

Status: proposal.

Provider integration reflects Claude Code 2.1.170 and the codex version pinned in
`scripts/agents/manifest.toml`; the behaviors it depends on must be re-verified on
version bumps (see Risk register).

## North star

void-box is a secure runtime for AI agents that handle sensitive data and reach
sensitive downstream services. A prompt-injected or compromised agent is the
expected adversary, so the runtime lets an agent **use** credentials without
**holding** them.

The governing invariant:

> Durable secrets live on the host. The guest never holds a credential — the host
> injects it into the agent's outbound request at egress. The guest sees only
> non-secret placeholders.

## Mechanism

A single host-side component provides this for the LLM providers and for arbitrary
downstream services:

> A host-side, TLS-terminating, header-injecting egress proxy, backed by a host
> credential store. The guest's client is pointed at the proxy and trusts a CA
> installed in the guest image; the proxy rewrites the `Authorization` header (and
> any credential-bearing headers) with a host-held secret and forwards to the real
> upstream. The durable secret — and, for OAuth, the refresh token and rotation —
> never leave the host.

### How a request flows

```
guest agent client (claude / codex / git / curl)
   │  points at the proxy: ANTHROPIC_BASE_URL / openai_base_url / HTTPS_PROXY
   │  trusts the proxy CA: NODE_EXTRA_CA_CERTS / CODEX_CA_CERTIFICATE
   │  carries only a PLACEHOLDER token
   ▼ (guest network → SLIRP)
host injection proxy ── TLS-terminate (proxy CA) ── rewrite Authorization +
   │                                                provider headers with the
   │                                                host-held real credential
   ▼ re-encrypt to the real upstream (TLS intact on the external wire)
provider token endpoints / downstream services
   ▲
host credential store: holds OAuth refresh tokens & downstream secrets; refreshes
   against the provider; mints short-lived access tokens; sole rotation owner.
```

Everything the guest holds — placeholder token, CA certificate, base-URL value —
is non-secret. The real credential exists only in the host proxy/credential store.

### Why TLS termination is required, and why it is safe

The `Authorization: Bearer …` header lives inside the TLS stream; rewriting it
requires decrypting the request, so the proxy must terminate TLS. There is no
cryptographic way to modify an encrypted header without termination.

Termination is not a confidentiality regression: the host is the trusted party
and already owns the guest's RAM, filesystem, and network, so it can already read
everything the agent sends. Termination grants no new visibility. The design
minimizes exposure regardless: the proxy streams bodies through without inspecting
or logging them, touching only credential headers; TLS to the upstream is
re-established so the external wire stays encrypted; and the guest trusts only a CA
installed in its own image — a scoped, deliberate trust, not a general
interception capability.

## The risk this addresses

The `claude-personal` and `codex` providers stage host-side OAuth credentials —
including the **refresh** token — into the guest: an RW bind-mount of the
credential file at `/home/sandbox/.claude` or `/home/sandbox/.codex`
(`src/runtime.rs`), or a privileged WriteFile copy into guest tmpfs
(`src/agent_box.rs:464`). A uid-1000 agent can read the file and exfiltrate the
refresh token, yielding account access that outlives the run. Both CLIs self-
refresh in-process and rotate single-use refresh tokens, so host-only ownership is
also the only correct design — two refreshers invalidate each other's token.

## Non-goals

- Preventing a guest process from *using* the proxy to make authenticated calls
  (e.g. spending the operator's LLM quota). The proxy hands out use, not the
  credential; abuse is bounded by the destination allow-list and a per-run proxy
  token, not eliminated.
- Per-process isolation inside the guest, which is a single trust domain.
- Delivering any real secret into the guest (see Alternatives considered).
- Non-HTTP credential schemes and a general operator surface (deferred to M3).

## Attack surface

The proxy is a host component in the hot path of all agent egress, holding tokens
and terminating guest-originated TLS. It is hardened as untrusted-input-facing (it
parses attacker-influenced HTTP with bounded resources) and is authenticated so
only the intended guest can use it: it binds to a guest-only path (loopback behind
SLIRP on Linux; the VZ-specific address on macOS) and requires a per-run proxy
token injected into the guest. That token is low-stakes — it permits use of
allow-listed destinations, not credential extraction — and is analogous to the
vsock session secret. If the proxy or credential store is unavailable, egress
fails rather than bypassing injection (fail-closed).

## Milestones

One mechanism, sequenced from the provider risk outward.

### M1 — injection proxy + credential store, applied to the LLM providers

Build the host proxy and credential store and route both `claude-personal` and
`codex` through them. Remove both providers' RW credential mounts and the
`src/agent_box.rs:464` WriteFile copy.

Shared host components:
- **Credential store**: holds each provider's OAuth refresh token, refreshes
  against the provider, mints short-lived access tokens, and is the sole rotation
  owner (serialized refresh, rate-capped independently of request volume).
- **Injection proxy**: terminates TLS with a cert chaining to the guest-installed
  CA, rewrites the credential header(s), and forwards to the real upstream.

**Claude (`claude-personal`).** Guest configuration: `ANTHROPIC_BASE_URL=<proxy>`,
`NODE_EXTRA_CA_CERTS=<proxy CA PEM>`, `CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST=1`, a
placeholder `ANTHROPIC_AUTH_TOKEN`, and no credentials file. Only `/v1/messages`
(inference) is a blocking call and it honors `ANTHROPIC_BASE_URL`; a missing
credentials file resolves to null rather than an error; `PROVIDER_MANAGED_BY_HOST`
suppresses the hardcoded OAuth-refresh recovery and force-login; the placeholder
Bearer reaches the proxy, which injects the real one. Claude Code applies
`NODE_EXTRA_CA_CERTS` additively and does not pin certificates.

**codex (`codex`).** Guest configuration in `$CODEX_HOME/config.toml`:
`openai_base_url=<proxy>` (overrides the chatgpt-mode inference base) and
`[auth] credentials_store="file"`; env `CODEX_CA_CERTIFICATE=<proxy CA PEM>`; and a
placeholder `auth.json` — `tokens.id_token` a structurally valid 3-part JWT with a
minimal payload, `tokens.access_token` an arbitrary non-JWT placeholder,
`tokens.refresh_token` a placeholder, `tokens.account_id` the real account id, and
`last_refresh` recent — so codex loads, sends the placeholder Bearer, and does not
proactively refresh. The proxy rewrites `Authorization` to the real Bearer, injects
the real `ChatGPT-Account-ID`, and passes through `originator: codex_cli_rs`. codex
trusts `CODEX_CA_CERTIFICATE`/`SSL_CERT_FILE` additively (rustls) and does not pin
certificates.

The clients reach the proxy over the guest network (SLIRP); the proxy binds
guest-only per platform and requires the per-run proxy token.

Gate: `e2e_agent_mcp` (Claude) and the codex smoke specs authenticate; tests assert
the guest holds only placeholders and no credentials file.

### M2 — credential-less downstream HTTP egress

Apply the same proxy to allow-listed downstream services (e.g. GitHub API, internal
APIs): the host injects the destination's credential and the secret never enters
the guest. Two interception forms reuse M1's proxy — explicit, for tools that honor
`HTTPS_PROXY` (curl, git, most HTTP libraries; the common case); and transparent,
network-level interception (`src/network/slirp.rs`) with per-destination certs, for
tools that ignore `HTTPS_PROXY`. Adds operator-declared per-destination credential
and injection policy; these credentials are typically static (no OAuth refresh).

### M3 — broader downstream and operator surface

Non-HTTP mediation (database/mTLS via a host-mediated tunnel), an operator-facing
surface to register downstream credentials and policy, and per-consumer scoping if
intra-guest compartmentalization comes into scope.

## Alternatives considered

**Token in the guest (mode 1).** The guest receives a short-lived access token and
a way to refresh it, rather than routing through a proxy — for codex via a loopback
endpoint behind `CODEX_REFRESH_TOKEN_URL_OVERRIDE` that proxies the OAuth refresh to
the host, and for Claude via a host-minted token through the
`CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR` injector. This is not used: it leaves a
real short-lived token at rest in the guest (readable by uid 1000, captured in
snapshots for its lifetime), it requires a guest-initiated fetch channel (a vsock
RPC with its own multiplex inversion and a guest-side helper), and Claude offers no
clean mid-run refresh hook for a spawned CLI. The proxy removes the in-guest token
and that machinery at once. Mode 1 remains a fallback only for a hypothetical tool
that can be pointed at neither a proxy nor a custom CA; no supported agent is such a
tool.

**Guest-side decryption / no host TLS termination.** Terminating TLS in the guest
to inject the header would keep the proxy off the host but reintroduces a real token
in the guest. Operating at L4 (TCP pass-through) cannot modify HTTP headers. mTLS
client-cert injection would avoid decryption but applies only to upstreams that
authenticate by client certificate, which the supported providers do not.

## Security properties

- **No credential in the guest** — only non-secret placeholders, a CA, and a
  base-URL value; the real access token and OAuth refresh token live only on the
  host.
- **Nothing real in snapshots, `/proc`, or crash dumps** — there is no secret in
  guest memory to capture.
- **Single rotation owner** — only the host refreshes, eliminating the
  rotation-conflict failure mode.
- **Fail-closed** — proxy or credential store unavailable ⇒ egress fails; never an
  un-injected call.
- **Bounded abuse, not credential theft** — a guest process can use the proxy for
  allow-listed destinations but cannot extract the secret; bounded by the
  allow-list and the per-run proxy token.
- **External wire stays encrypted** — TLS is re-established to the upstream; only a
  host-internal hop is plaintext, inside the already-trusted host.
- **Redaction and zeroization** for host-held secrets via `secrecy`; opt-in, with no
  behavior change for providers that do not stage OAuth.

### Residual risks

- The host decrypts inference traffic at the proxy — inherent to header injection
  and benign, since the host already sees the guest's data; the proxy streams
  bodies without inspecting them.
- The proxy is a hot-path dependency — a defect or outage stops all egress
  (fail-closed is the correct posture, but it is an availability coupling).
- The per-run proxy token lives in the guest — low-stakes (use, not theft), needed
  so a LAN or other-guest process cannot drive the proxy.
- codex's placeholder `auth.json` still contains a real `id_token`/`account_id` —
  identity material, not a bearer or refresh token.

## Risk register

Ordered by how much each could disrupt implementation.

| # | Risk | Likelihood | Impact | Mitigation / fallback |
|---|------|-----------|--------|-----------------------|
| R1 | **Server-side acceptance of host-reproduced auth** — the provider's token endpoint accepting the host-side refresh, and the inference endpoint accepting the host-minted token injected as a plain Bearer (the subscription-scope client path is not active for a proxied placeholder). Cannot be confirmed by static analysis; needs a live account. | Medium (some header reproduction may be required) | High (could force rework of provider integration) | Validate first (V1). The injected token is a genuine subscription token, and the `CLAUDE_CODE_OAUTH_TOKEN` env path demonstrates the inference endpoint accepts an externally-supplied subscription Bearer. The headers the subscription/ChatGPT clients send are known and can be reproduced at the proxy. |
| R2 | **Proxy streaming/lifecycle correctness** — TLS termination plus SSE (Claude) and WebSocket (codex) streaming, backpressure, connection reuse, timeouts, partial failures, fail-closed. | Medium (bugs likely) | Medium–High (degrades all egress) | Standard reverse-proxy patterns; dedicated streaming tests; primary engineering budget. |
| R3 | **codex WebSocket transport** — codex defaults to Responses-over-WebSocket; a header-rewriting proxy must inject on the (plaintext) WS upgrade handshake and pipe frames. | Low | Low–Medium (codex only) | Two fallbacks: inject on the upgrade then pipe frames, or force plain HTTPS via a custom provider with `supports_websockets=false`. Version pinning controls change. |
| R4 | **macOS/VZ proxy reachability** — binding the proxy guest-only without LAN exposure. | Low–Medium | Medium (capped) | Per-run proxy token caps the downside (use only, no credential theft); bind to the VZ-specific address. |
| R5 | **Provider version drift** — a bundled Claude/codex version changing the redirect, refresh, header, or transport behavior the design relies on. | Low | Low–Medium | Pin versions; re-verify R1/R3 facts on bump (the agent-bump workflow gates version changes). |
| R6 | **M2 transparent cert generation** — on-the-fly per-destination leaf certs under the proxy CA. | Low | Low (M2 only) | Deferred; explicit `HTTPS_PROXY` avoids it; established SSL-bump pattern. |

## Implementation plan and validation order

Validation is front-loaded so the highest-impact unknown (R1) is resolved before
the surrounding machinery is built. Each validation step is a gate for the work
that follows.

**V1 — auth smoke test (gate for everything).** A minimal TLS-terminating proxy
with a hardcoded real token: perform one host-side OAuth refresh and inject the
minted token as a Bearer on one inference request, for both Claude and codex,
against a live account. Confirm `200` and a usable completion. Resolves R1. If extra
headers are required, capture them here.

**V2 — CA trust.** Confirm each client trusts the proxy CA via its env mechanism
(`NODE_EXTRA_CA_CERTS`; `CODEX_CA_CERTIFICATE`/`SSL_CERT_FILE`, PEM file — neither
honors `SSL_CERT_DIR`).

**V3 — streaming.** Drive a full streamed response through the proxy: SSE for
Claude, and codex over its default transport (resolving R3: inject-on-upgrade or
forced HTTPS).

Then build M1 against these gates:

1. **Credential store** — per-provider OAuth refresh, short-lived-token minting,
   serialized rotation, rate-capping. (`src/credentials.rs` becomes host-retained;
   `reqwest` + rustls are already vendored.)
2. **Injection proxy** — TLS termination, header rewrite, streaming, fail-closed.
   Validated by V2/V3.
3. **Guest provisioning** — install the proxy CA into the guest image; set the
   base-URL/config, placeholder tokens / `auth.json`, and `PROVIDER_MANAGED_BY_HOST`;
   delete the RW mount and the `src/agent_box.rs:464` WriteFile copy.
   (`src/runtime.rs` `~234`, `~1352-1408`; `src/llm.rs` provider wiring.)
4. **Reachability and scoping** — bind the proxy guest-only per platform and require
   the per-run token; verify on both KVM and VZ. Resolves R4.
5. **Long-run refresh/rotation** — token expiry handled host-side with no guest
   involvement across a service-mode run.

M1 exit gate: `e2e_agent_mcp` (Claude) and the codex smoke specs pass with the guest
holding only placeholders.

M2 and M3 follow once the M1 substrate exists; M2 adds the per-destination policy
and the explicit/transparent interception forms (R6 applies only to the transparent
form).

## Affected code

- New host modules — the injection proxy (TLS-terminating, header-rewriting,
  streaming) and the credential store (OAuth refresh/mint/rotation).
- `src/credentials.rs` — host-retained OAuth credential, refresh/rotation, and
  short-lived-token minting; staging is host-only.
- `src/runtime.rs` (`~234`, `~1352-1408`), `src/agent_box.rs` (`~464`) — remove the
  RW mount and the privileged WriteFile copy; provision the guest with the proxy
  base-URL, CA PEM, placeholder tokens / `auth.json`, and `PROVIDER_MANAGED_BY_HOST`.
- `src/llm.rs` — provider → proxy/credential wiring.
- Guest image — install the proxy CA into the trust stores the clients honor.
- `src/network/slirp.rs` — host-side interception point for M2's transparent form.
- `scripts/agents/manifest.toml` — the pinned codex version against which the
  redirect and transport behavior are verified.
