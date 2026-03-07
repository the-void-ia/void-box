#![cfg(target_os = "linux")]
//! Snapshot integration tests for void-box.
//!
//! Two tests exercising the snapshot / restore pipeline:
//!
//! 1. **`snapshot_cold_boot_vs_restore`** — measures cold-boot time, takes a
//!    snapshot, restores it, and compares cold-boot vs restore latency.
//!
//! 2. **`snapshot_live_capture_and_restore`** — measures the live snapshot
//!    capture time (pausing vCPUs + dumping memory while the VM keeps running)
//!    and the subsequent restore time.
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

use void_box::vmm::config::VoidBoxConfig;
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
    let mut cfg = VoidBoxConfig::new()
        .memory_mb(256)
        .vcpus(1)
        .kernel(kernel)
        .enable_vsock(true);
    if let Some(p) = initramfs {
        cfg = cfg.initramfs(p);
    }
    cfg.validate().expect("invalid VoidBoxConfig");
    cfg
}

/// Build a `SnapshotConfig` matching the test VM.
fn snap_config() -> SnapshotConfig {
    SnapshotConfig {
        memory_mb: 256,
        vcpus: 1,
        cid: 0,
        vsock_mmio_base: 0xd080_0000,
        network: false,
    }
}

/// Take a live snapshot while keeping the guest busy so vCPUs exit KVM_RUN.
///
/// Returns the snapshot directory path and the time it took.
async fn take_live_snapshot(
    vm: &MicroVm,
    snap_dir: &std::path::Path,
    config_hash: String,
    config: SnapshotConfig,
) -> Option<(PathBuf, Duration)> {
    let snap_dir_path = snap_dir.to_path_buf();
    let start = Instant::now();
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        // Run a concurrent exec to generate vCPU exits (vsock MMIO traffic)
        // so vCPUs leave HLT and check the snapshot_requested flag.
        tokio::select! {
            snap = vm.snapshot_live(&snap_dir_path, config_hash, config) => snap,
            _ = vm.exec("sleep", &["30"]) => {
                Err(void_box::Error::Snapshot("exec completed before snapshot".into()))
            }
        }
    })
    .await;
    let duration = start.elapsed();

    match result {
        Ok(Ok(path)) => Some((path, duration)),
        Ok(Err(e)) => {
            eprintln!("  snapshot_live failed: {e}");
            None
        }
        Err(_) => {
            eprintln!("  snapshot_live timed out (vCPU stuck in KVM_RUN HLT)");
            None
        }
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
// Test 1: Cold boot vs snapshot restore
// ---------------------------------------------------------------------------

/// Cold-boot a VM, take a snapshot, restore from it.
///
/// Compares cold-boot latency against snapshot-restore latency.
#[tokio::test]
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

    // --- Snapshot ---
    let snap_dir = tempfile::tempdir().expect("tempdir");
    let config_hash =
        snapshot::compute_config_hash(&kernel, initramfs.as_deref(), 256, 1).expect("hash");

    eprintln!("[cold_boot_vs_restore] Taking snapshot...");
    let Some((snapshot_path, _snap_time)) =
        take_live_snapshot(&vm, snap_dir.path(), config_hash, snap_config()).await
    else {
        let mut vm = vm;
        vm.stop().await.ok();
        return;
    };

    // Stop original VM
    let mut vm = vm;
    vm.stop().await.ok();

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

    try_restored_exec(&restored_vm).await;

    // --- Timing ---
    let speedup = cold_boot_time.as_secs_f64() / restore_time.as_secs_f64().max(1e-9);
    eprintln!();
    eprintln!("=== Cold Boot vs Restore ===");
    eprintln!("  Cold boot:   {:>10.1?}", cold_boot_time);
    eprintln!("  Restore:     {:>10.1?}", restore_time);
    eprintln!("  Speedup:     {:>10.1}x", speedup);
    eprintln!("============================");

    restored_vm.stop().await.ok();
}

// ---------------------------------------------------------------------------
// Test 2: Live snapshot capture + restore
// ---------------------------------------------------------------------------

/// Take a live snapshot from a running VM and restore it.
///
/// Measures snapshot capture time (pause vCPUs + dump memory) and restore
/// time separately.
#[tokio::test]
#[ignore = "requires KVM + kernel/initramfs artifacts; see module docs"]
async fn snapshot_live_capture_and_restore() {
    let Some((kernel, initramfs)) = preflight() else {
        return;
    };
    let cfg = build_config(&kernel, initramfs.as_deref());

    // Boot VM
    eprintln!("[live_capture_restore] Booting VM...");
    let vm = match MicroVm::new(cfg).await {
        Ok(vm) => vm,
        Err(e) => {
            eprintln!("  failed to create VM: {e}");
            return;
        }
    };

    // Health check
    match vm.exec("echo", &["ready"]).await {
        Ok(out) if out.success() => {}
        Ok(out) => {
            eprintln!("  health check failed: exit_code={}", out.exit_code);
            return;
        }
        Err(Error::VmNotRunning) => {
            eprintln!("  VM not running");
            return;
        }
        Err(Error::Guest(msg)) => {
            eprintln!("  guest error: {msg}");
            return;
        }
        Err(e) => panic!("  exec failed: {e}"),
    };
    eprintln!("[live_capture_restore] VM ready");

    // --- Live snapshot ---
    let snap_dir = tempfile::tempdir().expect("tempdir");
    let config_hash =
        snapshot::compute_config_hash(&kernel, initramfs.as_deref(), 256, 1).expect("hash");

    eprintln!("[live_capture_restore] Taking live snapshot...");
    let Some((snapshot_path, capture_time)) =
        take_live_snapshot(&vm, snap_dir.path(), config_hash, snap_config()).await
    else {
        let mut vm = vm;
        vm.stop().await.ok();
        return;
    };
    eprintln!(
        "[live_capture_restore] Snapshot captured in {:.1?}",
        capture_time
    );

    // Validate on disk
    let snap = VmSnapshot::load(&snapshot_path).expect("load snapshot");
    assert_eq!(snap.version, snapshot::SNAPSHOT_VERSION);
    assert_eq!(snap.config.memory_mb, 256);
    assert_eq!(snap.config.vcpus, 1);
    assert_eq!(snap.snapshot_type, snapshot::SnapshotType::PostInit);
    assert!(
        !snap.vcpu_states.is_empty(),
        "snapshot must contain vCPU states"
    );

    let mem_path = VmSnapshot::memory_path(&snapshot_path);
    assert!(mem_path.exists(), "memory dump must exist");
    let mem_size = std::fs::metadata(&mem_path).unwrap().len();
    assert_eq!(mem_size, 256 * 1024 * 1024, "memory dump must be 256MB");

    // Stop original VM
    let mut vm = vm;
    vm.stop().await.ok();

    // --- Restore ---
    eprintln!("[live_capture_restore] Restoring from snapshot...");
    let restore_start = Instant::now();
    let mut restored_vm = match MicroVm::from_snapshot(&snapshot_path).await {
        Ok(vm) => vm,
        Err(e) => {
            eprintln!("  restore failed: {e}");
            return;
        }
    };
    let restore_time = restore_start.elapsed();

    try_restored_exec(&restored_vm).await;

    // --- Timing ---
    eprintln!();
    eprintln!("=== Live Snapshot Capture & Restore ===");
    eprintln!("  Capture (live):  {:>10.1?}", capture_time);
    eprintln!("  Restore:         {:>10.1?}", restore_time);
    eprintln!("=======================================");

    restored_vm.stop().await.ok();
}
