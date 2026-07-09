//! VoidBox: Agent(Skills) + Isolation
//!
//! A `VoidBox` binds skills (MCP servers, CLI tools, procedural knowledge)
//! to an isolated execution environment (KVM micro-VM). Each VoidBox:
//!
//! - Has a name and a purpose (prompt)
//! - Has one or more Skills installed
//! - Runs in a fresh, disposable VM
//! - Produces structured output for the next VoidBox
//!
//! Inspired by [Ed Huang's "Box" concept](https://me.0xffff.me/agent_infra.html):
//! *"A Box exposes no execution details, has no external dependencies,
//! has no side effects, and encapsulates Skill-guided Actions +
//! a reproducible, disposable environment."*
//!
//! # Example
//!
//! ```no_run
//! use void_box::skill::Skill;
//! use void_box::agent_box::VoidBox;
//!
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! let reasoning = Skill::agent("claude-code")
//!     .description("Autonomous reasoning and code execution");
//!
//! let market_data = Skill::mcp("market-data-mcp")
//!     .description("Provides OHLCV and news data for equities");
//!
//! let data_box = VoidBox::new("data_analyst")
//!     .skill(market_data)
//!     .skill(reasoning)
//!     .memory_mb(256)
//!     .prompt("Fetch 30 days of OHLCV data for AAPL, NVDA, MSFT, GOOGL")
//!     .build()?;
//! # Ok(())
//! # }
//! ```

use std::path::PathBuf;
use std::sync::Arc;

use secrecy::{ExposeSecret, SecretString};
use tracing::{debug, error, info, warn};

use crate::backend::guest_host_gateway;
use crate::credentials::{resolve_codex_auth_mode, OAuthProviderKind, OAuthTokenStore};
use crate::llm::LlmProvider;
use crate::observe::claude::AgentExecOpts;
use crate::observe::telemetry::TelemetryBuffer;
use crate::pipeline::StageResult;
use crate::proxy::{
    assert_no_real_credential, build_guest_provisioning, render_codex_config_toml,
    render_codex_mcp_servers_toml, render_guest_hosts, start_proxy, CredentialInjector,
    GuestClient, OAuthBearerInjector, ProxiedAuth, ProxiedUpstream, ProxyCa, ProxyHandle,
    ProxyToken, SandboxContext, StaticApiKeyInjector, CHATGPT_ACCOUNT_ID_HEADER,
    GUEST_CODEX_CONFIG_PATH, GUEST_HOSTS_PATH,
};
use crate::sandbox::Sandbox;
use crate::skill::{Skill, SkillKind};
use crate::spec::AgentMode;
use crate::Result;

/// Project-scoped config directory. Claude Code reads skills, settings, and
/// MCP config relative to the working directory (set to /workspace).
const CLAUDE_HOME: &str = "/workspace/.claude";

/// MCP config file path. Claude Code reads project-scoped MCP servers from
/// .mcp.json at the project root.
const MCP_CONFIG_PATH: &str = "/workspace/.mcp.json";
const CLAUDE_ONBOARDING_PATH: &str = "/home/sandbox/.claude.json";

/// Whether `key` is an env var the proxy owns when active: a real provider API
/// key that must be withheld from the guest (the proxy injects it host-side),
/// or the provider base URL that the proxy's own redirect must be the single
/// source of (the Custom provider's `env_vars()` emits a real
/// `ANTHROPIC_BASE_URL`; letting it coexist with the proxy's redirect would
/// leave the effective endpoint to env-ordering precedence).
fn is_proxy_owned_env(key: &str) -> bool {
    matches!(
        key,
        "ANTHROPIC_API_KEY" | "OPENAI_API_KEY" | "ANTHROPIC_BASE_URL"
    )
}

/// Drop the proxy-owned provider env when the credential proxy will supply it
/// host-side (R14). Pure (no host-env dependence) so the withholding can be
/// tested directly rather than only through a provider whose `env_vars()` reads
/// the ambient environment.
fn filter_withheld_env(env: Vec<(String, String)>, withhold: bool) -> Vec<(String, String)> {
    env.into_iter()
        .filter(|(k, _)| !withhold || !is_proxy_owned_env(k))
        .collect()
}

/// Whether the proxy serves `provider` at all. Pure (no host IO), so the R14
/// withholding decision never depends on host filesystem or env state: whenever
/// the proxy is enabled for a servable provider the real key is withheld, even
/// if proxy setup later fails for a host-side reason — the unrepresentable
/// alternative would be a guest holding both the placeholder routing and the
/// real credential.
fn provider_is_proxy_servable(provider: &LlmProvider) -> bool {
    match provider {
        LlmProvider::Claude
        | LlmProvider::ClaudePersonal
        | LlmProvider::Codex
        | LlmProvider::Custom { .. } => true,
        LlmProvider::Ollama { .. } | LlmProvider::LmStudio { .. } => false,
    }
}

/// Resolve the host-held API key for an API-key-mode provider the proxy serves.
/// Errors name the provider-specific remediation.
fn resolve_provider_secret(provider: &LlmProvider) -> Result<SecretString> {
    match provider {
        LlmProvider::Claude => std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .filter(|key| !key.trim().is_empty())
            .map(SecretString::from)
            .ok_or_else(|| {
                crate::Error::Config(
                    "credential_proxy is enabled but ANTHROPIC_API_KEY is not set on the host"
                        .into(),
                )
            }),
        LlmProvider::Codex => crate::credentials::discover_codex_api_key(),
        LlmProvider::Custom { api_key, .. } => api_key
            .as_ref()
            .map(|key| SecretString::from(key.expose_secret().to_string()))
            .ok_or_else(|| {
                crate::Error::Config(
                    "credential_proxy is enabled for a custom provider but no API key was \
                     resolved — set the spec's api_key_env to a host env var holding the key"
                        .into(),
                )
            }),
        _ => Err(crate::Error::Config(format!(
            "no host-held API key applies to provider '{}'",
            provider.description()
        ))),
    }
}

/// A credential proxy registered for the current sandbox: the handle keeps the
/// per-sandbox listener alive, and `exec_env` is the guest env (proxy base-URL, CA
/// path, placeholder, token header) injected at exec time.
struct ActiveCredentialProxy {
    handle: ProxyHandle,
    token_hex: String,
    exec_env: Vec<(String, String)>,
}

impl ActiveCredentialProxy {
    /// Stop the sandbox's listener. Call after the agent exec completes.
    async fn teardown(self) {
        self.handle.unregister_sandbox(&self.token_hex).await;
    }
}

/// Redirect the proxied upstream hostnames to the guest→host gateway so the
/// client's TLS connection (SNI = upstream host) lands on the per-sandbox proxy
/// listener while the cert/name-constraint stay scoped to the real upstream name.
///
/// `fs_guard` forbids host writes to `/etc/hosts` directly, so the rendered hosts
/// file is staged under `/etc/voidbox` (an allowed root); the guest-agent mirrors
/// it into `/etc/hosts` with its own privileged write on receipt.
async fn provision_proxy_hosts(sandbox: &Sandbox, aliases: &[(String, String)]) -> Result<()> {
    let hosts = render_guest_hosts(aliases);
    sandbox.mkdir_p("/etc/voidbox").await?;
    sandbox.write_file(GUEST_HOSTS_PATH, hosts.as_bytes()).await
}

/// Refuse the credential proxy on platforms with no guest-reachable bind that is
/// not also LAN-exposed. Linux/KVM binds host loopback (SLIRP-forwarded to the
/// guest); macOS/VZ binds the host-local VZ NAT gateway address
/// ([`crate::backend::credential_proxy_bind_addr`], ADR-0007). Any other
/// platform has neither, so it fails closed.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn ensure_credential_proxy_platform_supported() -> Result<()> {
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn ensure_credential_proxy_platform_supported() -> Result<()> {
    Err(crate::Error::Config(
        "credential_proxy is supported on Linux/KVM and macOS/VZ only: this platform \
         has no bind address for the proxy listener that the guest can reach without \
         also exposing the credential-injecting parser to the host's network"
            .into(),
    ))
}

/// Published output from a service agent.
/// Contains only what agent_box knows: guest execution output.
/// Artifact publication is built later by the daemon/persistence layer.
pub struct ServicePublication {
    pub box_name: String,
    pub output: Vec<u8>,
    pub report: crate::runtime::RunReport,
}

/// Terminal lifecycle result for a service agent. No candidate output.
pub enum ServiceExit {
    Exited {
        success: bool,
        error: Option<String>,
    },
    Canceled,
    Crashed(String),
}

