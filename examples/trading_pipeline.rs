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
//!   1. Build the guest initramfs:
//!      ```
//!      CLAUDE_CODE_BIN=$(which claude) BUSYBOX=/usr/bin/busybox \
//!        scripts/build_guest_image.sh
//!      ```
//!   2. Run with Anthropic API:
//!      ```
//!      ANTHROPIC_API_KEY=sk-ant-xxx \
//!      VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
//!      VOID_BOX_INITRAMFS=/tmp/void-box-rootfs.cpio.gz \
//!      cargo run --example trading_pipeline
//!      ```
//!   3. Or run with Ollama (local LLM):
//!      ```
//!      ollama pull phi4-mini
//!      OLLAMA_MODEL=phi4-mini \
//!      VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
//!      VOID_BOX_INITRAMFS=/tmp/void-box-rootfs.cpio.gz \
//!      cargo run --example trading_pipeline
//!      ```

use std::error::Error;
use std::path::PathBuf;

use void_box::agent_box::AgentBox;
use void_box::llm::LlmProvider;
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

    // ---- LLM Provider: Claude (default) or Ollama (opt-in) ----

    let llm = detect_llm_provider();
    println!("[llm] {}", llm);

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

    let data_box = make_box("data_analyst", use_kvm, &llm)
        .skill(data_skill)
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
             Write plain text with clear sections per symbol."
        )
        .build()?;

    println!("  [1] {} -- {} skills", data_box.name, data_box.skills.len());

    // ---- Box 2: Quant Analyst ----

    let quant_box = make_box("quant_analyst", use_kvm, &llm)
        .skill(quant_skill)
        .skill(reasoning.clone())
        .prompt(
            "You are a quantitative analyst. Read the data summary from /workspace/input.json.\n\n\
             For each symbol (AAPL, NVDA, MSFT, GOOGL), provide:\n\
             - RSI interpretation (overbought >70, neutral 30-70, oversold <30)\n\
             - P/E relative to sector average (Tech sector avg ~28)\n\
             - A composite signal: BULLISH, NEUTRAL, or BEARISH\n\n\
             Write plain text. Do NOT output JSON or templates.\n\
             Use the actual numbers from the input data."
        )
        .build()?;

    println!("  [2] {} -- {} skills", quant_box.name, quant_box.skills.len());

    // ---- Box 3: Research Analyst (pure reasoning, no special skills) ----

    let sentiment_box = make_box("research_analyst", use_kvm, &llm)
        .skill(reasoning.clone())
        .prompt(
            "You are a research analyst. Read the quant analysis from /workspace/input.json.\n\n\
             For each symbol (AAPL, NVDA, MSFT, GOOGL):\n\
             - Score sentiment from -1.0 (very bearish) to +1.0 (very bullish)\n\
             - Write 2 sentences explaining your score\n\n\
             Consider the technical signals, fundamentals, and catalysts from the input.\n\
             Write plain text. Do NOT output JSON or templates.\n\
             Example: AAPL: +0.3 (mildly bullish). The earnings beat suggests..."
        )
        .build()?;

    println!("  [3] {} -- {} skills (pure reasoning)", sentiment_box.name, sentiment_box.skills.len());

    // ---- Box 4: Portfolio Strategist ----

    let strategy_box = make_box("portfolio_strategist", use_kvm, &llm)
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
             Use real numbers from the analysis, not placeholders."
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
            let preview = if r.result_text.len() > 500 {
                format!("{}...", &r.result_text[..500])
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
fn make_box(name: &str, use_kvm: bool, llm: &LlmProvider) -> AgentBox {
    let mut ab = AgentBox::new(name).llm(llm.clone()).memory_mb(1024);

    // Allow per-stage timeout override via STAGE_TIMEOUT_SECS env var
    if let Ok(secs) = std::env::var("STAGE_TIMEOUT_SECS") {
        if let Ok(s) = secs.parse::<u64>() {
            ab = ab.timeout_secs(s);
        }
    }

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

/// Detect the LLM provider from environment variables.
///
/// - `OLLAMA_MODEL=qwen3-coder` -> Ollama with that model
/// - `LLM_BASE_URL=...` -> Custom provider
/// - Otherwise -> Claude (default)
fn detect_llm_provider() -> LlmProvider {
    // Check for Ollama
    if let Ok(model) = std::env::var("OLLAMA_MODEL") {
        if !model.is_empty() {
            return LlmProvider::ollama(model);
        }
    }

    // Check for custom endpoint
    if let Ok(base_url) = std::env::var("LLM_BASE_URL") {
        if !base_url.is_empty() {
            let mut provider = LlmProvider::custom(base_url);
            if let Ok(key) = std::env::var("LLM_API_KEY") {
                provider = provider.api_key(key);
            }
            if let Ok(model) = std::env::var("LLM_MODEL") {
                provider = provider.model(model);
            }
            return provider;
        }
    }

    // Default: Claude
    LlmProvider::Claude
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
