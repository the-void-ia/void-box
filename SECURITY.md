# Security Policy

void-box is a security-positioned project — we take vulnerability reports seriously and would rather hear about an issue early than read about it later. This document describes how to report, what to expect from us, and what falls inside the scope of this policy.

## Reporting a vulnerability

**Please do not open a public issue for a suspected vulnerability.**

Use one of the two private channels below:

1. **GitHub Security Advisories (preferred).** Open a private report at [https://github.com/the-void-ia/void-box/security/advisories/new](https://github.com/the-void-ia/void-box/security/advisories/new). This gives us a private collaboration space with the reporter and the cleanest path to a coordinated fix + advisory.
2. **Email backup.** If you do not have a GitHub account or prefer email, write to **[contact@voidplatform.ai](mailto:contact@voidplatform.ai)** with subject line `SECURITY: <short description>`. We will move the discussion into a private GHSA thread once we have triaged it.

Please include, where you can:

- A clear description of the issue and its impact.
- The version, commit SHA, or release tag affected.
- Steps to reproduce, ideally a minimal reproducer (YAML spec, Rust snippet, or `gh` command sequence).
- Your environment (host OS + version, kernel version, KVM/VZ).
- Whether the issue is already public anywhere.

You do **not** need to have a fix or full root-cause analysis to file a report — a credible reproducer is enough.

## What to expect from us


| Stage                           | Target                                                      |
| ------------------------------- | ----------------------------------------------------------- |
| Initial acknowledgement         | within **3 business days**                                  |
| Triage + severity assessment    | within **7 days** of acknowledgement                        |
| Fix for **critical** issues     | within **30 days**                                          |
| Fix for **high** issues         | within **60 days**                                          |
| Fix for **medium / low** issues | within **90 days**                                          |
| Public advisory                 | published once the fix lands, coordinated with the reporter |


If we expect to miss any of these, we will tell you and explain why. If we cannot fix an issue (for example, a third-party dependency we don't control), we will say so and document the mitigation.

## Disclosure flow

We follow coordinated disclosure:

1. Reporter files privately via GHSA or email.
2. We acknowledge, triage, and assign a severity.
3. We work on a fix in a private branch / private GHSA fork.
4. Once a fix is ready, we agree on a public disclosure date with the reporter (typically when the fix is released, or shortly after).
5. We publish a GitHub Security Advisory crediting the reporter (unless they prefer to remain anonymous) and reference it in the changelog.

## Scope

**In scope** (this repository and its workspace crates):

- The `voidbox` host runtime and its backends (KVM, VZ).
- `guest-agent` and the host↔guest control channel.
- `void-mcp`, `void-message`, `voidbox-oci`, `void-box-protocol`.
- Build scripts under `scripts/` that produce production images (`build_claude_rootfs.sh`, `build_codex_rootfs.sh`, `build_guest_image.sh`).
- Released artifacts (kernels, initramfs, CLI binaries) and their signing / pinning chain.

**Examples of in-scope issues:**

- Guest escape from a void-box micro-VM to the host.
- Weakening of defense-in-depth controls on the VMM thread (seccomp-BPF allowlist, syscall surface).
- Authentication / session-secret bypass on the vsock control channel.
- Any new privilege escalation primitive that lets the uid-1000 agent gain root inside the guest (path-resolution flaws or TOCTOU bugs in privileged vsock RPCs, kernel-module load gaps, etc.).
- A malicious snapshot file that compromises the host VMM or host kernel on restore (e.g. via a bug in the parsing of restored vCPU state).
- Supply-chain weaknesses in the agent-binary pinning pipeline (see `scripts/agents/manifest.toml`).

**Out of scope:**

- Vulnerabilities in third-party dependencies — please report those upstream first; we will track the impact via Dependabot / `cargo audit` once a CVE exists.
- Host kernel vulnerabilities (KVM, Linux), Apple Hypervisor.framework bugs, or hardware errata — report to the relevant vendor.
- Theoretical issues without a practical impact path.
- Denial of service that requires already-granted resources to be consumed within their documented limits (e.g. spawning the maximum configured number of VMs).
- Social-engineering reports targeting maintainers.
- Issues that only reproduce against a self-built image where the reporter has skipped the hash-pinning step (`CLAUDE_BIN`, `CODEX_BIN`, or local-PATH discovery — see `docs/agents/claude.md` and `docs/agents/codex.md`).
- "Bypasses" of the command allowlist (`DEFAULT_COMMAND_ALLOWLIST` in `src/backend/mod.rs`). The allowlist is a vsock gate that controls which binary the host can launch as the **initial** guest child; it is not a syscall filter and not an in-guest sandbox. A compromised LLM-driven agent that is already running inside the guest can `execve` any binary on the rootfs, subject only to uid-1000 filesystem permissions, per-process `setrlimit`, and SLIRP network policy. Reports framed as "I got the agent to run X that wasn't on the allowlist" are expected behavior, not a vulnerability.
- Compromise of the agent's own in-guest sandbox **by the agent itself**. void-box defends the host (and the host's other local state) from a compromised agent inside the VM; it does not defend the contents of the guest VM from that agent. Anything mounted into the guest, anything the agent can read, and any host credentials staged for the run should be considered exfiltratable by a compromised agent. If this matters to your use case, run with API-key providers (no host OAuth tokens staged) and minimize what you mount in.
- A malicious host operator. The user running `voidbox` is trusted to configure their own sandbox honestly — they can mount any host directory, disable defense-in-depth via `BackendConfig::minimal`, or read any artifact under `~/.void-box/`. void-box is not a multi-tenant isolation platform and does not defend against its own operator.
- Side-channel attacks across the KVM / VZ boundary (cache, timing, Spectre/Meltdown class) beyond what the underlying hypervisor itself mitigates.

**Known limitations under active work:**

The behaviors below are documented current state and are tracked under the ongoing security & performance push (see [voidplatform.ai/updates](https://voidplatform.ai/updates/) for progress). Reports describing them in isolation are **not** vulnerabilities. Reports of *new exploitation primitives chained on top of them*, or of *bypasses of fixes once they land*, **are** in scope.

| Behavior | Status |
|---|---|
| `voidbox serve` accepts unauthenticated requests on `127.0.0.1:43100`. | Per-run authentication model in design. |
| The sidecar HTTP server binds an interface reachable beyond loopback on macOS (Virtualization.framework NAT). | Bearer-token auth in design; tighter macOS bind contingent on a VZ-specific interface. |
| GHCR base images and GitHub Releases artifacts are integrity-verified with SHA-256 but not cryptographically signed. | Sigstore signing + verification on the roadmap. |
| The privileged `ReadFile` vsock RPC has no path restriction at the guest-agent layer. | Read-side `openat2`-based restriction planned alongside the `WriteFile` / `MkdirP` fix. |
| Host OAuth tokens (access + refresh) for the `claude-personal` and `codex` providers are staged into the guest VM, where a compromised in-guest agent can read and exfiltrate them. | Credential broker redesign on the roadmap — architectural change so refresh tokens stay host-side and only short-lived access tokens cross into the guest on demand. |

## Supported versions

void-box is pre-1.0; the supported surface is intentionally small:


| Version                                | Status                         |
| -------------------------------------- | ------------------------------ |
| Latest released minor (current `v0.x`) | Receives security fixes        |
| `main` branch                          | Receives security fixes        |
| Older `v0.x` releases                  | Not supported — please upgrade |


When we cut a new minor, the previous one stops receiving fixes after a **30-day grace period** to give downstreams time to upgrade.

## Safe harbor

We will not pursue legal action against, or ask law-enforcement to investigate, security researchers who:

- Make a good-faith effort to follow this policy.
- Avoid privacy violations, data destruction, service degradation, or disruption to other users.
- Only interact with systems and accounts they own, or for which they have explicit permission from the owner.
- Stop testing and contact us as soon as they identify a potentially exploitable issue.

If you're unsure whether something is in scope or whether your testing plan is acceptable, ask us first via the private channels above — we would much rather have that conversation than guess.

## Credit

We credit reporters in the published advisory and in the relevant changelog entry, unless the reporter asks to remain anonymous. If you would like to be listed differently (handle, organization, link), let us know in your report.