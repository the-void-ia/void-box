//! Claude-in-voidbox example: run the real Claude Code CLI inside a KVM micro-VM sandbox.
//!
//! Demonstrates multi-turn interaction with `claude-code` executing inside the
//! guest VM, with SLIRP networking providing NAT-based API access. Uses
//! `--output-format stream-json` for structured telemetry extraction.
//!
//! Usage (mock, no KVM required):
//!   cargo run --example claude_in_voidbox_example
//!
//! Usage (real KVM + real Claude API):
//!   1. Build the guest initramfs:  scripts/build_guest_image.sh
//!   2. Run:
//!      ANTHROPIC_API_KEY=sk-ant-xxx \
//!      VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
//!      VOID_BOX_INITRAMFS=/tmp/void-box-rootfs.cpio.gz \
//!      cargo run --example claude_in_voidbox_example
//!
//! With OTel export (requires `--features opentelemetry`):
//!   VOIDBOX_OTLP_ENDPOINT=http://localhost:4317 \
//!   cargo run --features opentelemetry --example claude_in_voidbox_example
//!
//! The sandbox uses SLIRP networking (guest 10.0.2.15/24, gateway 10.0.2.2, DNS 10.0.2.3).

use std::error::Error;
use std::io::{self, Write};
use std::sync::Arc;

use void_box::observe::claude::{parse_stream_json, ClaudeExecResult};
use void_box::sandbox::Sandbox;

const WORKSPACE: &str = "/workspace";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Initialize tracing so we can see debug/error output from the VMM
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let sandbox = choose_sandbox()?;
    eprintln!("[claude-in-voidbox] Using sandbox (mock or KVM from env)");

    println!("Modes:");
    println!("  1. Demo (automated plan -> apply with telemetry)");
    println!("  2. Interactive (type prompts, see structured output)");
    print!("Select mode [1/2]: ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let choice = line.trim();

    if choice == "2" {
        interactive_session(sandbox.clone()).await?;
    } else {
        demo_multi_turn(sandbox.clone()).await?;
    }

    // Gracefully stop the sandbox VM
    eprintln!("[claude-in-voidbox] Stopping sandbox...");
    sandbox.stop().await?;
    eprintln!("[claude-in-voidbox] Sandbox stopped cleanly.");

    println!("\nDone.");
    Ok(())
}

fn choose_sandbox() -> Result<Arc<Sandbox>, Box<dyn Error>> {
    if let Some(sb) = try_kvm_sandbox()? {
        return Ok(sb);
    }
    let mut b = Sandbox::mock();
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        if !key.is_empty() {
            b = b.env("ANTHROPIC_API_KEY", key);
            eprintln!("[claude-in-voidbox] ANTHROPIC_API_KEY set in sandbox env (mock)");
        }
    }
    Ok(b.build()?)
}

fn try_kvm_sandbox() -> Result<Option<Arc<Sandbox>>, Box<dyn Error>> {
    use std::path::PathBuf;

    let kernel = match std::env::var_os("VOID_BOX_KERNEL") {
        Some(k) => PathBuf::from(k),
        None => return Ok(None),
    };
    if !kernel.exists() {
        return Ok(None);
    }
    let initramfs = std::env::var_os("VOID_BOX_INITRAMFS").map(PathBuf::from);
    if let Some(ref p) = initramfs {
        if !p.exists() {
            return Ok(None);
        }
    }

    // Check for ANTHROPIC_API_KEY to pass to the guest
    let api_key = std::env::var("ANTHROPIC_API_KEY").ok();
    if api_key.is_some() {
        eprintln!("[claude-in-voidbox] ANTHROPIC_API_KEY detected, will pass to guest");
    }

    let mut b = Sandbox::local()
        .memory_mb(512)
        .vcpus(1)
        .kernel(&kernel)
        .network(true); // Enable SLIRP networking for API access

    if let Some(ref p) = initramfs {
        b = b.initramfs(p);
    }

    // Pass API key to sandbox environment if set and non-empty
    if let Some(ref key) = api_key {
        if !key.is_empty() {
            b = b.env("ANTHROPIC_API_KEY", key);
        }
    }

    match b.build() {
        Ok(sb) => {
            if api_key.is_some() {
                eprintln!("[claude-in-voidbox] KVM sandbox with SLIRP networking enabled");
            }
            Ok(Some(sb))
        }
        Err(_) => Ok(None),
    }
}

