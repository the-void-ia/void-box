# Design: on-demand credential broker for the guest agent

Status: **proposal** (no code yet). Owner: runtime. Target branch:
`claude/vm-agent-credentials-syz9c2`. This revision incorporates the project's
STRIDE security review, whose top residual finding is credential exfiltration:
a prompt-injected or compromised in-guest agent can read the OAuth **refresh**
tokens we mount today and take over the operator's provider account for the
lifetime of that token. Containing that is the headline requirement here, not a
nice-to-have generalization.

## Problem

The agent running inside the VM needs credentials — the LLM provider's API key
or OAuth tokens, but increasingly also the *task's* own secrets: a GitHub token
to push, an npm/cargo registry token, cloud keys, an SSH key, a database URL,
the OpenClaw gateway's Telegram token. Today three uncoordinated paths carry
that material into the guest, none of them general and all of them leaving the
secret **at rest** inside the sandbox for the whole run — including, for the
`claude-personal` and `codex` providers, the long-lived refresh token.

We want one mechanism that is:

- **secure by construction** — for the high-value case (OAuth tokens) the
  refresh token never crosses into the guest at all; the guest only ever holds
  a short-lived access token whose blast radius is time-bounded. For other
  secrets, the default delivery keeps them off the guest filesystem, out of the
  exec environment, out of `/proc/<pid>/environ`, and out of snapshots; the
  host stays the single point of allow-listing, auditing, and revocation;
- **flexible** — any secret, from any host-side source, for any consumer
  (LLM runtime, task tooling, MCP servers, skills), not just the two LLM
  providers wired today;
- **decoupled** — adding a new source (Vault, 1Password, a host command)
  or a new consumer must not touch the call sites that deliver credentials.

The direction is **on-demand delivery via a host-side credential broker**: the
host hands material to the guest only when the guest asks, over the existing
authenticated host↔guest channel, and — for OAuth — performs token refresh
host-side so the refresh token is structurally absent from the guest.

## Goals / non-goals

Goals:

1. **OAuth refresh tokens never enter the guest.** For `claude-personal` and
   `codex`, the host holds the refresh token, performs refresh against the
   provider, and returns only a short-lived access token with an explicit
   expiry. This is the primary, must-ship goal.
2. A single declarative credential model (`CredentialSpec`) covering *what*
   secret is needed, *where* it comes from on the host, and *how* the guest
   receives it — with source and delivery as orthogonal, open sets — so the
   OAuth case and ordinary task secrets share one pipeline.
3. A host-side broker that resolves/refreshes at request time and returns over
   the vsock control channel, so the default delivery never writes the secret
   to guest-resident state, and **fails closed** when unavailable.
4. Make LLM-provider auth an *instance* of this model — data, not branching
   code — collapsing the duplicated staging in `src/credentials.rs` and the
   hardcoded env in `src/llm.rs`.
5. Per-name allow-list + audit log + rate limit on the host; opt-in, no
   implicit behavior, consistent with the snapshot/messaging design principles
   in `AGENTS.md`.

Non-goals (for this proposal):

- **Preventing use of a leaked access token during its validity window.** A
  compromised agent that copies its short-lived access token and uses it from
  outside the VM before expiry is expected to succeed. The broker bounds blast
  radius *by time*; it does not make a live access token unusable.
- Per-process isolation *inside* the guest. The guest is one trust domain;
  anything in it that can reach the channel can request an allow-listed
  credential. The broker's value is "refresh token absent / not at rest" +
  "host-mediated," not intra-guest compartmentalization. (A capability-token
  extension is sketched under Residual risks.)
- A general host-side secret manager. The broker *reads from* and *refreshes
  against* managers and providers; it is not one.

## Threat model and why on-demand

The sandbox boundary protects the host *from* the guest. Credentials invert
that: we hand the guest something valuable, so the question is blast radius if
the guest workload (or a dependency it pulls, or a prompt injection it obeys)
turns hostile.

At-rest delivery (today) means the credential is readable for the whole run by
any process in the guest (exec env / `/proc/<pid>/environ`), anything that can
read the mounted credential file, **any snapshot** taken during the run (the
secret is serialized into guest RAM and persists in the snapshot image on disk,
`src/vmm/snapshot.rs`), and crash/core dumps inside the guest. For OAuth the
mounted file holds the **refresh** token — so exfiltration is not bounded by the
run: it grants account access until the operator manually rotates.

