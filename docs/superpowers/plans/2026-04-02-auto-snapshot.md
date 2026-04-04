# Auto-Snapshot Design Spec

> **Status:** Design — not yet implemented
>
> **Depends on:** snapshot infrastructure (`src/vmm/snapshot.rs`, `src/snapshot_store.rs`), `voidbox shell` (`src/bin/voidbox/attach.rs`)
>
> **Blockers (must resolve before implementation):**
> 1. ~~**Fix snapshot_integration test regressions**~~ — **Resolved.** Root cause: missing `IA32_XSS` MSR capture/restore caused XRSTORS #GP on CET-enabled kernels. Fixed by adding `IA32_XSS` (0x0DA0) to `SNAPSHOT_MSR_INDICES`. All 7/7 tests pass.
> 2. ~~**Enable snapshot_integration in CI**~~ — **Resolved.** Removed `continue-on-error: true` from `e2e.yml` so snapshot test failures block the pipeline.

## Problem

`voidbox shell` cold-boots a VM every time. With a 30MB initramfs, boot takes
~3-5 seconds (kernel decompress → initramfs extract → guest-agent init → module
load → vsock listener). For interactive use, this latency is noticeable —
especially on repeated invocations with the same configuration.

The snapshot infrastructure already supports full VM state capture and restore
with sub-second startup. The missing piece is **automatic** snapshot management
tied to the shell lifecycle.

## Goal

After the first cold boot, automatically save a snapshot so subsequent
`voidbox shell` invocations with the same VM configuration restore instantly
instead of cold-booting.

**Non-goals:**
- Snapshot portability across machines
- Snapshot sharing or distribution
- OCI-layer-aware snapshots (defer to snapshot layering work)

## Design

### Lifecycle

```
voidbox shell --auto-snapshot ...
  │
  ├─ compute config_hash(kernel, initramfs, memory_mb, vcpus)
  │
  ├─ snapshot exists for this hash?
  │   ├─ YES → restore from snapshot → attach PTY
  │   └─ NO  → cold boot → wait for guest-agent ready
  │            → take snapshot → attach PTY
  │
  └─ PTY session ends → stop VM
```

On the **first run** (no snapshot), the user pays the cold-boot cost once. On
every subsequent run with the same config, the VM restores from snapshot.

### Config hash (already implemented)

`compute_config_hash()` in `src/snapshot_store.rs` hashes:
- kernel binary (full content)
- initramfs binary (full content)
- memory_mb
- vcpus

Two VMs with the same hash share snapshots. This is the right granularity —
mounts, env vars, and credentials are applied *after* boot and don't affect the
base VM state that gets snapshotted.

### Snapshot timing

The snapshot is taken **after guest-agent is ready** (control channel handshake
succeeds) but **before** credentials are written, mounts are verified, or the
PTY is opened. This captures a "warm kernel + guest-agent" state that is
reusable regardless of what the shell session does.

Sequence:

1. Cold boot VM
2. `ControlChannel::wait_for_snapshot_ready()` — round-trip `SnapshotReady`
   message (type 17) to confirm guest-agent is fully initialized
3. `MicroVm::snapshot(dir, config_hash, config)` — stops VM, dumps state
4. `MicroVm::from_snapshot(dir)` — restores immediately (same process)
5. Write credentials, apply env, open PTY

Steps 3-4 happen once per config hash. On subsequent runs, step 1-3 are skipped
entirely — the VM starts at step 4 directly from `from_snapshot`.

### Opt-in mechanism

**No new spec field.** `sandbox.snapshot` is already `Option<String>` (a path
or hash prefix). Auto-snapshot reuses this existing field and the existing
`resolve_snapshot()` in `runtime.rs`.

**CLI flag:**

```bash
voidbox shell --auto-snapshot --mount $(pwd):/workspace:rw --program claude ...
```

New flag on `Shell` subcommand. Default: off. When set:

1. Computes config hash from kernel + initramfs + memory_mb + vcpus
2. Sets `sandbox.snapshot` to that hash in the ephemeral spec
3. `resolve_snapshot()` finds the snapshot dir if it exists → restore
4. If no snapshot exists → cold boot → save snapshot after ready

This means `--auto-snapshot` is literally sugar for:
```bash
voidbox shell --snapshot $(compute_config_hash) ...
```
...plus the "save if missing" logic.

**Spec-level:** Users who want auto-snapshot in YAML specs can set
`snapshot: auto` (special sentinel value). `resolve_snapshot()` recognizes
"auto" and computes the config hash.

```yaml
sandbox:
  memory_mb: 3024
  snapshot: auto
```

**Default-on for interactive mode (future):** When `mode: interactive` is
stable, `snapshot: auto` could be the implicit default. Defer until proven.

### Storage

Snapshots go to `~/.void-box/snapshots/<config_hash_prefix>/` (existing
infrastructure). Files per snapshot:

```
~/.void-box/snapshots/a61e02fd1ccce72d/
  state.bin      # serialized VM state (vCPU regs, device state)
  memory.mem     # full memory dump
```

### Invalidation

