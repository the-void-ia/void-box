//! LLM Provider configuration for VoidBox.
//!
//! By default, `VoidBox` uses Claude via the Anthropic API. The `LlmProvider`
//! enum allows opting in to alternative backends -- most notably a local
//! [Ollama](https://ollama.com) instance -- without changing anything else in
//! the execution pipeline.
//!
//! # How it works
//!
//! Ollama v0.14+ supports the Anthropic Messages API natively, so `claude-code`
//! in the guest VM can talk to Ollama by simply setting environment variables:
//!
//! ```text
//! ANTHROPIC_BASE_URL=http://10.0.2.2:11434   (SLIRP gateway → host localhost)
//! ANTHROPIC_API_KEY=""                        (must be empty)
//! ANTHROPIC_AUTH_TOKEN=ollama
//! ```
//!
//! The guest binary, output format, and parser remain unchanged.
//!
//! # Example
//!
//! ```no_run
//! use void_box::llm::LlmProvider;
//! use void_box::agent_box::VoidBox;
//!
//! # fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! // Default: uses Claude API (requires ANTHROPIC_API_KEY)
//! let claude_box = VoidBox::new("default").prompt("hello").build()?;
//!
//! // Opt-in: use a local Ollama model
//! let ollama_box = VoidBox::new("local")
//!     .llm(LlmProvider::ollama("qwen3-coder"))
//!     .prompt("hello")
//!     .build()?;
//! # Ok(())
//! # }
//! ```

/// The SLIRP gateway IP as seen from the guest VM.
/// Traffic to this IP is translated to `127.0.0.1` on the host,
/// allowing the guest to reach host services like Ollama.
const SLIRP_GATEWAY: &str = "10.0.2.2";

/// The guest binary name for Claude Code and all Claude-compatible
/// providers (Ollama, LmStudio, Custom, ClaudePersonal). These all route
/// through the same Bun-built `claude-code` binary via `ANTHROPIC_BASE_URL`.
const CLAUDE_CODE_BINARY: &str = "claude-code";

/// LLM backend provider for an [`VoidBox`](crate::agent_box::VoidBox).
///
/// Determines which LLM service the agent talks to. The provider is
/// translated into environment variables injected into the guest VM.
#[derive(Debug, Default, Clone)]
pub enum LlmProvider {
    /// Anthropic Claude API (default).
    ///
    /// Requires `ANTHROPIC_API_KEY` in the host environment.
    /// No extra env vars are injected.
    #[default]
    Claude,

    /// Local Ollama instance running on the host.
    ///
    /// Ollama must be running (`ollama serve`) and the model must be
    /// pulled (`ollama pull <model>`). The guest reaches Ollama through
    /// the SLIRP gateway IP, which is mapped to `127.0.0.1` on the host.
    Ollama {
        /// Model name (e.g. `"qwen3-coder"`, `"llama3.1"`, `"deepseek-coder-v2"`).
        model: String,
        /// Ollama host URL as seen from the guest.
        /// Default: `http://10.0.2.2:11434` (SLIRP gateway → host localhost).
        host: Option<String>,
    },

    /// Local LM Studio instance running on the host.
    ///
    /// LM Studio must be running with the local server enabled (default port 1234).
    /// The guest reaches it through the SLIRP gateway IP (`10.0.2.2`), which is
    /// mapped to `127.0.0.1` on the host.
    ///
    /// LM Studio 0.3.x+ exposes an Anthropic-compatible API; no proxy needed.
    LmStudio {
        /// Model identifier as shown in LM Studio (e.g. `"qwen2.5-coder-7b-instruct"`).
        model: String,
        /// LM Studio host URL as seen from the guest.
        /// Default: `http://10.0.2.2:1234` (SLIRP gateway → host localhost).
        host: Option<String>,
    },

    /// Claude using personal OAuth credentials (from `claude auth login`).
    ///
    /// Unlike [`Claude`](LlmProvider::Claude), this does not require
    /// `ANTHROPIC_API_KEY`. Instead, the runtime discovers OAuth credentials
    /// from the host (macOS Keychain or `~/.claude/.credentials.json`) and
    /// mounts them into the guest at `/home/sandbox/.claude`.
    ClaudePersonal,

