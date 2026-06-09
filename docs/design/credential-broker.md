# Design: on-demand credential broker for the guest agent

Status: **proposal** (no code yet). Owner: runtime. Target branch:
`claude/vm-agent-credentials-syz9c2`.

## Problem

The agent running inside the VM needs credentials — the LLM provider's API
key or OAuth token, but increasingly also the *task's* own secrets: a GitHub
token to push, an npm/cargo registry token, cloud keys, an SSH key, a database
URL, the OpenClaw gateway's Telegram token. Today three uncoordinated paths
carry that material into the guest, none of them general and all of them
leaving the secret **at rest** inside the sandbox for the whole run.

We want one mechanism that is:

- **flexible** — any secret, from any host-side source, for any consumer
  (LLM runtime, task tooling, MCP servers, skills), not just the two LLM
  providers wired today;
- **decoupled** — adding a new source (Vault, 1Password, a host command)
  or a new consumer must not touch the call sites that deliver credentials;
- **secure by construction** — the chosen primary delivery keeps the secret
  off the guest filesystem, out of the exec environment, out of
  `/proc/<pid>/environ`, and out of snapshots; the host stays the single
  point of allow-listing, auditing, and revocation.

The selected direction (see the scoping decision that produced this doc) is
**maximum security via on-demand delivery**: a host-side *credential broker*
that hands a secret to the guest only when the guest asks for it, over the
existing authenticated host↔guest channel, and never persists it in the guest.

## Goals / non-goals

Goals:

1. A single declarative credential model (`CredentialSpec`) that describes
   *what* secret is needed, *where* it comes from on the host, and *how* the
   guest receives it — with source and delivery as orthogonal, open sets.
2. A host-side broker that resolves a secret at request time and returns it
   over the vsock control channel, so the default delivery never writes the
   secret to guest-resident state.
3. Make LLM-provider auth (Claude, Codex, custom, …) an *instance* of this
   model — data, not branching code — collapsing the duplicated staging in
   `src/credentials.rs` and the hardcoded env in `src/llm.rs`.
4. Per-name allow-list + audit log on the host; opt-in, no implicit behavior,
   consistent with the snapshot/messaging design principles in `AGENTS.md`.

Non-goals (for this proposal):

- Per-process isolation *inside* the guest. The guest is one trust domain;
  anything in it that can reach the channel can request an allow-listed
  secret. The broker's value is "not at rest" + "host-mediated," not
  intra-guest compartmentalization. (A future capability-token extension is
  sketched under Residual risks.)
- Replacing file-shaped delivery outright. Third-party CLIs that insist on
  reading `$HOME/.codex/auth.json` still need a real file; the broker
  *materializes* it just-in-time rather than mounting it for the whole run.
- A general host-side secret manager. The broker *reads from* managers; it is
  not one.

## Threat model and why on-demand

The sandbox boundary protects the host *from* the guest. Credentials invert
that: we are handing the guest something valuable, so the question is blast
radius if the guest workload (or a compromised dependency it pulls) turns
hostile or simply leaks.

At-rest delivery (today) means the secret is readable for the entire run by:

- any process in the guest via the exec env or `/proc/<pid>/environ`;
- anything that can read the mounted credential file;
- **any snapshot** taken during the run — the secret is serialized into guest
  RAM and persists in the snapshot image on disk (see `src/vmm/snapshot.rs`);
- crash dumps / core files inside the guest.

On-demand delivery shrinks the window to "between request and use," removes the
secret from snapshots entirely (it is never resident when no request is in
flight), and gives the host a chokepoint to allow-list which names are
obtainable and to log every access. The kernel cmdline is already avoided for
app credentials (only the vsock session secret rides there,
`src/backend/mod.rs:235`); the broker keeps it that way.

## Use-case taxonomy

The model must cover four orthogonal axes. Every existing and anticipated case
is a point in this space.

| Axis | Values |
|---|---|
| **Consumer** | LLM runtime · task tooling (git/npm/aws/ssh) · in-guest MCP server / skill · sidecar bridge |
| **Source** | host env var · host file · OS keychain (macOS today; Linux Secret Service later) · spec literal · host command stdout · external manager (Vault / 1Password / AWS-SM) |
| **Delivery** | broker pull (default) · just-in-time file · exec env (compat) |
| **Lifetime** | static for run · refreshable (codex rewrites `auth.json`) · short-lived / minted per request |

