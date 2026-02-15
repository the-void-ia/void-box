//! Parallel Trading Pipeline: Fan-Out / Fan-In
//!
//! Demonstrates the `fan_out` API: multiple Boxes run in parallel on the same
//! input, and their outputs are merged as a JSON array for the next stage.
//!
//! ## Pipeline (diamond topology)
//!
//! ```text
//! ┌─────────────┐
//! │ Data Analyst │
//! │  (collect)   │
//! └──────┬───────┘
//!        │
//!   ┌────┴────┐        fan_out: quant + sentiment run in parallel
//!   │         │
//!   ▼         ▼
//! ┌──────┐  ┌───────────┐
//! │Quant │  │ Sentiment  │
//! │Analyst│  │ Analyst    │
//! └──┬───┘  └────┬──────┘
//!    │           │
//!    └─────┬─────┘        outputs merged as JSON array
//!          │
//!          ▼
//! ┌─────────────────────┐
//! │ Portfolio Strategist │
//! │  (final synthesis)   │
//! └─────────────────────┘
//! ```
//!
//! ## Usage
//!
//! Mock mode (no KVM, no API key):
//!   cargo run --example parallel_pipeline
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
//!      cargo run --example parallel_pipeline
//!      ```
//!   3. Or run with Ollama (local LLM, same model for all boxes):
//!      ```
//!      ollama pull phi4-mini
//!      OLLAMA_MODEL=phi4-mini \
//!      VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
//!      VOID_BOX_INITRAMFS=/tmp/void-box-rootfs.cpio.gz \
//!      cargo run --example parallel_pipeline
//!      ```
//!   4. Or run with different Ollama models per box:
//!      ```
//!      ollama pull qwen3-coder && ollama pull phi4-mini && ollama pull gemma3
//!      OLLAMA_MODEL=phi4-mini \
//!      OLLAMA_MODEL_QUANT=qwen3-coder \
//!      OLLAMA_MODEL_SENTIMENT=phi4-mini \
//!      OLLAMA_MODEL_STRATEGY=gemma3 \
//!      VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
//!      VOID_BOX_INITRAMFS=/tmp/void-box-rootfs.cpio.gz \
//!      cargo run --example parallel_pipeline
//!      ```

#[path = "common/mod.rs"]
mod common;

use std::error::Error;

use void_box::llm::LlmProvider;
use void_box::pipeline::Pipeline;
use void_box::skill::Skill;

