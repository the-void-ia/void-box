use std::path::{Path, PathBuf};

use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};

use void_box::snapshot_store;

/// Read a bearer-token file via `void_box::daemon_listen::read_token_file`
/// — same `0o600`-style owner-only perm check the daemon applies. Returns
/// `None` and falls through to the next discovery tier on any failure.
///
/// When `explicit` is true (the operator pointed `VOIDBOX_DAEMON_TOKEN_FILE`
/// at this path), failures emit a one-line `eprintln!` warning so a
/// misconfigured token file produces a hint rather than a confusing 401
/// from the daemon. When `explicit` is false (tier-3 auto-discovery at
/// `~/.config/voidbox/daemon-token`), missing files are silent — they're
/// the expected state until the daemon generates one — but loose
/// permissions still warn, since that's a real misconfig regardless of
/// how we found the file.
fn read_secure_token_file_or_warn(path_str: Option<String>, explicit: bool) -> Option<String> {
    let path_str = path_str?;
    let path = Path::new(&path_str);
    if !explicit && !path.exists() {
        return None;
    }
    match void_box::daemon_listen::read_token_file(path) {
        Ok(secret) => {
            let trimmed = secret.expose_secret().trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        }
        Err(err) => {
            eprintln!(
                "warning: ignoring daemon token at {}: {err}",
                path.display()
            );
            None
        }
    }
}

/// Resolved filesystem paths used by the CLI.
#[derive(Debug, Clone)]
pub struct CliPaths {
    pub state_dir: PathBuf,
    pub log_dir: PathBuf,
    pub snapshot_dir: PathBuf,
    pub config_dir: PathBuf,
}

impl CliPaths {
    fn from_config(cfg: &VoidboxCliConfig) -> Self {
        let base = Self::default_base();
        let home_override = std::env::var("VOIDBOX_HOME").ok().map(PathBuf::from);

        let state_dir = cfg
            .paths
            .state_dir
            .clone()
            .or_else(|| home_override.as_ref().map(|h| h.join("state")))
            .unwrap_or_else(|| base.state_dir.clone());

        let log_dir = cfg
            .paths
            .log_dir
            .clone()
            .or_else(|| home_override.as_ref().map(|h| h.join("log")))
            .unwrap_or_else(|| base.log_dir.clone());

        let snapshot_dir = cfg
            .paths
            .snapshot_dir
            .clone()
            .or_else(|| home_override.as_ref().map(|h| h.join("snapshots")))
            .unwrap_or_else(|| base.snapshot_dir.clone());

        Self {
            state_dir,
            log_dir,
            snapshot_dir,
            config_dir: base.config_dir,
        }
    }

    fn default_base() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .expect("HOME environment variable must be set");

        // XDG Base Directory Specification
        // https://specifications.freedesktop.org/basedir-spec/basedir-spec-latest.html

        let config_dir = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("voidbox");

        let data_dir = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local").join("share"))
            .join("voidbox");

        let state_dir = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local").join("state"))
            .join("voidbox");

        Self {
            state_dir,
            log_dir: data_dir.join("log"),
            snapshot_dir: snapshot_store::default_snapshot_dir(),
            config_dir,
        }
    }
}

/// On-disk config file shape (YAML).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VoidboxCliConfig {
    #[serde(default)]
    pub log_level: Option<String>,
    #[serde(default)]
    pub daemon_url: Option<String>,
    #[serde(default)]
    pub banner: Option<bool>,
    #[serde(default)]
    pub paths: PathsConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PathsConfig {
    #[serde(default)]
    pub state_dir: Option<PathBuf>,
    #[serde(default)]
    pub log_dir: Option<PathBuf>,
    #[serde(default)]
    pub snapshot_dir: Option<PathBuf>,
    #[serde(default)]
    pub kernel: Option<PathBuf>,
    #[serde(default)]
    pub initramfs: Option<PathBuf>,
}

/// Fully resolved configuration after merging all sources.
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub log_level: String,
    pub daemon_url: String,
    pub banner: bool,
    pub paths: CliPaths,
    pub kernel: Option<PathBuf>,
    pub initramfs: Option<PathBuf>,
    /// Bearer token used by the CLI when talking to a TCP daemon. Read at
    /// load time from `VOIDBOX_DAEMON_TOKEN`, `VOIDBOX_DAEMON_TOKEN_FILE`,
    /// or `~/.config/voidbox/daemon-token` (in that priority). Always
    /// `None` for AF_UNIX daemons — the resolution is short-circuited
    /// when `daemon_url` starts with `unix://`, both because the token
    /// would never be used (AF_UNIX gates access via socket permissions)
    /// and to avoid spurious warnings from a stale or misconfigured
    /// token file that isn't actually relevant to this run.
    pub daemon_token: Option<String>,
}

