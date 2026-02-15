//! Observability Playground Pipeline
//!
//! Runs a small workflow pipeline and exports traces/metrics via OTLP when
//! `VOIDBOX_OTLP_ENDPOINT` is configured.
//!
//! Recommended run:
//!   VOIDBOX_OTLP_ENDPOINT=http://localhost:4317 \
//!   VOIDBOX_SERVICE_NAME=void-box-playground \
//!   cargo run --example playground_pipeline --features opentelemetry

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use void_box::observe::ObserveConfig;
use void_box::sandbox::Sandbox;
use void_box::workflow::{Workflow, WorkflowExt};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let run_id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let provider = std::env::var("PLAYGROUND_PROVIDER").unwrap_or_else(|_| "mock".to_string());
    let workflow_name = format!("playground-{}-{}", provider, run_id);
    let sandbox = build_sandbox()?;

    let provider_for_ingest = provider.clone();
    let workflow = Workflow::define(&workflow_name)
        .step("ingest", move |ctx| {
            let provider = provider_for_ingest.clone();
            async move {
                ctx.exec(
                    "echo",
                    &[&format!(
                        "provider={};aapl,227,nvda,138,msft,442,googl,192",
                        provider
                    )],
                )
                .await
            }
        })
        .step("normalize", |ctx| async move {
            ctx.exec_piped("tr", &["a-z", "A-Z"]).await
        })
        .step("score", |ctx| async move {
            ctx.exec_piped("sha256sum", &[]).await
        })
        .pipe("ingest", "normalize")
        .pipe("normalize", "score")
        .output("score")
        .build();

    let mut observe = ObserveConfig::from_env()
        .enable_metrics(true)
        .enable_logs(true);
    observe.tracer.in_memory = true;
    observe.metrics.in_memory = true;
    observe.logs.in_memory = true;

    let observed = workflow.observe(observe).run_in(sandbox).await?;

    println!("=== Playground Pipeline ===");
    println!("workflow: {}", workflow_name);
    println!("provider: {}", provider);
    println!("success: {}", observed.result.success());
    println!("output: {}", observed.result.output_str().trim());
    println!("duration_ms: {}", observed.result.duration_ms);
    println!("traces captured: {}", observed.traces().len());
    println!("metrics captured: {}", observed.metrics().metrics.len());
    println!("logs captured: {}", observed.logs().len());

    println!();
    println!("=== Explore in Grafana ===");
    println!("Grafana: http://localhost:3000");
    println!(
        "Service: {}",
        std::env::var("VOIDBOX_SERVICE_NAME").unwrap_or_else(|_| "void-box-playground".into())
    );
    println!("Workflow span: workflow:{}", workflow_name);

    Ok(())
}

fn build_sandbox() -> Result<Arc<Sandbox>, Box<dyn std::error::Error>> {
    let has_kvm = std::path::Path::new("/dev/kvm").exists();
    let has_kernel = std::env::var("VOID_BOX_KERNEL")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    let has_initramfs = std::env::var("VOID_BOX_INITRAMFS")
        .map(|v| !v.is_empty())
        .unwrap_or(false);

    if has_kvm && has_kernel && has_initramfs {
        match Sandbox::local().from_env()?.network(true).build() {
            Ok(sb) => {
                eprintln!("[playground] mode=KVM");
                return Ok(sb);
            }
            Err(e) => {
                eprintln!("[playground] WARN: KVM setup failed ({e}), falling back to mock mode");
            }
        }
    } else {
        eprintln!("[playground] mode=Mock (set VOID_BOX_KERNEL and VOID_BOX_INITRAMFS to use KVM)");
    }

    Ok(Sandbox::mock().build()?)
}
