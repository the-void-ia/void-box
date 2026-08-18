//! End-to-End mount integration tests
//!
//! These tests boot a real VM and exercise host↔guest directory sharing via
//! virtio-9p (Linux/KVM) or virtiofs (macOS/VZ). Each test creates a temporary
//! host directory, configures a mount, boots the VM, and runs commands inside
//! the guest to validate the mount behavior.
//!
//! Kernel and initramfs are auto-provisioned by [`test_artifacts`] under
//! `--ignored`; `VOID_BOX_KERNEL` / `VOID_BOX_INITRAMFS` are optional overrides.
//! All tests are `#[ignore]`, so a plain `cargo test` never provisions or boots.
//!
//! ```bash
//! cargo test --test e2e_mount -- --ignored --test-threads=1
//! ```

use std::path::Path;

#[path = "../common/test_artifacts.rs"]
mod test_artifacts;

use void_box::backend::{
    BackendConfig, BackendSecurityConfig, GuestConsoleSink, MountConfig, VmmBackend,
};
use void_box_protocol::SessionSecret;

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// Build a `BackendConfig` with a mount, auto-provisioning the test artifacts.
fn build_config_with_mount(host_dir: &Path, guest_path: &str, read_only: bool) -> BackendConfig {
    let (kernel, initramfs) = test_artifacts::artifacts();

    let mut secret = [0u8; 32];
    getrandom::fill(&mut secret).expect("getrandom");

    BackendConfig {
        memory_mb: 256,
        vcpus: 1,
        kernel,
        initramfs: Some(initramfs),
        rootfs: None,
        network: true,
        enable_vsock: true,
        guest_console: GuestConsoleSink::Stderr,
        shared_dir: None,
        mounts: vec![MountConfig {
            host_path: host_dir.to_string_lossy().into_owned(),
            guest_path: guest_path.to_string(),
            read_only,
        }],
        oci_rootfs: None,
        oci_rootfs_dev: None,
        oci_rootfs_disk: None,
        env: vec![],
        security: BackendSecurityConfig {
            session_secret: SessionSecret::new(secret),
            command_allowlist: vec![
                "sh".into(),
                "cat".into(),
                "echo".into(),
                "mkdir".into(),
                "rm".into(),
                "mv".into(),
                "chmod".into(),
                "stat".into(),
                "dd".into(),
                "ls".into(),
                "wc".into(),
                "test".into(),
                "grep".into(),
            ],
            network_deny_list: vec!["169.254.0.0/16".into()],
            max_connections_per_second: 50,
            max_concurrent_connections: 64,
            seccomp: true,
        },
        snapshot: None,
        enable_snapshots: false,
    }
}

/// Create and start a VM backend with a mount, or `None` when the host
/// genuinely cannot virtualize (the caller skips) — the skip-or-fail contract
/// lives on [`test_artifacts::start_backend`].
async fn create_started_backend_with_mount(
    host_dir: &Path,
    guest_path: &str,
    read_only: bool,
) -> Option<Box<dyn VmmBackend>> {
    let config = build_config_with_mount(host_dir, guest_path, read_only);
    test_artifacts::start_backend(config, "backend start (mount)").await
}

/// Execute a shell command inside the guest, returning the ExecOutput.
async fn guest_sh(backend: &dyn VmmBackend, script: &str) -> void_box::ExecOutput {
    test_artifacts::expect_vm(
        backend
            .exec("sh", &["-c", script], &[], &[], None, Some(30))
            .await,
        "guest exec (mount)",
    )
}

// ===========================================================================
// Test 1: Write a file in guest, read it back
// ===========================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires VM backend + kernel/initramfs artifacts"]
async fn mount_rw_write_read() {
    let host_dir = tempfile::tempdir().unwrap();
    let Some(backend) =
        create_started_backend_with_mount(host_dir.path(), "/mnt/shared", false).await
    else {
        return;
    };

    let out = guest_sh(
        &*backend,
        "echo 'hello 9p' > /mnt/shared/test.txt && cat /mnt/shared/test.txt",
    )
    .await;

    assert!(
        out.success(),
        "write+read failed: exit={} stderr={}",
        out.exit_code,
        out.stderr_str()
    );
    assert_eq!(out.stdout_str().trim(), "hello 9p");

    eprintln!("PASSED: mount_rw_write_read");
}