On-demand delivery, plus host-side refresh for OAuth:

- removes the refresh token from the guest entirely — it is never written, never
  resident, never snapshot-captured;
- bounds a leaked access token to its short upstream lifetime;
- removes ordinary secrets from snapshots (never resident when no request is in
  flight) and gives the host a chokepoint to allow-list names, rate-limit, and
  audit every access.

Note the **new host-side attack surface** this introduces: the broker makes the
host parse guest-initiated requests. Until now the guest→host traffic is
essentially telemetry; a guest-driven credential RPC is exactly the
"arbitrary-code-in-guest attacking host backends" adversary. The broker handler
must treat every request field as untrusted, bound allocation, and be a fuzz
target alongside the existing virtio/multiplex parsers (see Security
properties).

## Use-case taxonomy

The model must cover four orthogonal axes. Every existing and anticipated case
is a point in this space.

| Axis | Values |
|---|---|
| **Consumer** | LLM runtime · task tooling (git/npm/aws/ssh) · in-guest MCP server / skill · sidecar bridge |
| **Source** | managed OAuth (host refreshes) · host env var · host file · OS keychain (macOS today; Linux Secret Service later) · spec literal · host command stdout · external manager (Vault / 1Password / AWS-SM) |
| **Delivery** | broker pull (default) · short-lived access-token file (for path-bound CLIs) · exec env (compat, non-secret or low-value only) |
| **Lifetime** | static for run · host-refreshed short-lived access token (≤ provider default) · minted per request |

Worked examples:

- *Claude personal (OAuth)* — **priority case.** Source = managed OAuth; the
  host keeps `~/.claude/.credentials.json` (refresh token included) and
  performs refresh. The guest's `claude` process obtains an access token via
  the broker (env-on-demand shim or access-token file). The refresh token is
  never delivered.
- *Codex (OAuth)* — same. Codex expects `~/.codex/auth.json` on disk, so this
  is the path-bound variant: the guest-side shim writes an access-token-only
  `auth.json` (no `refresh_token` key) just before launch and refreshes it in
  place by re-asking the broker before expiry. Which exact shape codex
  tolerates is the open decision below.
- *Claude API key (`claude`)* — source = host env `ANTHROPIC_API_KEY`;
  delivery = broker pull via the env-on-demand shim, so the key lives in one
  process, not the run-wide env or a pre-launch snapshot. (Static, not
  refreshed.)
- *GitHub push token* — source = host env or `gh auth token` (command);
  delivery = broker pull behind a git credential helper that calls the broker
  at network-use time.
- *Vault-issued DB password* — source = Vault (future plugin); minted per
  request; broker pull each time the task opens a connection.

## Current state (what we unify)

| Path | Mechanism | Code |
|---|---|---|
| LLM provider env | `LlmProvider::env_vars()` hardcodes passthrough + injected base-url/token per provider | `src/llm.rs:417` |
| Spec env | `SandboxSpec.env` map, `$VAR` host expansion | `src/spec.rs:52`, `src/runtime.rs:1262` |
| OAuth file staging | `discover_*` → 0600 tempdir → **RW mount of the refresh-token file** at a fixed guest path; also a privileged `WriteFile`-RPC copy for service/agent-mode claude-personal; Claude and Codex are near-duplicate copies | `src/credentials.rs`, `src/runtime.rs:234,1399`, `src/agent_box.rs:464` |

All three become *delivery instances* of `CredentialSpec`. The OAuth staging in
particular is replaced — not merely re-expressed — by the managed-OAuth flow,
which is the whole point.

## Proposed architecture

```
┌────────────────────────── host ──────────────────────────┐
│  CredentialSpec[]  ──►  CredentialResolver                │
│       (provider-required + spec.credentials)              │
│                              │                            │
│                     ┌────────▼─────────┐  resolve / OAuth- │
│                     │ CredentialBroker │  refresh at        │
│                     │  - allow-list    │  request time;     │
│                     │  - rate limit    │  refresh token      │
│                     │  - audit log     │  stays here         │
│                     └────────┬─────────┘                  │
│                              │ vsock control channel      │
└──────────────────────────────┼───────────────────────────┘
                               │  (authenticated, multiplexed)
┌──────────────────────────────▼─── guest ─────────────────┐
│  void-cred shim / git helper / MCP / skill (uid 1000)     │
│   CredentialRequest{name} ─► CredentialResponse           │
│                               {access_token, expires_at}  │
│   access token only; lives in the requesting process /     │
│   a 0600 file the shim writes and refreshes before expiry  │
└───────────────────────────────────────────────────────────┘
```

