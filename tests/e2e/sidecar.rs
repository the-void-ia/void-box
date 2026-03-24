//! End-to-End sidecar integration tests
//!
//! These tests boot a real VM and verify that a guest agent can communicate
//! with the host-side sidecar via SLIRP networking (10.0.2.2:<port>).
//!
//! ## Prerequisites
//!
//! 1. Build the test initramfs:
//!    ```bash
//!    scripts/build_test_image.sh
//!    ```
//!
//! 2. Run with:
//!    ```bash
//!    VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
//!    VOID_BOX_INITRAMFS=/tmp/void-box-test-rootfs.cpio.gz \
//!    cargo test --test e2e_sidecar -- --ignored --test-threads=1
//!    ```
//!
//! All tests are `#[ignore]` so they don't run in a normal `cargo test`.

#[cfg(target_os = "linux")]
use std::path::PathBuf;

#[path = "../common/vm_preflight.rs"]
mod vm_preflight;

#[cfg(target_os = "linux")]
use void_box::backend::{BackendConfig, BackendSecurityConfig, VmmBackend};
#[cfg(target_os = "linux")]
use void_box::sidecar;

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn backend_available() -> bool {
    vm_preflight::require_kvm_usable().is_ok() && vm_preflight::require_vsock_usable().is_ok()
}

#[cfg(target_os = "linux")]
fn kvm_artifacts() -> Option<(PathBuf, PathBuf)> {
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
    Some((kernel, initramfs))
}

#[cfg(target_os = "linux")]
fn build_network_config() -> Option<BackendConfig> {
    if !backend_available() {
        eprintln!("skipping: VM backend not available");
        return None;
    }
    let (kernel, initramfs) = match kvm_artifacts() {
        Some(a) => a,
        None => {
            eprintln!("skipping: set VOID_BOX_KERNEL and VOID_BOX_INITRAMFS");
            return None;
        }
    };

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
        mounts: vec![],
        oci_rootfs: None,
        oci_rootfs_dev: None,
        oci_rootfs_disk: None,
        env: vec![],
        security: BackendSecurityConfig {
            session_secret: secret,
            command_allowlist: vec!["sh".into(), "wget".into(), "cat".into(), "echo".into()],
            network_deny_list: vec!["169.254.0.0/16".into()],
            max_connections_per_second: 50,
            max_concurrent_connections: 64,
            seccomp: true,
        },
        snapshot: None,
    })
}

#[cfg(target_os = "linux")]
async fn start_backend() -> Option<Box<dyn VmmBackend>> {
    let config = build_network_config()?;
    let mut backend = void_box::backend::create_backend();
    match backend.start(config).await {
        Ok(()) => Some(backend),
        Err(e) => {
            eprintln!("skipping: backend start failed: {e}");
            None
        }
    }
}

#[cfg(target_os = "linux")]
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
// Test 1: Guest reads sidecar health endpoint
// ===========================================================================

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires KVM + kernel/initramfs + network"]
async fn guest_reads_sidecar_health() {
    let backend = match start_backend().await {
        Some(b) => b,
        None => return,
    };

    // Start sidecar on host (random port)
    let handle = sidecar::start_sidecar(
        "run-e2e",
        "exec-e2e",
        "c-1",
        vec!["c-2".into()],
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .expect("failed to start sidecar");

    let port = handle.addr().port();

    // Guest fetches /v1/health via SLIRP gateway (10.0.2.2)
    let script = format!("wget -q -O - http://10.0.2.2:{}/v1/health", port);
    let out = guest_sh(&*backend, &script).await;
    let Some(out) = out else {
        handle.stop().await;
        return;
    };

    assert!(out.success(), "wget failed: {}", out.stderr_str());
    let body = out.stdout_str();
    eprintln!("health response: {body}");

    let parsed: serde_json::Value =
        serde_json::from_str(&body).expect("health response is not valid JSON");
    assert_eq!(parsed["status"], "ok");
    assert_eq!(parsed["run_id"], "run-e2e");

    handle.stop().await;
}

// ===========================================================================
// Test 2: Guest reads inbox and posts intent
// ===========================================================================

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires KVM + kernel/initramfs + network"]
async fn guest_reads_inbox_and_posts_intent() {
    let backend = match start_backend().await {
        Some(b) => b,
        None => return,
    };

    let handle = sidecar::start_sidecar(
        "run-e2e",
        "exec-e2e",
        "c-1",
        vec![],
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .expect("failed to start sidecar");

    let port = handle.addr().port();

    // Load inbox via host-side handle (simulates void-control push)
    handle
        .load_inbox(sidecar::InboxSnapshot {
            version: 1,
            execution_id: "exec-e2e".into(),
            candidate_id: "c-1".into(),
            iteration: 1,
            entries: vec![sidecar::InboxEntry {
                message_id: "msg-001".into(),
                from_candidate_id: "c-2".into(),
                kind: "proposal".into(),
                payload: serde_json::json!({"summary_text": "use approach A"}),
            }],
        })
        .await;

    // Guest reads inbox
    let script = format!("wget -q -O - http://10.0.2.2:{}/v1/inbox", port);
    let out = guest_sh(&*backend, &script).await;
    let Some(out) = out else {
        handle.stop().await;
        return;
    };

    assert!(out.success(), "wget inbox failed: {}", out.stderr_str());
    let inbox: serde_json::Value =
        serde_json::from_str(&out.stdout_str()).expect("inbox is not valid JSON");
    assert_eq!(inbox["version"], 1);
    assert_eq!(inbox["entries"].as_array().unwrap().len(), 1);
    assert_eq!(inbox["entries"][0]["message_id"], "msg-001");

    // Guest posts an intent
    let intent_json = r#"{"kind":"signal","audience":"broadcast","payload":{"summary_text":"ack from guest"},"priority":"normal"}"#;
    let script = format!(
        "wget -q -O - --post-data='{}' --header='Content-Type: application/json' http://10.0.2.2:{}/v1/intents",
        intent_json, port
    );
    let out = guest_sh(&*backend, &script).await;
    let Some(out) = out else {
        handle.stop().await;
        return;
    };

    assert!(
        out.success(),
        "wget intent post failed: {}",
        out.stderr_str()
    );
    let stamped: serde_json::Value =
        serde_json::from_str(&out.stdout_str()).expect("stamped intent is not valid JSON");
    assert_eq!(stamped["kind"], "signal");
    assert_eq!(stamped["audience"], "broadcast");
    assert_eq!(stamped["iteration"], 1);
    assert_eq!(stamped["from_candidate_id"], "c-1");
    assert!(!stamped["intent_id"].as_str().unwrap_or("").is_empty());

    // Drain intents from host side and verify
    let drained = handle.drain_intents().await;
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].kind, "signal");

    handle.stop().await;
}

