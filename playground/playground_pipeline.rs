//! Observability Playground Pipeline
//!
//! Runs a 4-stage trading-analysis Pipeline with full OTLP instrumentation
//! (traces, metrics) exported to the local LGTM stack.
//!
//! Each stage is a VoidBox running claude-code in a real KVM micro-VM with
//! either Ollama or the Anthropic API as the LLM provider. Traces in Grafana
//! show real `pipeline:*` → `stage:*` → `claude.exec` → `claude.tool.*` spans.
//!
//! Recommended run (via helper script):
//!   playground/up.sh
//!
//! Or manually:
//!   OLLAMA_MODEL=phi4-mini \
//!   VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
//!   VOID_BOX_INITRAMFS=/tmp/void-box-rootfs.cpio.gz \
//!   VOIDBOX_OTLP_ENDPOINT=http://localhost:4317 \
//!   VOIDBOX_SERVICE_NAME=void-box-playground \
//!   cargo run --example playground_pipeline --features opentelemetry

#[path = "../examples/common/mod.rs"]
mod common;

use std::time::{SystemTime, UNIX_EPOCH};

use void_box::observe::ObserveConfig;
use void_box::pipeline::Pipeline;
use void_box::skill::Skill;

use common::{detect_llm_provider, is_kvm_available, make_box};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    // ---- Require KVM ----

    if !is_kvm_available() {
        eprintln!("ERROR: KVM is required for the playground pipeline.");
        eprintln!();
        eprintln!("  1. Ensure /dev/kvm exists (KVM-capable host)");
        eprintln!("  2. Build the guest image:");
        eprintln!("       scripts/build_guest_image.sh");
        eprintln!("  3. Export artifacts:");
        eprintln!("       export VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r)");
        eprintln!("       export VOID_BOX_INITRAMFS=/tmp/void-box-rootfs.cpio.gz");
        std::process::exit(1);
    }

    let started_at_ms = now_ms();
    let run_id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let pipeline_name = format!("trading_analysis_{}", run_id);

    // ---- LLM Provider: Claude (default) or Ollama (opt-in) ----

    let llm = detect_llm_provider();
    eprintln!("[playground] llm: {}", llm);

    // ---- Skills ----

    let reasoning =
        Skill::agent("claude-code").description("Autonomous reasoning and code execution");

    let data_skill = Skill::file("examples/trading_pipeline/skills/financial-data-analysis.md")
        .description("Financial data collection and quality methodology");

    let quant_skill = Skill::file("examples/trading_pipeline/skills/quant-technical-analysis.md")
        .description("Technical indicator computation and signal generation");

    let risk_skill = Skill::file("examples/trading_pipeline/skills/portfolio-risk-management.md")
        .description("Portfolio risk management and position sizing");

    let market_data_mcp =
        Skill::mcp("market-data").description("Provides OHLCV and news data for equities");

    // ---- Boxes (real KVM) ----

    // claude-code (Bun SEA) reserves ~3.5GB virtual; 1024MB default OOMs.
    // Max usable is ~3200MB (MMIO gap starts at 3.25GB).
    let vm_memory_mb = 3072;

    let data_box = make_box("data_analyst", true, &llm)
        .memory_mb(vm_memory_mb)
        .skill(data_skill)
        .skill(market_data_mcp)
        .skill(reasoning.clone())
        .prompt(
            "You are a financial data analyst. Here is recent market data (Feb 2026):\n\n\
             AAPL: price $227, P/E 34, RSI 62, 52w range $170-$243, EPS $2.40 beat est. $2.36, \
             Services revenue missed ($23.1B vs $23.5B est.), iPhone revenue +4% YoY.\n\
             NVDA: price $138, P/E 55, RSI 71, 52w range $78-$153, data center revenue +95% YoY, \
             new Blackwell GPU ramping, China export restrictions tightening.\n\
             MSFT: price $442, P/E 36, RSI 58, 52w range $385-$470, Azure grew +29%, \
             Copilot revenue accelerating, gaming flat YoY.\n\
             GOOGL: price $192, P/E 24, RSI 55, 52w range $152-$207, Search +12%, \
             Cloud +28%, DOJ antitrust ruling pending.\n\n\
             For each symbol, write a brief data summary with key metrics and recent catalysts.\n\
             Do NOT write or run code. Do NOT output JSON or templates.\n\
             Write plain text with clear sections per symbol.",
        )
        .build()?;

    let quant_box = make_box("quant_analyst", true, &llm)
        .memory_mb(vm_memory_mb)
        .skill(quant_skill)
        .skill(reasoning.clone())
        .prompt(
            "You are a quantitative analyst. Read the data summary from /workspace/input.json.\n\n\
             For each symbol (AAPL, NVDA, MSFT, GOOGL), provide:\n\
             - RSI interpretation (overbought >70, neutral 30-70, oversold <30)\n\
             - P/E relative to sector average (Tech sector avg ~28)\n\
             - A composite signal: BULLISH, NEUTRAL, or BEARISH\n\n\
             Write plain text. Do NOT output JSON or templates.\n\
             Use the actual numbers from the input data.",
        )
        .build()?;

    let research_box = make_box("research_analyst", true, &llm)
        .memory_mb(vm_memory_mb)
        .skill(reasoning.clone())
        .prompt(
            "You are a research analyst. Read the quant analysis from /workspace/input.json.\n\n\
             For each symbol (AAPL, NVDA, MSFT, GOOGL):\n\
             - Score sentiment from -1.0 (very bearish) to +1.0 (very bullish)\n\
             - Write 2 sentences explaining your score\n\n\
             Consider the technical signals, fundamentals, and catalysts from the input.\n\
             Write plain text. Do NOT output JSON or templates.\n\
             Example: AAPL: +0.3 (mildly bullish). The earnings beat suggests...",
        )
        .build()?;

    let strategy_box = make_box("portfolio_strategist", true, &llm)
        .memory_mb(vm_memory_mb)
        .skill(risk_skill)
        .skill(reasoning.clone())
        .prompt(
            "You are a portfolio strategist managing a $100,000 portfolio.\n\
             Read the sentiment analysis from /workspace/input.json.\n\n\
             For each symbol (AAPL, NVDA, MSFT, GOOGL) produce a trade recommendation:\n\
             - ACTION: BUY, SELL, or HOLD\n\
             - ALLOCATION: percentage of portfolio (must sum to <=100%)\n\
             - ENTRY PRICE: target buy price\n\
             - STOP LOSS: price to cut losses (set 5-10% below entry)\n\
             - RATIONALE: one sentence\n\n\
             Keep at least 20% in cash. Write plain text. Do NOT output JSON or templates.\n\
             Use real numbers from the analysis, not placeholders.",
        )
        .build()?;

    // ---- Observability config ----

    let mut observe = ObserveConfig::from_env()
        .enable_metrics(true)
        .enable_logs(true);
    // Keep in-memory copies so the summary below can report counts,
    // while also exporting via OTLP to Grafana.
    observe.tracer.in_memory = true;
    observe.metrics.in_memory = true;
    observe.logs.in_memory = true;

    // ---- Run pipeline with observe ----

    eprintln!("[playground] pipeline: {}", pipeline_name);
    eprintln!(
        "[playground] stages: data_analyst -> quant_analyst -> research_analyst -> portfolio_strategist"
    );

    let observed = Pipeline::named(&pipeline_name, data_box)
        .pipe(quant_box)
        .pipe(research_box)
        .pipe(strategy_box)
        .observe(observe)
        .run()
        .await?;

    let ended_at_ms = now_ms();
    let result = &observed.result;

    // ---- Summary ----

    let grafana_base = std::env::var("PLAYGROUND_GRAFANA_URL")
        .unwrap_or_else(|_| "http://localhost:3000".to_string());
    let service_name =
        std::env::var("VOIDBOX_SERVICE_NAME").unwrap_or_else(|_| "void-box-playground".into());

    println!("=== Playground Pipeline ===");
    println!("pipeline: {}", result.name);
    println!("stages: {}", result.stages.len());
    println!("success: {}", if result.success() { "YES" } else { "NO" });
    println!("total_cost: ${:.6}", result.total_cost_usd());
    println!(
        "total_tokens: {} in / {} out",
        result.total_input_tokens(),
        result.total_output_tokens()
    );
    println!("total_tool_calls: {}", result.total_tool_calls());
    println!();

    for (i, stage) in result.stages.iter().enumerate() {
        let r = &stage.claude_result;
        let status = if r.is_error { "FAILED" } else { "OK" };
        println!(
            "  Stage {}: {} [{}] -- {} in + {} out tokens, {} tools, ${:.4}",
            i + 1,
            stage.box_name,
            status,
            r.input_tokens,
            r.output_tokens,
            r.tool_calls.len(),
            r.total_cost_usd,
        );
    }

    println!();
    println!("traces captured: {}", observed.traces().len());
    println!("metrics captured: {}", observed.metrics().metrics.len());
    println!("logs captured: {}", observed.logs().len());

    println!();
    println!("=== Explore in Grafana ===");
    println!("Grafana: {}", grafana_base);
    println!("Service: {}", service_name);
    println!(
        "Traces URL: {}",
        grafana_trace_url(&grafana_base, &service_name, started_at_ms, ended_at_ms)
    );
    println!(
        "Metrics URL: {}",
        grafana_metrics_url(&grafana_base, started_at_ms, ended_at_ms)
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

fn grafana_trace_url(grafana_base: &str, service_name: &str, from_ms: u64, to_ms: u64) -> String {
    let query = format!(
        "{{ resource.service.name = \"{}\" && name =~ \"pipeline:.*\" }}",
        service_name
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

fn grafana_metrics_url(grafana_base: &str, from_ms: u64, to_ms: u64) -> String {
    let expr = "sum by (stage) (pipeline_stage_input_tokens_total)";
    let left = format!(
        "[{}, {}, \"prometheus\", {{\"refId\":\"A\",\"expr\":\"{}\"}}]",
        from_ms,
        to_ms.saturating_add(1000),
        escape_json_string(expr)
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
