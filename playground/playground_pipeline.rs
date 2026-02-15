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

use void_box::observe::{flush_global_otel, ObserveConfig};
use void_box::sandbox::Sandbox;
use void_box::workflow::{Workflow, WorkflowExt};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let started_at_ms = now_ms();
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
    let ended_at_ms = now_ms();

    if let Err(e) = flush_global_otel() {
        eprintln!("[playground] WARN: failed to flush OTLP exporters: {e}");
    }

    let grafana_base = std::env::var("PLAYGROUND_GRAFANA_URL")
        .unwrap_or_else(|_| "http://localhost:3000".to_string());
    let service_name =
        std::env::var("VOIDBOX_SERVICE_NAME").unwrap_or_else(|_| "void-box-playground".into());
    let workflow_span = format!("workflow:{workflow_name}");

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
    println!("Grafana: {}", grafana_base);
    println!("Service: {}", service_name);
    println!("Workflow span: {}", workflow_span);
    println!(
        "Traces URL: {}",
        grafana_trace_url(
            &grafana_base,
            &service_name,
            &workflow_span,
            started_at_ms,
            ended_at_ms
        )
    );
    println!(
        "Metrics URL: {}",
        grafana_metrics_url(&grafana_base, &service_name, started_at_ms, ended_at_ms)
    );
    if let Ok(log_path) = std::env::var("PLAYGROUND_LOG_PATH") {
        if !log_path.is_empty() {
            println!("Logs (local): {}", log_path);
        }
    }

    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn grafana_trace_url(
    grafana_base: &str,
    service_name: &str,
    workflow_span: &str,
    from_ms: u64,
    to_ms: u64,
) -> String {
    let query = format!(
        "{{ resource.service.name = \"{}\" && name = \"{}\" }}",
        service_name, workflow_span
    );
    let left = format!(
        "[{}, {}, \"tempo\", {{\"queryType\":\"traceql\",\"query\":\"{}\",\"refId\":\"A\"}}]",
        from_ms,
        to_ms.saturating_add(1000),
        escape_json_string(&query),
    );

    format!(
        "{}/explore?orgId=1&left={}",
        grafana_base.trim_end_matches('/'),
        percent_encode(&left)
    )
}

fn grafana_metrics_url(grafana_base: &str, service_name: &str, from_ms: u64, to_ms: u64) -> String {
    let _ = service_name;
    let expr = String::from(
        "sum by (__name__) ({__name__=~\"(ingest|normalize|score)_duration_ms(_bucket|_sum|_count)?\"})",
    );
    let left = format!(
        "[{}, {}, \"prometheus\", {{\"refId\":\"A\",\"expr\":\"{}\"}}]",
        from_ms,
        to_ms.saturating_add(1000),
        escape_json_string(&expr)
    );

    format!(
        "{}/explore?orgId=1&left={}",
        grafana_base.trim_end_matches('/'),
        percent_encode(&left)
    )
}

fn escape_json_string(input: &str) -> String {
    input.replace('\\', "\\\\").replace('"', "\\\"")
}

fn percent_encode(input: &str) -> String {
    let mut encoded = String::with_capacity(input.len() * 3 / 2);
    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(char::from(b));
            }
            _ => {
                encoded.push('%');
                encoded.push(char::from(b"0123456789ABCDEF"[(b >> 4) as usize]));
                encoded.push(char::from(b"0123456789ABCDEF"[(b & 0x0F) as usize]));
            }
        }
    }
    encoded
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
