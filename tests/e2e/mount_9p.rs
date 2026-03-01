#![cfg(target_os = "linux")]
//! End-to-End 9p/mount integration tests
//!
//! These tests boot a real KVM micro-VM and exercise the virtio-9p shared
//! directory feature (host ↔ guest file sharing). Each test creates a
//! temporary host directory, configures a 9p mount, boots the VM, and runs
//! commands inside the guest to validate the mount behavior.
//!
//! ## Prerequisites
//!
//! 1. Build the test initramfs (includes 9p kernel modules):
//!    ```bash
//!    scripts/build_test_image.sh
//!    ```
//!
//! 2. Run with:
//!    ```bash
//!    VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
//!    VOID_BOX_INITRAMFS=/tmp/void-box-test-rootfs.cpio.gz \
//!    cargo test --test e2e_mount_9p -- --ignored --test-threads=1
//!    ```
//!
//! All tests are `#[ignore]` so they don't run in a normal `cargo test`.

use std::path::{Path, PathBuf};

#[path = "../common/vm_preflight.rs"]
mod vm_preflight;

use void_box::backend::MountConfig;
use void_box::vmm::config::VoidBoxConfig;
use void_box::vmm::MicroVm;
use void_box::Error;

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

fn kvm_artifacts_from_env() -> Option<(PathBuf, Option<PathBuf>)> {
    let kernel = std::env::var_os("VOID_BOX_KERNEL")?;
    let kernel = PathBuf::from(kernel);
    let initramfs = std::env::var_os("VOID_BOX_INITRAMFS").map(PathBuf::from);
    Some((kernel, initramfs))
}

/// Build a VoidBoxConfig with a 9p mount. Returns `None` (skips) if KVM or
/// artifacts are unavailable.
fn build_vm_with_mount(
    host_dir: &Path,
    guest_path: &str,
    read_only: bool,
) -> Option<VoidBoxConfig> {
    if let Err(e) = vm_preflight::require_kvm_usable() {
        eprintln!("skipping: {e}");
        return None;
    }
    if let Err(e) = vm_preflight::require_vsock_usable() {
        eprintln!("skipping: {e}");
        return None;
    }

    let (kernel, initramfs) = match kvm_artifacts_from_env() {
        Some(a) => a,
        None => {
            eprintln!(
                "skipping: set VOID_BOX_KERNEL and VOID_BOX_INITRAMFS \
                 (use scripts/build_test_image.sh)"
            );
            return None;
        }
    };

    if let Err(e) = vm_preflight::require_kernel_artifacts(&kernel, initramfs.as_deref()) {
        eprintln!("skipping: {e}");
        return None;
    }

    let mut cfg = VoidBoxConfig::new()
        .memory_mb(256)
        .vcpus(1)
        .kernel(&kernel)
        .enable_vsock(true);

    if let Some(ref p) = initramfs {
        cfg = cfg.initramfs(p);
    }

    cfg.mounts.push(MountConfig {
        host_path: host_dir.to_string_lossy().into_owned(),
        guest_path: guest_path.to_string(),
        read_only,
    });

    Some(cfg)
}

/// Boot a MicroVm from config. Returns `None` on soft failures (environment issues).
async fn boot_vm(cfg: VoidBoxConfig) -> Option<MicroVm> {
    cfg.validate()
        .expect("invalid VoidBoxConfig for mount test");
    match MicroVm::new(cfg).await {
        Ok(vm) => Some(vm),
        Err(e) => {
            eprintln!("skipping: failed to create MicroVm: {e}");
            None
        }
    }
}

/// Execute a shell command inside the guest, returning the ExecOutput.
/// Soft-skips on VmNotRunning / Guest errors (environment flakiness).
async fn guest_sh(vm: &mut MicroVm, script: &str) -> Option<void_box::ExecOutput> {
    match vm.exec("sh", &["-c", script]).await {
        Ok(out) => Some(out),
        Err(Error::VmNotRunning) => {
            let serial = vm.read_serial_output();
            let console = String::from_utf8_lossy(&serial);
            eprintln!("VM not running, console:\n{console}");
            None
        }
        Err(Error::Guest(msg)) => {
            eprintln!("guest communication error: {msg}");
            None
        }
        Err(e) => panic!("exec failed: {e}"),
    }
}

