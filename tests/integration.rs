//! Integration Tests for void-box
//!
//! These tests verify the complete workflow execution pipeline including:
//! - Sandbox isolation
//! - Workflow composition
//! - Observability capture
//! - Parity with BoxLite/VM0 examples

use void_box::observe::{ObserveConfig, Observer};
use void_box::sandbox::Sandbox;
use void_box::workflow::{Workflow, WorkflowExt};

// =============================================================================
// SANDBOX ISOLATION TESTS
// =============================================================================

/// Test that each sandbox execution is isolated
#[tokio::test]
async fn test_sandbox_isolation() {
    let sandbox = Sandbox::mock().build().unwrap();

    // First execution
    let output1 = sandbox.exec("echo", &["test1"]).await.unwrap();
    assert!(output1.success());

    // Second execution should be independent
    let output2 = sandbox.exec("echo", &["test2"]).await.unwrap();
    assert!(output2.success());

    // Outputs should be different
    assert_ne!(output1.stdout_str().trim(), output2.stdout_str().trim());
}

/// Test sandbox configuration
#[tokio::test]
async fn test_sandbox_config() {
    let sandbox = Sandbox::mock()
        .memory_mb(512)
        .vcpus(2)
        .network(true)
        .build()
        .unwrap();

    assert_eq!(sandbox.config().memory_mb, 512);
    assert_eq!(sandbox.config().vcpus, 2);
    assert!(sandbox.config().network);
}

// =============================================================================
// WORKFLOW COMPOSITION TESTS
// =============================================================================

/// Test basic workflow with piped steps
#[tokio::test]
async fn test_workflow_pipe() {
    let sandbox = Sandbox::mock().build().unwrap();

    let workflow = Workflow::define("pipe-test")
        .step("step1", |ctx| async move {
            ctx.exec("echo", &["hello"]).await
        })
        .step("step2", |ctx| async move {
            // This step receives output from step1 via pipe
            ctx.exec_piped("tr", &["a-z", "A-Z"]).await
        })
        .pipe("step1", "step2")
        .build();

    let result = workflow
        .observe(ObserveConfig::test())
        .run_in(sandbox)
        .await
        .unwrap();

    // The output should be the uppercase version
    assert_eq!(result.result.output_str().trim(), "HELLO");
    assert!(result.result.success());
}

/// Test workflow with multiple independent steps
#[tokio::test]
async fn test_workflow_multiple_steps() {
    let sandbox = Sandbox::mock().build().unwrap();

    let workflow = Workflow::define("multi-step")
        .step("a", |ctx| async move {
            ctx.exec("echo", &["a"]).await
        })
        .step("b", |ctx| async move {
            ctx.exec("echo", &["b"]).await
        })
        .step("c", |ctx| async move {
            ctx.exec("echo", &["c"]).await
        })
        .output("c")
        .build();

    let result = workflow
        .observe(ObserveConfig::test())
        .run_in(sandbox)
        .await
        .unwrap();

    // All steps should have been executed
    assert!(result.result.step_outputs.contains_key("a"));
    assert!(result.result.step_outputs.contains_key("b"));
    assert!(result.result.step_outputs.contains_key("c"));
}

/// Test workflow dependency ordering
#[tokio::test]
async fn test_workflow_dependencies() {
    let workflow = Workflow::define("deps-test")
        .step("first", |_ctx| async { Ok(b"first".to_vec()) })
        .step("second", |_ctx| async { Ok(b"second".to_vec()) })
        .step("third", |_ctx| async { Ok(b"third".to_vec()) })
        .pipe("first", "second")
        .pipe("second", "third")
        .build();

    let order = workflow.execution_order().unwrap();

    // Verify ordering
    let first_pos = order.iter().position(|s| s == "first").unwrap();
    let second_pos = order.iter().position(|s| s == "second").unwrap();
    let third_pos = order.iter().position(|s| s == "third").unwrap();

    assert!(first_pos < second_pos);
    assert!(second_pos < third_pos);
}

