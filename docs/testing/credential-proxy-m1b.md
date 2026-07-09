# Credential-proxy M1b — test plan

This is the validation plan for the M1b credential-proxy work (RFC-0002): codex through the proxy (API-key and ChatGPT OAuth), WebSocket fail-closed (R8), long-run credential rotation, KVM+VZ parity, and the Anthropic-compatible Custom provider.

It is embodied as one runnable, self-documenting script — [`scripts/test_credential_proxy_m1b.sh`](../../scripts/test_credential_proxy_m1b.sh). This document frames what the script does and how to read its output; the script's `--list` prints the per-test detail (what each test checks and the expected result), so the two never drift. Run it as a human or hand it to an AI agent: every test declares its own pass condition and writes its raw output to an evidence folder.

## What "done" looks like

The proxy holds each provider's durable secret on the host and injects the credential header at TLS egress, so the untrusted guest (uid 1000) carries only a non-secret placeholder. The plan validates that end to end across five properties:

- **Containment (R14).** No durable secret — API key or OAuth refresh token — ever reaches the guest env, files, or mounts. The host-side `assert_no_real_credential` gate aborts any run that would leak, and an adversarial probe confirms an agent asking for its own key gets the placeholder.
- **Injection correctness.** The real host-held credential authenticates against the real upstream, and the placeholder/token never travel onward — for Claude (`x-api-key`), codex (`Authorization: Bearer`, plus `chatgpt-account-id` for ChatGPT mode), and the Custom provider.
- **Fail-closed behavior.** A misconfigured or unserviceable proxy config is rejected before boot; a failed injection returns `502` rather than an unauthenticated call; a WebSocket upgrade is refused (R8) rather than silently downgraded.
- **Rotation safety (R12).** The host store is the single rotation owner: single-use refresh tokens rotate and write back atomically without double-spend, across many cycles and simulated concurrent runs.
- **Platform parity.** The proxy binds a guest-reachable, non-LAN-exposed address on both Linux/KVM (loopback) and macOS/VZ (the NAT gateway, ADR-0007).

## Tiers

The script runs in tiers, cheapest and safest first. Each tier is independently selectable.

| Tier | What it validates | Needs | Cost | Safety |
|------|-------------------|-------|------|--------|
| **A — Static & unit** | fmt, clippy `-D warnings`, all unit/integration tests (rotation chains, provision renderers, spec validation, R14 preconditions, withholding), the in-process proxy pipeline (codex carriers, OAuth+account-id, WS fail-closed, token auth, name-constraints, SSRF, cross-sandbox), doctests | nothing | ~2 min | safe |
| **B — Config validation (CLI)** | `voidbox validate` accepts the known-good proxy specs and rejects the parse-time failure cases (Custom without https, Custom without `api_key_env`) | nothing | ~20 s | safe |
| **C — codex provisioning (R9)** | the real pinned codex binary honors the generated `config.toml`/`auth.json` on the wire: redirect path, token carriers, `supports_websockets=false`, no self-refresh | network (fetches the pinned codex binary) | ~15 s | safe |
| **D — Real-upstream injection** | the production proxy injects the host-held API key and the real provider accepts it, placeholder/token never forwarded — Claude → `api.anthropic.com`, codex → `api.openai.com` | API keys | ~1 min | safe, ToS-clean, rotates nothing |
| **E — Full VM on VZ/KVM** | the whole chain in a booted guest: deterministic containment (e2e), a real Claude API-key completion with the R14 log checks, an adversarial containment probe, a Custom-provider completion | API keys, image builds, `RUN_VM=1` | ~10–20 min (first build) | safe, ToS-clean |
| **F — Subscription OAuth** | the personal-subscription OAuth paths end to end: Claude personal and codex ChatGPT, host store refreshes and injects | subscriptions, `RUN_SUBSCRIPTION=1` | ~10 min | **rotates your real login's single-use refresh token** — see below |

Default run: **A B C D**, plus **E** if `RUN_VM=1`, plus **F** if `RUN_SUBSCRIPTION=1`.

## Safety: subscription tokens (tier F)

Tiers A–E are ToS-clean and rotate nothing: they use API keys (the sanctioned programmatic path) or no credential at all.

