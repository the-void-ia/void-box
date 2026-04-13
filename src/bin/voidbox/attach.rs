//! CLI handlers for `voidbox attach` and `voidbox shell`.

use std::path::Path;

use rustix::termios::tcgetwinsize;
use tracing::info;
use void_box::backend::pty_session::RawModeGuard;
use void_box::backend::{GuestConsoleSink, MountConfig};
use void_box::credentials::{discover_oauth_credentials, stage_credentials, StagedCredentials};
use void_box::llm::LlmProvider;
use void_box::snapshot_store::{compute_config_hash, snapshot_dir_for_hash, snapshot_exists};
use void_box::spec::{self, RunKind, RunSpec, SandboxSpec};
use void_box_protocol::PtyOpenRequest;

const GUEST_CONSOLE_LOG_FILENAME: &str = "guest-console.log";

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
    /// Number of vCPUs (used when no spec file).
    pub vcpus: usize,
    /// Enable guest networking (used when no spec file).
    pub network: bool,
    /// LLM provider override.
    pub provider: Option<&'a str>,
    /// Restore from snapshot.
    pub snapshot: Option<&'a str>,
    /// Automatically snapshot the VM on exit.
    pub auto_snapshot: bool,
    /// Mount flags (HOST:GUEST[:ro|rw]).
    pub mounts: &'a [String],
    /// Env flags (KEY=VALUE).
    pub env_vars: &'a [String],
    /// Directory for interactive runtime logs.
    pub log_dir: &'a Path,
}

/// Boots a VM from a spec file or ephemeral config and attaches an interactive PTY.
///
/// # Errors
///
/// Returns an error if the spec is invalid, credentials cannot be staged, or
/// the sandbox fails to start.
pub async fn cmd_shell(opts: ShellOpts<'_>) -> Result<i32, Box<dyn std::error::Error>> {
    if opts.auto_snapshot && opts.snapshot.is_some() {
        return Err("--auto-snapshot and --snapshot are mutually exclusive".into());
    }

    let run_spec = match opts.file {
        Some(path) => {
            let loaded_spec = spec::load_spec(path)?;
            spec::validate_spec(&loaded_spec)?;
            loaded_spec
        }
        None => build_ephemeral_spec(opts.memory_mb, opts.vcpus, opts.network),
    };

    // Resolve kernel: spec → env var → installed paths → auto-download.
    let flavor = opts
        .provider
        .map(|p| match p.to_ascii_lowercase().as_str() {
            "codex" => "codex",
            _ => "claude",
        })
        .unwrap_or("claude");

    let (kernel, initramfs) = resolve_shell_images(&run_spec, flavor).await?;

    let effective_memory = if opts.file.is_some() {
        run_spec.sandbox.memory_mb
    } else {
        opts.memory_mb
    };
    let effective_vcpus = if opts.file.is_some() {
        run_spec.sandbox.vcpus
    } else {
        opts.vcpus
    };
    let effective_network = if opts.file.is_some() {
        run_spec.sandbox.network
    } else {
        opts.network
    };

    let mut builder = void_box::sandbox::Sandbox::local()
        .kernel(&kernel)
        .memory_mb(effective_memory)
        .vcpus(effective_vcpus)
        .network(effective_network)
        .guest_console(GuestConsoleSink::File(
            opts.log_dir.join(GUEST_CONSOLE_LOG_FILENAME),
        ));

    if let Some(path) = &initramfs {
        builder = builder.initramfs(path);
    }

    let mut auto_snapshot_pending = false;

    if opts.auto_snapshot {
        let config_hash = compute_config_hash(
            Path::new(&kernel),
            initramfs.as_deref().map(Path::new),
            effective_memory,
            effective_vcpus,
        )?;
        let snap_dir = snapshot_dir_for_hash(&config_hash);
        if snapshot_exists(&snap_dir) {
            info!("Auto-snapshot: restoring from {}", snap_dir.display());
            builder = builder.snapshot(&snap_dir);
        } else {
            info!(
                "Auto-snapshot: no snapshot for hash {}, will cold boot and save",
                &config_hash[..16]
            );
            auto_snapshot_pending = true;
        }
    } else if let Some(snapshot_path) = opts.snapshot {
        builder = builder.snapshot(snapshot_path);
    } else if let Some(ref snapshot_path) = run_spec.sandbox.snapshot {
        builder = builder.snapshot(snapshot_path);
    }

    for mount_spec in &run_spec.sandbox.mounts {
        builder = builder.mount(MountConfig {
            host_path: mount_spec.host.clone(),
            guest_path: mount_spec.guest.clone(),
            read_only: mount_spec.mode == "ro",
        });
    }

    for raw in opts.mounts {
        builder = builder.mount(parse_mount_flag(raw)?);
    }

    // Credentials are written via write_file after boot (not mounted),
    // because the VMM only supports one 9p device at a time.

    for (key, value) in &run_spec.sandbox.env {
        builder = builder.env(key, value);
    }

    for raw in opts.env_vars {
        let (key, value) = parse_env_flag(raw)?;
        builder = builder.env(key, value);
    }

    let sandbox = builder.build()?;

    if auto_snapshot_pending {
        let config_hash = compute_config_hash(
            Path::new(&kernel),
            initramfs.as_deref().map(Path::new),
            effective_memory,
            effective_vcpus,
        )?;
        let snap_dir = snapshot_dir_for_hash(&config_hash);
        std::fs::create_dir_all(&snap_dir)?;
        sandbox.create_auto_snapshot(&snap_dir, config_hash).await?;
        info!("Auto-snapshot: saved for next run");
    }

    let program_base = match Path::new(opts.program).file_name() {
        Some(name) => name.to_str().unwrap_or(opts.program),
        None => opts.program,
    };
    let provider = resolve_provider(
        program_base,
        opts.provider,
        opts.file.is_some(),
        run_spec.llm.as_ref(),
    )?;
    let staged_creds = prepare_credentials(provider.as_ref())?;
    if program_base == "claude" || program_base == "claude-code" {
        let onboarding = r#"{"hasCompletedOnboarding":true}"#;
        let _ = sandbox
            .write_file("/home/sandbox/.claude.json", onboarding.as_bytes())
            .await;

        if let Some(ref creds) = staged_creds {
            let creds_path = std::path::PathBuf::from(&creds.host_path).join(".credentials.json");
            if let Ok(credentials_bytes) = std::fs::read(&creds_path) {
                let _ = sandbox.mkdir_p("/home/sandbox/.claude").await;
                let _ = sandbox
                    .write_file(
                        "/home/sandbox/.claude/.credentials.json",
                        &credentials_bytes,
                    )
                    .await;
            }
        }
    }

    let mut pty_env: Vec<(String, String)> = provider
        .as_ref()
        .map(LlmProvider::env_vars)
        .unwrap_or_default();
    for raw in opts.env_vars {
        let (key, value) = parse_env_flag(raw)?;
        pty_env.push((key.to_string(), value.to_string()));
    }

    let (cols, rows) = terminal_size()?;

    let request = PtyOpenRequest {
        cols,
        rows,
        program: opts.program.to_string(),
        args: opts.args.to_vec(),
        env: pty_env,
        working_dir: opts.working_dir.map(String::from),
        interactive: true,
    };

    let pty_result = async {
        let session = sandbox.attach_pty(request).await?;
        let guard =
            RawModeGuard::engage(0).map_err(|e| format!("failed to enter raw mode: {e}"))?;
        let exit_code = tokio::task::spawn_blocking(move || session.run())
            .await
            .map_err(|e| format!("pty task panicked: {e}"))??;
        drop(guard);
        Ok::<i32, Box<dyn std::error::Error>>(exit_code)
    }
    .await;

    let _ = sandbox.stop().await;
    drop(staged_creds);

    pty_result
}