/// Handle for a running service agent.
pub struct ServiceStageHandle {
    /// Fires exactly once when structured output is ready.
    pub output_rx: tokio::sync::oneshot::Receiver<ServicePublication>,
    /// Send to stop the service.
    pub stop_tx: tokio::sync::oneshot::Sender<()>,
    /// Fires when the service process exits.
    pub exit_rx: tokio::sync::oneshot::Receiver<ServiceExit>,
}

/// Result of running an agent — either a terminal task result or a service handle.
pub enum AgentRunOutcome {
    Task(crate::pipeline::StageResult),
    Service(ServiceStageHandle),
}

/// An agent Box: Agent(Skills) + Isolation.
///
/// Constructed via the builder pattern with `VoidBox::new("name")`.
pub struct VoidBox {
    /// Human-readable name of this Box
    pub name: String,
    /// The prompt that defines what this Box does
    pub prompt: String,
    /// Skills installed in this Box
    pub skills: Vec<Skill>,
    /// The underlying sandbox (built lazily or eagerly)
    sandbox: Option<Arc<Sandbox>>,
    /// Builder config (before build)
    config: BoxConfig,
}

/// Internal configuration before the Box is built.
#[derive(Debug, Clone)]
struct BoxConfig {
    memory_mb: usize,
    vcpus: usize,
    network: bool,
    kernel: Option<PathBuf>,
    initramfs: Option<PathBuf>,
    env: Vec<(String, String)>,
    /// Host directory mounts into the guest.
    mounts: Vec<crate::backend::MountConfig>,
    /// Guest path where an OCI rootfs is mounted (triggers pivot_root in guest-agent).
    oci_rootfs: Option<String>,
    /// OCI rootfs block device in guest (e.g. /dev/vda).
    oci_rootfs_dev: Option<String>,
    /// Host path to OCI rootfs disk image for virtio-blk.
    oci_rootfs_disk: Option<PathBuf>,
    /// Path to a snapshot directory to restore from (skips cold boot).
    snapshot: Option<PathBuf>,
    /// Path where the agent should write its output (read after execution)
    output_file: String,
    /// Whether to use mock sandbox
    mock: bool,
    /// LLM provider (default: Claude)
    llm: LlmProvider,
    /// Route the LLM provider's API key through the host credential-injection
    /// proxy instead of forwarding it into the guest. Opt-in; default `false`
    /// keeps the legacy env-forwarding behaviour unchanged.
    credential_proxy: bool,
    /// Per-stage timeout in seconds (overrides the default vsock read timeout).
    /// `None` means use the system default (1200s / 20 minutes).
    timeout_secs: Option<u64>,
    /// Agent mode: Task (run-to-completion) or Service (long-running).
    mode: AgentMode,
    /// Optional staged Claude personal credentials to copy into the guest.
    claude_credentials_host_path: Option<PathBuf>,
}

impl Default for BoxConfig {
    fn default() -> Self {
        Self {
            memory_mb: 256,
            vcpus: 1,
            network: false,
            kernel: None,
            initramfs: None,
            env: Vec::new(),
            mounts: Vec::new(),
            oci_rootfs: None,
            oci_rootfs_dev: None,
            oci_rootfs_disk: None,
            snapshot: None,
            output_file: "/workspace/output.json".to_string(),
            mock: false,
            llm: LlmProvider::default(),
            credential_proxy: false,
            timeout_secs: None,
            mode: AgentMode::default(),
            claude_credentials_host_path: None,
        }
    }
}

