# Design: host-mediated credential containment for the guest agent

Status: proposal. The host credential store and the injection proxy are the
committed substrate (required for API-key containment and downstream credential
injection). For OAuth providers, the delivery mechanism — token injection vs. proxy
injection — is left open and chosen against the criteria below.

Provider behaviors here are confirmed against Claude Code 2.1.170 and openai/codex
at commit `9e3081be9672c65f8a0cd958719065f49f47d839`; re-verify on version bumps
(R9), including against the codex version actually bundled
(`scripts/agents/manifest.toml`) if it differs.

**Scope.** This design covers *credential containment* — keeping durable credentials
off the guest and injecting them at egress for the endpoints that need them. It uses
**selective routing**: only the clients that need a host-held credential (the LLM
providers; later GitHub) are pointed at the proxy. Network **egress policy** — which
destinations the agent may reach at all, audit, allow-lists, and routing profiles —
is **orthogonal and specified separately** (`docs/design/egress-policy.md`).
Credential containment holds under any egress policy.

## Trust model

Single-tenant: the host operator is the data owner and is authorized to see all
guest plaintext — it already owns the guest's RAM, filesystem, and network. The
guest agent (uid 1000) is untrusted, the expected adversary (prompt injection, a
compromised dependency). The design is **not** suitable as-is where the operator
must not read tenant data — proxy TLS termination would expose it; that deployment
needs an additional control and is out of scope. This boundary also tracks provider
policy (below): a user running their own subscription is "ordinary use"; an operator
routing other users' subscription credentials is not.

## North star and invariant

void-box lets an agent **use** credentials without **holding** the durable ones.

- **Invariant:** durable credentials — OAuth **refresh** tokens, long-lived **API
  keys**, and downstream service secrets — never enter the guest. The host holds
  them, performs all refresh, and is the sole rotation owner. Any token the guest
  holds is short-lived and host-revocable.
- **Stronger tier (proxy):** the guest holds no credential at all, only non-secret
  placeholders. API keys and downstream injection always use this tier; OAuth
  providers use it only if the proxy path is chosen.

## The risk this addresses

The `claude-personal` and `codex` providers stage the OAuth credential —
**including the refresh token** — into the guest: an RW bind-mount of the credential
file at `/home/sandbox/.claude` or `/home/sandbox/.codex`
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

A host-side, TLS-terminating, header-injecting proxy, backed by the credential
store. Only the clients that need a host-held credential are pointed at it (selective
routing — see Scope); the client trusts a per-run CA installed in the guest, the
proxy rewrites the credential header(s) with the host-held secret, and forwards to
the real upstream. The guest holds only placeholders.

```
guest client ──▶ host injection proxy ──▶ real upstream
 placeholder cred,   TLS-terminate (per-run, name-constrained CA),
 base-URL redirect,  rewrite credential header(s),
 trusts proxy CA     re-encrypt to upstream (external wire stays encrypted)
```

**Credential store.** Holds each provider's durable secret — read from
`~/.claude`/`~/.codex`/Keychain, or the host env for API keys. For OAuth it refreshes
against the provider's token endpoint, mints short-lived access tokens, is the sole
rotation owner (serialized refresh, rate-capped), and **persists the rotated refresh
token back to the host** so subsequent runs stay valid. Secrets live only here, in
host memory, `mlock`ed and zeroized via `secrecy`.

**Why TLS termination, and why it is safe.** The credential header lives inside the
TLS stream; rewriting it requires terminating TLS. Under the trust model this exposes
nothing new — the host already sees the guest's data. The proxy streams bodies
through without inspecting them, re-establishes TLS to the upstream (the external
wire stays encrypted), and trusts only a per-run CA installed in the guest image (a
scoped trust, not general interception). The one non-transparent effect: the upstream
sees the proxy's TLS fingerprint, not the client's (R7).