impl ResolvedConfig {
    /// Default daemon URL: the AF_UNIX path the daemon resolves at startup.
    /// Both ends consult `void_box::daemon_listen::default_unix_socket_path`
    /// so a same-uid invocation auto-discovers the socket.
    pub fn default_daemon_url() -> String {
        let path = void_box::daemon_listen::default_unix_socket_path();
        format!("unix://{}", path.display())
    }
}

/// Load and merge configuration from all sources.
///
/// Precedence (highest wins): CLI flags → `VOIDBOX_*` env vars →
/// `VOIDBOX_CONFIG` file (merged as an overlay, not a full replacement) →
/// user config → system config → defaults.
pub fn load_and_merge(
    cli_log_level: Option<&str>,
    cli_log_dir: Option<&Path>,
    cli_daemon_url: Option<&str>,
    cli_no_banner: bool,
) -> ResolvedConfig {
    let mut merged = VoidboxCliConfig::default();

    // System config: /etc/voidbox/config.yaml
    if let Some(sys) = load_config_file(Path::new("/etc/voidbox/config.yaml")) {
        merge_into(&mut merged, &sys);
    }

    // User config: ~/.config/voidbox/config.yaml (or XDG equivalent)
    let user_config_path = CliPaths::default_base().config_dir.join("config.yaml");
    if let Some(user) = load_config_file(&user_config_path) {
        merge_into(&mut merged, &user);
    }

    // VOIDBOX_CONFIG: optional extra YAML merged on top of system + user (highest among file sources).
    if let Ok(explicit) = std::env::var("VOIDBOX_CONFIG") {
        if let Some(cfg) = load_config_file(Path::new(&explicit)) {
            merge_into(&mut merged, &cfg);
        }
    }

    // VOIDBOX_LOG_LEVEL env
    if let Ok(level) = std::env::var("VOIDBOX_LOG_LEVEL") {
        merged.log_level = Some(level);
    }

    // VOIDBOX_LOG_DIR env
    if let Ok(dir) = std::env::var("VOIDBOX_LOG_DIR") {
        merged.paths.log_dir = Some(PathBuf::from(dir));
    }

    // VOIDBOX_DAEMON_URL env
    if let Ok(url) = std::env::var("VOIDBOX_DAEMON_URL") {
        merged.daemon_url = Some(url);
    }

    // VOIDBOX_NO_BANNER env
    if std::env::var("VOIDBOX_NO_BANNER")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        merged.banner = Some(false);
    }

    // CLI flags (highest precedence)
    if let Some(level) = cli_log_level {
        merged.log_level = Some(level.to_string());
    }
    if let Some(dir) = cli_log_dir {
        merged.paths.log_dir = Some(dir.to_path_buf());
    }
    if let Some(url) = cli_daemon_url {
        merged.daemon_url = Some(url.to_string());
    }
    if cli_no_banner {
        merged.banner = Some(false);
    }

    let paths = CliPaths::from_config(&merged);

    let log_level = merged
        .log_level
        .or_else(|| std::env::var("RUST_LOG").ok())
        .unwrap_or_else(|| "info".into());

    // The bearer token is only sent on TCP daemon URLs; AF_UNIX gates
    // access via socket permissions. Skipping the resolution work for
    // unix:// URLs avoids spurious "bad perms" warnings from a stale
    // ~/.config/voidbox/daemon-token (or a misconfigured
    // VOIDBOX_DAEMON_TOKEN_FILE) when the user has switched back to a
    // local AF_UNIX daemon — the file isn't being used either way.
    let resolved_daemon_url = merged
        .daemon_url
        .clone()
        .unwrap_or_else(ResolvedConfig::default_daemon_url);
    let daemon_token = if resolved_daemon_url.starts_with("unix://") {
        None
    } else {
        std::env::var("VOIDBOX_DAEMON_TOKEN")
            .ok()
            .or_else(|| {
                read_secure_token_file_or_warn(
                    std::env::var("VOIDBOX_DAEMON_TOKEN_FILE").ok(),
                    true,
                )
            })
            .or_else(|| {
                // Tier-3 fallback: pick up the token the daemon
                // auto-generates and persists when started without
                // explicit token wiring. Same path on both sides
                // converges client and server with zero env-var plumbing
                // for the typical local-dev TCP case.
                let path = void_box::daemon_listen::default_token_path();
                read_secure_token_file_or_warn(Some(path.to_string_lossy().into_owned()), false)
            })
    };

    ResolvedConfig {
        log_level,
        daemon_url: resolved_daemon_url,
        banner: merged.banner.unwrap_or(true),
        kernel: merged.paths.kernel,
        initramfs: merged.paths.initramfs,
        paths,
        daemon_token,
    }
}

