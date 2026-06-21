# RFC-0001: RFC + ADR process

- **Status:** Accepted
- **Authors:** Cristian Spinetta (@cspinetta)
- **Created:** 2026-06-20
- **Discussion:** branch `claude/rfc-adr-process-oj8vxd`
- **Related ADRs:** ADR-0001

## Summary

Adopt a lightweight, two-artifact design process: **RFCs** capture proposals and their discussion, and **ADRs** record the atomic decisions that come out of an accepted RFC. Both live in-tree under `docs/`, are numbered with independent monotonic sequences, and are treated as an append-only historical record that evolves by superseding rather than editing.

## Motivation / problem

VoidBox already accumulates hard-won design rationale — the snapshot-restore XCR0/LAPIC/IA32_XSS saga, the synchronous-I/O-over-`spawn_blocking` control channel, the per-RPC multiplex layout, the hash-pinned agent manifest. Today that rationale is scattered across commit messages, `docs/war-histories.md`, `docs/architecture.md`, and CLAUDE.md/AGENTS.md prose. There is no consistent place to:

- propose a large or cross-cutting change and gather review *before* code lands;
- record *why* a load-bearing decision was made, separately from *what* the code currently does;
- track when an old decision has been replaced, without losing the original reasoning.

The result is that rationale rots into code comments (which AGENTS.md explicitly discourages), or is reconstructed from git archaeology. We need a low-ceremony process that is obvious enough that contributors actually use it.

## Detailed design

### Two artifacts, two roles

- **RFC** — a proposal and a narrative. It states a problem, proposes a design in enough detail to implement, weighs alternatives, and names risks. It is the venue for discussion and iteration.
- **ADR** — a single atomic decision and its consequences, in active voice. One accepted RFC can distill into several ADRs.

### When to write an RFC

Open an RFC when a change is large, complex, cross-cutting, or needs review from other peers before committing to it — new subsystems, protocol/wire changes, security-sensitive designs, anything with non-obvious trade-offs or hard-to-reverse consequences. Small, local, obvious changes skip the RFC and go straight to a PR.

### Flow

1. **Propose — RFC.** Contributor writes an RFC in `docs/rfc/` (next number, status `Draft`), opens it as a PR, gets review. Iterate until `Accepted`, `Rejected`, or `Withdrawn`.
2. **Record — ADR(s).** Once accepted, distill the load-bearing decisions into one or more ADRs in `docs/adr/` (status `Accepted`).
3. **Implement.** Build it, linking PR(s) back to the RFC and ADR numbers.

### Layout, numbering, conventions

- `docs/rfc/` and `docs/adr/`, each with a `README.md` index table and a `template.md`.
- Filenames `NNNN-short-kebab-title.md`: zero-padded 4-digit number, monotonically increasing, never reused or renumbered. A withdrawn RFC keeps its number; the next proposal takes the following one. RFC and ADR numbers are independent sequences.
- Each `README.md` index is a table: `| # | Title | Status | Date | Link |`.
- The process is documented once in `docs/rfc/README.md` and cross-linked from `AGENTS.md` and `CONTRIBUTING.md` so contributors find it.

### Append-only, supersede-don't-edit

RFCs and ADRs are a historical record, not living docs. ADRs are immutable once `Accepted`. When a decision changes, write a successor ADR and flip the old one's status to `Superseded by ADR-NNNN`. Current behavior is described by code plus `docs/architecture.md`; the RFC/ADR set explains how it got there. Retro-filing a decision already made is allowed so the record stays complete.

### Templates

The RFC template (`docs/rfc/template.md`) carries: Summary, Motivation/problem, Detailed design, Alternatives considered, Risks & trade-offs, Unresolved questions, Rollout/implementation plan. The ADR template (`docs/adr/template.md`, MADR-lite) carries: Context, Decision, Consequences.

## Alternatives considered

- **ADRs only, no RFCs.** ADRs are good at recording a settled decision but poor at hosting an open-ended proposal with alternatives still in play. Discussion would have nowhere structured to live, and large proposals would land as faits accomplis. Rejected: we want a review surface *before* commitment.
- **RFCs only, no ADRs.** A long RFC buries the actual decisions inside narrative, and an accepted RFC is a frozen proposal — it can't cleanly express "this specific decision was later replaced." Rejected: we want atomic, supersedable decision records.
- **A wiki / external doc tool (Notion, GitHub wiki).** Out-of-tree docs drift from the code, aren't reviewed through the normal PR gate, and aren't present in a fresh clone. Rejected: in-tree Markdown reviewed as PRs matches how the rest of the project's docs already work.
- **Heavier RFC process (mandatory shepherds, fixed comment windows, formal voting).** Too much ceremony for the project's size; would discourage use. Rejected in favor of the lightweight flow above.

## Risks & trade-offs

- **Process can be ignored.** If RFCs feel like busywork, contributors route around them. Mitigation: keep the bar narrow (only large/cross-cutting/risky changes) and the templates short.
- **Index drift.** The `README.md` tables are maintained by hand and can fall out of sync. Mitigation: updating the index is part of the same PR that adds or re-statuses a document; it is one table row.
- **Boundary judgement.** "Large/complex/cross-cutting" is a judgement call. Some borderline changes will skip an RFC that, in hindsight, deserved one — retro-filing an ADR covers that case.

## Unresolved questions

- Whether to add CI lint for index/file consistency (duplicate numbers, missing index rows). Deferred until the process has enough volume to warrant it.
- Whether RFCs need a dedicated label or just live as ordinary PRs. Deferred to practice.

## Rollout / implementation plan

Ship in one PR: create `docs/rfc/` and `docs/adr/` with their `README.md` indexes and `template.md` files; retro-file this RFC and ADR-0001 as the first entries so the process is self-demonstrating; cross-link from `AGENTS.md` and `CONTRIBUTING.md`. No code changes, no migration, fully backward compatible — existing rationale in `war-histories.md` and `architecture.md` stays where it is, and future significant decisions accrue as ADRs.