// ===========================================================================
// Test 3: Guest reads context endpoint
// ===========================================================================

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires KVM + kernel/initramfs + network"]
async fn guest_reads_context() {
    let backend = match start_backend().await {
        Some(b) => b,
        None => return,
    };

    let handle = sidecar::start_sidecar(
        "run-e2e",
        "exec-e2e",
        "c-3",
        vec!["c-1".into(), "c-2".into()],
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .expect("failed to start sidecar");

    let port = handle.addr().port();

    let script = format!("wget -q -O - http://10.0.2.2:{}/v1/context", port);
    let out = guest_sh(&*backend, &script).await;
    let Some(out) = out else {
        handle.stop().await;
        return;
    };

    assert!(out.success(), "wget context failed: {}", out.stderr_str());
    let ctx: serde_json::Value =
        serde_json::from_str(&out.stdout_str()).expect("context is not valid JSON");
    assert_eq!(ctx["execution_id"], "exec-e2e");
    assert_eq!(ctx["candidate_id"], "c-3");
    assert_eq!(ctx["role"], "candidate");
    assert_eq!(ctx["peers"].as_array().unwrap().len(), 2);

    handle.stop().await;
}

// ===========================================================================
// Test 4: Full agent flow — read inbox, reason, emit intent
// ===========================================================================

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires KVM + kernel/initramfs + network"]
async fn guest_full_agent_flow() {
    let backend = match start_backend().await {
        Some(b) => b,
        None => return,
    };

    let handle = sidecar::start_sidecar(
        "run-e2e",
        "exec-e2e",
        "c-1",
        vec!["c-2".into(), "c-3".into()],
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .expect("failed to start sidecar");

    let port = handle.addr().port();

    // Simulate void-control pushing inbox
    handle
        .load_inbox(sidecar::InboxSnapshot {
            version: 1,
            execution_id: "exec-e2e".into(),
            candidate_id: "c-1".into(),
            iteration: 1,
            entries: vec![
                sidecar::InboxEntry {
                    message_id: "msg-001".into(),
                    from_candidate_id: "c-2".into(),
                    kind: "proposal".into(),
                    payload: serde_json::json!({"summary_text": "approach A"}),
                },
                sidecar::InboxEntry {
                    message_id: "msg-002".into(),
                    from_candidate_id: "c-3".into(),
                    kind: "signal".into(),
                    payload: serde_json::json!({"summary_text": "converging"}),
                },
            ],
        })
        .await;

    // Guest script: read context, read inbox, post evaluation intent
    // This simulates what a generic agent would do
    let script = format!(
        r#"
        SIDECAR="http://10.0.2.2:{port}"

        # 1. Read context to know who I am
        CTX=$(wget -q -O - "$SIDECAR/v1/context")
        echo "context: $CTX"

        # 2. Read inbox to see messages
        INBOX=$(wget -q -O - "$SIDECAR/v1/inbox")
        echo "inbox: $INBOX"

        # 3. Post an evaluation intent
        wget -q -O - \
            --post-data='{{"kind":"evaluation","audience":"leader","payload":{{"summary_text":"approach A looks good"}},"priority":"high"}}' \
            --header='Content-Type: application/json' \
            "$SIDECAR/v1/intents"
        "#
    );

    let out = guest_sh(&*backend, &script).await;
    let Some(out) = out else {
        handle.stop().await;
        return;
    };

    assert!(out.success(), "agent flow failed: {}", out.stderr_str());
    let stdout = out.stdout_str();
    eprintln!("agent output:\n{stdout}");

    // Verify the intent was received
    let drained = handle.drain_intents().await;
    assert_eq!(drained.len(), 1, "expected 1 drained intent");
    assert_eq!(drained[0].kind, "evaluation");
    assert_eq!(drained[0].audience, "leader");
    assert_eq!(drained[0].iteration, 1);

    handle.stop().await;
}
