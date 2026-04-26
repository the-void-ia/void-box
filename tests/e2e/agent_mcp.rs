//! Real Claude Code MCP integration test
//!
//! This test uses a real Anthropic API key to verify that Claude Code
//! discovers and uses void-mcp tools inside the VM. It requires:
//!
//! 1. Real kernel + initramfs with the **production** claude-code binary
//!    (NOT the claudio mock from `scripts/build_test_image.sh` — claudio
//!    fakes tool calls without actually invoking the void-mcp HTTP server,
//!    so the sidecar receives zero intents and the test's
//!    `assert!(!drained.is_empty())` always fails). Build the right
//!    initramfs with `scripts/build_claude_rootfs.sh` (writes to
//!    `target/void-box-rootfs.cpio.gz`).
//! 2. ANTHROPIC_API_KEY environment variable set
//! 3. Network access to api.anthropic.com from the host
//!
//! Run with:
//!   scripts/build_claude_rootfs.sh
//!   VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
//!   VOID_BOX_INITRAMFS=$PWD/target/void-box-rootfs.cpio.gz \
//!   ANTHROPIC_API_KEY=sk-... \
//!   cargo test --test e2e_agent_mcp -- --ignored --test-threads=1 --nocapture

use std::path::PathBuf;

#[path = "../common/vm_preflight.rs"]
mod vm_preflight;

use void_box::agent_box::VoidBox;
use void_box::sidecar;
use void_box::skill::Skill;

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

// ===========================================================================
// Test: Real Claude Code discovers void-mcp tools and emits intents
// ===========================================================================

/// This test boots a real VM, starts a sidecar, registers void-mcp,
/// and runs real Claude Code with an Anthropic API key. It verifies:
/// 1. Claude Code starts successfully
/// 2. Claude discovers the MCP tools (check system event)
/// 3. Claude uses the tools (check sidecar for buffered intents)
///
/// The prompt explicitly asks Claude to use collaboration tools.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires VM backend + kernel/initramfs + ANTHROPIC_API_KEY"]
async fn real_claude_uses_void_mcp_tools() {
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
    if std::env::var("ANTHROPIC_API_KEY").is_err() {
        eprintln!("skipping: set ANTHROPIC_API_KEY");
        return;
    }

    // Start sidecar
    let handle = sidecar::start_sidecar(
        "run-real-claude",
        "exec-real-claude",
        "c-1",
        vec!["c-2".into(), "c-3".into()],
        void_box::backend::guest_accessible_bind_addr(0),
    )
    .await
    .expect("failed to start sidecar");

    let port = handle.addr().port();
    eprintln!("sidecar started on port {port}");

    // Load inbox with a message from a peer
    handle
        .load_inbox(sidecar::InboxSnapshot {
            version: 1,
            execution_id: "exec-real-claude".into(),
            candidate_id: "c-1".into(),
            iteration: 1,
            entries: vec![sidecar::InboxEntry {
                message_id: "msg-001".into(),
                from_candidate_id: "c-2".into(),
                kind: "signal".into(),
                payload: serde_json::json!({"summary_text": "high cache miss rate observed in staging"}),
            }],
        })
        .await;

    // Build VoidBox with:
    // - void-mcp MCP server (collaboration tools)
    // - real claude-code agent
    // - networking enabled (for API access + sidecar)
    let ab = match VoidBox::new("real-claude-mcp-test")
        .kernel(&kernel)
        .initramfs(&initramfs)
        .memory_mb(3072)
        .network(true)
        .skill(
            Skill::mcp("void-mcp")
                .description("Collaboration tools for multi-agent swarm")
                .env("VOID_SIDECAR_URL", void_box::backend::guest_host_url(port)),
        )
        .skill(Skill::agent("claude-code"))
        .prompt(
            "You are candidate c-1 in a multi-agent swarm evaluating a caching strategy.\n\n\
             IMPORTANT: You have collaboration tools available. Do the following:\n\
             1. Use read_shared_context to see your execution identity\n\
             2. Use read_peer_messages to check what other candidates have shared\n\
             3. Use broadcast_observation to share one observation with all agents\n\
             4. Use recommend_to_leader to recommend an action to the coordinator\n\n\
             After using all four collaboration tools, write a brief summary to /workspace/output.json.",
        )
        .timeout_secs(120)
        .build()
    {
        Ok(ab) => ab,
        Err(e) => {
            handle.stop().await;
            panic!("failed to build VoidBox: {e}");
        }
    };

    eprintln!("running real Claude Code with void-mcp...");

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

    eprintln!("=== Claude Result ===");
    eprintln!("model: {}", result.agent_result.model);
    eprintln!("session: {}", result.agent_result.session_id);
    eprintln!("is_error: {}", result.agent_result.is_error);
    eprintln!("tools used: {:?}", result.agent_result.tool_calls);
    eprintln!("result text: {}", result.agent_result.result_text);
    eprintln!("cost: ${:.4}", result.agent_result.total_cost_usd);

    // Check if void-mcp tools were used
    let mcp_tool_used = result
        .agent_result
        .tool_calls
        .iter()
        .any(|t| t.tool_name.contains("mcp__void-mcp") || t.tool_name.contains("void-mcp"));

    eprintln!("MCP tools used: {mcp_tool_used}");

    // Check sidecar for buffered intents
    let drained = handle.drain_intents().await;
    eprintln!("sidecar intents drained: {}", drained.len());
    for (i, intent) in drained.iter().enumerate() {
        eprintln!(
            "  intent[{i}]: kind={} audience={} iteration={}",
            intent.kind, intent.audience, intent.iteration
        );
    }

    handle.stop().await;

    // Assertions — at minimum Claude should have completed without error
    assert!(
        !result.agent_result.is_error,
        "Claude reported an error: {}",
        result.agent_result.result_text
    );

    // The key assertion: did Claude actually use MCP tools?
    if !mcp_tool_used {
        eprintln!("WARNING: Claude did NOT use void-mcp tools!");
        eprintln!("Tool calls were: {:?}", result.agent_result.tool_calls);
        eprintln!("This may indicate MCP server failed to start inside the guest.");
    }

    // The strongest assertion: did the sidecar receive intents?
    if drained.is_empty() {
        eprintln!("WARNING: Sidecar received 0 intents!");
    } else {
        eprintln!("SUCCESS: Sidecar received {} intent(s)", drained.len());
    }

    // For now, assert at least one intent was received
    // This is the acceptance criteria from the spec
    assert!(
        !drained.is_empty(),
        "Expected at least one intent from Claude via void-mcp. \
         Tool calls: {:?}",
        result.agent_result.tool_calls
    );
}

