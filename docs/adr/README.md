# ADRs — VoidBox

This directory holds **Architecture Decision Records**: one file per atomic, load-bearing decision and its consequences.

An ADR is distinct from an RFC. An RFC (in [`../rfc/`](../rfc/README.md)) is a proposal and a narrative — where discussion and alternatives live. An ADR is a single decision, stated in active voice, with the consequences that follow from it. One accepted RFC can yield several ADRs. The full process is documented in [`../rfc/README.md`](../rfc/README.md).

## Conventions

- Filenames: `NNNN-short-kebab-title.md` — zero-padded 4-digit number, monotonically increasing, **never reused or renumbered**. ADR and RFC numbers are independent sequences.
- Use [`template.md`](template.md) (MADR-lite) as the starting point.
- ADRs are **immutable** once `Accepted`. Decisions change by **superseding, not editing**: write a new ADR, point `Related` at the old one, and flip the old one's status to `Superseded by ADR-NNNN`.
- Retro-filing is fine: a significant decision already made can get an ADR after the fact so the record is complete.

## ADR statuses

`Proposed` → `Accepted`, with terminal `Deprecated` or `Superseded by ADR-NNNN`.

## Index

| #    | Title                       | Status   | Date       | Link                                                         |
|------|-----------------------------|----------|------------|--------------------------------------------------------------|
| 0001 | Adopt the RFC + ADR process | Accepted | 2026-06-20 | [0001-adopt-rfc-adr-process.md](0001-adopt-rfc-adr-process.md) |