**API keys (`Claude`, `codex` env-key, `Custom`).** A static secret: the proxy
injects `x-api-key` (Anthropic) or `Authorization: Bearer` (OpenAI/codex; the Custom
provider's configured header). No refresh, no rotation, no policy tension — the
simplest slice and the first proof of the proxy spine (M0). codex API-key mode
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
  inject on the WS upgrade (R8).

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
- **Proxy injection.** Zero credential in the guest, at the cost of CA custody (R2),
  streaming correctness (R1), SSRF surface (R3), and a hot-path dependency.

**Decision criteria.** The proxy is built regardless (API keys, downstream
injection), so routing OAuth through it unifies the mechanism and yields zero
in-guest credential. Token injection is lighter and preserves the client's TLS
fingerprint (R7 — relevant if a subscription endpoint fingerprints TLS), but leaves a
bounded token in the guest and lacks a clean Claude long-run refresh. For personal
subscriptions, weigh the policy tradeoff (ToS, above). Decide after V1/V2.

## Downstream credential injection

Some named downstream services need a host-held secret injected (e.g. a GitHub token
for `api.github.com`). The same proxy injects it: the service is routed to the proxy
by **explicit client config** (a `git` credential helper, or a scoped `HTTPS_PROXY`
for that host), and the proxy rewrites the auth header with the host-held credential.
These are usually static secrets (no OAuth refresh). The proxy injects a credential
only on **exact upstream host+path match**, never on agent-controlled redirects or
`Host` headers, and follows no credentialed redirects (R3). Per-destination injection
policy is operator-declared.

How the agent's *general* (non-credentialed) egress is permitted, audited, or
restricted — including transparent interception and domain allow-lists — is the
concern of the separate egress design (`docs/design/egress-policy.md`).

## Security properties

- **Refresh token / durable secret never in the guest** (both candidates) — held and
  rotated only on the host.
- **No credential at all in the guest** (proxy candidate; API keys and downstream
  always).
- **Single rotation owner** — only the host refreshes, removing the rotation-conflict
  failure mode.
- **Containment is bypass-safe** — a guest ignoring the proxy/injector cannot obtain
  a credential (a direct call to the upstream carries no valid token).
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
| R1 | **Proxy streaming/lifecycle correctness** — TLS-terminate + SSE + WS + backpressure + reuse + fail-closed, on every routed call; a bug degrades all output. | Medium | High | Standard reverse-proxy patterns + dedicated streaming tests; primary engineering budget. Proxy path only. |
| R2 | **CA private-key custody / blast radius** (proxy) — a leaked CA key impersonates sites to the guest. | Low | High | **Per-run ephemeral CA**, **Name-Constrained** to the injected upstreams, generated at boot and destroyed on teardown; never exposed to the guest. |
| R3 | **SSRF / confused-deputy** via the credential-injecting proxy. | Medium | Medium–High | Exact host+path injection match; no credentialed redirects; no agent-controlled `Host`. |
| R4 | **Host-side OAuth refresh acceptance** — the token endpoint accepting an off-client refresh (refresh grant has no PKCE, but tokens could be binding-bound). | Low–Med | Medium–High | Confirm in V2 (throwaway). Evidence: standard refresh shape, plain Bearer, no DPoP observed; same egress IP via NAT. |
| R5 | **Inference acceptance of a host-supplied subscription Bearer.** | Low–Med | Medium–High | V2. Precedent: `CLAUDE_CODE_OAUTH_TOKEN` is an externally-supplied subscription Bearer the inference endpoint accepts. |
| R6 | **Provider ToS** — personal-subscription OAuth used outside the native client, or multi-tenant routing of subscription credentials, is restricted. | — | High (policy) | API keys for programmatic/hosted use; personal-OAuth is single-tenant ordinary use, user-owned (see ToS section). |
| R7 | **TLS fingerprint** — proxy termination changes the upstream-visible JA3/JA4; on subscription endpoints a client mismatch could be a signal. | Low | Low–Med | Low signal on API endpoints (diverse clients expected); token injection preserves the client fingerprint; uTLS-style impersonation if needed. |
| R8 | **codex WebSocket transport** vs header injection. | Low | Low–Med | Default `supports_websockets=false` (plain HTTPS); inject-on-upgrade optional. |
| R9 | **Provider version drift** changing redirect/refresh/header/transport behavior. | Low | Low–Med | Pin versions; re-verify V1/V2 facts on bump (bump workflow gates it). |

## Validation order and implementation plan

Two technical gates retire the feasibility unknowns before milestone work; provider
policy is settled by reading the terms (ToS section), not by a spike.

**V1 — redirect / CA / suppression on the pinned versions (no account, gates all
proxy provisioning).**

1. Stand up a throwaway HTTPS proxy with a self-signed CA that logs requests and
   forwards upstream.
2. Claude: set `ANTHROPIC_BASE_URL=<proxy>`, `NODE_EXTRA_CA_CERTS=<CA PEM>`,
   `CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST=1`, a placeholder `ANTHROPIC_AUTH_TOKEN`,
   and no credentials file; run `claude -p "hi"`.
3. codex: set `openai_base_url=<proxy>` + `credentials_store="file"` in
   `config.toml`, `CODEX_CA_CERTIFICATE=<CA PEM>`, and a placeholder `auth.json`;
   run `codex exec "hi"` (set `supports_websockets=false` to force plain HTTPS).
4. **Pass:** both clients route inference through the proxy, trust the CA, do not
   hard-fail on the missing real credential, and trigger no force-login or
   retry-storm against hardcoded endpoints. Confirm codex emits `ChatGPT-Account-ID`
   + `originator`. Unblocks M0 and the proxy path.

**V2 — OAuth acceptance (throwaway account, OAuth path only; the API-key path skips
this).**

1. On a throwaway Claude Pro/Max and a throwaway ChatGPT Plus account, log in
   normally and capture the real refresh token.
2. Host-side, replay the refresh (`client_id` + `grant_type=refresh_token` +
   `refresh_token`) against each token endpoint; confirm a usable access token comes
   back, and **store the rotated refresh token host-side** (R4).
3. Inject the minted Bearer through the proxy on one real inference request per
   provider; confirm a usable completion (R5).
4. **Pass:** host-minted tokens authenticate inference for both providers. Watch for
   any extra required header/param or a `401/403` indicating token binding/
   attestation. **Fail → the credential-store premise needs a rethink** (fall back
   to API-key-only, or re-evaluate token injection).

**Then build, smallest-risk first:**
- **M0** — static-key proxy for the **API-key** providers (Anthropic `x-api-key`,
  OpenAI/codex Bearer, Custom). No credential store, refresh, or rotation. Ungated by
  V2 and by the OAuth decision; needs only V1 plus the per-run name-constrained CA
  (R2) and the streaming proxy (R1). The lowest-risk proof of the proxy spine, so it
  leads.
- **M1a** — credential store (refresh/mint/rotation + host write-back) + the chosen
  OAuth mechanism for **Claude only**, single platform, inference path. If proxy:
  reuses M0's CA and streaming proxy.
- **M1b** — codex (WS handling, R8), long-run rotation, and KVM+VZ parity.
- **M2** — downstream credential injection for named services (e.g. GitHub), explicit
  routing, per-destination policy, SSRF hardening (R3).

Remove the RW credential mounts and the `src/agent_box.rs:464` WriteFile copy as each
provider migrates. Exit gate: `e2e_agent_mcp` (Claude) and the codex smoke specs pass
with no refresh token in the guest (and, on the proxy path, no credential at all).

## Implementation readiness

What an implementer can take as settled versus what they must still design.

**Settled — do not re-derive:** the invariant, trust model, and ToS boundary; the
mechanism (host injection proxy + credential store; selective routing of credentialed
endpoints); the per-provider client knobs (env vars, config, headers) and their
verified behavior; the milestone order (M0 leads); the risk register and the V1/V2
gates.

**The implementer must design (within the chosen approach):**
- **Guest→proxy reachability and binding** — how a configured client reaches the
  proxy (host listener via the SLIRP gateway), bound guest-only per platform
  (loopback on KVM; a VZ-specific address on macOS, not `UNSPECIFIED`).
- **Per-run proxy token** — generation, injection into the guest, the proxy-side
  check, and stripping it before forwarding upstream.
- **Per-run CA** — generation, name-constraint, install into the guest trust stores
  per image (`NODE_EXTRA_CA_CERTS`, `CODEX_CA_CERTIFICATE`), and teardown.
- **Lifecycle placement** — which host process runs the proxy and store, one per
  sandbox, started/stopped on which VM hook.
- **Credential store internals** — reuse the discovery in `src/credentials.rs`;
  per-provider refresh-request construction; token cache + serialized refresh; host
  write-back of rotated tokens.

**Decisions to escalate (do not guess):**
- The OAuth delivery mechanism (token injection vs. proxy), after V2.
- Whether to contain personal-subscription OAuth at all given the ToS tradeoff, or to
  steer programmatic use to API keys.

**Suggested first session:** run V1, then build M0 (which needs the proxy
reachability, per-run token, CA install, and lifecycle placement — all reusable by
M1). Defer the credential store until V2 passes; bring the OAuth-mode decision to a
human.

## Affected code

- New host modules — the credential store (durable secret custody; OAuth
  refresh/mint/rotation + host write-back) and the injection proxy (TLS-terminating,
  header-rewriting, streaming) plus the per-run CA.
- `src/credentials.rs` — host-retained credential, host-side refresh/rotation,
  short-lived-token minting; staging is host-only.
- `src/runtime.rs` (`~234`, `~1352-1408`), `src/agent_box.rs` (`~464`) — remove the RW
  mount and the WriteFile copy; provision the guest per the chosen mechanism.
- `src/llm.rs` — provider → store/proxy/injector wiring, replacing the API-key env
  forwarding in `env_vars()` (`~417`).
- `src/network/*` — the credential proxy is reached by the configured clients via the
  SLIRP gateway; it does **not** change egress policy (owned by the egress design).
- Guest image — install the per-run CA into the trust stores the clients honor
  (`NODE_EXTRA_CA_CERTS`; `CODEX_CA_CERTIFICATE`/`SSL_CERT_FILE`) — a per-image
  checklist across initramfs and OCI-rootfs.
- `scripts/agents/manifest.toml` — the bundled provider versions; re-verify these
  behaviors against them on bumps (R9).
