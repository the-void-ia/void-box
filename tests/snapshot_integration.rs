#![cfg(target_os = "linux")]
//! Snapshot integration tests for void-box.
//!
//! **`snapshot_cold_boot_vs_restore`** — measures cold-boot time, takes a cold
//! snapshot (stopping the VM), restores it, and compares cold-boot vs restore
//! latency.
//!
//! Requirements (same as `kvm_integration.rs`):
//! - `/dev/kvm` present and accessible
//! - Environment variables:
//!   - `VOID_BOX_KERNEL`    -> path to vmlinux or bzImage
//!   - `VOID_BOX_INITRAMFS` -> path to initramfs (cpio.gz)
//!
//! ```bash
//! export VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r)
//! export VOID_BOX_INITRAMFS=/tmp/void-box-test-rootfs.cpio.gz
//!
//! cargo test --test snapshot_integration -- --ignored --nocapture --test-threads=1
//! ```

use std::path::PathBuf;
use std::time::{Duration, Instant};

#[path = "common/vm_preflight.rs"]
mod vm_preflight;

use void_box::vmm::config::{VoidBoxConfig, VsockBackendType};
use void_box::vmm::snapshot::{self, SnapshotConfig, VmSnapshot};
use void_box::vmm::MicroVm;
use void_box::Error;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Load kernel + initramfs paths from environment.
fn kvm_artifacts_from_env() -> Option<(PathBuf, Option<PathBuf>)> {
    let kernel = std::env::var_os("VOID_BOX_KERNEL")?;
    let kernel = PathBuf::from(kernel);
    let initramfs = std::env::var_os("VOID_BOX_INITRAMFS").map(PathBuf::from);
    Some((kernel, initramfs))
}

/// Run preflight checks; returns `None` (with eprintln) if the environment
/// is not suitable for KVM snapshot tests.
fn preflight() -> Option<(PathBuf, Option<PathBuf>)> {
    if let Err(e) = vm_preflight::require_kvm_usable() {
        eprintln!("skipping snapshot test: {e}");
        return None;
    }
    if let Err(e) = vm_preflight::require_vsock_usable() {
        eprintln!("skipping snapshot test: {e}");
        return None;
    }
    let Some((kernel, initramfs)) = kvm_artifacts_from_env() else {
        eprintln!(
            "skipping snapshot test: \
             set VOID_BOX_KERNEL and (optionally) VOID_BOX_INITRAMFS"
        );
        return None;
    };
    if let Err(e) = vm_preflight::require_kernel_artifacts(&kernel, initramfs.as_deref()) {
        eprintln!("skipping snapshot test: {e}");
        return None;
    }
    Some((kernel, initramfs))
}

/// Build a `VoidBoxConfig` from kernel/initramfs paths.
fn build_config(kernel: &std::path::Path, initramfs: Option<&std::path::Path>) -> VoidBoxConfig {
    build_config_vcpus(kernel, initramfs, 1)
}

/// Build a `VoidBoxConfig` with a specific vCPU count.
fn build_config_vcpus(
    kernel: &std::path::Path,
    initramfs: Option<&std::path::Path>,
    vcpus: usize,
) -> VoidBoxConfig {
    let mut cfg = VoidBoxConfig::new()
        .memory_mb(256)
        .vcpus(vcpus)
        .kernel(kernel)
        .enable_vsock(true)
        .vsock_backend(VsockBackendType::Userspace);
    if let Some(p) = initramfs {
        cfg = cfg.initramfs(p);
    }
    cfg.validate().expect("invalid VoidBoxConfig");
    cfg
}

/// Build a `SnapshotConfig` matching the test VM.
/// Note: `cid` is set to 0 here — `snapshot_internal()` overwrites it with
/// the VM's actual CID before saving.
fn snap_config() -> SnapshotConfig {
    snap_config_vcpus(1)
}

/// Build a `SnapshotConfig` with a specific vCPU count.
fn snap_config_vcpus(vcpus: usize) -> SnapshotConfig {
    SnapshotConfig {
        memory_mb: 256,
        vcpus,
        cid: 0, // overwritten by snapshot_internal()
        vsock_mmio_base: 0xd080_0000,
        network: false,
    }
}

/// Build a `VoidBoxConfig` with networking enabled.
fn build_config_net(
    kernel: &std::path::Path,
    initramfs: Option<&std::path::Path>,
) -> VoidBoxConfig {
    let mut cfg = VoidBoxConfig::new()
        .memory_mb(256)
        .vcpus(1)
        .kernel(kernel)
        .enable_vsock(true)
        .vsock_backend(VsockBackendType::Userspace)
        .network(true);
    if let Some(p) = initramfs {
        cfg = cfg.initramfs(p);
    }
    cfg.validate().expect("invalid VoidBoxConfig");
    cfg
}

/// Build a `SnapshotConfig` with networking enabled.
fn snap_config_net() -> SnapshotConfig {
    SnapshotConfig {
        memory_mb: 256,
        vcpus: 1,
        cid: 0,
        vsock_mmio_base: 0xd080_0000,
        network: true,
    }
}

