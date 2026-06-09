# Design: host-mediated credential containment for the guest agent

Status: proposal. The host-side credential store and the downstream-egress proxy
are the committed substrate; the **LLM-provider delivery mechanism (token
injection vs. proxy injection) is decided after the feasibility spikes S-A/S-B**
(see Validation order).

Provider behaviors referenced below are confirmed against Claude Code 2.1.170 and
the codex version pinned in `scripts/agents/manifest.toml`; they must be
re-verified on version bumps (R9).

## Trust model

This design assumes the **host operator is authorized to see all guest plaintext**
— it already owns the guest's RAM, filesystem, and network. The guest (the agent,
uid 1000) is untrusted and is the expected adversary (prompt injection, supply-
chain compromise of a dependency). This is a single-tenant model: operator ==
data owner. It is **not** suitable, as written, for deployments where the host
operator must not read tenant data — proxy TLS termination (below) would expose
that data to the operator. Such a deployment needs an explicit additional control
and is out of scope here.

## North star and invariant

void-box is a secure runtime for AI agents that handle sensitive data and reach
sensitive downstream services. The runtime lets an agent **use** credentials
without **holding** the durable ones.

- **Required invariant:** durable credentials — OAuth **refresh** tokens,
  long-lived **API keys**, and downstream service secrets — never enter the guest.
  The host holds them, performs all refresh, and is the sole rotation owner. Any
  token the guest does hold is short-lived and host-revocable.
- **Stronger property (proxy injection):** the guest holds **no** credential at
  all — only non-secret placeholders. Downstream egress always uses this stronger
  tier (there is no short-lived-token equivalent for arbitrary downstream
  secrets); the LLM providers use it only if the proxy path is chosen.

## The risk this addresses

The `claude-personal` and `codex` providers stage host-side OAuth credentials —
**including the refresh token** — into the guest: an RW bind-mount of the
credential file at `/home/sandbox/.claude` or `/home/sandbox/.codex`
(`src/runtime.rs:234-247`, `1399-1408`), or a privileged WriteFile copy into guest
tmpfs (`src/agent_box.rs:464`). A uid-1000 agent can read the file and exfiltrate
the refresh token, yielding account access that outlives the run. Both CLIs
self-refresh in-process and rotate single-use refresh tokens, so host-only
ownership is also the only correct design — two refreshers invalidate each
other's token.

**API-key auth carries the same exposure, and worse.** The default `Claude`
provider forwards a host `ANTHROPIC_API_KEY`, `codex` falls back to
`OPENAI_API_KEY` when the host is not OAuth-logged-in, and `Custom` providers
forward their key — all injected into the guest exec environment today
(`src/llm.rs:427,474,480`), readable by uid 1000 and snapshot-captured. An API key
is a worse leak than a refresh token: it is long-lived, non-rotating, and grants
full programmatic/billing access. It is in scope here. (Local providers — Ollama,
LM Studio — pass only non-secret placeholders and need no containment.)

A leaked **short-lived access token** (auto-expiring, host-re-mintable, not
durable account access) is an accepted, bounded residual, not the target.

## Shared foundation: the host credential store

Both provider mechanisms depend on one host component:

> A host credential store that holds each provider's OAuth refresh token (read
> from the host's `~/.claude`/`~/.codex`/Keychain), performs refresh against the
> provider's token endpoint, mints short-lived access tokens, and is the sole
> rotation owner — serialized refresh, rate-capped independently of request
> volume. The refresh token lives only here, in host process memory, `mlock`ed and
> zeroized via `secrecy`.

**The store's central premise is unproven and existential:** that a *host* process
(not the original client, off-device) can replay the OAuth refresh and have the
provider's token endpoint accept it, and that doing so on a *personal
subscription* does not trip anti-abuse/ban heuristics. These are spikes S-A and
S-B and gate all build work.

## LLM-provider delivery — two candidates, decided after S-A/S-B

Both satisfy the required invariant (refresh token off the guest). They differ in
what else the guest holds and what the host must run.

### Candidate A — token injection

The store mints a short-lived access token; the guest receives it through each
client's native injection point and refreshes by re-fetching from the host. TLS
stays end-to-end; no host CA, no termination, no hot-path proxy.

- **Claude:** inject via `CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR` (OAuth-Bearer
  tier, passed by inherited fd — not in `/proc/environ` or on disk). ~24 h tokens
  cover task-mode; a spawned CLI has **no** clean mid-run refresh hook, so
  long/service runs need re-mint-and-relaunch or are unsupported under this
  candidate.
