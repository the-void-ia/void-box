//! CLI handlers for `voidbox attach` and `voidbox shell`.

use std::path::{Path, PathBuf};

use void_box::backend::pty_session::RawModeGuard;
use void_box::backend::MountConfig;
use void_box::credentials::{discover_oauth_credentials, stage_credentials, StagedCredentials};
use void_box::llm::LlmProvider;
use void_box::spec::{self, RunKind, RunSpec, SandboxSpec};
use void_box_protocol::PtyOpenRequest;

/// Attaches to a running VM by run ID (not yet implemented).
pub async fn cmd_attach(
    _run_id: &str,
    _program: Option<&str>,
    _args: &[String],
    _working_dir: Option<&str>,
    _daemon_url: &str,
) -> Result<i32, Box<dyn std::error::Error>> {
    Err("attach to a running VM is not yet implemented".into())
}

/// Options for the `voidbox shell` command.
pub struct ShellOpts<'a> {
    /// Spec file (optional; generates ephemeral spec if omitted).
    pub file: Option<&'a Path>,
    /// Program to run in the PTY.
    pub program: &'a str,
    /// Arguments to the program.
    pub args: &'a [String],
    /// Working directory inside the guest.
    pub working_dir: Option<&'a str>,
    /// Guest memory in MB (used when no spec file).
    pub memory_mb: usize,
    /// Enable guest networking (used when no spec file).
    pub network: bool,
    /// LLM provider override.
    pub provider: Option<&'a str>,
    /// Restore from snapshot.
    pub snapshot: Option<&'a str>,
    /// Mount flags (HOST:GUEST[:ro|rw]).
    pub mounts: &'a [String],
    /// Env flags (KEY=VALUE).
    pub env_vars: &'a [String],
}

/// Boots a VM from a spec file or ephemeral config and attaches an interactive PTY.
///
/// # Errors
///
/// Returns an error if the spec is invalid, credentials cannot be staged, or
/// the sandbox fails to start.
pub async fn cmd_shell(opts: ShellOpts<'_>) -> Result<i32, Box<dyn std::error::Error>> {
    let spec = match opts.file {
        Some(path) => {
            let s = spec::load_spec(path)?;
            spec::validate_spec(&s)?;
            s
        }
        None => build_ephemeral_spec(opts.memory_mb, opts.network),
    };

    let provider = resolve_provider(opts.provider, opts.file.is_some(), spec.llm.as_ref())?;

    let staged_creds = prepare_credentials(&provider)?;

    let kernel = spec
        .sandbox
        .kernel
        .clone()
        .or_else(|| std::env::var("VOID_BOX_KERNEL").ok())
        .ok_or("VOID_BOX_KERNEL not set and no kernel in spec")?;
    let initramfs = spec
        .sandbox
        .initramfs
        .clone()
        .or_else(|| std::env::var("VOID_BOX_INITRAMFS").ok());

    let effective_memory = if opts.file.is_some() {
        spec.sandbox.memory_mb
    } else {
        opts.memory_mb
    };
    let effective_network = if opts.file.is_some() {
        spec.sandbox.network
    } else {
        opts.network
    };

    let mut builder = void_box::sandbox::Sandbox::local()
        .kernel(&kernel)
        .memory_mb(effective_memory)
        .network(effective_network);

    if let Some(path) = &initramfs {
        builder = builder.initramfs(path);
    }

    if let Some(snap) = opts.snapshot {
        builder = builder.snapshot(snap);
    } else if let Some(ref snap) = spec.sandbox.snapshot {
        builder = builder.snapshot(snap);
    }

    for ms in &spec.sandbox.mounts {
        builder = builder.mount(MountConfig {
            host_path: ms.host.clone(),
            guest_path: ms.guest.clone(),
            read_only: ms.mode == "ro",
        });
    }

    for raw in opts.mounts {
        builder = builder.mount(parse_mount_flag(raw)?);
    }

    if let Some(ref creds) = staged_creds {
        builder = builder.mount(MountConfig {
            host_path: creds.host_path.clone(),
            guest_path: "/home/sandbox/.claude".into(),
            read_only: false,
        });
    }

    for (key, value) in &spec.sandbox.env {
        builder = builder.env(key, value);
    }

    for raw in opts.env_vars {
        let (k, v) = parse_env_flag(raw)?;
        builder = builder.env(k, v);
    }

    let sandbox = builder.build()?;

    let program_base = match Path::new(opts.program).file_name() {
        Some(name) => name.to_str().unwrap_or(opts.program),
        None => opts.program,
    };
    if program_base == "claude" || program_base == "claude-code" {
        let onboarding = r#"{"hasCompletedOnboarding":true}"#;
        let _ = sandbox
            .write_file("/home/sandbox/.claude.json", onboarding.as_bytes())
            .await;
    }

    let mut pty_env: Vec<(String, String)> = provider.env_vars();
    for raw in opts.env_vars {
        let (k, v) = parse_env_flag(raw)?;
        pty_env.push((k.to_string(), v.to_string()));
    }

    let (cols, rows) = terminal_size()?;

    let request = PtyOpenRequest {
        cols,
        rows,
        program: opts.program.to_string(),
        args: opts.args.to_vec(),
        env: pty_env,
        working_dir: opts.working_dir.map(String::from),
    };

    let session = sandbox.attach_pty(request).await?;

    let guard = RawModeGuard::engage(0).map_err(|e| format!("failed to enter raw mode: {e}"))?;

    let exit_code = tokio::task::spawn_blocking(move || session.run())
        .await
        .map_err(|e| format!("pty task panicked: {e}"))??;

    drop(guard);
    let _ = sandbox.stop().await;

    drop(staged_creds);

    Ok(exit_code)
}

