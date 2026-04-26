//! End-to-End sidecar integration tests
//!
//! These tests boot a real VM and verify that a guest agent can communicate
//! with the host-side sidecar via the backend-specific guest→host gateway.
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

use std::path::PathBuf;

#[path = "../common/vm_preflight.rs"]
mod vm_preflight;

use void_box::backend::{BackendConfig, BackendSecurityConfig, GuestConsoleSink, VmmBackend};
use void_box::sidecar;
use void_box_protocol::SessionSecret;

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

fn backend_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        vm_preflight::require_kvm_usable().is_ok() && vm_preflight::require_vsock_usable().is_ok()
    }
    #[cfg(target_os = "macos")]
    {
        true
    }
}

fn vm_artifacts() -> Option<(PathBuf, PathBuf)> {
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

fn build_network_config_with_deny_list(deny_list: Vec<String>) -> Option<BackendConfig> {
    if !backend_available() {
        eprintln!("skipping: VM backend not available");
        return None;
    }
    let (kernel, initramfs) = match vm_artifacts() {
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
        guest_console: GuestConsoleSink::Stderr,
        shared_dir: None,
        mounts: vec![],
        oci_rootfs: None,
        oci_rootfs_dev: None,
        oci_rootfs_disk: None,
        env: vec![],
        security: BackendSecurityConfig {
            session_secret: SessionSecret::new(secret),
            command_allowlist: vec!["sh".into(), "wget".into(), "cat".into(), "echo".into()],
            network_deny_list: deny_list,
            max_connections_per_second: 50,
            max_concurrent_connections: 64,
            seccomp: true,
        },
        snapshot: None,
        enable_snapshots: false,
    })
}

fn build_network_config() -> Option<BackendConfig> {
    build_network_config_with_deny_list(vec!["169.254.0.0/16".into()])
}

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

