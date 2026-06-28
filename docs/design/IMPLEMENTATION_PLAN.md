<!--
TEMPORARY SCRATCH FILE — delete before opening the review PR.
This file drives the implementation of the credential-broker + egress-policy
designs across multiple agent sessions. It is not part of the shipped docs.
-->

# Implementation plan (TEMPORARY — delete before review)

## How to use this file

Point a new session at this file. It says **what to build, in what order, what
depends on what, where to test, and which review sub-agents to launch**. Each phase
has a ready-to-paste session prompt at the end. Update the checkboxes as phases land.

**Source of truth (read first, every session):**
- `docs/design/credential-broker.md` — the credential containment design.
- `docs/design/egress-policy.md` — the egress policy/profiles design (a draft skeleton).
- `AGENTS.md` + `CLAUDE.md` — repo conventions (LSP-first navigation, platform parity,
  testing, style). Follow them.
- Branch: `claude/vm-agent-credentials-syz9c2`. Small descriptive commits; push when a
  slice is green. **Do not open a PR unless the user asks** (but see Validation: a
  draft PR is how CI e2e runs).

## Environment & validation model

This dev environment has **no `/dev/kvm`** — it cannot boot micro-VMs. Plan testing in
three tiers:

1. **Here (host-side, no VM):** all host-side code; `cargo test --workspace`
   (non-`#[ignore]` unit + integration); proxy/store tested in-process against a local
   TLS mock upstream + a fake guest client; `nat.rs` policy is pure functions. Plus
   **V1 host-side** (below) — run the real `claude-code`/`codex` *clients* against a
   local proxy; no VM needed.
2. **CI (VM-in-the-loop):** `.github/workflows/e2e.yml` ("E2E KVM", `ubuntu-latest`,
   enables `/dev/kvm`+`vhost-vsock`, builds `scripts/build_test_image.sh`, runs the
   `--ignored` suites) and `e2e-macos.yml` (VZ on `macos-14`). **Both trigger on PRs to
   `main`.** Add new e2e suites as `tests/e2e_<name>.rs` (`#[ignore]`d, following
   `tests/e2e_pty.rs` / `e2e_mount.rs`) and wire a `cargo test --test e2e_<name> --
   --ignored --test-threads=1` line into `e2e.yml`. To run them, **open a draft PR to
   `main` (ask the user first)** — that's the only way CI exercises the VM path on this
   branch.
3. **Deferred (needs accounts/hardware):** **V2** (real throwaway Claude Pro/Max +
   ChatGPT Plus accounts and live provider acceptance — see credential-broker.md);
   macOS/VZ manual smoke. Mark these as "blocked: needs X", don't fake them.

Gate hygiene every session: `cargo fmt --all -- --check`, `cargo clippy --workspace
--all-targets --all-features -- -D warnings`, `cargo test --workspace --all-features`.

## Decision to lock BEFORE coding (prevents the rework)

**One shared proxy, not two.** The credential "injection proxy" and the egress
"egress proxy" are the **same host process** (egress Open Question 2; the perf review
recommends shared — fixed memory cost vs per-sandbox). Build it once with a
per-connection **handler pipeline** so both concerns plug in without re-architecting:

```
guest conn ─▶ [auth: per-run token → resolve run]
           ─▶ [destination: CONNECT host / SNI / base-URL]
           ─▶ [EgressPolicy: allow/deny by FQDN]        (egress track)
           ─▶ [CredentialInjector: rewrite header]      (credential track)  OR
              [Tunnel: CONNECT pass-through, no TLS-term]
           ─▶ [Audit sink]                              (egress track)
           ─▶ upstream (re-resolve per conn, SSRF-pin)
```

Phase 0 builds the skeleton + the API-key `CredentialInjector`. Phase 1 adds OAuth to
the store/injector. Phase 2 adds `EgressPolicy`, `Tunnel`, `Audit`, and the `nat.rs`
pinning/profiles. **The Phase-0 owner freezes this interface; later phases extend, not
rebuild it.**

