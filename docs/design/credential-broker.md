# Design: host-mediated credential injection for the guest agent

Status: **proposal** (no code yet). Owner: runtime. Target branch:
`claude/vm-agent-credentials-syz9c2`.

Provider behavior here is verified against shipped code — the Claude Code CLI
bundle (`@anthropic-ai/claude-code` 2.1.170) and the open-source codex Rust
source — not just docs. Items needing a runtime check against a live account or
the bundled version are flagged as spikes.

## North star

void-box is a secure runtime for AI agents that handle sensitive data and reach
sensitive downstream services. A prompt-injected or compromised agent is the
expected adversary, so the runtime's job is to let an agent **use** credentials
without **holding** them.

The governing invariant:

> Durable secrets live on the host. The guest never holds a credential at all —
> the host injects it into the agent's outbound request at egress. The guest
> sees only non-secret placeholders.

## One mechanism

A single host-side component closes this for the LLM providers today and for
arbitrary downstream services tomorrow:

> **A host-side, TLS-terminating, header-injecting egress proxy**, backed by a
> host credential store. The guest's client is pointed at the proxy and trusts a
> CA we install; the proxy rewrites the `Authorization` header (and any
> credential-bearing headers) with a host-held secret and forwards to the real
> upstream. The durable secret — and, for OAuth, the refresh token and rotation —
> never leave the host.

This replaces the earlier two-mode design (a guest-side token-fetch broker over
vsock for some providers, a proxy for others). One mechanism covers every case;
the guest-side fetch path is **ruled out** (see Alternatives).

### How a request flows

```
guest agent client (claude / codex / git / curl)
   │  points at the proxy: ANTHROPIC_BASE_URL / openai_base_url / HTTPS_PROXY
   │  trusts our CA: NODE_EXTRA_CA_CERTS / CODEX_CA_CERTIFICATE
   │  carries only a PLACEHOLDER token
   ▼ (guest network → SLIRP)
host injection proxy ── TLS-terminate (our CA) ── rewrite Authorization +
   │                                              provider headers with the
   │                                              host-held real credential
   ▼ re-encrypt to the real upstream (TLS intact on the external wire)
provider token endpoints / downstream services
   ▲
host credential store / broker: holds OAuth refresh tokens & downstream secrets;
   refreshes against the provider; mints access tokens; sole rotation owner.
```

Everything the guest holds (placeholder token, CA cert, base-URL value) is
**non-secret**. The real credential exists only in the host proxy/broker.

### Why TLS termination — and why that's fine

The `Authorization: Bearer …` header lives inside the TLS stream; to rewrite it
the proxy must decrypt the request — there is no cryptographic way to modify an
encrypted header without terminating TLS. This is **not** a confidentiality
regression: the host is the trusted party and already owns the guest's RAM,
filesystem, and network, so it can already read everything the agent sends.
Termination grants no new visibility. We minimize anyway: the proxy streams
bodies through **without inspecting/logging**, only touching credential headers;
TLS to the upstream is re-established so the external wire stays encrypted; and
the guest trusts only a **CA we install in the guest image** — a scoped,
deliberate trust, not a general MITM. The only alternative that avoids host
decryption is to put a token in the guest (mode 1), which is a strictly worse
trade for the "nothing in the guest" goal — hence ruled out.

## The finding this closes

Today (`src/credentials.rs`, `src/runtime.rs`, `src/agent_box.rs`) the
`claude-personal` and `codex` providers **RW-bind-mount the full OAuth
credential — including the refresh token — into the guest** (or copy it in via a
privileged WriteFile RPC, `src/agent_box.rs:464`). A uid-1000 agent can `cat` it
and exfiltrate the **refresh** token → account takeover beyond the run. Both CLIs
self-refresh in-process and rotate single-use refresh tokens, so host-only
ownership is also the only *correct* design (two refreshers invalidate each
other) and fixes a latent repeated-run bug.

## Non-goals

