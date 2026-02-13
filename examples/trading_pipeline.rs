//! Trading Analysis Pipeline: Skill + Environment = Box
//!
//! Demonstrates the void-box "Box" abstraction inspired by Ed Huang's article
//! (https://me.0xffff.me/agent_infra.html) where each Box is an autonomous
//! Claude agent with domain-specific skills running in an isolated KVM micro-VM.
//!
//! ## Pipeline
//!
//! ```text
//! ┌─────────────────┐    ┌──────────────────┐    ┌───────────────────┐    ┌─────────────────────┐
//! │  Data Analyst    │───>│  Quant Analyst    │───>│ Research Analyst  │───>│ Portfolio Strategist │
//! │  (MCP + Skill)   │    │  (Skill)          │    │  (Pure Reasoning) │    │  (Skill)             │
//! └─────────────────┘    └──────────────────┘    └───────────────────┘    └─────────────────────┘
//!    OHLCV + News    ───>   Tech Indicators   ───>   Sentiment Notes   ───>   Trade Recs
//! ```
//!
//! Each Box boots a fresh VM, provisions skills, runs the agent, and is destroyed.
//! No state leaks between stages. Pure, isolated, composable.
//!
//! ## Usage
//!
//! Mock mode (no KVM, no API key):
//!   cargo run --example trading_pipeline
//!
//! KVM mode (requires kernel + initramfs):
//!   VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
//!   VOID_BOX_INITRAMFS=/tmp/void-box-test-rootfs.cpio.gz \
//!   cargo run --example trading_pipeline

use std::error::Error;
use std::path::PathBuf;

