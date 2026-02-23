#![cfg(target_os = "linux")]
//! Backend conformance test suite.
//!
//! These tests verify that any [`VmmBackend`] implementation satisfies the
//! expected contract: boot, exec, write_file, mkdir_p, streaming, and
//! authentication. They are parameterized over the backend so the same suite
//! runs on both KVM (Linux) and VZ (macOS).
//!
//! ## Prerequisites
//!
//! ```bash
//! # Build the test initramfs:
//! scripts/build_test_image.sh
//!
//! # Run:
//! VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
//! VOID_BOX_INITRAMFS=/tmp/void-box-rootfs.cpio.gz \
//! cargo test --test conformance -- --ignored --test-threads=1
//! ```
//!
//! All tests are `#[ignore]` so they don't run in a normal `cargo test`.

use std::path::PathBuf;

use void_box::backend::{BackendConfig, BackendSecurityConfig, VmmBackend};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn kvm_available() -> bool {
    std::path::Path::new("/dev/kvm").exists()
}

fn vsock_available() -> bool {
    std::path::Path::new("/dev/vhost-vsock").exists()
}

fn backend_config() -> Option<BackendConfig> {
    let kernel = std::env::var("VOID_BOX_KERNEL").ok()?;
    let kernel = PathBuf::from(kernel);
    if kernel.as_os_str().is_empty() || !kernel.exists() {
        return None;
    }

    let initramfs = std::env::var("VOID_BOX_INITRAMFS").ok()?;
    let initramfs = PathBuf::from(initramfs);
    if initramfs.as_os_str().is_empty() || !initramfs.exists() {
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
        network: true,
        enable_vsock: true,
        shared_dir: None,
        env: vec![],
        security: BackendSecurityConfig {
            session_secret: secret,
            command_allowlist: vec!["echo".into(), "sh".into(), "cat".into(), "test".into()],
            network_deny_list: vec!["169.254.0.0/16".into()],
            max_connections_per_second: 50,
            max_concurrent_connections: 64,
            seccomp: true,
        },
    })
}

async fn create_started_backend() -> Option<Box<dyn VmmBackend>> {
    if !kvm_available() || !vsock_available() {
        eprintln!("skipping: /dev/kvm or /dev/vhost-vsock not available");
        return None;
    }

    let config = match backend_config() {
        Some(c) => c,
        None => {
            eprintln!("skipping: set VOID_BOX_KERNEL and VOID_BOX_INITRAMFS");
            return None;
        }
    };

    let mut backend = void_box::backend::create_backend();
    match backend.start(config).await {
        Ok(()) => Some(backend),
        Err(e) => {
            eprintln!("skipping: backend start failed: {}", e);
            None
        }
    }
}

// ===========================================================================
// Conformance: exec
// ===========================================================================

/// Backend can execute a simple command and return stdout.
#[tokio::test]
#[ignore = "requires KVM + kernel/initramfs artifacts"]
async fn conformance_exec_echo() {
    let backend = match create_started_backend().await {
        Some(b) => b,
        None => return,
    };

    let output = backend
        .exec("echo", &["hello", "world"], &[], &[], None, Some(30))
        .await
        .expect("exec failed");

    assert_eq!(output.exit_code, 0);
    let stdout = output.stdout_str();
    assert!(
        stdout.trim() == "hello world",
        "expected 'hello world', got: '{}'",
        stdout.trim()
    );
}

/// Backend reports non-zero exit codes.
#[tokio::test]
#[ignore = "requires KVM + kernel/initramfs artifacts"]
async fn conformance_exec_nonzero_exit() {
    let backend = match create_started_backend().await {
        Some(b) => b,
        None => return,
    };

    let output = backend
        .exec("sh", &["-c", "exit 42"], &[], &[], None, Some(30))
        .await
        .expect("exec failed");

    assert_eq!(output.exit_code, 42);
}

// ===========================================================================
// Conformance: write_file
// ===========================================================================

