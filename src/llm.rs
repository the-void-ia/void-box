//! LLM Provider configuration for AgentBox.
//!
//! By default, `AgentBox` uses Claude via the Anthropic API. The `LlmProvider`
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
//! use void_box::agent_box::AgentBox;
//!
//! # fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! // Default: uses Claude API (requires ANTHROPIC_API_KEY)
//! let claude_box = AgentBox::new("default").prompt("hello").build()?;
//!
//! // Opt-in: use a local Ollama model
//! let ollama_box = AgentBox::new("local")
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

/// LLM backend provider for an [`AgentBox`](crate::agent_box::AgentBox).
///
/// Determines which LLM service the agent talks to. The provider is
/// translated into environment variables injected into the guest VM.
#[derive(Debug, Clone)]
pub enum LlmProvider {
    /// Anthropic Claude API (default).
    ///
    /// Requires `ANTHROPIC_API_KEY` in the host environment.
    /// No extra env vars are injected.
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
}

impl Default for LlmProvider {
    fn default() -> Self {
        LlmProvider::Claude
    }
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
            _ => {}
        }
        self
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
            LlmProvider::Claude => Vec::new(),
            LlmProvider::Ollama { model, .. } => {
                vec!["--model".into(), model.clone()]
            }
            LlmProvider::Custom { model: Some(m), .. } => {
                vec!["--model".into(), m.clone()]
            }
            LlmProvider::Custom { model: None, .. } => Vec::new(),
        }
    }

    /// Generate the environment variables to inject into the guest VM.
    pub(crate) fn env_vars(&self) -> Vec<(String, String)> {
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
        }
    }

    /// Whether this provider runs locally (no real API cost).
    ///
    /// When true, `total_cost_usd` reported by claude-code is meaningless
    /// (it applies Anthropic pricing to local model tokens) and should be
    /// zeroed in the final report.
    pub(crate) fn is_local(&self) -> bool {
        matches!(self, LlmProvider::Ollama { .. })
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
            LlmProvider::Ollama { model, host } => {
                let h = host.as_deref().unwrap_or("localhost:11434");
                format!("Ollama ({} @ {})", model, h)
            }
            LlmProvider::Custom {
                base_url, model, ..
            } => {
                let m = model.as_deref().unwrap_or("default");
                format!("Custom ({} @ {})", m, base_url)
            }
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
}