/// Run claude-code with stream-json output and parse the result.
async fn run_claude(sandbox: &Sandbox, prompt: &str) -> Result<ClaudeExecResult, Box<dyn Error>> {
    let out = sandbox
        .exec(
            "claude-code",
            &[
                "-p",
                prompt,
                "--output-format",
                "stream-json",
                "--dangerously-skip-permissions",
            ],
        )
        .await?;

    if !out.stderr.is_empty() {
        eprintln!("  [stderr] {}", out.stderr_str().trim_end());
    }

    let result = parse_stream_json(&out.stdout);
    Ok(result)
}

/// Print a structured telemetry summary from a ClaudeExecResult.
fn print_telemetry(label: &str, result: &ClaudeExecResult) {
    println!("\n--- {} Telemetry ---", label);
    println!("  Session:     {}", result.session_id);
    println!("  Model:       {}", result.model);
    println!("  Turns:       {}", result.num_turns);
    println!(
        "  Tokens:      {} in / {} out",
        result.input_tokens, result.output_tokens
    );
    println!("  Cost:        ${:.6}", result.total_cost_usd);
    println!(
        "  Duration:    {}ms (API: {}ms)",
        result.duration_ms, result.duration_api_ms
    );
    println!(
        "  Error:       {}",
        if result.is_error { "YES" } else { "no" }
    );

    if !result.tool_calls.is_empty() {
        println!("  Tool calls:  {}", result.tool_calls.len());
        for (i, tc) in result.tool_calls.iter().enumerate() {
            let output_preview = tc.output.as_deref().unwrap_or("(none)");
            let output_short = if output_preview.len() > 60 {
                format!("{}...", &output_preview[..60])
            } else {
                output_preview.to_string()
            };
            println!(
                "    [{}] {} (id={}) -> {}",
                i + 1,
                tc.tool_name,
                tc.tool_use_id,
                output_short
            );
        }
    }

    if let Some(ref err) = result.error {
        println!("  Error msg:   {}", err);
    }

    // Print the result text (truncated)
    if !result.result_text.is_empty() {
        let text = if result.result_text.len() > 200 {
            format!("{}...", &result.result_text[..200])
        } else {
            result.result_text.clone()
        };
        println!("  Result:      {}", text);
    }
    println!("---");
}

/// Optionally create OTel spans from the result.
fn maybe_create_otel_spans(result: &ClaudeExecResult) {
    // Check if OTLP endpoint is configured
    let otlp_configured = std::env::var("VOIDBOX_OTLP_ENDPOINT").is_ok()
        || std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_ok();

    if otlp_configured {
        let tracer = void_box::observe::tracer::Tracer::new(
            void_box::observe::tracer::TracerConfig::in_memory(),
        );
        void_box::observe::claude::create_otel_spans(result, None, &tracer);
        eprintln!(
            "  [otel] Created {} spans (claude.exec + {} tool spans)",
            1 + result.tool_calls.len(),
            result.tool_calls.len(),
        );
    }
}

