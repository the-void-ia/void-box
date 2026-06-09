# Design: host-mediated credential broker for the guest agent

Status: **proposal** (no code yet). Owner: runtime. Target branch:
`claude/vm-agent-credentials-syz9c2`.

## North star

void-box is a secure runtime for AI agents that handle sensitive data and reach
sensitive downstream services. A prompt-injected or compromised agent is the
expected adversary, so the runtime's job is to let an agent **use** credentials
without **holding** them.

The governing invariant of this design:

> Durable secrets live on the host. The guest never holds a durable credential —
> only **time-bounded, host-mediated use**: either a short-lived minted token, or
> host-side injection at egress so the secret never enters the guest at all.

The single highest-priority instance is the LLM provider's own OAuth tokens
(today we mount the **refresh** token into the guest — see the finding below).
The natural generalization is the agent's access to *any* sensitive downstream
service. Both are the same broker applied at different points. We build it
**incrementally by milestone**, each milestone standalone-valuable and gated on a
real consumer — not a speculative framework built ahead of need.

## Two mediation modes

| Mode | In-guest exposure | When | Mechanism |
|---|---|---|---|
| **Minted short-lived token** | brief, time-bounded (≤ provider TTL) | consumer must hold a token (claude/codex CLIs, `git`) | host mints/refreshes; guest fetches on demand |
| **Host-side injection at egress** | **none** | the runtime controls the channel to the downstream service | host injects the credential into the agent's outbound request; secret never enters the guest |

Mode 1 is the pragmatic path for tools that read a token from their environment.
Mode 2 is the strongest property and the product differentiator: the agent can
call a credentialed downstream service and never have anything to exfiltrate.

## The finding that anchors milestone 1–2

Today (`src/credentials.rs`, `src/runtime.rs`, `src/agent_box.rs`) the
`claude-personal` and `codex` providers **RW-bind-mount the full OAuth
credential — including the refresh token — into the guest** (or, for
service-mode claude-personal, copy it in via a privileged WriteFile RPC,
`src/agent_box.rs:464`). A uid-1000 agent can `cat` it and exfiltrate the
**refresh** token → account takeover lasting well beyond the run.

Two facts shape the fix:

- **Both CLIs self-refresh in-process** and write rotated tokens back. The RW
  mount exists *specifically* so they can.
