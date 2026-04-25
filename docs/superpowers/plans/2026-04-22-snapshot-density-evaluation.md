# Spec: Snapshot-restore density and concurrency evaluation

## Goal

Evaluate options to (1) reduce host-memory footprint when many void-box VMs
run concurrently, and (2) reduce wall-clock time from API call to
"VM ready to exec" on the hot path. Decide which combination, if any, to
adopt — and in what order.

This is an **evaluation spec**, not an implementation plan. Each mechanism
below is a candidate; selection happens against the decision criteria at the
end, informed by deployment-target evidence.

## Context

Today's snapshot/restore is designed for **single-VM pause/resume**. The
`Base` snapshot is correct (XCR0/`IA32_XSS`/LAPIC-timer/CID preservation are
all handled — see `AGENTS.md` Known Issues), but it carries no story for:

- Memory sharing between concurrent VMs — every restore allocates fresh guest
  RAM and copies from `memory-ranges`.
- Fan-out from a single warm template — each agent run pays the full restore
  cost on the critical path.
- Idle reclaim — no balloon, no cooperative giveback.

If the product stays at 1–5 concurrent VMs per host, none of this matters. If
it grows toward many-concurrent-agents-per-host, the current design forces N×
memory cost and N× restore latency.

## Non-goals

- Live migration between hosts.
- VFIO / GPU passthrough.
- Confidential VMs (SEV/TDX).
- Replacing existing pause/resume semantics. Anything proposed must be
  **additive**.
- macOS/VZ density parity for mechanisms that depend on Linux-only kernel
  primitives. Document the asymmetry; do not gate the feature on closing it.
- OCI rootfs path changes; virtio-blk RO scheme; network model changes
  (SLIRP stays).

## Threat-model constraints

Any mechanism that shares memory across VMs at the host level **must**:

- Not allow one VM to read another's writes (CoW correctness).
- Not introduce timing/cache side channels materially worse than today.
- Preserve existing snapshot security guarantees: fresh session secret per
  restored VM where applicable, fresh CID per fork, no auth-token reuse.

Any mechanism that cannot meet these is rejected regardless of density gain.

## Mechanisms under evaluation

### M1 — Kernel page-dedup hints (`MADV_MERGEABLE`)

**What.** Mark guest memory regions as candidates for the host kernel's
page-merge daemon. Identical pages across VMs (initramfs text, kernel rodata,
zero pages, common libraries) get merged transparently.

**Cost.** ~10 LOC in `MemoryManager`. No protocol changes. No guest-agent
changes. No snapshot-format change. Linux-only (no-op on VZ).

**Gives.** Memory dedup between any two VMs that happen to share page
contents. Effect grows with similarity.

**Breaks.** Nothing.

**Evaluation gate.** Boot N=2,4,8 identical VMs from the same initramfs.
Measure combined RSS via `/proc/$pid/smaps` `Shared_Clean` after the merge
daemon has had time to scan. Pass if combined RSS scales sub-linearly with N.

### M2 — Demand allocation (`MAP_NORESERVE`)

**What.** Allocate guest RAM without committing physical pages until first
write.

**Cost.** Audit existing memory map flags; add the bit if missing. Trivial.

**Gives.** A VM configured with 4GB whose guest kernel touches 200MB
consumes 200MB of host RAM, not 4GB.

**Breaks.** OOM failure mode shifts from allocation time to write time.
Mitigated by host cgroup limits.

**Evaluation gate.** Boot a 4GB-RAM VM, run a small workload, measure RSS.
Pass if RSS reflects working set, not configured size.

### M3 — CoW restore via `mmap(snapshot, MAP_PRIVATE)`

**What.** On restore, instead of allocating fresh guest RAM and copying
`memory-ranges` into it, `mmap` the snapshot's memory file into guest RAM with
`MAP_PRIVATE`. Concurrent restores of the same snapshot share kernel page-cache
pages until each VM writes; each per-page first-write diverges into a private
copy.

**Cost.**
- New code path in `MemoryManager::from_snapshot`.
- Snapshot file lifetime management — must outlive every VM that maps it.
  Refcounted handle in the daemon.
