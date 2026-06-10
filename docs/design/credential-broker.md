# Design: host-mediated credential containment for the guest agent

Status: proposal. The host credential store and the injection proxy are the
committed substrate (required for API-key containment and downstream egress). For
OAuth providers, the delivery mechanism — token injection vs. proxy injection — is
left open and chosen against the criteria below.

Provider behaviors here are confirmed against Claude Code 2.1.170 and the codex
version pinned in `scripts/agents/manifest.toml`; re-verify on version bumps (R10).

## Trust model

Single-tenant: the host operator is the data owner and is authorized to see all
guest plaintext — it already owns the guest's RAM, filesystem, and network. The
guest agent (uid 1000) is untrusted, the expected adversary (prompt injection, a
compromised dependency). The design is **not** suitable as-is where the operator
must not read tenant data — proxy TLS termination would expose it; that deployment
needs an additional control and is out of scope. This boundary also tracks provider
policy (below): a user running their own subscription is "ordinary use"; an
operator routing other users' subscription credentials is not.

## North star and invariant

void-box lets an agent **use** credentials without **holding** the durable ones.

- **Invariant:** durable credentials — OAuth **refresh** tokens, long-lived **API
  keys**, and downstream service secrets — never enter the guest. The host holds
  them, performs all refresh, and is the sole rotation owner. Any token the guest
  holds is short-lived and host-revocable.
- **Stronger tier (proxy):** the guest holds no credential at all, only non-secret
  placeholders. Downstream egress and API keys always use this tier; OAuth
  providers use it only if the proxy path is chosen.

## The risk this addresses

The `claude-personal` and `codex` providers stage the OAuth credential —
**including the refresh token** — into the guest: an RW bind-mount of the
credential file at `/home/sandbox/.claude` or `/home/sandbox/.codex`
(`src/runtime.rs:234-247`, `1399-1408`), or a privileged WriteFile copy
(`src/agent_box.rs:464`). A uid-1000 agent can read it and exfiltrate the refresh
token, yielding account access that outlives the run. Both CLIs self-refresh
in-process and rotate single-use refresh tokens, so host-only ownership is the only
correct design — two refreshers invalidate each other's token.

API-key auth carries the same exposure, worse: the default `Claude` provider
forwards `ANTHROPIC_API_KEY`, `codex` falls back to `OPENAI_API_KEY`, and `Custom`
providers forward their key — all into the guest exec env (`src/llm.rs:427,474,480`),
readable by uid 1000 and snapshot-captured. An API key is long-lived, non-rotating,
full billing access. In scope. (Local providers — Ollama, LM Studio — pass only
non-secret placeholders.)

A leaked short-lived access token (auto-expiring, host-re-mintable) is an accepted,
bounded residual, not the target.

## Provider terms of service

The mechanism choice is constrained by provider policy, not only by security.

