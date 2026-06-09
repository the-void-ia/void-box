# Design: host-side OAuth credential broker for the guest agent

Status: **proposal** (no code yet). Owner: runtime. Target branch:
`claude/vm-agent-credentials-syz9c2`.

This doc is scoped to **one security finding**: a prompt-injected or compromised
in-guest agent (uid 1000) can read the OAuth **refresh** tokens we mount today
for the `claude-personal` and `codex` providers and take over the operator's
provider account for the lifetime of that token — well beyond the run. The
durable fix is to keep refresh tokens on the host and hand the guest only
short-lived access tokens.

A broader, decoupled "any-secret-from-any-source" credential mechanism is
desirable but **deliberately deferred** — it is sketched as future work at the
end so this proposal does not inflate its risk or commit to speculative
generality before a second, non-OAuth consumer exists.

## The finding, precisely

Today (`src/credentials.rs`, `src/runtime.rs`, `src/agent_box.rs`):

- `codex` (always) and sandbox-mode `claude-personal` **RW-bind-mount** a 0600
  staged temp dir containing the full OAuth credential — **including the refresh
  token** — at `/home/sandbox/.codex` or `/home/sandbox/.claude`.
- service/agent-mode `claude-personal` instead does a **privileged WriteFile-RPC
  copy** of the full `.credentials.json` into guest tmpfs (`src/agent_box.rs:464`).

In both shapes the uid-1000 agent can `cat` the file and exfiltrate the
access **and refresh** tokens. The existing mitigations (0600 staging, temp-dir
drop at run end) only limit guest-side *tampering* persistence; they do nothing
for *exfiltration*. Read-tightening doesn't help either — a valid token is
usable wherever it's read.

Two facts make this worse and shape the fix:

- **Both CLIs self-refresh in-process.** codex (confirmed) refreshes proactively
  before expiry and reactively on 401, writing rotated tokens back to
  `auth.json`; the RW mount exists *specifically* so it can. claude-code's
  personal OAuth is structurally the same (a `.credentials.json` with
  `accessToken`/`refreshToken`/`expiresAt`).
- **OpenAI uses refresh-token rotation** — each refresh consumes the old refresh
  token and issues a new one. Two parties refreshing the same token invalidate
  each other ("refresh token was already used"). So there can be only **one**
  refresh owner. Today that's ambiguous (the guest's CLI rotates inside an
  ephemeral mount we then discard), which is not just insecure but a latent
  correctness bug across repeated runs.

## Goal / non-goals

Goal: for `claude-personal` and `codex`, the **refresh token never enters the
guest**. The host is the sole holder and sole refresher; the guest only ever
sees a short-lived access token with an explicit expiry. Fail closed if the
broker is unavailable.

Non-goals:

- **Preventing use of a leaked *access* token during its validity window.** The
  bound is temporal (≤ provider default, ~60 min), not preventive. A compromised
  agent that copies its access token and uses it before expiry is expected to
  succeed.
- Per-process isolation inside the guest. Any guest process reaching the broker
  can request an allow-listed credential; the guest is one trust domain. The
  win is "refresh token absent" + "host-mediated + time-bounded", not
  intra-guest compartmentalization.
- The plain `claude` (API-key) and `OPENAI_API_KEY` paths — those forward a
  static key, a separate concern from OAuth refresh-token containment.
- A general multi-source credential framework (see Future work).

## Why on-demand, and the new surface it creates

At-rest delivery leaks the credential to every guest process, the mounted file,
**any snapshot** taken during the run (guest RAM is dumped wholesale to disk,
`src/vmm/snapshot.rs:187`), and crash dumps. Host-side refresh removes the
refresh token entirely and bounds a leaked access token to its short lifetime.

