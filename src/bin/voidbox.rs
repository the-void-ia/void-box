//! void-box CLI - Quick testing and utility wrapper
//!
//! Usage:
//!   voidbox exec "echo hello"
//!   voidbox workflow plan /workspace
//!   voidbox daemon --port 8080

use std::process;
use void_box::prelude::*;

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        print_usage();
        process::exit(1);
    }

    let command = &args[1];

    match command.as_str() {
        "exec" => cmd_exec(&args[2..]).await?,
        "workflow" => cmd_workflow(&args[2..]).await?,
        "version" => cmd_version(),
        "help" | "--help" | "-h" => print_usage(),
        _ => {
            eprintln!("Unknown command: {}", command);
            print_usage();
            process::exit(1);
        }
    }

    Ok(())
}

async fn cmd_exec(args: &[String]) -> std::result::Result<(), Box<dyn std::error::Error>> {
    if args.is_empty() {
        eprintln!("Usage: voidbox exec <command> [args...]");
        process::exit(1);
    }

    let program = &args[0];
    let cmd_args: Vec<&str> = args[1..].iter().map(|s| s.as_str()).collect();

    // Try to create KVM sandbox, fall back to mock
    let sandbox = match try_kvm_sandbox() {
        Some(s) => {
            eprintln!("[voidbox] Using KVM sandbox");
            s
        }
        None => {
            eprintln!("[voidbox] Using mock sandbox (set VOID_BOX_KERNEL and VOID_BOX_INITRAMFS for KVM)");
            Sandbox::mock().build()?
        }
    };

    let output = sandbox.exec(program, &cmd_args).await?;

    // Print output
    print!("{}", output.stdout_str());
    eprint!("{}", output.stderr_str());

    process::exit(output.exit_code);
}

async fn cmd_workflow(args: &[String]) -> std::result::Result<(), Box<dyn std::error::Error>> {
    if args.is_empty() {
        eprintln!("Usage: voidbox workflow <plan|apply> <directory>");
        process::exit(1);
    }

    let action = &args[0];
    let workspace = args.get(1).map(|s| s.as_str()).unwrap_or("/workspace");

    let sandbox = try_kvm_sandbox()
        .unwrap_or_else(|| {
            eprintln!("[voidbox] Using mock sandbox");
            Sandbox::mock().build().unwrap()
        });

    match action.as_str() {
        "plan" => {
            let output = sandbox.exec("claude-code", &["plan", workspace]).await?;
            println!("{}", output.stdout_str());
            if !output.stderr.is_empty() {
                eprintln!("{}", output.stderr_str());
            }
        }
        "apply" => {
            let output = sandbox.exec("claude-code", &["apply", workspace]).await?;
            println!("{}", output.stdout_str());
            if !output.stderr.is_empty() {
                eprintln!("{}", output.stderr_str());
            }
        }
        _ => {
            eprintln!("Unknown workflow action: {}", action);
            eprintln!("Valid actions: plan, apply");
            process::exit(1);
        }
    }

    Ok(())
}

fn cmd_version() {
    println!("voidbox {}", env!("CARGO_PKG_VERSION"));
}

fn print_usage() {
    println!(
        r#"
void-box - Composable workflow sandbox with KVM micro-VMs

USAGE:
    voidbox <COMMAND> [OPTIONS]

COMMANDS:
    exec <cmd> [args...]    Execute command in sandbox
    workflow <action> <dir> Run workflow (plan/apply)
    version                 Print version
    help                    Print this help

EXAMPLES:
    # Simple execution
    voidbox exec echo "hello world"

    # With environment variables for KVM mode
    VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
    VOID_BOX_INITRAMFS=void-box-initramfs-v0.1.0-x86_64.cpio.gz \
    voidbox exec echo "hello from KVM"

    # Workflow
    voidbox workflow plan /workspace
    voidbox workflow apply /workspace

ENVIRONMENT:
    VOID_BOX_KERNEL       Path to kernel (for KVM mode)
    VOID_BOX_INITRAMFS    Path to initramfs (for KVM mode)

    If not set, uses mock sandbox (no isolation).

For more information: https://github.com/the-void-ia/void-box
"#
    );
}

fn try_kvm_sandbox() -> Option<std::sync::Arc<Sandbox>> {
    use std::path::PathBuf;

    let kernel = std::env::var_os("VOID_BOX_KERNEL")
        .map(PathBuf::from)
        .filter(|p| p.exists())?;

    let initramfs = std::env::var_os("VOID_BOX_INITRAMFS")
        .map(PathBuf::from)
        .filter(|p| p.exists());

    let mut builder = Sandbox::local()
        .kernel(kernel)
        .memory_mb(512)
        .vcpus(1)
        .network(true);

    if let Some(initramfs) = initramfs {
        builder = builder.initramfs(initramfs);
    }

    builder.build().ok()
}
