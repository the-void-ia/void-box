# ADR-0001: Adopt the RFC + ADR process

- **Status:** Accepted
- **Date:** 2026-06-20
- **Related:** RFC-0001

## Context

Design rationale for VoidBox is currently scattered across commit messages, `docs/war-histories.md`, `docs/architecture.md`, and agent-instruction prose. There is no consistent surface to propose a large or cross-cutting change for review before it lands, and no atomic place to record *why* a load-bearing decision was made — separate from *what* the code currently does. AGENTS.md already discourages letting rationale rot into code comments, which leaves git archaeology as the fallback. RFC-0001 proposes a lightweight, in-tree process to fix this.

## Decision

We will adopt a two-artifact design process:

- **RFCs** in `docs/rfc/` capture proposals, discussion, and alternatives for changes that are large, complex, cross-cutting, or that need peer review before commitment. Small, local, obvious changes skip the RFC.
- **ADRs** in `docs/adr/` record the atomic, load-bearing decisions distilled from an accepted RFC.

Both use `NNNN-short-kebab-title.md` filenames with independent, zero-padded, monotonically increasing numbers that are never reused or renumbered; each directory keeps a `README.md` index table and a `template.md`. Both are an append-only historical record: ADRs are immutable once `Accepted`, and decisions change by superseding (a successor ADR plus a status flip on the original), never by editing. The process is documented in `docs/rfc/README.md` and cross-linked from `AGENTS.md` and `CONTRIBUTING.md`.

## Consequences

What follows from the decision.

- **Positive:** A single, reviewed-as-PR home for design proposals and for the rationale behind decisions. Rationale stops leaking into code comments and git archaeology. Superseding gives a faithful history of how the system's design evolved without rewriting the past. The record lives in-tree, present in every fresh clone.
- **Negative / cost:** Manually maintained index tables can drift; the "large / cross-cutting" boundary is a judgement call that some changes will get wrong. Contributors must remember the process exists — hence the cross-links.
- **Follow-ups:** Future significant decisions accrue as new ADRs (retro-filing allowed). Possible later work: CI lint for index/file consistency, and a convention for labelling RFC PRs — both deferred in RFC-0001 until volume justifies them.
