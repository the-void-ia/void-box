# RFCs — VoidBox

This directory holds **Requests for Comments**: design proposals for changes that are large, complex, cross-cutting, or that need review from other peers before they are committed to. An RFC is where the discussion, the alternatives, and the trade-offs live.

The companion record is the **ADR** set in [`../adr/`](../adr/README.md). This file documents the combined RFC + ADR process; the ADR README only indexes the decisions.

## When to write an RFC

Open an RFC when a change is **large, complex, cross-cutting, or needs review from other peers** before you commit to it — new subsystems, protocol/wire changes, security-sensitive designs, anything with non-obvious trade-offs or hard-to-reverse consequences.

Small, local, obvious changes skip the RFC and go straight to a PR.

## The flow

1. **Propose — RFC.** The contributor writes an RFC in `docs/rfc/` (next number, status `Draft`), opens it as a PR, and gets review. The RFC is where the discussion and alternatives live. Iterate until it is `Accepted` (or `Rejected` / `Withdrawn`).
2. **Record — ADR(s).** Once the RFC is accepted, distill the concrete, load-bearing decisions into one or more ADRs in `docs/adr/` (status `Accepted`). An RFC is a proposal and a narrative; an ADR is a single atomic decision and its consequences. One RFC can yield several ADRs.
3. **Implement.** Build it, linking the PR(s) back to the RFC and ADR numbers. The RFC/ADRs are the historical record of *why*; the current state of the system lives in the code and in living docs (`docs/architecture.md`), not in the RFC.

### Drift is handled by superseding, not editing

RFCs and ADRs are an **append-only historical record**, not living docs. Don't rewrite history to match the present. An ADR that no longer reflects reality gets a successor ADR, and its status flips to `Superseded by ADR-NNNN`. ADRs are **immutable** once `Accepted` — when a decision changes later, write a new ADR that supersedes the old one; never edit the original.

Current behavior is described by code + `docs/architecture.md`; the RFC/ADR set explains how it got there. Retro-filing is fine: a significant decision already made can get an ADR after the fact so the record is complete.

## Folders, numbering, conventions

- `docs/rfc/` and `docs/adr/`, each with a `README.md` index.
- Filenames: `NNNN-short-kebab-title.md` — zero-padded 4-digit number, monotonically increasing, **never reused or renumbered** (a withdrawn RFC keeps its number; the next proposal takes the following one). RFC and ADR numbers are independent sequences.
- Use [`template.md`](template.md) as the starting point for a new RFC.
- Keep the index table below current when you add or re-status an RFC.

## RFC statuses

`Draft` → `In Review` → `Accepted` → (implemented), or terminal `Rejected` / `Withdrawn`. An accepted RFC whose direction is later replaced becomes `Superseded by RFC-NNNN`.

## Index

| #    | Title                          | Status   | Date       | Link                                            |
|------|--------------------------------|----------|------------|-------------------------------------------------|
| 0001 | RFC + ADR process              | Accepted | 2026-06-20 | [0001-rfc-adr-process.md](0001-rfc-adr-process.md) |
| 0002 | Guest network egress and credential containment | Accepted | 2026-06-21 | [0002-guest-network-egress-and-credential-containment.md](0002-guest-network-egress-and-credential-containment.md) |