impl VoidBox {
    /// Create a new VoidBox builder with the given name.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            prompt: String::new(),
            skills: Vec::new(),
            sandbox: None,
            config: BoxConfig::default(),
        }
    }

    // -- Builder methods --

    /// Add a Skill to this Box.
    pub fn skill(mut self, skill: Skill) -> Self {
        self.skills.push(skill);
        self
    }

    /// Set the prompt that defines this Box's purpose.
    pub fn prompt(mut self, prompt: impl Into<String>) -> Self {
        self.prompt = prompt.into();
        self
    }

    /// Set memory in MB.
    pub fn memory_mb(mut self, mb: usize) -> Self {
        self.config.memory_mb = mb;
        self
    }

    /// Set number of vCPUs.
    pub fn vcpus(mut self, count: usize) -> Self {
        self.config.vcpus = count;
        self
    }

    /// Enable or disable networking.
    pub fn network(mut self, enable: bool) -> Self {
        self.config.network = enable;
        self
    }

    /// Set kernel path.
    pub fn kernel(mut self, path: impl Into<PathBuf>) -> Self {
        self.config.kernel = Some(path.into());
        self
    }

    /// Set initramfs path.
    pub fn initramfs(mut self, path: impl Into<PathBuf>) -> Self {
        self.config.initramfs = Some(path.into());
        self
    }

    /// Add an environment variable.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.config.env.push((key.into(), value.into()));
        self
    }

    /// Set the output file path the agent should write to.
    /// Defaults to `/workspace/output.json`.
    pub fn output_file(mut self, path: impl Into<String>) -> Self {
        self.config.output_file = path.into();
        self
    }

    /// Point at a staged host-side Claude credentials directory whose
    /// `.credentials.json` should be copied into the guest before launch.
    pub fn claude_credentials_host_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.config.claude_credentials_host_path = Some(path.into());
        self
    }

    /// Set the LLM provider (default: Claude).
    ///
    /// When set to `Ollama` or `Custom`, the appropriate environment variables
    /// are injected into the guest VM and networking is auto-enabled.
    ///
    /// ```no_run
    /// use void_box::llm::LlmProvider;
    /// use void_box::agent_box::VoidBox;
    ///
    /// # fn demo() -> Result<(), Box<dyn std::error::Error>> {
    /// let ab = VoidBox::new("local")
    ///     .llm(LlmProvider::ollama("qwen3-coder"))
    ///     .prompt("hello")
    ///     .build()?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn llm(mut self, provider: LlmProvider) -> Self {
        self.config.llm = provider;
        self
    }

    /// Route the LLM provider's API key through the host credential-injection
    /// proxy instead of forwarding it into the guest.
    ///
    /// Opt-in: with the default `false`, behaviour is unchanged. When enabled
    /// for an API-key provider the proxy serves (Claude, Anthropic-compatible
    /// Custom), the real key is withheld from the guest env and injected
    /// host-side at egress; the guest carries only a placeholder + the per-sandbox
    /// CA + proxy token.
    pub fn credential_proxy(mut self, enable: bool) -> Self {
        self.config.credential_proxy = enable;
        self
    }

    /// Set a per-stage timeout in seconds.
    ///
    /// Overrides the system default (1200s / 20 min).  Useful when running
    /// small local models that are slower or faster than the default.
    ///
    /// ```no_run
    /// # use void_box::agent_box::VoidBox;
    /// let ab = VoidBox::new("fast_box")
    ///     .timeout_secs(300) // 5 minutes
    ///     .prompt("Quick task")
    ///     .build().unwrap();
    /// ```
    pub fn timeout_secs(mut self, secs: u64) -> Self {
        self.config.timeout_secs = Some(secs);
        self
    }

    /// Set the agent mode (Task or Service).
    pub fn mode(mut self, mode: AgentMode) -> Self {
        self.config.mode = mode;
        self
    }

    /// Add a host directory mount.
    pub fn mount(mut self, mount: crate::backend::MountConfig) -> Self {
        self.config.mounts.push(mount);
        self
    }

    /// Set the OCI rootfs guest path (triggers pivot_root in guest-agent).
    pub fn oci_rootfs(mut self, guest_path: impl Into<String>) -> Self {
        self.config.oci_rootfs = Some(guest_path.into());
        self
    }

    /// Set the OCI rootfs block device path in guest (e.g. `/dev/vda`).
    pub fn oci_rootfs_dev(mut self, dev_path: impl Into<String>) -> Self {
        self.config.oci_rootfs_dev = Some(dev_path.into());
        self
    }

    /// Set the host OCI rootfs disk image path for virtio-blk.
    pub fn oci_rootfs_disk(mut self, path: impl Into<PathBuf>) -> Self {
        self.config.oci_rootfs_disk = Some(path.into());
        self
    }

    /// Set a snapshot directory to restore from (skips cold boot).
    pub fn snapshot(mut self, path: impl Into<PathBuf>) -> Self {
        self.config.snapshot = Some(path.into());
        self
    }

    /// Use a mock sandbox (for testing without KVM).
    pub fn mock(mut self) -> Self {
        self.config.mock = true;
        self
    }

    /// Build the Box, creating the underlying sandbox.
    pub fn build(mut self) -> Result<Self> {
        let sandbox = self.create_sandbox()?;
        self.sandbox = Some(sandbox);
        Ok(self)
    }

    /// Create the sandbox from the current configuration.
    fn create_sandbox(&self) -> Result<Arc<Sandbox>> {
        // Reject an unusable credential-proxy configuration before staging any
        // env or booting the guest. `withhold_provider_secret` is false for a
        // provider the proxy does not serve, so without this gate an unsupported
        // provider would boot with the real key in its exec env and only fail
        // once `maybe_setup_credential_proxy` runs — after the pre-run
        // provisioning execs have already seen it (R14).
        if self.config.credential_proxy {
            self.validate_credential_proxy_preconditions()?;
        }

        let mut builder = if self.config.mock {
            Sandbox::mock()
        } else {
            Sandbox::local()
        };

        // Auto-enable networking if the LLM provider needs it
        let needs_network = self.config.network || self.config.llm.requires_network();

        builder = builder
            .memory_mb(self.config.memory_mb)
            .vcpus(self.config.vcpus)
            .network(needs_network);

        if let Some(ref k) = self.config.kernel {
            builder = builder.kernel(k);
        }
        if let Some(ref i) = self.config.initramfs {
            builder = builder.initramfs(i);
        }

        // Inject LLM provider env vars first, then user overrides. When the
        // credential proxy is enabled for a provider it serves, the real API key
        // is withheld here (R14) — the proxy injects it host-side, and the guest
        // receives only a placeholder via the runtime proxy env.
        for (k, v) in self.staged_llm_env() {
            builder = builder.env(&k, &v);
        }
        for (k, v) in &self.config.env {
            builder = builder.env(k, v);
        }

        // Add host directory mounts
        for m in &self.config.mounts {
            builder = builder.mount(m.clone());
        }

        // OCI rootfs pivot_root
        if let Some(ref path) = self.config.oci_rootfs {
            builder = builder.oci_rootfs(path);
        }
        if let Some(ref dev) = self.config.oci_rootfs_dev {
            builder = builder.oci_rootfs_dev(dev);
        }
        if let Some(ref disk) = self.config.oci_rootfs_disk {
            builder = builder.oci_rootfs_disk(disk);
        }

        // Snapshot restore (explicit opt-in only)
        if let Some(ref snap) = self.config.snapshot {
            builder = builder.snapshot(snap);
        }

        builder.build()
    }

    /// Fail closed on a credential-proxy configuration the proxy cannot serve,
    /// before any guest env is staged. Provider support is platform-independent,
    /// so it is checked first — an unsupported provider reports the provider error
    /// on every host — and the platform gate second.
    fn validate_credential_proxy_preconditions(&self) -> Result<()> {
        if !provider_is_proxy_servable(&self.config.llm) {
            return Err(crate::Error::Config(format!(
                "credential_proxy is enabled but provider '{}' holds no host-side \
                 credential for the proxy to contain (local providers are not proxied)",
                self.config.llm.description()
            )));
        }
        // Surface provider-shape config errors (a non-HTTPS or internal Custom
        // base URL, an unresolvable codex auth mode) now, before boot, rather
        // than after the guest is already running.
        self.resolve_proxied_upstream()?;
        // R14: the proxy injects credentials host-side, so a guest-staged
        // credentials file must never coexist with it. A per-box override can
        // enable the proxy while a credentials file was staged from the top-level
        // config; reject that here — before any provisioning exec — rather than
        // let the durable refresh token ride into the guest in a file the
        // env/CA/hosts R14 audit does not cover. Checked before the platform gate
        // because the conflict is platform-independent.
        if self.config.claude_credentials_host_path.is_some() {
            return Err(crate::Error::Config(
                "credential_proxy is active but an OAuth credentials file is also staged \
                 into the guest — refusing to leak the durable refresh token (R14)"
                    .into(),
            ));
        }
        // Same R14 shape for credential-directory mounts: the codex auth.json
        // mount (or any user mount over an agent config home) would carry a
        // durable secret into the guest beside the proxy's placeholders.
        for mount in &self.config.mounts {
            if matches!(
                mount.guest_path.as_str(),
                "/home/sandbox/.codex" | "/home/sandbox/.claude"
            ) {
                return Err(crate::Error::Config(format!(
                    "credential_proxy is active but a mount stages {} into the guest — \
                     refusing to leak a durable credential beside the proxy placeholders (R14)",
                    mount.guest_path
                )));
            }
        }
        ensure_credential_proxy_platform_supported()?;
        Ok(())
    }

    /// Resolve the proxied-upstream descriptor for this run's provider, including
    /// the host-side codex auth-mode resolution when the provider is codex.
    fn resolve_proxied_upstream(&self) -> Result<ProxiedUpstream> {
        let codex_mode = if matches!(self.config.llm, LlmProvider::Codex) {
            Some(resolve_codex_auth_mode()?)
        } else {
            None
        };
        ProxiedUpstream::for_provider(&self.config.llm, codex_mode)?.ok_or_else(|| {
            crate::Error::Config(format!(
                "credential_proxy is enabled but provider '{}' is not served by the proxy",
                self.config.llm.description()
            ))
        })
    }

    /// Whether the proxy-owned provider env must be withheld from the guest
    /// because the credential proxy will inject the credential host-side.
    fn withhold_provider_secret(&self) -> bool {
        self.config.credential_proxy && provider_is_proxy_servable(&self.config.llm)
    }

    /// The provider/LLM env staged into the guest, with the real provider key
    /// removed when [`Self::withhold_provider_secret`] holds (R14): the guest
    /// then carries only the proxy's placeholder, never the durable key.
    fn staged_llm_env(&self) -> Vec<(String, String)> {
        filter_withheld_env(self.config.llm.env_vars(), self.withhold_provider_secret())
    }

    /// The complete environment staged into the guest for an agent run: the
    /// provider/LLM env (post-withholding), the user overrides, and the proxy's
    /// own provisioning env. This is the set the R14 gate audits — auditing only
    /// the proxy env would miss a real key reaching the guest through the LLM env
    /// or a user override.
    fn r14_audit_env(&self, proxy_env: &[(String, String)]) -> Vec<(String, String)> {
        let mut env = self.staged_llm_env();
        env.extend(self.config.env.iter().cloned());
        env.extend(proxy_env.iter().cloned());
        env
    }

    /// Start the credential proxy for this sandbox when opted in, deliver the
    /// per-sandbox CA + `/etc/hosts` redirect into the guest, and return the guest
    /// env to inject at exec time. Returns `None` when the proxy is disabled.
    ///
    /// Errors (rather than silently falling back) when the proxy is enabled but
    /// the provider is unsupported or no host key is available — a silent
    /// fallback would forward the real key into the guest, defeating the point.
    async fn maybe_setup_credential_proxy(
        &self,
        sandbox: &Sandbox,
        codex_mcp_toml: &str,
    ) -> Result<Option<ActiveCredentialProxy>> {
        if !self.config.credential_proxy {
            return Ok(None);
        }
        // The credential-injecting listener — and its in-process, pre-auth
        // TLS/HTTP parser (R10) — must be reachable by the guest without being
        // LAN-exposed. `credential_proxy_bind_addr` provides that on Linux/KVM
        // (loopback, SLIRP-forwarded) and macOS/VZ (the host-local NAT gateway
        // address, ADR-0007); any other platform fails closed here.
        ensure_credential_proxy_platform_supported()?;
        let provider = &self.config.llm;
        let upstream = self.resolve_proxied_upstream()?;
        // Build the credential injector for the provider's auth mode, and capture
        // the durable secret so the R14 gate can assert it never reaches the guest.
        //  - API key: a static host-held key injected verbatim (Anthropic
        //    `x-api-key`, or `Authorization: Bearer` for codex/OpenAI).
        //  - OAuth: a host store that refreshes the durable token and mints a
        //    short-lived Bearer per request; its warm-up is spawned to overlap the
        //    first refresh with VM boot. codex ChatGPT additionally injects the
        //    host-held account identity next to the Bearer.
        let (injector, secret_plain): (Arc<dyn CredentialInjector>, String) = match upstream.auth {
            ProxiedAuth::ApiKey(scheme) => {
                let secret = resolve_provider_secret(provider)?;
                let secret_plain = secret.expose_secret().to_string();
                let injector = Arc::new(StaticApiKeyInjector::new(
                    upstream.host.clone(),
                    scheme,
                    secret,
                ));
                (injector, secret_plain)
            }
            ProxiedAuth::Oauth(OAuthProviderKind::ClaudeCode) => {
                let store = Arc::new(OAuthTokenStore::claude_from_host()?);
                let secret_plain = store
                    .durable_secret_snapshot()
                    .await
                    .expose_secret()
                    .to_string();
                let warm = store.clone();
                tokio::spawn(async move { warm.warm_up().await });
                let injector = Arc::new(OAuthBearerInjector::new(upstream.host.clone(), store));
                (injector, secret_plain)
            }
            ProxiedAuth::Oauth(OAuthProviderKind::CodexChatGpt) => {
                let store = Arc::new(OAuthTokenStore::codex_from_host()?);
                // The ChatGPT backend authorizes the (Bearer, account) pair, so a
                // credential without its account id cannot be injected usefully —
                // fail closed now with the remediation named.
                let account_id = store.codex_account_id().await.ok_or_else(|| {
                    crate::Error::Config(
                        "codex auth.json holds no tokens.account_id — re-run 'codex login' \
                         to refresh the stored ChatGPT identity"
                            .into(),
                    )
                })?;
                let secret_plain = store
                    .durable_secret_snapshot()
                    .await
                    .expose_secret()
                    .to_string();
                let warm = store.clone();
                tokio::spawn(async move { warm.warm_up().await });
                let injector = Arc::new(
                    OAuthBearerInjector::new(upstream.host.clone(), store)
                        .with_extra_header(CHATGPT_ACCOUNT_ID_HEADER, account_id),
                );
                (injector, secret_plain)
            }
        };

        // CA keygen + self-sign is CPU-bound; keep it off the async runtime.
        let upstream_host = upstream.host.clone();
        let ca = tokio::task::spawn_blocking(move || ProxyCa::generate(vec![upstream_host]))
            .await
            .map_err(|e| crate::Error::Network(format!("proxy CA task join failed: {e}")))??;
        let ca = Arc::new(ca);
        let ca_pem = ca.ca_cert_pem().to_string();
        let ctx = SandboxContext::new(
            ProxyToken::generate(),
            ca,
            injector,
            vec![upstream.host.clone()],
        )
        .with_upstream_port(upstream.port);

        let handle = start_proxy().await?;
        let binding = handle.register_sandbox(ctx).await?;
        let provisioning =
            build_guest_provisioning(&upstream, &binding, &ca_pem, guest_host_gateway());

        // Everything the proxy writes into the guest: the CA PEM (and, for codex,
        // the placeholder auth.json) from the provisioning, plus the generated
        // codex config.toml — composed here because MCP server entries share that
        // file (`provision_skills` hands them over instead of writing the file
        // itself when the proxy owns it).
        let mut staged_files = provisioning.files.clone();
        if upstream.client == GuestClient::CodexCli {
            staged_files.push((
                GUEST_CODEX_CONFIG_PATH.to_string(),
                render_codex_config_toml(&upstream, &binding, codex_mcp_toml),
            ));
        }

        // R14: assert no real credential reaches the guest. The audit covers the
        // full staged env — provider/LLM env (post-withholding) + user overrides +
        // the proxy's provisioning env — and every file the proxy stages (CA PEM,
        // codex auth.json/config.toml, `/etc/hosts`). The primary control is
        // structural: the key is withheld from the staged env, and the staged
        // files are rendered from placeholders in the first place. This gate is
        // the backstop that would catch a withholding or rendering regression; it
        // runs before the agent exec and the proxy's writes (a regression could
        // still reach the earlier pre-run provisioning execs before this aborts).
        let staged_env = self.r14_audit_env(&provisioning.env);
        let mut audit_files = staged_files.clone();
        audit_files.push((
            GUEST_HOSTS_PATH.to_string(),
            render_guest_hosts(&provisioning.host_aliases),
        ));
        assert_no_real_credential(&staged_env, &audit_files, &secret_plain)?;

        if upstream.client == GuestClient::CodexCli {
            sandbox.mkdir_p("/home/sandbox/.codex").await?;
        }
        for (path, contents) in &staged_files {
            sandbox.write_file(path, contents.as_bytes()).await?;
        }
        provision_proxy_hosts(sandbox, &provisioning.host_aliases).await?;

        info!(
            "[vm:{}] credential proxy active on port {} for {} (real key withheld from guest)",
            self.name, binding.port, upstream.host
        );

        Ok(Some(ActiveCredentialProxy {
            handle,
            token_hex: binding.token_hex,
            exec_env: provisioning.env,
        }))
    }

    /// Provision security configuration into the guest.
    ///
    /// Writes resource limits and command allowlist as JSON files that
    /// the guest-agent reads at connection time.
    async fn provision_security(&self, sandbox: &Sandbox) -> Result<()> {
        let tag = &self.name;

        // Write resource limits (use defaults from SecurityConfig)
        let rl = crate::backend::ResourceLimits::default();
        let limits = serde_json::json!({
            "max_virtual_memory": rl.max_virtual_memory,
            "max_open_files": rl.max_open_files,
            "max_processes": rl.max_processes,
            "max_file_size": rl.max_file_size,
        });
        let limits_json = serde_json::to_string_pretty(&limits).map_err(|e| {
            crate::Error::Config(format!("Failed to serialize resource limits: {}", e))
        })?;
        sandbox.mkdir_p("/etc/voidbox").await?;
        sandbox
            .write_file("/etc/voidbox/resource_limits.json", limits_json.as_bytes())
            .await?;
        eprintln!(
            "[vm:{}] Wrote resource limits to /etc/voidbox/resource_limits.json",
            tag,
        );

        // Write command allowlist
        let allowlist: Vec<&str> = crate::backend::DEFAULT_COMMAND_ALLOWLIST.to_vec();
        let allowlist_json = serde_json::to_string_pretty(&allowlist).map_err(|e| {
            crate::Error::Config(format!("Failed to serialize command allowlist: {}", e))
        })?;
        sandbox
            .write_file(
                "/etc/voidbox/allowed_commands.json",
                allowlist_json.as_bytes(),
            )
            .await?;
        eprintln!(
            "[vm:{}] Wrote command allowlist ({} commands) to /etc/voidbox/allowed_commands.json",
            tag,
            allowlist.len(),
        );

        Ok(())
    }

    /// Write a skill file to the project-scoped .claude/skills/ directory.
    async fn write_skill_file(sandbox: &Sandbox, name: &str, content: &[u8]) -> Result<()> {
        let path = format!("{}/skills/{}.md", CLAUDE_HOME, name);
        sandbox.write_file(&path, content).await?;
        Ok(())
    }

    async fn provision_claude_bootstrap(&self, sandbox: &Sandbox) -> Result<()> {
        if !self.config.llm.supports_claude_settings() {
            return Ok(());
        }

        let onboarding = r#"{"hasCompletedOnboarding":true}"#;
        sandbox
            .write_file(CLAUDE_ONBOARDING_PATH, onboarding.as_bytes())
            .await?;

        let settings = serde_json::json!({
            "skipWebFetchPreflight": true
        });
        sandbox
            .write_file(
                &format!("{}/settings.json", CLAUDE_HOME),
                settings.to_string().as_bytes(),
            )
            .await?;

        if let Some(ref host_dir) = self.config.claude_credentials_host_path {
            let creds_path = host_dir.join(".credentials.json");
            if let Ok(credentials_bytes) = std::fs::read(&creds_path) {
                sandbox.mkdir_p("/home/sandbox/.claude").await?;
                sandbox
                    .write_file(
                        "/home/sandbox/.claude/.credentials.json",
                        &credentials_bytes,
                    )
                    .await?;
            }
        }

        // Ensure the sandbox user (uid 1000) owns ~/.claude and the
        // onboarding marker.  The guest-agent runs as root so files it
        // creates are root-owned by default; claude-code runs as uid 1000
        // and must be able to read credentials and write token refreshes.
        // Use `sh -c` because standalone `chown` may not exist in minimal
        // initramfs images — busybox provides it as a shell built-in.
        if let Err(e) = sandbox
            .exec(
                "sh",
                &[
                    "-c",
                    "chown -R 1000:1000 /home/sandbox/.claude 2>/dev/null; \
                     chown 1000:1000 /home/sandbox/.claude.json 2>/dev/null; \
                     true",
                ],
            )
            .await
        {
            warn!(
                "[vm:{}] claude bootstrap chown exec failed: {} — \
                 claude-code may be unable to read credentials or refresh tokens",
                self.name, e
            );
        }

        Ok(())
    }

    /// Provision skills into the sandbox: write SKILL.md files and MCP config.
    ///
    /// Returns the rendered codex MCP-server TOML instead of writing
    /// `~/.codex/config.toml` when the credential proxy owns that file (a proxied
    /// codex run generates the whole config, and a second writer here would
    /// clobber it); the caller threads the returned TOML into the proxy's
    /// composer. `None` in every other case.
    async fn provision_skills(&self, sandbox: &Sandbox) -> Result<Option<String>> {
        let tag = &self.name;

        // Collect MCP servers for mcp.json
        let mut mcp_servers = serde_json::Map::new();

        for skill in &self.skills {
            match &skill.kind {
                SkillKind::File { path } => {
                    // Read local SKILL.md and write to guest
                    let content = std::fs::read(path).map_err(|e| {
                        crate::Error::Config(format!(
                            "Failed to read skill file {}: {}",
                            path.display(),
                            e
                        ))
                    })?;
                    Self::write_skill_file(sandbox, &skill.name, &content).await?;
                    eprintln!(
                        "[vm:{}] Installing skill '{}' ({})",
                        tag,
                        skill.name,
                        skill
                            .description_text
                            .as_deref()
                            .unwrap_or("no description"),
                    );
                }
                SkillKind::Remote { id } => {
                    eprintln!(
                        "[vm:{}] Fetching remote skill '{}' from skills.sh/{}",
                        tag, skill.name, id
                    );
                    match skill.fetch_remote_content().await {
                        Ok(content) => {
                            Self::write_skill_file(sandbox, &skill.name, content.as_bytes())
                                .await?;
                            eprintln!("[vm:{}] Installed remote skill '{}'", tag, skill.name);
                        }
                        Err(e) => {
                            eprintln!(
                                "[vm:{}] WARN: Failed to fetch skill '{}': {}. Writing fallback.",
                                tag, skill.name, e
                            );
                            let fallback = format!(
                                "# Skill: {} (fetch failed)\n\n\
                                 Source: https://skills.sh/{}\n\n\
                                 Could not fetch: {}\n\n\
                                 Install manually: `npx skills add {}`\n",
                                skill.name, id, e, id
                            );
                            Self::write_skill_file(sandbox, &skill.name, fallback.as_bytes())
                                .await?;
                        }
                    }
                }
                SkillKind::Mcp { command, args, env } => {
                    // Start the MCP server as a background HTTP process inside the
                    // guest, then point Claude Code at it via streamable-HTTP URL.
                    // This avoids Claude Code (Bun) needing to spawn the server as
                    // a child process, which fails in minimal VM environments.
                    let mcp_port = 8222 + mcp_servers.len() as u16;
                    let env_prefix: String =
                        env.iter().map(|(k, v)| format!("{k}='{v}' ")).collect();
                    let args_str: String = args.iter().map(|a| format!(" {a}")).collect();
                    let start_cmd = format!(
                        "{env_prefix}{command}{args_str} --sse --port {mcp_port} \
                         >/dev/null 2>/dev/null &"
                    );
                    match sandbox.exec("sh", &["-c", &start_cmd]).await {
                        Ok(output) if output.exit_code == 0 => {
                            eprintln!(
                                "[vm:{}] Started MCP server '{}' on port {} (HTTP/SSE)",
                                tag, skill.name, mcp_port
                            );
                        }
                        Ok(output) => {
                            eprintln!(
                                "[vm:{}] WARNING: MCP server '{}' start returned exit {}: {}",
                                tag,
                                skill.name,
                                output.exit_code,
                                output.stderr_str()
                            );
                        }
                        Err(e) => {
                            eprintln!(
                                "[vm:{}] WARNING: Failed to start MCP server '{}': {}",
                                tag, skill.name, e
                            );
                        }
                    }

                    // Brief pause for the server to bind the port
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

                    let entry = serde_json::json!({
                        "type": "http",
                        "url": format!("http://127.0.0.1:{mcp_port}/mcp"),
                    });
                    mcp_servers.insert(skill.name.clone(), entry);
                    eprintln!(
                        "[vm:{}] Registering MCP server '{}' (url: http://127.0.0.1:{}/mcp)",
                        tag, skill.name, mcp_port
                    );
                }
                SkillKind::Cli { command } => {
                    eprintln!(
                        "[vm:{}] CLI tool '{}' available at {}",
                        tag, skill.name, command
                    );
                    // CLI binaries are expected to be in the initramfs already
                }
                SkillKind::Agent { command } => {
                    eprintln!(
                        "[vm:{}] Reasoning engine: {} ({})",
                        tag, skill.name, command
                    );
                }
                SkillKind::Oci {
                    image,
                    mount,
                    readonly,
                } => {
                    eprintln!(
                        "[vm:{}] OCI skill '{}': image={}, mount={}, readonly={}",
                        tag, skill.name, image, mount, readonly
                    );
                    // OCI skill provisioning: the image is pulled and extracted
                    // by the host, then mounted into the guest via virtiofs/9p.
                    // The actual pull+extract is deferred to the pipeline/runtime
                    // layer which adds mount configs before sandbox creation.
                    // Here we just ensure the guest PATH includes the skill's bins.
                    let path_extension = format!(
                        "export PATH=\"{}/usr/local/bin:{}/usr/bin:$PATH\"",
                        mount, mount
                    );
                    let profile_path = format!("{}/skills/{}_path.sh", CLAUDE_HOME, skill.name);
                    sandbox
                        .write_file(&profile_path, path_extension.as_bytes())
                        .await?;
                    eprintln!(
                        "[vm:{}] OCI skill '{}' PATH extension -> {}",
                        tag, skill.name, profile_path
                    );
                }
                SkillKind::Inline { content } => {
                    Self::write_skill_file(sandbox, &skill.name, content.as_bytes()).await?;
                    eprintln!(
                        "[vm:{}] Installing inline skill '{}' ({} bytes)",
                        tag,
                        skill.name,
                        content.len(),
                    );
                }
            }
        }

        // Write MCP config if any MCP servers were registered.
        // Claude reads .mcp.json; codex reads ~/.codex/config.toml. The
        // void-mcp HTTP server is the same — only the discovery file differs.
        if !mcp_servers.is_empty() {
            let mcp_config = serde_json::json!({
                "mcpServers": mcp_servers
            });
            let config_str = serde_json::to_string_pretty(&mcp_config).map_err(|e| {
                crate::Error::Config(format!("Failed to serialize MCP config: {}", e))
            })?;
            sandbox
                .write_file(MCP_CONFIG_PATH, config_str.as_bytes())
                .await?;
            eprintln!(
                "[vm:{}] Wrote MCP config ({} servers) to {}",
                tag,
                mcp_servers.len(),
                MCP_CONFIG_PATH,
            );

            if !self.config.llm.supports_claude_settings() {
                let servers: Vec<(String, String)> = mcp_servers
                    .iter()
                    .filter_map(|(name, entry)| {
                        entry
                            .get("url")
                            .and_then(|v| v.as_str())
                            .map(|url| (name.clone(), url.to_string()))
                    })
                    .collect();
                let toml_buf = render_codex_mcp_servers_toml(&servers);
                // A proxied codex run's config.toml is generated wholesale by the
                // credential proxy (provider redirect + these MCP entries in one
                // file); hand the section to that composer instead of writing a
                // file it would clobber.
                if self.config.credential_proxy && matches!(self.config.llm, LlmProvider::Codex) {
                    return Ok(Some(toml_buf));
                }
                if !toml_buf.is_empty() {
                    sandbox
                        .write_file(GUEST_CODEX_CONFIG_PATH, toml_buf.as_bytes())
                        .await?;
                    eprintln!(
                        "[vm:{}] Wrote codex MCP config ({} servers) to {}",
                        tag,
                        mcp_servers.len(),
                        GUEST_CODEX_CONFIG_PATH,
                    );
                }
            }
        }

        Ok(None)
    }

    fn build_full_prompt(&self, input: Option<&[u8]>) -> String {
        let Some(data) = input else {
            return format!(
                "{}\n\nWrite your output to {}.",
                self.prompt, self.config.output_file
            );
        };
        let input_text = String::from_utf8_lossy(data);
        let inline = if input_text.len() > 4000 {
            format!(
                "{}...\n(truncated; full data in /workspace/input.json)",
                &input_text[..4000]
            )
        } else {
            input_text.to_string()
        };
        format!(
            "{}\n\n--- Previous stage output ---\n{}\n--- End previous stage output ---\n\n\
             The above data is also available at /workspace/input.json.\n\
             Write your output to {}.",
            self.prompt, inline, self.config.output_file
        )
    }

    /// Run this Box: provision skills, execute the agent, return the result.
    ///
    /// If `input` is provided, it's written to `/workspace/input.json` before
    /// the agent runs, and the prompt is augmented to reference it.
    pub async fn run(
        self,
        input: Option<&[u8]>,
        telemetry_buffer: Option<TelemetryBuffer>,
    ) -> Result<StageResult> {
        let sandbox = self.sandbox.as_ref().ok_or_else(|| {
            crate::Error::Config("VoidBox not built — call .build() first".into())
        })?;

        // Provision security configuration (resource limits, command allowlist)
        self.provision_security(sandbox).await?;

        // Start guest telemetry (best-effort, don't fail the run)
        let tag = &self.name;
        match sandbox.start_telemetry(telemetry_buffer).await {
            Ok(agg) => {
                agg.set_current_stage(&self.name);
                eprintln!("[vm:{}] Guest telemetry started", tag);
            }
            Err(e) => {
                eprintln!("[vm:{}] Guest telemetry unavailable: {}", tag, e);
            }
        }

        // Provision skills into the guest. A proxied codex run hands its MCP
        // config-section back here for the proxy's config.toml composer.
        let codex_mcp_toml = self.provision_skills(sandbox).await?;

        self.provision_claude_bootstrap(sandbox).await?;

        // Start the credential proxy (opt-in) and capture the guest env to
        // inject at exec time.
        let active_proxy = self
            .maybe_setup_credential_proxy(sandbox, codex_mcp_toml.as_deref().unwrap_or(""))
            .await?;

        let tag = &self.name;

        // Write input data if provided
        if let Some(data) = input {
            sandbox.write_file("/workspace/input.json", data).await?;
            eprintln!(
                "[vm:{}] Writing input ({} bytes) to /workspace/input.json",
                tag,
                data.len()
            );
        }

        let full_prompt = self.build_full_prompt(input);

        eprintln!(
            "[vm:{}] Executing agent | llm={} | prompt_len={} chars",
            tag,
            self.config.llm.description(),
            full_prompt.len()
        );

        let mut extra_args: Vec<String> = Vec::new();
        if self.config.llm.supports_claude_settings() {
            extra_args.extend([
                "--settings".to_string(),
                r#"{"skipWebFetchPreflight":true}"#.to_string(),
            ]);

            let has_mcp = self.skills.iter().any(|s| match &s.kind {
                SkillKind::Mcp { .. } => true,
                SkillKind::Cli { .. }
                | SkillKind::Agent { .. }
                | SkillKind::Remote { .. }
                | SkillKind::File { .. }
                | SkillKind::Oci { .. }
                | SkillKind::Inline { .. } => false,
            });
            if has_mcp {
                extra_args.extend(["--mcp-config".to_string(), MCP_CONFIG_PATH.to_string()]);
            }
        }

        let proxy_env = active_proxy
            .as_ref()
            .map(|p| p.exec_env.clone())
            .unwrap_or_default();

        let tag_clone = tag.to_string();
        let exec_outcome = sandbox
            .exec_agent_streaming(
                &self.config.llm,
                &full_prompt,
                AgentExecOpts {
                    dangerously_skip_permissions: true,
                    extra_args,
                    timeout_secs: self.config.timeout_secs,
                    env: proxy_env,
                },
                |event| match event {
                    crate::observe::claude::AgentStreamEvent::ToolUse(ref tc) => {
                        let summary = tc.tool_summary();
                        if summary.is_empty() {
                            eprintln!("[vm:{}]   tool: {}", tag_clone, tc.tool_name);
                        } else {
                            eprintln!("[vm:{}]   tool: {}  {}", tag_clone, tc.tool_name, summary);
                        }
                    }
                },
            )
            .await;

        // Tear down the per-sandbox proxy listener regardless of exec outcome.
        if let Some(proxy) = active_proxy {
            proxy.teardown().await;
        }
        let mut agent_result = exec_outcome?;

        // Local providers (Ollama) have no real API cost; claude-code
        // still reports a dollar amount using Anthropic pricing, so zero it.
        if self.config.llm.is_local() {
            agent_result.total_cost_usd = 0.0;
        }

        eprintln!(
            "[vm:{}] Agent finished | tokens={}in/{}out | tools={} | cost=${:.4} | error={}",
            tag,
            agent_result.input_tokens,
            agent_result.output_tokens,
            agent_result.tool_calls.len(),
            agent_result.total_cost_usd,
            agent_result.is_error,
        );

        // Try to read the output file
        let file_output = match sandbox.read_file(&self.config.output_file).await {
            Ok(data) if !data.is_empty() => {
                eprintln!(
                    "[vm:{}] Reading output ({} bytes) from {}",
                    tag,
                    data.len(),
                    self.config.output_file
                );
                Some(data)
            }
            _ => None,
        };

        Ok(StageResult {
            box_name: self.name.clone(),
            agent_result,
            file_output,
        })
    }

    /// Run this Box as a long-running service.
    ///
    /// Provisions skills and launches the agent identically to [`run()`](Self::run),
    /// but instead of awaiting completion returns a `ServiceStageHandle` that
    /// lets the caller:
    /// - receive the first output publication via `output_rx`
    /// - stop the service via `stop_tx`
    /// - observe the terminal exit reason via `exit_rx`
    pub async fn run_service(
        self,
        input: Option<&[u8]>,
        telemetry_buffer: Option<TelemetryBuffer>,
    ) -> Result<ServiceStageHandle> {
        let sandbox = self.sandbox.as_ref().ok_or_else(|| {
            crate::Error::Config("VoidBox not built — call .build() first".into())
        })?;

        // ── Provisioning (identical to run()) ──────────────────────────

        self.provision_security(sandbox).await?;

        let tag = self.name.clone();
        match sandbox.start_telemetry(telemetry_buffer).await {
            Ok(agg) => {
                agg.set_current_stage(&tag);
                eprintln!("[vm:{}] Guest telemetry started", tag);
            }
            Err(e) => {
                eprintln!("[vm:{}] Guest telemetry unavailable: {}", tag, e);
            }
        }

        // Service mode and the credential proxy are mutually exclusive (spec
        // validation), so a deferred codex MCP section can never be produced here.
        self.provision_skills(sandbox).await?;

        self.provision_claude_bootstrap(sandbox).await?;

        if let Some(data) = input {
            sandbox.write_file("/workspace/input.json", data).await?;
            eprintln!(
                "[vm:{}] Writing input ({} bytes) to /workspace/input.json",
                tag,
                data.len()
            );
        }

        let full_prompt = self.build_full_prompt(input);

        eprintln!(
            "[vm:{}] Launching service agent | llm={} | prompt_len={} chars",
            tag,
            self.config.llm.description(),
            full_prompt.len()
        );

        // ── Build CLI args ─────────────────────────────────────────────

        let mut extra_args: Vec<String> = Vec::new();
        if self.config.llm.supports_claude_settings() {
            extra_args.extend([
                "--settings".to_string(),
                r#"{"skipWebFetchPreflight":true}"#.to_string(),
            ]);

            let has_mcp = self.skills.iter().any(|s| match &s.kind {
                SkillKind::Mcp { .. } => true,
                SkillKind::Cli { .. }
                | SkillKind::Agent { .. }
                | SkillKind::Remote { .. }
                | SkillKind::File { .. }
                | SkillKind::Oci { .. }
                | SkillKind::Inline { .. } => false,
            });
            if has_mcp {
                extra_args.extend(["--mcp-config".to_string(), MCP_CONFIG_PATH.to_string()]);
            }
        }

        let is_local_llm = self.config.llm.is_local();
        let llm_provider = self.config.llm.clone();
        let output_file = self.config.output_file.clone();
        let box_name = self.name.clone();

        // ── Channels ───────────────────────────────────────────────────

        let (output_tx, output_rx) = tokio::sync::oneshot::channel::<ServicePublication>();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let (exit_tx, exit_rx) = tokio::sync::oneshot::channel::<ServiceExit>();

        // Shared state so both tasks can coordinate without blocking each other.
        let output_tx = Arc::new(tokio::sync::Mutex::new(Some(output_tx)));
        let exited = Arc::new(std::sync::atomic::AtomicBool::new(false));

        // ── Clone sandbox for spawned tasks ────────────────────────────

        let sandbox_agent = Arc::clone(sandbox);
        let sandbox_monitor = Arc::clone(sandbox);

        // ── Spawn agent process task ───────────────────────────────────

        let tag_agent = tag.clone();
        let box_name_agent = box_name.clone();
        let output_file_agent = output_file.clone();
        let output_tx_agent = Arc::clone(&output_tx);
        let exited_agent = Arc::clone(&exited);
        tokio::spawn(async move {
            let tag = tag_agent;

            // timeout_secs = Some(0) means infinite timeout for service mode.
            let result = sandbox_agent.exec_agent_streaming(
                &llm_provider,
                &full_prompt,
                AgentExecOpts {
                    dangerously_skip_permissions: true,
                    extra_args,
                    timeout_secs: Some(0),
                    ..Default::default()
                },
                |event| match event {
                    crate::observe::claude::AgentStreamEvent::ToolUse(ref tc) => {
                        let summary = tc.tool_summary();
                        if summary.is_empty() {
                            eprintln!("[vm:{}]   tool: {}", tag, tc.tool_name);
                        } else {
                            eprintln!("[vm:{}]   tool: {}  {}", tag, tc.tool_name, summary);
                        }
                    }
                },
            );

            // Race: agent finishes naturally vs. stop signal.
            tokio::select! {
                agent_result = result => {
                    match agent_result {
                        Ok(mut res) => {
                            if is_local_llm {
                                res.total_cost_usd = 0.0;
                            }
                            eprintln!(
                                "[vm:{}] Service agent exited | tokens={}in/{}out | cost=${:.4} | error={}",
                                box_name_agent,
                                res.input_tokens,
                                res.output_tokens,
                                res.total_cost_usd,
                                res.is_error,
                            );
                            if res.is_error {
                                if let Some(error_message) = res.error.as_deref() {
                                    error!(
                                        "[vm:{}] Service agent error: {}",
                                        box_name_agent, error_message
                                    );
                                }
                                if !res.result_text.trim().is_empty() {
                                    warn!(
                                        "[vm:{}] Service agent result preview: {}",
                                        box_name_agent,
                                        res.result_text.trim()
                                    );
                                }
                            }

                            // Best-effort exit-time publication. Hard timeout so
                            // a dying sandbox cannot block exit_tx forever.
                            if !res.is_error {
                                let read_result = tokio::time::timeout(
                                    std::time::Duration::from_secs(3),
                                    sandbox_agent.read_file(&output_file_agent),
                                ).await;

                                if let Ok(Ok(data)) = read_result {
                                    if !data.is_empty() {
                                        eprintln!(
                                            "[vm:{}] Service agent: publishing output at exit ({} bytes)",
                                            box_name_agent, data.len()
                                        );
                                        if let Some(tx) = output_tx_agent.lock().await.take() {
                                            let _ = tx.send(ServicePublication {
                                                box_name: box_name_agent.clone(),
                                                output: data,
                                                report: crate::runtime::RunReport {
                                                    name: box_name_agent.clone(),
                                                    kind: "service".to_string(),
                                                    success: true,
                                                    output: output_file_agent.clone(),
                                                    stages: 1,
                                                    total_cost_usd: res.total_cost_usd,
                                                    input_tokens: res.input_tokens,
                                                    output_tokens: res.output_tokens,
                                                },
                                            });
                                        }
                                    }
                                } else {
                                    warn!(
                                        "[vm:{}] Service agent: exit-time output read failed or timed out",
                                        box_name_agent
                                    );
                                }
                            }

                            // Signal exit — unconditional, never depends on publication.
                            exited_agent.store(true, std::sync::atomic::Ordering::SeqCst);
                            info!("[vm:{}] Service agent: sending exit", box_name_agent);
                            let _ = exit_tx.send(ServiceExit::Exited {
                                success: !res.is_error,
                                error: res.error,
                            });
                        }
                        Err(e) => {
                            error!("[vm:{}] Service agent crashed: {}", box_name_agent, e);
                            exited_agent.store(true, std::sync::atomic::Ordering::SeqCst);
                            let _ = exit_tx.send(ServiceExit::Crashed(e.to_string()));
                        }
                    }
                }
                _ = stop_rx => {
                    info!("[vm:{}] Service agent stop requested", box_name_agent);
                    exited_agent.store(true, std::sync::atomic::Ordering::SeqCst);
                    let _ = exit_tx.send(ServiceExit::Canceled);
                }
            }
        });

        // ── Spawn output file monitor task ─────────────────────────────

        let tag_monitor = tag.clone();
        let output_tx_monitor = Arc::clone(&output_tx);
        let exited_monitor = Arc::clone(&exited);
        tokio::spawn(async move {
            let tag = tag_monitor;
            let mut probe_failures = 0u32;

            loop {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;

                // Stop if the agent task already exited.
                if exited_monitor.load(std::sync::atomic::Ordering::SeqCst) {
                    debug!("[vm:{}] Output monitor: agent exited, stopping", tag);
                    return;
                }

                // Stop if the sender was already consumed by the exit fallback.
                if output_tx_monitor.lock().await.is_none() {
                    debug!("[vm:{}] Output monitor: already published, stopping", tag);
                    return;
                }

                // Check if the output file exists (with timeout).
                let exists = match tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    sandbox_monitor.file_exists(&output_file),
                )
                .await
                {
                    Ok(Ok(found)) => found,
                    Ok(Err(e)) => {
                        warn!("[vm:{}] Output monitor: file_exists failed: {}", tag, e);
                        probe_failures += 1;
                        false
                    }
                    Err(_) => {
                        warn!("[vm:{}] Output monitor: file_exists timed out", tag);
                        probe_failures += 1;
                        false
                    }
                };

                if !exists {
                    if probe_failures >= 10 {
                        error!(
                            "[vm:{}] Output monitor: too many probe failures, stopping",
                            tag
                        );
                        return;
                    }
                    continue;
                }

                // File exists — read it (with timeout).
                let data = match tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    sandbox_monitor.read_file(&output_file),
                )
                .await
                {
                    Ok(Ok(data)) if !data.is_empty() => data,
                    Ok(Ok(_)) => {
                        debug!("[vm:{}] Output monitor: file empty, retrying", tag);
                        continue;
                    }
                    Ok(Err(e)) => {
                        warn!("[vm:{}] Output monitor: read failed: {}", tag, e);
                        continue;
                    }
                    Err(_) => {
                        warn!("[vm:{}] Output monitor: read timed out", tag);
                        continue;
                    }
                };

                info!(
                    "[vm:{}] Output monitor: publishing output ({} bytes)",
                    tag,
                    data.len()
                );
                if let Some(tx) = output_tx_monitor.lock().await.take() {
                    let _ = tx.send(ServicePublication {
                        box_name: box_name.clone(),
                        output: data,
                        report: crate::runtime::RunReport {
                            name: box_name.clone(),
                            kind: "service".to_string(),
                            success: true,
                            output: output_file.clone(),
                            stages: 1,
                            total_cost_usd: 0.0,
                            input_tokens: 0,
                            output_tokens: 0,
                        },
                    });
                }
                return; // one-shot
            }
        });

        Ok(ServiceStageHandle {
            output_rx,
            stop_tx,
            exit_rx,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skill::Skill;

    #[test]
    fn proxy_owned_env_covers_provider_keys_and_base_url() {
        assert!(is_proxy_owned_env("ANTHROPIC_API_KEY"));
        assert!(is_proxy_owned_env("OPENAI_API_KEY"));
        // The proxy's redirect must be the single source of the base URL (the
        // Custom provider's env_vars() emits a real one).
        assert!(is_proxy_owned_env("ANTHROPIC_BASE_URL"));
        assert!(!is_proxy_owned_env("HOME"));
    }

    #[test]
    fn resolve_secret_per_provider() {
        // OAuth providers and local providers have no static key to resolve.
        assert!(resolve_provider_secret(&LlmProvider::ClaudePersonal).is_err());
        assert!(resolve_provider_secret(&LlmProvider::ollama("m")).is_err());
        // A Custom provider resolves the key its spec carried (from api_key_env).
        let custom_key = resolve_provider_secret(&LlmProvider::Custom {
            base_url: "https://example.test/v1".into(),
            api_key: Some(crate::llm::ApiKey::new("sk-custom-secret")),
            model: None,
        })
        .expect("custom key resolves");
        assert_eq!(custom_key.expose_secret(), "sk-custom-secret");
        // ...and fails closed when the spec resolved none.
        assert!(resolve_provider_secret(&LlmProvider::Custom {
            base_url: "https://example.test/v1".into(),
            api_key: None,
            model: None,
        })
        .is_err());
    }

    #[test]
    fn provider_servability_is_pure_and_covers_m1b_set() {
        assert!(provider_is_proxy_servable(&LlmProvider::Claude));
        assert!(provider_is_proxy_servable(&LlmProvider::ClaudePersonal));
        assert!(provider_is_proxy_servable(&LlmProvider::Codex));
        assert!(provider_is_proxy_servable(&LlmProvider::Custom {
            base_url: "https://example.test/v1".into(),
            api_key: None,
            model: None,
        }));
        assert!(!provider_is_proxy_servable(&LlmProvider::ollama("m")));
    }

    #[test]
    fn credential_proxy_builder_flag_defaults_off_and_sets() {
        let off = VoidBox::new("b");
        assert!(!off.config.credential_proxy);
        let on = VoidBox::new("b").credential_proxy(true);
        assert!(on.config.credential_proxy);
    }

    #[test]
    fn credential_proxy_rejects_a_staged_credentials_file_r14() {
        // Enabling the proxy while also staging an OAuth credentials file is a
        // config contradiction: the proxy injects host-side, so the file would
        // carry the durable refresh token into the guest. Rejected before boot,
        // ahead of the platform gate, so it fires on every host (R14).
        let vb = VoidBox::new("b")
            .credential_proxy(true)
            .claude_credentials_host_path("/tmp/voidbox-r14-test-creds");
        let err = vb
            .validate_credential_proxy_preconditions()
            .expect_err("proxy + staged credentials file must be rejected");
        assert!(err.to_string().contains("R14"), "got: {err}");
    }

    #[test]
    fn credential_proxy_without_a_staged_credentials_file_passes_r14_check() {
        // Default provider (Claude) is proxy-served and no credentials file is
        // staged, so the provider and R14 checks pass on both supported
        // platforms (Linux/KVM and macOS/VZ).
        let vb = VoidBox::new("b").credential_proxy(true);
        let result = vb.validate_credential_proxy_preconditions();
        assert!(result.is_ok(), "got: {result:?}");
    }

    #[test]
    fn filter_withheld_env_strips_proxy_owned_vars_only_when_withholding() {
        // Deterministic (no host-env dependence): the keys are present in the
        // input, so the assertions can't pass vacuously.
        let env = vec![
            ("ANTHROPIC_API_KEY".to_string(), "sk-ant-REAL".to_string()),
            (
                "ANTHROPIC_BASE_URL".to_string(),
                "https://gateway.example.com/v1".to_string(),
            ),
            ("HOME".to_string(), "/home/sandbox".to_string()),
        ];

        // Withholding removes the credential key AND the provider base URL (the
        // proxy's redirect must be the single source of it); everything else stays.
        let withheld = filter_withheld_env(env.clone(), true);
        assert!(!withheld.iter().any(|(k, _)| k == "ANTHROPIC_API_KEY"));
        assert!(!withheld.iter().any(|(k, _)| k == "ANTHROPIC_BASE_URL"));
        assert!(withheld.iter().any(|(k, _)| k == "HOME"));

        // Not withholding keeps everything.
        let kept = filter_withheld_env(env, false);
        assert!(kept
            .iter()
            .any(|(k, v)| k == "ANTHROPIC_API_KEY" && v == "sk-ant-REAL"));
        assert!(kept.iter().any(|(k, _)| k == "ANTHROPIC_BASE_URL"));
    }

    #[test]
    fn credential_proxy_rejects_unsupported_provider_before_provisioning() {
        // A provider without a host-side credential (local) under
        // `credential_proxy` must fail at build time — before any env is staged
        // or the guest boots — rather than staging env and only aborting once
        // `maybe_setup_credential_proxy` runs. Provider support is
        // platform-independent, so this asserts the provider error on both Linux
        // and macOS.
        let err = match VoidBox::new("b")
            .llm(LlmProvider::ollama("m"))
            .credential_proxy(true)
            .build()
        {
            Ok(_) => panic!("local provider under credential_proxy must fail to build"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(
            msg.contains("holds no host-side credential"),
            "expected a provider-unsupported error, got: {msg}"
        );
    }

    #[test]
    fn credential_proxy_rejects_plaintext_custom_base_url_before_boot() {
        // A Custom provider with an http:// base URL cannot be proxied — the
        // proxy injects the key at TLS egress. Rejected at build time with the
        // scheme named, on every host (no platform or host-state dependence).
        let err = match VoidBox::new("b")
            .llm(LlmProvider::Custom {
                base_url: "http://gateway.example.com/v1".into(),
                api_key: Some(crate::llm::ApiKey::new("sk-x")),
                model: None,
            })
            .credential_proxy(true)
            .build()
        {
            Ok(_) => panic!("plaintext custom base_url under credential_proxy must fail"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("https"), "got: {err}");
    }

    #[test]
    fn credential_proxy_rejects_credential_home_mounts_r14() {
        // A mount over an agent credential home would carry a durable secret in
        // beside the proxy placeholders; rejected before boot (R14). Uses Claude
        // as the provider so the check is deterministic (no codex host state).
        let err = VoidBox::new("b")
            .llm(LlmProvider::Claude)
            .credential_proxy(true)
            .mount(crate::backend::MountConfig {
                host_path: "/tmp/voidbox-r14-codex-creds".into(),
                guest_path: "/home/sandbox/.codex".into(),
                read_only: false,
            })
            .validate_credential_proxy_preconditions()
            .expect_err("credential-home mount under the proxy must be rejected");
        assert!(err.to_string().contains("R14"), "got: {err}");
    }

    #[test]
    fn claude_under_proxy_withholds_provider_secret() {
        let vb = VoidBox::new("b")
            .llm(LlmProvider::Claude)
            .credential_proxy(true);
        assert!(vb.withhold_provider_secret());
        // Proxy off: nothing is withheld.
        let vb_off = VoidBox::new("b").llm(LlmProvider::Claude);
        assert!(!vb_off.withhold_provider_secret());
    }

    #[test]
    fn r14_audit_catches_real_key_in_user_env() {
        // A real provider key copied into a user env override under a
        // non-withheld name reaches the guest — the R14 audit set must include it
        // so the gate fails. Auditing only the proxy env would miss this.
        let secret = "sk-ant-REALKEY-must-not-leak";
        let vb = VoidBox::new("b")
            .llm(LlmProvider::Claude)
            .credential_proxy(true)
            .env("BACKUP_ANTHROPIC_KEY", secret);

        let proxy_env = vec![(
            "ANTHROPIC_API_KEY".to_string(),
            crate::proxy::provision::ANTHROPIC_KEY_PLACEHOLDER.to_string(),
        )];
        let audit_env = vb.r14_audit_env(&proxy_env);

        assert!(
            audit_env
                .iter()
                .any(|(k, v)| k == "BACKUP_ANTHROPIC_KEY" && v == secret),
            "audit set must include user env overrides"
        );
        assert!(
            crate::proxy::assert_no_real_credential(&audit_env, &[], secret).is_err(),
            "R14 gate must reject a real key staged via a user env override"
        );
    }

    #[test]
    fn test_agent_box_builder() {
        let reasoning = Skill::agent("claude-code").description("Autonomous reasoning");

        let market_data = Skill::mcp("market-data-mcp").description("Market data provider");

        let ab = VoidBox::new("data_analyst")
            .skill(market_data)
            .skill(reasoning)
            .memory_mb(512)
            .prompt("Fetch OHLCV data for AAPL")
            .mock()
            .build()
            .unwrap();

        assert_eq!(ab.name, "data_analyst");
        assert_eq!(ab.skills.len(), 2);
        assert!(!ab.prompt.is_empty());
        assert!(ab.sandbox.is_some());
    }

    #[tokio::test]
    async fn test_agent_box_run_mock() {
        let reasoning = Skill::agent("claude-code");

        let ab = VoidBox::new("test_box")
            .skill(reasoning)
            .prompt("Do something")
            .mock()
            .build()
            .unwrap();

        // Mock sandbox will return default claude-code response
        let result = ab.run(None, None).await.unwrap();
        assert_eq!(result.box_name, "test_box");
    }
}