Worked examples:

- *Claude API key*: source = host env `ANTHROPIC_API_KEY`; consumer = LLM
  runtime; delivery = broker pull (the Claude CLI reads `ANTHROPIC_API_KEY`
  from its env — a thin shim requests it from the broker and `exec`s the CLI
  with it set, so it lives only in that process, not the whole run).
- *Codex OAuth*: source = host file `~/.codex/auth.json`; lifetime =
  refreshable; delivery = just-in-time file (codex demands the path and
  rewrites it). The broker writes it on first request and can collect the
  refreshed copy back to the host on teardown.
- *GitHub push token*: source = host env or `gh auth token` (command);
  consumer = task tooling; delivery = broker pull, materialized into a
  `git` credential helper that calls the broker.
- *Vault-issued DB password*: source = Vault (future plugin); lifetime =
  short-lived; delivery = broker pull each time the task opens a connection.

## Current state (what we unify)

| Path | Mechanism | Code |
|---|---|---|
| LLM provider env | `LlmProvider::env_vars()` hardcodes passthrough + injected base-url/token per provider | `src/llm.rs:417` |
| Spec env | `SandboxSpec.env` map, `$VAR` host expansion | `src/spec.rs:52`, `src/runtime.rs:1262` |
| OAuth file staging | `discover_*` → 0600 tempdir → RW mount at fixed guest path; Claude and Codex are near-duplicate copies | `src/credentials.rs`, `src/runtime.rs:234` |

All three become *delivery instances* of `CredentialSpec`. The provider
`match` arms in `llm.rs` and the two staging functions in `credentials.rs`
collapse into data + one resolver.

## Proposed architecture

```
┌────────────────────────── host ──────────────────────────┐
│  CredentialSpec[]  ──►  CredentialResolver                │
│       (provider-required + spec.credentials)              │
│                              │                            │
│                     ┌────────▼─────────┐   reads at       │
│                     │ CredentialBroker │──► request time  │
│                     │  - allow-list    │   from Source    │
│                     │  - audit log     │   plugins        │
│                     └────────┬─────────┘                  │
│                              │ vsock control channel      │
└──────────────────────────────┼───────────────────────────┘
                               │  (authenticated, multiplexed)
┌──────────────────────────────▼─── guest ─────────────────┐
│  void-cred shim / git helper / MCP / skill                │
│    CredentialRequest{name} ──► CredentialResponse{secret} │
│    secret lives only in the requesting process            │
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
    /// If false, a missing source is fatal at resolve time; if true, the
    /// credential is simply absent (matches today's soft-fail for codex).
    pub optional: bool,
}

pub enum CredentialSource {
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
    Broker,
    /// Materialized into a 0600 file at `guest_path` on first request
    /// (for CLIs that read a fixed path). `writable` mirrors codex's refresh.
    File { guest_path: PathBuf, writable: bool, mode: u32 },
    /// Injected into the exec env. Backwards-compat shape; least secure.
    Env,
}
```

Every value is wrapped through the `secrecy` crate (`SecretString`), as
`ApiKey` and the staged credentials already are, so it auto-redacts in
`Debug` and zeroizes on drop. The existing `is_sensitive_env_key` redaction
in `ExecRequest::Debug` (`void-box-protocol/src/lib.rs:454`) still backstops
the `Env` shape.

### 2. Provider auth as data

`LlmProvider::env_vars()` and the `prepare_claude_personal` / `prepare_codex`
/ `apply_codex_credential_mount` functions are replaced by one method:

```rust
impl LlmProvider {
    fn required_credentials(&self) -> Vec<CredentialSpec> { /* table */ }
}
```

For example Codex becomes two specs — a `HostFile` → `File{writable:true}`
for `auth.json` (optional) and a `HostEnv` → `Broker`-or-`Env` for
`OPENAI_API_KEY` (optional) — exactly today's behavior, expressed as data.
Ollama/Lm-Studio/custom become `Literal` and `HostEnv` specs with `Env`
delivery (their values are non-secret base URLs and placeholder tokens, so
broker delivery buys nothing). This is the decoupling: a new provider adds a
table row, not a `match` arm in two files.

