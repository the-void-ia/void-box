//! Canonical Claude-in-void workflow example.
//!
//! Runs plan → apply using the mock sandbox (simulated claude-code).
//! Demonstrates workflow definition, piping, and observability.

use void_box::observe::ObserveConfig;
use void_box::sandbox::Sandbox;
use void_box::workflow::{Workflow, WorkflowExt};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sandbox = Sandbox::mock().build()?;

    let workflow = Workflow::define("claude-in-void")
        .step("plan", |ctx| async move {
            // In a real guest: claude-code plan /workspace
            ctx.exec("claude-code", &["plan", "/workspace"]).await
        })
        .step("apply", |ctx| async move {
            // Pipe plan output into claude-code apply
            ctx.exec_piped("claude-code", &["apply", "/workspace"]).await
        })
        .pipe("plan", "apply")
        .output("apply")
        .build();

    let observed = workflow
        .observe(ObserveConfig::test())
        .run_in(sandbox)
        .await?;

    println!("=== Workflow result ===");
    println!("success: {}", observed.result.success());
    println!("output: {}", observed.result.output_str().trim());
    println!("duration_ms: {}", observed.result.duration_ms);

    println!("\n=== Step outputs ===");
    for (name, out) in &observed.result.step_outputs {
        let stdout = String::from_utf8_lossy(&out.stdout);
        println!("  {}: exit={} stdout_len={}", name, out.exit_code, stdout.len());
        if !stdout.is_empty() && stdout.len() <= 200 {
            println!("    -> {}", stdout.trim());
        }
    }

    println!("\n=== Observability ({} traces, {} logs) ===",
        observed.traces().len(),
        observed.logs().len());
    for span in observed.traces() {
        println!("  span: {} status={:?}", span.name, span.status);
    }

    Ok(())
}