async fn start_backend_with_deny_list(deny_list: Vec<String>) -> Option<Box<dyn VmmBackend>> {
    let config = build_network_config_with_deny_list(deny_list)?;
    let mut backend = void_box::backend::create_backend();
    match backend.start(config).await {
        Ok(()) => Some(backend),
        Err(e) => {
            eprintln!("skipping: backend start failed: {e}");
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
// Test 1: Guest reads sidecar health endpoint
// ===========================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires VM backend + kernel/initramfs + network"]
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
        void_box::backend::guest_accessible_bind_addr(0),
    )
    .await
    .expect("failed to start sidecar");

    let port = handle.addr().port();

    let script = format!(
        "wget -q -O - {}/v1/health",
        void_box::backend::guest_host_url(port)
    );
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

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires VM backend + kernel/initramfs + network"]
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
        void_box::backend::guest_accessible_bind_addr(0),
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
    let script = format!(
        "wget -q -O - {}/v1/inbox",
        void_box::backend::guest_host_url(port)
    );
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
        "wget -q -O - --post-data='{}' --header='Content-Type: application/json' {}/v1/intents",
        intent_json,
        void_box::backend::guest_host_url(port),
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

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires VM backend + kernel/initramfs + network"]
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
        void_box::backend::guest_accessible_bind_addr(0),
    )
    .await
    .expect("failed to start sidecar");

    let port = handle.addr().port();

    let script = format!(
        "wget -q -O - {}/v1/context",
        void_box::backend::guest_host_url(port)
    );
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
// Test 4: Guest cannot reach host gateway when it is deny-listed
// ===========================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires VM backend + kernel/initramfs + network"]
async fn guest_cannot_reach_denied_host_gateway() {
    let gateway_cidr = format!("{}/32", void_box::backend::guest_host_gateway());
    let backend = match start_backend_with_deny_list(vec![gateway_cidr.clone()]).await {
        Some(b) => b,
        None => return,
    };

    let handle = sidecar::start_sidecar(
        "run-deny-e2e",
        "exec-deny-e2e",
        "c-1",
        vec![],
        void_box::backend::guest_accessible_bind_addr(0),
    )
    .await
    .expect("failed to start sidecar");

    let port = handle.addr().port();
    let script = format!(
        "wget -T 2 -q -O - {}/v1/health >/tmp/deny.out 2>/tmp/deny.err; echo $?",
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
    let wget_exit = out.stdout_str().trim().to_string();
    assert_ne!(
        wget_exit, "0",
        "guest unexpectedly reached deny-listed host gateway {}",
        gateway_cidr
    );

    let stderr_out = guest_sh(&*backend, "cat /tmp/deny.err").await;
    let stderr_text = stderr_out
        .map(|output| output.stdout_str())
        .unwrap_or_default();
    eprintln!(
        "deny-list blocked guest->host request as expected: exit={} stderr={}",
        wget_exit,
        stderr_text.trim()
    );

    handle.stop().await;
}

// ===========================================================================
// Test 5: Unrelated deny-list CIDR does not block guest->host access
// ===========================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires VM backend + kernel/initramfs + network"]
async fn guest_reaches_host_gateway_when_deny_list_targets_unrelated_cidr() {
    let backend = match start_backend_with_deny_list(vec!["203.0.113.0/24".into()]).await {
        Some(b) => b,
        None => return,
    };

    let handle = sidecar::start_sidecar(
        "run-allow-e2e",
        "exec-allow-e2e",
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

    assert!(
        out.success(),
        "guest unexpectedly lost host-gateway access with unrelated deny list: {}",
        out.stderr_str()
    );
    let parsed: serde_json::Value =
        serde_json::from_str(&out.stdout_str()).expect("health response is not valid JSON");
    assert_eq!(parsed["status"], "ok");
    assert_eq!(parsed["run_id"], "run-allow-e2e");

    handle.stop().await;
}

// ===========================================================================
// Test 6: Full agent flow — read inbox, reason, emit intent
// ===========================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires VM backend + kernel/initramfs + network"]
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
        void_box::backend::guest_accessible_bind_addr(0),
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
        SIDECAR="{sidecar_url}"

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
        "#,
        sidecar_url = void_box::backend::guest_host_url(port),
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

// ===========================================================================
// Test 7: Skill injection — claudio discovers the provisioned messaging skill
// ===========================================================================

/// Build a VoidBox with an inline messaging skill and claudio (mock claude-code).
/// Claudio scans /home/sandbox/.claude/skills/*.md on startup and reports
/// discovered skills in its output. This test verifies the full provisioning
/// pipeline: SkillKind::Inline → provision_skills → guest filesystem → claudio
/// discovery.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires VM backend + kernel/initramfs + network"]
async fn claudio_discovers_injected_messaging_skill() {
    use void_box::agent_box::VoidBox;
    use void_box::skill::Skill;

    if vm_preflight::require_kvm_usable().is_err() {
        eprintln!("skipping: VM backend not available");
        return;
    }
    if vm_preflight::require_vsock_usable().is_err() {
        eprintln!("skipping: vsock not available");
        return;
    }
    let (kernel, initramfs) = match vm_artifacts() {
        Some(a) => a,
        None => {
            eprintln!("skipping: set VOID_BOX_KERNEL and VOID_BOX_INITRAMFS");
            return;
        }
    };
    if vm_preflight::require_kernel_artifacts(&kernel, Some(&initramfs)).is_err() {
        eprintln!("skipping: kernel/initramfs not found");
        return;
    }

    // Start sidecar (skill content is now static, but we need the handle for cleanup)
    let handle = sidecar::start_sidecar(
        "run-skill-inject",
        "exec-skill-inject",
        "c-1",
        vec![],
        void_box::backend::guest_accessible_bind_addr(0),
    )
    .await
    .expect("failed to start sidecar");

    let skill_content = sidecar::messaging_skill_content();

    // Build VoidBox with inline messaging skill + claudio
    let ab = match VoidBox::new("skill-inject-test")
        .kernel(&kernel)
        .initramfs(&initramfs)
        .memory_mb(256)
        .network(true)
        .skill(Skill::inline("void-messaging", &skill_content))
        .skill(Skill::agent("claude-code"))
        .prompt("Check your available skills and tell me what you found.")
        .timeout_secs(60)
        .build()
    {
        Ok(ab) => ab,
        Err(e) => {
            eprintln!("skipping: failed to build VoidBox: {e}");
            handle.stop().await;
            return;
        }
    };

    // Run claudio — it scans skills dir and reports discoveries
    let result = match ab.run(None, None).await {
        Ok(r) => r,
        Err(void_box::Error::Guest(msg)) if msg.contains("control_channel: deadline reached") => {
            eprintln!("skipping: guest control channel unavailable: {msg}");
            handle.stop().await;
            return;
        }
        Err(e) => {
            handle.stop().await;
            panic!("VoidBox::run failed: {e}");
        }
    };

    eprintln!("claudio result_text: {}", result.agent_result.result_text);

    // Claudio should have discovered the void-messaging skill file
    // It reports discovered skills in the result text
    assert!(
        result.agent_result.result_text.contains("void-messaging"),
        "claudio should discover void-messaging skill, got: {}",
        result.agent_result.result_text
    );

    handle.stop().await;
    eprintln!("PASSED: claudio_discovers_injected_messaging_skill");
}

// ===========================================================================
// Test 8: void-message CLI works from inside the guest VM
// ===========================================================================

/// Boot a real VM and run void-message CLI commands from inside the guest.
/// Requires the test initramfs to include void-message binary
/// (rebuild with scripts/build_test_image.sh after adding void-message).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires VM backend + kernel/initramfs with void-message"]
async fn guest_uses_void_message_cli() {
    let backend = match start_backend().await {
        Some(b) => b,
        None => return,
    };

    let handle = sidecar::start_sidecar(
        "run-cli-e2e",
        "exec-cli-e2e",
        "c-1",
        vec!["c-2".into()],
        void_box::backend::guest_accessible_bind_addr(0),
    )
    .await
    .expect("failed to start sidecar");

    let port = handle.addr().port();
    let sidecar_url = void_box::backend::guest_host_url(port);

    // Load inbox
    handle
        .load_inbox(sidecar::InboxSnapshot {
            version: 1,
            execution_id: "exec-cli-e2e".into(),
            candidate_id: "c-1".into(),
            iteration: 1,
            entries: vec![sidecar::InboxEntry {
                message_id: "msg-1".into(),
                from_candidate_id: "c-2".into(),
                kind: "proposal".into(),
                payload: serde_json::json!({"summary_text": "approach A"}),
            }],
        })
        .await;

    // Test: void-message health
    let script = format!("VOID_SIDECAR_URL={sidecar_url} void-message health");
    let out = guest_sh(&*backend, &script).await;
    let Some(out) = out else {
        eprintln!("skipping: void-message not in initramfs (rebuild test image)");
        handle.stop().await;
        return;
    };
    if !out.success() {
        eprintln!("skipping: void-message not available: {}", out.stderr_str());
        handle.stop().await;
        return;
    }
    let health: serde_json::Value = serde_json::from_str(&out.stdout_str()).unwrap();
    assert_eq!(health["status"], "ok");

    // Test: void-message context
    let script = format!("VOID_SIDECAR_URL={sidecar_url} void-message context");
    let out = guest_sh(&*backend, &script).await.unwrap();
    assert!(out.success(), "context failed: {}", out.stderr_str());
    let ctx: serde_json::Value = serde_json::from_str(&out.stdout_str()).unwrap();
    assert_eq!(ctx["candidate_id"], "c-1");

    // Test: void-message inbox
    let script = format!("VOID_SIDECAR_URL={sidecar_url} void-message inbox");
    let out = guest_sh(&*backend, &script).await.unwrap();
    assert!(out.success(), "inbox failed: {}", out.stderr_str());
    let inbox: serde_json::Value = serde_json::from_str(&out.stdout_str()).unwrap();
    assert_eq!(inbox["entries"].as_array().unwrap().len(), 1);

    // Test: void-message send
    let script = format!(
        "VOID_SIDECAR_URL={sidecar_url} void-message send --kind signal --audience broadcast --summary 'cli e2e works'"
    );
    let out = guest_sh(&*backend, &script).await.unwrap();
    assert!(out.success(), "send failed: {}", out.stderr_str());

    // Verify intent received by sidecar
    let drained = handle.drain_intents().await;
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].kind, "signal");

    handle.stop().await;
    eprintln!("PASSED: guest_uses_void_message_cli");
}

// ===========================================================================
// Test 7: Claudio discovers void-mcp MCP bridge and simulates tool calls
// ===========================================================================

/// Build a VoidBox with void-mcp registered as an MCP server, run claudio,
/// verify claudio discovers it in mcp.json and simulates mcp__void-mcp tool calls.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires VM backend + kernel/initramfs with void-mcp"]
async fn claudio_discovers_void_mcp_tools() {
    use void_box::agent_box::VoidBox;
    use void_box::skill::Skill;

    if vm_preflight::require_kvm_usable().is_err() {
        eprintln!("skipping: VM backend not available");
        return;
    }
    if vm_preflight::require_vsock_usable().is_err() {
        eprintln!("skipping: vsock not available");
        return;
    }
    let (kernel, initramfs) = match vm_artifacts() {
        Some(a) => a,
        None => {
            eprintln!("skipping: set VOID_BOX_KERNEL and VOID_BOX_INITRAMFS");
            return;
        }
    };
    if vm_preflight::require_kernel_artifacts(&kernel, Some(&initramfs)).is_err() {
        eprintln!("skipping: kernel/initramfs not found");
        return;
    }

    // Start sidecar
    let handle = sidecar::start_sidecar(
        "run-mcp-e2e",
        "exec-mcp-e2e",
        "c-1",
        vec!["c-2".into()],
        void_box::backend::guest_accessible_bind_addr(0),
    )
    .await
    .expect("failed to start sidecar");

    let port = handle.addr().port();

    // Build VoidBox with void-mcp MCP server + claudio
    let ab = match VoidBox::new("mcp-bridge-test")
        .kernel(&kernel)
        .initramfs(&initramfs)
        .memory_mb(256)
        .network(true)
        .skill(
            Skill::mcp("void-mcp")
                .description("Collaboration tools")
                .env("VOID_SIDECAR_URL", void_box::backend::guest_host_url(port)),
        )
        .skill(Skill::agent("claude-code"))
        .prompt("Use your collaboration tools.")
        .timeout_secs(60)
        .build()
    {
        Ok(ab) => ab,
        Err(e) => {
            eprintln!("skipping: failed to build VoidBox: {e}");
            handle.stop().await;
            return;
        }
    };

    let result = match ab.run(None, None).await {
        Ok(r) => r,
        Err(void_box::Error::Guest(msg)) if msg.contains("control_channel: deadline reached") => {
            eprintln!("skipping: guest control channel unavailable: {msg}");
            handle.stop().await;
            return;
        }
        Err(e) => {
            handle.stop().await;
            panic!("VoidBox::run failed: {e}");
        }
    };

    eprintln!("claudio result: {}", result.agent_result.result_text);

    // Claudio should discover void-mcp as an MCP server in mcp.json
    assert!(
        result.agent_result.result_text.contains("void-mcp"),
        "claudio should discover void-mcp MCP server, got: {}",
        result.agent_result.result_text
    );

    handle.stop().await;
    eprintln!("PASSED: claudio_discovers_void_mcp_tools");
}
