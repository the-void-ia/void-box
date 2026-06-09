# Design: host-mediated credential broker for the guest agent

Status: **proposal** (no code yet). Owner: runtime. Target branch:
`claude/vm-agent-credentials-syz9c2`.

The provider mechanisms below were verified by reading the actual shipped code —
the Claude Code CLI bundle (`@anthropic-ai/claude-code` 2.1.170) and the
open-source codex Rust source — not just the docs. Where a claim still needs a
runtime check against a live account, it is called out as a spike.

## North star

void-box is a secure runtime for AI agents that handle sensitive data and reach
sensitive downstream services. A prompt-injected or compromised agent is the
expected adversary, so the runtime's job is to let an agent **use** credentials
without **holding** them.

The governing invariant:

> Durable secrets live on the host. The guest never holds a durable credential —
> only **time-bounded, host-mediated use**: either a short-lived minted token, or
> host-side injection at egress so the secret never enters the guest at all.

The highest-priority instance is the LLM provider's own OAuth tokens (today we
mount the **refresh** token into the guest). The natural generalization is the
agent's access to *any* sensitive downstream service. Both are the same broker
applied at different points. We build it **incrementally by milestone**, each
standalone-valuable and gated on a real consumer — not a framework built ahead of
need.

## Two mediation modes

| Mode | In-guest exposure | Provider fit | Mechanism |
|---|---|---|---|
| **1 — minted short-lived token** | brief, time-bounded (≤ provider TTL) | **codex** (clean refresh-URL override); Claude *task-mode* slice | host mints/refreshes; guest fetches/holds a short-lived token |
| **2 — host-side injection at egress** | **none** | **Claude** (no refresh override → inference proxy); all downstream services | host injects the credential into the agent's outbound request; secret never enters the guest |

A key finding from reading the source: **the two LLM providers want opposite
modes.** codex exposes a clean refresh-endpoint override, so it fits mode 1.
Claude exposes *no* refresh-URL override and its spawned CLI can't be handed a
refreshed token mid-run, so its robust path is mode 2 — which also happens to be
the reusable substrate for credential-less downstream access.

## The finding that anchors the provider milestones

Today (`src/credentials.rs`, `src/runtime.rs`, `src/agent_box.rs`) the
`claude-personal` and `codex` providers **RW-bind-mount the full OAuth
credential — including the refresh token — into the guest** (or, for service-mode
claude-personal, copy it in via a privileged WriteFile RPC,
`src/agent_box.rs:464`). A uid-1000 agent can `cat` it and exfiltrate the
**refresh** token → account takeover lasting well beyond the run.

Two facts shape the fix:

- **Both CLIs self-refresh in-process** and write rotated tokens back; the RW
  mount exists *specifically* so they can.
- **Both rotate refresh tokens (single-use)** — two refreshers invalidate each
  other ("refresh token already used", confirmed in upstream codex/claude-code
  issues). So there can be exactly **one** refresh owner; host-only ownership is
  the only *correct* design and fixes a latent repeated-run bug in today's
  discard-the-ephemeral-mount behavior.

## Non-goals

- Preventing use of a leaked *short-lived* token during its validity window
  (mode 1). The bound is temporal (≤ provider default), not preventive.
- Per-process isolation inside the guest (one trust domain). The win is "durable
  secret absent" + "host-mediated", not intra-guest compartmentalization.
- Any path that *delivers* a downstream secret to the agent as plaintext
  env/file — that contradicts the invariant. Sources/mechanisms beyond what a
  live milestone needs are out of scope until that milestone exists.

## Why on-demand, and the new surface it creates

At-rest delivery leaks the credential to every guest process, the mounted file,
**any snapshot** taken during the run (guest RAM is dumped wholesale to disk,
`src/vmm/snapshot.rs:187`), and crash dumps. Host-mediation removes the durable
secret and time-bounds (mode 1) or eliminates (mode 2) in-guest exposure.

The cost is a **new host-side attack surface**: the broker makes the host
*respond to guest-initiated requests*. Until now guest→host traffic is only
telemetry, and even that is host-initiated (the host owns the `request_id`).
**There is no precedent for the guest originating a request.** The broker's
handler is therefore designed untrusted-input-facing from the start (bounded
fields, allow-list check before any work, DoS budget — below), on an isolated,
independently-fuzzable listener rather than grafted onto the control channel.

## Architecture

