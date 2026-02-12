//! Claude-in-voidbox example: multi-turn interaction with Claude-style CLI inside the sandbox.
//!
//! Rust port of BoxLite's [claude_in_boxlite_example.py](https://github.com/boxlite-ai/boxlite/blob/main/examples/python/claude_in_boxlite_example.py).
//! Uses void-box's `claude-code plan` / `claude-code apply` interface (mock in guest by default).
//!
//! Usage:
//!   cargo run --example claude_in_voidbox_example
//!
//! Then choose:
//!   1. Demo — automated plan then apply (multi-turn).
//!   2. Interactive — type messages; "plan" runs plan, "apply" runs apply with previous plan as stdin.
//!
//! For real Claude API:
//!   1. Set ANTHROPIC_API_KEY environment variable
//!   2. Build initramfs with: scripts/build-initramfs.sh
//!   3. Set VOID_BOX_KERNEL and VOID_BOX_INITRAMFS
//!   4. Run: ANTHROPIC_API_KEY=sk-ant-xxx cargo run --example claude_in_voidbox_example
//!
//! The sandbox will have SLIRP networking enabled (10.0.2.15/24) for API access.

use std::error::Error;
use std::io::{self, Write};
use std::sync::Arc;

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
    println!("  1. Demo (automated plan → apply)");
    println!("  2. Interactive (type 'plan' / 'apply' / 'quit')");
    print!("Select mode [1/2]: ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let choice = line.trim();

    if choice == "2" {
        interactive_session(sandbox).await?;
    } else {
        demo_multi_turn(sandbox).await?;
    }

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
        .memory_mb(256)
        .vcpus(1)
        .kernel(&kernel)
        .network(true);  // Enable SLIRP networking for API access

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

/// Automated demo: run plan, then apply with plan output as stdin.
async fn demo_multi_turn(sandbox: Arc<Sandbox>) -> Result<(), Box<dyn Error>> {
    println!("\n=== Multi-turn Demo ===\n");

    println!("Turn 1: plan");
    let plan_out = sandbox
        .exec("claude-code", &["plan", WORKSPACE])
        .await?;
    let plan_stdout = plan_out.stdout.clone();
    display_output("Claude (plan)", &plan_out);

    println!("\nTurn 2: apply (with plan as stdin)");
    let apply_out = sandbox
        .exec_with_stdin("claude-code", &["apply", WORKSPACE], &plan_stdout)
        .await?;
    display_output("Claude (apply)", &apply_out);

    let ok = apply_out.success() && apply_out.stdout_str().contains("applied");
    if ok {
        println!("\n✓ Demo completed.");
    } else {
        println!("\n✗ Apply step did not report success.");
    }
    Ok(())
}

/// Interactive: loop reading "plan" / "apply" / "quit"; run claude-code in the sandbox.
async fn interactive_session(sandbox: Arc<Sandbox>) -> Result<(), Box<dyn Error>> {
    println!("\n=== Interactive Session ===");
    println!("Commands: plan | apply | quit\n");

    let mut last_plan: Option<Vec<u8>> = None;

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
        if input.eq_ignore_ascii_case("quit") || input.eq_ignore_ascii_case("exit") || input == "q" {
            break;
        }

        if input.eq_ignore_ascii_case("apply") {
            let stdin = last_plan.as_deref().unwrap_or(&[]);
            if stdin.is_empty() {
                println!("Claude: (Run 'plan' first to have something to apply.)");
                continue;
            }
            let out = sandbox
                .exec_with_stdin("claude-code", &["apply", WORKSPACE], stdin)
                .await?;
            display_output("Claude", &out);
            continue;
        }

        if input.eq_ignore_ascii_case("plan") || input.starts_with("plan") {
            let out = sandbox
                .exec("claude-code", &["plan", WORKSPACE])
                .await?;
            last_plan = Some(out.stdout.clone());
            display_output("Claude", &out);
            continue;
        }

        // Any other input: treat as "plan" request (user might type a prompt; we still run plan)
        let out = sandbox
            .exec("claude-code", &["plan", WORKSPACE])
            .await?;
        last_plan = Some(out.stdout.clone());
        display_output("Claude", &out);
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