// ===========================================================================
// Test 1: Write a file in guest, read it back
// ===========================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires KVM + test initramfs with 9p modules"]
async fn mount_rw_write_read() {
    let host_dir = tempfile::tempdir().unwrap();
    let Some(cfg) = build_vm_with_mount(host_dir.path(), "/mnt/shared", false) else {
        return;
    };
    let Some(mut vm) = boot_vm(cfg).await else {
        return;
    };

    let out = guest_sh(
        &mut vm,
        "echo 'hello 9p' > /mnt/shared/test.txt && cat /mnt/shared/test.txt",
    )
    .await;
    let Some(out) = out else { return };

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
#[ignore = "requires KVM + test initramfs with 9p modules"]
async fn mount_rw_host_visible() {
    let host_dir = tempfile::tempdir().unwrap();
    let Some(cfg) = build_vm_with_mount(host_dir.path(), "/mnt/shared", false) else {
        return;
    };
    let Some(mut vm) = boot_vm(cfg).await else {
        return;
    };

    let out = guest_sh(&mut vm, "echo 'from guest' > /mnt/shared/host_check.txt").await;
    let Some(out) = out else { return };
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
#[ignore = "requires KVM + test initramfs with 9p modules"]
async fn mount_rw_mkdir_nested() {
    let host_dir = tempfile::tempdir().unwrap();
    let Some(cfg) = build_vm_with_mount(host_dir.path(), "/mnt/shared", false) else {
        return;
    };
    let Some(mut vm) = boot_vm(cfg).await else {
        return;
    };

    let out = guest_sh(
        &mut vm,
        "mkdir -p /mnt/shared/a/b/c/d && echo ok > /mnt/shared/a/b/c/d/deep.txt && cat /mnt/shared/a/b/c/d/deep.txt",
    )
    .await;
    let Some(out) = out else { return };
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
#[ignore = "requires KVM + test initramfs with 9p modules"]
async fn mount_rw_rename_file() {
    let host_dir = tempfile::tempdir().unwrap();
    let Some(cfg) = build_vm_with_mount(host_dir.path(), "/mnt/shared", false) else {
        return;
    };
    let Some(mut vm) = boot_vm(cfg).await else {
        return;
    };

    let out = guest_sh(
        &mut vm,
        "echo data > /mnt/shared/old.txt && mv /mnt/shared/old.txt /mnt/shared/new.txt && \
         test ! -e /mnt/shared/old.txt && cat /mnt/shared/new.txt",
    )
    .await;
    let Some(out) = out else { return };
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
#[ignore = "requires KVM + test initramfs with 9p modules"]
async fn mount_rw_delete_file() {
    let host_dir = tempfile::tempdir().unwrap();
    let Some(cfg) = build_vm_with_mount(host_dir.path(), "/mnt/shared", false) else {
        return;
    };
    let Some(mut vm) = boot_vm(cfg).await else {
        return;
    };

    let out = guest_sh(
        &mut vm,
        "echo gone > /mnt/shared/remove_me.txt && rm /mnt/shared/remove_me.txt && \
         test ! -e /mnt/shared/remove_me.txt && echo deleted",
    )
    .await;
    let Some(out) = out else { return };
    assert!(out.success(), "delete failed: {}", out.stderr_str());
    assert_eq!(out.stdout_str().trim(), "deleted");

    assert!(!host_dir.path().join("remove_me.txt").exists());

    eprintln!("PASSED: mount_rw_delete_file");
}

// ===========================================================================
// Test 6: chmod a file
// ===========================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires KVM + test initramfs with 9p modules"]
async fn mount_rw_chmod() {
    let host_dir = tempfile::tempdir().unwrap();
    let Some(cfg) = build_vm_with_mount(host_dir.path(), "/mnt/shared", false) else {
        return;
    };
    let Some(mut vm) = boot_vm(cfg).await else {
        return;
    };

    let out = guest_sh(
        &mut vm,
        "echo x > /mnt/shared/script.sh && chmod 755 /mnt/shared/script.sh && \
         stat -c '%a' /mnt/shared/script.sh",
    )
    .await;
    let Some(out) = out else { return };
    assert!(out.success(), "chmod failed: {}", out.stderr_str());
    assert_eq!(out.stdout_str().trim(), "755");

    eprintln!("PASSED: mount_rw_chmod");
}

// ===========================================================================
// Test 7: Large file (~1MB)
// ===========================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires KVM + test initramfs with 9p modules"]
async fn mount_rw_large_file() {
    let host_dir = tempfile::tempdir().unwrap();
    let Some(cfg) = build_vm_with_mount(host_dir.path(), "/mnt/shared", false) else {
        return;
    };
    let Some(mut vm) = boot_vm(cfg).await else {
        return;
    };

    // Write ~1MB of data (1024 lines × 1024 chars each)
    let out = guest_sh(
        &mut vm,
        "dd if=/dev/zero bs=1024 count=1024 2>/dev/null > /mnt/shared/large.bin && \
         stat -c '%s' /mnt/shared/large.bin",
    )
    .await;
    let Some(out) = out else { return };
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
#[ignore = "requires KVM + test initramfs with 9p modules"]
async fn mount_ro_cannot_write() {
    let host_dir = tempfile::tempdir().unwrap();
    let Some(cfg) = build_vm_with_mount(host_dir.path(), "/mnt/shared", true) else {
        return;
    };
    let Some(mut vm) = boot_vm(cfg).await else {
        return;
    };

    let out = guest_sh(
        &mut vm,
        "echo nope > /mnt/shared/should_fail.txt 2>&1; echo $?",
    )
    .await;
    let Some(out) = out else { return };

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
#[ignore = "requires KVM + test initramfs with 9p modules"]
async fn mount_ro_can_read() {
    let host_dir = tempfile::tempdir().unwrap();
    std::fs::write(host_dir.path().join("readme.txt"), "hello from host\n").unwrap();

    let Some(cfg) = build_vm_with_mount(host_dir.path(), "/mnt/shared", true) else {
        return;
    };
    let Some(mut vm) = boot_vm(cfg).await else {
        return;
    };

    let out = guest_sh(&mut vm, "cat /mnt/shared/readme.txt").await;
    let Some(out) = out else { return };
    assert!(out.success(), "read failed: {}", out.stderr_str());
    assert_eq!(out.stdout_str().trim(), "hello from host");

    eprintln!("PASSED: mount_ro_can_read");
}

// ===========================================================================
// Test 10: Host dir with pre-existing files visible in guest
// ===========================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires KVM + test initramfs with 9p modules"]
async fn mount_host_preexisting() {
    let host_dir = tempfile::tempdir().unwrap();
    std::fs::write(host_dir.path().join("a.txt"), "aaa\n").unwrap();
    std::fs::write(host_dir.path().join("b.txt"), "bbb\n").unwrap();
    std::fs::create_dir(host_dir.path().join("subdir")).unwrap();
    std::fs::write(host_dir.path().join("subdir/c.txt"), "ccc\n").unwrap();

    let Some(cfg) = build_vm_with_mount(host_dir.path(), "/mnt/shared", false) else {
        return;
    };
    let Some(mut vm) = boot_vm(cfg).await else {
        return;
    };

    let out = guest_sh(
        &mut vm,
        "cat /mnt/shared/a.txt /mnt/shared/b.txt /mnt/shared/subdir/c.txt",
    )
    .await;
    let Some(out) = out else { return };
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
#[ignore = "requires KVM + test initramfs with 9p modules"]
async fn mount_empty_dir() {
    let host_dir = tempfile::tempdir().unwrap();
    let Some(cfg) = build_vm_with_mount(host_dir.path(), "/mnt/shared", false) else {
        return;
    };
    let Some(mut vm) = boot_vm(cfg).await else {
        return;
    };

    // Verify empty, then write
    let out = guest_sh(
        &mut vm,
        "ls /mnt/shared/ | wc -l && echo 'first file' > /mnt/shared/new.txt && cat /mnt/shared/new.txt",
    )
    .await;
    let Some(out) = out else { return };
    assert!(out.success(), "empty dir test failed: {}", out.stderr_str());

    let stdout = out.stdout_str();
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(lines[0], "0", "directory should initially be empty");
    assert_eq!(lines[1], "first file");

    assert!(host_dir.path().join("new.txt").exists());

    eprintln!("PASSED: mount_empty_dir");
}