use void_box::agent_box::AgentBox;
use void_box::pipeline::Pipeline;
use void_box::skill::Skill;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║     Trading Analysis Pipeline: Skill + Environment = Box    ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
    println!();

    // ---- Skills: declared capabilities ----

    let reasoning = Skill::agent("claude-code")
        .description("Autonomous reasoning and code execution");

    let data_skill = Skill::file("skills/financial-data-analysis.md")
        .description("Financial data collection and quality methodology");

    let quant_skill = Skill::file("skills/quant-technical-analysis.md")
        .description("Technical indicator computation and signal generation");

    let risk_skill = Skill::file("skills/portfolio-risk-management.md")
        .description("Portfolio risk management and position sizing");

    // ---- Detect environment: KVM or mock ----

    let use_kvm = is_kvm_available();

    if use_kvm {
        println!("[mode] KVM -- each Box is a real KVM micro-VM");
    } else {
        println!("[mode] Mock -- simulating pipeline (set VOID_BOX_KERNEL for real VMs)");
    }
    println!();

    // ---- Box 1: Data Analyst ----

    println!("--- Defining Boxes ---");
    println!();

    let data_box = make_box("data_analyst", use_kvm)
        .skill(data_skill)
        .skill(reasoning.clone())
        .prompt(
            "You are a financial data analyst. Generate realistic 30-day OHLCV data \
             for AAPL, NVDA, MSFT, and GOOGL. Include mock news headlines for each symbol. \
             Write a Python script to generate the data and run it. \
             Follow the schema from your financial-data-analysis skill."
        )
        .build()?;

    println!("  [1] {} -- {} skills", data_box.name, data_box.skills.len());

    // ---- Box 2: Quant Analyst ----

    let quant_box = make_box("quant_analyst", use_kvm)
        .skill(quant_skill)
        .skill(reasoning.clone())
        .prompt(
            "You are a quantitative analyst. Read the market data from /workspace/input.json. \
             Compute technical indicators (SMA, RSI, MACD, Bollinger Bands) for each symbol. \
             Generate composite trading signals. \
             Follow the methodology from your quant-technical-analysis skill."
        )
        .build()?;

    println!("  [2] {} -- {} skills", quant_box.name, quant_box.skills.len());

    // ---- Box 3: Research Analyst (pure reasoning, no special skills) ----

    let sentiment_box = make_box("research_analyst", use_kvm)
        .skill(reasoning.clone())
        .prompt(
            "You are a research analyst. Read the technical signals from /workspace/input.json. \
             For each symbol, assess the market sentiment considering the technical indicators, \
             recent price action, and any news context. Score sentiment from -1.0 (very bearish) \
             to +1.0 (very bullish). Provide brief reasoning for each score."
        )
        .build()?;

    println!("  [3] {} -- {} skills (pure reasoning)", sentiment_box.name, sentiment_box.skills.len());

    // ---- Box 4: Portfolio Strategist ----

    let strategy_box = make_box("portfolio_strategist", use_kvm)
        .skill(risk_skill)
        .skill(reasoning.clone())
        .memory_mb(512)
        .prompt(
            "You are a portfolio strategist. Read the analysis from /workspace/input.json \
             which contains technical signals and sentiment scores. \
             Generate specific trade recommendations with position sizing, entry/exit prices, \
             stop loss levels, and risk management. \
             Follow the framework from your portfolio-risk-management skill."
        )
        .build()?;

    println!("  [4] {} -- {} skills", strategy_box.name, strategy_box.skills.len());

    // ---- Compose the pipeline ----

    println!();
    println!("--- Running Pipeline ---");
    println!();

    let result = Pipeline::named("trading_analysis", data_box)
        .pipe(quant_box)
        .pipe(sentiment_box)
        .pipe(strategy_box)
        .run()
        .await?;

    // ---- Report ----

    println!();
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║                      Pipeline Report                        ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
    println!();
    println!("  Pipeline:       {}", result.name);
    println!("  Stages:         {}", result.stages.len());
    println!("  Success:        {}", if result.success() { "YES" } else { "NO" });
    println!("  Total cost:     ${:.6}", result.total_cost_usd());
    println!("  Total tokens:   {} in / {} out",
        result.total_input_tokens(), result.total_output_tokens());
    println!("  Total tools:    {}", result.total_tool_calls());
    println!();

    for (i, stage) in result.stages.iter().enumerate() {
        let r = &stage.claude_result;
        let status = if r.is_error { "FAILED" } else { "OK" };
        println!(
            "  Stage {}: {} [{}] -- {} tokens, {} tools, ${:.4}",
            i + 1,
            stage.box_name,
            status,
            r.input_tokens + r.output_tokens,
            r.tool_calls.len(),
            r.total_cost_usd,
        );
        if !r.result_text.is_empty() {
            let preview = if r.result_text.len() > 120 {
                format!("{}...", &r.result_text[..120])
            } else {
                r.result_text.clone()
            };
            println!("         -> {}", preview);
        }
    }

    println!();
    println!("--- Final Output ---");
    println!();
    if result.output.len() > 500 {
        println!("{}", &result.output[..500]);
        println!("... ({} chars total)", result.output.len());
    } else if result.output.is_empty() {
        println!("(no text output -- check file outputs)");
    } else {
        println!("{}", result.output);
    }

    println!();
    println!("Done.");
    Ok(())
}

/// Create an AgentBox builder pre-configured for the current environment.
fn make_box(name: &str, use_kvm: bool) -> AgentBox {
    let mut ab = AgentBox::new(name);

    if use_kvm {
        if let Some(kernel) = kvm_kernel() {
            ab = ab.kernel(kernel);
        }
        if let Some(initramfs) = kvm_initramfs() {
            ab = ab.initramfs(initramfs);
        }
    } else {
        ab = ab.mock();
    }

    ab
}

/// Check if KVM artifacts are available.
fn is_kvm_available() -> bool {
    std::path::Path::new("/dev/kvm").exists()
        && std::env::var("VOID_BOX_KERNEL").map(|v| !v.is_empty()).unwrap_or(false)
}

/// Get the kernel path from environment.
fn kvm_kernel() -> Option<PathBuf> {
    std::env::var_os("VOID_BOX_KERNEL").map(PathBuf::from)
}

/// Get the initramfs path from environment.
fn kvm_initramfs() -> Option<PathBuf> {
    std::env::var_os("VOID_BOX_INITRAMFS").map(PathBuf::from)
}
