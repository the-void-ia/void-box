mod attach;
mod backend;
mod banner;
mod cli_config;
mod output;
mod snapshot;

use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use tracing_subscriber::fmt::writer::MakeWriterExt;
use tracing_subscriber::EnvFilter;

use backend::{LocalBackend, RemoteBackend};
use cli_config::ResolvedConfig;
use output::{format_json_value, print_json_value, OutputFormat};

const RUNTIME_LOG_FILENAME: &str = "voidbox.log";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CommandIoMode {
    Standard,
    InteractivePty,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TracingMode {
    Standard,
    InteractivePty,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum TracingWriterMode {
    StderrOnly,
    StderrAndFile,
    FileOnly,
    SinkOnly,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TracingPlan {
    writer_mode: TracingWriterMode,
    notice: Option<String>,
}

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

/// VoidBox — composable workflow sandbox with micro-VMs and native observability.
#[derive(Parser, Debug)]
#[command(name = "voidbox", version, about, long_about = None)]
struct Cli {
    /// Output format: human (default) or json.
    #[arg(long, global = true, default_value = "human")]
    output: OutputFormat,

    /// Suppress the ASCII startup banner.
    #[arg(long, global = true)]
    no_banner: bool,

    /// Override log level (trace, debug, info, warn, error).
    #[arg(long, global = true, env = "VOIDBOX_LOG_LEVEL")]
    log_level: Option<String>,

    /// Override log directory for file-based runtime logs.
    #[arg(long, global = true, env = "VOIDBOX_LOG_DIR")]
    log_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run a spec file (local, in-process).
    Run {
        /// Path to the spec file (YAML or JSON).
        #[arg(long)]
        file: PathBuf,
        /// Optional input text for the run.
        #[arg(long)]
        input: Option<String>,
    },

    /// Execute a command in a sandbox (legacy, deprecated).
    Exec {
        /// Program to execute.
        program: String,
        /// Arguments to the program.
        args: Vec<String>,
    },

    /// Validate a spec file without running it.
    Validate {
        /// Path to the spec file.
        #[arg(long)]
        file: PathBuf,
    },

    /// Inspect a spec file: validate and show resolved configuration.
    Inspect {
        /// Path to the spec file.
        #[arg(long)]
        file: PathBuf,
    },

    /// List skills defined in a spec file.
    Skills {
        /// Path to the spec file.
        #[arg(long)]
        file: PathBuf,
    },

    /// Query run status from the daemon (remote only).
    Status {
        /// Run ID to query.
        #[arg(long)]
        run_id: String,
        /// Daemon URL override.
        #[arg(long)]
        daemon: Option<String>,
    },

    /// Fetch run logs from the daemon (remote only).
    Logs {
        /// Run ID to query.
        #[arg(long)]
        run_id: String,
        /// Daemon URL override.
        #[arg(long)]
        daemon: Option<String>,
    },

    /// Interactive TUI (connects to daemon).
    Tui {
        /// Optional spec file to start a run immediately.
        #[arg(long)]
        file: Option<String>,
        /// Daemon URL override.
        #[arg(long)]
        daemon: Option<String>,
        /// Session ID.
        #[arg(long, default_value = "default")]
        session: String,
        /// Path to a custom ASCII logo file.
        #[arg(long)]
        logo_ascii: Option<String>,
    },

    /// Manage VM snapshots.
    Snapshot {
        #[command(subcommand)]
        command: snapshot::SnapshotCommand,
    },

    /// Manage CLI configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },

    /// Print version information.
    Version,

    /// Internal HTTP daemon (future `voidboxd`).
    Serve {
        /// Listen address.
        #[arg(long, default_value = "127.0.0.1:43100")]
        listen: String,
    },

    /// Attach an interactive PTY to a running VM.
    Attach {
        /// Run ID of the target VM.
        #[arg(long)]
        run_id: String,
        /// Program to run (default: sh).
        #[arg(long, default_value = "sh")]
        program: String,
        /// Arguments to the program.
        #[arg(long)]
        args: Vec<String>,
        /// Working directory inside the guest.
        #[arg(long)]
        working_dir: Option<String>,
        /// Daemon URL override.
        #[arg(long)]
        daemon: Option<String>,
    },

    /// Boot an ephemeral VM and open an interactive shell.
    Shell {
        /// Spec file (optional; generates ephemeral spec if omitted).
        #[arg(long)]
        file: Option<PathBuf>,
        /// Program to run in the PTY.
        #[arg(long, default_value = "claude")]
        program: String,
        /// Arguments to the program.
        #[arg(long)]
        args: Vec<String>,
        /// Working directory inside the guest.
        #[arg(long)]
        working_dir: Option<String>,
        /// Guest memory in MB.
        #[arg(long, default_value = "1024")]
        memory_mb: usize,
        /// Number of vCPUs.
        #[arg(long, default_value = "2")]
        vcpus: usize,
        /// Enable guest networking.
        #[arg(long, default_value = "true")]
        network: bool,
        /// LLM provider override.
        #[arg(long)]
        provider: Option<String>,
        /// Restore from snapshot.
        #[arg(long)]
        snapshot: Option<String>,
        /// Mount host directory (HOST:GUEST[:ro|rw], repeatable).
        #[arg(long = "mount")]
        mounts: Vec<String>,
        /// Set guest env var (KEY=VALUE, repeatable).
        #[arg(long = "env")]
        env_vars: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
enum ConfigCommand {
    /// Write a template config file to ~/.config/voidbox/config.yaml.
    Init,
    /// Validate and display the resolved configuration.
    Validate,
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let output_format = cli.output;

    let config = cli_config::load_and_merge(
        cli.log_level.as_deref(),
        cli.log_dir.as_deref(),
        None, // daemon URL from CLI is per-subcommand
        cli.no_banner,
    );

    if let Some(warning) = init_tracing(tracing_mode_for_command(&cli.command), &config) {
        eprintln!("{warning}");
    }

    let daemon_url = resolved_daemon_url(&cli.command, &config);
    let remote = RemoteBackend::new(daemon_url);
    let exit_code = match run(cli, &config, &remote).await {
        Ok(code) => code,
        Err(e) => {
            output::report_error(output_format, e.as_ref());
            1
        }
    };

    std::process::exit(exit_code);
}

/// Effective daemon URL for this invocation (`--daemon` on remote subcommands wins over config).
fn resolved_daemon_url(command: &Command, config: &ResolvedConfig) -> String {
    match command {
        Command::Status { daemon, .. }
        | Command::Logs { daemon, .. }
        | Command::Tui { daemon, .. }
        | Command::Attach { daemon, .. } => {
            daemon.clone().unwrap_or_else(|| config.daemon_url.clone())
        }
        _ => config.daemon_url.clone(),
    }
}

fn command_io_mode(command: &Command) -> CommandIoMode {
    match command {
        Command::Shell { .. } => CommandIoMode::InteractivePty,
        _ => CommandIoMode::Standard,
    }
}

fn tracing_mode_for_command(command: &Command) -> TracingMode {
    match command_io_mode(command) {
        CommandIoMode::Standard => TracingMode::Standard,
        CommandIoMode::InteractivePty => TracingMode::InteractivePty,
    }
}

async fn run(
    cli: Cli,
    config: &ResolvedConfig,
    remote: &RemoteBackend,
) -> Result<i32, Box<dyn std::error::Error>> {
    let output = cli.output;
    match cli.command {
        Command::Run { file, input } => {
            let spec = void_box::spec::load_spec(&file)?;
            if banner::should_show_banner(output, config.banner) {
                banner::print_startup_banner(&spec.sandbox);
            }
            let result = LocalBackend::run(&file, input).await?;
            output::print_json_or_human(output, &result, |r| print!("{r}"));
            Ok(0)
        }
        Command::Exec { program, args } => cmd_exec(&program, &args).await,
        Command::Validate { file } => cmd_validate(output, &file).map(|_| 0),
        Command::Inspect { file } => cmd_inspect(output, &file).map(|_| 0),
        Command::Skills { file } => cmd_skills(output, &file).map(|_| 0),
        Command::Status { run_id, .. } => cmd_status(output, remote, &run_id).await.map(|_| 0),
        Command::Logs { run_id, .. } => cmd_logs(output, remote, &run_id).await.map(|_| 0),
        Command::Tui {
            file,
            session,
            logo_ascii,
            ..
        } => cmd_tui(remote, &session, file.as_deref(), logo_ascii.as_deref())
            .await
            .map(|_| 0),
        Command::Snapshot { command } => {
            snapshot::handle(command, output, &config.paths.snapshot_dir)
                .await
                .map(|_| 0)
        }
        Command::Config { command } => cmd_config(command, output, config).map(|_| 0),
        Command::Version => cmd_version(output).map(|_| 0),
        Command::Serve { listen } => cmd_serve(&listen).await.map(|_| 0),
        Command::Attach {
            run_id,
            program,
            args,
            working_dir,
            daemon,
            ..
        } => {
            let url = daemon.unwrap_or_else(|| config.daemon_url.clone());
            attach::cmd_attach(&run_id, Some(&program), &args, working_dir.as_deref(), &url).await
        }
        Command::Shell {
            file,
            program,
            args,
            working_dir,
            memory_mb,
            vcpus,
            network,
            provider,
            snapshot,
            mounts,
            env_vars,
        } => {
            attach::cmd_shell(attach::ShellOpts {
                file: file.as_deref(),
                program: &program,
                args: &args,
                working_dir: working_dir.as_deref(),
                memory_mb,
                vcpus,
                network,
                provider: provider.as_deref(),
                snapshot: snapshot.as_deref(),
                mounts: &mounts,
                env_vars: &env_vars,
                log_dir: &config.paths.log_dir,
            })
            .await
        }
    }
}

// ---------------------------------------------------------------------------
// Tracing initialization
// ---------------------------------------------------------------------------

fn init_tracing(mode: TracingMode, config: &ResolvedConfig) -> Option<String> {
    let filter = EnvFilter::try_new(&config.log_level).unwrap_or_else(|_| EnvFilter::new("info"));
    let log_dir = &config.paths.log_dir;
    let runtime_log_path = log_dir.join(RUNTIME_LOG_FILENAME);
    let log_dir_ready = log_dir.exists() || std::fs::create_dir_all(log_dir).is_ok();
    let plan = tracing_plan(mode, log_dir, log_dir_ready);

    if log_dir_ready {
        let file_appender = tracing_appender::rolling::daily(log_dir, RUNTIME_LOG_FILENAME);
        let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
        // Leak the guard so the appender lives for the process lifetime.
        std::mem::forget(_guard);

        let subscriber = tracing_subscriber::fmt().with_env_filter(filter);
        match plan.writer_mode {
            TracingWriterMode::FileOnly => subscriber.with_writer(non_blocking).init(),
            TracingWriterMode::StderrAndFile => subscriber
                .with_writer(non_blocking.and(std::io::stderr))
                .init(),
            TracingWriterMode::StderrOnly | TracingWriterMode::SinkOnly => unreachable!(),
        }
    } else {
        match plan.writer_mode {
            TracingWriterMode::SinkOnly => {
                tracing_subscriber::fmt()
                    .with_env_filter(filter)
                    .with_writer(std::io::sink)
                    .init();
            }
            TracingWriterMode::StderrOnly => {
                tracing_subscriber::fmt().with_env_filter(filter).init();
            }
            TracingWriterMode::StderrAndFile | TracingWriterMode::FileOnly => unreachable!(),
        }
    }
    let _ = runtime_log_path;
    plan.notice
}

fn tracing_plan(mode: TracingMode, log_dir: &Path, log_dir_ready: bool) -> TracingPlan {
    let runtime_log_path = log_dir.join(RUNTIME_LOG_FILENAME);
    match (mode, log_dir_ready) {
        (TracingMode::InteractivePty, true) => TracingPlan {
            writer_mode: TracingWriterMode::FileOnly,
            notice: Some(format!(
                "interactive mode: runtime logs will be written to {} to avoid terminal corruption.",
                runtime_log_path.display()
            )),
        },
        (TracingMode::InteractivePty, false) => TracingPlan {
            writer_mode: TracingWriterMode::SinkOnly,
            notice: Some(format!(
                "warning: interactive logs could not be routed to file at {}. runtime logs will be suppressed for this session to avoid terminal corruption. set --log-dir, VOIDBOX_LOG_DIR, VOIDBOX_HOME, or paths.log_dir to enable interactive log capture.",
                log_dir.display()
            )),
        },
        (TracingMode::Standard, true) => TracingPlan {
            writer_mode: TracingWriterMode::StderrAndFile,
            notice: None,
        },
        (TracingMode::Standard, false) => TracingPlan {
            writer_mode: TracingWriterMode::StderrOnly,
            notice: None,
        },
    }
}

// ---------------------------------------------------------------------------
// Command handlers
// ---------------------------------------------------------------------------

async fn cmd_exec(program: &str, args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    eprintln!("[legacy] `exec` is deprecated; use `voidbox run --file ...`");

    let cmd_args: Vec<&str> = args.iter().map(String::as_str).collect();

    let sandbox = if let Some(kernel) = std::env::var_os("VOID_BOX_KERNEL") {
        let mut b = void_box::sandbox::Sandbox::local()
            .kernel(kernel)
            .network(true);
        if let Some(initramfs) = std::env::var_os("VOID_BOX_INITRAMFS") {
            b = b.initramfs(initramfs);
        }
        b.build()?
    } else {
        eprintln!("[exec] VOID_BOX_KERNEL not specified, falling back to mock");
        void_box::sandbox::Sandbox::mock().build()?
    };

    let out = sandbox.exec(program, &cmd_args).await?;
    print!("{}", out.stdout_str());
    eprint!("{}", out.stderr_str());
    Ok(out.exit_code)
}

fn cmd_validate(format: OutputFormat, file: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let spec = void_box::spec::load_spec(file)?;
    void_box::spec::validate_spec(&spec)?;

    #[derive(serde::Serialize)]
    struct ValidateResult {
        valid: bool,
        file: String,
        kind: String,
        api_version: String,
    }

    let result = ValidateResult {
        valid: true,
        file: file.display().to_string(),
        kind: format!("{:?}", spec.kind).to_lowercase(),
        api_version: spec.api_version.clone(),
    };

    output::print_json_or_human(format, &result, |r| {
        println!(
            "valid: {} (kind={}, api_version={})",
            r.file, r.kind, r.api_version
        );
    });
    Ok(())
}

fn cmd_inspect(format: OutputFormat, file: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let spec = void_box::spec::load_spec(file)?;
    void_box::spec::validate_spec(&spec)?;

    let kernel = spec
        .sandbox
        .kernel
        .clone()
        .or_else(|| std::env::var("VOID_BOX_KERNEL").ok());
    let initramfs = spec
        .sandbox
        .initramfs
        .clone()
        .or_else(|| std::env::var("VOID_BOX_INITRAMFS").ok());

    #[derive(serde::Serialize)]
    struct InspectReport {
        file: String,
        name: String,
        kind: String,
        api_version: String,
        sandbox: SandboxReport,
    }

    #[derive(serde::Serialize)]
    struct SandboxReport {
        mode: String,
        kernel: Option<String>,
        initramfs: Option<String>,
        memory_mb: usize,
        vcpus: usize,
        network: bool,
        image: Option<String>,
        snapshot: Option<String>,
        mounts: usize,
        env_vars: usize,
    }

    let report = InspectReport {
        file: file.display().to_string(),
        name: spec.name.clone(),
        kind: format!("{:?}", spec.kind).to_lowercase(),
        api_version: spec.api_version.clone(),
        sandbox: SandboxReport {
            mode: spec.sandbox.mode.clone(),
            kernel,
            initramfs,
            memory_mb: spec.sandbox.memory_mb,
            vcpus: spec.sandbox.vcpus,
            network: spec.sandbox.network,
            image: spec.sandbox.image.clone(),
            snapshot: spec.sandbox.snapshot.clone(),
            mounts: spec.sandbox.mounts.len(),
            env_vars: spec.sandbox.env.len(),
        },
    };

    output::print_json_or_human(format, &report, |r| {
        println!("File:       {}", r.file);
        println!("Name:       {}", r.name);
        println!("Kind:       {}", r.kind);
        println!("API:        {}", r.api_version);
        println!("--- Sandbox ---");
        println!("  Mode:       {}", r.sandbox.mode);
        println!(
            "  Kernel:     {}",
            r.sandbox.kernel.as_deref().unwrap_or("(env/default)")
        );
        println!(
            "  Initramfs:  {}",
            r.sandbox.initramfs.as_deref().unwrap_or("(env/default)")
        );
        println!("  Memory:     {} MB", r.sandbox.memory_mb);
        println!("  vCPUs:      {}", r.sandbox.vcpus);
        println!("  Network:    {}", r.sandbox.network);
        if let Some(img) = &r.sandbox.image {
            println!("  Image:      {}", img);
        }
        if let Some(snap) = &r.sandbox.snapshot {
            println!("  Snapshot:   {}", snap);
        }
        println!("  Mounts:     {}", r.sandbox.mounts);
        println!("  Env vars:   {}", r.sandbox.env_vars);
    });
    Ok(())
}

fn cmd_skills(format: OutputFormat, file: &Path) -> Result<(), Box<dyn std::error::Error>> {
    use void_box::spec::SkillEntry;

    #[derive(serde::Serialize)]
    struct SkillRow {
        source: String,
        value: String,
    }

    fn to_row(source: &str, entry: &SkillEntry) -> SkillRow {
        match entry {
            SkillEntry::Simple(s) => SkillRow {
                source: source.into(),
                value: s.clone(),
            },
            SkillEntry::Oci {
                image,
                mount,
                readonly,
            } => SkillRow {
                source: source.into(),
                value: format!("oci:{image} → {mount} (ro={readonly})"),
            },
            SkillEntry::Mcp {
                command,
                args,
                env: _,
            } => SkillRow {
                source: source.into(),
                value: format!("mcp:{command} {}", args.join(" ")),
            },
            SkillEntry::Inline { name, .. } => SkillRow {
                source: source.into(),
                value: format!("inline:{name}"),
            },
        }
    }

    let spec = void_box::spec::load_spec(file)?;
    let mut skills: Vec<SkillRow> = Vec::new();

    if let Some(agent) = &spec.agent {
        for entry in &agent.skills {
            skills.push(to_row("agent", entry));
        }
    }

    if let Some(pipeline) = &spec.pipeline {
        for bx in &pipeline.boxes {
            for entry in &bx.skills {
                skills.push(to_row(&format!("pipeline:{}", bx.name), entry));
            }
        }
    }

    output::print_json_or_human(format, &skills, |rows: &Vec<SkillRow>| {
        if rows.is_empty() {
            println!("No skills defined.");
            return;
        }
        println!("{:<30} SKILL", "SOURCE");
        for row in rows {
            println!("{:<30} {}", row.source, row.value);
        }
    });
    Ok(())
}

async fn cmd_status(
    format: OutputFormat,
    remote: &RemoteBackend,
    run_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let value = remote.status(run_id).await?;
    print_json_value(format, &value);
    Ok(())
}

async fn cmd_logs(
    format: OutputFormat,
    remote: &RemoteBackend,
    run_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let value = remote.logs(run_id).await?;
    print_json_value(format, &value);
    Ok(())
}

/// Parsed TUI command from a user input line.
#[derive(Debug, PartialEq)]
enum TuiCommand<'a> {
    Quit,
    Help,
    Run(&'a str),
    Input(&'a str),
    Status,
    Logs,
    Cancel,
    History,
    Unknown,
}

fn parse_tui_command(line: &str) -> TuiCommand<'_> {
    let line = line.trim();
    match line {
        "/quit" | "/exit" => TuiCommand::Quit,
        "/help" => TuiCommand::Help,
        "/status" => TuiCommand::Status,
        "/logs" => TuiCommand::Logs,
        "/cancel" => TuiCommand::Cancel,
        "/history" => TuiCommand::History,
        _ if line.starts_with("/run ") => TuiCommand::Run(line["/run ".len()..].trim()),
        _ if line.starts_with("/input ") => TuiCommand::Input(&line["/input ".len()..]),
        _ => TuiCommand::Unknown,
    }
}

async fn tui_persist(remote: &RemoteBackend, session_id: &str, role: &str, content: &str) {
    if let Err(e) = remote.append_message(session_id, role, content).await {
        eprintln!("[tui] warning: failed to persist message: {e}");
    }
}

async fn cmd_tui(
    remote: &RemoteBackend,
    session_id: &str,
    file: Option<&str>,
    logo_ascii: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut current_run: Option<String> = None;
    let mut staged_input: Option<String> = None;

    if let Some(file) = file {
        let run = remote.create_run(file, None).await?;
        println!("[tui] started {}", run);
        tui_persist(
            remote,
            session_id,
            "assistant",
            &format!("started run {}", run),
        )
        .await;
        current_run = Some(run);
    }

    banner::print_logo_header(logo_ascii);
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

        tui_persist(remote, session_id, "user", line).await;

        match parse_tui_command(line) {
            TuiCommand::Quit => break,
            TuiCommand::Help => {
                println!("/run <file>");
                println!("/input <text>");
                println!("/status");
                println!("/logs");
                println!("/cancel");
                println!("/history");
                println!("/quit");
            }
            TuiCommand::Run(file) => {
                let run = remote.create_run(file, staged_input.take()).await?;
                println!("[tui] started {}", run);
                tui_persist(
                    remote,
                    session_id,
                    "assistant",
                    &format!("started run {}", run),
                )
                .await;
                current_run = Some(run);
            }
            TuiCommand::Input(text) => {
                staged_input = Some(text.to_string());
                println!("[tui] staged input updated");
                tui_persist(remote, session_id, "assistant", "staged input updated").await;
            }
            TuiCommand::Status => {
                if let Some(run_id) = &current_run {
                    let body = remote.status(run_id).await?;
                    let text = format_json_value(&body);
                    println!("{text}");
                    tui_persist(remote, session_id, "assistant", &text).await;
                } else {
                    println!("[tui] no active run");
                }
            }
            TuiCommand::Logs => {
                if let Some(run_id) = &current_run {
                    let body = remote.logs(run_id).await?;
                    let text = format_json_value(&body);
                    println!("{text}");
                    tui_persist(remote, session_id, "assistant", &text).await;
                } else {
                    println!("[tui] no active run");
                }
            }
            TuiCommand::Cancel => {
                if let Some(run_id) = &current_run {
                    let body = remote.cancel_run(run_id).await?;
                    let text = format_json_value(&body);
                    println!("{text}");
                    tui_persist(remote, session_id, "assistant", &text).await;
                } else {
                    println!("[tui] no active run");
                }
            }
            TuiCommand::History => {
                let body = remote.get_messages(session_id).await?;
                println!("{}", format_json_value(&body));
            }
            TuiCommand::Unknown => {
                println!("assistant: use /commands (try /help)");
                tui_persist(remote, session_id, "assistant", "use /commands (try /help)").await;
            }
        }
    }

    Ok(())
}

fn cmd_config(
    command: ConfigCommand,
    format: OutputFormat,
    config: &ResolvedConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        ConfigCommand::Init => {
            let path = config.paths.config_dir.join("config.yaml");
            if path.exists() {
                return Err(format!(
                    "config file already exists at {}; remove it first to re-initialize",
                    path.display()
                )
                .into());
            }
            cli_config::write_template(&path)?;
            println!("Wrote config template to {}", path.display());
            Ok(())
        }
        ConfigCommand::Validate => {
            #[derive(serde::Serialize)]
            struct ConfigReport {
                log_level: String,
                daemon_url: String,
                banner: bool,
                state_dir: String,
                log_dir: String,
                snapshot_dir: String,
                config_dir: String,
                kernel: Option<String>,
                initramfs: Option<String>,
            }

            let report = ConfigReport {
                log_level: config.log_level.clone(),
                daemon_url: config.daemon_url.clone(),
                banner: config.banner,
                state_dir: config.paths.state_dir.display().to_string(),
                log_dir: config.paths.log_dir.display().to_string(),
                snapshot_dir: config.paths.snapshot_dir.display().to_string(),
                config_dir: config.paths.config_dir.display().to_string(),
                kernel: config.kernel.as_ref().map(|p| p.display().to_string()),
                initramfs: config.initramfs.as_ref().map(|p| p.display().to_string()),
            };

            output::print_json_or_human(format, &report, |r| {
                println!("Resolved configuration:");
                println!("  log_level:    {}", r.log_level);
                println!("  daemon_url:   {}", r.daemon_url);
                println!("  banner:       {}", r.banner);
                println!("  state_dir:    {}", r.state_dir);
                println!("  log_dir:      {}", r.log_dir);
                println!("  snapshot_dir: {}", r.snapshot_dir);
                println!("  config_dir:   {}", r.config_dir);
                println!(
                    "  kernel:       {}",
                    r.kernel.as_deref().unwrap_or("(not set)")
                );
                println!(
                    "  initramfs:    {}",
                    r.initramfs.as_deref().unwrap_or("(not set)")
                );
            });
            Ok(())
        }
    }
}

fn cmd_version(format: OutputFormat) -> Result<(), Box<dyn std::error::Error>> {
    let version = env!("CARGO_PKG_VERSION");

    #[derive(serde::Serialize)]
    struct VersionInfo {
        version: String,
        name: String,
    }

    let info = VersionInfo {
        version: version.into(),
        name: "voidbox".into(),
    };

    output::print_json_or_human(format, &info, |_| {
        println!("voidbox {version}");
    });
    Ok(())
}

/// Handler for `serve`: internal HTTP daemon (future `voidboxd`); see `Command::Serve`.
async fn cmd_serve(listen: &str) -> Result<(), Box<dyn std::error::Error>> {
    let addr: SocketAddr = listen.parse()?;
    void_box::daemon::serve(addr).await
}

// ---------------------------------------------------------------------------
// Clap argv parsing (no subprocess — exercises Parser / Subcommand wiring)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod cli_parse_tests {
    use super::snapshot::SnapshotCommand;
    use super::{Cli, Command, ConfigCommand, OutputFormat};
    use clap::Parser;
    use std::path::PathBuf;

    #[test]
    fn global_output_json_and_version_subcommand() {
        let cli = Cli::try_parse_from(["voidbox", "--output", "json", "version"]).unwrap();
        assert_eq!(cli.output, OutputFormat::Json);
        assert!(matches!(cli.command, Command::Version));
    }

    #[test]
    fn no_banner_flag_parses() {
        let cli = Cli::try_parse_from(["voidbox", "--no-banner", "version"]).unwrap();
        assert!(cli.no_banner);
    }

    #[test]
    fn validate_requires_file() {
        let cli = Cli::try_parse_from(["voidbox", "validate", "--file", "spec.yaml"]).unwrap();
        match cli.command {
            Command::Validate { file } => assert_eq!(file, PathBuf::from("spec.yaml")),
            _ => panic!("expected Validate"),
        }
    }

    #[test]
    fn run_without_file_errors() {
        assert!(Cli::try_parse_from(["voidbox", "run"]).is_err());
    }

    #[test]
    fn run_file_and_optional_input() {
        let cli = Cli::try_parse_from([
            "voidbox",
            "run",
            "--file",
            "workflow.yaml",
            "--input",
            "hello world",
        ])
        .unwrap();
        match cli.command {
            Command::Run { file, input } => {
                assert_eq!(file, PathBuf::from("workflow.yaml"));
                assert_eq!(input.as_deref(), Some("hello world"));
            }
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn exec_program_and_trailing_args() {
        let cli = Cli::try_parse_from(["voidbox", "exec", "echo", "a", "b"]).unwrap();
        match cli.command {
            Command::Exec { program, args } => {
                assert_eq!(program, "echo");
                assert_eq!(args, vec!["a".to_string(), "b".to_string()]);
            }
            _ => panic!("expected Exec"),
        }
    }

    #[test]
    fn inspect_file() {
        let cli = Cli::try_parse_from(["voidbox", "inspect", "--file", "spec.yaml"]).unwrap();
        match cli.command {
            Command::Inspect { file } => assert_eq!(file, PathBuf::from("spec.yaml")),
            _ => panic!("expected Inspect"),
        }
    }

    #[test]
    fn skills_file() {
        let cli = Cli::try_parse_from(["voidbox", "skills", "--file", "spec.yaml"]).unwrap();
        match cli.command {
            Command::Skills { file } => assert_eq!(file, PathBuf::from("spec.yaml")),
            _ => panic!("expected Skills"),
        }
    }

    #[test]
    fn logs_run_id_and_daemon() {
        let cli = Cli::try_parse_from([
            "voidbox",
            "logs",
            "--run-id",
            "run-9",
            "--daemon",
            "http://127.0.0.1:43100",
        ])
        .unwrap();
        match cli.command {
            Command::Logs { run_id, daemon } => {
                assert_eq!(run_id, "run-9");
                assert_eq!(daemon.as_deref(), Some("http://127.0.0.1:43100"));
            }
            _ => panic!("expected Logs"),
        }
    }

    #[test]
    fn tui_defaults_and_overrides() {
        let cli = Cli::try_parse_from(["voidbox", "tui"]).unwrap();
        match cli.command {
            Command::Tui {
                file,
                daemon,
                session,
                logo_ascii,
            } => {
                assert!(file.is_none());
                assert!(daemon.is_none());
                assert_eq!(session, "default");
                assert!(logo_ascii.is_none());
            }
            _ => panic!("expected Tui"),
        }

        let cli = Cli::try_parse_from([
            "voidbox",
            "tui",
            "--file",
            "x.yaml",
            "--daemon",
            "http://example:43100",
            "--session",
            "sess-1",
            "--logo-ascii",
            "/tmp/logo.txt",
        ])
        .unwrap();
        match cli.command {
            Command::Tui {
                file,
                daemon,
                session,
                logo_ascii,
            } => {
                assert_eq!(file.as_deref(), Some("x.yaml"));
                assert_eq!(daemon.as_deref(), Some("http://example:43100"));
                assert_eq!(session, "sess-1");
                assert_eq!(logo_ascii.as_deref(), Some("/tmp/logo.txt"));
            }
            _ => panic!("expected Tui"),
        }
    }

    #[test]
    fn snapshot_create_flags() {
        let cli = Cli::try_parse_from([
            "voidbox",
            "snapshot",
            "create",
            "--kernel",
            "/boot/vmlinuz",
            "--initramfs",
            "/tmp/init.cpio.gz",
            "--memory",
            "256",
            "--vcpus",
            "2",
            "--diff",
        ])
        .unwrap();
        match cli.command {
            Command::Snapshot { command } => match command {
                SnapshotCommand::Create {
                    kernel,
                    initramfs,
                    memory,
                    vcpus,
                    diff,
                } => {
                    assert_eq!(kernel, PathBuf::from("/boot/vmlinuz"));
                    assert_eq!(initramfs, Some(PathBuf::from("/tmp/init.cpio.gz")));
                    assert_eq!(memory, 256);
                    assert_eq!(vcpus, 2);
                    assert!(diff);
                }
                _ => panic!("expected Create"),
            },
            _ => panic!("expected Snapshot"),
        }

        let cli = Cli::try_parse_from(["voidbox", "snapshot", "create", "--kernel", "/k/vmlinux"])
            .unwrap();
        match cli.command {
            Command::Snapshot { command } => match command {
                SnapshotCommand::Create {
                    kernel,
                    initramfs,
                    memory,
                    vcpus,
                    diff,
                } => {
                    assert_eq!(kernel, PathBuf::from("/k/vmlinux"));
                    assert!(initramfs.is_none());
                    assert_eq!(memory, 512);
                    assert_eq!(vcpus, 1);
                    assert!(!diff);
                }
                _ => panic!("expected Create"),
            },
            _ => panic!("expected Snapshot"),
        }
    }

    #[test]
    fn config_init_subcommand() {
        let cli = Cli::try_parse_from(["voidbox", "config", "init"]).unwrap();
        match cli.command {
            Command::Config { command } => assert!(matches!(command, ConfigCommand::Init)),
            _ => panic!("expected Config"),
        }
    }

    #[test]
    fn global_log_level_override() {
        let cli = Cli::try_parse_from(["voidbox", "--log-level", "debug", "version"]).unwrap();
        assert_eq!(cli.log_level.as_deref(), Some("debug"));
    }

    #[test]
    fn global_log_dir_override() {
        let cli =
            Cli::try_parse_from(["voidbox", "--log-dir", "/tmp/voidbox-log", "version"]).unwrap();
        assert_eq!(
            cli.log_dir.as_deref(),
            Some(std::path::Path::new("/tmp/voidbox-log"))
        );
    }

    #[test]
    fn snapshot_list() {
        let cli = Cli::try_parse_from(["voidbox", "snapshot", "list"]).unwrap();
        match cli.command {
            Command::Snapshot { command } => {
                assert!(matches!(command, SnapshotCommand::List));
            }
            _ => panic!("expected Snapshot"),
        }
    }

    #[test]
    fn snapshot_delete_hash_prefix() {
        let cli = Cli::try_parse_from(["voidbox", "snapshot", "delete", "abc12"]).unwrap();
        match cli.command {
            Command::Snapshot { command } => match command {
                SnapshotCommand::Delete { hash_prefix } => assert_eq!(hash_prefix, "abc12"),
                _ => panic!("expected Delete"),
            },
            _ => panic!("expected Snapshot"),
        }
    }

    #[test]
    fn serve_hidden_listen_override() {
        let cli = Cli::try_parse_from(["voidbox", "serve", "--listen", "127.0.0.1:9999"]).unwrap();
        match cli.command {
            Command::Serve { listen } => assert_eq!(listen, "127.0.0.1:9999"),
            _ => panic!("expected Serve"),
        }
    }

    #[test]
    fn config_validate_subcommand() {
        let cli = Cli::try_parse_from(["voidbox", "config", "validate"]).unwrap();
        match cli.command {
            Command::Config { command } => {
                assert!(matches!(command, ConfigCommand::Validate));
            }
            _ => panic!("expected Config"),
        }
    }

    #[test]
    fn status_run_id_and_daemon() {
        let cli = Cli::try_parse_from([
            "voidbox",
            "status",
            "--run-id",
            "run-1",
            "--daemon",
            "http://127.0.0.1:43100",
        ])
        .unwrap();
        match cli.command {
            Command::Status { run_id, daemon } => {
                assert_eq!(run_id, "run-1");
                assert_eq!(daemon.as_deref(), Some("http://127.0.0.1:43100"));
            }
            _ => panic!("expected Status"),
        }
    }
}

#[cfg(test)]
mod tracing_tests {
    use super::*;

    #[test]
    fn tracing_plan_uses_file_only_for_interactive_mode() {
        let plan = tracing_plan(
            TracingMode::InteractivePty,
            Path::new("/tmp/voidbox-log"),
            true,
        );
        assert_eq!(plan.writer_mode, TracingWriterMode::FileOnly);
        assert!(plan
            .notice
            .unwrap()
            .contains("interactive mode: runtime logs"));
    }

    #[test]
    fn tracing_plan_uses_sink_with_warning_when_interactive_file_logging_unavailable() {
        let plan = tracing_plan(
            TracingMode::InteractivePty,
            Path::new("/tmp/voidbox-log"),
            false,
        );
        assert_eq!(plan.writer_mode, TracingWriterMode::SinkOnly);
        assert!(plan
            .notice
            .unwrap()
            .contains("runtime logs will be suppressed"));
    }

    #[test]
    fn tracing_plan_keeps_stderr_and_file_for_standard_mode() {
        let plan = tracing_plan(TracingMode::Standard, Path::new("/tmp/voidbox-log"), true);
        assert_eq!(plan.writer_mode, TracingWriterMode::StderrAndFile);
        assert!(plan.notice.is_none());
    }
}

// ---------------------------------------------------------------------------
// Behavioral tests — command execution, routing, error handling, output
// ---------------------------------------------------------------------------

#[cfg(test)]
mod behavior_tests {
    use super::*;
    use cli_config::{CliPaths, ResolvedConfig};
    use output::OutputFormat;
    use std::path::PathBuf;

    /// Minimal `ResolvedConfig` pointing at an isolated temp directory.
    fn fake_config(tmp: &std::path::Path) -> ResolvedConfig {
        fake_config_with_daemon(tmp, "http://127.0.0.1:43100")
    }

    fn fake_config_with_daemon(tmp: &std::path::Path, daemon_url: &str) -> ResolvedConfig {
        ResolvedConfig {
            log_level: "info".into(),
            daemon_url: daemon_url.into(),
            banner: false,
            paths: CliPaths {
                state_dir: tmp.join("state"),
                log_dir: tmp.join("log"),
                snapshot_dir: tmp.join("snapshots"),
                config_dir: tmp.join("config"),
            },
            kernel: None,
            initramfs: None,
        }
    }

    /// Run a command through the full `run()` dispatcher and return the exit code.
    async fn run_command(
        command: Command,
        format: OutputFormat,
    ) -> Result<i32, Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir().unwrap();
        let config = fake_config(tmp.path());
        std::fs::create_dir_all(&config.paths.snapshot_dir).unwrap();
        let remote = backend::RemoteBackend::new(config.daemon_url.clone());
        let cli = Cli {
            output: format,
            no_banner: true,
            log_level: None,
            log_dir: None,
            command,
        };
        run(cli, &config, &remote).await
    }

    /// Create an isolated snapshot dir inside a new tempdir.
    fn isolated_snapshot_dir() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let snap_dir = tmp.path().join("snapshots");
        std::fs::create_dir_all(&snap_dir).unwrap();
        (tmp, snap_dir)
    }

    // -----------------------------------------------------------------------
    // resolved_daemon_url
    // -----------------------------------------------------------------------

    #[test]
    fn resolved_daemon_url_prefers_cli_override() {
        let tmp = tempfile::tempdir().unwrap();
        let config = fake_config_with_daemon(tmp.path(), "http://config-host:1000");

        let cases: Vec<(Command, &str)> = vec![
            (
                Command::Status {
                    run_id: "r1".into(),
                    daemon: Some("http://cli-override:2000".into()),
                },
                "http://cli-override:2000",
            ),
            (
                Command::Logs {
                    run_id: "r1".into(),
                    daemon: Some("http://cli-logs:3000".into()),
                },
                "http://cli-logs:3000",
            ),
            (
                Command::Tui {
                    file: None,
                    daemon: Some("http://cli-tui:4000".into()),
                    session: "s".into(),
                    logo_ascii: None,
                },
                "http://cli-tui:4000",
            ),
        ];

        for (cmd, expected) in &cases {
            assert_eq!(
                resolved_daemon_url(cmd, &config),
                *expected,
                "CLI --daemon should override config for {:?}",
                cmd
            );
        }
    }

    #[test]
    fn resolved_daemon_url_falls_back_to_config() {
        let tmp = tempfile::tempdir().unwrap();
        let config = fake_config_with_daemon(tmp.path(), "http://config-host:1000");

        let remote_commands_without_override: Vec<Command> = vec![
            Command::Status {
                run_id: "r1".into(),
                daemon: None,
            },
            Command::Logs {
                run_id: "r1".into(),
                daemon: None,
            },
            Command::Tui {
                file: None,
                daemon: None,
                session: "s".into(),
                logo_ascii: None,
            },
        ];

        for cmd in &remote_commands_without_override {
            assert_eq!(
                resolved_daemon_url(cmd, &config),
                "http://config-host:1000",
                "remote command {:?} without --daemon should fall back to config",
                cmd
            );
        }
    }

    #[test]
    fn resolved_daemon_url_non_remote_command_uses_config() {
        let tmp = tempfile::tempdir().unwrap();
        let config = fake_config_with_daemon(tmp.path(), "http://config-host:1000");

        let non_remote_commands: Vec<Command> = vec![
            Command::Version,
            Command::Config {
                command: ConfigCommand::Validate,
            },
            Command::Validate {
                file: PathBuf::from("x.yaml"),
            },
            Command::Inspect {
                file: PathBuf::from("x.yaml"),
            },
            Command::Skills {
                file: PathBuf::from("x.yaml"),
            },
            Command::Run {
                file: PathBuf::from("x.yaml"),
                input: None,
            },
            Command::Exec {
                program: "echo".into(),
                args: vec![],
            },
            Command::Serve {
                listen: "127.0.0.1:9999".into(),
            },
            Command::Snapshot {
                command: snapshot::SnapshotCommand::List,
            },
        ];

        for cmd in &non_remote_commands {
            assert_eq!(
                resolved_daemon_url(cmd, &config),
                "http://config-host:1000",
                "non-remote command {:?} should use config daemon_url",
                cmd
            );
        }
    }

    // -----------------------------------------------------------------------
    // run() command routing
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn run_version_returns_zero() {
        assert_eq!(
            run_command(Command::Version, OutputFormat::Human)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn run_version_json_returns_zero() {
        assert_eq!(
            run_command(Command::Version, OutputFormat::Json)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn run_config_validate_returns_zero() {
        let cmd = Command::Config {
            command: ConfigCommand::Validate,
        };
        assert_eq!(run_command(cmd, OutputFormat::Human).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn run_snapshot_list_returns_ok() {
        let cmd = Command::Snapshot {
            command: snapshot::SnapshotCommand::List,
        };
        assert_eq!(run_command(cmd, OutputFormat::Human).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn run_snapshot_list_json_returns_ok() {
        let cmd = Command::Snapshot {
            command: snapshot::SnapshotCommand::List,
        };
        assert_eq!(run_command(cmd, OutputFormat::Json).await.unwrap(), 0);
    }

    // -----------------------------------------------------------------------
    // run() failure paths
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn run_validate_nonexistent_file_returns_error() {
        let cmd = Command::Validate {
            file: PathBuf::from("nonexistent.yaml"),
        };
        assert!(run_command(cmd, OutputFormat::Human).await.is_err());
    }

    #[tokio::test]
    async fn run_inspect_nonexistent_file_returns_error() {
        let cmd = Command::Inspect {
            file: PathBuf::from("nonexistent.yaml"),
        };
        assert!(run_command(cmd, OutputFormat::Human).await.is_err());
    }

    #[tokio::test]
    async fn run_skills_nonexistent_file_returns_error() {
        let cmd = Command::Skills {
            file: PathBuf::from("nonexistent.yaml"),
        };
        assert!(run_command(cmd, OutputFormat::Human).await.is_err());
    }

    #[tokio::test]
    async fn run_run_nonexistent_file_returns_error() {
        let cmd = Command::Run {
            file: PathBuf::from("nonexistent.yaml"),
            input: None,
        };
        assert!(run_command(cmd, OutputFormat::Human).await.is_err());
    }

    #[tokio::test]
    async fn run_validate_invalid_yaml_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let bad_file = tmp.path().join("bad.yaml");
        std::fs::write(&bad_file, "not: [valid: spec").unwrap();

        let cmd = Command::Validate { file: bad_file };
        assert!(run_command(cmd, OutputFormat::Human).await.is_err());
    }

    #[tokio::test]
    async fn run_validate_json_error_is_propagated() {
        let cmd = Command::Validate {
            file: PathBuf::from("nonexistent.yaml"),
        };
        assert!(run_command(cmd, OutputFormat::Json).await.is_err());
    }

    #[tokio::test]
    async fn run_snapshot_delete_nonexistent_returns_error() {
        let cmd = Command::Snapshot {
            command: snapshot::SnapshotCommand::Delete {
                hash_prefix: "nonexistent".into(),
            },
        };
        assert!(run_command(cmd, OutputFormat::Human).await.is_err());
    }

    #[tokio::test]
    async fn run_config_init_twice_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let config = fake_config(tmp.path());
        std::fs::create_dir_all(&config.paths.snapshot_dir).unwrap();
        let remote = backend::RemoteBackend::new(config.daemon_url.clone());

        let cli = Cli {
            output: OutputFormat::Human,
            no_banner: true,
            log_level: None,
            log_dir: None,
            command: Command::Config {
                command: ConfigCommand::Init,
            },
        };
        run(cli, &config, &remote).await.unwrap();

        let cli2 = Cli {
            output: OutputFormat::Human,
            no_banner: true,
            log_level: None,
            log_dir: None,
            command: Command::Config {
                command: ConfigCommand::Init,
            },
        };
        assert!(run(cli2, &config, &remote).await.is_err());
    }

    // -----------------------------------------------------------------------
    // cmd_config behavior
    // -----------------------------------------------------------------------

    #[test]
    fn cmd_config_init_fails_if_file_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let config = fake_config(tmp.path());

        let config_file = config.paths.config_dir.join("config.yaml");
        std::fs::create_dir_all(&config.paths.config_dir).unwrap();
        std::fs::write(&config_file, "existing: true").unwrap();

        let result = cmd_config(ConfigCommand::Init, OutputFormat::Human, &config);
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("already exists"),
            "expected 'already exists' in error, got: {msg}"
        );
    }

    #[test]
    fn cmd_config_init_succeeds_when_no_file() {
        let tmp = tempfile::tempdir().unwrap();
        let config = fake_config(tmp.path());

        cmd_config(ConfigCommand::Init, OutputFormat::Human, &config).unwrap();

        let config_file = config.paths.config_dir.join("config.yaml");
        assert!(
            config_file.exists(),
            "template file should have been written"
        );
    }

    #[test]
    fn cmd_config_init_written_file_is_valid_yaml() {
        let tmp = tempfile::tempdir().unwrap();
        let config = fake_config(tmp.path());

        cmd_config(ConfigCommand::Init, OutputFormat::Human, &config).unwrap();

        let config_file = config.paths.config_dir.join("config.yaml");
        let contents = std::fs::read_to_string(&config_file).unwrap();
        let parsed: serde_yaml::Value = serde_yaml::from_str(&contents).unwrap();
        assert!(parsed.is_mapping());
    }

    #[test]
    fn cmd_config_validate_returns_ok_human() {
        let tmp = tempfile::tempdir().unwrap();
        let config = fake_config(tmp.path());
        cmd_config(ConfigCommand::Validate, OutputFormat::Human, &config).unwrap();
    }

    #[test]
    fn cmd_config_validate_returns_ok_json() {
        let tmp = tempfile::tempdir().unwrap();
        let config = fake_config(tmp.path());
        cmd_config(ConfigCommand::Validate, OutputFormat::Json, &config).unwrap();
    }

    // -----------------------------------------------------------------------
    // cmd_version output modes
    // -----------------------------------------------------------------------

    #[test]
    fn cmd_version_human_returns_ok() {
        cmd_version(OutputFormat::Human).unwrap();
    }

    #[test]
    fn cmd_version_json_returns_ok() {
        cmd_version(OutputFormat::Json).unwrap();
    }

    // -----------------------------------------------------------------------
    // snapshot::handle — list on empty dir
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn snapshot_handle_list_empty_dir() {
        let (_tmp, snap_dir) = isolated_snapshot_dir();
        snapshot::handle(
            snapshot::SnapshotCommand::List,
            OutputFormat::Human,
            &snap_dir,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn snapshot_handle_list_json_empty_dir() {
        let (_tmp, snap_dir) = isolated_snapshot_dir();
        snapshot::handle(
            snapshot::SnapshotCommand::List,
            OutputFormat::Json,
            &snap_dir,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn snapshot_handle_delete_nonexistent_returns_error() {
        let (_tmp, snap_dir) = isolated_snapshot_dir();
        let result = snapshot::handle(
            snapshot::SnapshotCommand::Delete {
                hash_prefix: "nonexistent".into(),
            },
            OutputFormat::Human,
            &snap_dir,
        )
        .await;
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // Mock sandbox exec (exercises the same path as cmd_exec's mock branch)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn mock_sandbox_echo_returns_zero() {
        let sandbox = void_box::sandbox::Sandbox::mock().build().unwrap();
        let out = sandbox.exec("echo", &["hello"]).await.unwrap();
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.stdout_str(), "hello\n");
    }

    #[tokio::test]
    async fn mock_sandbox_cat_missing_file_returns_nonzero() {
        let sandbox = void_box::sandbox::Sandbox::mock().build().unwrap();
        let out = sandbox.exec("cat", &["missing.txt"]).await.unwrap();
        assert_ne!(
            out.exit_code, 0,
            "cat with a missing file should return non-zero"
        );
    }

    #[tokio::test]
    async fn mock_sandbox_unknown_program_returns_zero() {
        let sandbox = void_box::sandbox::Sandbox::mock().build().unwrap();
        let out = sandbox.exec("__nonexistent__", &[]).await.unwrap();
        assert_eq!(
            out.exit_code, 0,
            "mock sandbox returns 0 for unknown programs"
        );
    }

    // -----------------------------------------------------------------------
    // parse_tui_command
    // -----------------------------------------------------------------------

    #[test]
    fn tui_parse_quit_variants() {
        assert_eq!(parse_tui_command("/quit"), TuiCommand::Quit);
        assert_eq!(parse_tui_command("/exit"), TuiCommand::Quit);
        assert_eq!(parse_tui_command("  /quit  "), TuiCommand::Quit);
    }

    #[test]
    fn tui_parse_help() {
        assert_eq!(parse_tui_command("/help"), TuiCommand::Help);
    }

    #[test]
    fn tui_parse_run_extracts_file() {
        assert_eq!(
            parse_tui_command("/run spec.yaml"),
            TuiCommand::Run("spec.yaml")
        );
        assert_eq!(
            parse_tui_command("/run  extra-spaces.yaml "),
            TuiCommand::Run("extra-spaces.yaml")
        );
    }

    #[test]
    fn tui_parse_input_preserves_text() {
        assert_eq!(
            parse_tui_command("/input hello world"),
            TuiCommand::Input("hello world")
        );
    }

    #[test]
    fn tui_parse_status_logs_cancel_history() {
        assert_eq!(parse_tui_command("/status"), TuiCommand::Status);
        assert_eq!(parse_tui_command("/logs"), TuiCommand::Logs);
        assert_eq!(parse_tui_command("/cancel"), TuiCommand::Cancel);
        assert_eq!(parse_tui_command("/history"), TuiCommand::History);
    }

    #[test]
    fn tui_parse_unknown_command() {
        assert_eq!(parse_tui_command("random text"), TuiCommand::Unknown);
        assert_eq!(parse_tui_command("/unknown"), TuiCommand::Unknown);
    }

    #[test]
    fn tui_parse_empty_run_is_unknown() {
        // "/run" without a space+argument is not a valid /run command
        assert_eq!(parse_tui_command("/run"), TuiCommand::Unknown);
    }

    // -----------------------------------------------------------------------
    // snapshot create — early failure paths
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn snapshot_create_nonexistent_kernel_returns_error() {
        let cmd = snapshot::SnapshotCommand::Create {
            kernel: PathBuf::from("/nonexistent/kernel"),
            initramfs: None,
            memory: 128,
            vcpus: 1,
            diff: false,
        };
        let (_tmp, snap_dir) = isolated_snapshot_dir();
        let result = snapshot::handle(cmd, OutputFormat::Human, &snap_dir).await;
        assert!(result.is_err(), "nonexistent kernel should fail early");
    }

    #[tokio::test]
    async fn snapshot_create_nonexistent_initramfs_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let fake_kernel = tmp.path().join("vmlinuz");
        std::fs::write(&fake_kernel, b"fake-kernel-data").unwrap();

        let cmd = snapshot::SnapshotCommand::Create {
            kernel: fake_kernel,
            initramfs: Some(PathBuf::from("/nonexistent/initramfs")),
            memory: 128,
            vcpus: 1,
            diff: false,
        };
        let (_tmp2, snap_dir) = isolated_snapshot_dir();
        let result = snapshot::handle(cmd, OutputFormat::Human, &snap_dir).await;
        assert!(result.is_err(), "nonexistent initramfs should fail early");
    }
}