- **Both providers rotate refresh tokens (single-use).** Two parties refreshing
  the same token invalidate each other ("refresh token already used" — confirmed
  for codex and for claude-code in upstream issues #9634 / #24317). So there can
  be exactly **one** refresh owner. Host-only ownership is therefore not just
  more secure — it is the only *correct* design, and it fixes a latent
  repeated-run bug in today's discard-the-ephemeral-mount behavior.

## Non-goals

- Preventing use of a leaked *short-lived* token during its validity window
  (mode 1). The bound is temporal (≤ provider default), not preventive.
- Per-process isolation inside the guest. The guest is one trust domain; any
  guest process reaching the broker can request an allow-listed credential. The
  win is "durable secret absent" + "host-mediated", not intra-guest
  compartmentalization (a capability-token extension is reserved, below).
- A speculative multi-source delivery framework, or any path that *delivers* a
  downstream secret to the agent as plaintext env/file — that contradicts the
  invariant. Sources beyond what a live milestone needs are out of scope until
  that milestone exists.

## Why on-demand, and the new surface it creates

At-rest delivery leaks the credential to every guest process, the mounted file,
**any snapshot** taken during the run (guest RAM is dumped wholesale to disk,
`src/vmm/snapshot.rs:187`), and crash dumps. Host-mediation removes the durable
secret and time-bounds (mode 1) or eliminates (mode 2) in-guest exposure.

The cost is a **genuinely new host-side attack surface**: the broker makes the
host *respond to guest-initiated requests*. Until now guest→host traffic is only
telemetry, and even that is host-initiated (the host owns the `request_id`; the
guest streams frames tagged with it). **There is no precedent for the guest
originating a request.** The broker's handler is therefore designed as
untrusted-input-facing from the start (bounded fields, allow-list check before
any work, a concrete DoS budget — below), and the transport is an isolated,
independently-fuzzable listener (below) rather than a graft onto the existing
control channel.

## Architecture

```
   provider token endpoints / downstream services
            ▲ HTTPS (durable secret — host only)
┌───────────┴──────────────── host ────────────────────────┐
│  CredentialBroker                                         │
│   - holds durable secrets (OAuth refresh tokens, API keys,│
│     downstream creds) per run; sole refresh/rotation owner│
│   - mode 1: mint/refresh short-lived tokens               │
│   - mode 2: inject creds into mediated egress             │
│   - allow-list · rate/DoS budget · audit                  │
└───────────┬──────────────────────────────────────────────┘
            │ dedicated CID-scoped vsock port (guest→host),
            │ authenticated by a fresh session-secret handshake
┌───────────▼──────────────── guest (uid 1000) ────────────┐
│  void-cred                                                │
│   - token fetch (stdout helper / env-on-demand)           │
│   - codex loopback refresh shim (127.0.0.1 → vsock)       │
│   - (M3+) local egress proxy → host-injected requests     │
└───────────────────────────────────────────────────────────┘
```

`reqwest` + rustls is already a workspace dependency, so host-side HTTPS
refresh/injection is cheap to add.

### Transport (shared substrate, the dominant cost of M1)

The guest must initiate requests to the host. Decision:

- **Chosen: a dedicated, CID-scoped vsock port** the host listens on and the
  guest connects out to, authenticated by a **fresh** session-secret handshake
  (reusing `connect_with_handshake_sync` semantics). A new listener does **not**
  inherit the existing control channel's authenticated state, so we don't claim
  it does. It must be **vsock (CID-scoped), never a TCP port on the SLIRP
  gateway**, or it inherits the sidecar's macOS `UNSPECIFIED`-bind LAN exposure
  (`src/sidecar/server.rs`, `guest_accessible_bind_addr` in
  `src/backend/mod.rs`).
- **Rejected: reverse-RPC on the existing multiplex channel.** That stack
  assumes *host allocates `request_id`, guest responds*; an unsolicited guest
  frame is dropped (`src/backend/multiplex.rs`). Supporting guest-as-caller means
  duplicating the caller/responder machinery in reverse — larger and riskier.

Platform note: the guest-outbound vsock connection exercises the **listen** side
of the VZ connector (GCD-callback based), a different path from today's
host-initiated connect — real macOS/VZ work, not a checkbox.

**Transport-free initial slice (M1).** For **task-mode** runs shorter than the
access-token TTL (Claude access tokens are ~24 h), the host can mint a fresh
access token and inject it at launch via the *existing* `ExecRequest` env
(`ANTHROPIC_AUTH_TOKEN`) — no guest-initiated transport at all. The vsock broker
is needed only for **mid-run refresh** (long/service runs) and for codex's shim.

### Wire protocol

Append-only, matching the protocol's discipline:

```text
MessageType::CredentialRequest  = 28
  // { name: String, nonce: [u8;16] }   -- nonce reserved for future
  //                                        per-consumer capability binding
MessageType::CredentialResponse = 29
  // { status: ok|denied|unavailable|expired,
  //   secret?: bytes, expires_at?: u64, error?: String }
```

`MAX_MESSAGE_SIZE` bounds payloads; the handler bounds `name` and checks the
allow-list before any source work. `unavailable`/`denied` are terminal — the
consumer **fails closed**, never falling back to a mount.

### Host-side mint / refresh

- The broker holds durable secrets per run, wrapped in `SecretString`, and is the
  **only** refresher. Upstream refreshes are **serialized** and rate-capped
  *independently* of guest-facing requests (refresh at most once per ~TTL/2, else
  serve cached). This respects single-use rotation and blunts a guest that spams
  requests to burn the provider's rate limit or force rotation.
- **Expiry ceiling** is enforced on the broker-*promised* `expires_at` (a signed
  JWT's `exp` can't be rewritten): never beyond `now + CEIL`, `CEIL ≤` provider
  default. The guest shim trusts `expires_at` and re-requests before it.

### File-content allow-list

Any guest-visible credential file declares a **positive `key_allowlist`**; the
shim writes only those keys (no "reject keys containing 'refresh'" denylist —
bypassable). A refresh-token-shaped key therefore cannot appear.

## Milestones

Each milestone is independently valuable and shares the broker + transport
substrate. Later milestones are sketched at lower fidelity deliberately — they
firm up when their consumer is real.

### M1 — Claude provider OAuth containment (mode 1)

Claude Code exposes **`apiKeyHelper`**: a script in `.claude/settings.json` that
Claude Code invokes for a credential, proactively (~every 5 min, tunable via
`CLAUDE_CODE_API_KEY_HELPER_TTL_MS`) and reactively on 401. This is a first-class
broker hook — cleaner than codex's:

- Point `apiKeyHelper` at `void-cred`; on each call it fetches a fresh
  short-lived token from the broker over vsock and prints it. **No credentials
  file, no refresh token, and no access-token *file* in the guest** — so M1 even
  avoids the snapshot-captures-the-file residual.
- The host broker holds the refresh token (host `~/.claude/.credentials.json` /
  Keychain), refreshes against Anthropic, owns rotation.
- Task-mode slice: inject a short-lived token via `ANTHROPIC_AUTH_TOKEN` at
  launch — transport-free.

Deletions: claude-personal RW mount + the `agent_box.rs:464` WriteFile copy.

**Spike (narrow):** confirm that `apiKeyHelper` output (and `ANTHROPIC_AUTH_TOKEN`)
carries a **subscription OAuth access token** — i.e. Claude Code sends it as a
Bearer token, not `x-api-key`, and the inference endpoint accepts a host-minted
short-lived OAuth access token. **Avoid `CLAUDE_CODE_OAUTH_TOKEN`** for
containment: it bypasses the file but is a **1-year** token — exfiltrating it is
nearly as bad as a refresh token. Note there is **no** OAuth-refresh-endpoint
override in Claude Code (upstream feature request #48011); `apiKeyHelper` is the
intended path. Corroboration that the current approach is wrong: upstream
issues #21765 (copied creds don't refresh on remote machines) and #24317
(concurrent-refresh rotation race).

Gate: `e2e_agent_mcp` still authenticates; tests assert no refresh-token key and
≤ ceiling expiry in any guest-visible credential.

### M2 — Codex provider OAuth containment (mode 1, seeds mode 2)

Codex self-refreshes against `https://auth.openai.com/oauth/token`, overridable
via the official **`CODEX_REFRESH_TOKEN_URL_OVERRIDE`**. Use a loopback refresh
shim:

- Seed `auth.json` with a host-minted access token + an **opaque sentinel**
  refresh token; set `CODEX_REFRESH_TOKEN_URL_OVERRIDE` to `void-cred`'s
  `127.0.0.1` endpoint.
- When codex refreshes, `void-cred` forwards the request over vsock to the
  broker, which uses the **real** host-held refresh token to refresh and returns
  a normal token response. Real refresh token stays host-side; HTTP stays
  loopback-only. This local-shim-proxies-to-host shape is the seed of mode 2.

Deletions: codex RW mount. **Spike:** confirm, against the pinned codex version,
the exact request/response shape codex uses with the override
(`access_token`/`id_token`/`refresh_token`/`expires_in`/`account_id`).

Gate: codex smoke specs still authenticate; same no-refresh-token / bounded-
expiry assertions.

### M3 — Credential-less downstream HTTP egress (mode 2)

The first true mode-2 capability: the agent calls an allow-listed downstream
service (e.g. `api.github.com`, an internal API) and the **host injects the
credential** into the request; the secret never enters the guest. void-box
already relays all guest TCP through a host-side SLIRP stack
(`src/network/slirp.rs`), which is the natural injection point.

Direction (firms up with the consumer): a host-side authenticating proxy for
allow-listed destinations. The open design question is TLS: to inject an
`Authorization` header the host must terminate TLS, which means either a
guest-trusted host CA (host-side MITM — acceptable since the host owns the guest)
or an explicit forward-proxy contract. Per-destination credential + injection
policy is declared by the operator (mode-2 entries never name a guest delivery
target, by construction).

### M4 — Broader downstream + operator surface (mode 1 & 2)

Generalize beyond HTTP (database/mTLS via a host-mediated tunnel; `git` push via
a short-lived-token helper), add an operator-facing spec surface to register
downstream credentials and their mediation policy, and add **per-consumer
capability tokens** (the reserved `nonce`) for intra-guest scoping. Built only as
concrete consumers land.

## Anti-over-engineering guardrails

To keep the broad north star from regressing into speculative generality (the
review's valid concern):

- **No mechanism without a live consumer.** M3/M4 are directional until something
  needs them; do not pre-build a source/delivery enum or external-manager
  (Vault/1Password) plugins.
- **No plaintext delivery of downstream secrets to the agent.** Mode 2 means
  injection/mediation; there is no "write the DB password into the guest" path.
- **One substrate, many hooks.** The broker, transport, and wire format are
  shared; each milestone adds a provider/destination hook, not a new framework.

## Security properties

- **No durable secret in the guest** — refresh tokens / API keys / downstream
  creds are held and rotated only on the host. Enforced structurally + by the
  positive `key_allowlist` test (mode 1) and by never delivering the secret at
  all (mode 2).
- **Time-bounded blast radius** (mode 1) — a leaked minted token dies at
  `expires_at ≤ now + CEIL`. **Zero blast radius** (mode 2) — nothing to leak.
- **Fail closed** — broker unavailable/denied ⇒ the run errors; never a silent
  mount fallback.
- **Single rotation owner** — only the host refreshes; eliminates the
  "refresh token already used" conflict and the latent repeated-run bug.
- **Host treats guest input as hostile** — bounded `name`, allow-list before work,
  DoS budget (max concurrent broker requests per channel, bounded handler memory,
  serialized + rate-capped upstream refresh), isolated fuzzable listener.
- **Loopback-only codex HTTP** — the refresh shim binds `127.0.0.1`; no host port
  on the NAT gateway.
- **Redaction + zeroize** via `secrecy`; **opt-in / no behavior change** for
  providers that don't stage OAuth today.

### Honest residual risks

- Live minted-token use during its window (mode 1) — out of scope by design.
- **Access-token *file* (codex) is snapshot-captured for its lifetime.** "Out of
  snapshots" holds for mode 2, env/helper delivery, and M1 — but **not** codex's
  `auth.json`. Acceptable only because it is ≤ CEIL and refresh-token-free;
  stated, not glossed.
- Intra-guest sharing — any guest process can request an allow-listed name; the
  `nonce` reserves room for future capability tokens.
- Rate-limit self-DoS — the legitimate path *delays* rather than *denies*.

## Coverage of the security-review findings

| Review criterion | Where addressed |
|---|---|
| Refresh tokens never enter the guest | M1 apiKeyHelper / M2 shim; host-only refresh |
| Host performs refresh; returns access token + expiry | broker; `CredentialResponse.expires_at` |
| ≤ provider-default ceiling; re-request before expiry | expiry ceiling; shim trusts `expires_at` |
| Fail closed, no silent mount fallback | wire `denied`/`unavailable` terminal |
| Remove RW mount **and** privileged WriteFile copy | M1/M2 deletions |
| New vsock RPC; auth via fresh handshake | Transport; wire protocol |
| Rate limits / DoS budget (guest vs upstream decoupled) | Host-side mint/refresh; Security properties |
| Per-provider allowed-key list for guest files | positive `key_allowlist` |
| Don't use the unauthenticated sidecar; CID-scoped only | Transport |
| Codex self-refresh vs clean injection | M2 refresh-shim via official override |
| New host-side attack surface treated as design | "Why on-demand" + DoS budget |
| Over-engineering / YAGNI | Anti-over-engineering guardrails; milestone-gated |

## Open questions

1. **claude-personal token plumbing (M1 spike):** does `apiKeyHelper` /
   `ANTHROPIC_AUTH_TOKEN` carry a subscription OAuth access token (Bearer), and
   does the inference endpoint accept a host-minted short-lived one?
2. **codex override contract (M2 spike):** exact request/response shape at the
   pinned version.
3. **M3 TLS termination:** guest-trusted host CA (host-side MITM) vs explicit
   forward-proxy contract for credential injection.
4. **macOS/VZ listen-side** validation for the guest-outbound vsock connection.

## Affected code (for the implementation plan)

- `void-box-protocol/src/lib.rs` — `MessageType` 28/29 + types (`nonce`,
  `expires_at`).
- `src/backend/` — dedicated vsock listener + handshake for guest-initiated
  requests.
- `guest-agent/` + new `void-cred/` crate (uid 1000) — broker client, token-fetch
  helper, codex loopback shim, (M3+) egress proxy; `DEFAULT_COMMAND_ALLOWLIST`.
- `src/credentials.rs` — host-retained durable secret + refresh logic (rotation,
  expiry); staging shrinks to host-only.
- `src/runtime.rs` (`~234`, `~1352-1408`), `src/agent_box.rs` (`~464`) — delete RW
  mount + privileged WriteFile copy; wire the broker; (M1) configure
  `apiKeyHelper`.
- `src/llm.rs` — provider → required-credential mapping (data, replacing the
  hardcoded OAuth-provider `env_vars()` arms).
- `src/network/slirp.rs` — (M3) host-side egress injection point.