- **codex:** a guest loopback endpoint behind `CODEX_REFRESH_TOKEN_URL_OVERRIDE`
  proxies the OAuth refresh to the store; the guest's `auth.json` holds a
  short-lived access token (refresh token never present). codex sends the token as
  `Authorization: Bearer`; `ChatGPT-Account-ID` comes from the real
  `account_id`/`id_token` it holds.

Residual: a short-lived, auto-expiring access token (and, for codex, the real
`id_token`/`account_id`) is at rest in the guest for its lifetime — bounded, but
not zero.

### Candidate B — proxy injection

A host-side, TLS-terminating, header-injecting egress proxy: the client is pointed
at the proxy and trusts a per-run CA installed in the guest; the proxy rewrites the
credential header(s) with the host-held token and forwards to the real upstream.
The guest holds **no** credential.

```
guest client ──▶ host injection proxy ──▶ real upstream
 placeholder token,  TLS-terminate (per-run, name-constrained CA),
 base-URL redirect,  rewrite Authorization (+ provider headers),
 trusts proxy CA     re-encrypt to upstream (external wire stays encrypted)
```

- **Claude:** `ANTHROPIC_BASE_URL=<proxy>`, `NODE_EXTRA_CA_CERTS=<CA PEM>`,
  `CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST=1` (confirmed in 2.1.170 to suppress the
  hardcoded OAuth-refresh recovery and force-login), placeholder
  `ANTHROPIC_AUTH_TOKEN`, no credentials file. Only `/v1/messages` is a blocking
  call and it honors `ANTHROPIC_BASE_URL` (confirmed); a missing credentials file
  resolves to null, not an error.
- **codex:** `openai_base_url=<proxy>` and `credentials_store="file"` in
  `$CODEX_HOME/config.toml`, `CODEX_CA_CERTIFICATE=<CA PEM>`, and a placeholder
  `auth.json` (structurally-valid **dummy** `id_token`, placeholder `access_token`
  and `account_id`, recent `last_refresh`). The proxy injects the real Bearer
  **and the real `ChatGPT-Account-ID`**, so real identity material stays host-side;
  it passes `originator: codex_cli_rs` through. codex defaults to
  Responses-over-WebSocket — force plain HTTPS via a custom provider
  `supports_websockets=false` (default for M1), or inject on the WS upgrade and
  pipe frames (R8).

Neither client pins certificates; both honor an additive CA via the env above
(PEM file; neither honors `SSL_CERT_DIR`).

Residual: the host decrypts inference traffic at the proxy (benign under the
trust model), and the proxy is a hot-path dependency (R4).

### API-key auth — proxy only