- Falls back to copy path on VZ and on filesystems where mmap is unsuitable.
- Diff snapshots do not compose directly with this trick: either fall back to
  copy, or design a base+overlay mmap scheme later.

**Gives.** N concurrent restores of one snapshot share the unmodified pages.
Zero impact when N=1.

**Breaks.**
- "Delete after restore" semantics for snapshot files become a refcounted
  variant.
- Snapshot files must live on a local filesystem with reliable mmap support
  (no NFS surprises, no FUSE pitfalls).

**Evaluation gate.**
- Restore N=2,4,8 VMs from one snapshot. Measure combined `Shared_Clean`
  across VMM processes; expect ≈ snapshot_size − Σ per-VM dirty.
- Isolation: write distinct sentinel values in each VM's guest RAM, confirm
  no cross-VM leakage.
- Resume correctness: existing `snapshot_integration` suite passes unchanged
  when the new path is used.

### M4 — Fork mode (identity-rewrite restore)

**What.** Distinguish two restore semantics:

- **Resume.** Same VM identity (CID, session secret, hostname).
  Pause→snapshot→restore→running. Existing behavior.
- **Fork.** Fresh CID, fresh session secret, fresh hostname per restored VM.
  Allows N restores of the same snapshot to coexist without colliding on
  vsock or sidecar identity.

Resolves an existing tension: the Known Issues section requires Resume to
**preserve** CID because the guest kernel caches it during the virtio-vsock
probe. Fork must **replace** CID. Two viable approaches:

- Defer the guest's vsock probe in template until after a "fork imprint"
  handshake.
- Extend the protocol with a re-probe kick the guest-agent issues on receipt
  of a post-fork rehydrate message.

**Cost.**
- New `RestoreMode` enum in the snapshot API.
- Device manager resets vsock CID and net state on Fork.
- Guest-agent gains a post-fork rehydrate handshake (new void-box-protocol
  message): re-read CID from vsock device, re-read hostname/cmdline, reset
  sidecar registration.
- Sidecar must accept N concurrent registrations from "the same template" with
  distinct identities — likely needs a per-fork namespace.

**Gives.** A single warm template fans out into many concurrent sandboxes,
each with its own identity. Combined with M3, per-fork memory cost is just the
dirty-page delta.

**Breaks.**
- Snapshot/restore contract grows — every consumer must pick a mode.
- Guest-agent and protocol bump (backward-compatible if the new message is
  opt-in).

**Evaluation gate.**
- Fork N=4 VMs from one template; each gets a distinct CID, distinct session
  secret, distinct hostname.
- All N can talk to the sidecar concurrently without intent-key collision.
- Existing Resume tests pass unchanged.

### M5 — Warm template pool

**What.** Daemon keeps a configurable number of pre-restored VMs ready in a
pool, refilled in the background. Hot-path API call pops from the pool
instead of triggering a restore.

**Cost.**
- Daemon-side pool manager.
- New API (or option on existing API) to spawn from a pool.
- Background worker maintaining pool depth.
- Configuration: pool size per template, refill batch size, max idle time
  before reaping.

**Gives.** Hot-path latency drops from "full restore cost" to "pool-pop +
identity-rewrite cost". Restore cost is paid in the background, off the
critical path.

**Breaks.**
- Idle pool VMs cost memory (mitigated by M1 + M3).
- Pool-warmth tuning: too small misses traffic spikes, too big wastes RAM.
- Adds daemon complexity even for users who only want single-shot agent runs.
  Make it opt-in.

**Evaluation gate.**
- With pool depth K, sustained N requests/sec, measure P50/P95/P99 of
  API-to-VM-ready latency. Compare against pool depth 0 (cold-restore
  baseline).
- Verify pool refill keeps up under burst (depth K, burst 2K → measure tail).

### M6 — virtio-balloon for idle reclaim

**What.** Cooperative memory reclaim from idle guests via a virtio-balloon
device. Host inflates the balloon when an idle policy fires; guest kernel
returns pages to the host.

