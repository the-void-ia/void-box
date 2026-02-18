//! Code Review Agent: Two-Stage Pipeline with Remote Skills
//!
//! Demonstrates the full void-box value proposition:
//! - Fetch skills from the open ecosystem (skills.sh)
//! - Clone a real GitHub repo inside an isolated VM
//! - Analyze code and propose concrete improvements
//!
//! ## Pipeline
//!
//! ```text
//! ┌──────────────────────┐
//! │      analyzer         │
//! │  (clone + analyze)    │
//! │  network: enabled     │
//! │  skills: claude-code, │
//! │   systematic-debug,   │
//! │   tdd                 │
//! └──────────┬────────────┘
//!            │
//!            ▼
//! ┌──────────────────────┐
//! │      proposer         │
//! │  (write diffs)        │
//! │  network: disabled    │
//! │  skills: claude-code  │
//! └──────────────────────┘
//! ```
//!
//! ## Usage
//!
//! Mock mode (no KVM, no API key):
//!   cargo run --example code_review_agent
//!
//! KVM + Anthropic (real isolation + real LLM):
//!   ```
//!   scripts/build_guest_image.sh
//!   ANTHROPIC_API_KEY=sk-ant-xxx \
//!   VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
//!   VOID_BOX_INITRAMFS=/tmp/void-box-rootfs.cpio.gz \
//!   cargo run --example code_review_agent
//!   ```
//!
//! Custom repo (default: void-box itself):
//!   REVIEW_REPO=https://github.com/user/repo \
//!   cargo run --example code_review_agent

#[path = "common/mod.rs"]
mod common;

use std::error::Error;

use void_box::pipeline::Pipeline;
use void_box::skill::Skill;

use common::{detect_llm_provider, is_kvm_available, make_box};

const DEFAULT_REPO: &str = "https://github.com/the-void-ia/void-box";

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
    println!("║   Code Review Agent: Remote Skills + Isolated Pipeline      ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
    println!();

    // ---- Configuration ----

    let repo_url = std::env::var("REVIEW_REPO").unwrap_or_else(|_| DEFAULT_REPO.to_string());
    let llm = detect_llm_provider();
    let use_kvm = is_kvm_available();

    println!("[repo] {}", repo_url);
    println!("[llm]  {}", llm);
    if use_kvm {
        println!("[mode] KVM -- each Box is a real micro-VM");
    } else {
        println!("[mode] Mock -- simulating (set VOID_BOX_KERNEL for real VMs)");
    }
    println!();

    // ---- Skills ----

    let reasoning =
        Skill::agent("claude-code").description("Autonomous reasoning and code execution");

    let debugging = Skill::remote("obra/superpowers/systematic-debugging")
        .description("Systematic debugging methodology from skills.sh");

    let tdd = Skill::remote("obra/superpowers/test-driven-development")
        .description("Test-driven development methodology from skills.sh");

    // ---- Stage 1: Analyzer ----
    //
    // Clones the repo, reads the code, identifies improvements.
    // Network ENABLED (needs git clone).

    let analyzer = make_box("analyzer", use_kvm, &llm)
        .memory_mb(2048)
        .network(true)
        .timeout_secs(600)
        .skill(reasoning.clone())
        .skill(debugging)
        .skill(tdd)
        .prompt(format!(
            "You are a senior code reviewer with expertise in systematic debugging and TDD.\n\n\
             Clone the repository: {}\n\
             Read the code structure and key files.\n\n\
             Identify exactly 3 concrete improvements. For each one provide:\n\
             - Category: one of BUG, MISSING_TEST, CODE_QUALITY\n\
             - File path and line number(s)\n\
             - Description of the issue\n\
             - Suggested fix (high-level)\n\n\
             Output a structured analysis in this format:\n\n\
             ## Analysis\n\n\
             ### 1. [CATEGORY] Title\n\
             **File:** path/to/file.rs:42\n\
             **Issue:** Description of what's wrong.\n\
             **Fix:** How to fix it.\n\n\
             (repeat for all 3)",
            repo_url
        ))
        .build()?;

    println!(
        "[box] {} -- {} skill(s), network=true, 2048MB",
        analyzer.name,
        analyzer.skills.len()
    );

    // ---- Stage 2: Proposer ----
    //
    // Reads the analysis, writes concrete diffs.
    // Network DISABLED (no external access needed).

    let proposer = make_box("proposer", use_kvm, &llm)
        .memory_mb(1024)
        .timeout_secs(300)
        .skill(reasoning)
        .prompt(
            "You are a senior developer. Read the code review analysis from /workspace/input.json.\n\n\
             For each identified improvement, write a concrete code change as a unified diff.\n\n\
             Output a markdown report in this format:\n\n\
             ## Proposed Changes\n\n\
             ### 1. Title (from analysis)\n\n\
             **Rationale:** Brief explanation of why this change improves the code.\n\n\
             ```diff\n\
             --- a/path/to/file.rs\n\
             +++ b/path/to/file.rs\n\
             @@ -line,count +line,count @@\n\
              context\n\
             -old line\n\
             +new line\n\
              context\n\
             ```\n\n\
             (repeat for all improvements)\n\n\
             ## Summary\n\n\
             A brief paragraph summarizing all changes and their combined impact.",
        )
        .build()?;

    println!(
        "[box] {} -- {} skill(s), network=false, 1024MB",
        proposer.name,
        proposer.skills.len()
    );
    println!();

    // ---- Pipeline: analyzer -> proposer ----

    println!("--- Running Pipeline ---");
    println!();
    println!("  analyzer (clone + analyze) -> proposer (write diffs)");
    println!();

    let result = Pipeline::named("code_review", analyzer)
        .pipe(proposer)
        .run()
        .await?;

    // ---- Report ----

    println!();
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║                      Pipeline Report                        ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
    println!();
    println!("  Pipeline:  {}", result.name);
    println!("  Repo:      {}", repo_url);
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
    }

    println!();
    println!("--- Final Output ---");
    println!();
    if result.output.len() > 2000 {
        println!("{}", &result.output[..2000]);
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
