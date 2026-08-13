# RFC-0004: Gates for deciding a change is safe to ship

- **Status:** Accepted
- **Authors:** Cristian Spinetta
- **Created:** 2026-07-29
- **Discussion:** [PR #148](https://github.com/the-void-ia/void-box/pull/148)
- **Related ADRs:** ADR-0009, ADR-0010

## Summary

This RFC defines the checks a change must clear before it merges, and closes the gaps that let unsafe changes pass today. A green build must mean the change was validated, not merely that nothing objected. The gates are format and lint, a test suite that runs real scenarios and fails when it cannot run them, a dependency vulnerability audit, and two independent reviews for security and performance. The first three block a merge. The two reviews are advisory: they surface risk for a human to judge.

## Motivation / problem

A change can reach `main` today without being validated.

First, the integration tests that boot a real VM report success when no VM boots. `create_started_backend` in `tests/conformance.rs:113-134` returns `None` when the backend is unavailable, the kernel or initramfs is unset, or the boot fails. Each test then returns early, and the harness records `ok`. These suites cover `src/vmm`, `src/devices`, `src/backend`, and `guest-agent` — the code where a bug is most dangerous — so the checks that matter most can pass without running. `AGENTS.md` warns about this. One CI step guards against it with `VOID_BOX_DIAGNOSTIC=1`, which turns a raised boot or RPC error into a failure; the other VM suites do not, and even that guard passes a capable machine that skips without raising an error.

Second, some failures have no check at all. Nothing fuzzes the host-side parsers that read guest-controlled bytes. No memory-safety tooling covers the 265 `unsafe` blocks in `src` and `guest-agent`. There is no performance gate, so a boot that is three times slower still merges.

Third, none of the checks block a merge. The branch ruleset requires signed commits, linear history, and a pull request, but zero approvals. The ruleset that lists the status checks runs in report-only mode, so a failing check does not stop the change. (These are repository ruleset settings, not visible in the source tree.)

## Detailed design

A change must clear five gates. Three exist already and need only to be enforced. Two are new.

| Gate | Type | Status |
|------|------|--------|
| Format and lint (`cargo fmt --check`, `cargo clippy -D warnings`) | block | exists |
| Tests (unit, doc, and VM integration) | block | exists; needs the fixes below |
| Dependency audit (`cargo audit`) | block | exists; make it blocking |
| Security review | advisory | new |
| Performance review | advisory | new |

Format, lint, and audit already run and are objective; the only change is to require them on merge.

### Tests that validate real scenarios

Tests are the primary gate. They must satisfy two properties: they must actually run, and they must cover the paths a real change exercises.

**They must run.** A VM test can end three ways, and the harness must tell them apart: passed, failed, or skipped because the machine cannot run VMs. Today the third case swallows the second. `create_started_backend` returns `None` both when there is no KVM and when a boot fails, and the caller returns early either way.

The fix is to separate capability from failure — but not with an upfront probe. As built (ADR-0011), the harness auto-provisions the kernel and test initramfs into `target/`, so an unset artifact is not a skip; then `vm_start` classifies the `backend.start()` result. A genuine hardware-incapability signal — the typed `Error::HypervisorUnavailable` raised only at the `/dev/kvm` probe, or Apple's `Virtualization is not available on this hardware` — is a loud skip. Every other error is a failure, with no flag to set. A machine that can boot but did not is a bug. `test_artifacts::vm_start` and each suite's `start` helper share this, so every suite behaves the same.

CI needs one assertion in the other direction. A runner meant to boot VMs must not skip them silently if it loses `/dev/kvm`. Set `VOID_BOX_REQUIRE_VM=1` on those jobs: when it is set, a genuine capability absence fails the job instead of skipping. The provisioner's source-fingerprint stamp and its post-build initramfs validation are the backstop against a stale or truncated image booting nothing.

macOS is different. `tests/conformance.rs:41` reports the VZ backend as always available, and the hosted macOS runner cannot boot nested VZ. So VZ integration runs on a real macOS host, not in CI, and CI treats the VZ suites as advisory. A contributor without a Mac cannot run these suites, and must say so in the pull request or report, so the gap is visible rather than assumed.

**They must cover the real paths.** Several scenario suites exist but do not run in CI: `oci_integration`, `kvm_integration`, `e2e_sidecar`, `e2e_service_mode`, and `e2e_agent_mcp`. Wire them into the gate, so that boot, command execution, OCI root switch, host-directory mounts, snapshot and restore, network egress, service mode, and the sidecar and MCP paths are validated on every change. Add fuzzing for the host-side parsers that read guest-controlled data — the vsock frame decoder, the virtqueue reader, and the 9p message parser. That is the largest untested surface and the one the threat model weighs most.

### The two reviews

The security review and the performance review are advisory. Each runs on the diff, separately from the change's author, so a review is not the author checking their own work.

Both run locally for now. Running them in CI is the goal, but that cost is deferred until the project needs it. Until then a contributor runs each review on their own machine and reports the result (see "What the pull request must report").

Each review pairs an anchor with agent judgment, but the anchors differ. Performance has an objective anchor: the benchmark fails in the binary when a boot is slower than the floor. Security has a required checklist — the documented invariants the agent must check — which no code mechanically enforces. On top of the anchor, one or more AI agents read the diff and review it for their domain, catching problems the anchor does not encode. The agent review is advisory and never blocks a merge.

Where the contributor has more than one model, run each review under each — for example a Claude Code agent and a Codex agent — so models trained differently reach their own verdicts. One is enough; two of different lineage is better; which ones depends on the tools the contributor has. A contributor who cannot run a review says so in the pull request, so the gap is visible rather than assumed.

A finding must clear a bar, in either review. It must be introduced by the diff, not pre-existing. It must name the specific thing it breaks — the invariant or mechanism for security, the hot path and the added cost for performance — and cite `file:line`. If the agent cannot meet that bar, it omits the finding. Style, refactor advice, and problems the change does not touch are out of scope.

The agents' prompt and command live in the repo: a script a human can run, and a skill an agent can invoke. The skill wraps the script, so the prompt has one home and the review is not reinvented per change.

### Security review

void-box's security analysis lives in a separate private repository. Its conclusions are distilled into a checked-in file, `docs/security/invariants.md`: one entry per invariant, each naming the mechanism that enforces it, the code it lives in, and what a change that breaks it looks like. The file carries mechanisms only, with no exploit detail, so it is safe in public source and present in every clone.

The reviewing agent reads the diff and looks for security problems the change introduces, with the invariants file as its baseline. It also reasons beyond that baseline — a new capability the guest gains, a widened trust boundary, guest input reaching a privileged path unvalidated, a secret reaching a new place — subject to the finding standard above. The baseline invariants it must always check:

- Privileged guest file operations resolve paths in the kernel (`openat2` with `RESOLVE_IN_ROOT | RESOLVE_NO_SYMLINKS` in `guest-agent/src/fs_guard.rs`), never by string.
- The session secret is compared in constant time.
- Host-side parsers of guest data cap the size before allocating.
- No new secret is passed on the kernel command line.
- OCI image unpack tolerates an entry it cannot apply, instead of aborting the whole image.

It surfaces findings; a human decides. It never edits code.

### Performance review

void-box ships a startup benchmark, `voidbox-startup-bench`, that boots a VM and measures the time to boot. It prints percentiles but never fails on a slow result. Add `--assert-cold-p50-ms` and `--assert-warm-p50-ms` flags, so a boot slower than a fixed floor fails in the binary itself.

There is no dedicated host for performance benchmarking today, and setting one up is not justified yet; revisit as the project grows. Contributors run the benchmark on their own machine or VM, and results are not comparable across different hardware. So the review compares a change against its base commit on the same machine — a before-and-after delta — and flags a regression beyond a margin. There is no tuned margin yet; start with a generous one and tighten it in later iterations. CI's nested virtualization is too noisy even for the delta, so on CI the review is advisory and catches only gross regressions. A contributor who cannot run the benchmark — for example, on a machine that cannot boot the VM — must say so in the pull request or the report, so the missing check is visible rather than assumed.

The benchmark is the objective anchor, but it covers only boot and restore latency. The agent part covers the rest — the runtime hot paths a boot benchmark never exercises — so one or more AI agents read the diff for regressions the numbers miss: a new allocation on a hot path, extra work per RPC, a lock held across I/O, a blocking call on the async runtime, or an accidental O(n²) in a loop. These findings have no benchmark behind them, so they carry more uncertainty than a measured regression. No runtime or RPC micro-benchmark exists to anchor them yet. Like the security review, this part is advisory, follows the finding standard above, uses the repo's reviewer script and skill, and never edits code.

### Enforcement

The gates only help if they block. Move the status-check ruleset from report-only to enforcing, require the format, lint, test, and audit checks, and remove the stale required check that no workflow produces.

### What the pull request must report

The pull request records what the contributor validated locally, so the approver can decide whether more is needed before merging. The approver checks CI separately. The PR template carries this block:

```
## Local validation
- Host: <os/arch>
- VM suites: booted (<suites>) | skipped (<no KVM | no Mac>)
- Perf bench: <cold/warm p50 delta vs base> on <machine> | not run (<reason>)
- Security review: <agent/model; …> — <findings, each fixed | dismissed (reason)> | none (<reason>)
- Perf review: <agent/model; …> — <findings, each fixed | dismissed (reason)> | none (<reason>)
```

Trustworthy gates also open a later option: automating more of the development loop, such as letting a coding agent iterate against them until they pass. That is out of scope here, and depends on this foundation being in place first.

## Alternatives considered

**Keep the current checks and rely on review discipline.** Rejected. The gaps above show a green build does not mean a validated change, and zero approvals are required to merge.

**Add memory-safety tooling (miri, sanitizers) across the codebase.** `miri` cannot execute the ioctl, mmap, and FFI code that holds most of the `unsafe`. Fuzzing the guest-facing parsers hits the highest-risk subset for far less effort, so it comes first.

**Make the security and performance reviews block a merge.** A review that blocks on a judgment call produces false stops and trains people to route around it. Advisory reviews surface risk and leave the decision with a human. The one objective piece — a boot slower than the floor — is enforced by the benchmark, not by the review; the security invariants are a checklist the agent applies, not a mechanical gate.

**Parallel VM test fleets and autonomous fix loops.** The VM gate is serial: each suite runs single-threaded and boots one guest at a time. Parallel fleets add cost without buying coverage, and that machinery is disproportionate for a solo, pre-release project.

## Risks & trade-offs

- Failing the tests when they do not run turns a silent pass into a visible failure on machines without KVM or vsock. The "did not run" state must stay distinct from "failed".
- The reviews check known invariants and past measurements. A novel issue that matches none of them passes. The reviews narrow the work; they do not replace human judgment.
- Without a dedicated perf host, the before-and-after delta depends on a quiet machine, and a contributor may not be able to run it at all. Confirm a flagged regression with a second run. Call out a skipped run in the PR.
- VZ integration runs on a Mac, not CI. A VZ-only regression can slip through if that run is skipped and not called out in the PR.
- The invariants file can fall behind the private analysis. Keep it in sync when the analysis changes.

## Unresolved questions

- What regression margin does the performance review start with, and how is it tightened? The first cut is a generous margin, refined in later iterations.
- When do the reviews move from local to CI? They run locally for now to defer cost; the trigger to run them in CI is not set.
- What anchors the performance agent's runtime findings? The benchmark covers boot and restore latency only; no runtime or RPC micro-benchmark exists to anchor the rest yet.

## Rollout / implementation plan

Sequenced, smallest first. Each step stands on its own.

- **M0 — Honest tests and enforcement.** Auto-provision the kernel and test initramfs, and classify the `backend.start()` result: skip only on a genuine hardware-incapability signal, fail on every other boot or RPC error (ADR-0011, superseding ADR-0009's upfront-probe design). Add the `VOID_BOX_REQUIRE_VM=1` assertion so capable CI runners fail instead of skipping, a meta-test that locks the classification, and `cargo nextest` for a bounded-parallelism CI VM lane. Add the benchmark `--assert-*` flags. Make format, lint, test, and audit blocking, and remove the stale required check. Add the report section to the PR template.
- **M1 — Coverage.** Wire the CI-absent scenario suites into the gate. Add fuzzing for the vsock, virtqueue, and 9p parsers.
- **M2 — Reviews.** Land `docs/security/invariants.md`, plus the reviewer script and skill that carry the agents' prompt and command. Then the security and performance reviews as advisory, local-for-now subagents: security against the invariants, performance against the benchmark delta and an agent pass over the diff. A contributor who cannot run a review says so in the PR.

Nothing here changes runtime code or wire formats. On acceptance, file ADRs for the test-honesty contract and the advisory-review design.