/// Resolve kernel and initramfs for `voidbox shell`, using the same
/// fallback chain as `voidbox run`: spec → env var → installed paths →
/// auto-download from GitHub Releases.
async fn resolve_shell_images(
    spec: &RunSpec,
    flavor: &str,
) -> Result<(String, Option<String>), Box<dyn std::error::Error>> {
    // 1. Spec field or env var
    let kernel_explicit = spec
        .sandbox
        .kernel
        .clone()
        .or_else(|| std::env::var("VOID_BOX_KERNEL").ok());
    let initramfs_explicit = spec
        .sandbox
        .initramfs
        .clone()
        .or_else(|| std::env::var("VOID_BOX_INITRAMFS").ok());

    if let Some(ref k) = kernel_explicit {
        if std::path::Path::new(k).exists() {
            return Ok((k.clone(), initramfs_explicit));
        }
    }

    // 2. Well-known installed paths
    if let Some(installed) = void_box::image::resolve_installed_artifacts() {
        return Ok((
            installed.kernel.display().to_string(),
            Some(installed.initramfs.display().to_string()),
        ));
    }

    // 3. Auto-download from GitHub Releases
    let cache_root = void_box::image::default_cache_root()?;
    let kernel_path = void_box::image::resolve_kernel(
        kernel_explicit.as_deref().map(std::path::Path::new),
        &cache_root,
    )
    .await?;
    let initramfs_path = void_box::image::resolve_initramfs(
        initramfs_explicit.as_deref().map(std::path::Path::new),
        flavor,
        &cache_root,
    )
    .await?;

    Ok((
        kernel_path.display().to_string(),
        Some(initramfs_path.display().to_string()),
    ))
}