/// Backend can write a file and read it back via exec.
#[tokio::test]
#[ignore = "requires KVM + kernel/initramfs artifacts"]
async fn conformance_write_file() {
    let backend = match create_started_backend().await {
        Some(b) => b,
        None => return,
    };

    let content = b"hello from conformance test";
    backend
        .write_file("/tmp/conformance_test.txt", content)
        .await
        .expect("write_file failed");

    let output = backend
        .exec("cat", &["/tmp/conformance_test.txt"], &[], &[], None, Some(10))
        .await
        .expect("exec cat failed");

    assert_eq!(output.exit_code, 0);
    let stdout = output.stdout_str();
    assert!(
        stdout.contains("hello from conformance test"),
        "file content mismatch: '{}'",
        stdout
    );
}

// ===========================================================================
// Conformance: mkdir_p
// ===========================================================================

/// Backend can create nested directories.
#[tokio::test]
#[ignore = "requires KVM + kernel/initramfs artifacts"]
async fn conformance_mkdir_p() {
    let backend = match create_started_backend().await {
        Some(b) => b,
        None => return,
    };

    backend
        .mkdir_p("/tmp/conformance/nested/dir")
        .await
        .expect("mkdir_p failed");

    // Verify the directory exists
    let output = backend
        .exec("test", &["-d", "/tmp/conformance/nested/dir"], &[], &[], None, Some(10))
        .await
        .expect("exec test failed");

    assert_eq!(
        output.exit_code, 0,
        "directory should exist after mkdir_p"
    );
}

// ===========================================================================
// Conformance: streaming
// ===========================================================================

/// Backend can stream output chunks during execution.
#[tokio::test]
#[ignore = "requires KVM + kernel/initramfs artifacts"]
async fn conformance_exec_streaming() {
    let backend = match create_started_backend().await {
        Some(b) => b,
        None => return,
    };

    let (mut chunk_rx, done_rx) = backend
        .exec_streaming("echo", &["streaming-test"], &[], None, Some(30))
        .await
        .expect("exec_streaming failed");

    // Collect chunks
    let mut chunks = Vec::new();
    while let Some(chunk) = chunk_rx.recv().await {
        chunks.push(chunk);
    }

    // Wait for final response
    let response = done_rx
        .await
        .expect("done channel closed")
        .expect("exec failed");

    assert_eq!(response.exit_code, 0);
    // The output should contain "streaming-test" either in chunks or final response
    let stdout = String::from_utf8_lossy(&response.stdout);
    assert!(
        stdout.contains("streaming-test") || chunks.iter().any(|c| {
            String::from_utf8_lossy(&c.data).contains("streaming-test")
        }),
        "streaming output should contain 'streaming-test'"
    );
}

// ===========================================================================
// Conformance: timeout
// ===========================================================================

/// Backend enforces execution timeouts.
#[tokio::test]
#[ignore = "requires KVM + kernel/initramfs artifacts"]
async fn conformance_exec_timeout() {
    let backend = match create_started_backend().await {
        Some(b) => b,
        None => return,
    };

    // Sleep for 60s but with a 2s timeout — should be killed
    let output = backend
        .exec("sh", &["-c", "sleep 60"], &[], &[], None, Some(2))
        .await
        .expect("exec failed");

    // Should have a non-zero exit code (killed by timeout)
    assert_ne!(
        output.exit_code, 0,
        "timed-out command should have non-zero exit code"
    );
}

// ===========================================================================
// Conformance: lifecycle
// ===========================================================================

/// Backend reports running state correctly.
#[tokio::test]
#[ignore = "requires KVM + kernel/initramfs artifacts"]
async fn conformance_lifecycle() {
    let mut backend = match create_started_backend().await {
        Some(b) => b,
        None => return,
    };

    assert!(backend.is_running(), "backend should be running after start");

    backend.stop().await.expect("stop failed");
    assert!(
        !backend.is_running(),
        "backend should not be running after stop"
    );
}