/// Automated demo: run two turns with the real Claude CLI using stream-json.
///
/// Turn 1: Ask Claude to create a plan.
/// Turn 2: Feed the plan back and ask Claude to apply it.
async fn demo_multi_turn(sandbox: Arc<Sandbox>) -> Result<(), Box<dyn Error>> {
    println!("\n=== Multi-turn Demo (stream-json) ===\n");

    // Step 0: Network diagnostic
    println!("Step 0: Network diagnostic");
    let diag = sandbox.exec("sh", &["-c",
        "echo '--- interfaces ---'; ip addr 2>&1; echo '--- routes ---'; ip route 2>&1; echo '--- dns ---'; cat /etc/resolv.conf 2>&1; echo '--- dns test ---'; /bin/busybox nslookup api.anthropic.com 10.0.2.3 2>&1; echo dns_exit=$?"
    ]).await?;
    display_output("Network", &diag);

    // Turn 1: ask Claude to create a plan
    let plan_prompt = format!(
        "Create a simple plan to add a hello-world Python script in {}. \
         Output only the plan as a numbered list, nothing else.",
        WORKSPACE
    );
    println!("Turn 1: plan\n  prompt: {}\n", plan_prompt);

    let plan_result = run_claude(&sandbox, &plan_prompt).await?;
    print_telemetry("Plan", &plan_result);
    maybe_create_otel_spans(&plan_result);

    if plan_result.is_error {
        println!("\nPlan step failed: {:?}", plan_result.error);
        return Ok(());
    }

    // Turn 2: ask Claude to apply the plan
    let apply_prompt = format!(
        "Apply the following plan in {}. Execute each step.\n\n{}",
        WORKSPACE, plan_result.result_text,
    );
    println!("\nTurn 2: apply\n  prompt: {} bytes\n", apply_prompt.len());

    let apply_result = run_claude(&sandbox, &apply_prompt).await?;
    print_telemetry("Apply", &apply_result);
    maybe_create_otel_spans(&apply_result);

    // Summary
    let total_cost = plan_result.total_cost_usd + apply_result.total_cost_usd;
    let total_tokens_in = plan_result.input_tokens + apply_result.input_tokens;
    let total_tokens_out = plan_result.output_tokens + apply_result.output_tokens;
    let total_tools = plan_result.tool_calls.len() + apply_result.tool_calls.len();

    println!("\n=== Session Summary ===");
    println!("  Total cost:   ${:.6}", total_cost);
    println!(
        "  Total tokens: {} in / {} out",
        total_tokens_in, total_tokens_out
    );
    println!("  Total tools:  {}", total_tools);

    if !apply_result.is_error {
        println!("\n  Demo completed successfully.");
    } else {
        println!("\n  Apply step did not succeed: {:?}", apply_result.error);
    }
    Ok(())
}

/// Interactive: type a prompt, Claude responds with stream-json telemetry.
async fn interactive_session(sandbox: Arc<Sandbox>) -> Result<(), Box<dyn Error>> {
    println!("\n=== Interactive Session (stream-json) ===");
    println!("Type a prompt for Claude (or 'quit' to exit).\n");

    let mut total_cost = 0.0_f64;
    let mut total_tokens_in = 0_u64;
    let mut total_tokens_out = 0_u64;
    let mut turn_count = 0_u32;

    loop {
        print!("You: ");
        io::stdout().flush()?;
        let mut line = String::new();
        if io::stdin().read_line(&mut line)? == 0 {
            println!("\n[EOF]");
            break;
        }
        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        if input.eq_ignore_ascii_case("quit") || input.eq_ignore_ascii_case("exit") || input == "q"
        {
            break;
        }

        turn_count += 1;
        let result = run_claude(&sandbox, input).await?;

        // Show the text result
        if !result.result_text.is_empty() {
            println!("Claude: {}", result.result_text);
        } else if result.is_error {
            println!("Claude [ERROR]: {:?}", result.error);
        }

        // Show telemetry
        print_telemetry(&format!("Turn {}", turn_count), &result);
        maybe_create_otel_spans(&result);

        // Accumulate session totals
        total_cost += result.total_cost_usd;
        total_tokens_in += result.input_tokens;
        total_tokens_out += result.output_tokens;
    }

    if turn_count > 0 {
        println!("\n=== Session Summary ({} turns) ===", turn_count);
        println!("  Total cost:   ${:.6}", total_cost);
        println!(
            "  Total tokens: {} in / {} out",
            total_tokens_in, total_tokens_out
        );
    }

    Ok(())
}

fn display_output(label: &str, o: &void_box::ExecOutput) {
    if !o.stdout.is_empty() {
        println!("{}: {}", label, o.stdout_str().trim_end());
    }
    if !o.stderr.is_empty() {
        eprintln!("{} stderr: {}", label, o.stderr_str().trim_end());
    }
    if !o.success() {
        println!("{} (exit_code: {})", label, o.exit_code);
    }
}
