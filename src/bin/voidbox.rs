use std::fs;
use std::io::{self, IsTerminal, Write};
use std::net::SocketAddr;
use std::path::PathBuf;

use void_box::daemon;
use void_box::runtime::run_file;
use void_box::spec::{load_spec, validate_spec};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        print_usage();
        std::process::exit(1);
    }

    match args[1].as_str() {
        "serve" => cmd_serve(&args[2..]).await?,
        "run" => cmd_run(&args[2..]).await?,
        "validate" => cmd_validate(&args[2..])?,
        "status" => cmd_status(&args[2..]).await?,
        "logs" => cmd_logs(&args[2..]).await?,
        "tui" => cmd_tui(&args[2..]).await?,
        "exec" => cmd_legacy_exec(&args[2..]).await?,
        "workflow" => cmd_legacy_workflow(&args[2..]).await?,
        "version" | "--version" | "-V" => println!("voidbox {}", env!("CARGO_PKG_VERSION")),
        "help" | "--help" | "-h" => print_usage(),
        _ => {
            eprintln!("unknown command: {}", args[1]);
            print_usage();
            std::process::exit(1);
        }
    }

    Ok(())
}

async fn cmd_serve(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut listen = "127.0.0.1:43100".to_string();
    if let Some(v) = arg_value(args, "--listen") {
        listen = v;
    }
    let addr: SocketAddr = listen.parse()?;
    daemon::serve(addr).await
}

async fn cmd_run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let file = arg_value(args, "--file")
        .map(PathBuf::from)
        .ok_or("run requires --file <path>")?;
    let input = arg_value(args, "--input");

    let spec = load_spec(&file)?;
    print_startup_banner(&spec.sandbox);

    let report = run_file(&file, input, None).await?;
    println!("name: {}", report.name);
    println!("kind: {}", report.kind);
    println!("success: {}", report.success);
    println!("stages: {}", report.stages);
    println!("cost_usd: {:.6}", report.total_cost_usd);
    println!(
        "tokens: {} in / {} out",
        report.input_tokens, report.output_tokens
    );
    println!("output:\n{}", report.output);
    Ok(())
}

fn cmd_validate(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let file = arg_value(args, "--file")
        .map(PathBuf::from)
        .ok_or("validate requires --file <path>")?;

    let spec = load_spec(&file)?;
    validate_spec(&spec)?;
    println!(
        "valid: {} (kind={:?}, api_version={})",
        file.display(),
        spec.kind,
        spec.api_version
    );
    Ok(())
}

async fn cmd_status(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let run_id = arg_value(args, "--run-id").ok_or("status requires --run-id <id>")?;
    let daemon_url = arg_value(args, "--daemon").unwrap_or_else(|| "http://127.0.0.1:43100".into());

    let url = format!("{}/v1/runs/{}", daemon_url.trim_end_matches('/'), run_id);
    let body = reqwest::get(url).await?.text().await?;
    println!("{}", pretty_json(&body));
    Ok(())
}

async fn cmd_logs(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let run_id = arg_value(args, "--run-id").ok_or("logs requires --run-id <id>")?;
    let daemon_url = arg_value(args, "--daemon").unwrap_or_else(|| "http://127.0.0.1:43100".into());

    let url = format!(
        "{}/v1/runs/{}/events",
        daemon_url.trim_end_matches('/'),
        run_id
    );
    let body = reqwest::get(url).await?.text().await?;
    println!("{}", pretty_json(&body));
    Ok(())
}