```
   provider token endpoints / downstream services
            ▲ HTTPS (durable secret — host only)
┌───────────┴──────────────── host ────────────────────────┐
│  CredentialBroker                                         │
│   - holds durable secrets per run; sole refresh owner     │
│   - mode 1: mint/refresh short-lived tokens               │
│   - mode 2: inject creds into mediated egress             │
│   - allow-list · rate/DoS budget · audit                  │
└───────────┬──────────────────────────────────────────────┘
            │ dedicated CID-scoped vsock port (guest→host),
            │ authenticated by a fresh session-secret handshake
┌───────────▼──────────────── guest (uid 1000) ────────────┐
│  void-cred                                                │
│   - codex loopback refresh shim (127.0.0.1 → vsock)       │
│   - token fetch / FD-inject helper                        │
│   - (M2+) local relay for the host inference/egress proxy │
└───────────────────────────────────────────────────────────┘
```

`reqwest` + rustls is already a workspace dependency, so host-side HTTPS
refresh/injection is cheap to add.

### Transport (shared substrate, the dominant cost; lands in M1)

The guest must initiate requests to the host. Decision:

- **Chosen: a dedicated, CID-scoped vsock port** the host listens on and the
  guest connects out to, authenticated by a **fresh** session-secret handshake
  (reusing `connect_with_handshake_sync` semantics). A new listener does **not**
  inherit the control channel's authenticated state, so we don't claim it does.
  It must be **vsock (CID-scoped), never a TCP port on the SLIRP gateway**, or it
  inherits the sidecar's macOS `UNSPECIFIED`-bind LAN exposure.
- **Rejected: reverse-RPC on the existing multiplex channel** — that stack
  assumes *host allocates `request_id`, guest responds*; an unsolicited guest
  frame is dropped (`src/backend/multiplex.rs`). Supporting guest-as-caller means
  duplicating the machinery in reverse.

Platform note: the guest-outbound connection exercises the **listen** side of the
VZ connector (GCD-callback based) — real macOS/VZ work, not a checkbox.

### Wire protocol

Append-only:

```text
MessageType::CredentialRequest  = 28   // { name: String, nonce: [u8;16] }
MessageType::CredentialResponse = 29   // { status: ok|denied|unavailable|expired,
                                       //   secret?: bytes, expires_at?: u64, error?: String }
```

`MAX_MESSAGE_SIZE` bounds payloads; the handler bounds `name` and checks the
allow-list before any work. `unavailable`/`denied` are terminal — the consumer
**fails closed**, never falling back to a mount. The `nonce` is reserved now
(append-only makes it costly to add later) for future per-consumer capability
binding.

### Host-side mint / refresh

- The broker holds durable secrets per run (`SecretString`) and is the **only**
  refresher. Upstream refreshes are **serialized** and rate-capped *independently*
  of guest-facing requests (refresh at most once per ~TTL/2, else serve cached) —
  respecting single-use rotation and blunting a guest that spams to burn the
  provider rate limit or force rotation.
