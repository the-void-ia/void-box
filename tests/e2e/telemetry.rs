//! End-to-End Telemetry Tests
//!
//! These tests use a real KVM micro-VM with `claudio` (mock claude-code) installed
//! as `/usr/local/bin/claude-code` in the test initramfs, providing deterministic
//! telemetry output without requiring an Anthropic API key.
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
//!    cargo test --test e2e_telemetry -- --ignored
//!    ```
//!
//! All tests are `#[ignore]` so they don't run in a normal `cargo test`.

use std::path::PathBuf;
use std::sync::Arc;

use void_box::observe::claude::{parse_stream_json, ClaudeExecOpts};
use void_box::observe::tracer::{SpanContext, Tracer, TracerConfig};
use void_box::sandbox::Sandbox;
use void_box::vmm::config::VoidBoxConfig;
use void_box::vmm::VoidBox;

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

fn kvm_available() -> bool {
    std::path::Path::new("/dev/kvm").exists()
}

fn vsock_available() -> bool {
    std::path::Path::new("/dev/vhost-vsock").exists()
}

fn kvm_artifacts_from_env() -> Option<(PathBuf, Option<PathBuf>)> {
    let kernel = std::env::var_os("VOID_BOX_KERNEL")?;
    let kernel = PathBuf::from(kernel);
    let initramfs = std::env::var_os("VOID_BOX_INITRAMFS").map(PathBuf::from);
    Some((kernel, initramfs))
}