use common::{detect_llm_provider, is_kvm_available, make_box};

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
    println!("║   Parallel Trading Pipeline: Fan-Out / Fan-In               ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
    println!();

    // ---- LLM Providers: each Box can use a different model ----
    //
    // Default: OLLAMA_MODEL (or Claude if unset)
    // Per-box overrides: OLLAMA_MODEL_QUANT, OLLAMA_MODEL_SENTIMENT, OLLAMA_MODEL_STRATEGY

    let default_llm = detect_llm_provider();
    println!("[llm] default: {}", default_llm);

    let llm_for = |env_suffix: &str| -> LlmProvider {
        let var = format!("OLLAMA_MODEL_{}", env_suffix);
        if let Ok(model) = std::env::var(&var) {
            if !model.is_empty() {
                println!("[llm] {} -> Ollama ({})", env_suffix.to_lowercase(), model);
                return LlmProvider::ollama(model);
            }
        }
        default_llm.clone()
    };

    let quant_llm = llm_for("QUANT");
    let sentiment_llm = llm_for("SENTIMENT");
    let strategy_llm = llm_for("STRATEGY");

    // ---- Skills ----

    let reasoning =
        Skill::agent("claude-code").description("Autonomous reasoning and code execution");

    let quant_skill = Skill::file("examples/trading_pipeline/skills/quant-technical-analysis.md")
        .description("Technical indicator computation and signal generation");

    // ---- Environment ----

    let use_kvm = is_kvm_available();
    if use_kvm {
        println!("[mode] KVM -- each Box is a real KVM micro-VM");
    } else {
        println!("[mode] Mock -- simulating pipeline (set VOID_BOX_KERNEL for real VMs)");
    }
    println!();

    // ---- Box 1: Data Analyst (sequential, uses default LLM) ----

    let data_box = make_box("data_analyst", use_kvm, &default_llm)
        .skill(reasoning.clone())
        .prompt(
            "You are a financial data analyst. Here is recent market data (Feb 2026):\n\n\
             AAPL: price $227, P/E 34, RSI 62, 52w range $170-$243\n\
             NVDA: price $138, P/E 55, RSI 71, 52w range $78-$153\n\
             MSFT: price $442, P/E 36, RSI 58, 52w range $385-$470\n\
             GOOGL: price $192, P/E 24, RSI 55, 52w range $152-$207\n\n\
             Write a brief data summary for each symbol with key metrics.",
        )
        .build()?;

    println!("  [1]  {} (sequential) -- {}", data_box.name, default_llm);

    // ---- Box 2a: Quant Analyst (parallel leg A, can use different model) ----

    let quant_box = make_box("quant_analyst", use_kvm, &quant_llm)
        .skill(quant_skill)
        .skill(reasoning.clone())
        .prompt(
            "You are a quantitative analyst. Read data from /workspace/input.json.\n\
             For each symbol: interpret RSI, compare P/E to sector avg (~28),\n\
             and give a composite signal: BULLISH, NEUTRAL, or BEARISH.",
        )
        .build()?;

    println!("  [2a] {} (parallel) -- {}", quant_box.name, quant_llm);

    // ---- Box 2b: Sentiment Analyst (parallel leg B, can use different model) ----

    let sentiment_box = make_box("sentiment_analyst", use_kvm, &sentiment_llm)
        .skill(reasoning.clone())
        .prompt(
            "You are a sentiment analyst. Read data from /workspace/input.json.\n\
             For each symbol: score sentiment from -1.0 (bearish) to +1.0 (bullish)\n\
             with a 2-sentence explanation.",
        )
        .build()?;

    println!(
        "  [2b] {} (parallel) -- {}",
        sentiment_box.name, sentiment_llm
    );

    // ---- Box 3: Portfolio Strategist (sequential, can use different model) ----

    let strategy_box = make_box("portfolio_strategist", use_kvm, &strategy_llm)
        .skill(reasoning.clone())
        .prompt(
            "You are a portfolio strategist managing $100,000.\n\
             Read /workspace/input.json which contains a JSON array with two analyses:\n\
             [0] = quantitative signals, [1] = sentiment scores.\n\n\
             Synthesize both into trade recommendations per symbol:\n\
             ACTION, ALLOCATION %, ENTRY PRICE, STOP LOSS, RATIONALE.\n\
             Keep at least 20% in cash.",
        )
        .build()?;

    println!(
        "  [3]  {} (sequential) -- {}",
        strategy_box.name, strategy_llm
    );

    // ---- Compose: sequential -> fan_out -> sequential ----

    println!();
    println!("--- Running Pipeline ---");
    println!();
    println!("  data_analyst -> [quant_analyst | sentiment_analyst] -> portfolio_strategist");
    println!();

    let result = Pipeline::named("parallel_trading", data_box)
        .fan_out(vec![quant_box, sentiment_box]) // parallel: both get data_box output
        .pipe(strategy_box) // sequential: gets merged JSON array
        .run_streaming(|box_name, chunk| {
            // Show real-time output from each agent so users can see WTF is happening
            let text = String::from_utf8_lossy(&chunk.data);
            for line in text.lines() {
                println!("  [{}/{}] {}", box_name, chunk.stream, line);
            }
        })
        .await?;

    // ---- Report ----

    println!();
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║                      Pipeline Report                        ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
    println!();
    println!("  Pipeline:       {}", result.name);
    println!("  Stages:         {}", result.stages.len());
    println!(
        "  Success:        {}",
        if result.success() { "YES" } else { "NO" }
    );
    println!("  Total cost:     ${:.6}", result.total_cost_usd());
    println!(
        "  Total tokens:   {} in / {} out",
        result.total_input_tokens(),
        result.total_output_tokens()
    );
    println!();

    for (i, stage) in result.stages.iter().enumerate() {
        let r = &stage.claude_result;
        let status = if r.is_error { "FAILED" } else { "OK" };
        println!(
            "  Stage {}: {} [{}] -- {} tokens, ${:.4}",
            i + 1,
            stage.box_name,
            status,
            r.input_tokens + r.output_tokens,
            r.total_cost_usd,
        );
    }

    println!();
    println!("--- Final Output ---");
    println!();
    if result.output.len() > 500 {
        println!("{}", &result.output[..500]);
        println!("... ({} chars total)", result.output.len());
    } else if result.output.is_empty() {
        println!("(no text output)");
    } else {
        println!("{}", result.output);
    }

    println!();
    println!("Done.");
    Ok(())
}