- **API keys are the sanctioned path for programmatic use.** Anthropic's policy
  states OAuth "is intended exclusively for … ordinary use of Claude Code and other
  native Anthropic applications," that developers "building products or services …
  should use API key authentication," and that Anthropic "does not permit
  third-party developers … to route requests through Free, Pro, or Max plan
  credentials on behalf of their users"
  ([Claude Code legal & compliance](https://code.claude.com/docs/en/legal-and-compliance);
  [Anthropic Consumer Terms](https://www.anthropic.com/legal/consumer-terms);
  [Usage Policy](https://www.anthropic.com/legal/aup)). OpenAI similarly restricts
  programmatic ChatGPT-subscription use, tolerating personal use of the official
  codex CLI on one's own subscription
  ([OpenAI Terms](https://openai.com/policies/row-terms-of-use/);
  [Usage Policies](https://openai.com/policies/usage-policies/)). So the **API-key
  proxy path carries no policy tension** — programmatic/proxied use is the intended
  use of an API key.
- **Personal-subscription OAuth is "ordinary use" only when single-tenant.** A user
  running their own subscription through the real Claude Code/codex is ordinary use;
  an operator routing other users' subscription credentials is the prohibited
  pattern — the same boundary as the trust model. void-box must not be deployed as a
  multi-tenant service over subscription credentials; such deployments use API keys.
- **Containment-vs-policy tradeoff for personal OAuth.** Host-side refresh/injection
  means the OAuth token is used by the host store, not strictly by Claude Code — the
  gray edge of "ordinary use of Claude Code." Today's mount keeps the token's use
  inside the client (policy-cleaner) but leaks the refresh token (the risk above).
  For personal subscriptions this is a tradeoff the user owns; the policy-clean path
  for anything programmatic is API keys.

This is a reading of public policy, not legal advice; operators should consult the
linked terms for their use case.

## Mechanism: the host injection proxy

A host-side, TLS-terminating, header-injecting egress proxy, backed by the
credential store. The client is pointed at the proxy and trusts a per-run CA
installed in the guest; the proxy rewrites the credential header(s) with the
host-held secret and forwards to the real upstream. The guest holds only
placeholders. This serves API-key containment and downstream egress, and is the
optional OAuth proxy path.

```
guest client ──▶ host injection proxy ──▶ real upstream
 placeholder cred,   TLS-terminate (per-run, name-constrained CA),
 base-URL redirect,  rewrite credential header(s),
 trusts proxy CA     re-encrypt to upstream (external wire stays encrypted)
```

**Credential store.** Holds each provider's durable secret — read from
`~/.claude`/`~/.codex`/Keychain, or the host env for API keys. For OAuth it refreshes
against the provider's token endpoint, mints short-lived access tokens, and is the
sole rotation owner (serialized refresh, rate-capped). Secrets live only here, in
host memory, `mlock`ed and zeroized via `secrecy`.

**Why TLS termination, and why it is safe.** The credential header lives inside the
TLS stream; rewriting it requires terminating TLS. Under the trust model this
exposes nothing new — the host already sees the guest's data. The proxy streams
bodies through without inspecting them, re-establishes TLS to the upstream (the
external wire stays encrypted), and trusts only a per-run CA installed in the guest
image (a scoped trust, not general interception). The one non-transparent effect:
the upstream sees the proxy's TLS fingerprint, not the client's (R8).

**API keys (`Claude`, `codex` env-key, `Custom`).** A static secret: the proxy
injects `x-api-key` (Anthropic) or `Authorization: Bearer` (OpenAI/codex; the
Custom provider's configured header). No refresh, no rotation, no policy tension —
the simplest slice and the first proof of the proxy spine (M0). codex API-key mode
targets `api.openai.com/v1`, redirected the same way.

**OAuth via the proxy (Claude/codex ChatGPT).**
- Claude: `ANTHROPIC_BASE_URL=<proxy>`, `NODE_EXTRA_CA_CERTS=<CA PEM>`,
  `CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST=1` (suppresses the hardcoded OAuth-refresh
  recovery and force-login), placeholder `ANTHROPIC_AUTH_TOKEN`, no credentials
  file. Only `/v1/messages` blocks and it honors the base URL.
- codex: `openai_base_url=<proxy>` + `credentials_store="file"` in
  `$CODEX_HOME/config.toml`, `CODEX_CA_CERTIFICATE=<CA PEM>`, and a placeholder
  `auth.json` (dummy `id_token`, placeholder `access_token`/`account_id`, recent
  `last_refresh`). The proxy injects the real Bearer and `ChatGPT-Account-ID` (real
  identity stays host-side) and passes `originator` through. codex defaults to
  Responses-over-WebSocket — force plain HTTPS (`supports_websockets=false`) or
  inject on the WS upgrade (R9).

Neither client pins certificates; both honor an additive CA via the env above (PEM
file; not `SSL_CERT_DIR`).

## OAuth-provider delivery: token injection vs. proxy

Both keep the refresh token off the guest; they differ in what else the guest holds
and what the host runs.

- **Token injection.** The store mints a short-lived access token delivered through
  the client's native injector; TLS stays end-to-end (no host CA, termination, or
  hot-path). Claude: `CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR` (Bearer tier,
  inherited fd — not in `/proc/environ` or on disk); ~24 h tokens cover task-mode,
  but a spawned CLI has no clean mid-run refresh hook, so long/service runs need
  re-mint-and-relaunch. codex: a loopback endpoint behind
  `CODEX_REFRESH_TOKEN_URL_OVERRIDE` proxies the refresh to the store; `auth.json`
  holds only a short-lived access token. Residual: a short-lived access token (and
  codex's real `id_token`/`account_id`) at rest in the guest.
- **Proxy injection.** Zero credential in the guest, at the cost of CA custody (R3),
  streaming correctness (R1), SSRF surface (R4), and a hot-path dependency.

**Decision criteria.** The proxy is built regardless (API keys, downstream), so
routing OAuth through it unifies the mechanism and yields zero in-guest credential.
Token injection is lighter and preserves the client's TLS fingerprint (R8 — relevant
if a subscription endpoint fingerprints TLS), but leaves a bounded token in the guest
and lacks a clean Claude long-run refresh. For personal subscriptions, weigh the
policy tradeoff (ToS, above). Decide after V1/V2.

## Downstream egress (M2/M3)

For allow-listed downstream services (GitHub API, internal APIs, later
database/mTLS) the proxy is the only mechanism — the host injects the destination's
(usually static) credential and the secret never enters the guest. Two interception
forms reuse the proxy: **explicit**, for tools honoring `HTTPS_PROXY` (curl, git,
most libraries — the common case, and the only form where the per-run proxy token
authenticates the client); and **transparent**, network-level interception
(`src/network/slirp.rs`) with per-destination certs for tools that ignore
`HTTPS_PROXY` (no place for a proxy token — relies on network isolation). The proxy
injects a credential only on **exact upstream host+path match**, never on
agent-controlled redirects or `Host` headers, and follows no credentialed redirects
(R4).

## Egress enforcement

VoidBox's network is **default-allow with a deny-list** (`src/network/nat.rs`); there
is no allow-list today. The proxy is therefore **advisory** until a default-deny
egress allow-list that pins the guest to the proxy (and blocks direct `:443` to
provider/destination IPs) is built (R2). Without it a compromised agent can bypass
the proxy and reach the internet directly — it cannot leak a credential (a direct
call carries no valid token), so containment holds, but the "bounded abuse" and
"fail-closed-for-usage" properties do not. This allow-list is scoped as M1 work, not
assumed.

## Security properties

- **Refresh token / durable secret never in the guest** (both candidates) — held and
  rotated only on the host.
- **No credential at all in the guest** (proxy candidate; downstream and API keys
  always).
- **Single rotation owner** — only the host refreshes, removing the rotation-conflict
  failure mode.
- **Containment is bypass-safe** — a guest ignoring the proxy/injector cannot obtain
  a credential.
- **Bounded abuse / fail-closed-for-usage** — only once the egress allow-list exists
  (R2); otherwise advisory.
- **External wire stays encrypted** — for the proxy, TLS is re-established to the
  upstream; only a host-internal hop is plaintext.
- Opt-in; no behavior change for providers that do not stage a credential.

### Residual risks

- The durable secret now lives in **host** process memory for the run (vs. today's
  0600 temp file dropped on teardown) — a wider host-side surface on a shared host;
  mitigated by `mlock`/zeroize and per-run process isolation.
- Token-injection candidate: a short-lived access token (+ codex `id_token`/
  `account_id`) at rest in the guest for its lifetime.
- Proxy candidate: the host decrypts inference traffic (trust-model dependent); a
  hot-path availability coupling; the per-run proxy token is guest-readable (use, not
  theft — guards against neighbors, not the in-guest adversary).

## Risk register

Ordered by how much each could disrupt implementation.

| # | Risk | Likelihood | Impact | Mitigation / fallback |
|---|------|-----------|--------|-----------------------|
| R1 | **Proxy streaming/lifecycle correctness** — TLS-terminate + SSE + WS + backpressure + reuse + fail-closed, on every call in the hot path; a bug degrades all output. | Medium | High | Standard reverse-proxy patterns + dedicated streaming tests; primary engineering budget. Proxy path only. |
| R2 | **No egress allow-list** — "bounded abuse"/"fail-closed-for-usage" need a default-deny allow-list pinning guest→proxy; absent today (`src/network/nat.rs`). | High (absent) | Medium | Build as M1 work; until then those properties are advisory. Containment is unaffected. |
| R3 | **CA private-key custody / blast radius** (proxy) — a leaked CA key impersonates sites to the guest. | Low | High | **Per-run ephemeral CA**, **Name-Constrained** to the injected upstreams, generated at boot and destroyed on teardown; never exposed to the guest. |
| R4 | **SSRF / confused-deputy** via the credential-injecting proxy (acute in M2). | Medium | Medium–High | Exact host+path injection match; no credentialed redirects; no agent-controlled `Host`. |
| R5 | **Host-side OAuth refresh acceptance** — the token endpoint accepting an off-client refresh (refresh grant has no PKCE, but tokens could be binding-bound). | Low–Med | Medium–High | Confirm in V2 (throwaway). Evidence: standard refresh shape, plain Bearer, no DPoP observed; same egress IP via NAT. |
| R6 | **Inference acceptance of a host-supplied subscription Bearer.** | Low–Med | Medium–High | V2. Precedent: `CLAUDE_CODE_OAUTH_TOKEN` is an externally-supplied subscription Bearer the inference endpoint accepts. |
| R7 | **Provider ToS** — personal-subscription OAuth used outside the native client, or multi-tenant routing of subscription credentials, is restricted. | — | High (policy) | API keys for programmatic/hosted use; personal-OAuth is single-tenant ordinary use, user-owned (see ToS section). |
| R8 | **TLS fingerprint** — proxy termination changes the upstream-visible JA3/JA4; on subscription endpoints a client mismatch could be a signal. | Low | Low–Med | Low signal on API endpoints (diverse clients expected); token injection preserves the client fingerprint; uTLS-style impersonation if needed. |
| R9 | **codex WebSocket transport** vs header injection. | Low | Low–Med | Default `supports_websockets=false` (plain HTTPS); inject-on-upgrade optional. |
| R10 | **Provider version drift** changing redirect/refresh/header/transport behavior. | Low | Low–Med | Pin versions; re-verify V1/V2 facts on bump (bump workflow gates it). |
| R11 | **M2 transparent per-destination cert generation.** | Low | Low | Deferred; explicit `HTTPS_PROXY` avoids it; established SSL-bump pattern. |

## Validation order and implementation plan

Two technical gates retire the feasibility unknowns before milestone work; provider
policy is settled by reading the terms (ToS section), not by a spike.

**V1 — redirect/CA/suppress on the pinned versions (no account).** Point the pinned
clients at a dumb logging proxy; confirm `ANTHROPIC_BASE_URL`/`openai_base_url`
redirect, `PROVIDER_MANAGED_BY_HOST` suppresses refresh/force-login, the CA env vars
are honored, and only inference blocks (no retry-storm on the other Claude
endpoints). Gates all proxy provisioning (API-key and OAuth).

**V2 — OAuth acceptance (throwaway account).** Have a host process replay the OAuth
refresh (`client_id` + `refresh_token` + `grant_type=refresh_token`) and inject the
minted Bearer on one real inference request, for Claude and codex; confirm a usable
completion. Resolves R5/R6. OAuth path only; the API-key path does not need it.

**Then build, smallest-risk first:**
- **M0** — static-key proxy for the **API-key** providers (Anthropic `x-api-key`,
  OpenAI/codex Bearer, Custom). No credential store, refresh, or rotation. Ungated by
  V2 and by the OAuth decision; needs only V1 plus the per-run name-constrained CA
  (R3) and the streaming proxy (R1). The lowest-risk proof of the proxy spine, so it
  leads.
- **M1a** — credential store (refresh/mint/rotation) + the chosen OAuth mechanism for
  **Claude only**, single platform, inference path. If proxy: reuses M0's CA and
  streaming proxy.
- **M1b** — codex (WS handling, R9), long-run rotation, and KVM+VZ parity.
- **Egress allow-list** (R2) — default-deny, guest pinned to the proxy — so the
  bounded-abuse/fail-closed properties become real.
- **M2/M3** — downstream egress, per-destination policy, SSRF hardening (R4),
  transparent form (R11).

Remove the RW credential mounts and the `src/agent_box.rs:464` WriteFile copy as each
provider migrates. Exit gate: `e2e_agent_mcp` (Claude) and the codex smoke specs pass
with no refresh token in the guest (and, on the proxy path, no credential at all).

## Affected code

- New host modules — the credential store (durable secret custody; OAuth
  refresh/mint/rotation) and the injection proxy (TLS-terminating, header-rewriting,
  streaming) plus the per-run CA.
- `src/credentials.rs` — host-retained credential, host-side refresh/rotation,
  short-lived-token minting; staging is host-only.
- `src/runtime.rs` (`~234`, `~1352-1408`), `src/agent_box.rs` (`~464`) — remove the RW
  mount and the WriteFile copy; provision the guest per the chosen mechanism.
- `src/llm.rs` — provider → store/proxy/injector wiring, replacing the API-key env
  forwarding in `env_vars()` (`~417`).
- `src/network/nat.rs` / `src/network/slirp.rs` — the default-deny egress allow-list
  (R2) and the M2 transparent interception point.
- Guest image — install the per-run CA into the trust stores the clients honor
  (`NODE_EXTRA_CA_CERTS`; `CODEX_CA_CERTIFICATE`/`SSL_CERT_FILE`; and the system store
  for M2 tools) — a per-image checklist across initramfs and OCI-rootfs.
- `scripts/agents/manifest.toml` — the pinned versions the behaviors are verified
  against.