/// Try to build a VoidBoxConfig. Returns `None` if KVM or artifacts are unavailable.
fn setup_test_vm() -> Option<(VoidBoxConfig, PathBuf, Option<PathBuf>)> {
    if !kvm_available() {
        eprintln!("skipping: /dev/kvm not available");
        return None;
    }
    if !vsock_available() {
        eprintln!("skipping: /dev/vhost-vsock not available");
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

    if !kernel.exists() {
        eprintln!("skipping: kernel not found at {}", kernel.display());
        return None;
    }

    if let Some(ref p) = initramfs {
        if !p.exists() {
            eprintln!("skipping: initramfs not found at {}", p.display());
            return None;
        }
    }

    let mut cfg = VoidBoxConfig::new()
        .memory_mb(256)
        .vcpus(1)
        .kernel(&kernel)
        .enable_vsock(true);

    if let Some(ref p) = initramfs {
        cfg = cfg.initramfs(p);
    }

    Some((cfg, kernel, initramfs))
}

/// Build a Sandbox::local() backed by a real KVM VM.
fn build_test_sandbox() -> Option<Arc<Sandbox>> {
    if !kvm_available() {
        eprintln!("skipping: /dev/kvm not available");
        return None;
    }
    if !vsock_available() {
        eprintln!("skipping: /dev/vhost-vsock not available");
        return None;
    }

    let (kernel, initramfs) = match kvm_artifacts_from_env() {
        Some(a) => a,
        None => {
            eprintln!("skipping: set VOID_BOX_KERNEL and VOID_BOX_INITRAMFS");
            return None;
        }
    };

    if !kernel.exists() {
        return None;
    }

    let mut builder = Sandbox::local().memory_mb(256).vcpus(1).kernel(&kernel);

    if let Some(ref p) = initramfs {
        if !p.exists() {
            return None;
        }
        builder = builder.initramfs(p);
    }

    match builder.build() {
        Ok(sb) => Some(sb),
        Err(e) => {
            eprintln!("skipping: failed to build sandbox: {e}");
            None
        }
    }
}

/// Build a Sandbox::local() with custom env vars for claudio configuration.
fn build_test_sandbox_with_env(env: Vec<(&str, &str)>) -> Option<Arc<Sandbox>> {
    if !kvm_available() {
        return None;
    }
    if !vsock_available() {
        eprintln!("skipping: /dev/vhost-vsock not available");
        return None;
    }

    let (kernel, initramfs) = match kvm_artifacts_from_env() {
        Some(a) => a,
        None => return None,
    };

    if !kernel.exists() {
        return None;
    }

    let mut builder = Sandbox::local().memory_mb(256).vcpus(1).kernel(&kernel);

    if let Some(ref p) = initramfs {
        if !p.exists() {
            return None;
        }
        builder = builder.initramfs(p);
    }

    for (k, v) in env {
        builder = builder.env(k, v);
    }

    match builder.build() {
        Ok(sb) => Some(sb),
        Err(e) => {
            eprintln!("skipping: failed to build sandbox: {e}");
            None
        }
    }
}

/// Helper: run claudio in a sandbox with given scenario env vars, return parsed result.
async fn run_claudio(
    sandbox: &Sandbox,
    prompt: &str,
) -> void_box::observe::claude::ClaudeExecResult {
    let output = sandbox
        .exec(
            "claude-code",
            &["-p", prompt, "--output-format", "stream-json"],
        )
        .await
        .expect("exec failed");

    assert!(
        output.success(),
        "claude-code exited with code {}: stderr={}",
        output.exit_code,
        output.stderr_str(),
    );

    parse_stream_json(&output.stdout)
}

// ===========================================================================
// Test 1: Default scenario -- parsing, spans, and exec_claude (single VM)
// ===========================================================================

/// Boot ONE VM and run multiple checks on the default (simple) scenario:
///  - stream-json parsing correctness
///  - OTel span creation from result
///  - exec_claude() high-level wrapper
#[tokio::test]
#[ignore = "requires KVM + test initramfs from scripts/build_test_image.sh"]
async fn test_default_scenario() {
    let sandbox = match build_test_sandbox() {
        Some(sb) => sb,
        None => return,
    };

    // --- A) Parse stream-json ---
    let result = run_claudio(&sandbox, "hello world test").await;

    assert!(
        !result.session_id.is_empty(),
        "session_id should be populated"
    );
    assert!(
        result.model.contains("claude") || result.model.contains("sonnet"),
        "model should contain 'claude' or 'sonnet', got: {}",
        result.model
    );
    assert!(
        !result.tool_calls.is_empty(),
        "should have at least 1 tool call"
    );
    assert_eq!(
        result.tool_calls[0].tool_name, "Write",
        "first tool should be Write in simple scenario"
    );
    assert!(!result.is_error, "should not be an error");
    assert!(result.input_tokens > 0, "should have input tokens");
    assert!(result.output_tokens > 0, "should have output tokens");
    assert!(result.total_cost_usd > 0.0, "should have a cost");
    assert!(result.num_turns >= 1, "should have at least 1 turn");
    assert!(
        result.result_text.contains("hello world test"),
        "result should echo the prompt, got: {}",
        result.result_text
    );
    eprintln!("  [A] stream-json parsed correctly");

    // --- B) OTel span creation ---
    let tracer = Tracer::new(TracerConfig::in_memory());
    void_box::observe::claude::create_otel_spans(&result, None, &tracer);

    let spans = tracer.get_spans();
    let exec_spans: Vec<_> = spans.iter().filter(|s| s.name == "claude.exec").collect();
    let tool_spans: Vec<_> = spans
        .iter()
        .filter(|s| s.name.starts_with("claude.tool."))
        .collect();

    assert_eq!(
        exec_spans.len(),
        1,
        "should have exactly 1 claude.exec span"
    );
    assert_eq!(
        tool_spans.len(),
        result.tool_calls.len(),
        "should have one tool span per tool call"
    );

    let exec = &exec_spans[0];
    // OTel GenAI semconv: Required
    assert!(exec.attributes.contains_key("gen_ai.operation.name"));
    assert!(exec.attributes.contains_key("gen_ai.system"));
    // OTel GenAI semconv: Conditionally Required / Recommended
    assert!(exec.attributes.contains_key("gen_ai.request.model"));
    assert!(exec.attributes.contains_key("gen_ai.response.model"));
    assert!(exec.attributes.contains_key("gen_ai.conversation.id"));
    assert!(exec.attributes.contains_key("gen_ai.usage.input_tokens"));
    assert!(exec.attributes.contains_key("gen_ai.usage.output_tokens"));
    // Custom void-box extensions
    assert!(exec.attributes.contains_key("claude.total_cost_usd"));

    for (i, tool_call) in result.tool_calls.iter().enumerate() {
        let expected_name = format!("claude.tool.{}", tool_call.tool_name);
        assert_eq!(tool_spans[i].name, expected_name, "tool span name mismatch");
    }
    eprintln!("  [B] OTel spans created correctly");

    // --- C) exec_claude() high-level wrapper ---
    let opts = ClaudeExecOpts {
        dangerously_skip_permissions: true,
        ..Default::default()
    };
    let result2 = sandbox
        .exec_claude("exec_claude test", opts)
        .await
        .expect("exec_claude failed");
    assert!(!result2.is_error, "exec_claude should not error");
    assert!(
        !result2.session_id.is_empty(),
        "exec_claude should have session_id"
    );
    assert!(
        !result2.tool_calls.is_empty(),
        "exec_claude should have tool calls"
    );
    eprintln!("  [C] exec_claude() works correctly");

    // Note: we don't call sandbox.stop() -- vCPU threads are blocked in KVM_RUN
    // and won't exit until the process ends. The Drop impl handles cleanup.
    eprintln!("PASSED: test_default_scenario (3 checks on 1 VM)");
}

// ===========================================================================
// Test 2: TRACEPARENT propagation (needs raw VoidBox for set_span_context)
// ===========================================================================

/// Set a known span context on the VM and verify TRACEPARENT reaches the guest
/// by checking claudio's system event output.
#[tokio::test]
#[ignore = "requires KVM + test initramfs from scripts/build_test_image.sh"]
async fn test_traceparent_propagation() {
    let (cfg, _kernel, _initramfs) = match setup_test_vm() {
        Some(v) => v,
        None => return,
    };

    cfg.validate().expect("invalid config");
    let mut vm = VoidBox::new(cfg).await.expect("failed to create VM");

    let trace_id = "aaaabbbbccccddddeeeeffff00001111";
    let span_id = "1234567890abcdef";
    let ctx = SpanContext {
        trace_id: trace_id.to_string(),
        span_id: span_id.to_string(),
        parent_span_id: None,
        trace_flags: 1,
    };
    vm.set_span_context(ctx);

    let output = vm
        .exec_with_env(
            "claude-code",
            &["-p", "trace test", "--output-format", "stream-json"],
            &[],
            &[],
            None,
        )
        .await
        .expect("exec failed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let expected_traceparent = format!("00-{}-{}-01", trace_id, span_id);

    assert!(
        stdout.contains(&expected_traceparent),
        "claudio output should contain TRACEPARENT.\nExpected: {}\nGot:\n{}",
        expected_traceparent,
        stdout,
    );

    eprintln!("PASSED: test_traceparent_propagation");
}

// ===========================================================================
// Test 3: Telemetry aggregator with guest metrics (needs raw VoidBox)
// ===========================================================================

/// Boot a VM, subscribe to telemetry, verify guest CPU/memory metrics arrive.
#[tokio::test]
#[ignore = "requires KVM + test initramfs from scripts/build_test_image.sh"]
async fn test_telemetry_aggregator() {
    let (cfg, _kernel, _initramfs) = match setup_test_vm() {
        Some(v) => v,
        None => return,
    };

    cfg.validate().expect("invalid config");
    let mut vm = VoidBox::new(cfg).await.expect("failed to create VM");

    // Warmup: ensure VM is ready
    let _warmup = vm
        .exec(
            "claude-code",
            &["-p", "warmup", "--output-format", "stream-json"],
        )
        .await;

    let observer = void_box::observe::Observer::test();
    let agg = vm
        .start_telemetry(observer.clone())
        .await
        .expect("failed to start telemetry");

    // Poll for telemetry (guest-agent sends every ~2s)
    let mut latest = None;
    for _ in 0..5 {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        latest = agg.latest_batch();
        if latest.is_some() {
            break;
        }
    }

    assert!(
        latest.is_some(),
        "should have received at least 1 telemetry batch after 10s"
    );

    let batch = latest.unwrap();
    assert!(
        batch.system.is_some(),
        "batch should contain system metrics"
    );

    if let Some(ref sys) = batch.system {
        assert!(
            sys.cpu_percent >= 0.0 && sys.cpu_percent <= 100.0,
            "cpu_percent should be 0-100, got: {}",
            sys.cpu_percent
        );
        assert!(
            sys.memory_total_bytes > 0,
            "memory_total_bytes should be > 0"
        );
    }

    let snapshot = observer.get_metrics();
    assert!(
        !snapshot.metrics.is_empty(),
        "observer should have recorded guest metrics"
    );

    eprintln!("PASSED: test_telemetry_aggregator");
}

// ===========================================================================
// Test 4: Scenario variations (single VM, multiple scenarios via env)
// ===========================================================================

/// Run the error scenario and verify error is parsed and reflected in spans.
#[tokio::test]
#[ignore = "requires KVM + test initramfs from scripts/build_test_image.sh"]
async fn test_error_scenario() {
    let sandbox = match build_test_sandbox_with_env(vec![("MOCK_CLAUDE_SCENARIO", "error")]) {
        Some(sb) => sb,
        None => return,
    };

    let output = sandbox
        .exec(
            "claude-code",
            &["-p", "trigger error", "--output-format", "stream-json"],
        )
        .await
        .expect("exec failed");

    let result = parse_stream_json(&output.stdout);

    assert!(
        result.is_error,
        "error scenario should produce is_error=true"
    );
    assert!(result.error.is_some(), "should have error message");
    assert!(
        result.error.as_ref().unwrap().contains("Permission denied"),
        "error message should contain 'Permission denied', got: {:?}",
        result.error,
    );
    assert!(
        !result.tool_calls.is_empty(),
        "error scenario should still have tool calls before error"
    );

    // Verify OTel span has error attributes
    let tracer = Tracer::new(TracerConfig::in_memory());
    void_box::observe::claude::create_otel_spans(&result, None, &tracer);
    let spans = tracer.get_spans();
    let exec_spans: Vec<_> = spans.iter().filter(|s| s.name == "claude.exec").collect();
    assert_eq!(exec_spans.len(), 1);
    assert_eq!(
        exec_spans[0].attributes.get("error.type"),
        Some(&"agent_error".to_string()),
        "error.type should be set on error spans (OTel semconv)"
    );

    eprintln!("PASSED: test_error_scenario");
}

/// Heavy scenario: 20 tool calls across 5 tool types, 10 turns.
#[tokio::test]
#[ignore = "requires KVM + test initramfs from scripts/build_test_image.sh"]
async fn test_heavy_scenario() {
    let sandbox = match build_test_sandbox_with_env(vec![("MOCK_CLAUDE_SCENARIO", "heavy")]) {
        Some(sb) => sb,
        None => return,
    };

    let result = run_claudio(&sandbox, "build a complex application").await;

    assert!(!result.is_error, "heavy scenario should succeed");
    assert!(
        result.tool_calls.len() >= 20,
        "heavy: expected 20+ tool calls, got: {}",
        result.tool_calls.len()
    );
    let tool_names: std::collections::HashSet<_> = result
        .tool_calls
        .iter()
        .map(|t| t.tool_name.as_str())
        .collect();
    assert!(
        tool_names.len() >= 3,
        "heavy: expected 3+ tool types, got: {:?}",
        tool_names,
    );
    assert_eq!(result.num_turns, 10, "heavy scenario should have 10 turns");

    let tracer = Tracer::new(TracerConfig::in_memory());
    void_box::observe::claude::create_otel_spans(&result, None, &tracer);
    let spans = tracer.get_spans();
    let tool_span_count = spans
        .iter()
        .filter(|s| s.name.starts_with("claude.tool."))
        .count();
    assert_eq!(tool_span_count, result.tool_calls.len());

    eprintln!(
        "PASSED: test_heavy_scenario ({} tool calls)",
        result.tool_calls.len()
    );
}

/// Multi-tool scenario: Read, Write, Bash, Read, Write in order.
#[tokio::test]
#[ignore = "requires KVM + test initramfs from scripts/build_test_image.sh"]
async fn test_multi_tool_scenario() {
    let sandbox = match build_test_sandbox_with_env(vec![("MOCK_CLAUDE_SCENARIO", "multi_tool")]) {
        Some(sb) => sb,
        None => return,
    };

    let result = run_claudio(&sandbox, "refactor the code").await;

    assert!(!result.is_error);
    assert_eq!(
        result.tool_calls.len(),
        5,
        "multi_tool should have 5 tool calls"
    );
    let names: Vec<&str> = result
        .tool_calls
        .iter()
        .map(|t| t.tool_name.as_str())
        .collect();
    assert_eq!(names, vec!["Read", "Write", "Bash", "Read", "Write"]);
    assert_eq!(result.num_turns, 3);

    eprintln!("PASSED: test_multi_tool_scenario");
}

// ===========================================================================
// Test 5: Configurable tokens + TRACEPARENT via env (single VM)
// ===========================================================================

/// Test custom token counts, cost, and TRACEPARENT propagation via sandbox env.
#[tokio::test]
#[ignore = "requires KVM + test initramfs from scripts/build_test_image.sh"]
async fn test_configurable_env_overrides() {
    let sandbox = match build_test_sandbox_with_env(vec![
        ("MOCK_CLAUDE_INPUT_TOKENS", "1234"),
        ("MOCK_CLAUDE_OUTPUT_TOKENS", "567"),
        ("MOCK_CLAUDE_COST", "0.042"),
        (
            "TRACEPARENT",
            "00-abcd1234abcd1234abcd1234abcd1234-1234567890abcdef-01",
        ),
    ]) {
        Some(sb) => sb,
        None => return,
    };

    let output = sandbox
        .exec(
            "claude-code",
            &["-p", "combined test", "--output-format", "stream-json"],
        )
        .await
        .expect("exec failed");

    let result = parse_stream_json(&output.stdout);

    // --- Token / cost overrides ---
    assert_eq!(
        result.input_tokens, 1234,
        "input tokens should be overridden"
    );
    assert_eq!(
        result.output_tokens, 567,
        "output tokens should be overridden"
    );
    assert!(
        (result.total_cost_usd - 0.042).abs() < 0.0001,
        "cost should be 0.042, got: {}",
        result.total_cost_usd,
    );
    eprintln!("  [tokens] overrides OK");

    // --- TRACEPARENT in output ---
    let stdout_str = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout_str.contains("abcd1234abcd1234abcd1234abcd1234"),
        "system event should contain the trace_id from TRACEPARENT",
    );
    eprintln!("  [traceparent] propagation OK");

    eprintln!("PASSED: test_configurable_env_overrides (tokens + traceparent on 1 VM)");
}