`Claude` (Anthropic API key), `codex` (`OPENAI_API_KEY` fallback), and `Custom`
providers authenticate with a static, long-lived API key. There is no short-lived
token to mint, so **token injection cannot contain them** — injecting the raw key
is no better than today's env forwarding. Containment is the proxy: the host holds
the key and the proxy injects it (`x-api-key` for Anthropic; `Authorization:
Bearer` for OpenAI/codex; the Custom provider's configured header) while the guest
carries only a placeholder. This path has **no OAuth, no refresh, no rotation**, so
the credential store reduces to a static secret and **S-A/S-B do not apply** — the
static-key proxy is the simplest, lowest-risk slice and the natural first proof of
the proxy spine (see Validation). For codex the base is `api.openai.com/v1` (not
the ChatGPT backend), redirected the same way via `openai_base_url`.

### Decision criteria

This choice applies to **OAuth** auth only (`claude-personal`, codex ChatGPT). For
**API-key** providers the proxy is the only containment, since injection cannot
contain a static key; absent the proxy, API keys remain forwarded into the guest as
today.

Choose after S-A/S-B/S-C/V1. The proxy carries more standing risk (CA custody,
streaming correctness, SSRF, hot-path) but yields zero in-guest credential and is
**built regardless** for downstream egress and API-key containment; token injection
is lighter and sufficient for the OAuth finding but leaves a bounded token in the
guest and has no clean Claude long-run refresh. If the proxy is built anyway,
routing OAuth providers through it unifies the mechanism; otherwise token injection
is the smaller OAuth-only footprint.

## Downstream egress (M2/M3): proxy injection

For allow-listed downstream services (GitHub API, internal APIs, later
database/mTLS) the proxy is the only mechanism — the host injects the
destination's (usually static) credential and the secret never enters the guest.
Two interception forms reuse the proxy: **explicit**, for tools honoring
`HTTPS_PROXY` (curl, git, most libraries; the common case, and the only form where
the per-run proxy token authenticates the client); and **transparent**,
network-level interception (`src/network/slirp.rs`) with per-destination certs for
tools that ignore `HTTPS_PROXY` (no place for a proxy token — relies on network
isolation). The proxy injects a credential only on **exact upstream host+path
match**, never on agent-controlled redirects or `Host` headers, and follows no
credentialed redirects (SSRF/confused-deputy — R7).

## Egress enforcement (required for the "bounded abuse" properties)

VoidBox's network is **default-allow with a deny-list** (`src/network/nat.rs`);
there is no allow-list today. The proxy is therefore **advisory** until a
**default-deny egress allow-list that pins the guest to the proxy address** (and
blocks direct `:443` to provider/destination IPs) is built. Without it, a
compromised agent can bypass the proxy and reach the internet directly — it cannot
leak the credential (a direct call carries no valid token), so containment holds,
but the "bounded abuse" and "fail-closed-for-usage" properties do **not** hold.
Building this allow-list is scoped as M1 work, not assumed.

## Security properties

- **Refresh token / durable secret never in the guest** (both candidates) — held
  and rotated only on the host.
- **No credential at all in the guest** (proxy candidate; downstream always).
- **Single rotation owner** — only the host refreshes, removing the rotation-
  conflict failure mode.
- **Containment is bypass-safe** — a guest that ignores the proxy/injector cannot
  obtain a credential.
- **Bounded abuse / fail-closed-for-usage** — only once the egress allow-list
  exists (above); otherwise advisory.
- **External wire stays encrypted** — for the proxy, TLS is re-established to the
  upstream; only a host-internal hop is plaintext.
- Opt-in; no behavior change for providers that do not stage OAuth.

### Residual risks

- The refresh token now lives in **host** process memory for the whole run (vs.
  today's 0600 temp file dropped on teardown) — wider host-side surface on a
  shared host; mitigated by `mlock`/zeroize and per-run process isolation.
- Token-injection candidate: a short-lived access token (+ codex `id_token`/
  `account_id`) at rest in the guest for its lifetime.
- Proxy candidate: host decrypts inference traffic (trust-model dependent);
  hot-path availability coupling; per-run proxy token is guest-readable (use, not
  theft — guards against neighbors, not the in-guest adversary).

## Risk register

Ordered by how much each could disrupt implementation.

| # | Risk | Likelihood | Impact | Mitigation / fallback |
|---|------|-----------|--------|-----------------------|
| R1 | **Host-side OAuth refresh acceptance** — the token endpoint accepting a refresh performed by the host, off-device (refresh grant has no PKCE, but tokens may be binding/attestation-bound). Existential for both candidates. | Medium | High | **Spike S-A first**, throwaway account. If rejected, the whole approach needs rethink (e.g. API-key-only). |
| R2 | **Provider ToS / personal-subscription abuse-detection** — server-side refresh + injection from a host IP tripping automation/sharing heuristics → account flag/ban (worse than the theft prevented). | Medium | High | **Spike S-B first**, throwaway account + read ToS. May restrict this path to API-key/commercial tiers. |
| R3 | **Inference acceptance of a host-supplied subscription Bearer** (the easier half of auth). | Low–Med | High | V1. Precedent: the `CLAUDE_CODE_OAUTH_TOKEN` env path is an externally-supplied subscription Bearer the inference endpoint accepts. |
| R4 | **Proxy streaming/lifecycle correctness** — TLS-terminate + SSE + WS + backpressure + reuse + fail-closed, on every call in the hot path; a bug degrades all output. | Medium | High | Standard reverse-proxy patterns + dedicated streaming tests; primary engineering budget. Proxy candidate only. |
| R5 | **No egress allow-list** — "bounded abuse"/"fail-closed-for-usage" require a default-deny allow-list pinning guest→proxy; absent today. | High (absent) | Medium | Build it as M1 work; until then mark those properties advisory. Containment itself is unaffected. |
| R6 | **CA private-key custody / blast radius** (proxy) — a leaked CA key impersonates sites to the guest. | Low | High | **Per-run ephemeral CA**, generated at boot, destroyed on teardown; **Name-Constrained** to the injected upstreams; never expose the key to the guest. |
| R7 | **SSRF / confused-deputy** via the credential-injecting proxy (acute in M2). | Medium | Medium–High | Exact host+path injection match; no credentialed redirects; no agent-controlled `Host`. |
| R8 | **codex WebSocket transport** vs header injection. | Low | Low–Med | Default to `supports_websockets=false` (plain HTTPS); inject-on-upgrade as optional. |
| R9 | **Provider version drift** changing redirect/refresh/header/transport behavior. | Low | Low–Med | Pin versions; re-verify R1/R3/redirect facts on bump (bump workflow gates it). |
| R10 | **M2 transparent per-destination cert generation.** | Low | Low | Deferred; explicit `HTTPS_PROXY` avoids it; established SSL-bump pattern. |

## Validation order and implementation plan

Front-loaded so the existential unknowns retire before any milestone work.

**S-A — host-side refresh feasibility (gate for everything).** On a throwaway
account, have a host process replay the OAuth refresh (`client_id` +
`refresh_token` + `grant_type=refresh_token`) for Claude and codex and confirm the
token endpoint returns a usable access token. Resolves R1. If it fails, stop and
re-scope.

**S-B — provider ToS posture (gate for everything).** On a throwaway account,
exercise sustained proxied/server-refreshed subscription use and read each
provider's ToS for programmatic/proxied subscription auth. Resolves R2. If
prohibitive, restrict to API-key/commercial auth.

**S-C — redirect/suppress assumptions on the pinned versions.** Point the pinned
clients at a dumb logging proxy; confirm `ANTHROPIC_BASE_URL`/`openai_base_url`
redirect, `PROVIDER_MANAGED_BY_HOST` suppresses refresh/force-login, the CA env
vars are honored, and only inference blocks (no retry-storm on the other Claude
endpoints).

**V1 — inference acceptance.** Inject a host-minted Bearer on one real inference
request per provider; confirm a usable completion (R3).

**Mode decision point.** With S-A/S-B/S-C/V1 green, choose token injection vs.
proxy per the decision criteria.

**Then build, split into smaller milestones:**
- **M0** (if containing API keys) — static-key proxy for the **API-key** providers
  (Anthropic `x-api-key`, OpenAI/codex Bearer, Custom). No credential store,
  refresh, or rotation — the host holds the static key and the proxy injects it.
  **Ungated by S-A/S-B and by the OAuth mode decision** (the proxy is the only
  containment for a static key); needs only S-C (redirect + CA trust) plus the
  per-run name-constrained CA and the streaming proxy. The lowest-risk proof of the
  proxy spine, so it can lead.
- **M1a** — credential store (refresh/mint/rotation) + the chosen provider
  mechanism for **Claude only**, single platform, inference path. Proves the spine.
  If proxy: reuses M0's CA + streaming proxy (R4/R6).
- **M1b** — codex (WS handling), long-run rotation, and KVM+VZ parity.
- **Egress allow-list** (R5) — default-deny, guest pinned to the proxy (if proxy
  chosen) — so the bounded-abuse/fail-closed properties become real.
- **M2/M3** — downstream egress (proxy), per-destination policy, SSRF hardening
  (R7), transparent form (R10).

Remove the RW credential mounts and the `src/agent_box.rs:464` WriteFile copy as
each provider migrates. Exit gate: `e2e_agent_mcp` (Claude) and the codex smoke
specs pass with no refresh token in the guest.

## Affected code

- New host modules — the credential store (OAuth refresh/mint/rotation) and, if
  the proxy is chosen, the injection proxy (TLS-terminating, header-rewriting,
  streaming) plus the per-run CA.
- `src/credentials.rs` — host-retained OAuth credential, host-side refresh/
  rotation, short-lived-token minting; staging is host-only.
- `src/runtime.rs` (`~234`, `~1352-1408`), `src/agent_box.rs` (`~464`) — remove the
  RW mount and the WriteFile copy; provision the guest per the chosen mechanism.
- `src/llm.rs` — provider → store/proxy/injector wiring, replacing the API-key
  env forwarding in `env_vars()` (`~417`; `ANTHROPIC_API_KEY`/`OPENAI_API_KEY`/
  Custom key) with host-side injection.
- `src/network/nat.rs` / `src/network/slirp.rs` — the default-deny egress
  allow-list (R5) and the M2 transparent interception point.
- Guest image — install the per-run CA into the trust stores the clients honor
  (`NODE_EXTRA_CA_CERTS`; `CODEX_CA_CERTIFICATE`/`SSL_CERT_FILE`; and the system
  store for M2 tools) — a per-image checklist across initramfs and OCI-rootfs.
- `scripts/agents/manifest.toml` — the pinned versions the behaviors are verified
  against.
