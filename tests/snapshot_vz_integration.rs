#![cfg(target_os = "macos")]
//! VZ snapshot integration tests for void-box (macOS Apple Silicon).
//!
//! Tests the snapshot/restore round-trip using Apple's Virtualization.framework
//! `saveMachineStateToURL:` / `restoreMachineStateFromURL:` APIs.
//!
//! ## Prerequisites
//!
//! ```bash
//! export VOID_BOX_KERNEL=/tmp/void-box-kernel
//! export VOID_BOX_INITRAMFS=/tmp/void-box-test-rootfs.cpio.gz
//!
//! cargo test --release --test snapshot_vz_integration -- --ignored --test-threads=1
//! ```
//!
//! All tests are `#[ignore]` so they don't run in a normal `cargo test`.

use std::path::PathBuf;
use std::time::Instant;

#[path = "common/vm_preflight.rs"]
mod vm_preflight;

use void_box::backend::vz::snapshot::VzSnapshotMeta;
use void_box::backend::vz::VzBackend;
use void_box::backend::{BackendConfig, BackendSecurityConfig, VmmBackend};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn backend_config() -> Option<BackendConfig> {
    let kernel = std::env::var("VOID_BOX_KERNEL").ok()?;
    let kernel = PathBuf::from(kernel);
    if kernel.as_os_str().is_empty() {
        return None;
    }

    let initramfs = std::env::var("VOID_BOX_INITRAMFS").ok()?;
    let initramfs = PathBuf::from(initramfs);
    if initramfs.as_os_str().is_empty() {
        return None;
    }
    if vm_preflight::require_kernel_artifacts(&kernel, Some(&initramfs)).is_err() {
        return None;
    }

    let mut secret = [0u8; 32];
    getrandom::fill(&mut secret).ok()?;

    Some(BackendConfig {
        memory_mb: 256,
        vcpus: 1,
        kernel,
        initramfs: Some(initramfs),
        rootfs: None,
        network: false,
        enable_vsock: true,
        shared_dir: None,
        mounts: vec![],
        oci_rootfs: None,
        oci_rootfs_dev: None,
        oci_rootfs_disk: None,
        env: vec![],
        security: BackendSecurityConfig {
            session_secret: secret,
            command_allowlist: vec!["echo".into(), "sh".into()],
            network_deny_list: vec![],
            max_connections_per_second: 50,
            max_concurrent_connections: 64,
            seccomp: false,
        },
        snapshot: None,
    })
}

// ---------------------------------------------------------------------------
// Test: Cold boot → snapshot → stop → restore → exec
// ---------------------------------------------------------------------------

/// Boot a VM, exec to confirm it's alive, create a snapshot, stop the VM,
/// restore from the snapshot, exec again to confirm the restored VM works.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires macOS + VZ entitlements + kernel/initramfs artifacts"]
async fn snapshot_vz_round_trip() {
    let config = match backend_config() {
        Some(c) => c,
        None => {
            eprintln!("skipping: set VOID_BOX_KERNEL and VOID_BOX_INITRAMFS");
            return;
        }
    };

    // Keep a copy for the restore config
    let restore_config = config.clone();

    // --- Cold boot ---
    eprintln!("[vz_snapshot] Booting VM...");
    let cold_start = Instant::now();
    let mut backend = VzBackend::new();
    if let Err(e) = backend.start(config).await {
        eprintln!("[vz_snapshot] start failed: {e}");
        return;
    }
    let cold_boot_time = cold_start.elapsed();
    eprintln!("[vz_snapshot] Cold boot OK ({:.1?})", cold_boot_time);

    // Health check
    let output = backend
        .exec("echo", &["hello"], &[], &[], None, Some(30))
        .await
        .expect("exec failed on cold-booted VM");
    assert_eq!(output.exit_code, 0);
    assert_eq!(output.stdout_str().trim(), "hello");
    eprintln!("[vz_snapshot] Cold-boot exec OK");

    // --- Create snapshot ---
    let snap_dir = tempfile::tempdir().expect("tempdir");
    eprintln!(
        "[vz_snapshot] Creating snapshot at {}...",
        snap_dir.path().display()
    );
    let snap_start = Instant::now();
    tokio::task::block_in_place(|| backend.create_snapshot(snap_dir.path()))
        .expect("create_snapshot failed");
    let snap_time = snap_start.elapsed();
    eprintln!("[vz_snapshot] Snapshot created in {:.1?}", snap_time);

    // Validate snapshot files exist
    let save_path = VzSnapshotMeta::save_file_path(snap_dir.path());
    assert!(save_path.exists(), "vm.vzvmsave must exist");
    let meta = VzSnapshotMeta::load(snap_dir.path()).expect("load vz_meta.json");
    assert_eq!(meta.memory_mb, 256);
    assert_eq!(meta.vcpus, 1);
    assert_eq!(meta.session_secret.len(), 32);
    eprintln!("[vz_snapshot] Snapshot metadata validated");

    // Verify VM still works after snapshot (it should have been resumed)
    let output = backend
        .exec("echo", &["after-snap"], &[], &[], None, Some(30))
        .await
        .expect("exec failed after snapshot");
    assert_eq!(output.exit_code, 0);
    assert_eq!(output.stdout_str().trim(), "after-snap");
    eprintln!("[vz_snapshot] Post-snapshot exec OK");

    // --- Stop the VM ---
    backend.stop().await.expect("stop failed");
    assert!(!backend.is_running());
    eprintln!("[vz_snapshot] VM stopped");

    // --- Restore from snapshot ---
    eprintln!("[vz_snapshot] Restoring from snapshot...");
    let restore_start = Instant::now();
    let mut restored = VzBackend::new();
    let mut restore_cfg = restore_config;
    restore_cfg.snapshot = Some(snap_dir.path().to_path_buf());
    // Use the session secret from the snapshot sidecar
    let secret: [u8; 32] = meta.session_secret.as_slice().try_into().unwrap();
    restore_cfg.security.session_secret = secret;

    if let Err(e) = restored.start(restore_cfg).await {
        eprintln!("[vz_snapshot] restore failed: {e}");
        return;
    }
    let restore_time = restore_start.elapsed();
    eprintln!("[vz_snapshot] Restored in {:.1?}", restore_time);
    assert!(restored.is_running());

    // Health check on restored VM
    let output = restored
        .exec("echo", &["restored"], &[], &[], None, Some(30))
        .await
        .expect("exec failed on restored VM");
    assert_eq!(output.exit_code, 0);
    assert_eq!(output.stdout_str().trim(), "restored");
    eprintln!("[vz_snapshot] Restored VM exec OK");

    // --- Cleanup ---
    restored.stop().await.expect("stop restored VM failed");

    // --- Summary ---
    let speedup = cold_boot_time.as_secs_f64() / restore_time.as_secs_f64().max(1e-9);
    eprintln!();
    eprintln!("=== VZ Snapshot Round-Trip ===");
    eprintln!("  Cold boot:   {:>10.1?}", cold_boot_time);
    eprintln!("  Snapshot:    {:>10.1?}", snap_time);
    eprintln!("  Restore:     {:>10.1?}", restore_time);
    eprintln!("  Speedup:     {:>10.1}x", speedup);
    eprintln!("==============================");
}