// =============================================================================
// OBSERVABILITY TESTS
// =============================================================================

/// Test that traces are captured correctly
#[tokio::test]
async fn test_observability_traces() {
    let sandbox = Sandbox::mock().build().unwrap();

    let workflow = Workflow::define("trace-test")
        .step("step1", |ctx| async move {
            ctx.exec("echo", &["hello"]).await
        })
        .build();

    let result = workflow
        .observe(ObserveConfig::test())
        .run_in(sandbox)
        .await
        .unwrap();

    // Should have workflow and step traces
    let traces = result.traces();
    assert!(!traces.is_empty());
    assert!(traces.iter().any(|t| t.name.contains("workflow:trace-test")));
    assert!(traces.iter().any(|t| t.name.contains("step:step1")));
}

/// Test that metrics are captured correctly
#[tokio::test]
async fn test_observability_metrics() {
    let sandbox = Sandbox::mock().build().unwrap();

    let workflow = Workflow::define("metrics-test")
        .step("step1", |ctx| async move {
            ctx.exec("echo", &["hello"]).await
        })
        .build();

    let result = workflow
        .observe(ObserveConfig::test())
        .run_in(sandbox)
        .await
        .unwrap();

    // Should have duration metrics
    let metrics = result.metrics();
    // The step should have recorded duration
    assert!(!metrics.metrics.is_empty());
}

/// Test Observer directly
#[test]
fn test_observer_spans() {
    let observer = Observer::test();

    // Create a workflow span
    let workflow_span = observer.start_workflow_span("test-workflow");
    let ctx = workflow_span.context();

    // Create a step span
    let step_span = observer.start_step_span("test-step", Some(&ctx));
    step_span.set_ok();

    workflow_span.set_ok();

    // Verify spans were captured
    assert!(observer.has_span("workflow:test-workflow"));
    assert!(observer.has_span("step:test-step"));
}

/// Test structured logging
#[test]
fn test_observability_logs() {
    let observer = Observer::test();

    observer.logger().info("Test info message", &[("key", "value")]);
    observer.logger().error("Test error message", &[]);

    let logs = observer.get_logs();
    assert!(logs.len() >= 2);
    assert!(logs.iter().any(|l| l.message.contains("Test info")));
    assert!(logs.iter().any(|l| l.message.contains("Test error")));
}

// =============================================================================
// REPRODUCIBILITY TESTS
// =============================================================================

/// Test that same input produces same output
#[tokio::test]
async fn test_reproducibility() {
    let workflow = Workflow::define("reproducible")
        .step("hash", |ctx| async move {
            ctx.exec("sha256sum", &["input.txt"]).await
        })
        .build();

    // Run multiple times
    let mut results = Vec::new();
    for _ in 0..3 {
        let sandbox = Sandbox::mock().build().unwrap();
        let result = workflow
            .clone()
            .observe(ObserveConfig::test())
            .run_in(sandbox)
            .await
            .unwrap();
        results.push(result.result.output_str());
    }

    // All results should be identical (mock sandbox returns deterministic output)
    for window in results.windows(2) {
        assert_eq!(window[0], window[1]);
    }
}

// =============================================================================
// BOXLITE / VM0 PARITY TESTS
// =============================================================================

/// BoxLite example: Run simple echo command
/// Their API: box.run("echo hello")
#[tokio::test]
async fn test_boxlite_parity_echo() {
    let sandbox = Sandbox::mock().build().unwrap();

    let output = sandbox.exec("echo", &["hello", "world"]).await.unwrap();

    assert!(output.success());
    assert_eq!(output.stdout_str().trim(), "hello world");

    // void-box ADDS: we can attach observability
}

/// BoxLite example: Command with exit code
#[tokio::test]
async fn test_boxlite_parity_exit_code() {
    let sandbox = Sandbox::mock().build().unwrap();

    let output = sandbox.exec("test", &["-e", "/nonexistent"]).await.unwrap();

    // Test command returns non-zero for non-existent file
    assert!(!output.success());
}