async fn cmd_tui(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let daemon_url = arg_value(args, "--daemon").unwrap_or_else(|| "http://127.0.0.1:43100".into());
    let session_id = arg_value(args, "--session").unwrap_or_else(|| "default".into());

    let mut current_run: Option<String> = None;
    let mut staged_input: Option<String> = None;

    if let Some(file) = arg_value(args, "--file") {
        let run = create_remote_run(&daemon_url, &file, None).await?;
        println!("[tui] started {}", run);
        let _ = append_remote_message(
            &daemon_url,
            &session_id,
            "assistant",
            &format!("started run {}", run),
        )
        .await;
        current_run = Some(run);
    }

    print_logo_header(args);
    println!("voidbox tui");
    println!(
        "commands: /run <file>, /input <text>, /status, /logs, /cancel, /history, /help, /quit"
    );

    loop {
        print!("> ");
        io::stdout().flush()?;

        let mut line = String::new();
        if io::stdin().read_line(&mut line)? == 0 {
            break;
        }

        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let _ = append_remote_message(&daemon_url, &session_id, "user", line).await;

        if line == "/quit" || line == "/exit" {
            break;
        }

        if line == "/help" {
            println!("/run <file>");
            println!("/input <text>");
            println!("/status");
            println!("/logs");
            println!("/cancel");
            println!("/history");
            println!("/quit");
            continue;
        }

        if let Some(file) = line.strip_prefix("/run ") {
            let run = create_remote_run(&daemon_url, file.trim(), staged_input.take()).await?;
            println!("[tui] started {}", run);
            let _ = append_remote_message(
                &daemon_url,
                &session_id,
                "assistant",
                &format!("started run {}", run),
            )
            .await;
            current_run = Some(run);
            continue;
        }

        if let Some(text) = line.strip_prefix("/input ") {
            staged_input = Some(text.to_string());
            println!("[tui] staged input updated");
            let _ = append_remote_message(
                &daemon_url,
                &session_id,
                "assistant",
                "staged input updated",
            )
            .await;
            continue;
        }

        if line == "/status" {
            if let Some(run_id) = &current_run {
                let url = format!("{}/v1/runs/{}", daemon_url.trim_end_matches('/'), run_id);
                let body = reqwest::get(url).await?.text().await?;
                println!("{}", pretty_json(&body));
                let _ = append_remote_message(&daemon_url, &session_id, "assistant", &body).await;
            } else {
                println!("[tui] no active run");
            }
            continue;
        }

        if line == "/logs" {
            if let Some(run_id) = &current_run {
                let url = format!(
                    "{}/v1/runs/{}/events",
                    daemon_url.trim_end_matches('/'),
                    run_id
                );
                let body = reqwest::get(url).await?.text().await?;
                println!("{}", pretty_json(&body));
                let _ = append_remote_message(&daemon_url, &session_id, "assistant", &body).await;
            } else {
                println!("[tui] no active run");
            }
            continue;
        }

        if line == "/cancel" {
            if let Some(run_id) = &current_run {
                let url = format!(
                    "{}/v1/runs/{}/cancel",
                    daemon_url.trim_end_matches('/'),
                    run_id
                );
                let body = reqwest::Client::new()
                    .post(url)
                    .body("{}")
                    .send()
                    .await?
                    .text()
                    .await?;
                println!("{}", pretty_json(&body));
                let _ = append_remote_message(&daemon_url, &session_id, "assistant", &body).await;
            } else {
                println!("[tui] no active run");
            }
            continue;
        }

        if line == "/history" {
            let body = get_remote_messages(&daemon_url, &session_id).await?;
            println!("{}", pretty_json(&body));
            continue;
        }

        println!("assistant: use /commands (try /help)");
        let _ = append_remote_message(
            &daemon_url,
            &session_id,
            "assistant",
            "use /commands (try /help)",
        )
        .await;
    }

    Ok(())
}

async fn create_remote_run(
    daemon_url: &str,
    file: &str,
    input: Option<String>,
) -> Result<String, Box<dyn std::error::Error>> {
    let url = format!("{}/v1/runs", daemon_url.trim_end_matches('/'));
    let body = serde_json::json!({ "file": file, "input": input }).to_string();
    let resp = reqwest::Client::new()
        .post(url)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await?
        .text()
        .await?;

    let value = serde_json::from_str::<serde_json::Value>(&resp)?;
    let run_id = value
        .get("run_id")
        .and_then(serde_json::Value::as_str)
        .ok_or("missing run_id in daemon response")?;

    Ok(run_id.to_string())
}

async fn append_remote_message(
    daemon_url: &str,
    session_id: &str,
    role: &str,
    content: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!(
        "{}/v1/sessions/{}/messages",
        daemon_url.trim_end_matches('/'),
        session_id
    );
    let body = serde_json::json!({
        "role": role,
        "content": content
    })
    .to_string();

    let _ = reqwest::Client::new()
        .post(url)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await?;

    Ok(())
}

async fn get_remote_messages(
    daemon_url: &str,
    session_id: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let url = format!(
        "{}/v1/sessions/{}/messages",
        daemon_url.trim_end_matches('/'),
        session_id
    );
    let body = reqwest::get(url).await?.text().await?;
    Ok(body)
}

async fn cmd_legacy_exec(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.is_empty() {
        eprintln!("usage: voidbox exec <cmd> [args...]");
        std::process::exit(1);
    }

    eprintln!("[legacy] `exec` is deprecated; use `voidbox run --file ...`");

    let program = &args[0];
    let cmd_args = args[1..].iter().map(String::as_str).collect::<Vec<_>>();

    let sandbox = if let Some(kernel) = std::env::var_os("VOID_BOX_KERNEL") {
        let mut b = void_box::sandbox::Sandbox::local()
            .kernel(kernel)
            .network(true);
        if let Some(initramfs) = std::env::var_os("VOID_BOX_INITRAMFS") {
            b = b.initramfs(initramfs);
        }
        b.build()
            .unwrap_or_else(|_| void_box::sandbox::Sandbox::mock().build().unwrap())
    } else {
        void_box::sandbox::Sandbox::mock().build()?
    };

    let out = sandbox.exec(program, &cmd_args).await?;
    print!("{}", out.stdout_str());
    eprint!("{}", out.stderr_str());
    std::process::exit(out.exit_code);
}