### 1. The declarative model

```rust
/// One credential the guest may obtain. Source and delivery are independent.
pub struct CredentialSpec {
    /// Logical name: the broker key, and the env-var name / file basename
    /// the guest expects.
    pub name: String,
    pub source: CredentialSource,
    pub delivery: CredentialDelivery,
    /// If true and the source is absent, the credential is simply omitted
    /// (today's soft-fail for codex). If false, absence is fatal at resolve
    /// time. Independent of broker availability, which always fails closed.
    pub optional: bool,
}

pub enum CredentialSource {
    /// Host holds the full OAuth credential (incl. refresh token) and exchanges
    /// it for short-lived access tokens against the provider. The refresh token
    /// never leaves the host. This is the variant the security review requires
    /// for `claude-personal` and `codex`.
    ManagedOAuth { provider: OAuthProvider },
    HostEnv { var: String },
    HostFile { path: PathBuf },
    OsKeychain { service: String, account: Option<String> },
    Command { program: String, args: Vec<String> },
    Literal(SecretString),
    // future, behind the same enum — no call-site changes:
    // Vault { .. }, AwsSecretsManager { .. }, OnePassword { .. }
}

pub enum CredentialDelivery {
    /// Default. Held on the host; returned over vsock on request, keyed by name.
    /// For `ManagedOAuth`, the response carries an access token + expiry only.
    Broker,
    /// A short-lived 0600 file the *uid-1000* guest shim writes just before the
    /// consumer starts (never the host via a privileged WriteFile), removes on
    /// exit, and — for `ManagedOAuth` — rewrites with a refreshed access token
    /// before `expires_at`. `key_allowlist` names exactly which JSON keys may
    /// appear, so a refresh-token key can never be written.
    File { guest_path: PathBuf, mode: u32, key_allowlist: Vec<String> },
    /// Injected into the exec env. Compat shape; restricted to non-secret or
    /// low-value values (base URLs, placeholder tokens). Not for OAuth.
    Env,
}
```

Every value is wrapped through the `secrecy` crate (`SecretString`), as
`ApiKey` and the staged credentials already are, so it auto-redacts in `Debug`
and zeroizes on drop. The existing `is_sensitive_env_key` redaction in
`ExecRequest::Debug` (`void-box-protocol/src/lib.rs:454`) still backstops the
`Env` shape.

### 2. Managed OAuth — the priority flow

This is the part that closes the headline finding. For `claude-personal` and
`codex`:

1. The host discovers the full OAuth credential as today (`src/credentials.rs`)
   but **does not stage the refresh token into the guest**. It keeps it
   host-side, wrapped in `SecretString`, keyed by run.
2. On a guest `CredentialRequest`, the broker returns a **short-lived access
   token plus an `expires_at`**. If the host's cached upstream token has
   > the ceiling remaining, it returns it; otherwise it refreshes against the
   provider and returns the fresh one.
3. **Expiry ceiling (blast-radius bound, not ergonomics).** The broker never
   promises the guest an `expires_at` beyond `now + CEIL`, where `CEIL` is at
   or below the provider's default access-token lifetime (~60 min for both
   Anthropic and OpenAI today). A signed JWT's internal `exp` can't be
   rewritten, so the ceiling is enforced behaviorally on the broker's promised
   `expires_at`; the guest shim trusts that field, not the token's claims, and
   re-requests before it. Staying at/below the provider default means the
   ceiling never changes operational behavior — long runs already
   re-authenticate at that cadence.
4. **Refresh-token absence is a tested invariant.** Any `File` delivery for a
   managed-OAuth credential declares a `key_allowlist`; the shim writes only
   those keys. No key whose name contains "refresh" (case-insensitive) may ever
   appear in the guest-visible file. This is verified independently of the
   expiry check.
5. **Fail closed.** If the broker is unreachable or refuses, the consumer's
   credential acquisition fails and the run errors — there is no silent
   fallback to mounting the refresh-token file.

`OAuthProvider` encapsulates the per-provider refresh endpoint and token shape,
so adding a provider is a new arm there, not in the broker or transport.