    /// Any Anthropic-compatible API endpoint.
    ///
    /// Use this for OpenRouter, Together AI, or self-hosted vLLM/TGI with
    /// an Anthropic-compatible adapter.
    Custom {
        /// Base URL of the API (e.g. `"https://openrouter.ai/api/v1"`).
        base_url: String,
        /// API key (optional for local services).
        api_key: Option<String>,
        /// Model name override.
        model: Option<String>,
    },

    /// OpenAI Codex CLI.
    ///
    /// Auth is provided primarily via a mounted `~/.codex/auth.json`
    /// (from `codex login` on the host), with `OPENAI_API_KEY` available
    /// as a fallback for endpoints that accept it.
    ///
    /// The guest executes the bundled `codex` binary (see
    /// `scripts/build_codex_rootfs.sh`) with
    /// `codex exec --json --dangerously-bypass-approvals-and-sandbox
    /// --skip-git-repo-check <prompt>`.
    ///
    /// Output is emitted as JSONL and parsed via the Codex observer
    /// (`crate::observe::codex::parse_codex_line`).
    Codex,
}

/// Stream observer dispatcher for `Sandbox::exec_agent_streaming`.
///
/// Each provider tells the sandbox which parser to use for its agent's
/// stdout. The sandbox dispatches to the matching `parse_*_line` function
/// from the appropriate `observe::*` module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObserverKind {
    /// Claude Code's `--output-format stream-json` JSONL events.
    /// Parsed by `crate::observe::claude::parse_jsonl_line`.
    ClaudeStreamJson,
    /// Codex's `exec --json` JSONL events.
    /// Parsed by `crate::observe::codex::parse_codex_line`.
    Codex,
}

impl LlmProvider {
    // -- Constructors --

    /// Create an Ollama provider with the given model name.
    ///
    /// ```
    /// use void_box::llm::LlmProvider;
    /// let provider = LlmProvider::ollama("qwen3-coder");
    /// ```
    pub fn ollama(model: impl Into<String>) -> Self {
        LlmProvider::Ollama {
            model: model.into(),
            host: None,
        }
    }

    /// Create an Ollama provider with a custom host URL.
    ///
    /// Use this when Ollama is not on the default port, or is running
    /// on a different machine accessible from the host.
    pub fn ollama_with_host(model: impl Into<String>, host: impl Into<String>) -> Self {
        LlmProvider::Ollama {
            model: model.into(),
            host: Some(host.into()),
        }
    }

    /// Create an LM Studio provider with the given model identifier.
    pub fn lm_studio(model: impl Into<String>) -> Self {
        LlmProvider::LmStudio {
            model: model.into(),
            host: None,
        }
    }

    /// Create an LM Studio provider with a custom host URL.
    ///
    /// Use this when LM Studio is not on the default port, or is running
    /// on a different machine accessible from the host.
    pub fn lm_studio_with_host(model: impl Into<String>, host: impl Into<String>) -> Self {
        LlmProvider::LmStudio {
            model: model.into(),
            host: Some(host.into()),
        }
    }

    /// Create a custom provider with the given base URL.
    pub fn custom(base_url: impl Into<String>) -> Self {
        LlmProvider::Custom {
            base_url: base_url.into(),
            api_key: None,
            model: None,
        }
    }

    // -- Builder methods --

    /// Set the API key (for Custom provider).
    pub fn api_key(mut self, key: impl Into<String>) -> Self {
        if let LlmProvider::Custom {
            ref mut api_key, ..
        } = self
        {
            *api_key = Some(key.into());
        }
        self
    }

    /// Set the model name (for Custom provider).
    pub fn model(mut self, name: impl Into<String>) -> Self {
        match &mut self {
            LlmProvider::Custom { ref mut model, .. } => {
                *model = Some(name.into());
            }
            LlmProvider::Ollama {
                model: ref mut m, ..
            } => {
                *m = name.into();
            }
            LlmProvider::LmStudio {
                model: ref mut m, ..
            } => {
                *m = name.into();
            }
            _ => {}
        }
        self
    }

    // -- Provider-aware exec helpers --