- Preventing a guest process from *using* the proxy to make authenticated calls
  (e.g. burning the operator's LLM quota). The proxy hands out *use*, not the
  credential; abuse is bounded by the destination allow-list + a per-run proxy
  token, not eliminated.
- Per-process isolation inside the guest (one trust domain).
- Any path that delivers a real secret into the guest. (That's mode 1 —
  ruled out.)
- Non-HTTP credential schemes and a general operator surface — deferred (M3).

## New attack surface this introduces

The proxy is a host component **in the hot path of all agent egress**, holding
tokens and terminating guest-originated TLS. It must be hardened as
untrusted-input-facing (it parses attacker-influenced HTTP) and **authenticated**
so only the intended guest can use it: bind to the guest-only path (loopback
behind SLIRP on Linux; the VZ-specific address on macOS — the sidecar's
`UNSPECIFIED` bind is the anti-pattern to avoid) and require a **per-run proxy
token** injected into the guest. That token is low-stakes (it permits *use* of
allow-listed destinations, not credential theft) — analogous to the session
secret. Fail-closed: if the proxy/broker is down, egress fails rather than
bypassing injection.

Note what this design *removes* versus a guest-fetch broker: no guest-initiated
vsock RPC, no `request_id` multiplex inversion, no new wire protocol, no
privileged guest write (placeholders aren't secret). The remaining new surface is
the proxy itself.

## Milestones

One mechanism, sequenced from the security finding outward.

### M1 — the injection proxy + broker, applied to the LLM providers

Build the host proxy + credential store and contain **both** `claude-personal`
and `codex` through it. Deletions: both providers' RW credential mounts and the
`agent_box.rs:464` WriteFile copy.

**Shared host components:**
- Credential store/broker: holds each provider's OAuth refresh token, refreshes
  against the provider, mints short-lived access tokens, is the sole rotation
  owner (serialized refresh; rate-capped independently of request volume).
- Injection proxy: TLS-terminate (cert chaining to our guest-installed CA),
  rewrite the credential header(s), forward to the real upstream.

**Claude (`claude-personal`)** — verified from the 2.1.170 bundle:
- Guest env: `ANTHROPIC_BASE_URL=<proxy>`, `NODE_EXTRA_CA_CERTS=<our CA PEM>`,
  `CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST=1`, a **placeholder** `ANTHROPIC_AUTH_TOKEN`;
  **no** credentials file present.
- Confirmed: only `/v1/messages` (inference) is blocking and it honors
  `ANTHROPIC_BASE_URL`; the missing credentials file returns null (not an error);
  `PROVIDER_MANAGED_BY_HOST=1` suppresses the hardcoded OAuth-refresh recovery and
  force-login; the placeholder Bearer is sent to the proxy, which injects the real
  one. No cert pinning.

**codex (`codex`)** — verified from `codex-rs`:
- Guest config `$CODEX_HOME/config.toml`: `openai_base_url=<proxy>` (overrides the
  `chatgpt` inference base), `[auth] credentials_store="file"`; env
  `CODEX_CA_CERTIFICATE=<our CA PEM>`; a placeholder `auth.json`
  (`{tokens:{id_token: <valid 3-part JWT, minimal payload>, access_token:
  <placeholder non-JWT>, refresh_token: <placeholder>, account_id: <real>},
  last_refresh: <~now>}`) so codex loads, sends the placeholder Bearer, and does
  **not** proactively refresh.
- Proxy must rewrite `Authorization` → real Bearer, inject the real
  `ChatGPT-Account-ID`, and pass through `originator: codex_cli_rs`. No cert
  pinning.
- **Caveat (spike):** codex defaults to **Responses-over-WebSocket** — the proxy
  must inject on the WS *upgrade* handshake (plain HTTP, injectable) then pipe
  frames, or we force plain HTTPS via a custom provider with
  `supports_websockets=false`. Verify against the codex version pinned in
  `scripts/agents/manifest.toml`.

**Reachability/auth:** the clients reach the proxy over the guest network
(SLIRP); bind guest-only per platform + require the per-run proxy token.

Gate: `e2e_agent_mcp` (Claude) and the codex smoke specs still authenticate; tests
assert the guest holds no real token (only placeholders) and no credentials file.

### M2 — generalize to credential-less downstream HTTP egress

The same proxy, applied to arbitrary allow-listed downstream services (GitHub
API, internal APIs): the host injects the destination's credential; the secret
never enters the guest. Two interception forms, both reusing M1's proxy:
- **explicit** — tools that honor `HTTPS_PROXY` (curl, git, most HTTP libs) point
  at the proxy directly (most cases);
- **transparent** — network-level interception (SLIRP, `src/network/slirp.rs`)
  with on-the-fly per-destination certs, only for tools that ignore `HTTPS_PROXY`.

Adds the per-destination credential/injection policy (operator-declared); the
credentials here are usually static (no OAuth refresh machinery).

### M3 — broader downstream + operator surface

Non-HTTP (database/mTLS via host-mediated tunnel), an operator-facing surface to
register downstream credentials + mediation policy, and per-consumer scoping if
intra-guest compartmentalization is ever in scope.

## Ruled-out alternative: mode 1 (token in the guest)

The earlier design gave the guest a **short-lived token** and a way to refresh it,
rather than proxying. Two shapes were considered and **rejected**:
- **codex refresh-shim** — a guest loopback endpoint behind
  `CODEX_REFRESH_TOKEN_URL_OVERRIDE` proxying the OAuth refresh to a host broker;
  the short-lived access token still lives in `auth.json`.
- **Claude token injection** — a host-minted short-lived access token via the
  `CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR` injector for task-mode, with no clean
  mid-run refresh (Claude exposes no refresh-URL override and the spawned CLI
  can't use the SDK host-auth callback).

Both required a **guest-initiated vsock broker** (new wire protocol +
`request_id` multiplex inversion + a `void-cred` fetch helper + a guest-trusted
listener), and both still left a **real short-lived token at rest in the guest**
(readable by uid 1000, snapshot-captured for its lifetime). Mode 2 removes the
token entirely *and* removes that machinery, so mode 1 is kept only as a
fallback-of-last-resort for a hypothetical tool that can neither be pointed at a
proxy nor accept our CA — none of the supported agents are such a tool.

## Security properties

- **No credential in the guest at all** — only non-secret placeholders + a CA +
  a base-URL value; the real token and the OAuth refresh token live only on the
  host.
- **Nothing real in snapshots / `/proc` / crash dumps** — there is no secret in
  guest memory to capture.
- **Single rotation owner** — only the host refreshes; eliminates the
  "refresh token already used" conflict and the latent repeated-run bug.
- **Fail closed** — proxy/broker down ⇒ egress fails; never an un-injected call.
- **Bounded abuse, not credential theft** — a guest process can *use* the proxy
  for allow-listed destinations; it cannot extract the secret. Limited by the
  destination allow-list + per-run proxy token.
- **External wire stays encrypted** — TLS is re-established to the upstream; only
  a host-internal hop is plaintext, in the already-trusted host.
- **Redaction + zeroize** for host-held secrets via `secrecy`; **opt-in**.

### Honest residual risks

- **Host decrypts inference traffic at the proxy.** Inherent to header injection;
  benign because the host already sees the guest's data (trusted boundary). The
  proxy streams without inspecting bodies.
- **Hot-path dependency.** A proxy defect/outage stops all egress (fail-closed —
  the correct posture, but an availability coupling).
- **Per-run proxy token in the guest.** Low-stakes (use, not theft); needed so a
  LAN/other-guest process can't drive the proxy, especially on macOS/VZ.
- **codex identity material.** The placeholder `auth.json` still contains a real
  `id_token`/`account_id` (identity, not a bearer or refresh token).

## Coverage of the security-review findings

| Review criterion | Where addressed |
|---|---|
| Refresh tokens never enter the guest | host broker; guest holds only placeholders |
| Host performs refresh; short-lived tokens only | broker mints/refreshes; proxy injects |
| Fail closed, no silent mount fallback | proxy injects or fails; no mount |
| Remove RW mount **and** privileged WriteFile copy | M1 deletions |
| Don't use the unauthenticated sidecar; scope to the guest | proxy bind + per-run token |
| Rate limits / DoS budget | serialized + rate-capped host refresh; proxy request bounds |
| Codex / Claude containment confirmed against real code | M1 (bundle 2.1.170 + `codex-rs`) |
| New host-side attack surface treated as design | "New attack surface"; proxy hardening |
| Over-engineering / YAGNI | one mechanism; mode-1 + vsock broker removed |

## Open questions / spikes

1. **codex WebSocket transport** — inject on the WS upgrade + pipe frames, or
   force plain HTTPS via `supports_websockets=false`? Verify at the bundled codex
   version.
2. **Claude subscription Bearer acceptance** — confirm the inference endpoint
   accepts a host-minted short-lived subscription access token presented as a
   plain Bearer through the proxy (the subscription-scope client path isn't active
   for a proxied placeholder).
3. **macOS/VZ proxy reachability** — bind to a guest-only address so the proxy
   isn't LAN-reachable (the sidecar `UNSPECIFIED` lesson).
4. **Per-destination cert generation** (M2 transparent mode) — on-the-fly leaf
   certs under our CA.

## Affected code (for the implementation plan)

- new host modules — the injection proxy (TLS-terminating, header-rewriting) and
  the credential store/refresh broker (`reqwest` + rustls already vendored).
- `src/credentials.rs` — host-retained OAuth credential + refresh/rotation +
  short-lived-token minting; staging shrinks to host-only.
- `src/runtime.rs` (`~234`, `~1352-1408`), `src/agent_box.rs` (`~464`) — delete RW
  mount + privileged WriteFile copy; provision the guest with the proxy base-URL,
  the CA PEM, placeholder tokens / `auth.json`, and `PROVIDER_MANAGED_BY_HOST`.
- `src/llm.rs` — provider → proxy/credential wiring (replacing the hardcoded
  OAuth-provider `env_vars()` arms).
- guest image — install the proxy CA into the trust stores the clients honor
  (`NODE_EXTRA_CA_CERTS` for Claude; `CODEX_CA_CERTIFICATE`/`SSL_CERT_FILE` for
  codex).
- `src/network/slirp.rs` — (M2 transparent mode) host-side interception point.
- `scripts/agents/manifest.toml` — the pinned codex version to re-verify the
  redirect/WS behavior against.
