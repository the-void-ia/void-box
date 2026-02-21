//! Example: Run a VoidBox with a local LM Studio model.
//!
//! This demonstrates using `LlmProvider::LmStudio` so that `claude-code` in
//! the guest VM talks to a local LM Studio instance instead of the Anthropic API.
//!
//! ## Prerequisites
//!
//! 1. Install LM Studio: https://lmstudio.ai
//! 2. Load a model in LM Studio and enable the Local Server (default port 1234).
//! 3. Ensure the server listens on all interfaces (`0.0.0.0:1234`), not just
//!    `127.0.0.1`, so the SLIRP gateway can reach it from the guest VM.
//! 4. Build the guest initramfs:
//!    ```
//!    CLAUDE_CODE_BIN=$(which claude) BUSYBOX=/usr/bin/busybox \
//!      scripts/build_guest_image.sh
//!    ```
//!
//! ## Run
//!
//! ```bash
//! LM_STUDIO_MODEL=qwen2.5-coder-7b-instruct \
//! VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
//! VOID_BOX_INITRAMFS=/tmp/void-box-rootfs.cpio.gz \
//! cargo run --example lm_studio_local
//! ```
//!
//! ## How it works
//!
//! The guest VM reaches LM Studio through SLIRP networking:
//!
//! ```text
//! Guest VM                          Host
//! ┌──────────────┐                 ┌────────────────┐
//! │ claude-code   │──SLIRP────────>│ LM Studio:1234 │
//! │ (stream-json) │  10.0.2.2      │ (localhost)     │
//! └──────────────┘                 └────────────────┘
//! ```
//!
//! The SLIRP gateway IP (10.0.2.2) is transparently mapped to 127.0.0.1
//! on the host, so `ANTHROPIC_BASE_URL=http://10.0.2.2:1234` reaches
//! the host's LM Studio process.

use std::path::PathBuf;

use void_box::agent_box::VoidBox;
use void_box::llm::LlmProvider;
use void_box::skill::Skill;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // -- Configuration --
    let model = std::env::var("LM_STUDIO_MODEL")
        .unwrap_or_else(|_| "qwen2.5-coder-7b-instruct".into());

    println!("=== void-box: LM Studio Local LLM Example ===");
    println!("Model: {}", model);
    println!();

    // -- Build the VoidBox --
    let mut builder = VoidBox::new("lm_studio_demo")
        .llm(LlmProvider::lm_studio(&model))
        .skill(Skill::agent("claude-code"))
        .memory_mb(2048)
        .prompt("Write a short Python script that prints the first 10 Fibonacci numbers. Save it to /workspace/fib.py");

    // Use KVM if kernel/initramfs are available, otherwise mock mode
    if let (Ok(kernel), Ok(initramfs)) = (
        std::env::var("VOID_BOX_KERNEL"),
        std::env::var("VOID_BOX_INITRAMFS"),
    ) {
        let kernel = PathBuf::from(&kernel);
        let initramfs = PathBuf::from(&initramfs);
        if !kernel.as_os_str().is_empty()
            && kernel.exists()
            && !initramfs.as_os_str().is_empty()
            && initramfs.exists()
        {
            println!("Mode: KVM (real VM)");
            println!("Kernel: {}", kernel.display());
            println!("Initramfs: {}", initramfs.display());
            builder = builder.kernel(kernel).initramfs(initramfs);
        } else {
            println!("Mode: Mock (KVM artifacts not found)");
            builder = builder.mock();
        }
    } else {
        println!("Mode: Mock (set VOID_BOX_KERNEL and VOID_BOX_INITRAMFS for KVM)");
        builder = builder.mock();
    }

    let agent_box = builder.build()?;

    println!("LLM Provider: {}", LlmProvider::lm_studio(&model));
    println!();

    // -- Run --
    println!("--- Running agent ---");
    let result = agent_box.run(None).await?;

    // -- Results --
    println!();
    println!("=== Results ===");
    println!("Box: {}", result.box_name);
    println!("Session: {}", result.claude_result.session_id);
    println!("Model: {}", result.claude_result.model);
    println!("Error: {}", result.claude_result.is_error);
    println!(
        "Tokens: {} in / {} out",
        result.claude_result.input_tokens, result.claude_result.output_tokens
    );
    println!("Cost: ${:.4}", result.claude_result.total_cost_usd);
    println!("Duration: {}ms", result.claude_result.duration_ms);
    println!("Tool calls: {}", result.claude_result.tool_calls.len());
    for tc in &result.claude_result.tool_calls {
        println!("  - {}", tc.tool_name);
    }
    println!();
    println!("Result text:");
    println!("{}", result.claude_result.result_text);

    if let Some(ref output) = result.file_output {
        println!();
        println!("File output ({} bytes):", output.len());
        println!("{}", String::from_utf8_lossy(output));
    }

    Ok(())
}