    /// Binary name that the guest-agent exec's for this provider.
    ///
    /// Used by `Sandbox::exec_agent_streaming` to resolve which bundled
    /// agent binary to run inside the VM. Each flavor's `build_*_rootfs.sh`
    /// script installs the matching binary into `/usr/local/bin/`.
    pub fn binary_name(&self) -> &'static str {
        match self {
            LlmProvider::Claude => CLAUDE_CODE_BINARY,
            LlmProvider::ClaudePersonal => CLAUDE_CODE_BINARY,
            LlmProvider::Ollama { .. } => CLAUDE_CODE_BINARY,
            LlmProvider::LmStudio { .. } => CLAUDE_CODE_BINARY,
            LlmProvider::Custom { .. } => CLAUDE_CODE_BINARY,
            LlmProvider::Codex => "codex",
        }
    }

    /// Stream observer to use for this provider's agent stdout.
    ///
    /// Drives dispatch in `Sandbox::exec_agent_streaming`: each
    /// [`ObserverKind`] maps to a different `parse_*_line` function.
    pub fn observer_kind(&self) -> ObserverKind {
        match self {
            LlmProvider::Claude
            | LlmProvider::ClaudePersonal
            | LlmProvider::Ollama { .. }
            | LlmProvider::LmStudio { .. }
            | LlmProvider::Custom { .. } => ObserverKind::ClaudeStreamJson,
            LlmProvider::Codex => ObserverKind::Codex,
        }
    }

    /// Whether this provider understands the Claude-specific `--settings`
    /// and `--mcp-config` CLI flags.
    ///
    /// Claude and Claude-compatible proxies (Ollama, LmStudio, Custom)
    /// return `true`; Codex returns `false`. Used by `agent_box.rs` to gate
    /// flag emission on the exec command line.
    pub fn supports_claude_settings(&self) -> bool {
        match self {
            LlmProvider::Claude
            | LlmProvider::ClaudePersonal
            | LlmProvider::Ollama { .. }
            | LlmProvider::LmStudio { .. }
            | LlmProvider::Custom { .. } => true,
            LlmProvider::Codex => false,
        }
    }

    /// Build the full `exec` argument vector for this provider.
    ///
    /// Returns the complete args list (subcommand, flags, prompt) that the
    /// guest-agent passes to the agent binary. The caller pairs this with
    /// [`binary_name`](Self::binary_name) to form the full exec invocation.
    ///
    /// Provider-specific args from `cli_args()` (for example,
    /// Ollama's `--model <name>`) are already folded into the Claude-shape
    /// variants. **Callers must NOT separately append `cli_args()` output**
    /// or they will produce duplicate flags.
    ///
    /// - `prompt`: the user prompt text.
    /// - `dangerously_skip_permissions`: whether to pass the bypass-approvals
    ///   flag (Claude's `--dangerously-skip-permissions` or Codex's
    ///   `--dangerously-bypass-approvals-and-sandbox`).
    /// - `extra_args`: caller-supplied extra args appended after the
    ///   provider-specific args and (for Codex) before the trailing prompt
    ///   positional.
    pub fn build_exec_args(
        &self,
        prompt: &str,
        dangerously_skip_permissions: bool,
        extra_args: &[String],
    ) -> Vec<String> {
        match self {
            LlmProvider::Claude
            | LlmProvider::ClaudePersonal
            | LlmProvider::Ollama { .. }
            | LlmProvider::LmStudio { .. }
            | LlmProvider::Custom { .. } => {
                let mut args = vec![
                    "-p".to_string(),
                    prompt.to_string(),
                    "--output-format".to_string(),
                    "stream-json".to_string(),
                    "--verbose".to_string(),
                ];
                if dangerously_skip_permissions {
                    args.push("--dangerously-skip-permissions".to_string());
                }
                for provider_arg in self.cli_args() {
                    args.push(provider_arg);
                }
                for extra in extra_args {
                    args.push(extra.clone());
                }
                args
            }
            LlmProvider::Codex => {
                let mut args = vec![
                    "exec".to_string(),
                    "--json".to_string(),
                    "--skip-git-repo-check".to_string(),
                ];
                if dangerously_skip_permissions {
                    args.push("--dangerously-bypass-approvals-and-sandbox".to_string());
                }
                for extra in extra_args {
                    args.push(extra.clone());
                }
                args.push(prompt.to_string());
                args
            }
        }
    }

    // -- Internal helpers --

    /// Generate extra CLI arguments for `claude-code`.
    ///
    /// For Ollama and Custom providers this returns `["--model", "<name>"]`
    /// so `claude-code` knows which model to request. Per the Ollama docs
    /// (<https://docs.ollama.com/integrations/claude-code>), `--model` accepts
    /// arbitrary Ollama model names when `ANTHROPIC_API_KEY` is empty.
    pub(crate) fn cli_args(&self) -> Vec<String> {
        match self {
            LlmProvider::Claude | LlmProvider::ClaudePersonal | LlmProvider::Codex => Vec::new(),
            LlmProvider::Ollama { model, .. } => {
                vec!["--model".into(), model.clone()]
            }
            LlmProvider::LmStudio { model, .. } => {
                vec!["--model".into(), model.clone()]
            }
            LlmProvider::Custom { model: Some(m), .. } => {
                vec!["--model".into(), m.clone()]
            }
            LlmProvider::Custom { model: None, .. } => Vec::new(),
        }
    }

    /// Generates the environment variables to inject into the guest VM.
    pub fn env_vars(&self) -> Vec<(String, String)> {
        match self {
            LlmProvider::Claude => {
                // Pass through host ANTHROPIC_API_KEY if available
                let mut vars = vec![
                    // Belt-and-suspenders: the guest-agent already sets HOME=/home/sandbox
                    // for child processes (uid=1000), but we also pass it from the host
                    // to ensure correctness even if the guest-agent is outdated.
                    ("HOME".into(), "/home/sandbox".into()),
                ];
                if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
                    vars.push(("ANTHROPIC_API_KEY".into(), key));
                }
                vars
            }
            LlmProvider::ClaudePersonal => {
                // No API key needed — the guest reads OAuth tokens from the
                // mounted credentials file at $HOME/.claude/.credentials.json.
                vec![("HOME".into(), "/home/sandbox".into())]
            }
            LlmProvider::Ollama { host, .. } => {
                let base_url = host
                    .clone()
                    .unwrap_or_else(|| format!("http://{}:11434", SLIRP_GATEWAY));

                // Per Ollama docs (https://docs.ollama.com/integrations/claude-code):
                //   ANTHROPIC_API_KEY must be empty (not a dummy value)
                //   ANTHROPIC_AUTH_TOKEN must be "ollama"
                vec![
                    ("ANTHROPIC_BASE_URL".into(), base_url),
                    ("ANTHROPIC_API_KEY".into(), String::new()),
                    ("ANTHROPIC_AUTH_TOKEN".into(), "ollama".into()),
                    // Belt-and-suspenders: see Claude variant comment above.
                    ("HOME".into(), "/home/sandbox".into()),
                ]
            }
            LlmProvider::LmStudio { host, .. } => {
                let base_url = host
                    .clone()
                    .unwrap_or_else(|| format!("http://{}:1234", SLIRP_GATEWAY));
                // LM Studio requires a non-empty API key; "lm-studio" is the
                // conventional placeholder value.
                vec![
                    ("ANTHROPIC_BASE_URL".into(), base_url),
                    ("ANTHROPIC_API_KEY".into(), "lm-studio".into()),
                    ("HOME".into(), "/home/sandbox".into()),
                ]
            }
            LlmProvider::Custom {
                base_url, api_key, ..
            } => {
                let mut vars = vec![
                    ("ANTHROPIC_BASE_URL".into(), base_url.clone()),
                    // Belt-and-suspenders: see Claude variant comment above.
                    ("HOME".into(), "/home/sandbox".into()),
                ];
                if let Some(key) = api_key {
                    vars.push(("ANTHROPIC_API_KEY".into(), key.clone()));
                }
                vars
            }
            LlmProvider::Codex => {
                let mut vars = vec![("HOME".into(), "/home/sandbox".into())];
                if let Ok(key) = std::env::var("OPENAI_API_KEY") {
                    vars.push(("OPENAI_API_KEY".into(), key));
                }
                vars
            }
        }
    }

    /// Whether this provider runs locally (no real API cost).
    ///
    /// When true, `total_cost_usd` reported by claude-code is meaningless
    /// (it applies Anthropic pricing to local model tokens) and should be
    /// zeroed in the final report.
    pub(crate) fn is_local(&self) -> bool {
        matches!(
            self,
            LlmProvider::Ollama { .. } | LlmProvider::LmStudio { .. }
        )
    }

    /// Whether this provider requires network access from the guest.
    pub(crate) fn requires_network(&self) -> bool {
        // All providers need network: Claude for api.anthropic.com,
        // Ollama for the SLIRP gateway, Custom for arbitrary endpoints.
        true
    }

    /// Human-readable description of the provider (for logging).
    pub fn description(&self) -> String {
        match self {
            LlmProvider::Claude => "Claude (Anthropic API)".into(),
            LlmProvider::ClaudePersonal => "Claude (personal OAuth)".into(),
            LlmProvider::Ollama { model, host } => {
                let h = host.as_deref().unwrap_or("localhost:11434");
                format!("Ollama ({} @ {})", model, h)
            }
            LlmProvider::LmStudio { model, host } => {
                let h = host.as_deref().unwrap_or("localhost:1234");
                format!("LM Studio ({} @ {})", model, h)
            }
            LlmProvider::Custom {
                base_url, model, ..
            } => {
                let m = model.as_deref().unwrap_or("default");
                format!("Custom ({} @ {})", m, base_url)
            }
            LlmProvider::Codex => "Codex (OpenAI API)".into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Display
// ---------------------------------------------------------------------------

impl std::fmt::Display for LlmProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.description())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_claude_env_vars() {
        // Claude provider always sets HOME, optionally ANTHROPIC_API_KEY
        let provider = LlmProvider::Claude;
        let vars = provider.env_vars();
        let map: std::collections::HashMap<_, _> = vars.into_iter().collect();
        assert_eq!(map.get("HOME").unwrap(), "/home/sandbox");
    }

    #[test]
    fn test_ollama_env_vars() {
        let provider = LlmProvider::ollama("qwen3-coder");
        let vars = provider.env_vars();

        let map: std::collections::HashMap<_, _> = vars.into_iter().collect();
        assert_eq!(
            map.get("ANTHROPIC_BASE_URL").unwrap(),
            "http://10.0.2.2:11434"
        );
        // Per Ollama docs: ANTHROPIC_API_KEY must be empty, AUTH_TOKEN is "ollama"
        assert_eq!(map.get("ANTHROPIC_API_KEY").unwrap(), "");
        assert_eq!(map.get("ANTHROPIC_AUTH_TOKEN").unwrap(), "ollama");
        assert_eq!(map.get("HOME").unwrap(), "/home/sandbox");
    }

    #[test]
    fn test_ollama_custom_host() {
        let provider = LlmProvider::ollama_with_host("llama3.1", "http://10.0.2.2:8080");
        let vars = provider.env_vars();

        let map: std::collections::HashMap<_, _> = vars.into_iter().collect();
        assert_eq!(
            map.get("ANTHROPIC_BASE_URL").unwrap(),
            "http://10.0.2.2:8080"
        );
        assert_eq!(map.get("HOME").unwrap(), "/home/sandbox");
    }

    #[test]
    fn test_custom_env_vars() {
        let provider = LlmProvider::custom("https://openrouter.ai/api/v1")
            .api_key("sk-or-xxx")
            .model("anthropic/claude-3.5-sonnet");
        let vars = provider.env_vars();

        let map: std::collections::HashMap<_, _> = vars.into_iter().collect();
        assert_eq!(
            map.get("ANTHROPIC_BASE_URL").unwrap(),
            "https://openrouter.ai/api/v1"
        );
        assert_eq!(map.get("ANTHROPIC_API_KEY").unwrap(), "sk-or-xxx");
        assert_eq!(map.get("HOME").unwrap(), "/home/sandbox");
    }

    #[test]
    fn test_custom_minimal() {
        let provider = LlmProvider::custom("http://localhost:8000");
        let vars = provider.env_vars();

        let map: std::collections::HashMap<_, _> = vars.into_iter().collect();
        assert_eq!(
            map.get("ANTHROPIC_BASE_URL").unwrap(),
            "http://localhost:8000"
        );
        assert!(!map.contains_key("ANTHROPIC_API_KEY"));
        assert!(!map.contains_key("CLAUDE_MODEL"));
    }

    #[test]
    fn test_description() {
        assert_eq!(LlmProvider::Claude.description(), "Claude (Anthropic API)");
        assert_eq!(
            LlmProvider::ollama("qwen3-coder").description(),
            "Ollama (qwen3-coder @ localhost:11434)"
        );
        assert_eq!(
            LlmProvider::custom("https://example.com")
                .model("gpt-4")
                .description(),
            "Custom (gpt-4 @ https://example.com)"
        );
    }

    #[test]
    fn test_default_is_claude() {
        let provider = LlmProvider::default();
        assert!(matches!(provider, LlmProvider::Claude));
    }

    #[test]
    fn test_requires_network() {
        assert!(LlmProvider::Claude.requires_network());
        assert!(LlmProvider::ollama("x").requires_network());
        assert!(LlmProvider::custom("x").requires_network());
    }

    #[test]
    fn test_cli_args_claude() {
        assert!(LlmProvider::Claude.cli_args().is_empty());
    }

    #[test]
    fn test_cli_args_ollama() {
        let provider = LlmProvider::ollama("qwen3-coder");
        assert_eq!(provider.cli_args(), vec!["--model", "qwen3-coder"]);
    }

    #[test]
    fn test_cli_args_custom_with_model() {
        let provider = LlmProvider::custom("http://localhost:8000").model("my-model");
        assert_eq!(provider.cli_args(), vec!["--model", "my-model"]);
    }

    #[test]
    fn test_cli_args_custom_without_model() {
        let provider = LlmProvider::custom("http://localhost:8000");
        assert!(provider.cli_args().is_empty());
    }

    #[test]
    fn test_lm_studio_env_vars() {
        let provider = LlmProvider::lm_studio("qwen2.5-coder-7b-instruct");
        let vars = provider.env_vars();
        let map: std::collections::HashMap<_, _> = vars.into_iter().collect();
        assert_eq!(
            map.get("ANTHROPIC_BASE_URL").unwrap(),
            "http://10.0.2.2:1234"
        );
        assert_eq!(map.get("ANTHROPIC_API_KEY").unwrap(), "lm-studio");
        assert_eq!(map.get("HOME").unwrap(), "/home/sandbox");
    }

    #[test]
    fn test_lm_studio_custom_host() {
        let provider = LlmProvider::lm_studio_with_host("model", "http://10.0.2.2:5678");
        let vars = provider.env_vars();
        let map: std::collections::HashMap<_, _> = vars.into_iter().collect();
        assert_eq!(
            map.get("ANTHROPIC_BASE_URL").unwrap(),
            "http://10.0.2.2:5678"
        );
    }

    #[test]
    fn test_lm_studio_cli_args() {
        let provider = LlmProvider::lm_studio("qwen2.5-coder-7b-instruct");
        assert_eq!(
            provider.cli_args(),
            vec!["--model", "qwen2.5-coder-7b-instruct"]
        );
    }

    #[test]
    fn test_lm_studio_is_local() {
        assert!(LlmProvider::lm_studio("x").is_local());
    }

    #[test]
    fn test_claude_personal_env_vars() {
        let provider = LlmProvider::ClaudePersonal;
        let vars = provider.env_vars();
        let map: std::collections::HashMap<_, _> = vars.into_iter().collect();
        assert_eq!(map.get("HOME").unwrap(), "/home/sandbox");
        assert!(!map.contains_key("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn test_claude_personal_cli_args() {
        assert!(LlmProvider::ClaudePersonal.cli_args().is_empty());
    }

    #[test]
    fn test_claude_personal_is_not_local() {
        assert!(!LlmProvider::ClaudePersonal.is_local());
    }

    #[test]
    fn test_claude_personal_description() {
        assert_eq!(
            LlmProvider::ClaudePersonal.description(),
            "Claude (personal OAuth)"
        );
    }

    #[test]
    fn test_claude_personal_requires_network() {
        assert!(LlmProvider::ClaudePersonal.requires_network());
    }

    #[test]
    fn test_lm_studio_description() {
        assert_eq!(
            LlmProvider::lm_studio("qwen2.5-coder-7b-instruct").description(),
            "LM Studio (qwen2.5-coder-7b-instruct @ localhost:1234)"
        );
    }

    #[test]
    fn test_codex_env_vars() {
        let provider = LlmProvider::Codex;
        let vars = provider.env_vars();
        let mut keys: Vec<&str> = vars.iter().map(|(k, _)| k.as_str()).collect();
        keys.sort();
        assert!(keys.contains(&"HOME"));
        assert!(vars
            .iter()
            .any(|(k, v)| k == "HOME" && v == "/home/sandbox"));
    }

    #[test]
    fn test_codex_env_vars_openai_api_key_conditional() {
        // Capture and clear any existing value so the "unset" branch is
        // deterministic regardless of the developer's shell environment.
        let prior = std::env::var("OPENAI_API_KEY").ok();

        // Absent case
        std::env::remove_var("OPENAI_API_KEY");
        let vars_absent = LlmProvider::Codex.env_vars();
        let absent_found = vars_absent.iter().any(|(k, _)| k == "OPENAI_API_KEY");

        // Present case
        std::env::set_var("OPENAI_API_KEY", "test-key-xyz");
        let vars_present = LlmProvider::Codex.env_vars();
        let present_ok = vars_present
            .iter()
            .any(|(k, v)| k == "OPENAI_API_KEY" && v == "test-key-xyz");

        // Restore
        match prior {
            Some(v) => std::env::set_var("OPENAI_API_KEY", v),
            None => std::env::remove_var("OPENAI_API_KEY"),
        }

        assert!(
            !absent_found,
            "OPENAI_API_KEY should be absent when not set on host"
        );
        assert!(present_ok, "OPENAI_API_KEY should be forwarded when set");
    }

    #[test]
    fn test_codex_observer_kind() {
        assert_eq!(LlmProvider::Codex.observer_kind(), ObserverKind::Codex);
    }

    #[test]
    fn test_claude_shaped_observer_kinds() {
        assert_eq!(
            LlmProvider::Claude.observer_kind(),
            ObserverKind::ClaudeStreamJson
        );
        assert_eq!(
            LlmProvider::ClaudePersonal.observer_kind(),
            ObserverKind::ClaudeStreamJson
        );
        assert_eq!(
            LlmProvider::ollama("test-model").observer_kind(),
            ObserverKind::ClaudeStreamJson
        );
        assert_eq!(
            LlmProvider::lm_studio("test-model").observer_kind(),
            ObserverKind::ClaudeStreamJson
        );
        assert_eq!(
            LlmProvider::custom("http://localhost:1234").observer_kind(),
            ObserverKind::ClaudeStreamJson
        );
    }

    #[test]
    fn test_codex_binary_name() {
        let provider = LlmProvider::Codex;
        assert_eq!(provider.binary_name(), "codex");
    }

    #[test]
    fn test_claude_shaped_binary_names_all_return_claude_code() {
        assert_eq!(LlmProvider::Claude.binary_name(), "claude-code");
        assert_eq!(LlmProvider::ClaudePersonal.binary_name(), "claude-code");
        assert_eq!(
            LlmProvider::ollama("test-model").binary_name(),
            "claude-code"
        );
        assert_eq!(
            LlmProvider::lm_studio("test-model").binary_name(),
            "claude-code"
        );
        assert_eq!(
            LlmProvider::custom("http://localhost:1234").binary_name(),
            "claude-code"
        );
    }

    #[test]
    fn test_codex_supports_claude_settings() {
        let provider = LlmProvider::Codex;
        assert!(!provider.supports_claude_settings());
    }

    #[test]
    fn test_claude_supports_claude_settings() {
        assert!(LlmProvider::Claude.supports_claude_settings());
        assert!(LlmProvider::ClaudePersonal.supports_claude_settings());
    }

    #[test]
    fn test_codex_is_not_local() {
        assert!(!LlmProvider::Codex.is_local());
    }

    #[test]
    fn test_codex_requires_network() {
        assert!(LlmProvider::Codex.requires_network());
    }

    #[test]
    fn test_codex_build_exec_args_contains_prompt() {
        let provider = LlmProvider::Codex;
        let args = provider.build_exec_args("hello world", true, &[]);
        assert_eq!(args[0], "exec");
        assert!(args.contains(&"--json".to_string()));
        assert!(args.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()));
        assert!(args.contains(&"--skip-git-repo-check".to_string()));
        assert_eq!(args.last().unwrap(), "hello world");
    }

    #[test]
    fn test_codex_build_exec_args_without_skip_permissions() {
        let provider = LlmProvider::Codex;
        let args = provider.build_exec_args("prompt", false, &[]);
        assert!(!args.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()));
    }
}