// ===========================================================================
// Test 2: Write in guest → verify on host
// ===========================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires VM backend + kernel/initramfs artifacts"]
async fn mount_rw_host_visible() {
    let host_dir = tempfile::tempdir().unwrap();
    let Some(backend) =
        create_started_backend_with_mount(host_dir.path(), "/mnt/shared", false).await
    else {
        return;
    };

    let out = guest_sh(&*backend, "echo 'from guest' > /mnt/shared/host_check.txt").await;
    assert!(out.success(), "write failed: {}", out.stderr_str());

    let host_file = host_dir.path().join("host_check.txt");
    assert!(host_file.exists(), "file should appear on host");
    let content = std::fs::read_to_string(&host_file).unwrap();
    assert_eq!(content.trim(), "from guest");

    eprintln!("PASSED: mount_rw_host_visible");
}

// ===========================================================================
// Test 3: Create nested directories
// ===========================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires VM backend + kernel/initramfs artifacts"]
async fn mount_rw_mkdir_nested() {
    let host_dir = tempfile::tempdir().unwrap();
    let Some(backend) =
        create_started_backend_with_mount(host_dir.path(), "/mnt/shared", false).await
    else {
        return;
    };

    let out = guest_sh(
        &*backend,
        "mkdir -p /mnt/shared/a/b/c/d && echo ok > /mnt/shared/a/b/c/d/deep.txt && cat /mnt/shared/a/b/c/d/deep.txt",
    )
    .await;
    assert!(out.success(), "mkdir -p failed: {}", out.stderr_str());
    assert_eq!(out.stdout_str().trim(), "ok");

    let deep = host_dir.path().join("a/b/c/d/deep.txt");
    assert!(deep.exists(), "nested file should exist on host");

    eprintln!("PASSED: mount_rw_mkdir_nested");
}

// ===========================================================================
// Test 4: Rename a file
// ===========================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires VM backend + kernel/initramfs artifacts"]
async fn mount_rw_rename_file() {
    let host_dir = tempfile::tempdir().unwrap();
    let Some(backend) =
        create_started_backend_with_mount(host_dir.path(), "/mnt/shared", false).await
    else {
        return;
    };

    let out = guest_sh(
        &*backend,
        "echo data > /mnt/shared/old.txt && mv /mnt/shared/old.txt /mnt/shared/new.txt && \
         test ! -e /mnt/shared/old.txt && cat /mnt/shared/new.txt",
    )
    .await;
    assert!(out.success(), "rename failed: {}", out.stderr_str());
    assert_eq!(out.stdout_str().trim(), "data");

    assert!(!host_dir.path().join("old.txt").exists());
    assert!(host_dir.path().join("new.txt").exists());

    eprintln!("PASSED: mount_rw_rename_file");
}

// ===========================================================================
// Test 5: Delete a file
// ===========================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires VM backend + kernel/initramfs artifacts"]
async fn mount_rw_delete_file() {
    let host_dir = tempfile::tempdir().unwrap();
    let Some(backend) =
        create_started_backend_with_mount(host_dir.path(), "/mnt/shared", false).await
    else {
        return;
    };

    let out = guest_sh(
        &*backend,
        "echo gone > /mnt/shared/remove_me.txt && rm /mnt/shared/remove_me.txt && \
         test ! -e /mnt/shared/remove_me.txt && echo deleted",
    )
    .await;
    assert!(out.success(), "delete failed: {}", out.stderr_str());
    assert_eq!(out.stdout_str().trim(), "deleted");

    assert!(!host_dir.path().join("remove_me.txt").exists());

    eprintln!("PASSED: mount_rw_delete_file");
}

// ===========================================================================
// Test 6: chmod a file
// ===========================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires VM backend + kernel/initramfs artifacts"]
async fn mount_rw_chmod() {
    let host_dir = tempfile::tempdir().unwrap();
    let Some(backend) =
        create_started_backend_with_mount(host_dir.path(), "/mnt/shared", false).await
    else {
        return;
    };

    let out = guest_sh(
        &*backend,
        "echo x > /mnt/shared/script.sh && chmod 755 /mnt/shared/script.sh && \
         stat -c '%a' /mnt/shared/script.sh",
    )
    .await;
    assert!(out.success(), "chmod failed: {}", out.stderr_str());
    assert_eq!(out.stdout_str().trim(), "755");

    eprintln!("PASSED: mount_rw_chmod");
}