## Order & dependencies

```
Phase 0  Shared proxy foundation (= credential M0, API-key static-key proxy)
   │     [host proxy process · guest reachability · per-run token ·
   │      per-run ECDSA CA + trust install · TLS-term/CONNECT · inject handler IFACE]
   │     BLOCKS everything.
   ├──────────────▶ Phase 1  Credential track (security priority)
   │                 M1a Claude OAuth → M1b codex OAuth → M2 downstream injection
   │                 [credential store: refresh/rotate/mint/write-back · provider config]
   │                 needs: V1 green, then V2 (deferred) for OAuth acceptance
   │
   └──────────────▶ Phase 2  Egress track  (after Phase 0 IFACE is frozen)
                     [nat.rs Rules/profiles + pinning · FQDN allow-list at proxy ·
                      CONNECT-tunnel · audit · rate-limit · transparent DNAT+SNI]
                     can run in parallel with Phase 1 IF the proxy IFACE is frozen
                     and the two agents own non-overlapping files (see below).
```

**Ownership to avoid collisions:** one agent owns the **proxy core + credential track
(Phases 0–1)**. A second agent does **Phase 2** after the interface is stable —
extending the proxy via handlers and building `nat.rs`. The proxy core stays under one
owner during the priority work. If you must parallelize, the proxy `trait`s/module
boundaries from Phase 0 are the contract; the egress agent must not edit the proxy core
loop.

**Escalate to the user, do not guess:**
- Whether to **drop token injection** (proxy-only) — gated on V2 + ToS check. Default:
  proxy-only; build the proxy path, leave token injection unbuilt unless told.
- The **default egress profile** (egress Open Question 1).
- **One-vs-two proxy** if you think shared is wrong (it isn't, per the perf review —
  raise it before diverging).

---

## Phase 0 — shared proxy foundation (= credential M0)

**Goal:** a host proxy process that injects a static API key, reachable from the
guest, with the handler interface frozen. This is the security-priority lowest-risk
slice AND the substrate both tracks need.

**Build:** the proxy process (low-priv, separate from the daemon — R10); guest→proxy
reachability via the SLIRP gateway (guest-only bind per platform — KVM loopback; VZ
address, not `UNSPECIFIED`); per-run auth token (generate/inject/check/strip);
**per-run ECDSA P-256 CA** + additive trust install (`NODE_EXTRA_CA_CERTS` /
`CODEX_CA_CERTIFICATE`, single PEM, no `ca-certificates`/initramfs rebuild); TLS
termination + the `CredentialInjector` for `x-api-key` (Anthropic) / `Bearer`
(OpenAI/codex/Custom); the handler pipeline interface. Wire provider selection in
`src/llm.rs`; **delete the API-key env forwarding** as each provider migrates
(`env_vars()` ~417).

**Test here:** unit tests for the proxy pipeline + CA + token; an in-process
integration test (fake client → proxy → mock upstream) asserting the right header is
injected and the placeholder never leaks; the **V1 host-side check** (run the pinned
`claude-code`/`codex` against the proxy with a mock upstream — redirect, CA-trust,
**name-constraint enforcement**, `PROVIDER_MANAGED_BY_HOST` suppression, no hard-fail).

**Add e2e (CI-validated):** `tests/e2e_credential_proxy.rs` (`#[ignore]`) — boot a VM,
provision the CA + placeholder + base-URL, exec a client through the proxy, assert no
real key in the guest (env/files/mounts) and a successful injected call to a mock
upstream. Wire it into `e2e.yml`.

**Exit:** fmt/clippy/test green here; V1 host-side green; e2e added (validated by a
draft PR run). Interface frozen and documented in code.