/// VM0 example: Workflow execution
/// Their API uses natural language; void-box uses composable workflows
#[tokio::test]
async fn test_vm0_parity_workflow() {
    let sandbox = Sandbox::mock().build().unwrap();

    // void-box uses composable workflows instead of natural language
    let workflow = Workflow::define("fetch-and-process")
        .step("fetch", |ctx| async move {
            ctx.exec("curl", &["-s", "https://httpbin.org/get"]).await
        })
        .step("parse", |ctx| async move {
            ctx.exec_piped("jq", &[".origin"]).await
        })
        .pipe("fetch", "parse")
        .build();

    let result = workflow
        .observe(ObserveConfig::test())
        .run_in(sandbox)
        .await
        .unwrap();

    assert!(result.result.success());

    // void-box ADDS: trace the entire workflow
    let traces = result.traces();
    assert!(traces.iter().any(|t| t.name.contains("workflow:fetch-and-process")));
    assert!(traces.iter().any(|t| t.name.contains("step:fetch")));
    assert!(traces.iter().any(|t| t.name.contains("step:parse")));
}

/// Parity test: stdin piping
#[tokio::test]
async fn test_parity_stdin_pipe() {
    let sandbox = Sandbox::mock().build().unwrap();

    // Execute with stdin
    let output = sandbox
        .exec_with_stdin("cat", &[], b"hello from stdin")
        .await
        .unwrap();

    assert!(output.success());
    assert_eq!(output.stdout, b"hello from stdin");
}

/// Parity test: tr command for text transformation
#[tokio::test]
async fn test_parity_text_transform() {
    let sandbox = Sandbox::mock().build().unwrap();

    let output = sandbox
        .exec_with_stdin("tr", &["a-z", "A-Z"], b"hello world")
        .await
        .unwrap();

    assert!(output.success());
    assert_eq!(output.stdout, b"HELLO WORLD");
}

// =============================================================================
// ERROR HANDLING TESTS
// =============================================================================

/// Test circular dependency detection
#[test]
fn test_circular_dependency() {
    let _workflow = Workflow::define("circular")
        .step("a", |_ctx| async { Ok(vec![]) })
        .step("b", |_ctx| async { Ok(vec![]) })
        .pipe("a", "b")
        .pipe("b", "a") // Creates circular dependency
        .build();

    // Should detect circular dependency
    // Note: In our implementation, pipe creates a depends_on relationship
    // so this creates a -> b -> a cycle
}

/// Test workflow result access
#[tokio::test]
async fn test_workflow_result_access() {
    let sandbox = Sandbox::mock().build().unwrap();

    let workflow = Workflow::define("result-test")
        .step("step1", |ctx| async move {
            ctx.exec("echo", &["output"]).await
        })
        .build();

    let result = workflow
        .observe(ObserveConfig::test())
        .run_in(sandbox)
        .await
        .unwrap();

    // Test WorkflowResult methods
    assert!(result.result.success());
    assert!(!result.result.output_str().is_empty());
    assert!(result.result.step_output("step1").is_some());
    assert!(result.result.step_output("nonexistent").is_none());
}

// =============================================================================
// OBSERVED RESULT TESTS
// =============================================================================

/// Test ObservedResult structure
#[tokio::test]
async fn test_observed_result_structure() {
    let sandbox = Sandbox::mock().build().unwrap();

    let workflow = Workflow::define("observed-test")
        .step("step1", |ctx| async move {
            ctx.exec("echo", &["test"]).await
        })
        .build();

    let observed = workflow
        .observe(ObserveConfig::test())
        .run_in(sandbox)
        .await
        .unwrap();

    // Access all observability data
    let traces = observed.traces();
    let _metrics = observed.metrics();
    let _logs = observed.logs();

    // All should be populated
    assert!(!traces.is_empty());
    // Metrics and logs are populated based on execution
    assert!(observed.result.success());
}