The cost is a **genuinely new host-side attack surface**: the broker makes the
host *respond to guest-initiated requests*. Until now guest→host traffic is only
telemetry, and even that is host-initiated (the host sends `SubscribeTelemetry`
and owns the `request_id`; the guest merely streams frames tagged with the
host's id). **There is no existing precedent for the guest originating a
request.** The broker introduces one, so its handler is designed as untrusted-
input-facing from the start (bounded fields, allow-list check before any work,
DoS budget — see below), not as an afterthought.

## Architecture

```
provider token endpoints
   (auth.openai.com / Anthropic)
            ▲ HTTPS (real, rotating refresh token — host only)
            │
┌───────────┴──────────────── host ────────────────────────┐
│  OAuthBroker                                              │
│   - holds the real rotating refresh token per run         │
│   - mints/refreshes access tokens; serializes refresh     │
│   - enforces <= CEIL promised expiry; rate/DoS budget     │
│   - audit log                                             │
└───────────┬──────────────────────────────────────────────┘
            │ dedicated CID-scoped vsock port (guest→host),
            │ authenticated by a fresh session-secret handshake
┌───────────▼──────────────── guest (uid 1000) ────────────┐
│  void-cred                                                │
│   role A: fetch access token  → env-on-demand / 0600 file │
│   role B: codex refresh shim  → 127.0.0.1 HTTP → vsock     │
│  consumers: claude-code, codex                            │
└───────────────────────────────────────────────────────────┘
```

The host holds and refreshes; the guest only ever holds access tokens. `reqwest`
+ rustls is already a workspace dependency, so the host-side HTTPS refresh is
cheap to add.

### Transport (the dominant cost of P1)

The guest must initiate requests to the host. The two options, settled here:

- **Chosen: a dedicated, CID-scoped vsock port** the host listens on and the
  guest connects out to, authenticated by a **fresh** session-secret handshake
  (reusing `connect_with_handshake_sync` semantics). It does **not** inherit the
  existing control channel's authenticated state — a new listener must
  re-handshake — so we don't claim that. It must be **vsock (CID-scoped), never
  a TCP port on the SLIRP gateway**, or it inherits the sidecar's macOS
  `UNSPECIFIED`-bind LAN-exposure problem (`src/sidecar/server.rs`,
  `guest_accessible_bind_addr` in `src/backend/mod.rs`).
- **Rejected: reverse-RPC on the existing multiplex channel.** The multiplex
  stack assumes *host allocates `request_id`, guest responds*; an unsolicited
  guest frame is dropped (`src/backend/multiplex.rs`). Supporting guest-as-caller
  means building a second caller/responder role, an outbound id allocator, and a
  pending-slot table on both ends — duplicating the machinery in reverse. Larger
  and riskier than the dedicated port.

Platform note: the guest-outbound vsock connection exercises the **listen** side
of the VZ connector (GCD-callback based), a different path from today's
host-initiated connect. This is real work to validate on macOS/VZ, not a
checkbox.

**Transport-free initial slice.** For **task-mode** runs shorter than the access-
token TTL, the host can mint a fresh access token and inject it at launch
through the *existing* host→guest `ExecRequest` env/launch path — no guest-
initiated transport at all. The vsock broker is needed only for **mid-run
refresh** (long/service runs) and for codex's refresh shim. So an initial slice
can close the finding for the common task-mode case before the transport lands.

### Wire protocol

Append-only, matching the protocol's discipline:

```text
MessageType::CredentialRequest  = 28
  // { name: String, nonce: [u8;16] }     -- nonce reserved for future
  //                                          per-consumer capability binding
MessageType::CredentialResponse = 29
  // { status: ok|denied|unavailable|expired,
  //   secret?: bytes,        -- access token / token-endpoint response body
  //   expires_at?: u64,      -- unix secs; broker-promised ceiling
  //   error?: String }
```

`MAX_MESSAGE_SIZE` bounds payloads; the handler additionally bounds `name`
length and checks the allow-list before any source work. `unavailable`/`denied`
are terminal — the consumer fails closed, never falling back to a mount. The
`nonce` is included now because the append-only format makes it expensive to add
later (replay/capability binding is otherwise deferred).

### Host-side OAuth refresh

- The broker holds the real OAuth credential per run, wrapped in `SecretString`,
  and is the **only** refresher. Upstream refreshes are **serialized** and
  rate-capped independently of guest-facing requests: refresh at most once per
  ~TTL/2, otherwise serve the cached access token. This both respects rotation
  (no concurrent "already used" failures) and blunts a guest that spams requests
  to burn the provider's rate limit or force rotation.
- **Expiry ceiling** is enforced on the broker-*promised* `expires_at`, not the
  token's internal claims (a signed JWT's `exp` can't be rewritten). The broker
  never promises beyond `now + CEIL`, `CEIL ≤` the provider default (~60 min for
  both); the guest shim trusts `expires_at` and re-requests before it. Staying at
  or below the provider default means the ceiling never changes operational
  behavior.

### Guest-side shim (`void-cred`, uid 1000)

`void-cred` is the only component speaking the broker protocol; it runs as uid
1000 (no PID-1/root involvement, consistent with the broader goal of demoting
privileged file RPCs). Two roles:

- **Role A — token fetch.** `void-cred exec --name … -- <program>` requests an
  access token and either sets it in the child's env and `execve`s, or writes a
  0600 file the consumer reads. For mid-run refresh it re-requests before
  `expires_at` and overwrites the file. "Refreshing the file" means *ask the
  broker again and rewrite* — `void-cred` never performs an OAuth exchange and
  never holds a refresh token.
- **Role B — codex refresh shim.** `void-cred` listens on `127.0.0.1:<port>`
  inside the guest; codex's `CODEX_REFRESH_TOKEN_URL_OVERRIDE` points at it.
  codex POSTs its refresh request (`grant_type=refresh_token`, `client_id`,
  `refresh_token=<opaque sentinel>`); `void-cred` forwards over vsock to the
  broker, which uses the **real** host-held refresh token to refresh against the
  provider and returns a normal token response. The HTTP stays loopback-only;
  the real refresh token stays host-side.

**Delivery-selection rule** (which shape for which case):

| Case | Delivery |
|---|---|
| Static key (`claude` API key, `OPENAI_API_KEY`) — out of finding scope | env injection |
| OAuth, task-mode shorter than TTL | host-mint at launch (no transport) |
| OAuth, long/service run, consumer self-refreshes via overridable endpoint (codex) | refresh shim (Role B) |
| OAuth, long/service run, file-based without override | 0600 access-token file + Role-A refresher |

When a background refresher is needed it is a small `void-cred` sleep loop
(renew at ~80% TTL with jitter), **not cron** (absent from the minimal
initramfs), tied to the agent's lifetime via `PR_SET_PDEATHSIG` so it dies with
the run. Prefer demand-driven refresh (Role B / on-401) over timers where the
consumer allows it.

### File contents allow-list

Any guest-visible credential file declares a **positive `key_allowlist`**; the
shim writes only those keys. There is no "reject keys containing 'refresh'"
denylist (bypassable by alternate field names) — correctness rests on the
positive allow-list plus a test asserting the emitted JSON's key set is a subset
of it. A refresh-token-shaped key therefore cannot appear.

## Milestones (all within this P1 finding)

- **M0 — broker foundation.** `OAuthBroker` (host-held refresh token, serialized
  refresh, expiry ceiling, rate/DoS budget, audit), the dedicated CID-scoped
  vsock port + handshake, the `CredentialRequest/Response` wire types, and
  `void-cred` Role A. Includes the transport-free host-mint-at-launch path so a
  first slice can land.
- **M1 — `claude-personal`.** Contain claude-personal: delete its RW mount and
  the `agent_box.rs:464` WriteFile copy; deliver access tokens via host-mint
  (task mode) and Role-A file refresher (long runs). **Spike:** confirm
  claude-code's personal-OAuth refresh behavior and whether it exposes a
  refresh-endpoint override (if yes, it can use a Role-B-style shim like codex;
  if not, it uses the file+refresher path). Regression gate: `e2e_agent_mcp`
  (Claude) still authenticates; tests assert no refresh-token key and ≤ ceiling
  expiry in the guest-visible credential.
- **M2 — `codex`.** Contain codex via the refresh shim: delete its RW mount;
  seed `auth.json` with a host-minted access token + sentinel refresh token; set
  `CODEX_REFRESH_TOKEN_URL_OVERRIDE` to `void-cred`'s loopback endpoint.
  **Spike:** confirm, against the pinned codex version, the exact request body /
  headers codex sends to the override URL and the response JSON fields it parses
  (`access_token` / `id_token` / `refresh_token` / `expires_in` / `account_id`).
  Regression gate: the codex smoke specs still authenticate; same
  no-refresh-token / bounded-expiry assertions.

## Security properties

- **No refresh token in the guest** for OAuth providers — held and rotated only
  on the host; enforced structurally + by the positive `key_allowlist` test.
- **Time-bounded blast radius** — a leaked access token is unusable after
  `expires_at ≤ now + CEIL`.
- **Fail closed** — broker unavailable/denied ⇒ the run errors; never a silent
  mount fallback.
- **Single rotation owner** — only the host refreshes, eliminating the
  "refresh token already used" conflict and fixing the latent repeated-run bug.
- **Host treats guest input as hostile** — bounded `name`, allow-list before any
  work, a concrete DoS budget (max concurrent broker requests per channel,
  bounded handler memory, serialized + rate-capped upstream refresh). The
  dedicated listener keeps this surface isolated and independently fuzzable.
- **Loopback-only codex HTTP** — the refresh shim binds `127.0.0.1`; no host
  HTTP port is exposed on the NAT gateway.
- **Redaction + zeroize** end to end via `secrecy`.
- **Opt-in / no behavior change** for providers that don't stage OAuth today.

### Honest residual risks

- **Live access-token use during its window** — out of scope by design (temporal
  bound, not preventive).
- **Access-token *file* is snapshot-captured for its lifetime.** The "out of
  snapshots" property holds for broker-pull/env, **not** for the file delivery
  codex (and possibly claude) needs — a mid-run snapshot captures the live access
  token from the file's page cache and the consumer process. Acceptable only
  because it is ≤ CEIL and refresh-token-free; stated plainly rather than
  glossed.
- **Intra-guest sharing** — any guest process reaching the broker can request an
  allow-listed name; the `nonce` field reserves room for future per-consumer
  capability tokens.
- **Rate-limit as self-DoS** — too-tight a limit starves the legitimate consumer;
  the legit path therefore *delays* rather than *denies*, with burst+steady
  values to be set from real cadence.

## Coverage of the security-review findings

| Review criterion | Where addressed |
|---|---|
| Refresh tokens never enter the guest | host-side refresh; positive `key_allowlist` |
| Host performs refresh; returns access token + expiry | OAuthBroker; `CredentialResponse.expires_at` |
| ≤ provider-default ceiling; re-request before expiry | expiry ceiling; shim trusts `expires_at` |
| Fail closed, no silent mount fallback | wire `denied`/`unavailable` terminal; Security properties |
| Remove RW mount **and** privileged WriteFile copy | M1/M2 deletions |
| New vsock RPC; auth via fresh handshake (not cmdline-secret-dependent long-term) | Transport; wire protocol |
| Rate limits / DoS budget (decoupled guest vs upstream) | Host-side refresh; Security properties |
| Per-provider allowed-key list for guest files | positive `key_allowlist` |
| Don't use the unauthenticated sidecar; CID-scoped only | Transport |
| Codex clean-injection vs self-refresh unknown | resolved: refresh shim via official override; M2 spike for exact contract |
| New host-side attack surface treated as design | "Why on-demand" + DoS budget |

## Open questions

1. **claude-personal refresh behavior (M1 spike).** Does claude-code self-refresh
   personal OAuth in-process, and does it expose a refresh-endpoint override
   (codex-style) or accept an access-token-only `.credentials.json` for short
   tasks? Decides M1's exact mechanism.
2. **codex override contract (M2 spike).** Exact request/response shape codex
   uses with `CODEX_REFRESH_TOKEN_URL_OVERRIDE` at the pinned version.
3. **`Env` delivery** for static keys: keep as a documented lower-security shape,
   or route everything through the shim?
4. **macOS/VZ listen-side** validation for the guest-outbound vsock connection.

## Affected code (for the implementation plan)

- `void-box-protocol/src/lib.rs` — `MessageType` 28/29 + request/response types
  (with `nonce`, `expires_at`).
- `src/backend/` — new dedicated vsock listener + handshake for guest-initiated
  requests (host side).
- `guest-agent/` and a new `void-cred/` crate (guest, uid 1000) — broker client,
  Role-A fetch, Role-B loopback refresh shim; `DEFAULT_COMMAND_ALLOWLIST`
  (`src/backend/mod.rs`).
- `src/credentials.rs` — host-retained OAuth credential + refresh logic
  (rotation handling, expiry); staging shrinks to host-only.
- `src/runtime.rs` (`~234`, `~1352-1408`), `src/agent_box.rs` (`~464`) — delete
  RW mount + privileged WriteFile copy; wire the broker.
- `src/llm.rs` — provider → required-credential mapping (data, replacing the
  hardcoded `env_vars()` arms for the OAuth providers).

## Future work (explicitly NOT built here)

A general, decoupled credential mechanism — the original broader goal — builds
*on* this broker once a real second, non-OAuth consumer exists (task secrets:
GitHub/npm/cloud/SSH tokens). When justified, it would generalize to:

- a declarative `CredentialSpec` with orthogonal **source** (host env / file /
  OS keychain / command stdout / external managers like Vault, 1Password,
  AWS-SM) and **delivery** (broker pull / file / env) axes, behind a
  `SourceResolver` trait so new backends don't touch call sites;
- a `sandbox.credentials:` spec surface (+ `BoxSandboxOverride`) for tasks to
  declare their own secrets instead of plaintext `env:`;
- per-consumer **capability tokens** (the reserved `nonce`) for intra-guest
  scoping.

These are deferred deliberately: none is required to close the finding, and
building the generic framework before a second consumer exists is speculative
generality. This section records the direction so the P1 work stays compatible
with it (the broker, transport, and wire format are the shared substrate).
