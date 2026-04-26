//! Integration test for Lever 7b: persistent multiplex control channel.
//!
//! Issues a burst of concurrent `exec` RPCs against a single started
//! [`VmmBackend`] and asserts every one completes with the correct
//! stdout. This exercises the multiplex demultiplexer under contention
//! on the shared writer mutex, the pending-slot table, and the reader
//! thread — the pressure the old per-RPC reconnect path could not take
//! (broken-pipe floods under 5 ms handshake timeout).
//!
//! ## Prerequisites
//!
//! ```bash
//! scripts/build_test_image.sh
//! VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
//! VOID_BOX_INITRAMFS=/tmp/void-box-test-rootfs.cpio.gz \
//! cargo test --test persistent_channel -- --ignored --test-threads=1
//! ```

use std::path::PathBuf;

#[path = "common/vm_preflight.rs"]
mod vm_preflight;

use void_box::backend::{BackendConfig, BackendSecurityConfig, GuestConsoleSink, VmmBackend};
use void_box_protocol::SessionSecret;

/// Number of concurrent `exec` RPCs fired at the multiplex channel.
const CONCURRENT_EXEC_COUNT: usize = 16;

fn backend_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        vm_preflight::require_kvm_usable().is_ok() && vm_preflight::require_vsock_usable().is_ok()
    }
    #[cfg(target_os = "macos")]
    {
        true
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        false
    }
}

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

    // Under VOID_BOX_DIAGNOSTIC=1, route the guest serial console to a file so
    // the Azure-CI handshake-deadline failure leaves a host-readable trail
    // ("did the guest kernel boot? did guest-agent reach vsock bind?"). The
    // file path is stable so the CI workflow can upload it as an artifact.
    let console = if matches!(std::env::var("VOID_BOX_DIAGNOSTIC").as_deref(), Ok("1")) {
        let path = std::env::var("VOID_BOX_DIAGNOSTIC_CONSOLE_PATH")
            .unwrap_or_else(|_| "/tmp/void-box-persistent-channel-console.log".to_string());
        eprintln!("persistent_channel: routing guest console to {path}");
        GuestConsoleSink::File(PathBuf::from(path))
    } else {
        GuestConsoleSink::Disabled
    };

    Some(BackendConfig {
        memory_mb: 2048,
        vcpus: 2,
        kernel,
        initramfs: Some(initramfs),
        rootfs: None,
        network: true,
        enable_vsock: true,
        guest_console: console,
        shared_dir: None,
        mounts: vec![],
        oci_rootfs: None,
        oci_rootfs_dev: None,
        oci_rootfs_disk: None,
        env: vec![],
        security: BackendSecurityConfig {
            session_secret: SessionSecret::new(secret),
            command_allowlist: vec!["sh".into(), "echo".into()],
            network_deny_list: vec![],
            max_connections_per_second: 200,
            max_concurrent_connections: 256,
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

    let Some(config) = backend_config() else {
        eprintln!("skipping: set VOID_BOX_KERNEL and VOID_BOX_INITRAMFS");
        return None;
    };

    let mut backend = void_box::backend::create_backend();
    match backend.start(config).await {
        Ok(()) => Some(backend),
        Err(e) => {
            eprintln!("skipping: backend start failed: {e}");
            None
        }
    }
}

/// Number of serial `exec` calls fired through the persistent channel.
///
/// This exercises the previously failing tight-loop path that stalled after
/// ~20 execs when guest-agent duplicated `kmsg()` to both stderr and
/// `/dev/kmsg`.
const SERIAL_EXEC_COUNT: usize = 100;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires VM backend + kernel/initramfs artifacts"]
async fn persistent_channel_serial_exec_many() {
    let Some(backend) = create_started_backend().await else {
        return;
    };

    let t_start = std::time::Instant::now();
    for index in 0..SERIAL_EXEC_COUNT {
        let expected = format!("serial-{index}");
        let script = format!("echo {expected}");
        let output = backend
            .exec("sh", &["-c", &script], &[], &[], None, Some(30))
            .await
            .unwrap_or_else(|e| panic!("exec {index} failed: {e}"));
        assert!(
            output.success(),
            "exec {index} exited with code {:?}",
            output.exit_code
        );
        assert_eq!(output.stdout_str().trim(), expected);
    }
    eprintln!(
        "persistent_channel: {SERIAL_EXEC_COUNT} serial execs completed in {:?}",
        t_start.elapsed()
    );
}

/// Fires [`CONCURRENT_EXEC_COUNT`] exec calls at the same backend in
/// parallel and asserts each returns its own unique stdout.
///
/// Failure modes this catches:
/// - Request-id collision routing one call's response to another
/// - Shared writer-mutex deadlock dropping requests mid-burst
/// - Guest-agent dispatch serialization dropping streamed chunks
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires VM backend + kernel/initramfs artifacts"]
async fn persistent_channel_concurrent_exec() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();

    let Some(backend) = create_started_backend().await else {
        return;
    };
    let backend: std::sync::Arc<dyn VmmBackend> = std::sync::Arc::from(backend);

    let mut handles = Vec::with_capacity(CONCURRENT_EXEC_COUNT);
    for index in 0..CONCURRENT_EXEC_COUNT {
        let backend = std::sync::Arc::clone(&backend);
        handles.push(tokio::spawn(async move {
            let expected = format!("req-{index}");
            let script = format!("echo {expected}");
            let output = backend
                .exec("sh", &["-c", &script], &[], &[], None, Some(30))
                .await
                .expect("exec should succeed");
            (index, expected, output)
        }));
    }

    let t_start = std::time::Instant::now();
    let mut successes = 0;
    for handle in handles {
        let (index, expected, output) = handle.await.expect("join exec task");
        eprintln!(
            "persistent_channel: exec {index}/{CONCURRENT_EXEC_COUNT} joined at {:?}",
            t_start.elapsed()
        );
        assert!(
            output.success(),
            "exec {index} exited with code {:?}",
            output.exit_code
        );
        let stdout = output.stdout_str();
        let stdout = stdout.trim();
        assert_eq!(
            stdout, expected,
            "exec {index} returned wrong stdout (demuxer misrouted response)"
        );
        successes += 1;
    }
    assert_eq!(
        successes, CONCURRENT_EXEC_COUNT,
        "expected all {CONCURRENT_EXEC_COUNT} concurrent execs to succeed"
    );
}