- **Expiry ceiling** is enforced on the broker-*promised* `expires_at` (a JWT's
  `exp` can't be rewritten): never beyond `now + CEIL`, `CEIL ≤` provider
  default. The guest re-requests before it.

### File-content allow-list

Any guest-visible credential file declares a **positive `key_allowlist`**; the
shim writes only those keys (no "reject keys containing refresh" denylist —
bypassable). A refresh-token-shaped key cannot appear.

## Milestones

Sequenced low-risk-first. Later milestones are sketched at lower fidelity
deliberately; they firm up with their consumer.

### M1 — codex provider OAuth containment (mode 1) + the broker/transport foundation

codex fits mode 1 cleanly. Verified from `codex-rs` source: the access token is
sent as `Authorization: Bearer`; refresh POSTs `{client_id, grant_type:
"refresh_token", refresh_token}` to `REFRESH_TOKEN_URL`, overridable via the
official **`CODEX_REFRESH_TOKEN_URL_OVERRIDE`**. `auth_mode: "chatgpt"` **is** the
personal ChatGPT-subscription flow, so this holds for personal subscriptions by
construction.

Design — loopback refresh shim:

- Seed `auth.json` with a host-minted access token + an **opaque sentinel**
  refresh token; set `CODEX_REFRESH_TOKEN_URL_OVERRIDE` to `void-cred`'s
  `127.0.0.1` endpoint.
- On refresh, `void-cred` forwards the `{client_id, grant_type, refresh_token}`
  POST over vsock to the broker, which uses the **real** host-held refresh token
  to refresh against `auth.openai.com` and returns a normal token response. Real
  refresh token stays host-side; HTTP stays loopback-only.

This milestone also delivers the **shared foundation**: the broker, the CID-scoped
vsock listener + handshake, the `CredentialRequest/Response` wire types, and
`void-cred`. Deletions: codex RW mount.

Residual (accepted): the short-lived *access* token is still briefly in-guest
(in `auth.json` + the codex process) — mode-1, time-bounded by rotation; only the
durable refresh token is contained. The `auth.json` is also snapshot-captured for
its lifetime (stated, not glossed; acceptable because ≤ CEIL and
refresh-token-free).

Spike: confirm codex tolerates an opaque sentinel `refresh_token` value and the
exact JSON field set in the override response.

### M2 — Claude provider OAuth containment (mode 2 primary; mode-1 task slice)

Reading the 2.1.170 bundle settles the mechanism and **rules out apiKeyHelper**:
the Anthropic SDK maps `apiKey → X-Api-Key`, `authToken → Authorization: Bearer`,
and stored-OAuth → `Bearer` only when `apiKey==null`. Configuring `apiKeyHelper`
forces **API-key mode**, routes its output to **`x-api-key`**, and *explicitly
disables subscription login* (the code warns "apiKeyHelper overriding Claude
subscription login"). So apiKeyHelper cannot carry a subscription OAuth token.

Two further bundle facts shape the design:

- **No refresh-URL override exists** for Claude; its OAuth refresh hits a
  *hardcoded* console host, not the inference base URL. A codex-style refresh shim
  is therefore not available.
- The clean host-managed-auth **refresh callback** (`getHostAuthToken`, gated by
  `CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST=1` / `CLAUDE_CODE_HOST_AUTH_ENV_VAR`) is an
  **SDK-embed hook**; void-box spawns the `claude` *binary*, so the spawned CLI
  can *read* an injected token but cannot be handed a refreshed one mid-run via
  that callback.

**Primary mechanism — host inference proxy (mode 2).** `ANTHROPIC_BASE_URL` keeps
`firstParty` mode and forwards inference requests (incl. the Bearer) to a proxy;
a subscription session honors the override too. The host proxy injects the real
subscription `Bearer` (refreshing host-side) and forwards to `api.anthropic.com`;
the guest carries only a **placeholder** token so the SDK's `validateHeaders`
passes (a zero-credential proxy throws "Could not resolve authentication
method"). With `PROVIDER_MANAGED_BY_HOST=1` Claude won't attempt its own refresh
against the hardcoded host, and since the server sees a valid token it never
401s. Result: **zero real token in the guest** — true mode 2. This is the same
egress-injection substrate M3 generalizes.

**Cheaper task-mode slice (mode 1, transport-free).** For task-mode runs shorter
than the access-token TTL (~24 h for Claude), inject a host-minted short-lived
subscription access token via the **`CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR`**
injector (OAuth-Bearer tier; passed through an inherited fd, so not in
`/proc/environ` or on disk) at launch — no transport, no refresh token, no
credentials file. Covers the common case; long/service runs use the proxy.

Deletions: claude-personal RW mount + the `agent_box.rs:464` WriteFile copy.
**Avoid `CLAUDE_CODE_OAUTH_TOKEN`** (a 1-year token — exfiltration nearly as bad
as a refresh token).

Spikes: (a) confirm the inference endpoint accepts a host-minted short-lived
subscription access token presented as a plain Bearer (via the proxy and via the
FD injector), since the subscription-scope client path (`J`) isn't active when the
token doesn't come from the stored credential; (b) the proxy's TLS termination —
a guest-trusted host CA (host-side MITM, acceptable since the host owns the
guest) vs an explicit forward-proxy contract.

### M3 — credential-less downstream egress (mode 2, generalizes M2's proxy)

Generalize the M2 host inference proxy to **any** allow-listed downstream service
(GitHub API, internal services): the host injects the destination's credential
into the agent's outbound request; the secret never enters the guest. void-box's
host-side SLIRP stack (`src/network/slirp.rs`) is the natural injection point.
Inherits M2's TLS-termination decision and adds per-destination
credential/injection policy declared by the operator.

### M4 — broader downstream + operator surface

Non-HTTP downstream (database/mTLS via host-mediated tunnel; `git` push via a
short-lived-token helper), an operator-facing surface to register downstream
credentials and mediation policy, and per-consumer **capability tokens** (the
reserved `nonce`) for intra-guest scoping. Built only as consumers land.

## Anti-over-engineering guardrails

- **No mechanism without a live consumer.** M3/M4 are directional; no source/
  delivery enum or external-manager (Vault/1Password) plugins pre-built.
- **No plaintext delivery of downstream secrets to the agent.** Mode 2 is
  injection/mediation; there is no "write the DB password into the guest" path.
- **One substrate, many hooks.** Broker, transport, and wire format are shared;
  each milestone adds a provider/destination hook, not a new framework.

## Security properties

- **No durable secret in the guest** — refresh tokens / downstream creds held and
  rotated only on the host. Enforced by host-only refresh + positive
  `key_allowlist` (mode 1) and by never delivering the secret at all (mode 2).
- **Time-bounded blast radius** (mode 1) — a leaked minted token dies at
  `expires_at`. **Zero blast radius** (mode 2).
- **Fail closed** — broker unavailable/denied ⇒ the run errors; never a silent
  mount fallback.
- **Single rotation owner** — only the host refreshes; eliminates the
  "refresh token already used" conflict and the latent repeated-run bug.
- **Host treats guest input as hostile** — bounded `name`, allow-list before work,
  DoS budget (max concurrent requests per channel, bounded handler memory,
  serialized + rate-capped upstream refresh), isolated fuzzable listener.
- **Loopback-only codex HTTP**; **redaction + zeroize** via `secrecy`; **opt-in**.

### Honest residual risks

- Live minted-token use during its window (mode 1) — out of scope by design.
- **codex `auth.json` access token is snapshot-captured for its lifetime** — mode 1
  carries a brief in-guest token; acceptable because ≤ CEIL and refresh-token-free.
  Claude mode-2 carries no real token at all.
- Intra-guest sharing — any guest process can request an allow-listed name; the
  `nonce` reserves room for capability tokens.
- Rate-limit self-DoS — the legitimate path *delays* rather than *denies*.

## Coverage of the security-review findings

| Review criterion | Where addressed |
|---|---|
| Refresh tokens never enter the guest | M1 codex shim / M2 Claude proxy; host-only refresh |
| Host performs refresh; returns access token + expiry | broker; `CredentialResponse.expires_at` |
| ≤ provider-default ceiling; re-request before expiry | expiry ceiling |
| Fail closed, no silent mount fallback | wire `denied`/`unavailable` terminal |
| Remove RW mount **and** privileged WriteFile copy | M1/M2 deletions |
| New vsock RPC; auth via fresh handshake | Transport; wire protocol |
| Rate limits / DoS budget (guest vs upstream decoupled) | Host-side refresh |
| Per-provider allowed-key list for guest files | positive `key_allowlist` |
| Don't use the unauthenticated sidecar; CID-scoped only | Transport |
| Codex self-refresh contract | M1 — confirmed from `codex-rs` source |
| Claude apiKeyHelper viability | M2 — ruled out from bundle; mode-2 proxy instead |
| New host-side attack surface treated as design | "Why on-demand" + DoS budget |
| Over-engineering / YAGNI | guardrails; milestone-gated |

## Open questions

1. **Claude subscription Bearer acceptance (M2 spike):** does the inference
   endpoint accept a host-minted short-lived subscription access token presented
   as a plain Bearer (via the proxy and the FD injector), given the
   subscription-scope client path isn't active for non-stored tokens?
2. **M2/M3 TLS termination:** guest-trusted host CA (host-side MITM) vs explicit
   forward-proxy contract for credential injection.
3. **codex override response shape (M1 spike):** exact JSON fields + tolerance of
   an opaque sentinel `refresh_token`, at the pinned version.
4. **macOS/VZ listen-side** validation for the guest-outbound vsock connection.

## Affected code (for the implementation plan)

- `void-box-protocol/src/lib.rs` — `MessageType` 28/29 + types (`nonce`,
  `expires_at`).
- `src/backend/` — dedicated vsock listener + handshake for guest-initiated
  requests.
- `guest-agent/` + new `void-cred/` crate (uid 1000) — broker client, codex
  loopback shim, FD-inject helper, (M2+) inference/egress relay;
  `DEFAULT_COMMAND_ALLOWLIST`.
- `src/credentials.rs` — host-retained durable secret + refresh logic (rotation,
  expiry); staging shrinks to host-only.
- `src/runtime.rs` (`~234`, `~1352-1408`), `src/agent_box.rs` (`~464`) — delete RW
  mount + privileged WriteFile copy; wire the broker; (M2) configure the
  inference proxy / FD injector + `PROVIDER_MANAGED_BY_HOST`.
- `src/llm.rs` — provider → required-credential mapping (data, replacing the
  hardcoded OAuth-provider `env_vars()` arms).
- `src/network/slirp.rs` — (M2/M3) host-side injection point.
