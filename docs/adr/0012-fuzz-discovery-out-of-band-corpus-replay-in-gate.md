# ADR-0012: Fuzz out of band, replay the corpus in the merge gate

- **Status:** Accepted
- **Date:** 2026-08-19
- **Related:** RFC-0004 (milestone M1); ADR-0011

## Context

Three host-side parsers read bytes a guest chooses: the control-channel frame decoder in `void-box-protocol` plus the multiplex request-id prefix, the split-virtqueue reader that walks descriptor chains out of guest memory, and the 9P server that turns guest requests into host filesystem calls. A panic in any of them ends the VMM process for every sandbox it hosts, and an allocation sized from a guest-supplied length is host memory exhaustion the guest triggers at will. RFC-0004 identifies this as the largest untested surface in the tree.

Coverage-guided fuzzing is the tool for that surface, and in Rust that means `cargo fuzz` over libFuzzer. Two properties of it decide where it can run. It needs a nightly toolchain, because `fuzz_target!` and the sanitizer instrumentation are nightly-only; the project otherwise builds on stable with a 1.88 minimum. And it is a search, so the same commit fuzzed twice gives different results — a run that finds a bug says nothing about whether the change under review introduced it.

RFC-0004's gates block a merge. A check that blocks must be deterministic and must fail only for the change in front of it.

## Decision

We will split fuzzing into discovery and regression, and put only regression in the merge gate.

Discovery runs `cargo fuzz` on a nightly toolchain in `.github/workflows/fuzz.yml`, weekly and on demand, one job per target. It is not a required status check and does not run on pull requests.

Regression runs in `tests/fuzz_corpus.rs`, inside the ordinary `cargo test` gate on stable. It replays every file under `fuzz/corpus/<target>/` and `fuzz/artifacts/<target>/` through the same harness the fuzzer drives, and it fails if a target declared in `fuzz/Cargo.toml` has no harness registered or no seed corpus.

Both callers share one set of harness bodies in `void_box::fuzz`, each taking a raw byte slice. The module is `#[doc(hidden)] pub` and always compiled, not behind a feature: a harness behind an off-by-default feature stops being replayed the day a command drops `--all-features`, and it stops silently.

A crash found by discovery is fixed by committing the crashing input under `fuzz/artifacts/<target>/` together with the parser fix. A crashing input is never committed ahead of its fix.

## Consequences

- **Positive:** the merge gate gains a check that is deterministic, runs on the shipped toolchain, and fails only for the change in front of it. Every bug the fuzzer has ever found is re-checked on every pull request, at the cost of milliseconds. A nightly toolchain regression cannot block a merge.
- **Positive:** the corpus is the durable artifact. Its filenames name the shapes each target covers, so it documents the surface as well as guarding it.
- **Negative / cost:** a bug discovery could have caught is caught up to a week late, and only for code paths the previous week's search reached. Fuzzing a parser changed in a pull request is a maintainer action — a manual dispatch — not something the gate arranges.
- **Negative / cost:** the harnesses are code that must track the parsers. A harness that stops reaching a rewritten parser still passes, and nothing detects that; only the coverage numbers a discovery run prints would show it.
- **Follow-ups:** the harnesses assert absence of panics plus a few structural invariants (a 9P reply's size field matches its length, a descriptor chain does not exceed the queue size, the multiplex prefix round-trips). Security invariants that need a richer oracle — that no 9P path operation escapes the shared root, for one — are not expressible this way and belong with the security review in RFC-0004 M2.
