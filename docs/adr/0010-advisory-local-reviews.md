# ADR-0010: Review security and performance with advisory local AI agents

- **Status:** Accepted
- **Date:** 2026-08-07
- **Related:** RFC-0004

## Context

A compiler and a test suite cannot judge a weakened security invariant or a performance regression the benchmark does not measure. The riskiest diffs — the isolation boundary, the hot paths — pass CI green. void-box's security analysis lives in a separate private repository that most clones do not have, so a reviewer cannot read it directly.

## Decision

We will add two advisory reviews, security and performance, run by AI agents on the diff and separately from the change's author. Each has a shared contract and a domain-specific anchor.

- **Shared contract.** Both reviews are advisory: they surface findings for a human to judge, never block a merge, and never edit code. A finding must be introduced by the diff, name the specific invariant, mechanism, or hot path it breaks, and cite the file and line; otherwise the agent omits it. Where the contributor has more than one model, each review runs under each, so different lineages reach their own verdicts. Both run locally for now; running them in CI is deferred for cost. The agents' prompt and command live in the repo as a script a human can run and a skill an agent can invoke, the skill wrapping the script.
- **Security anchor and scope.** The agent reads the diff for security problems the change introduces, using a public, sanitized `docs/security/invariants.md` — distilled from the private threat model, mechanisms only — as a required checklist, not a mechanical gate. It always checks the documented invariants and reasons beyond them.
- **Performance anchor and scope.** The benchmark is an objective floor — a boot slower than the `--assert-*` threshold fails in the binary — plus a before-and-after delta on the contributor's machine, and it covers boot and restore latency only. An agent pass covers the runtime hot paths the benchmark does not measure, so those findings carry more uncertainty.
- **Contributor fallback.** The contributor records in the pull request what ran, and says so when a review or the benchmark could not run, so a gap is visible rather than assumed.

## Consequences

- **Positive:** the changes a green build cannot judge get a review grounded in documented invariants and past measurements. More than one model can review, which catches what a single lineage misses. The script and skill keep the review reproducible; the pull-request report keeps it auditable.
- **Negative / cost:** advisory means a human still triages. The reviews reason beyond the baseline but are only as thorough as the model's judgment, so they can miss an issue or raise a false one. `docs/security/invariants.md` can fall behind the private analysis and must be kept in sync. Local-for-now means a contributor may skip a review, which must be called out in the pull request. There is no dedicated performance host, so a delta is per-machine.
- **Follow-ups:** RFC-0004 M2 implements this — `docs/security/invariants.md`, the reviewer script and skill, then the two review subagents.