/// Builds a minimal ephemeral `RunSpec` with `kind: sandbox`.
fn build_ephemeral_spec(memory_mb: usize, network: bool) -> RunSpec {
    RunSpec {
        api_version: "v1".into(),
        kind: RunKind::Sandbox,
        name: "shell".into(),
        sandbox: SandboxSpec {
            mode: "interactive".into(),
            kernel: None,
            initramfs: None,
            memory_mb,
            vcpus: 1,
            network,
            env: Default::default(),
            mounts: Vec::new(),
            image: None,
            guest_image: None,
            snapshot: None,
        },
        llm: None,
        observe: None,
        agent: None,
        pipeline: None,
        workflow: None,
    }
}

/// Resolves the LLM provider from the flag, spec, or environment auto-detection.
///
/// # Errors
///
/// Returns an error if no provider can be determined and no API key is available.
fn resolve_provider(
    flag: Option<&str>,
    has_file: bool,
    llm_spec: Option<&spec::LlmSpec>,
) -> Result<LlmProvider, Box<dyn std::error::Error>> {
    if let Some(name) = flag {
        return Ok(provider_from_name(name));
    }

    if has_file {
        if let Some(llm) = llm_spec {
            return Ok(provider_from_name(&llm.provider));
        }
    }

    if std::env::var("ANTHROPIC_API_KEY").is_ok() {
        return Ok(LlmProvider::Claude);
    }

    let home = std::env::var("HOME").unwrap_or_default();
    let creds_path = PathBuf::from(&home).join(".claude/.credentials.json");
    if creds_path.exists() {
        return Ok(LlmProvider::ClaudePersonal);
    }

    Err("no LLM provider detected: set ANTHROPIC_API_KEY or run `claude auth login`".into())
}

/// Maps a provider name string to an `LlmProvider` variant.
fn provider_from_name(name: &str) -> LlmProvider {
    match name.to_ascii_lowercase().as_str() {
        "claude" => LlmProvider::Claude,
        "claude-personal" => LlmProvider::ClaudePersonal,
        _ => LlmProvider::Claude,
    }
}

/// Stages OAuth credentials when using the `claude-personal` provider.
///
/// # Errors
///
/// Returns an error if credential discovery or staging fails.
fn prepare_credentials(
    provider: &LlmProvider,
) -> Result<Option<StagedCredentials>, Box<dyn std::error::Error>> {
    if !matches!(provider, LlmProvider::ClaudePersonal) {
        return Ok(None);
    }
    let json = discover_oauth_credentials()?;
    let staged = stage_credentials(&json)?;
    Ok(Some(staged))
}

/// Parses a `HOST:GUEST[:ro|rw]` mount flag into a `MountConfig`.
///
/// # Errors
///
/// Returns an error if the flag does not contain at least `HOST:GUEST`.
fn parse_mount_flag(raw: &str) -> Result<MountConfig, Box<dyn std::error::Error>> {
    let parts: Vec<&str> = raw.splitn(3, ':').collect();
    if parts.len() < 2 {
        return Err(format!("invalid mount flag: expected HOST:GUEST[:ro|rw], got '{raw}'").into());
    }
    let read_only = match parts.get(2) {
        Some(&"rw") => false,
        Some(&"ro") => true,
        None => true,
        Some(other) => {
            return Err(format!("invalid mount mode '{other}': expected 'ro' or 'rw'").into());
        }
    };
    Ok(MountConfig {
        host_path: parts[0].to_string(),
        guest_path: parts[1].to_string(),
        read_only,
    })
}

/// Parses a `KEY=VALUE` env flag.
///
/// # Errors
///
/// Returns an error if the flag does not contain `=`.
fn parse_env_flag(raw: &str) -> Result<(&str, &str), Box<dyn std::error::Error>> {
    let Some((key, value)) = raw.split_once('=') else {
        return Err(format!("invalid env flag: expected KEY=VALUE, got '{raw}'").into());
    };
    Ok((key, value))
}

/// Reads the current terminal size via ioctl.
fn terminal_size() -> Result<(u16, u16), Box<dyn std::error::Error>> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) };
    if ret < 0 {
        return Err(format!("TIOCGWINSZ failed: {}", std::io::Error::last_os_error()).into());
    }
    Ok((ws.ws_col, ws.ws_row))
}