/// Builds a minimal ephemeral `RunSpec` with `kind: sandbox`.
fn build_ephemeral_spec(memory_mb: usize, vcpus: usize, network: bool) -> RunSpec {
    RunSpec {
        api_version: "v1".into(),
        kind: RunKind::Sandbox,
        name: "shell".into(),
        sandbox: SandboxSpec {
            mode: "interactive".into(),
            kernel: None,
            initramfs: None,
            memory_mb,
            vcpus,
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

/// Resolves the LLM provider for Claude-based shell programs.
///
/// # Errors
///
/// Returns `Ok(None)` for non-Claude programs. Returns an error when the target
/// program is Claude-based and no provider can be determined.
fn resolve_provider(
    program: &str,
    flag: Option<&str>,
    has_file: bool,
    llm_spec: Option<&spec::LlmSpec>,
) -> Result<Option<LlmProvider>, Box<dyn std::error::Error>> {
    if !matches!(program, "claude" | "claude-code") {
        return Ok(None);
    }

    if let Some(name) = flag {
        return Ok(Some(provider_from_name(name)));
    }

    if has_file {
        if let Some(llm) = llm_spec {
            return Ok(Some(provider_from_name(&llm.provider)));
        }
    }

    if claude_personal_available() {
        return Ok(Some(LlmProvider::ClaudePersonal));
    }

    if std::env::var("ANTHROPIC_API_KEY").is_ok() {
        return Ok(Some(LlmProvider::Claude));
    }

    Err("no LLM provider detected: set ANTHROPIC_API_KEY or run `claude auth login`".into())
}

fn claude_personal_available() -> bool {
    discover_oauth_credentials().is_ok()
}

/// Maps a provider name string to an `LlmProvider` variant.
fn provider_from_name(name: &str) -> LlmProvider {
    match name.to_ascii_lowercase().as_str() {
        "claude" => LlmProvider::Claude,
        "claude-personal" => LlmProvider::ClaudePersonal,
        other => {
            tracing::warn!("unknown LLM provider '{}'; defaulting to claude", other);
            LlmProvider::Claude
        }
    }
}

/// Stages OAuth credentials when using the `claude-personal` provider.
///
/// # Errors
///
/// Returns an error if credential discovery or staging fails.
fn prepare_credentials(
    provider: Option<&LlmProvider>,
) -> Result<Option<StagedCredentials>, Box<dyn std::error::Error>> {
    if !matches!(provider, Some(LlmProvider::ClaudePersonal)) {
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
    let ws = tcgetwinsize(std::io::stdout().lock())?;
    Ok((ws.ws_col, ws.ws_row))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_provider_returns_none_for_non_claude_programs() {
        let provider = resolve_provider("sh", None, false, None).unwrap();
        assert!(provider.is_none());
    }

    #[test]
    fn resolve_provider_respects_explicit_provider_flag() {
        let provider = resolve_provider("claude", Some("claude-personal"), false, None).unwrap();
        assert!(matches!(provider, Some(LlmProvider::ClaudePersonal)));
    }

    #[test]
    fn resolve_provider_uses_spec_provider_when_file_is_present() {
        let llm_spec = spec::LlmSpec {
            provider: "claude-personal".into(),
            model: None,
            base_url: None,
            api_key_env: None,
        };
        let provider = resolve_provider("claude", None, true, Some(&llm_spec)).unwrap();
        assert!(matches!(provider, Some(LlmProvider::ClaudePersonal)));
    }

    #[test]
    fn unknown_provider_defaults_to_claude() {
        assert!(matches!(
            provider_from_name("not-a-real-provider"),
            LlmProvider::Claude
        ));
    }

    #[tokio::test]
    async fn resolve_shell_images_uses_spec_kernel_when_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        let kernel_path = dir.path().join("vmlinuz");
        let initramfs_path = dir.path().join("rootfs.cpio.gz");
        std::fs::write(&kernel_path, b"fake-kernel").unwrap();
        std::fs::write(&initramfs_path, b"fake-initramfs").unwrap();

        let spec = RunSpec {
            api_version: "v1".into(),
            kind: RunKind::Sandbox,
            name: "test".into(),
            sandbox: SandboxSpec {
                mode: "interactive".into(),
                kernel: Some(kernel_path.display().to_string()),
                initramfs: Some(initramfs_path.display().to_string()),
                memory_mb: 512,
                vcpus: 1,
                network: false,
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
        };

        let (kernel, initramfs) = resolve_shell_images(&spec, "claude").await.unwrap();
        assert_eq!(kernel, kernel_path.display().to_string());
        assert_eq!(initramfs, Some(initramfs_path.display().to_string()));
    }

    #[tokio::test]
    async fn resolve_shell_images_skips_nonexistent_spec_kernel() {
        let spec = RunSpec {
            api_version: "v1".into(),
            kind: RunKind::Sandbox,
            name: "test".into(),
            sandbox: SandboxSpec {
                mode: "interactive".into(),
                kernel: Some("/nonexistent/vmlinuz".into()),
                initramfs: None,
                memory_mb: 512,
                vcpus: 1,
                network: false,
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
        };

        // Should not return the nonexistent path — falls through to
        // installed paths or download (which will also fail in test,
        // but we're testing that it doesn't blindly return the bad path).
        let result = resolve_shell_images(&spec, "claude").await;
        if let Ok((k, _)) = result {
            assert_ne!(k, "/nonexistent/vmlinuz");
        }
    }
}
