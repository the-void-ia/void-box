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
use void_box::backend::{BackendConfig, BackendSecurityConfig, GuestConsoleSink, VmmBackend};
use void_box::snapshot_store;

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
        guest_console: GuestConsoleSink::Stderr,
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

    restored.start(restore_cfg).await.expect("restore failed");
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

/// Cold-boot a VZ VM, create an auto-snapshot, and verify the restored VM can
/// still execute commands without manual intervention.
///
/// Auto-snapshot semantics: save the live VM, stop it, restart from the saved
/// state so the caller ends up running against the restored VM (matching the
/// KVM backend). The post-snapshot exec below exercises the restored VM.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires macOS + VZ entitlements + kernel/initramfs artifacts"]
async fn auto_snapshot_vz_round_trip() {
    let config = match backend_config() {
        Some(c) => c,
        None => {
            eprintln!("skipping: set VOID_BOX_KERNEL and VOID_BOX_INITRAMFS");
            return;
        }
    };

    eprintln!("[vz_auto_snapshot] Booting VM...");
    let mut backend = VzBackend::new();
    if let Err(e) = backend.start(config).await {
        eprintln!("[vz_auto_snapshot] start failed: {e}");
        return;
    }

    let output = backend
        .exec("echo", &["before-auto-snap"], &[], &[], None, Some(30))
        .await
        .expect("exec failed before auto-snapshot");
    assert_eq!(output.exit_code, 0);
    assert_eq!(output.stdout_str().trim(), "before-auto-snap");

    let snap_dir = tempfile::tempdir().expect("tempdir");
    let config_hash = "vz-auto-snapshot-test-hash".to_string();
    eprintln!(
        "[vz_auto_snapshot] Creating auto-snapshot at {}...",
        snap_dir.path().display()
    );
    backend
        .create_auto_snapshot(snap_dir.path(), config_hash.clone())
        .await
        .expect("create_auto_snapshot failed");

    let save_path = VzSnapshotMeta::save_file_path(snap_dir.path());
    assert!(save_path.exists(), "vm.vzvmsave must exist after save step");
    let meta = VzSnapshotMeta::load(snap_dir.path()).expect("load vz_meta.json");
    assert_eq!(meta.config_hash.as_deref(), Some(config_hash.as_str()));
    assert!(
        meta.machine_identifier.is_some(),
        "sidecar must carry the VZGenericMachineIdentifier for restore"
    );

    assert!(
        backend.is_running(),
        "backend should be running against the restored VM"
    );
    let output = backend
        .exec("echo", &["after-auto-snap"], &[], &[], None, Some(30))
        .await
        .expect("exec failed after auto-snapshot restore");
    assert_eq!(output.exit_code, 0);
    assert_eq!(output.stdout_str().trim(), "after-auto-snap");
    backend.stop().await.expect("stop failed");
}

// ---------------------------------------------------------------------------
// CLI-level tests: snapshot_store list / delete / exists with VZ snapshots
// ---------------------------------------------------------------------------