Tier F exercises the personal-subscription OAuth paths, which the host store refreshes — and OAuth refresh tokens are single-use, so **running tier F rotates your real `claude`/`codex` login's refresh token**. The store is the rotation owner and writes the rotated token back atomically, so your login stays valid; but it is your *primary* login, not a throwaway, and if you run `claude`/`codex` yourself concurrently there is an unguarded race on the single-use token (the store's lock does not coordinate with those clients). Tier F is off by default and requires `RUN_SUBSCRIPTION=1`. This is RFC-0002's single-tenant "ordinary use" and the R4/R5 feasibility path; run it deliberately.

## Credentials

The script reads API keys from a file (default `/Users/cspinetta/dev/.credentials`, override with `CREDS_FILE=...`) containing:

```
ANTHROPIC_API_KEY_VOID_BOX_TEST=sk-ant-...
OPENAI_API_KEY_VOID_BOX_TEST=sk-...
```

Keys are loaded into the test subprocess environment only and are never echoed, logged, or written to the evidence folder. Tier F additionally uses the host's `~/.codex/auth.json` and `~/.claude` login, which you already have signed in.

## Usage

```bash
# Full plan detail (what each test checks + expected result), no execution:
scripts/test_credential_proxy_m1b.sh --list

# Safe default (tiers A–D):
scripts/test_credential_proxy_m1b.sh

# Add the full VM runs on this host (builds production images the first time):
RUN_VM=1 scripts/test_credential_proxy_m1b.sh

# Add the subscription OAuth paths (rotates your real login — deliberate):
RUN_VM=1 RUN_SUBSCRIPTION=1 scripts/test_credential_proxy_m1b.sh

# A single tier:
scripts/test_credential_proxy_m1b.sh A
scripts/test_credential_proxy_m1b.sh D
```

## Reading the results

Every test prints one line as it runs — `PASS`, `FAIL`, or `SKIP` (with the reason it was skipped, e.g. a missing key or platform) — plus a one-line description and its evidence file. At the end the script prints a summary table and the path to the evidence folder.

The evidence folder (`target/m1b-evidence/<timestamp>/`) persists after the run and contains:

- `<test-id>.log` — the raw stdout+stderr of each test, for when a `FAIL` needs investigating.
- `SUMMARY.md` — the same table the script prints, in a form you can paste into a PR or hand to a reviewer.

A contributor catching up runs the script, reads the summary table, and opens the `.log` for any non-`PASS` line. An AI agent does the same: parse `SUMMARY.md`, and for any `FAIL` read the corresponding log.

## Coverage map

Some invariants are enforced before boot and are proven by unit tests (tier A) rather than a live run, because triggering them through the CLI would require a full boot for no extra signal. This maps each to where the plan checks it.

| Invariant | Enforced at | Checked by |
|-----------|-------------|-----------|
| Custom без https / без `api_key_env` | parse (`validate_llm_credential_proxy`) | B (CLI) |
| Custom internal-IP base URL (SSRF) | build (`custom_upstream`) | A — `proxy::provision::tests::custom_rejects_http_and_internal_hosts` |
| Unserviceable provider + proxy | build (`validate_credential_proxy_preconditions`) | A — `agent_box::tests::credential_proxy_rejects_unsupported_provider_before_provisioning` |
| Credential mount over `~/.codex` / `~/.claude` + proxy (R14) | build | A — `agent_box::tests::credential_proxy_rejects_credential_home_mounts_r14` |
| No durable secret in staged env/files (R14) | run (`assert_no_real_credential`) | E (every real run aborts on leak) + A unit tests |
| Token never reaches upstream | run (`presented_proxy_token` + strip) | A (`tests/proxy`) + D/E (real upstream) |
| Injection fail-closed → 502 | run | A (`tests/proxy`) |
| WebSocket upgrade refused (R8) | run | A (`tests/proxy`) + C (no WS attempt from real codex) |
| Single-use refresh rotation, no double-spend (R12) | run (host store) | A (`credentials::store_tests`) |
| Guest gets placeholder, injection replaces it | run | E — `e2e_credential_proxy` + the adversarial probe |