async fn cmd_legacy_workflow(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("[legacy] `workflow` is deprecated; use `voidbox run --file` with kind=workflow");

    let action = args.first().map(String::as_str).unwrap_or("plan");
    let dir = args.get(1).map(String::as_str).unwrap_or("/workspace");

    let sandbox = void_box::sandbox::Sandbox::mock().build()?;

    let out = match action {
        "plan" => sandbox.exec("claude-code", &["plan", dir]).await?,
        "apply" => sandbox.exec("claude-code", &["apply", dir]).await?,
        _ => {
            return Err(format!("unknown workflow action '{}', expected plan|apply", action).into())
        }
    };

    print!("{}", out.stdout_str());
    eprint!("{}", out.stderr_str());
    Ok(())
}

fn arg_value(args: &[String], key: &str) -> Option<String> {
    args.windows(2).find(|w| w[0] == key).map(|w| w[1].clone())
}

fn pretty_json(raw: &str) -> String {
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| raw.to_string())
}

fn print_usage() {
    println!(
        r#"voidbox

USAGE:
  voidbox serve [--listen 127.0.0.1:43100]
  voidbox run --file <spec.json|yaml> [--input <text>]
  voidbox validate --file <spec.json|yaml>
  voidbox status --run-id <id> [--daemon http://127.0.0.1:43100]
  voidbox logs --run-id <id> [--daemon http://127.0.0.1:43100]
  voidbox tui [--file <spec.json|yaml>] [--daemon http://127.0.0.1:43100] [--session default]

LEGACY:
  voidbox exec <cmd> [args...]
  voidbox workflow <plan|apply> [dir]

NOTES:
  - Spec files: JSON (.json) and YAML (.yaml, .yml) are both supported.
  - TUI logo file: set VOIDBOX_LOGO_ASCII_PATH or pass --logo-ascii <path>.
  - LLM override envs for `run`: VOIDBOX_LLM_PROVIDER, VOIDBOX_LLM_MODEL, VOIDBOX_LLM_BASE_URL, VOIDBOX_LLM_API_KEY_ENV."#
    );
}

fn print_startup_banner(sandbox: &void_box::spec::SandboxSpec) {
    let banner = concat!(
        " ██╗   ██╗ ██████╗ ██╗██████╗        ██████╗  ██████╗ ██╗  ██╗\n",
        " ██║   ██║██╔═══██╗██║██╔══██╗       ██╔══██╗██╔═══██╗╚██╗██╔╝\n",
        " ██║   ██║██║   ██║██║██║  ██║       ██████╔╝██║   ██║ ╚███╔╝\n",
        " ╚██╗ ██╔╝██║   ██║██║██║  ██║█████╗ ██╔══██╗██║   ██║ ██╔██╗\n",
        "  ╚████╔╝ ╚██████╔╝██║██████╔╝╚════╝ ██████╔╝╚██████╔╝██╔╝ ██╗\n",
        "   ╚═══╝   ╚═════╝ ╚═╝╚═════╝        ╚═════╝  ╚═════╝ ╚═╝  ╚═╝",
    );
    let version = env!("CARGO_PKG_VERSION");
    let net = if sandbox.network { "on" } else { "off" };
    let mut summary = format!(
        "  {}MB RAM · {} vCPUs · network={}",
        sandbox.memory_mb, sandbox.vcpus, net
    );
    if sandbox.image.is_some() {
        summary.push_str(" · oci=yes");
    }
    if std::io::stderr().is_terminal() {
        eprintln!(
            "\x1b[38;5;153m{}  v{}\n\n{}\x1b[0m\n",
            banner, version, summary
        );
    } else {
        eprintln!("{}  v{}\n\n{}\n", banner, version, summary);
    }
}

fn print_logo_header(args: &[String]) {
    let logo_path = arg_value(args, "--logo-ascii")
        .or_else(|| std::env::var("VOIDBOX_LOGO_ASCII_PATH").ok())
        .unwrap_or_else(|| "assets/logo/void-box.txt".to_string());

    if let Ok(text) = fs::read_to_string(&logo_path) {
        if !text.trim().is_empty() {
            println!("{}", text);
            return;
        }
    }

    println!("⬢ VOID-BOX");
}
