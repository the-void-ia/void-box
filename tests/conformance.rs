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

#[path = "common/vm_preflight.rs"]
mod vm_preflight;

use void_box::backend::{BackendConfig, BackendSecurityConfig, GuestConsoleSink, VmmBackend};
use void_box::sidecar;
use void_box_protocol::SessionSecret;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Returns `true` when the platform's VM backend is available.
fn backend_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        vm_preflight::require_kvm_usable().is_ok() && vm_preflight::require_vsock_usable().is_ok()
    }
    #[cfg(target_os = "macos")]
    {
        true // Virtualization.framework is always available on macOS 13+
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        false
    }
}

fn backend_config() -> Option<BackendConfig> {
    backend_config_with(
        vec!["169.254.0.0/16".into()],
        vec!["echo".into(), "sh".into(), "cat".into(), "test".into()],
    )
}

fn backend_config_with(
    network_deny_list: Vec<String>,
    command_allowlist: Vec<String>,
) -> Option<BackendConfig> {
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
        network: true,
        enable_vsock: true,
        guest_console: GuestConsoleSink::Stderr,
        shared_dir: None,
        mounts: vec![],
        oci_rootfs: None,
        oci_rootfs_dev: None,
        oci_rootfs_disk: None,
        env: vec![],
        security: BackendSecurityConfig {
            session_secret: SessionSecret::new(secret),
            command_allowlist,
            network_deny_list,
            max_connections_per_second: 50,
            max_concurrent_connections: 64,
            seccomp: true,
        },
        snapshot: None,
        enable_snapshots: false,
    })
}

async fn create_started_backend() -> Option<Box<dyn VmmBackend>> {
    if !backend_available() {
        eprintln!("skipping: VM backend not available on this platform");
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

async fn create_started_backend_with_config(config: BackendConfig) -> Option<Box<dyn VmmBackend>> {
    if !backend_available() {
        eprintln!("skipping: VM backend not available on this platform");
        return None;
    }

    let mut backend = void_box::backend::create_backend();
    match backend.start(config).await {
        Ok(()) => Some(backend),
        Err(e) => {
            eprintln!("skipping: backend start failed: {}", e);
            None
        }
    }
}

async fn guest_sh(backend: &dyn VmmBackend, script: &str) -> Option<void_box::ExecOutput> {
    match backend
        .exec("sh", &["-c", script], &[], &[], None, Some(30))
        .await
    {
        Ok(out) => Some(out),
        Err(e) => {
            eprintln!("guest exec error: {e}");
            None
        }
    }
}

// ===========================================================================
// Conformance: exec
// ===========================================================================

/// Backend can execute a simple command and return stdout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires VM backend + kernel/initramfs artifacts"]
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
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires VM backend + kernel/initramfs artifacts"]
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
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires VM backend + kernel/initramfs artifacts"]
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
        .exec(
            "cat",
            &["/tmp/conformance_test.txt"],
            &[],
            &[],
            None,
            Some(10),
        )
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
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires VM backend + kernel/initramfs artifacts"]
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
        .exec(
            "test",
            &["-d", "/tmp/conformance/nested/dir"],
            &[],
            &[],
            None,
            Some(10),
        )
        .await
        .expect("exec test failed");

    assert_eq!(output.exit_code, 0, "directory should exist after mkdir_p");
}

// ===========================================================================
// Conformance: streaming
// ===========================================================================

/// Backend can stream output chunks during execution.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires VM backend + kernel/initramfs artifacts"]
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
        stdout.contains("streaming-test")
            || chunks
                .iter()
                .any(|c| { String::from_utf8_lossy(&c.data).contains("streaming-test") }),
        "streaming output should contain 'streaming-test'"
    );
}

// ===========================================================================
// Conformance: timeout
// ===========================================================================

/// Backend enforces execution timeouts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires VM backend + kernel/initramfs artifacts"]
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
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires VM backend + kernel/initramfs artifacts"]
async fn conformance_lifecycle() {
    let mut backend = match create_started_backend().await {
        Some(b) => b,
        None => return,
    };

    assert!(
        backend.is_running(),
        "backend should be running after start"
    );

    backend.stop().await.expect("stop failed");
    assert!(
        !backend.is_running(),
        "backend should not be running after stop"
    );
}