### 3. The broker and its transport

The broker is a host-side service holding the resolved `CredentialSpec` set
plus an allow-list of obtainable names. It answers guest requests by resolving
the source *then* (so `Command`/`Vault` sources mint fresh material) and
returns the secret.

**Transport: extend the vsock control channel** (recommended) rather than the
sidecar HTTP server. Rationale:

- The control channel already exists for *every* run, is authenticated by the
  Ping/Pong session-secret handshake, and is multiplexed
  (`src/backend/multiplex.rs`). The sidecar (`src/sidecar/server.rs`) only
  runs when `messaging.enabled` and is reachable as plain HTTP on
  `10.0.2.2:<port>` to anything in the guest with no auth — a weaker default
  for secret traffic.
- Telemetry already demonstrates guest→host-initiated frames on this channel
  (`MessageType::TelemetryData`), so guest-initiated credential requests are
  not a new traffic direction in principle.

The one real cost: today the host is the multiplex *caller* and the guest the
responder. A guest-initiated request inverts caller/responder roles on the
channel. Two implementation options, to be settled in the plan:

- **(a) reverse RPC on the existing channel** — teach the multiplex reader on
  both ends to dispatch peer-initiated requests (guest allocates the
  `request_id`, host registers a handler). Cleanest long-term; touches
  `multiplex.rs` and `guest-agent` dispatch.
- **(b) a second, broker-only vsock port** — guest connects out to a
  dedicated host-listening port, same session-secret handshake. Smaller blast
  radius and simpler to reason about; one more listener to manage.