**Sub-agents to launch in this phase:**
- ▶ **Investigation** (general-purpose/Explore) if the SLIRP-gateway reachability or
  guest-only bind details are unclear — map `src/backend/`, `src/network/slirp.rs`,
  `guest_accessible_bind_addr`.
- ▶ **Security review** (general-purpose) after the proxy skeleton: parser-surface
  (R10), CA custody/name-constraints (R2), per-run token, SSRF (R3).
- ▶ **Performance review** after the proxy works: hot-path crypto, shared-vs-per-sandbox
  footprint, startup cost (CA keygen, trust install) against the 252 ms/400 ms budget.

---

## Phase 1 — credential track (depends on Phase 0)

**Goal:** OAuth providers through the proxy; remove the credential mounts/WriteFile.

**Build (in order):**
- **Credential store**: read host creds (`src/credentials.rs` discovery), host-side
  OAuth refresh against the provider token endpoint, mint short-lived access tokens
  (lazy, overlapped with boot), sole rotation owner, **atomic 0600 write-back** of
  rotated tokens (R12), `mlock`+zeroize+`PR_SET_DUMPABLE=0`.
- **M1a Claude OAuth** via the proxy (`ANTHROPIC_BASE_URL`, `PROVIDER_MANAGED_BY_HOST`,
  placeholder `ANTHROPIC_AUTH_TOKEN`, no creds file).
- **M1b codex OAuth** (`openai_base_url`, `credentials_store="file"`,
  `CODEX_CA_CERTIFICATE`, placeholder `auth.json`; inject `ChatGPT-Account-ID`; WS →
  `supports_websockets=false`).
- **Snapshot/restore re-mint** over the control channel; keep CA key/store out of the
  snapshot (R11; `src/vmm/snapshot.rs`/`mod.rs`).
- **M2 downstream injection** for named services (e.g. GitHub) via explicit routing.
- Remove RW mounts + `src/agent_box.rs:464` WriteFile; add the **automated "no real
  credential in the guest" assertion** gating each migration (R14).

**Test here:** store refresh/rotation/write-back unit tests (mock provider token
endpoint); injection integration tests per provider against a mock upstream. **V2 is
deferred** (real accounts) — stub the provider-acceptance assertion behind a
"requires-account" gate and document it.

**Add e2e (CI):** extend `tests/e2e_credential_proxy.rs` (or add `e2e_oauth_proxy.rs`)
— VM run with the OAuth placeholder config, snapshot→restore→re-mint path, assert no
refresh token in guest and re-mint on restore.

**Exit:** gates green; `e2e_agent_mcp` (Claude) + codex smoke still authenticate
(needs the bundled binaries + an account/key — mark blocked if unavailable); V2
checklist written for the account-holder to run.

**Sub-agents:** ▶ correctness review of the store (rotation races, write-back
atomicity, R12); ▶ security review of the OAuth path + snapshot re-mint (R11, R13).

---

## Phase 2 — egress track (depends on Phase 0 interface)

**Goal:** configurable egress profiles enforced at the proxy + network layer.

**Build:** the `nat.rs` `Rules`/policy redesign (open/monitored/allowlist/proxy-only/
none + **pinning independent of proxy liveness**, fail-closed); FQDN allow-list by
hostname at the proxy (CONNECT host / SNI), per-connection resolve **cached short-TTL**
+ SSRF-pin; CONNECT-tunnel for non-credentialed allow-listed traffic (no TLS-term);
audit sink (`src/observe/`); rate-limit/kill-switch (integrate SLIRP conn limits);
transparent interception (DNAT + SNI in the proxy, **never in the SLIRP relay loop**);
egress spec/config plumbing (`src/spec.rs`, runtime, per-box overrides). Adopt the
FQDN-allowlist + report/enforce conventions (Smokescreen/Cilium/Istio) from the doc.

**Test here:** `nat.rs` Rules unit tests per profile; proxy allow-list/tunnel/audit
integration tests (mock destinations); fail-closed-on-proxy-crash test.