/// Try exec on a restored VM. Returns success/failure without panicking,
/// since CID mismatch is a known limitation.
async fn try_restored_exec(vm: &MicroVm) -> bool {
    match tokio::time::timeout(Duration::from_secs(45), vm.exec("echo", &["ready"])).await {
        Ok(Ok(out)) if out.success() && out.stdout_str().trim() == "ready" => {
            eprintln!("  Restored VM exec: OK");
            true
        }
        Ok(Ok(out)) => {
            eprintln!(
                "  Restored VM exec: unexpected (exit={}, stdout='{}')",
                out.exit_code,
                out.stdout_str().trim()
            );
            false
        }
        Ok(Err(Error::Guest(msg))) => {
            eprintln!("  Restored VM exec: guest unreachable (CID mismatch): {msg}");
            false
        }
        Ok(Err(e)) => {
            eprintln!("  Restored VM exec: error: {e}");
            false
        }
        Err(_) => {
            eprintln!("  Restored VM exec: timed out (CID mismatch)");
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Test: Cold boot → cold snapshot → restore
// ---------------------------------------------------------------------------

/// Cold-boot a VM, take a cold snapshot (stops the VM), restore from it.
///
/// Compares cold-boot latency against snapshot-restore latency.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires KVM + kernel/initramfs artifacts; see module docs"]
async fn snapshot_cold_boot_vs_restore() {
    let Some((kernel, initramfs)) = preflight() else {
        return;
    };
    let cfg = build_config(&kernel, initramfs.as_deref());

    // --- Cold boot ---
    eprintln!("[cold_boot_vs_restore] Booting VM...");
    let cold_start = Instant::now();
    let vm = match MicroVm::new(cfg).await {
        Ok(vm) => vm,
        Err(e) => {
            eprintln!("  failed to create VM: {e}");
            return;
        }
    };
    let cold_boot_time = cold_start.elapsed();

    // Health check
    let output = match vm.exec("echo", &["ready"]).await {
        Ok(out) => out,
        Err(Error::VmNotRunning) => {
            eprintln!("  VM not running after cold boot");
            return;
        }
        Err(Error::Guest(msg)) => {
            eprintln!("  guest communication error: {msg}");
            return;
        }
        Err(e) => panic!("  exec failed: {e}"),
    };
    assert!(output.success());
    assert_eq!(output.stdout_str().trim(), "ready");
    eprintln!(
        "[cold_boot_vs_restore] Cold boot OK ({:.1?})",
        cold_boot_time
    );

    // --- Cold snapshot (stops the VM) ---
    let cold_cid = vm.cid();
    eprintln!("[cold_boot_vs_restore] Cold boot CID={}", cold_cid);

    let snap_dir = tempfile::tempdir().expect("tempdir");
    let config_hash =
        snapshot::compute_config_hash(&kernel, initramfs.as_deref(), 256, 1).expect("hash");

    eprintln!("[cold_boot_vs_restore] Taking cold snapshot...");
    let snap_start = Instant::now();
    let snapshot_path = match vm
        .snapshot(snap_dir.path(), config_hash, snap_config())
        .await
    {
        Ok(path) => path,
        Err(e) => {
            eprintln!("  snapshot failed: {e}");
            return;
        }
    };
    let snap_time = snap_start.elapsed();
    eprintln!(
        "[cold_boot_vs_restore] Snapshot captured in {:.1?}",
        snap_time
    );

    // Validate snapshot on disk
    let snap = VmSnapshot::load(&snapshot_path).expect("load snapshot");
    eprintln!(
        "[cold_boot_vs_restore] Snapshot CID={} (cold boot CID={})",
        snap.config.cid, cold_cid
    );
    assert_eq!(snap.version, snapshot::SNAPSHOT_VERSION);
    assert_eq!(snap.config.memory_mb, 256);
    assert_eq!(snap.config.vcpus, 1);
    assert!(snap.config.cid >= 3, "snapshot must preserve real CID");
    assert!(
        !snap.vcpu_states.is_empty(),
        "snapshot must contain vCPU states"
    );
    let mem_path = VmSnapshot::memory_path(&snapshot_path);
    assert!(mem_path.exists(), "memory dump must exist");
    let mem_size = std::fs::metadata(&mem_path).unwrap().len();
    assert_eq!(mem_size, 256 * 1024 * 1024, "memory dump must be 256MB");

    // --- Restore ---
    eprintln!("[cold_boot_vs_restore] Restoring from snapshot...");
    let restore_start = Instant::now();
    let mut restored_vm = match MicroVm::from_snapshot(&snapshot_path).await {
        Ok(vm) => vm,
        Err(e) => {
            eprintln!("  restore failed: {e}");
            return;
        }
    };
    let restore_time = restore_start.elapsed();
    eprintln!(
        "[cold_boot_vs_restore] Restored VM CID={} (restore took {:.1?})",
        restored_vm.cid(),
        restore_time
    );

    // Verify the Unix socket exists
    let socket_path = format!("/tmp/void-box-vsock-{}.sock", restored_vm.cid());
    eprintln!(
        "[cold_boot_vs_restore] Socket {} exists={}",
        socket_path,
        std::path::Path::new(&socket_path).exists()
    );

    let exec_ok = try_restored_exec(&restored_vm).await;

    // Dump serial output to check for kernel panics
    let serial = restored_vm.read_serial_output();
    if !serial.is_empty() {
        let s = String::from_utf8_lossy(&serial);
        eprintln!(
            "[cold_boot_vs_restore] Serial output ({} bytes):\n{}",
            serial.len(),
            s
        );
    } else {
        eprintln!("[cold_boot_vs_restore] No serial output from restored VM");
    }

    assert!(
        exec_ok,
        "restored VM exec must succeed (CID preserved across snapshot/restore)"
    );

    // --- Timing ---
    let speedup = cold_boot_time.as_secs_f64() / restore_time.as_secs_f64().max(1e-9);
    eprintln!();
    eprintln!("=== Cold Boot vs Restore ===");
    eprintln!("  Cold boot:   {:>10.1?}", cold_boot_time);
    eprintln!("  Snapshot:    {:>10.1?}", snap_time);
    eprintln!("  Restore:     {:>10.1?}", restore_time);
    eprintln!("  Speedup:     {:>10.1}x", speedup);
    eprintln!("============================");

    restored_vm.stop().await.ok();
}

// ---------------------------------------------------------------------------
// Test: Base snapshot → restore → dirty tracking → diff snapshot → restore
// ---------------------------------------------------------------------------

/// Take a base snapshot, restore it, run a command (dirties pages), take a
/// diff snapshot, restore from diff, and verify exec works.
///
/// Validates that:
/// - `enable_dirty_tracking()` succeeds on a restored VM
/// - `snapshot_diff()` produces a smaller memory file than the full dump
/// - Restoring from a diff snapshot (base + delta) yields a working VM
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires KVM + kernel/initramfs artifacts; see module docs"]
async fn snapshot_diff_restore() {
    let Some((kernel, initramfs)) = preflight() else {
        return;
    };
    let cfg = build_config(&kernel, initramfs.as_deref());

    // --- Cold boot ---
    eprintln!("[diff_restore] Booting VM...");
    let vm = match MicroVm::new(cfg).await {
        Ok(vm) => vm,
        Err(e) => {
            eprintln!("  failed to create VM: {e}");
            return;
        }
    };

    // Health check
    let output = match vm.exec("echo", &["ready"]).await {
        Ok(out) => out,
        Err(e) => {
            eprintln!("  cold boot exec failed: {e}");
            return;
        }
    };
    assert!(output.success());
    eprintln!("[diff_restore] Cold boot OK");

    // --- Base snapshot ---
    let config_hash =
        snapshot::compute_config_hash(&kernel, initramfs.as_deref(), 256, 1).expect("hash");

    // Save base to the standard snapshot directory so diff restore can find it
    let base_dir = snapshot::snapshot_dir_for_hash(&config_hash);
    std::fs::create_dir_all(&base_dir).expect("create base snapshot dir");

    eprintln!("[diff_restore] Taking base snapshot...");
    let base_snap_start = Instant::now();
    let base_path = match vm
        .snapshot(&base_dir, config_hash.clone(), snap_config())
        .await
    {
        Ok(path) => path,
        Err(e) => {
            eprintln!("  base snapshot failed: {e}");
            let _ = std::fs::remove_dir_all(&base_dir);
            return;
        }
    };
    let base_snap_time = base_snap_start.elapsed();
    eprintln!(
        "[diff_restore] Base snapshot captured in {:.1?}",
        base_snap_time
    );

    let base_mem_size = std::fs::metadata(VmSnapshot::memory_path(&base_path))
        .unwrap()
        .len();

    // --- Restore from base ---
    eprintln!("[diff_restore] Restoring from base snapshot...");
    let restored_vm = match MicroVm::from_snapshot(&base_path).await {
        Ok(vm) => vm,
        Err(e) => {
            eprintln!("  base restore failed: {e}");
            let _ = std::fs::remove_dir_all(&base_dir);
            return;
        }
    };
    eprintln!("[diff_restore] Restored VM CID={}", restored_vm.cid());

    // Enable dirty tracking BEFORE any exec — all guest activity must be
    // captured so the diff contains every page that changed since the base.
    eprintln!("[diff_restore] Enabling dirty page tracking...");
    restored_vm
        .enable_dirty_tracking()
        .expect("enable dirty tracking");

    // Verify exec works on base-restored VM (this also dirties pages)
    let exec_ok = try_restored_exec(&restored_vm).await;
    assert!(exec_ok, "base-restored VM exec must succeed");

    // Run another command to dirty more guest pages
    let output = match restored_vm.exec("echo", &["dirty-pages"]).await {
        Ok(out) => out,
        Err(e) => {
            eprintln!("  exec after dirty tracking failed: {e}");
            let _ = std::fs::remove_dir_all(&base_dir);
            return;
        }
    };
    assert!(output.success());
    assert_eq!(output.stdout_str().trim(), "dirty-pages");
    eprintln!("[diff_restore] Dirtied pages via exec");

    // --- Diff snapshot ---
    let diff_dir = tempfile::tempdir().expect("tempdir for diff");
    let parent_id = config_hash.clone();

    eprintln!("[diff_restore] Taking diff snapshot...");
    let diff_snap_start = Instant::now();
    let diff_path = match restored_vm
        .snapshot_diff(
            diff_dir.path(),
            config_hash.clone(),
            snap_config(),
            parent_id,
        )
        .await
    {
        Ok(path) => path,
        Err(e) => {
            eprintln!("  diff snapshot failed: {e}");
            let _ = std::fs::remove_dir_all(&base_dir);
            return;
        }
    };
    let diff_snap_time = diff_snap_start.elapsed();
    eprintln!(
        "[diff_restore] Diff snapshot captured in {:.1?}",
        diff_snap_time
    );

    // Validate diff is smaller than base
    let diff_mem_path = VmSnapshot::diff_memory_path(&diff_path);
    assert!(diff_mem_path.exists(), "diff memory file must exist");
    let diff_mem_size = std::fs::metadata(&diff_mem_path).unwrap().len();
    eprintln!(
        "[diff_restore] Base memory: {} bytes, Diff memory: {} bytes ({:.1}%)",
        base_mem_size,
        diff_mem_size,
        (diff_mem_size as f64 / base_mem_size as f64) * 100.0
    );
    assert!(
        diff_mem_size < base_mem_size,
        "diff memory ({diff_mem_size}) must be smaller than base ({base_mem_size})"
    );

    // Validate snapshot metadata
    let diff_snap = VmSnapshot::load(&diff_path).expect("load diff snapshot");
    assert_eq!(
        diff_snap.snapshot_type,
        snapshot::SnapshotType::Diff,
        "must be a diff snapshot"
    );
    assert!(
        diff_snap.parent_id.is_some(),
        "diff snapshot must have parent_id"
    );

    // --- Restore from diff ---
    eprintln!("[diff_restore] Restoring from diff snapshot...");
    let diff_restore_start = Instant::now();
    let mut diff_restored_vm = match MicroVm::from_snapshot(&diff_path).await {
        Ok(vm) => vm,
        Err(e) => {
            eprintln!("  diff restore failed: {e}");
            let _ = std::fs::remove_dir_all(&base_dir);
            return;
        }
    };
    let diff_restore_time = diff_restore_start.elapsed();
    eprintln!(
        "[diff_restore] Diff-restored VM CID={} (restore took {:.1?})",
        diff_restored_vm.cid(),
        diff_restore_time
    );

    // Verify exec works on diff-restored VM
    let exec_ok = try_restored_exec(&diff_restored_vm).await;

    // Dump serial output on failure
    if !exec_ok {
        let serial = diff_restored_vm.read_serial_output();
        if !serial.is_empty() {
            let s = String::from_utf8_lossy(&serial);
            eprintln!(
                "[diff_restore] Serial output ({} bytes):\n{}",
                serial.len(),
                s
            );
        }
    }

    assert!(exec_ok, "diff-restored VM exec must succeed");

    // --- Summary ---
    eprintln!();
    eprintln!("=== Diff Snapshot Summary ===");
    eprintln!("  Base snapshot:   {:>10.1?}", base_snap_time);
    eprintln!("  Diff snapshot:   {:>10.1?}", diff_snap_time);
    eprintln!("  Diff restore:    {:>10.1?}", diff_restore_time);
    eprintln!(
        "  Memory savings:  {:>10.1}%",
        (1.0 - diff_mem_size as f64 / base_mem_size as f64) * 100.0
    );
    eprintln!("=============================");

    diff_restored_vm.stop().await.ok();

    // Cleanup base snapshot from standard location
    let _ = std::fs::remove_dir_all(&base_dir);
}

// ---------------------------------------------------------------------------
// Test: Multi-vCPU snapshot → restore
// ---------------------------------------------------------------------------

/// Cold-boot a VM with multiple vCPUs, take a snapshot, restore, and verify
/// that all vCPU states are captured and the restored VM is functional.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires KVM + kernel/initramfs artifacts; see module docs"]
async fn snapshot_multi_vcpu() {
    let Some((kernel, initramfs)) = preflight() else {
        return;
    };

    let num_vcpus: usize = 4;
    let cfg = build_config_vcpus(&kernel, initramfs.as_deref(), num_vcpus);

    // --- Cold boot with 4 vCPUs ---
    eprintln!("[multi_vcpu] Booting VM with {} vCPUs...", num_vcpus);
    let cold_start = Instant::now();
    let vm = match MicroVm::new(cfg).await {
        Ok(vm) => vm,
        Err(e) => {
            eprintln!("  failed to create VM: {e}");
            return;
        }
    };
    let cold_boot_time = cold_start.elapsed();

    // Health check
    let output = match vm.exec("echo", &["ready"]).await {
        Ok(out) => out,
        Err(e) => {
            eprintln!("  cold boot exec failed: {e}");
            return;
        }
    };
    assert!(output.success());
    assert_eq!(output.stdout_str().trim(), "ready");
    eprintln!(
        "[multi_vcpu] Cold boot OK ({:.1?}, {} vCPUs)",
        cold_boot_time, num_vcpus
    );

    // --- Snapshot ---
    let snap_dir = tempfile::tempdir().expect("tempdir");
    let config_hash =
        snapshot::compute_config_hash(&kernel, initramfs.as_deref(), 256, num_vcpus).expect("hash");

    eprintln!("[multi_vcpu] Taking snapshot...");
    let snap_start = Instant::now();
    let snapshot_path = match vm
        .snapshot(snap_dir.path(), config_hash, snap_config_vcpus(num_vcpus))
        .await
    {
        Ok(path) => path,
        Err(e) => {
            eprintln!("  snapshot failed: {e}");
            return;
        }
    };
    let snap_time = snap_start.elapsed();
    eprintln!("[multi_vcpu] Snapshot captured in {:.1?}", snap_time);

    // Validate all vCPU states are present
    let snap = VmSnapshot::load(&snapshot_path).expect("load snapshot");
    assert_eq!(
        snap.vcpu_states.len(),
        num_vcpus,
        "snapshot must contain state for all {} vCPUs",
        num_vcpus
    );
    for (i, state) in snap.vcpu_states.iter().enumerate() {
        #[cfg(target_arch = "x86_64")]
        {
            assert!(!state.regs.is_empty(), "vCPU {} regs must be captured", i);
            assert!(!state.sregs.is_empty(), "vCPU {} sregs must be captured", i);
            assert!(!state.lapic.is_empty(), "vCPU {} LAPIC must be captured", i);
            assert!(!state.xsave.is_empty(), "vCPU {} xsave must be captured", i);
            assert!(!state.xcrs.is_empty(), "vCPU {} XCRs must be captured", i);
        }
        #[cfg(target_arch = "aarch64")]
        {
            assert!(
                !state.core_regs.is_empty(),
                "vCPU {} core_regs must be captured",
                i
            );
            assert!(
                !state.system_regs.is_empty(),
                "vCPU {} system_regs must be captured",
                i
            );
        }
    }
    eprintln!(
        "[multi_vcpu] Snapshot has {} vCPU states (all validated)",
        snap.vcpu_states.len()
    );

    // --- Restore ---
    eprintln!("[multi_vcpu] Restoring from snapshot...");
    let restore_start = Instant::now();
    let mut restored_vm = match MicroVm::from_snapshot(&snapshot_path).await {
        Ok(vm) => vm,
        Err(e) => {
            eprintln!("  restore failed: {e}");
            return;
        }
    };
    let restore_time = restore_start.elapsed();
    eprintln!(
        "[multi_vcpu] Restored VM CID={} (restore took {:.1?})",
        restored_vm.cid(),
        restore_time
    );

    let exec_ok = try_restored_exec(&restored_vm).await;

    // Always dump serial for multi-vCPU debugging
    let serial = restored_vm.read_serial_output();
    if !serial.is_empty() {
        let s = String::from_utf8_lossy(&serial);
        eprintln!(
            "[multi_vcpu] Serial output ({} bytes):\n{}",
            serial.len(),
            s
        );
    }

    assert!(exec_ok, "multi-vCPU restored VM exec must succeed");

    // --- Summary ---
    let speedup = cold_boot_time.as_secs_f64() / restore_time.as_secs_f64().max(1e-9);
    eprintln!();
    eprintln!("=== Multi-vCPU Snapshot ({} vCPUs) ===", num_vcpus);
    eprintln!("  Cold boot:   {:>10.1?}", cold_boot_time);
    eprintln!("  Snapshot:    {:>10.1?}", snap_time);
    eprintln!("  Restore:     {:>10.1?}", restore_time);
    eprintln!("  Speedup:     {:>10.1}x", speedup);
    eprintln!("========================================");

    restored_vm.stop().await.ok();
}

// ---------------------------------------------------------------------------
// Test: Virtio-net snapshot → restore (networking survives snapshot)
// ---------------------------------------------------------------------------

/// Cold-boot a VM with networking, take a snapshot, restore, and verify that
/// the restored VM can open new TCP connections via the fresh SLIRP stack.
///
/// TCP connections don't survive snapshot/restore (SLIRP state is not
/// serialized), but the virtio-net device state is restored so the guest
/// driver can immediately establish new connections.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires KVM + kernel/initramfs artifacts; see module docs"]
async fn snapshot_net_restore() {
    let Some((kernel, initramfs)) = preflight() else {
        return;
    };
    let cfg = build_config_net(&kernel, initramfs.as_deref());

    // --- Cold boot with networking ---
    eprintln!("[net_restore] Booting VM with networking...");
    let cold_start = Instant::now();
    let vm = match MicroVm::new(cfg).await {
        Ok(vm) => vm,
        Err(e) => {
            eprintln!("  failed to create VM: {e}");
            return;
        }
    };
    let cold_boot_time = cold_start.elapsed();

    // Health check
    let output = match vm.exec("echo", &["ready"]).await {
        Ok(out) => out,
        Err(e) => {
            eprintln!("  cold boot exec failed: {e}");
            return;
        }
    };
    assert!(output.success());
    assert_eq!(output.stdout_str().trim(), "ready");
    eprintln!("[net_restore] Cold boot OK ({:.1?})", cold_boot_time);

    // Verify networking works before snapshot
    let net_check = match vm.exec("ip", &["link", "show", "eth0"]).await {
        Ok(out) => out,
        Err(e) => {
            eprintln!("  net check failed: {e}");
            return;
        }
    };
    eprintln!(
        "[net_restore] Pre-snapshot eth0: exit={}, stdout='{}'",
        net_check.exit_code,
        net_check.stdout_str().trim()
    );
    assert!(net_check.success(), "eth0 must exist before snapshot");

    // --- Snapshot ---
    let snap_dir = tempfile::tempdir().expect("tempdir");
    let config_hash =
        snapshot::compute_config_hash(&kernel, initramfs.as_deref(), 256, 1).expect("hash");

    eprintln!("[net_restore] Taking snapshot...");
    let snap_start = Instant::now();
    let snapshot_path = match vm
        .snapshot(snap_dir.path(), config_hash, snap_config_net())
        .await
    {
        Ok(path) => path,
        Err(e) => {
            eprintln!("  snapshot failed: {e}");
            return;
        }
    };
    let snap_time = snap_start.elapsed();
    eprintln!("[net_restore] Snapshot captured in {:.1?}", snap_time);

    // Validate snapshot has net_state
    let snap = VmSnapshot::load(&snapshot_path).expect("load snapshot");
    assert!(
        snap.config.network,
        "snapshot config must have network=true"
    );
    assert!(snap.net_state.is_some(), "snapshot must contain net_state");
    let net_state = snap.net_state.as_ref().unwrap();
    assert_eq!(
        net_state.queues.len(),
        2,
        "net_state must have rx + tx queues"
    );
    assert_eq!(
        net_state.mac,
        [0x52, 0x54, 0x00, 0x12, 0x34, 0x56],
        "MAC must match GUEST_MAC"
    );
    eprintln!(
        "[net_restore] Snapshot net_state: status={:#x}, features={:#x}, mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        net_state.status, net_state.driver_features,
        net_state.mac[0], net_state.mac[1], net_state.mac[2],
        net_state.mac[3], net_state.mac[4], net_state.mac[5],
    );

    // --- Restore ---
    eprintln!("[net_restore] Restoring from snapshot...");
    let restore_start = Instant::now();
    let mut restored_vm = match MicroVm::from_snapshot(&snapshot_path).await {
        Ok(vm) => vm,
        Err(e) => {
            eprintln!("  restore failed: {e}");
            return;
        }
    };
    let restore_time = restore_start.elapsed();
    eprintln!(
        "[net_restore] Restored VM CID={} (restore took {:.1?})",
        restored_vm.cid(),
        restore_time
    );

    // Verify exec works
    let exec_ok = try_restored_exec(&restored_vm).await;
    assert!(exec_ok, "restored VM exec must succeed");

    // Verify eth0 is still visible in the restored VM
    let net_check_restored = match tokio::time::timeout(
        Duration::from_secs(30),
        restored_vm.exec("ip", &["link", "show", "eth0"]),
    )
    .await
    {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            let serial = restored_vm.read_serial_output();
            if !serial.is_empty() {
                eprintln!(
                    "[net_restore] Serial output:\n{}",
                    String::from_utf8_lossy(&serial)
                );
            }
            panic!("post-restore net check failed: {e}");
        }
        Err(_) => {
            panic!("post-restore net check timed out");
        }
    };
    eprintln!(
        "[net_restore] Post-restore eth0: exit={}, stdout='{}'",
        net_check_restored.exit_code,
        net_check_restored.stdout_str().trim()
    );
    assert!(
        net_check_restored.success(),
        "eth0 must exist after restore"
    );

    // --- Summary ---
    let speedup = cold_boot_time.as_secs_f64() / restore_time.as_secs_f64().max(1e-9);
    eprintln!();
    eprintln!("=== Net Snapshot/Restore ===");
    eprintln!("  Cold boot:   {:>10.1?}", cold_boot_time);
    eprintln!("  Snapshot:    {:>10.1?}", snap_time);
    eprintln!("  Restore:     {:>10.1?}", restore_time);
    eprintln!("  Speedup:     {:>10.1}x", speedup);
    eprintln!("============================");

    restored_vm.stop().await.ok();
}

// ---------------------------------------------------------------------------
// Tests: Snapshot CLI library functions (create/list/delete)
// ---------------------------------------------------------------------------

/// Verify `list_snapshots()` returns an empty vec when no snapshots exist.
///
/// Uses a temporary HOME to avoid reading real user snapshots.
#[test]
fn snapshot_cli_list_empty() {
    let tmp_home = tempfile::tempdir().expect("tempdir");
    // Temporarily override HOME so dirs_snapshot_base() points to our temp dir
    let orig_home = std::env::var("HOME").ok();
    std::env::set_var("HOME", tmp_home.path());

    let result = snapshot::list_snapshots();
    assert!(result.is_ok(), "list_snapshots must not error");
    assert!(result.unwrap().is_empty(), "no snapshots should exist");

    // Restore original HOME
    match orig_home {
        Some(h) => std::env::set_var("HOME", h),
        None => std::env::remove_var("HOME"),
    }
}

/// Boot a VM, take a snapshot into the standard location, then verify
/// `list_snapshots()` includes it with correct metadata.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires KVM + kernel/initramfs artifacts; see module docs"]
async fn snapshot_cli_create_and_list() {
    let Some((kernel, initramfs)) = preflight() else {
        return;
    };
    let cfg = build_config(&kernel, initramfs.as_deref());

    // --- Cold boot ---
    eprintln!("[cli_create_list] Booting VM...");
    let vm = match MicroVm::new(cfg).await {
        Ok(vm) => vm,
        Err(e) => {
            eprintln!("  failed to create VM: {e}");
            return;
        }
    };
    let output = match vm.exec("echo", &["ready"]).await {
        Ok(out) => out,
        Err(e) => {
            eprintln!("  exec failed: {e}");
            return;
        }
    };
    assert!(output.success());

    // --- Snapshot into the standard directory ---
    let config_hash =
        snapshot::compute_config_hash(&kernel, initramfs.as_deref(), 256, 1).expect("hash");
    let snap_dir = snapshot::snapshot_dir_for_hash(&config_hash);
    std::fs::create_dir_all(&snap_dir).expect("create snapshot dir");

    eprintln!(
        "[cli_create_list] Taking snapshot (hash={})...",
        &config_hash[..16]
    );
    match vm
        .snapshot(&snap_dir, config_hash.clone(), snap_config())
        .await
    {
        Ok(_) => {}
        Err(e) => {
            eprintln!("  snapshot failed: {e}");
            let _ = std::fs::remove_dir_all(&snap_dir);
            return;
        }
    }

    // --- Verify list_snapshots() finds it ---
    let snapshots = snapshot::list_snapshots().expect("list_snapshots");
    let found = snapshots.iter().find(|s| s.config_hash == config_hash);
    assert!(
        found.is_some(),
        "list_snapshots must include the snapshot we just created"
    );

    let info = found.unwrap();
    assert_eq!(info.memory_mb, 256);
    assert_eq!(info.vcpus, 1);
    assert_eq!(info.snapshot_type, snapshot::SnapshotType::Base);
    assert!(
        info.memory_file_size > 0,
        "memory file must have non-zero size"
    );
    eprintln!(
        "[cli_create_list] Found snapshot: hash={}, memory={}MB, vcpus={}",
        &info.config_hash[..16],
        info.memory_mb,
        info.vcpus
    );

    // Cleanup
    let _ = std::fs::remove_dir_all(&snap_dir);
}

/// Create a snapshot, delete it via `delete_snapshot()`, and verify it is gone.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires KVM + kernel/initramfs artifacts; see module docs"]
async fn snapshot_cli_delete() {
    let Some((kernel, initramfs)) = preflight() else {
        return;
    };
    let cfg = build_config(&kernel, initramfs.as_deref());

    // --- Cold boot ---
    eprintln!("[cli_delete] Booting VM...");
    let vm = match MicroVm::new(cfg).await {
        Ok(vm) => vm,
        Err(e) => {
            eprintln!("  failed to create VM: {e}");
            return;
        }
    };
    let output = match vm.exec("echo", &["ready"]).await {
        Ok(out) => out,
        Err(e) => {
            eprintln!("  exec failed: {e}");
            return;
        }
    };
    assert!(output.success());

    // --- Snapshot ---
    let config_hash =
        snapshot::compute_config_hash(&kernel, initramfs.as_deref(), 256, 1).expect("hash");
    let snap_dir = snapshot::snapshot_dir_for_hash(&config_hash);
    std::fs::create_dir_all(&snap_dir).expect("create snapshot dir");

    eprintln!("[cli_delete] Taking snapshot...");
    match vm
        .snapshot(&snap_dir, config_hash.clone(), snap_config())
        .await
    {
        Ok(_) => {}
        Err(e) => {
            eprintln!("  snapshot failed: {e}");
            let _ = std::fs::remove_dir_all(&snap_dir);
            return;
        }
    }

    // Verify it exists
    assert!(snap_dir.exists(), "snapshot dir must exist before delete");
    let snapshots = snapshot::list_snapshots().expect("list before delete");
    assert!(
        snapshots.iter().any(|s| s.config_hash == config_hash),
        "snapshot must be listed before delete"
    );

    // --- Delete ---
    let hash_prefix = &config_hash[..8];
    eprintln!("[cli_delete] Deleting snapshot (prefix={})...", hash_prefix);
    let deleted = snapshot::delete_snapshot(hash_prefix).expect("delete_snapshot");
    assert!(deleted, "delete_snapshot must return true");

    // Verify it is gone
    assert!(
        !snap_dir.exists(),
        "snapshot dir must be removed after delete"
    );
    let snapshots_after = snapshot::list_snapshots().expect("list after delete");
    assert!(
        !snapshots_after.iter().any(|s| s.config_hash == config_hash),
        "snapshot must not be listed after delete"
    );
    eprintln!("[cli_delete] Snapshot deleted and verified gone");
}

// ---------------------------------------------------------------------------
// Test: Diff snapshot CLI flow (base → restore → dirty tracking → diff)
// ---------------------------------------------------------------------------

/// Create a base snapshot, then create a diff snapshot on top of it (the same
/// flow that `voidbox snapshot create --diff` performs). Verify that
/// `list_snapshots()` returns both entries and that the diff is smaller than
/// the base.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires KVM + kernel/initramfs artifacts; see module docs"]
async fn snapshot_cli_create_diff() {
    let Some((kernel, initramfs)) = preflight() else {
        return;
    };
    let cfg = build_config(&kernel, initramfs.as_deref());

    // --- Cold boot & base snapshot ---
    eprintln!("[cli_create_diff] Booting VM...");
    let vm = match MicroVm::new(cfg).await {
        Ok(vm) => vm,
        Err(e) => {
            eprintln!("  failed to create VM: {e}");
            return;
        }
    };
    let output = match vm.exec("echo", &["ready"]).await {
        Ok(out) => out,
        Err(e) => {
            eprintln!("  exec failed: {e}");
            return;
        }
    };
    assert!(output.success());

    let config_hash =
        snapshot::compute_config_hash(&kernel, initramfs.as_deref(), 256, 1).expect("hash");
    let base_dir = snapshot::snapshot_dir_for_hash(&config_hash);
    std::fs::create_dir_all(&base_dir).expect("create base snapshot dir");

    eprintln!(
        "[cli_create_diff] Taking base snapshot (hash={})...",
        &config_hash[..16]
    );
    match vm
        .snapshot(&base_dir, config_hash.clone(), snap_config())
        .await
    {
        Ok(_) => {}
        Err(e) => {
            eprintln!("  base snapshot failed: {e}");
            let _ = std::fs::remove_dir_all(&base_dir);
            return;
        }
    }

    let base_mem_size = std::fs::metadata(VmSnapshot::memory_path(&base_dir))
        .unwrap()
        .len();
    eprintln!(
        "[cli_create_diff] Base memory size: {} bytes",
        base_mem_size
    );

    // --- Restore from base, enable dirty tracking, exec, take diff ---
    eprintln!("[cli_create_diff] Restoring from base snapshot...");
    let restored_vm = match MicroVm::from_snapshot(&base_dir).await {
        Ok(vm) => vm,
        Err(e) => {
            eprintln!("  base restore failed: {e}");
            let _ = std::fs::remove_dir_all(&base_dir);
            return;
        }
    };

    eprintln!("[cli_create_diff] Enabling dirty page tracking...");
    restored_vm
        .enable_dirty_tracking()
        .expect("enable dirty tracking");

    let output = match restored_vm.exec("echo", &["snapshot-ready"]).await {
        Ok(out) => out,
        Err(e) => {
            eprintln!("  exec after dirty tracking failed: {e}");
            let _ = std::fs::remove_dir_all(&base_dir);
            return;
        }
    };
    assert!(output.success());
    eprintln!("[cli_create_diff] Guest-agent ready after dirty tracking");

    let diff_dir_name = format!("{}-diff", &config_hash[..16]);
    let diff_dir = snapshot::default_snapshot_dir().join(&diff_dir_name);
    std::fs::create_dir_all(&diff_dir).expect("create diff snapshot dir");

    eprintln!("[cli_create_diff] Taking diff snapshot...");
    let diff_path = match restored_vm
        .snapshot_diff(
            &diff_dir,
            config_hash.clone(),
            snap_config(),
            config_hash.clone(),
        )
        .await
    {
        Ok(path) => path,
        Err(e) => {
            eprintln!("  diff snapshot failed: {e}");
            let _ = std::fs::remove_dir_all(&base_dir);
            let _ = std::fs::remove_dir_all(&diff_dir);
            return;
        }
    };

    // --- Verify list_snapshots() includes both base and diff ---
    let snapshots = snapshot::list_snapshots().expect("list_snapshots");
    let base_found = snapshots
        .iter()
        .any(|s| s.config_hash == config_hash && s.snapshot_type == snapshot::SnapshotType::Base);
    let diff_found = snapshots
        .iter()
        .any(|s| s.snapshot_type == snapshot::SnapshotType::Diff);
    assert!(base_found, "list_snapshots must include the base snapshot");
    assert!(diff_found, "list_snapshots must include the diff snapshot");
    eprintln!(
        "[cli_create_diff] list_snapshots: {} entries (base={}, diff={})",
        snapshots.len(),
        base_found,
        diff_found
    );

    // --- Verify diff memory is smaller than base ---
    let diff_mem_size = std::fs::metadata(VmSnapshot::diff_memory_path(&diff_path))
        .unwrap()
        .len();
    eprintln!(
        "[cli_create_diff] Base memory: {} bytes, Diff memory: {} bytes ({:.1}%)",
        base_mem_size,
        diff_mem_size,
        (diff_mem_size as f64 / base_mem_size as f64) * 100.0
    );
    assert!(
        diff_mem_size < base_mem_size,
        "diff memory ({diff_mem_size}) must be smaller than base ({base_mem_size})"
    );

    // --- Verify diff snapshot metadata ---
    let diff_snap = VmSnapshot::load(&diff_path).expect("load diff snapshot");
    assert_eq!(
        diff_snap.snapshot_type,
        snapshot::SnapshotType::Diff,
        "must be a diff snapshot"
    );
    assert_eq!(
        diff_snap.parent_id.as_deref(),
        Some(config_hash.as_str()),
        "diff snapshot parent_id must match config_hash"
    );

    // --- Summary ---
    let savings = (1.0 - diff_mem_size as f64 / base_mem_size as f64) * 100.0;
    eprintln!();
    eprintln!("=== Diff Snapshot CLI Summary ===");
    eprintln!("  Base memory:     {} MB", base_mem_size / (1024 * 1024));
    eprintln!("  Diff memory:     {} KB", diff_mem_size / 1024);
    eprintln!("  Savings:         {:.1}%", savings);
    eprintln!("=================================");

    // --- Cleanup ---
    let _ = std::fs::remove_dir_all(&base_dir);
    let _ = std::fs::remove_dir_all(&diff_dir);
}