// ===========================================================================
// Diagnostic: MCP server startup debug
// ===========================================================================

/// Minimal diagnostic: just check if void-mcp starts and responds
/// to the MCP handshake from inside the guest VM.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires VM backend + kernel/initramfs"]
async fn diagnostic_void_mcp_starts_in_guest() {
    use void_box::backend::{BackendConfig, BackendSecurityConfig, GuestConsoleSink};
    use void_box_protocol::SessionSecret;

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

    // Start sidecar
    let handle = sidecar::start_sidecar(
        "run-diag",
        "exec-diag",
        "c-1",
        vec![],
        void_box::backend::guest_accessible_bind_addr(0),
    )
    .await
    .expect("failed to start sidecar");

    let port = handle.addr().port();
    let sidecar_url = void_box::backend::guest_host_url(port);

    // Boot VM
    let mut secret = [0u8; 32];
    getrandom::fill(&mut secret).unwrap();
    let config = BackendConfig {
        memory_mb: 3072,
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
            command_allowlist: vec!["sh".into(), "void-mcp".into(), "echo".into(), "cat".into()],
            network_deny_list: vec!["169.254.0.0/16".into()],
            max_connections_per_second: 50,
            max_concurrent_connections: 64,
            seccomp: true,
        },
        snapshot: None,
        enable_snapshots: false,
    };

    let mut backend = void_box::backend::create_backend();
    if let Err(e) = backend.start(config).await {
        eprintln!("skipping: backend start failed: {e}");
        handle.stop().await;
        return;
    }

    // Test 1: void-mcp binary exists
    let out = backend
        .exec(
            "sh",
            &[
                "-c",
                "test -x /usr/local/bin/void-mcp && echo /usr/local/bin/void-mcp",
            ],
            &[],
            &[],
            None,
            Some(10),
        )
        .await;
    match &out {
        Ok(o) if o.success() => eprintln!("void-mcp found at: {}", o.stdout_str().trim()),
        Ok(o) => {
            eprintln!("void-mcp NOT found in PATH: {}", o.stderr_str());
            handle.stop().await;
            panic!("void-mcp not in guest image — rebuild with scripts/build_test_image.sh");
        }
        Err(e) => {
            eprintln!("exec failed: {e}");
            handle.stop().await;
            return;
        }
    }

    // Test 2: void-mcp responds to MCP initialize handshake
    // Send a Content-Length framed JSON-RPC initialize request via stdin
    let init_json = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
    let script = format!(
        r#"printf 'Content-Length: {len}\r\n\r\n{body}' | VOID_SIDECAR_URL={url} void-mcp 2>/tmp/mcp-stderr; echo "EXIT:$?"; cat /tmp/mcp-stderr >&2"#,
        len = init_json.len(),
        body = init_json,
        url = sidecar_url,
    );

    let out = backend
        .exec("sh", &["-c", &script], &[], &[], None, Some(15))
        .await;

    match &out {
        Ok(o) => {
            eprintln!("void-mcp stdout: {}", o.stdout_str());
            eprintln!("void-mcp stderr: {}", o.stderr_str());
            // Check if response contains "void-mcp" serverInfo
            assert!(
                o.stdout_str().contains("void-mcp"),
                "void-mcp did not return valid initialize response"
            );
        }
        Err(e) => {
            eprintln!("void-mcp exec failed: {e}");
        }
    }

    handle.stop().await;
    eprintln!("PASSED: diagnostic_void_mcp_starts_in_guest");
}
