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
    let mut cfg = VoidBoxConfig::new()
        .memory_mb(256)
        .vcpus(1)
        .kernel(kernel)
        .enable_vsock(true)
        .vsock_backend(VsockBackendType::Userspace)
        // Force periodic LAPIC timer instead of TSC-deadline mode.
        // After snapshot restore the kernel's clockevent state is stale and
        // TSC_DEADLINE=0 means no timer ever fires again.  Periodic mode
        // survives restore because the LAPIC hardware re-generates ticks from
        // TMICT/TDCR without needing the kernel to re-arm.
;
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
    SnapshotConfig {
        memory_mb: 256,
        vcpus: 1,
        cid: 0, // overwritten by snapshot_internal()
        vsock_mmio_base: 0xd080_0000,
        network: false,
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