// ===========================================================================
// Conformance: network deny-list parity
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires VM backend + kernel/initramfs artifacts + network"]
async fn conformance_network_deny_list_blocks_guest_host_gateway() {
    let config = match backend_config_with(
        vec![format!("{}/32", void_box::backend::guest_host_gateway())],
        vec![
            "echo".into(),
            "sh".into(),
            "cat".into(),
            "test".into(),
            "wget".into(),
        ],
    ) {
        Some(c) => c,
        None => {
            eprintln!("skipping: set VOID_BOX_KERNEL and VOID_BOX_INITRAMFS");
            return;
        }
    };

    let backend = match create_started_backend_with_config(config).await {
        Some(b) => b,
        None => return,
    };

    let handle = sidecar::start_sidecar(
        "run-conformance-deny",
        "exec-conformance-deny",
        "c-1",
        vec![],
        void_box::backend::guest_accessible_bind_addr(0),
    )
    .await
    .expect("failed to start sidecar");

    let port = handle.addr().port();
    let script = format!(
        "wget -T 2 -q -O - {}/v1/health >/tmp/conformance-deny.out 2>/tmp/conformance-deny.err; echo $?",
        void_box::backend::guest_host_url(port)
    );
    let out = guest_sh(&*backend, &script).await;
    let Some(out) = out else {
        handle.stop().await;
        return;
    };

    assert_eq!(
        out.exit_code,
        0,
        "shell wrapper failed: {}",
        out.stderr_str()
    );
    assert_ne!(
        out.stdout_str().trim(),
        "0",
        "guest unexpectedly reached deny-listed host gateway"
    );

    handle.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires VM backend + kernel/initramfs artifacts + network"]
async fn conformance_unrelated_network_deny_list_preserves_guest_host_gateway_access() {
    let config = match backend_config_with(
        vec!["203.0.113.0/24".into()],
        vec![
            "echo".into(),
            "sh".into(),
            "cat".into(),
            "test".into(),
            "wget".into(),
        ],
    ) {
        Some(c) => c,
        None => {
            eprintln!("skipping: set VOID_BOX_KERNEL and VOID_BOX_INITRAMFS");
            return;
        }
    };

    let backend = match create_started_backend_with_config(config).await {
        Some(b) => b,
        None => return,
    };

    let handle = sidecar::start_sidecar(
        "run-conformance-allow",
        "exec-conformance-allow",
        "c-1",
        vec![],
        void_box::backend::guest_accessible_bind_addr(0),
    )
    .await
    .expect("failed to start sidecar");

    let port = handle.addr().port();
    let script = format!(
        "wget -T 2 -q -O - {}/v1/health",
        void_box::backend::guest_host_url(port)
    );
    let out = guest_sh(&*backend, &script).await;
    let Some(out) = out else {
        handle.stop().await;
        return;
    };

    assert!(out.success(), "wget failed: {}", out.stderr_str());
    let parsed: serde_json::Value =
        serde_json::from_str(&out.stdout_str()).expect("health response is not valid JSON");
    assert_eq!(parsed["status"], "ok");
    assert_eq!(parsed["run_id"], "run-conformance-allow");

    handle.stop().await;
}

// ===========================================================================
// Conformance: native file RPC
// ===========================================================================

/// Backend can stat a file that exists and a file that does not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires VM backend + kernel/initramfs artifacts"]
async fn conformance_file_stat() {
    let backend = match create_started_backend().await {
        Some(b) => b,
        None => return,
    };

    backend
        .write_file("/workspace/stat_test.txt", b"hello")
        .await
        .expect("write_file failed");

    let stat = backend
        .file_stat("/workspace/stat_test.txt")
        .await
        .expect("file_stat failed");
    assert!(stat.exists);
    assert_eq!(stat.size, Some(5));
    assert!(stat.error.is_none());

    let stat = backend
        .file_stat("/workspace/no_such_file.txt")
        .await
        .expect("file_stat on missing file failed");
    assert!(!stat.exists);
    assert!(stat.error.is_none());
}

/// Backend can read a file via the native file RPC channel.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires VM backend + kernel/initramfs artifacts"]
async fn conformance_read_file_native() {
    let backend = match create_started_backend().await {
        Some(b) => b,
        None => return,
    };

    let content = b"native read file test content";
    backend
        .write_file("/workspace/read_test.txt", content)
        .await
        .expect("write_file failed");

    let data = backend
        .read_file_native("/workspace/read_test.txt")
        .await
        .expect("read_file_native failed");
    assert_eq!(data, content);

    let result = backend
        .read_file_native("/workspace/no_such_file_read.txt")
        .await;
    assert!(result.is_err());
}

/// Native file RPC works while a long-running exec holds the exec channel.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires VM backend + kernel/initramfs artifacts"]
async fn conformance_file_rpc_while_exec_running() {
    let backend = match create_started_backend().await {
        Some(b) => b,
        None => return,
    };

    backend
        .write_file("/workspace/concurrent_test.txt", b"concurrent")
        .await
        .expect("write_file failed");

    let (_chunk_rx, _response_rx) = backend
        .exec_streaming("sh", &["-c", "sleep 10"], &[], None, Some(15))
        .await
        .expect("exec_streaming failed");

    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    let stat = backend
        .file_stat("/workspace/concurrent_test.txt")
        .await
        .expect("file_stat must work during active exec");
    assert!(stat.exists);
    assert_eq!(stat.size, Some(10));

    let data = backend
        .read_file_native("/workspace/concurrent_test.txt")
        .await
        .expect("read_file_native must work during active exec");
    assert_eq!(data, b"concurrent");

    let stat = backend
        .file_stat("/workspace/no_such_concurrent.txt")
        .await
        .expect("file_stat on missing file must work during exec");
    assert!(!stat.exists);
}