/// Boot a VM, create a snapshot into the standard snapshot directory, then
/// verify `snapshot_store::list_snapshots()` discovers it with correct metadata.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires macOS + VZ entitlements + kernel/initramfs artifacts"]
async fn snapshot_vz_cli_create_and_list() {
    let config = match backend_config() {
        Some(c) => c,
        None => {
            eprintln!("skipping: set VOID_BOX_KERNEL and VOID_BOX_INITRAMFS");
            return;
        }
    };

    let kernel = config.kernel.clone();
    let initramfs = config.initramfs.clone();

    // --- Cold boot ---
    eprintln!("[vz_cli_list] Booting VM...");
    let mut backend = VzBackend::new();
    if let Err(e) = backend.start(config).await {
        eprintln!("[vz_cli_list] start failed: {e}");
        return;
    }

    let output = backend
        .exec("echo", &["ready"], &[], &[], None, Some(30))
        .await
        .expect("exec failed");
    assert!(output.success());

    // --- Create snapshot into the standard directory ---
    let config_hash = snapshot_store::compute_config_hash(&kernel, initramfs.as_deref(), 256, 1)
        .expect("compute_config_hash");
    let snap_dir = snapshot_store::snapshot_dir_for_hash(&config_hash);
    std::fs::create_dir_all(&snap_dir).expect("create snapshot dir");

    eprintln!(
        "[vz_cli_list] Taking snapshot (hash={})...",
        &config_hash[..16]
    );
    tokio::task::block_in_place(|| backend.create_snapshot(&snap_dir))
        .expect("create_snapshot failed");

    // --- Verify snapshot_exists recognizes VZ snapshot ---
    assert!(
        snapshot_store::snapshot_exists(&snap_dir),
        "snapshot_exists must return true for VZ snapshot"
    );

    // --- Verify list_snapshots() finds it ---
    let snapshots = snapshot_store::list_snapshots().expect("list_snapshots");
    let found = snapshots.iter().find(|s| s.dir == snap_dir);
    assert!(
        found.is_some(),
        "list_snapshots must include the VZ snapshot we just created"
    );

    let info = found.unwrap();
    assert_eq!(info.memory_mb, 256);
    assert_eq!(info.vcpus, 1);
    assert_eq!(info.snapshot_type, snapshot_store::SnapshotType::Base);
    eprintln!(
        "[vz_cli_list] Found VZ snapshot: hash={}, memory={}MB, vcpus={}",
        info.config_hash, info.memory_mb, info.vcpus
    );

    // Cleanup
    backend.stop().await.expect("stop failed");
    let _ = std::fs::remove_dir_all(&snap_dir);
}

/// Create a VZ snapshot, delete it via `snapshot_store::delete_snapshot()`,
/// and verify it is gone from both the filesystem and `list_snapshots()`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires macOS + VZ entitlements + kernel/initramfs artifacts"]
async fn snapshot_vz_cli_delete() {
    let config = match backend_config() {
        Some(c) => c,
        None => {
            eprintln!("skipping: set VOID_BOX_KERNEL and VOID_BOX_INITRAMFS");
            return;
        }
    };

    let kernel = config.kernel.clone();
    let initramfs = config.initramfs.clone();

    // --- Cold boot ---
    eprintln!("[vz_cli_delete] Booting VM...");
    let mut backend = VzBackend::new();
    if let Err(e) = backend.start(config).await {
        eprintln!("[vz_cli_delete] start failed: {e}");
        return;
    }

    let output = backend
        .exec("echo", &["ready"], &[], &[], None, Some(30))
        .await
        .expect("exec failed");
    assert!(output.success());

    // --- Snapshot ---
    let config_hash = snapshot_store::compute_config_hash(&kernel, initramfs.as_deref(), 256, 1)
        .expect("compute_config_hash");
    let snap_dir = snapshot_store::snapshot_dir_for_hash(&config_hash);
    std::fs::create_dir_all(&snap_dir).expect("create snapshot dir");

    eprintln!("[vz_cli_delete] Taking snapshot...");
    tokio::task::block_in_place(|| backend.create_snapshot(&snap_dir))
        .expect("create_snapshot failed");

    // Verify it exists before delete
    assert!(snap_dir.exists(), "snapshot dir must exist before delete");
    assert!(snapshot_store::snapshot_exists(&snap_dir));
    let snapshots = snapshot_store::list_snapshots().expect("list before delete");
    assert!(
        snapshots.iter().any(|s| s.dir == snap_dir),
        "snapshot must be listed before delete"
    );

    // --- Delete ---
    let hash_prefix = &config_hash[..8];
    eprintln!(
        "[vz_cli_delete] Deleting snapshot (prefix={})...",
        hash_prefix
    );
    let deleted = snapshot_store::delete_snapshot(hash_prefix).expect("delete_snapshot");
    assert!(deleted, "delete_snapshot must return true");

    // Verify it is gone
    assert!(
        !snap_dir.exists(),
        "snapshot dir must be removed after delete"
    );
    let snapshots_after = snapshot_store::list_snapshots().expect("list after delete");
    assert!(
        !snapshots_after.iter().any(|s| s.dir == snap_dir),
        "snapshot must not be listed after delete"
    );
    eprintln!("[vz_cli_delete] Snapshot deleted and verified gone");

    // Cleanup
    backend.stop().await.expect("stop failed");
}