### 3. Provider auth as data

`LlmProvider::env_vars()` and the `prepare_claude_personal` / `prepare_codex` /
`apply_codex_credential_mount` functions are replaced by one method:

```rust
impl LlmProvider {
    fn required_credentials(&self) -> Vec<CredentialSpec> { /* table */ }
}
```

- `claude-personal` → `ManagedOAuth{Anthropic}` with `Broker` delivery (the
  CLI reads `ANTHROPIC_API_KEY`/credentials from env or file; the shim supplies
  an access token).
- `codex` → `ManagedOAuth{OpenAI}` with `File{ guest:"~/.codex/auth.json",
  key_allowlist: [access-token keys only] }`, plus an optional `HostEnv` for
  `OPENAI_API_KEY` (api-key mode).
- `claude` (API key) → `HostEnv{ANTHROPIC_API_KEY}` with `Broker` delivery.
- Ollama / LM-Studio / custom → `Literal`/`HostEnv` with `Env` delivery (their
  values are non-secret base URLs and placeholder tokens, so broker delivery
  buys nothing).

A new provider adds a table row, not a `match` arm in two files.

### 4. The broker and its transport

The broker is a host-side service holding the resolved `CredentialSpec` set,
the host-retained OAuth credentials, an allow-list of obtainable names, a
per-name rate limit, and an audit log. It answers guest requests by
resolving/refreshing the source *then* and returning the access token or secret.

**Transport: extend the vsock control channel**, not the sidecar HTTP server:

- The control channel exists for *every* run, is authenticated by the Ping/Pong
  session-secret handshake, and is multiplexed (`src/backend/multiplex.rs`).
- The sidecar (`src/sidecar/server.rs`) only runs when `messaging.enabled`,
  has **no auth on any route**, and on macOS binds `UNSPECIFIED` (every host
  interface, per `guest_accessible_bind_addr`) — the security review flags it
  as reachable by any local/LAN process. Routing secret traffic through it
  would inherit that exposure.
- Telemetry already shows guest→host-initiated frames
  (`MessageType::TelemetryData`), so guest-initiated requests are not a new
  direction in principle — but see the caller/responder inversion below.

The auth is whatever the channel uses; the design must **not** hard-code the
current static cmdline secret, because that secret is itself a known weakness
(visible via `/proc/<pid>/cmdline`) slated to be replaced by a per-boot
handshake-derived key. The broker inherits that improvement for free as long as
it reuses the channel's established session rather than re-reading the cmdline.

The one real cost: today the host is the multiplex *caller* and the guest the
responder. A guest-initiated request inverts roles. Two options, settled in the
plan:

- **(a) reverse RPC on the existing channel** — teach the multiplex reader on
  both ends to dispatch peer-initiated requests (guest allocates the
  `request_id`, host registers a handler). Tidiest long-term; touches
  `multiplex.rs` and `guest-agent` dispatch.
- **(b) a dedicated broker vsock port** — guest connects out to a host-listening
  port, same handshake. Smaller blast radius, simpler to reason about and to
  fuzz in isolation; one more listener.