**Add e2e (CI):** `tests/e2e_egress.rs` (`#[ignore]`) — VM run per profile: `open`
reaches an allowed host; `allowlist` blocks a non-listed host; pinning blocks a direct
bypass; audit logs the destination. Wire into `e2e.yml` (and consider `e2e-macos.yml`
for VZ pinning).

**Exit:** gates green; profiles e2e validated in CI; the "bounded abuse/fail-closed"
properties now hold (they were advisory without the allow-list).

**Sub-agents:** ▶ security review (egress bypass, transparent-SNI parser surface,
DoH/DoT lockdown); ▶ performance review (datapath cost, full-vs-selective routing).

---

## Sub-agent playbook (use throughout)

- **Investigation** (Explore / general-purpose): when a code path, convention, or
  external behavior is unclear — get the conclusion, not file dumps.
- **Correctness review** (general-purpose) after a non-trivial component: verify logic,
  edge cases, races, error handling against the code.
- **Security review** (general-purpose) after each phase: new attack surface,
  bypasses, the relevant R-items; does it regress any boundary.
- **Performance review** (general-purpose): hot-path cost, memory/density, startup
  budget (252 ms/400 ms) — ground in the repo's benches.
- **`/verify` skill** before declaring a phase done (fmt/clippy/test/audit + smoke).
- Prefer launching independent reviews in parallel; apply findings; re-verify.

## Definition of done & cleanup

- All three phases green on the gates; e2e suites added and **passing in a CI run**
  (draft PR to `main`); V2 + VZ checklists handed to the account/hardware holder.
- **Delete this file** (`docs/design/IMPLEMENTATION_PLAN.md`) before opening the review
  PR. The design docs + ADRs (if adopted) are the durable record; this plan is scratch.

---

## Ready-to-paste session prompts

### Session A — Phase 0 + Phase 1 (proxy core + credential track)

> Implement Phases 0–1 of `docs/design/IMPLEMENTATION_PLAN.md` in the void-box repo on
> branch `claude/vm-agent-credentials-syz9c2`. Read that plan, the two design docs it
> points to, and `AGENTS.md`/`CLAUDE.md` first. Build the shared proxy foundation
> (Phase 0 = the API-key static-key proxy, freezing the handler interface), then the
> credential track (Phase 1). Test host-side here (no KVM): `cargo test --workspace`,
> in-process proxy/store tests against a mock upstream, and the V1 host-side client
> check. Add the `#[ignore]` e2e tests and wire them into `.github/workflows/e2e.yml`
> for CI to validate. **Escalate to me, don't guess:** the drop-token-injection
> decision and anything the plan marks "escalate." Launch review sub-agents
> (correctness/security/performance) at the points the plan calls out. Do NOT open a
> PR or build token injection unless I say so. Mark V2/VZ steps blocked-needs-account.

### Session B — Phase 2 (egress track)

> Implement Phase 2 of `docs/design/IMPLEMENTATION_PLAN.md` AFTER the Phase-0 proxy
> interface is merged/stable on `claude/vm-agent-credentials-syz9c2`. Read the plan,
> `docs/design/egress-policy.md`, and `AGENTS.md`/`CLAUDE.md`. Extend the existing
> shared proxy via its handler interface (do NOT re-architect the proxy core) and build
> the `nat.rs` Rules/profiles + pinning. Test host-side here; add `tests/e2e_egress.rs`
> (`#[ignore]`) and wire it into `e2e.yml`. Escalate the default-profile decision.
> Launch security + performance review sub-agents as the plan calls out. No PR unless I
> ask.

### Validation run

> To run the VM/VZ e2e in CI: open a **draft** PR from `claude/vm-agent-credentials-syz9c2`
> to `main` (only on my go-ahead). `e2e.yml` (KVM) and `e2e-macos.yml` (VZ) run on PRs
> to `main`. Report failures with logs; fix and push; the PR re-runs CI.
