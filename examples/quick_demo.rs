//! Quick Demo: Agent(Skills) + Isolation = VoidBox
//!
//! A minimal example showing the core void-box abstraction:
//! each Box is an isolated KVM micro-VM with declared skills and a prompt.
//! Boxes compose into pipelines where output flows from one to the next.
//!
//! ## Mock mode (no KVM, no API key):
//!   cargo run --example quick_demo
//!
//! ## KVM + Ollama (real micro-VMs + local LLM):
//!   1. Install Ollama and pull a model: `ollama pull phi4-mini`
//!   2. Build the guest initramfs:
//!      ```
//!      CLAUDE_CODE_BIN=$(which claude) BUSYBOX=/usr/bin/busybox \
//!        scripts/build_guest_image.sh
//!      ```
//!   3. Run:
//!      ```
//!      OLLAMA_MODEL=phi4-mini \
//!      VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
//!      VOID_BOX_INITRAMFS=/tmp/void-box-rootfs.cpio.gz \
//!      cargo run --example quick_demo
//!      ```

use std::error::Error;
use std::path::PathBuf;

use void_box::agent_box::VoidBox;
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

    println!("╔══════════════════════════════════════════════════╗");
    println!("║   Quick Demo: Agent(Skills) + Isolation = VoidBox║");
    println!("╚══════════════════════════════════════════════════╝");
    println!();

    // ---- LLM Provider ----

    let llm = detect_llm_provider();
    println!("[llm] {}", llm);

    // ---- Detect environment ----

    let use_kvm = is_kvm_available();
    if use_kvm {
        println!("[mode] KVM -- each Box is a real micro-VM");
    } else {
        println!("[mode] Mock -- simulating (set VOID_BOX_KERNEL for real VMs)");
    }
    println!();

    // ---- Skills: declared capabilities ----
    //
    // A Skill is what a Box *can do*. The agent skill is the reasoning
    // engine itself; other skills (MCP, CLI, file) provide domain tools.

    let reasoning =
        Skill::agent("claude-code").description("Autonomous reasoning and code execution");

    // ---- Box 1: Analyst ----
    //
    // Agent(Skills) + Isolation = VoidBox.
    // This Box has reasoning capability and a focused prompt.

    let analyst = make_box("analyst", use_kvm, &llm)
        .skill(reasoning.clone())
        .prompt(
            "You are a stock analyst. AAPL is trading at $227 after Q1 2026 earnings beat expectations \
             (EPS $2.40 vs $2.36 est.) but Services revenue missed at $23.1B vs $23.5B est. \
             iPhone revenue grew 4% YoY. The stock has a P/E of 34 and RSI of 62.\n\n\
             List exactly 3 bullish and 3 bearish signals based on these facts.\n\
             Write one concrete sentence per signal referencing the actual numbers.\n\
             Do NOT output JSON. Do NOT output templates. Write plain text only.\n\n\
             Format:\n\
             BULLISH:\n\
             1. ...\n\
             2. ...\n\
             3. ...\n\
             BEARISH:\n\
             1. ...\n\
             2. ...\n\
             3. ..."
        )
        .build()?;

    println!(
        "[box] {} -- {} skill(s)",
        analyst.name,
        analyst.skills.len()
    );

    // ---- Box 2: Strategist ----
    //
    // Pure reasoning: read the analysis, produce a recommendation.

    let strategist = make_box("strategist", use_kvm, &llm)
        .skill(reasoning.clone())
        .prompt(
            "You are a senior investment strategist. The file /workspace/input.json \
             contains a stock analyst's bullish and bearish signals for AAPL.\n\n\
             Read the file, weigh the signals, and produce a SINGLE verdict.\n\n\
             Your output MUST be exactly this format (fill in real values, not placeholders):\n\
             VERDICT: <BUY or SELL or HOLD>\n\
             CONFIDENCE: <high or medium or low>\n\
             RATIONALE: <one paragraph of 3-5 sentences explaining your reasoning, \
             referencing specific signals from the analyst's report>\n\n\
             Do NOT output JSON. Do NOT output templates. Write real analysis.",
        )
        .build()?;

    println!(
        "[box] {} -- {} skill(s)",
        strategist.name,
        strategist.skills.len()
    );
    println!();

    // ---- Pipeline: analyst -> strategist ----
    //
    // Each Box boots a fresh VM, runs, and its output flows to the next.
    // No state leaks between stages.

    println!("--- Running Pipeline ---");
    println!();

    let result = Pipeline::named("quick_demo", analyst)
        .pipe(strategist)
        .run()
        .await?;

    // ---- Report ----

    println!();
    println!("╔══════════════════════════════════════════════════╗");
    println!("║                  Pipeline Report                 ║");
    println!("╚══════════════════════════════════════════════════╝");
    println!();
    println!("  Pipeline:  {}", result.name);
    println!("  Stages:    {}", result.stages.len());
    println!(
        "  Success:   {}",
        if result.success() { "YES" } else { "NO" }
    );
    println!("  Cost:      ${:.4}", result.total_cost_usd());
    println!(
        "  Tokens:    {} in / {} out",
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
    if result.output.is_empty() {
        println!("(no text output)");
    } else {
        println!("{}", result.output);
    }

    println!();
    println!("Done.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers (shared pattern with other examples)
// ---------------------------------------------------------------------------

/// Create an VoidBox builder pre-configured for the current environment.
fn make_box(name: &str, use_kvm: bool, llm: &LlmProvider) -> VoidBox {
    let mut ab = VoidBox::new(name).llm(llm.clone()).memory_mb(1024);

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
fn detect_llm_provider() -> LlmProvider {
    if let Ok(model) = std::env::var("OLLAMA_MODEL") {
        if !model.is_empty() {
            return LlmProvider::ollama(model);
        }
    }
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
    LlmProvider::Claude
}

fn is_kvm_available() -> bool {
    std::path::Path::new("/dev/kvm").exists()
        && std::env::var("VOID_BOX_KERNEL")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
}

fn kvm_kernel() -> Option<PathBuf> {
    std::env::var_os("VOID_BOX_KERNEL").map(PathBuf::from)
}

fn kvm_initramfs() -> Option<PathBuf> {
    std::env::var_os("VOID_BOX_INITRAMFS").map(PathBuf::from)
}