Wire additions (append-only, matching the protocol's append-only discipline):

```text
MessageType::CredentialRequest  = 28   // { name: String, nonce: [u8;16] }
MessageType::CredentialResponse = 29   // { found: bool, secret?: bytes, error?: String }
```

`MAX_MESSAGE_SIZE` already bounds payloads. The response is JSON like the rest
of the protocol; the secret bytes are zeroized on the guest side after use.

### 4. Guest-side integration (`void-cred`)

A small guest binary, `void-cred`, added to `DEFAULT_COMMAND_ALLOWLIST`
(`src/backend/mod.rs`), is the only component that speaks the broker protocol;
consumers never embed it. Three usage shapes cover the taxonomy:

- **env-on-demand** — `void-cred exec --name ANTHROPIC_API_KEY -- claude …`
  requests the secret, sets it in the child's env, and `exec`s. The secret
  lives in exactly one process for its lifetime, never in the run-wide env or
  a snapshot taken before launch.
- **just-in-time file** — for `File` delivery, the broker (or `void-cred`)
  writes the 0600 file immediately before the consumer starts and removes it
  on exit; `writable:true` credentials are read back to the host on teardown
  so refreshed OAuth tokens survive, replacing today's whole-run RW mount.
- **helper protocols** — a git credential helper / `npm` token script / MCP
  server config that shells out to `void-cred get --name …` so tools fetch
  lazily at the moment of network use.

### 5. Source plugins

`CredentialSource` resolution lives behind a `trait SourceResolver { fn
resolve(&self) -> Result<SecretString>; }` with one impl per variant. The
existing `discover_oauth_credentials` (Keychain/file) and
`discover_codex_credentials` become the `OsKeychain` and `HostFile` impls.
Adding Vault is a new impl + enum variant; the broker, resolver, transport,
and guest are untouched.

### 6. Spec surface

A new opt-in block lets the task declare its own (non-LLM) secrets
declaratively instead of plaintext `env:`:

```yaml
sandbox:
  credentials:
    - { name: GITHUB_TOKEN, from: { env: GH_PAT } }            # delivery defaults to broker
    - { name: NPM_TOKEN,    from: { command: { program: gh, args: [auth, token] } } }
    - name: id_ed25519
      from: { file: ~/.ssh/id_ed25519 }
      to:   { file: /home/sandbox/.ssh/id_ed25519, mode: 0600, writable: false }
```

`BoxSandboxOverride` gets the same field for per-box scoping in pipelines,
mirroring how `env`/`mounts` are already overridable (`src/spec.rs:152`).
Absent the block, behavior is identical to today.

## Security properties

- **Not at rest by default.** Broker delivery means the secret is never in the
  guest env, fs, `/proc`, or any snapshot; it exists only transiently in the
  requesting process.
- **Authenticated transport.** Reuses the session-secret handshake; a stale
  snapshot's secret does not grant access because each restored VM gets a
  fresh secret (`AGENTS.md`, Security considerations).
- **Host chokepoint.** Allow-list of obtainable names + an audit line per
  request (name, time, requesting context if available) — the host decides and
  records, the guest cannot enumerate beyond the allow-list.
- **Redaction + zeroize** end to end via `secrecy`, consistent with existing
  `ApiKey` / `SessionSecret` handling.
- **Opt-in.** No broker traffic, no `credentials:` semantics unless configured,
  per the project's opt-in-by-default principle.

## Residual risks

- **Intra-guest sharing.** Any guest process reaching the channel can request
  an allow-listed name. Mitigation path (future): a per-consumer capability
  token minted by the host and handed to a single launched process, required
  in `CredentialRequest`. Out of scope here; noted so the wire format leaves
  room (the `nonce` field generalizes to a token).
- **Time-of-use exposure.** The secret is in cleartext in the consumer's
  memory while used; unavoidable for any scheme that lets the guest use a
  credential. On-demand only shortens the window.
- **File delivery regressions.** Just-in-time files reintroduce a brief
  at-rest window for path-bound CLIs; scoped to the consumer's lifetime and
  0600, strictly better than today's whole-run mount.

## Migration / compatibility

1. Land the model + resolver + broker with **all existing flows re-expressed**
   through it but **delivery shapes unchanged** (env stays env, codex/claude
   stay file). Pure refactor; `conformance`, `e2e_agent_mcp`, and the codex
   smoke specs are the regression gate.
2. Switch LLM API keys from `Env` to `Broker` delivery behind the env-on-demand
   shim; verify `e2e_agent_mcp` (Claude) and `codex_smoke` still authenticate.
3. Expose `sandbox.credentials:` for task secrets.
4. Add non-host sources (`Command`, Linux Secret Service, Vault) incrementally.

## Phasing

- **P1 — unify + broker core.** `CredentialSpec`/`Source`/`Delivery`,
  `CredentialResolver`, broker over vsock (transport option chosen in the
  implementation plan), `void-cred` with env-on-demand + JIT file. Re-express
  Claude/Codex/custom. No behavior change observable to specs.
- **P2 — task secrets.** `sandbox.credentials:` + `BoxSandboxOverride`,
  git/npm/MCP helper shapes.
- **P3 — pluggable sources.** `Command`, Linux Secret Service, then external
  managers behind `SourceResolver`.
- **P4 — capability tokens** (optional) for intra-guest scoping.

## Open questions

1. Transport: reverse-RPC on the existing channel (option a) vs. a dedicated
   broker vsock port (option b)? Leaning (b) for blast-radius isolation;
   (a) is tidier long-term.
2. Do we keep `Env` delivery as a supported (documented, less-secure) shape
   for ergonomics, or deprecate it once the shim lands?
3. Refresh write-back policy for `writable` file credentials — always return
   the mutated copy to the host, or only when the source is itself a host file?
4. macOS/VZ parity: the broker is transport-only and platform-neutral, but
   `void-cred` and the JIT-file path must be validated on both KVM and VZ
   (`e2e_mount`, `e2e_agent_mcp` patterns).

## Affected code (for the implementation plan)

- `void-box-protocol/src/lib.rs` — `MessageType` 28/29 + request/response types.
- `src/backend/multiplex.rs`, `src/backend/control_channel.rs`,
  `guest-agent/src/main.rs` — transport for guest-initiated broker RPC.
- `src/credentials.rs` — becomes `SourceResolver` impls; staging kept for
  JIT-file delivery.
- `src/llm.rs` — `required_credentials()` replaces `env_vars()` + the
  provider-prepare helpers in `src/runtime.rs`.
- `src/spec.rs` — `CredentialSpec` YAML + `SandboxSpec`/`BoxSandboxOverride`
  fields.
- new `void-cred/` crate (guest) + `DEFAULT_COMMAND_ALLOWLIST`
  (`src/backend/mod.rs`).