fn load_config_file(path: &Path) -> Option<VoidboxCliConfig> {
    let contents = std::fs::read_to_string(path).ok()?;
    serde_yaml::from_str(&contents).ok()
}

fn merge_into(base: &mut VoidboxCliConfig, overlay: &VoidboxCliConfig) {
    if overlay.log_level.is_some() {
        base.log_level.clone_from(&overlay.log_level);
    }
    if overlay.daemon_url.is_some() {
        base.daemon_url.clone_from(&overlay.daemon_url);
    }
    if overlay.banner.is_some() {
        base.banner = overlay.banner;
    }
    if overlay.paths.state_dir.is_some() {
        base.paths.state_dir.clone_from(&overlay.paths.state_dir);
    }
    if overlay.paths.log_dir.is_some() {
        base.paths.log_dir.clone_from(&overlay.paths.log_dir);
    }
    if overlay.paths.snapshot_dir.is_some() {
        base.paths
            .snapshot_dir
            .clone_from(&overlay.paths.snapshot_dir);
    }
    if overlay.paths.kernel.is_some() {
        base.paths.kernel.clone_from(&overlay.paths.kernel);
    }
    if overlay.paths.initramfs.is_some() {
        base.paths.initramfs.clone_from(&overlay.paths.initramfs);
    }
}

/// Write a template config file to the given path.
pub fn write_template(path: &Path) -> std::io::Result<()> {
    let template = VoidboxCliConfig {
        log_level: Some("info".into()),
        daemon_url: Some(ResolvedConfig::default_daemon_url()),
        banner: Some(true),
        paths: PathsConfig::default(),
    };
    let yaml = serde_yaml::to_string(&template).unwrap_or_default();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, yaml)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voidbox_config_style_overlay_preserves_unset_fields() {
        // Regression: explicit file must merge like other layers, not replace `merged`.
        let mut merged = VoidboxCliConfig {
            log_level: Some("info".into()),
            daemon_url: Some("http://from-user:1".into()),
            banner: Some(true),
            paths: PathsConfig::default(),
        };
        let explicit = VoidboxCliConfig {
            daemon_url: Some("http://from-explicit:2".into()),
            ..Default::default()
        };
        merge_into(&mut merged, &explicit);
        assert_eq!(merged.log_level.as_deref(), Some("info"));
        assert_eq!(merged.daemon_url.as_deref(), Some("http://from-explicit:2"));
        assert_eq!(merged.banner, Some(true));
    }

    #[test]
    fn test_merge_overlay_wins() {
        let mut base = VoidboxCliConfig {
            log_level: Some("info".into()),
            daemon_url: Some("http://base:1234".into()),
            banner: Some(true),
            paths: PathsConfig::default(),
        };
        let overlay = VoidboxCliConfig {
            log_level: Some("debug".into()),
            daemon_url: None,
            banner: Some(false),
            paths: PathsConfig {
                snapshot_dir: Some(PathBuf::from("/custom/snapshots")),
                ..Default::default()
            },
        };
        merge_into(&mut base, &overlay);
        assert_eq!(base.log_level.as_deref(), Some("debug"));
        assert_eq!(base.daemon_url.as_deref(), Some("http://base:1234"));
        assert_eq!(base.banner, Some(false));
        assert_eq!(
            base.paths.snapshot_dir,
            Some(PathBuf::from("/custom/snapshots"))
        );
    }

    #[test]
    fn test_default_paths_are_set() {
        let paths = CliPaths::default_base();
        assert!(!paths.state_dir.as_os_str().is_empty());
        assert!(!paths.log_dir.as_os_str().is_empty());
        assert!(!paths.snapshot_dir.as_os_str().is_empty());
    }

    #[test]
    fn test_write_template_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        write_template(&path).unwrap();
        let loaded = load_config_file(&path).unwrap();
        assert_eq!(loaded.log_level.as_deref(), Some("info"));
        assert!(loaded.banner.unwrap());
    }

    #[test]
    fn cli_log_dir_override_wins() {
        let resolved = load_and_merge(
            Some("info"),
            Some(Path::new("/tmp/voidbox-logs")),
            None,
            false,
        );
        assert_eq!(resolved.paths.log_dir, PathBuf::from("/tmp/voidbox-logs"));
    }
}
