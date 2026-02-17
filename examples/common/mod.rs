//! Shared helpers for void-box examples.

use std::path::PathBuf;

use void_box::agent_box::VoidBox;
use void_box::llm::LlmProvider;

/// Create an VoidBox builder pre-configured for the current environment.
pub fn make_box(name: &str, use_kvm: bool, llm: &LlmProvider) -> VoidBox {
    let mut ab = VoidBox::new(name).llm(llm.clone()).memory_mb(1024);

    // Allow per-stage timeout override via STAGE_TIMEOUT_SECS env var
    if let Ok(secs) = std::env::var("STAGE_TIMEOUT_SECS") {
        if let Ok(s) = secs.parse::<u64>() {
            ab = ab.timeout_secs(s);
        }
    }

    if use_kvm {
        if let Some(kernel) = kvm_kernel() {
            ab = ab.kernel(kernel);
        }
        if let Some(initramfs) = kvm_initramfs() {
            ab = ab.initramfs(initramfs);
        }
    } else {
        ab = ab.mock();
    }

    ab
}

/// Detect the LLM provider from environment variables.
///
/// - `OLLAMA_MODEL=qwen3-coder` -> Ollama with that model
/// - `LLM_BASE_URL=...` -> Custom provider
/// - Otherwise -> Claude (default)
pub fn detect_llm_provider() -> LlmProvider {
    // Check for Ollama
    if let Ok(model) = std::env::var("OLLAMA_MODEL") {
        if !model.is_empty() {
            return LlmProvider::ollama(model);
        }
    }

    // Check for custom endpoint
    if let Ok(base_url) = std::env::var("LLM_BASE_URL") {
        if !base_url.is_empty() {
            let mut provider = LlmProvider::custom(base_url);
            if let Ok(key) = std::env::var("LLM_API_KEY") {
                provider = provider.api_key(key);
            }
            if let Ok(model) = std::env::var("LLM_MODEL") {
                provider = provider.model(model);
            }
            return provider;
        }
    }

    // Default: Claude
    LlmProvider::Claude
}

/// Check if KVM artifacts are available.
pub fn is_kvm_available() -> bool {
    std::path::Path::new("/dev/kvm").exists()
        && std::env::var("VOID_BOX_KERNEL")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
}

/// Get the kernel path from environment.
pub fn kvm_kernel() -> Option<PathBuf> {
    std::env::var_os("VOID_BOX_KERNEL").map(PathBuf::from)
}

/// Get the initramfs path from environment.
pub fn kvm_initramfs() -> Option<PathBuf> {
    std::env::var_os("VOID_BOX_INITRAMFS").map(PathBuf::from)
}