**Implicit invalidation:** `compute_config_hash` hashes the full kernel and
initramfs binaries. If either changes (new guest-agent build, new kernel), the
hash changes and a new snapshot is created. Old snapshots are not automatically
deleted.

**Explicit invalidation:**

```bash
voidbox snapshot list       # show all snapshots with config hashes
voidbox snapshot delete <hash>  # delete a specific snapshot
```

Both commands already exist.

**TTL/GC (defer):** Automatic cleanup of old snapshots by age or count. Not
needed for v1 — users can manage manually via `snapshot delete`.

### Interaction with existing `--snapshot` flag

| `--snapshot`   | `--auto-snapshot` | Effective `sandbox.snapshot` | Behavior |
|----------------|-------------------|-----------------------------|----------|
| not set        | not set           | `None`                      | Cold boot (current) |
| not set        | set               | computed config hash        | Restore if exists, else cold boot + save |
| `<hash/path>`  | not set           | `<hash/path>`               | Explicit restore |
| `<hash/path>`  | set               | Error                       | Mutually exclusive |

In YAML specs, `snapshot: auto` is equivalent to `--auto-snapshot`.

### Security considerations (from AGENTS.md)

Snapshot restore reuses the same guest memory layout. Per the existing
documentation:

- **RNG entropy:** Mitigated by fresh CID + session secret per restore and
  hardware RDRAND.
- **ASLR:** Mitigated by short-lived tasks, SLIRP NAT isolation, command
  allowlists.
- **Session secret:** Each restored VM gets a unique secret via kernel cmdline.

Auto-snapshot doesn't change the security model — it uses the same
`from_snapshot` path that explicit `--snapshot` uses.

## Implementation outline

### Phase 1: `--auto-snapshot` CLI flag

**Files:**

| File | Change |
|------|--------|
| `src/bin/voidbox/main.rs` | Add `--auto-snapshot` to `Shell` |
| `src/bin/voidbox/attach.rs` | Compute hash, set `sandbox.snapshot`, save-if-missing logic |

**`cmd_shell` changes (attach.rs):**

```rust
if auto_snapshot {
    let config_hash = compute_config_hash(&kernel, initramfs.as_deref(), memory_mb, vcpus)?;
    let snap_dir = snapshot_dir_for_hash(&config_hash);
    if snapshot_exists(&snap_dir) {
        // Reuse existing snapshot — set it on the spec
        builder = builder.snapshot(&snap_dir);
    }
    // If no snapshot exists, cold boot proceeds normally.
    // After build + ready, save snapshot (see below).
}

let sandbox = builder.build()?;

// If auto-snapshot and no snapshot was restored, save one now
if auto_snapshot && !was_restored {
    sandbox.wait_for_ready().await?;
    sandbox.create_auto_snapshot(&config_hash).await?;
}
```

No new spec field. `--auto-snapshot` computes the hash and feeds it into the
existing `builder.snapshot()` path.

**Key constraint:** `MicroVm::snapshot()` consumes `self`. For save-and-continue:
snapshot → `from_snapshot` → continue (~200ms one-time overhead). Same pattern
as snapshot integration tests.

### Phase 2: `snapshot: auto` sentinel in spec

Recognize `"auto"` as a special value in `resolve_snapshot()`:

```rust
fn resolve_snapshot(spec: &RunSpec) -> Option<PathBuf> {
    let hash = spec.sandbox.snapshot.as_deref()?;
    if hash == "auto" {
        // Compute config hash and check if snapshot exists
        let config_hash = compute_config_hash(...)?;
        let dir = snapshot_dir_for_hash(&config_hash);
        return if snapshot_exists(&dir) { Some(dir) } else { None };
    }
    // ... existing hash/path resolution
}
```

This lets YAML specs opt in:
```yaml
sandbox:
  snapshot: auto
```

No new field on `SandboxSpec`. Reuses the existing `snapshot: Option<String>`.

### Phase 3: Default for interactive mode (future)

When `mode: interactive` is stable, the runtime implicitly treats it as
`snapshot: auto` unless an explicit snapshot path is set.

## Open questions

1. **Snapshot after PTY exit?** Should we re-snapshot after the session ends to
   capture any guest-side state changes (installed packages, config)? Probably
   not for v1 — the "warm kernel" snapshot is the main win. Session state is
   ephemeral.

2. **Multiple configs?** If a user runs `voidbox shell` with different
   `memory_mb` or `vcpus` values, each gets its own snapshot. Storage could
   grow. Defer GC to later.

3. **Snapshot + mounts:** Mounts are applied after restore, so they don't need
   to be part of the snapshot. But if a mount provides `/lib/modules/`, the
   guest-agent module loading happens at boot — before snapshot. This is fine
   because module loading is part of the initramfs, which is hashed.

4. **`snapshot()` consumes `MicroVm`:** The current API takes ownership. For
   auto-snapshot in `cmd_shell`, we need to snapshot and then continue using
   the VM. Options:
   - Snapshot → `from_snapshot` → continue (stop + restart, ~200ms overhead)
   - Refactor `snapshot()` to borrow `&mut self` instead of consuming
   - Fork the VM state before stopping (complex)

   Option 1 (stop + restart) is simplest and the overhead is acceptable for a
   one-time operation.