// ===========================================================================
// Test 7: Large file (~1MB)
// ===========================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires VM backend + kernel/initramfs artifacts"]
async fn mount_rw_large_file() {
    let host_dir = tempfile::tempdir().unwrap();
    let Some(backend) =
        create_started_backend_with_mount(host_dir.path(), "/mnt/shared", false).await
    else {
        return;
    };

    // Write ~1MB of data (1024 lines × 1024 chars each)
    let out = guest_sh(
        &*backend,
        "dd if=/dev/zero bs=1024 count=1024 2>/dev/null > /mnt/shared/large.bin && \
         stat -c '%s' /mnt/shared/large.bin",
    )
    .await;
    assert!(
        out.success(),
        "large file write failed: {}",
        out.stderr_str()
    );

    let size: u64 = out.stdout_str().trim().parse().expect("parse size");
    assert_eq!(size, 1024 * 1024, "file should be exactly 1MB");

    let host_file = host_dir.path().join("large.bin");
    assert!(host_file.exists());
    let host_size = std::fs::metadata(&host_file).unwrap().len();
    assert_eq!(host_size, 1024 * 1024);

    eprintln!("PASSED: mount_rw_large_file");
}

// ===========================================================================
// Test 8: Read-only mount rejects writes
// ===========================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires VM backend + kernel/initramfs artifacts"]
async fn mount_ro_cannot_write() {
    let host_dir = tempfile::tempdir().unwrap();
    let Some(backend) =
        create_started_backend_with_mount(host_dir.path(), "/mnt/shared", true).await
    else {
        return;
    };

    let out = guest_sh(
        &*backend,
        "echo nope > /mnt/shared/should_fail.txt 2>&1; echo $?",
    )
    .await;

    // The echo $? captures the exit code of the write attempt
    let stdout = out.stdout_str();
    let last_line = stdout.trim().lines().last().unwrap_or("");
    assert_ne!(
        last_line, "0",
        "write to RO mount should fail (got exit code 0)"
    );
    assert!(
        !host_dir.path().join("should_fail.txt").exists(),
        "file should not appear on host"
    );

    eprintln!("PASSED: mount_ro_cannot_write");
}

// ===========================================================================
// Test 9: Read-only mount can read pre-populated files
// ===========================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires VM backend + kernel/initramfs artifacts"]
async fn mount_ro_can_read() {
    let host_dir = tempfile::tempdir().unwrap();
    std::fs::write(host_dir.path().join("readme.txt"), "hello from host\n").unwrap();

    let Some(backend) =
        create_started_backend_with_mount(host_dir.path(), "/mnt/shared", true).await
    else {
        return;
    };

    let out = guest_sh(&*backend, "cat /mnt/shared/readme.txt").await;
    assert!(out.success(), "read failed: {}", out.stderr_str());
    assert_eq!(out.stdout_str().trim(), "hello from host");

    eprintln!("PASSED: mount_ro_can_read");
}

// ===========================================================================
// Test 10: Host dir with pre-existing files visible in guest
// ===========================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires VM backend + kernel/initramfs artifacts"]
async fn mount_host_preexisting() {
    let host_dir = tempfile::tempdir().unwrap();
    std::fs::write(host_dir.path().join("a.txt"), "aaa\n").unwrap();
    std::fs::write(host_dir.path().join("b.txt"), "bbb\n").unwrap();
    std::fs::create_dir(host_dir.path().join("subdir")).unwrap();
    std::fs::write(host_dir.path().join("subdir/c.txt"), "ccc\n").unwrap();

    let Some(backend) =
        create_started_backend_with_mount(host_dir.path(), "/mnt/shared", false).await
    else {
        return;
    };

    let out = guest_sh(
        &*backend,
        "cat /mnt/shared/a.txt /mnt/shared/b.txt /mnt/shared/subdir/c.txt",
    )
    .await;
    assert!(
        out.success(),
        "read pre-existing failed: {}",
        out.stderr_str()
    );

    let stdout = out.stdout_str();
    assert!(stdout.contains("aaa"));
    assert!(stdout.contains("bbb"));
    assert!(stdout.contains("ccc"));

    eprintln!("PASSED: mount_host_preexisting");
}

// ===========================================================================
// Test 11: Empty host dir — guest sees empty, can write
// ===========================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires VM backend + kernel/initramfs artifacts"]
async fn mount_empty_dir() {
    let host_dir = tempfile::tempdir().unwrap();
    let Some(backend) =
        create_started_backend_with_mount(host_dir.path(), "/mnt/shared", false).await
    else {
        return;
    };

    // Verify empty, then write
    let out = guest_sh(
        &*backend,
        "ls /mnt/shared/ | wc -l && echo 'first file' > /mnt/shared/new.txt && cat /mnt/shared/new.txt",
    )
    .await;
    assert!(out.success(), "empty dir test failed: {}", out.stderr_str());

    let stdout = out.stdout_str();
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(lines[0], "0", "directory should initially be empty");
    assert_eq!(lines[1], "first file");

    assert!(host_dir.path().join("new.txt").exists());

    eprintln!("PASSED: mount_empty_dir");
}