**Cost.**
- Virtio-balloon device on host side.
- Guest kernel needs the balloon driver (typically yes).
- Reclaim policy with hysteresis to prevent thrash.

**Gives.** Idle VMs return pages to the host even when KSM hasn't merged
them.

**Breaks.** Adds a device. Adds policy state. Wrong policy degrades workload
latency.

**Evaluation gate.** Run a VM, let it idle for T seconds, measure RSS before
vs after reclaim. Then issue a workload, measure latency impact.

## Summary of mechanisms

| ID | Mechanism                       | Linux | macOS/VZ           | Effort | Risk    |
|----|---------------------------------|-------|--------------------|--------|---------|
| M1 | KSM hints                       | yes   | n/a                | tiny   | low     |
| M2 | Demand allocation               | yes   | partial (opaque)   | tiny   | low     |
| M3 | CoW restore via mmap            | yes   | n/a                | small  | medium  |
| M4 | Fork mode (identity rewrite)    | yes   | yes (no-op w/o M3) | medium | medium  |
| M5 | Warm template pool              | yes   | yes                | medium | low     |
| M6 | virtio-balloon                  | yes   | n/a (VZ has own)   | medium | medium  |

## How they compose

- **M1 + M2 alone:** "free" density floor. Almost zero design cost.
- **M1 + M2 + M3:** density wins on concurrent restores from one snapshot.
- **M3 + M4:** true fan-out from a warm template — multiple distinct
  sandboxes per template.
- **M3 + M4 + M5:** hides the fan-out latency in the background.
- **M6:** orthogonal — useful when many VMs idle for long periods.

## Decision criteria

A mechanism is adopted only if **all** of:

- **Quantified win.** Its evaluation gate produces a measurable, reproducible
  improvement on the relevant metric.
- **No regression.** Existing snapshot/restore correctness tests
  (`snapshot_integration`, `snapshot_vz_integration`) pass unchanged.
- **No security regression.** Fork variants produce distinct session secrets
  and CIDs; no accidental state leakage between VMs.
- **Cost proportional to real demand.** If the deployment targets ≤5
  concurrent VMs per host, M3+M4+M5 are over-engineering. Decision must be
  informed by deployment-target evidence, not aspiration.

## Recommended evaluation order

1. **M1 + M2.** Days. Always-on win on Linux. No architectural commitment.
2. **Establish baseline.** Capture today's metrics before building any of
   M3+: time-to-VM-ready cold, time-to-VM-ready resume, RSS per VM at idle,
   RSS for N=4 identical VMs.
3. **M3.** A week. Adds the CoW primitive without changing semantics. If
   M1+M2 already get us "good enough", reconsider.
4. **M5 alone (without M4).** Two weeks. Pool of *resume* VMs (not yet
   fork). Validates the pool machinery and latency win for the single-tenant
   pause/resume use case.
5. **M4 + M5 fork-pool.** Three weeks. Only if real demand for
   many-concurrent-sandboxes-per-host has been demonstrated.
6. **M6.** Only if measured idle pressure justifies it.

## Open questions

- What is the realistic upper bound for concurrent VMs per host in our
  deployment? Determines whether M4–M6 are worth the architectural cost.
- How important is macOS density? VZ doesn't expose primitives M1, M3, M6
  need — what's the acceptable asymmetry?
- Snapshot-file lifecycle in the daemon — extend the existing resolver, or
  add a new template registry?
- Sidecar — does it tolerate N concurrent agents claiming the same
  template-derived identity prefix? Likely needs a per-fork namespace.
- Diff snapshots — accept that they fall back to copy-on-restore (no CoW
  share), or design a base+overlay mmap scheme later?
- Guest-agent protocol bump for fork-rehydrate — back-compat story for older
  agents in deployed images?

## Decision needed

Before any of M3–M6 is implemented, we need an explicit answer to:

> **How many concurrent VMs per host does the product actually need to
> serve?**

Without that number, "build for density" is speculative and the architecture
cost is unjustified.

**M1 and M2 should ship regardless** because they are nearly free and have no
design-space implications. Everything beyond that waits on the answer above.
