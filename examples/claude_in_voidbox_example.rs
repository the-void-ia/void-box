//! Claude-in-voidbox example: run the real Claude Code CLI inside a KVM micro-VM sandbox.
//!
//! Demonstrates multi-turn interaction with `claude-code` executing inside the
//! guest VM, with SLIRP networking providing NAT-based API access.
//!
//! Usage (mock, no KVM required):
//!   cargo run --example claude_in_voidbox_example
//!
//! Usage (real KVM + real Claude API):
//!   1. Build the guest initramfs:  scripts/build_claude_rootfs.sh
//!   2. Run:
//!      ANTHROPIC_API_KEY=sk-ant-xxx \
//!      VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
//!      VOID_BOX_INITRAMFS=target/void-box-rootfs.cpio.gz \
//!      cargo run --example claude_in_voidbox_example
//!
//! The sandbox uses SLIRP networking (guest 10.0.2.15/24, gateway 10.0.2.2, DNS 10.0.2.3).

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
        .memory_mb(512)
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

/// Automated demo: run two turns with the real Claude CLI.
///
/// Turn 1: Ask Claude to create a plan (non-interactive via -p/--print).
/// Turn 2: Feed the plan back and ask Claude to apply it.
async fn demo_multi_turn(sandbox: Arc<Sandbox>) -> Result<(), Box<dyn Error>> {
    println!("\n=== Multi-turn Demo ===\n");

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
    let plan_out = sandbox
        .exec("claude-code", &[
            "-p", &plan_prompt,
            "--output-format", "text",
            "--dangerously-skip-permissions",
        ])
        .await?;
    let plan_stdout = plan_out.stdout.clone();
    display_output("Claude (plan)", &plan_out);

    // Turn 2: ask Claude to apply the plan
    let apply_prompt = format!(
        "Apply the following plan in {}. Execute each step.\n\n{}",
        WORKSPACE,
        String::from_utf8_lossy(&plan_stdout)
    );
    println!("\nTurn 2: apply\n  prompt: {} bytes\n", apply_prompt.len());
    let apply_out = sandbox
        .exec("claude-code", &[
            "-p", &apply_prompt,
            "--output-format", "text",
            "--dangerously-skip-permissions",
        ])
        .await?;
    display_output("Claude (apply)", &apply_out);

    if apply_out.success() {
        println!("\n✓ Demo completed.");
    } else {
        println!("\n✗ Apply step did not succeed (exit_code={}).", apply_out.exit_code);
    }
    Ok(())
}

/// Interactive: type a prompt, Claude responds. Type "quit" to exit.
async fn interactive_session(sandbox: Arc<Sandbox>) -> Result<(), Box<dyn Error>> {
    println!("\n=== Interactive Session ===");
    println!("Type a prompt for Claude (or 'quit' to exit).\n");

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

        // Send the user's input as a prompt to the real Claude CLI
        let out = sandbox
            .exec("claude-code", &[
                "-p", input,
                "--output-format", "text",
                "--dangerously-skip-permissions",
            ])
            .await?;
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