Wire additions (append-only, matching the protocol's append-only discipline):

```text
MessageType::CredentialRequest  = 28
  // { name: String }                       -- guest asks by logical name
MessageType::CredentialResponse = 29
  // { status: ok|denied|unavailable|expired,
  //   secret?: bytes,                      -- access token / secret value
  //   expires_at?: u64,                    -- unix secs; broker-promised ceiling
  //   error?: String }
```

`MAX_MESSAGE_SIZE` bounds payloads; the handler additionally bounds `name`
length and validates it against the allow-list before any work. Responses are
JSON like the rest of the protocol; secret bytes are zeroized on the guest side
after use. `unavailable`/`denied` are terminal for the consumer (fail closed).

### 5. Guest-side integration (`void-cred`)

A small guest binary, `void-cred`, runs as uid 1000 and is the only component
that speaks the broker protocol; consumers never embed it. It is the same
mechanism the broader plan to demote privileged file RPCs to uid 1000 wants —
no PID-1/root involvement in credential materialization.

- **env-on-demand** — `void-cred exec --name ANTHROPIC_API_KEY -- claude …`
  requests the token, sets it in the child's env, and `exec`s. The token lives
  in exactly one process, never in the run-wide env or a pre-launch snapshot.
- **access-token file** — for `File` delivery, `void-cred` (uid 1000) writes a
  0600 file containing only `key_allowlist` keys. Before `expires_at` it
  **re-requests a fresh access token from the broker over vsock** and overwrites
  the file; it removes the file on exit. `void-cred` never performs the OAuth
  exchange and never sees the refresh token — "refreshing the file" means
  "ask the broker again and rewrite," not "exchange a refresh token." The
  refresh-token→access-token exchange against the provider happens only on the
  host, inside the broker (§2). This **replaces** both the whole-run RW mount
  and the privileged `WriteFile`-RPC credential copy (`src/agent_box.rs:464`),
  which are deleted once both providers are migrated.
- **helper protocols** — a git credential helper / npm token script / MCP
  server config that shells out to `void-cred get --name …` at the moment of
  network use.

### 6. Source plugins

`CredentialSource` resolution lives behind a `trait SourceResolver { fn
resolve(&self) -> Result<Issued>; }` (where `Issued` carries the secret and an
optional `expires_at`) with one impl per variant. `discover_oauth_credentials`
and `discover_codex_credentials` feed the `ManagedOAuth` resolver (which also
owns refresh); a future Vault impl is a new variant; the broker, transport, and
guest are untouched.

### 7. Spec surface

A new opt-in block lets the task declare its own (non-LLM) secrets declaratively
instead of plaintext `env:`:

```yaml
sandbox:
  credentials:
    - { name: GITHUB_TOKEN, from: { env: GH_PAT } }            # delivery defaults to broker
    - { name: NPM_TOKEN,    from: { command: { program: gh, args: [auth, token] } } }
    - name: id_ed25519
      from: { file: ~/.ssh/id_ed25519 }
      to:   { file: /home/sandbox/.ssh/id_ed25519, mode: 0600 }
```

`BoxSandboxOverride` gets the same field for per-box scoping in pipelines,
mirroring how `env`/`mounts` are already overridable (`src/spec.rs:152`).
Absent the block, behavior is identical to today.

## Security properties

- **No refresh token in the guest.** For OAuth, the refresh token stays
  host-side; the guest only ever holds a short-lived access token. Enforced
  structurally (managed-OAuth never delivers it) and by a `key_allowlist` test
  that fails if any "refresh"-named key appears in a guest-visible file.
- **Time-bounded blast radius.** A leaked access token is unusable after
  `expires_at` ≤ `now + CEIL` (≤ provider default).
- **Fail closed.** Broker unavailable/denied ⇒ the run errors; never a silent
  fall back to mounting the credential.
- **Not at rest by default.** Non-OAuth broker delivery keeps secrets out of the
  guest env, fs, `/proc`, and snapshots; they exist only transiently in the
  requesting process.
- **Host chokepoint.** Per-name allow-list, **per-name rate limit**, and an
  audit line per request (name, time, status). The guest cannot enumerate
  beyond the allow-list.
- **Untrusted-input handling on the host.** The broker handler treats every
  request field as hostile (the guest may be running arbitrary code): bounded
  `name`, allow-list check before any source work, no unbounded allocation. It
  is a fuzz target alongside the virtio/multiplex parsers; option (b)'s
  dedicated listener keeps that surface isolated.
- **Auth inherits the channel.** Reuses the established session, not a re-read
  of the cmdline secret, so it composes with the planned move to a
  handshake-derived session key.
- **Redaction + zeroize** end to end via `secrecy`.
- **Opt-in.** No broker traffic or `credentials:` semantics unless configured.

## Residual risks

- **Live access-token use during its window.** Explicitly out of scope — the
  bound is temporal, not preventive (see Non-goals).
- **Intra-guest sharing.** Any guest process reaching the channel can request an
  allow-listed name. Mitigation path (future): a per-consumer capability token
  the host mints and hands to a single launched process, required in
  `CredentialRequest`. Noted so the wire format leaves room.
- **Local host user with the session secret.** On Linux the vsock rendezvous
  socket is `0o600`, so a foreign uid cannot reach the channel; a same-uid
  process is already equivalent to the operator. The broker doesn't widen this,
  and the planned handshake-derived key removes the cmdline-secret leak that
  makes it theoretically reachable.
- **Access-token file window.** The short-lived file is a brief at-rest window
  for path-bound CLIs, uid-1000 and 0600, refresh-token-free — strictly better
  than today's whole-run refresh-token mount.

## Coverage of the security-review findings

The review's credential-exfiltration item (its highest-priority hardening) and
its acceptance criteria map onto this design as follows:

| Review criterion | Where addressed |
|---|---|
| Refresh tokens never enter the guest | §2 managed OAuth; `ManagedOAuth` source; `key_allowlist` |
| Host performs refresh; returns only access token + expiry | §2; `SourceResolver`/`Issued`; `CredentialResponse.expires_at` |
| ≤ provider-default (~60 min) ceiling, re-request before expiry | §2 step 3; guest shim trusts `expires_at` |
| Fail closed, no silent mount fallback | §2 step 5; Security properties |
| Remove RW mount **and** privileged `WriteFile` copy | §5 access-token file; §"Current state" deletion note |
| New vsock RPC, auth reuses session | §4 wire additions + auth note |
| Rate limits | §4 broker; Security properties |
| Per-provider allowed-key list for guest files | `key_allowlist`; §2 step 4 |
| Don't use the unauthenticated sidecar | §4 transport rationale |
| Codex clean-injection vs shim-intercept | Open question 1 |

Adjacent review items this design also touches: it does **not** widen the host
process's attack surface uncontrolled — it adds one bounded, fuzzable handler
(Security properties) — and it composes with, rather than depends on, the
planned replacement of the static cmdline session secret.

## Open questions

1. **Codex token shape (blocks the codex slice).** Does current codex tolerate
   an `auth.json` containing only an access token (clean injection), or must we
   intercept its in-process refresh (shim-intercept)? The first is far cheaper;
   the answer decides the guest-side surface for codex. Both branches keep the
   refresh token host-side.
2. **Transport:** reverse-RPC on the existing channel (a) vs. a dedicated broker
   vsock port (b)? Leaning (b) for blast-radius isolation and isolated fuzzing.
3. **Env delivery:** keep it as a documented, lower-security shape for
   non-secret values, or deprecate once the env-on-demand shim lands?
4. **macOS/VZ parity:** the broker is transport-only and platform-neutral, but
   `void-cred` and the access-token-file path must be validated on both KVM and
   VZ (`e2e_mount`, `e2e_agent_mcp` patterns).

## Phasing

- **P1 — managed-OAuth broker (closes the headline finding).** `CredentialSpec`
  /`Source`/`Delivery` with `ManagedOAuth`, the broker over vsock (transport
  chosen in the plan), host-side refresh + expiry ceiling, `void-cred` with
  env-on-demand + access-token file, fail-closed. Migrate `claude-personal` and
  `codex`; delete the RW mount and the privileged `WriteFile` credential copy.
  Regression gate: `e2e_agent_mcp` (Claude) and the codex smoke specs still
  authenticate; new tests assert no refresh-token key and ≤ceiling expiry in the
  guest-visible credential.
- **P2 — unify the rest.** Re-express the API-key/env and provider-injected
  values through the model; collapse `env_vars()` and the prepare-helpers.
- **P3 — task secrets.** `sandbox.credentials:` + `BoxSandboxOverride`,
  git/npm/MCP helper shapes.
- **P4 — pluggable sources.** `Command`, Linux Secret Service, external managers
  behind `SourceResolver`.
- **P5 — capability tokens** (optional) for intra-guest scoping.

## Affected code (for the implementation plan)

- `void-box-protocol/src/lib.rs` — `MessageType` 28/29 + request/response types
  (with `expires_at`).
- `src/backend/multiplex.rs`, `src/backend/control_channel.rs`,
  `guest-agent/src/main.rs` — transport for guest-initiated broker RPC.
- `src/credentials.rs` — becomes `SourceResolver`/`ManagedOAuth` impls (refresh
  logic, expiry); staging shrinks to the host-retained credential.
- `src/runtime.rs` (`~234`, `~1352-1408`), `src/agent_box.rs` (`~464`) — remove
  RW mount + privileged `WriteFile` credential copy; wire the broker.
- `src/llm.rs` — `required_credentials()` replaces `env_vars()` + the
  provider-prepare helpers.
- `src/spec.rs` — `CredentialSpec` YAML + `SandboxSpec`/`BoxSandboxOverride`
  fields.
- new `void-cred/` crate (guest, uid 1000) + `DEFAULT_COMMAND_ALLOWLIST`
  (`src/backend/mod.rs`).
