# Auto Image Resolution — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `voidbox run --file spec.yaml` auto-resolves kernel and initramfs from GitHub Releases based on the spec's `llm.provider`, downloading and caching both on first run — zero env vars needed.

**Architecture:** A new `src/image.rs` module handles download, checksum verification, caching, and retry logic. It replaces the stub in `src/artifacts.rs`. The existing `resolve_guest_image()` chain in `src/runtime.rs` gains a new step between "well-known installed paths" and "OCI fallback" that calls into `image.rs`. A new `voidbox image` CLI subcommand delegates to `image.rs` for pull/list/clean operations. `LlmProvider` gets an `image_flavor()` method to map provider → artifact flavor.

**Tech Stack:** Rust, reqwest (already dep), sha2 (already dep), indicatif (new dep for progress bars), tokio, clap

**Spec:** `docs/superpowers/specs/2026-04-12-auto-image-resolution-design.md`

---

## File Map

| File | Action | Responsibility |
|---|---|---|
| `src/image.rs` | **Create** | Core module: `resolve_kernel()`, `resolve_initramfs()`, `download_and_cache()`, checksum verification, arch detection, retry logic, cache layout, progress bar |
| `src/bin/voidbox/image.rs` | **Create** | `voidbox image` subcommand: `ImageCommand` enum (Pull, List, Clean), handler functions |
| `src/lib.rs` | **Modify** | Add `pub mod image;`, remove `pub mod artifacts;` |
| `src/llm.rs` | **Modify** | Add `image_flavor(&self) -> &'static str` method |
| `src/runtime.rs` | **Modify** | Insert `image::resolve_*` calls between installed-artifacts and OCI fallback |
| `src/bin/voidbox/main.rs` | **Modify** | Add `Image` subcommand variant, wire to `image::handle()` |
| `src/artifacts.rs` | **Delete** | Replaced entirely by `src/image.rs` |
| `Cargo.toml` | **Modify** | Add `indicatif` dependency |

---

## Task 1: Add `image_flavor()` to `LlmProvider`

**Files:**
- Modify: `src/llm.rs:239` (after `binary_name()`)

- [ ] **Step 1: Write the test**

Add to the `tests` module at the bottom of `src/llm.rs`:

```rust
#[test]
fn test_image_flavor_codex() {
    assert_eq!(LlmProvider::Codex.image_flavor(), "codex");
}

#[test]
fn test_image_flavor_claude_variants() {
    assert_eq!(LlmProvider::Claude.image_flavor(), "claude");
    assert_eq!(LlmProvider::ClaudePersonal.image_flavor(), "claude");
    assert_eq!(LlmProvider::ollama("x").image_flavor(), "claude");
    assert_eq!(LlmProvider::lm_studio("x").image_flavor(), "claude");
    assert_eq!(LlmProvider::custom("x").image_flavor(), "claude");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p void-box --lib llm::tests::test_image_flavor -- --nocapture`
Expected: compile error — `image_flavor` method not found.

- [ ] **Step 3: Implement `image_flavor()`**

Add after `binary_name()` (around line 248) in `src/llm.rs`:

```rust
/// Initramfs flavor used by the auto image resolver.
///
/// Maps each provider to the pre-built initramfs artifact name:
/// - `"codex"` → `void-box-codex-<arch>.cpio.gz`
/// - `"claude"` → `void-box-claude-<arch>.cpio.gz` (all Claude-compatible providers)
///
/// Used by [`crate::image`] to construct the download URL and cache path.
pub fn image_flavor(&self) -> &'static str {
    match self {
        LlmProvider::Codex => "codex",
        LlmProvider::Claude
        | LlmProvider::ClaudePersonal
        | LlmProvider::Ollama { .. }
        | LlmProvider::LmStudio { .. }
        | LlmProvider::Custom { .. } => "claude",
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p void-box --lib llm::tests::test_image_flavor -- --nocapture`
Expected: 2 tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/llm.rs
git commit -m "feat(llm): add image_flavor() for auto image resolution"
```

---

## Task 2: Create `src/image.rs` — core download and cache module

**Files:**
- Create: `src/image.rs`
- Modify: `src/lib.rs` (add `pub mod image;`)
- Modify: `Cargo.toml` (add `indicatif`)

This is the largest task. It implements: arch detection, cache layout, download with retries, checksum verification, progress bar, and the two public resolve functions.

- [ ] **Step 1: Add `indicatif` dependency**

In `Cargo.toml`, add under `[dependencies]` (after `sha2`):

```toml
indicatif = "0.17"
```

- [ ] **Step 2: Write the tests**

Create `src/image.rs` with the full test module first (at the bottom), before the implementation:

```rust
//! Auto image resolution — download, cache, and verify pre-built artifacts.
//!
//! Downloads kernel and initramfs from GitHub Releases, verifies SHA-256
//! checksums, and caches under `~/.void-box/images/<version>/`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};
use tracing::{info, warn};

/// GitHub releases base URL for void-box artifacts.
const GITHUB_RELEASES_URL: &str =
    "https://github.com/the-void-ia/void-box/releases/download";

/// CLI version baked in at compile time — used as the cache bucket.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Maximum number of download retries on transient errors.
const MAX_RETRIES: u32 = 3;

/// Base backoff duration (doubles on each retry).
const BASE_BACKOFF: Duration = Duration::from_secs(1);

// ---------------------------------------------------------------------------
// Public API (stubs — filled in Step 3)
// ---------------------------------------------------------------------------

// ... (implementation comes in Step 3)

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_cache_dir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let cache = dir.path().join("images");
        fs::create_dir_all(&cache).expect("create cache dir");
        (dir, cache)
    }

    #[test]
    fn test_arch_detection() {
        let arch = detect_arch().expect("should detect arch");
        assert!(
            arch == "x86_64" || arch == "aarch64",
            "unexpected arch: {}",
            arch
        );
    }

    #[test]
    fn test_kernel_artifact_name() {
        assert_eq!(kernel_artifact_name("x86_64"), "vmlinuz-x86_64");
        assert_eq!(kernel_artifact_name("aarch64"), "vmlinux-aarch64");
    }

    #[test]
    fn test_initramfs_artifact_name() {
        assert_eq!(
            initramfs_artifact_name("claude", "x86_64"),
            "void-box-claude-x86_64.cpio.gz"
        );
        assert_eq!(
            initramfs_artifact_name("codex", "aarch64"),
            "void-box-codex-aarch64.cpio.gz"
        );
    }

    #[test]
    fn test_cache_dir_layout() {
        let (_tmp, cache) = temp_cache_dir();
        let path = version_cache_dir_in(&cache);
        assert!(path.ends_with(format!("images/v{}", VERSION)));
    }

    #[test]
    fn test_verify_checksum_valid() {
        let data = b"hello world";
        let mut hasher = Sha256::new();
        hasher.update(data);
        let hex = format!("{:x}", hasher.finalize());

        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.bin");
        fs::write(&file_path, data).unwrap();

        assert!(verify_checksum(&file_path, &hex).is_ok());
    }

    #[test]
    fn test_verify_checksum_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.bin");
        fs::write(&file_path, b"hello world").unwrap();

        let result = verify_checksum(&file_path, "0000000000000000000000000000000000000000000000000000000000000000");
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_kernel_from_env_override() {
        let dir = tempfile::tempdir().unwrap();
        let kernel = dir.path().join("vmlinuz");
        fs::write(&kernel, b"fake-kernel").unwrap();

        // When VOID_BOX_KERNEL is set, resolve_kernel should return it
        // without downloading. We test this indirectly through the helper.
        assert!(kernel.exists());
    }

    #[test]
    fn test_cache_hit_returns_path() {
        let (_tmp, cache) = temp_cache_dir();
        let ver_dir = version_cache_dir_in(&cache);
        fs::create_dir_all(&ver_dir).unwrap();

        let artifact = ver_dir.join("void-box-claude-x86_64.cpio.gz");
        fs::write(&artifact, b"cached-data").unwrap();

        // Write a valid checksum file
        let mut hasher = Sha256::new();
        hasher.update(b"cached-data");
        let hex = format!("{:x}", hasher.finalize());
        fs::write(
            ver_dir.join("void-box-claude-x86_64.cpio.gz.sha256"),
            &hex,
        )
        .unwrap();

        assert!(artifact.exists());
        assert_eq!(
            check_cache(&cache, "void-box-claude-x86_64.cpio.gz"),
            Some(artifact)
        );
    }

    #[test]
    fn test_cache_miss_returns_none() {
        let (_tmp, cache) = temp_cache_dir();
        assert_eq!(check_cache(&cache, "void-box-claude-x86_64.cpio.gz"), None);
    }

    #[test]
    fn test_download_url() {
        let url = download_url("v0.1.2", "void-box-claude-x86_64.cpio.gz");
        assert_eq!(
            url,
            "https://github.com/the-void-ia/void-box/releases/download/v0.1.2/void-box-claude-x86_64.cpio.gz"
        );
    }

    #[test]
    fn test_parse_checksum_file() {
        // Standard sha256sum output format: "<hex>  <filename>"
        let content = "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890  void-box-claude-x86_64.cpio.gz\n";
        let hex = parse_checksum_hex(content).unwrap();
        assert_eq!(hex, "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890");
    }

    #[test]
    fn test_parse_checksum_file_bare_hex() {
        let content = "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890\n";
        let hex = parse_checksum_hex(content).unwrap();
        assert_eq!(hex, "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890");
    }

    #[test]
    fn test_list_cached_empty() {
        let (_tmp, cache) = temp_cache_dir();
        let entries = list_cached(&cache);
        assert!(entries.is_empty());
    }

    #[test]
    fn test_list_cached_with_artifacts() {
        let (_tmp, cache) = temp_cache_dir();
        let ver_dir = cache.join("v0.1.2");
        fs::create_dir_all(&ver_dir).unwrap();
        fs::write(ver_dir.join("void-box-claude-x86_64.cpio.gz"), b"data").unwrap();
        fs::write(ver_dir.join("vmlinuz-x86_64"), b"kernel").unwrap();

        let entries = list_cached(&cache);
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn test_clean_removes_old_versions() {
        let (_tmp, cache) = temp_cache_dir();
        let old_dir = cache.join("v0.0.1");
        let cur_dir = cache.join(format!("v{}", VERSION));
        fs::create_dir_all(&old_dir).unwrap();
        fs::create_dir_all(&cur_dir).unwrap();
        fs::write(old_dir.join("artifact.bin"), b"old").unwrap();
        fs::write(cur_dir.join("artifact.bin"), b"current").unwrap();

        let freed = clean(&cache, false);
        assert!(freed > 0);
        assert!(!old_dir.exists());
        assert!(cur_dir.exists());
    }

    #[test]
    fn test_clean_all_removes_everything() {
        let (_tmp, cache) = temp_cache_dir();
        let cur_dir = cache.join(format!("v{}", VERSION));
        fs::create_dir_all(&cur_dir).unwrap();
        fs::write(cur_dir.join("artifact.bin"), b"current").unwrap();

        let freed = clean(&cache, true);
        assert!(freed > 0);
        assert!(!cache.exists());
    }
}
```

- [ ] **Step 3: Implement the module**

Fill in the implementation above the `#[cfg(test)]` block in `src/image.rs`:

```rust
/// Detect host CPU architecture, returning `"x86_64"` or `"aarch64"`.
pub fn detect_arch() -> Result<&'static str, ImageError> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("x86_64"),
        "aarch64" => Ok("aarch64"),
        other => Err(ImageError::UnsupportedArch(other.to_string())),
    }
}

/// Kernel artifact filename for the given architecture.
pub fn kernel_artifact_name(arch: &str) -> String {
    match arch {
        "aarch64" => "vmlinux-aarch64".to_string(),
        _ => format!("vmlinuz-{}", arch),
    }
}

/// Initramfs artifact filename for the given flavor and architecture.
pub fn initramfs_artifact_name(flavor: &str, arch: &str) -> String {
    format!("void-box-{}-{}.cpio.gz", flavor, arch)
}

/// Version-bucketed cache directory under the given root.
fn version_cache_dir_in(cache_root: &Path) -> PathBuf {
    cache_root.join(format!("v{}", VERSION))
}

/// Default cache root: `~/.void-box/images/`.
pub fn default_cache_root() -> Result<PathBuf, ImageError> {
    let home = std::env::var("HOME")
        .map_err(|_| ImageError::CacheDir("HOME not set".to_string()))?;
    Ok(PathBuf::from(home).join(".void-box/images"))
}

/// Check if an artifact is already cached. Returns `Some(path)` on hit.
pub fn check_cache(cache_root: &Path, artifact_name: &str) -> Option<PathBuf> {
    let path = version_cache_dir_in(cache_root).join(artifact_name);
    if path.exists() {
        Some(path)
    } else {
        None
    }
}

/// Build the GitHub Releases download URL for an artifact.
pub fn download_url(version_tag: &str, artifact_name: &str) -> String {
    format!("{}/{}/{}", GITHUB_RELEASES_URL, version_tag, artifact_name)
}

/// Parse a SHA-256 hex digest from checksum file content.
///
/// Accepts either bare hex or `sha256sum` format (`<hex>  <filename>`).
pub fn parse_checksum_hex(content: &str) -> Result<String, ImageError> {
    let line = content.trim();
    let hex = line.split_whitespace().next().unwrap_or(line);
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(ImageError::ChecksumParse(line.to_string()));
    }
    Ok(hex.to_string())
}

/// Verify a file's SHA-256 digest against an expected hex string.
pub fn verify_checksum(file_path: &Path, expected_hex: &str) -> Result<(), ImageError> {
    let data = fs::read(file_path)
        .map_err(|e| ImageError::Io(file_path.to_path_buf(), e))?;
    let mut hasher = Sha256::new();
    hasher.update(&data);
    let actual = format!("{:x}", hasher.finalize());
    if actual != expected_hex {
        return Err(ImageError::ChecksumMismatch {
            artifact: file_path.display().to_string(),
            expected: expected_hex.to_string(),
            actual,
        });
    }
    Ok(())
}

/// Download a single artifact with retries, checksum verification, and progress.
///
/// Returns the cached file path on success.
pub async fn download_and_cache(
    cache_root: &Path,
    artifact_name: &str,
) -> Result<PathBuf, ImageError> {
    let version_tag = format!("v{}", VERSION);
    let ver_dir = version_cache_dir_in(cache_root);
    fs::create_dir_all(&ver_dir)
        .map_err(|e| ImageError::CacheDir(format!("{}: {}", ver_dir.display(), e)))?;

    let artifact_url = download_url(&version_tag, artifact_name);
    let checksum_url = format!("{}.sha256", artifact_url);
    let dest = ver_dir.join(artifact_name);

    for attempt in 0..=MAX_RETRIES {
        if attempt > 0 {
            let backoff = BASE_BACKOFF * 2u32.pow(attempt - 1);
            warn!(attempt, "retrying download after {:?}", backoff);
            tokio::time::sleep(backoff).await;
        }

        // Download artifact
        match download_file(&artifact_url, &dest).await {
            Ok(()) => {}
            Err(e) if e.is_retryable() && attempt < MAX_RETRIES => {
                warn!(%artifact_url, error = %e, "download failed, will retry");
                continue;
            }
            Err(e) => return Err(e),
        }

        // Download checksum
        let checksum_dest = ver_dir.join(format!("{}.sha256", artifact_name));
        match download_file(&checksum_url, &checksum_dest).await {
            Ok(()) => {}
            Err(e) if e.is_retryable() && attempt < MAX_RETRIES => {
                let _ = fs::remove_file(&dest);
                warn!("checksum download failed, will retry: {}", e);
                continue;
            }
            Err(e) => {
                let _ = fs::remove_file(&dest);
                return Err(e);
            }
        }

        // Verify checksum
        let checksum_content = fs::read_to_string(&checksum_dest)
            .map_err(|e| ImageError::Io(checksum_dest.clone(), e))?;
        let expected_hex = parse_checksum_hex(&checksum_content)?;

        match verify_checksum(&dest, &expected_hex) {
            Ok(()) => {
                info!(artifact = artifact_name, "checksum verified");
                return Ok(dest);
            }
            Err(e) => {
                let _ = fs::remove_file(&dest);
                let _ = fs::remove_file(&checksum_dest);
                if attempt < MAX_RETRIES {
                    warn!("checksum mismatch, will retry: {}", e);
                    continue;
                }
                return Err(e);
            }
        }
    }

    unreachable!("retry loop should return or error")
}

/// Resolve the kernel path, following the resolution chain:
/// 1. `--kernel` flag / `VOID_BOX_KERNEL` env var → use it
/// 2. Linux: `/boot/vmlinuz-$(uname -r)` → use it (no download)
/// 3. Cache hit → use it
/// 4. Download from GitHub release → verify → cache → use it
pub async fn resolve_kernel(
    explicit: Option<&Path>,
    cache_root: &Path,
) -> Result<PathBuf, ImageError> {
    // Step 1: explicit override
    if let Some(path) = explicit {
        if path.exists() {
            return Ok(path.to_path_buf());
        }
        return Err(ImageError::NotFound(path.display().to_string()));
    }

    let arch = detect_arch()?;

    // Step 2: host kernel (Linux only)
    #[cfg(target_os = "linux")]
    {
        if let Ok(output) = std::process::Command::new("uname").arg("-r").output() {
            if output.status.success() {
                let ver = String::from_utf8_lossy(&output.stdout).trim().to_string();
                let host_kernel = PathBuf::from(format!("/boot/vmlinuz-{}", ver));
                if host_kernel.exists() {
                    info!(path = %host_kernel.display(), "using host kernel");
                    return Ok(host_kernel);
                }
            }
        }
    }

    // Step 3: cache hit
    let artifact = kernel_artifact_name(arch);
    if let Some(cached) = check_cache(cache_root, &artifact) {
        info!(path = %cached.display(), "using cached kernel");
        return Ok(cached);
    }

    // Step 4: download
    info!(artifact = %artifact, "downloading kernel");
    download_and_cache(cache_root, &artifact).await
}

/// Resolve the initramfs path, following the resolution chain:
/// 1. `--initramfs` flag / `VOID_BOX_INITRAMFS` env var → use it
/// 2. Cache hit → use it
/// 3. Download from GitHub release → verify → cache → use it
pub async fn resolve_initramfs(
    explicit: Option<&Path>,
    flavor: &str,
    cache_root: &Path,
) -> Result<PathBuf, ImageError> {
    // Step 1: explicit override
    if let Some(path) = explicit {
        if path.exists() {
            return Ok(path.to_path_buf());
        }
        return Err(ImageError::NotFound(path.display().to_string()));
    }

    let arch = detect_arch()?;

    // Step 2: cache hit
    let artifact = initramfs_artifact_name(flavor, arch);
    if let Some(cached) = check_cache(cache_root, &artifact) {
        info!(path = %cached.display(), "using cached initramfs");
        return Ok(cached);
    }

    // Step 3: download
    info!(artifact = %artifact, flavor, "downloading initramfs");
    download_and_cache(cache_root, &artifact).await
}

/// Download a file from `url` to `dest` with a progress bar on stderr.
async fn download_file(url: &str, dest: &Path) -> Result<(), ImageError> {
    let client = reqwest::Client::new();
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| ImageError::Network(url.to_string(), e.to_string()))?;

    let status = resp.status();
    if status.is_client_error() {
        return Err(ImageError::HttpStatus(url.to_string(), status.as_u16()));
    }
    if status.is_server_error() {
        return Err(ImageError::HttpRetryable(url.to_string(), status.as_u16()));
    }

    let total_size = resp.content_length().unwrap_or(0);
    let filename = dest
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("artifact");

    let pb = if total_size > 0 {
        let pb = indicatif::ProgressBar::new(total_size);
        pb.set_style(
            indicatif::ProgressStyle::default_bar()
                .template("[{bar:40.cyan/blue}] {bytes}/{total_bytes} {msg}")
                .expect("valid template")
                .progress_chars("=> "),
        );
        pb.set_message(filename.to_string());
        Some(pb)
    } else {
        eprintln!("[download] {}", filename);
        None
    };

    let bytes = resp
        .bytes()
        .await
        .map_err(|e| ImageError::Network(url.to_string(), e.to_string()))?;

    if let Some(ref pb) = pb {
        pb.set_position(bytes.len() as u64);
        pb.finish_and_clear();
    }

    let mut file = fs::File::create(dest)
        .map_err(|e| ImageError::Io(dest.to_path_buf(), e))?;
    file.write_all(&bytes)
        .map_err(|e| ImageError::Io(dest.to_path_buf(), e))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// List / Clean
// ---------------------------------------------------------------------------

/// Entry describing a cached artifact.
#[derive(Debug)]
pub struct CachedArtifact {
    pub version: String,
    pub flavor: String,
    pub arch: String,
    pub size_bytes: u64,
    pub path: PathBuf,
}

/// List all cached artifacts under `cache_root`.
pub fn list_cached(cache_root: &Path) -> Vec<CachedArtifact> {
    let mut entries = Vec::new();
    let Ok(versions) = fs::read_dir(cache_root) else {
        return entries;
    };
    for ver_entry in versions.flatten() {
        let ver_name = ver_entry.file_name().to_string_lossy().to_string();
        if !ver_name.starts_with('v') {
            continue;
        }
        let Ok(files) = fs::read_dir(ver_entry.path()) else {
            continue;
        };
        for file_entry in files.flatten() {
            let fname = file_entry.file_name().to_string_lossy().to_string();
            if fname.ends_with(".sha256") {
                continue;
            }
            let size = file_entry.metadata().map(|m| m.len()).unwrap_or(0);
            let (flavor, arch) = parse_artifact_name(&fname);
            entries.push(CachedArtifact {
                version: ver_name.clone(),
                flavor,
                arch,
                size_bytes: size,
                path: file_entry.path(),
            });
        }
    }
    entries.sort_by(|a, b| b.version.cmp(&a.version).then(a.flavor.cmp(&b.flavor)));
    entries
}

/// Parse an artifact filename into (flavor, arch).
fn parse_artifact_name(name: &str) -> (String, String) {
    // "void-box-claude-x86_64.cpio.gz" → ("claude", "x86_64")
    // "vmlinuz-x86_64" → ("kernel", "x86_64")
    // "vmlinux-aarch64" → ("kernel", "aarch64")
    if let Some(rest) = name.strip_prefix("void-box-") {
        let rest = rest.trim_end_matches(".cpio.gz");
        if let Some(idx) = rest.rfind('-') {
            return (rest[..idx].to_string(), rest[idx + 1..].to_string());
        }
    }
    if let Some(rest) = name.strip_prefix("vmlinuz-").or_else(|| name.strip_prefix("vmlinux-")) {
        return ("kernel".to_string(), rest.to_string());
    }
    ("unknown".to_string(), "unknown".to_string())
}

/// Remove cached versions. Returns total bytes freed.
///
/// - `all == false`: removes all versions except current (`v{VERSION}`).
/// - `all == true`: removes the entire `cache_root` directory.
pub fn clean(cache_root: &Path, all: bool) -> u64 {
    if all {
        let size = dir_size(cache_root);
        if fs::remove_dir_all(cache_root).is_ok() {
            return size;
        }
        return 0;
    }

    let current = format!("v{}", VERSION);
    let mut freed = 0u64;
    let Ok(versions) = fs::read_dir(cache_root) else {
        return 0;
    };
    for entry in versions.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name != current {
            freed += dir_size(&entry.path());
            let _ = fs::remove_dir_all(entry.path());
        }
    }
    freed
}

/// Recursively compute directory size in bytes.
fn dir_size(path: &Path) -> u64 {
    let mut total = 0u64;
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            let ft = entry.file_type().unwrap_or_else(|_| {
                // Fallback: treat as file
                fs::metadata(entry.path())
                    .map(|m| m.file_type())
                    .unwrap_or_else(|_| entry.file_type().unwrap())
            });
            if ft.is_dir() {
                total += dir_size(&entry.path());
            } else {
                total += entry.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    total
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors from the image resolution module.
#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    #[error("unsupported architecture: {0}")]
    UnsupportedArch(String),

    #[error("file not found: {0}")]
    NotFound(String),

    #[error("cache directory error: {0}")]
    CacheDir(String),

    #[error("I/O error on {0}: {1}")]
    Io(PathBuf, #[source] std::io::Error),

    #[error("network error downloading {0}: {1}")]
    Network(String, String),

    #[error("HTTP {1} for {0}")]
    HttpStatus(String, u16),

    #[error("HTTP {1} (retryable) for {0}")]
    HttpRetryable(String, u16),

    #[error("checksum parse error: {0}")]
    ChecksumParse(String),

    #[error("checksum mismatch for {artifact}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        artifact: String,
        expected: String,
        actual: String,
    },
}

impl ImageError {
    /// Whether this error is transient and should be retried.
    fn is_retryable(&self) -> bool {
        matches!(
            self,
            ImageError::Network(..)
                | ImageError::HttpRetryable(..)
                | ImageError::ChecksumMismatch { .. }
        )
    }
}
```

- [ ] **Step 4: Register the module in `src/lib.rs`**

Replace `pub mod artifacts;` with `pub mod image;` in `src/lib.rs`.

- [ ] **Step 5: Fix any references to `artifacts` module**

Search for `crate::artifacts::` in the codebase and update to `crate::image::` or remove.
The only known reference is in `src/runtime.rs:1003` (`crate::artifacts::resolve_installed_artifacts()`).
For now, move `resolve_installed_artifacts()` and its helpers into `src/image.rs` as private functions, keeping the same logic.

Add to `src/image.rs` (above the tests):

```rust
// ---------------------------------------------------------------------------
// Installed artifact detection (migrated from artifacts.rs)
// ---------------------------------------------------------------------------

/// Paths to void-box artifacts (kernel and initramfs).
#[derive(Debug, Clone)]
pub struct ArtifactPaths {
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
}

/// Try to resolve artifacts from well-known installation paths.
pub fn resolve_installed_artifacts() -> Option<ArtifactPaths> {
    let candidates = installed_artifact_dirs();
    for dir in &candidates {
        let kernel = dir.join(installed_kernel_name());
        let initramfs = dir.join("initramfs.cpio.gz");
        if kernel.exists() && initramfs.exists() {
            return Some(ArtifactPaths { kernel, initramfs });
        }
    }
    None
}

fn installed_artifact_dirs() -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        vec![
            PathBuf::from("/opt/homebrew/lib/voidbox"),
            PathBuf::from("/usr/local/lib/voidbox"),
        ]
    }
    #[cfg(not(target_os = "macos"))]
    {
        vec![PathBuf::from("/usr/lib/voidbox")]
    }
}

fn installed_kernel_name() -> &'static str {
    #[cfg(target_os = "macos")]
    { "vmlinux" }
    #[cfg(not(target_os = "macos"))]
    { "vmlinuz" }
}
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p void-box --lib image::tests -- --nocapture`
Expected: all tests pass (download/network tests are not called — only unit tests on pure functions).

- [ ] **Step 7: Run full workspace check**

Run: `cargo clippy --workspace --all-targets --all-features -- -D warnings`
Expected: no errors.

- [ ] **Step 8: Delete `src/artifacts.rs`**

Remove the file. All its functionality is now in `src/image.rs`.

- [ ] **Step 9: Commit**

```bash
git add src/image.rs src/lib.rs Cargo.toml
git rm src/artifacts.rs
git commit -m "feat(image): add auto image resolution module with download, cache, checksum"
```

---

## Task 3: Wire `image::resolve_*` into `runtime.rs`

**Files:**
- Modify: `src/runtime.rs:985-1050` (`resolve_guest_image()`)

- [ ] **Step 1: Update `resolve_guest_image()` to use `image` module**

Insert a new step between "well-known installed paths" (step 3) and "OCI fallback" (step 4). The new step calls `image::resolve_kernel()` and `image::resolve_initramfs()`.

In `src/runtime.rs`, update `resolve_guest_image()`:

```rust
async fn resolve_guest_image(spec: &RunSpec) -> Option<GuestFiles> {
    // Steps 1-2: local kernel/initramfs paths (spec + env vars).
    if let Some(kernel) = resolve_kernel_local(spec) {
        return Some(GuestFiles {
            kernel,
            initramfs: resolve_initramfs_local(spec),
        });
    }

    // Step 3: well-known installed paths (package manager installs).
    if let Some(installed) = crate::image::resolve_installed_artifacts() {
        eprintln!(
            "[void-box] Using installed artifacts: kernel={}, initramfs={}",
            installed.kernel.display(),
            installed.initramfs.display()
        );
        return Some(GuestFiles {
            kernel: installed.kernel,
            initramfs: Some(installed.initramfs),
        });
    }

    // Step 3.5: auto-resolve from GitHub Releases based on llm.provider.
    // Determine flavor from the spec's LLM provider.
    let flavor = spec
        .llm
        .as_ref()
        .and_then(|llm| {
            match llm.provider.to_ascii_lowercase().as_str() {
                "codex" => Some("codex"),
                "claude" | "claude-personal" | "ollama" | "lm-studio" | "custom" => Some("claude"),
                _ => None,
            }
        })
        .or_else(|| {
            // kind: workflow with no llm section → base
            if spec.kind.eq_ignore_ascii_case("workflow") {
                Some("base")
            } else {
                None
            }
        });

    if let Some(flavor) = flavor {
        let cache_root = match crate::image::default_cache_root() {
            Ok(root) => root,
            Err(e) => {
                warn!("cannot resolve image cache dir: {}", e);
                // Fall through to OCI
                return resolve_guest_image_oci(spec).await;
            }
        };

        let kernel_explicit = spec
            .sandbox
            .kernel
            .as_ref()
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("VOID_BOX_KERNEL").map(PathBuf::from));
        let initramfs_explicit = spec
            .sandbox
            .initramfs
            .as_ref()
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("VOID_BOX_INITRAMFS").map(PathBuf::from));

        // Download kernel and initramfs concurrently (halves cold-start wait).
        let (kernel_result, initramfs_result) = tokio::join!(
            crate::image::resolve_kernel(
                kernel_explicit.as_deref(),
                &cache_root,
            ),
            crate::image::resolve_initramfs(
                initramfs_explicit.as_deref(),
                flavor,
                &cache_root,
            )
        );

        match (kernel_result, initramfs_result) {
            (Ok(kernel), Ok(initramfs)) => {
                return Some(GuestFiles {
                    kernel,
                    initramfs: Some(initramfs),
                });
            }
            (Err(e), _) | (_, Err(e)) => {
                warn!("auto image resolution failed: {}. Falling back to OCI.", e);
            }
        }
    }

    // Step 4+: OCI fallback (existing logic)
    resolve_guest_image_oci(spec).await
}
```

Extract the existing OCI steps 4-5 into a helper:

```rust
async fn resolve_guest_image_oci(spec: &RunSpec) -> Option<GuestFiles> {
    if let Some(ref guest_image) = spec.sandbox.guest_image {
        if guest_image.is_empty() {
            return None;
        }
        match resolve_oci_guest_image(guest_image).await {
            Ok(files) => return Some(files),
            Err(e) => {
                eprintln!(
                    "[void-box] Failed to resolve guest image '{}': {}",
                    guest_image, e
                );
                return None;
            }
        }
    }

    let version = env!("CARGO_PKG_VERSION");
    let default_ref = format!("ghcr.io/the-void-ia/voidbox-guest:v{}", version);
    match resolve_oci_guest_image(&default_ref).await {
        Ok(files) => Some(files),
        Err(e) => {
            eprintln!(
                "[void-box] Failed to resolve default guest image '{}': {}",
                default_ref, e
            );
            None
        }
    }
}
```

- [ ] **Step 2: Add `use tracing::warn;` to runtime.rs imports if not present**

Check the top of `src/runtime.rs` for existing `tracing` imports and add `warn` if missing.

- [ ] **Step 3: Run workspace tests**

Run: `cargo test -p void-box --lib -- --nocapture`
Expected: all existing tests pass.

- [ ] **Step 4: Run clippy**

Run: `cargo clippy --workspace --all-targets --all-features -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add src/runtime.rs
git commit -m "feat(runtime): wire auto image resolution into guest image chain"
```

---

## Task 4: Add `voidbox image` CLI subcommand

**Files:**
- Create: `src/bin/voidbox/image.rs`
- Modify: `src/bin/voidbox/main.rs`

- [ ] **Step 1: Create `src/bin/voidbox/image.rs`**

```rust
use std::path::Path;

use clap::Subcommand;

/// Image artifact flavors that can be pulled.
const KNOWN_FLAVORS: &[&str] = &["base", "claude", "codex", "agents", "kernel"];

#[derive(Debug, Subcommand)]
pub enum ImageCommand {
    /// Download a specific image or kernel.
    Pull {
        /// Flavor to pull: base, claude, codex, agents, kernel, or all.
        flavor: String,
    },
    /// Show cached images.
    List,
    /// Remove old cached versions (keeps current).
    Clean {
        /// Remove everything, including current version.
        #[arg(long)]
        all: bool,
    },
}

pub async fn handle(cmd: ImageCommand) -> Result<(), Box<dyn std::error::Error>> {
    let cache_root = void_box::image::default_cache_root()?;

    match cmd {
        ImageCommand::Pull { flavor } => cmd_pull(&cache_root, &flavor).await,
        ImageCommand::List => cmd_list(&cache_root),
        ImageCommand::Clean { all } => cmd_clean(&cache_root, all),
    }
}

async fn cmd_pull(
    cache_root: &Path,
    flavor: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let arch = void_box::image::detect_arch()?;

    if flavor == "all" {
        // Pull all flavors + kernel
        for f in KNOWN_FLAVORS {
            if *f == "kernel" {
                let name = void_box::image::kernel_artifact_name(arch);
                pull_one(cache_root, &name).await?;
            } else {
                let name = void_box::image::initramfs_artifact_name(f, arch);
                pull_one(cache_root, &name).await?;
            }
        }
        return Ok(());
    }

    if flavor == "kernel" {
        let name = void_box::image::kernel_artifact_name(arch);
        pull_one(cache_root, &name).await?;
        return Ok(());
    }

    if !KNOWN_FLAVORS.contains(&flavor) {
        return Err(format!(
            "unknown flavor '{}'. Valid: base, claude, codex, agents, kernel, all",
            flavor
        )
        .into());
    }

    let name = void_box::image::initramfs_artifact_name(flavor, arch);
    pull_one(cache_root, &name).await?;
    Ok(())
}

async fn pull_one(
    cache_root: &Path,
    artifact_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(cached) = void_box::image::check_cache(cache_root, artifact_name) {
        eprintln!("{} — already cached at {}", artifact_name, cached.display());
        return Ok(());
    }
    let path = void_box::image::download_and_cache(cache_root, artifact_name).await?;
    eprintln!("{} — cached at {}", artifact_name, path.display());
    Ok(())
}

fn cmd_list(cache_root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let entries = void_box::image::list_cached(cache_root);
    if entries.is_empty() {
        eprintln!("No cached images.");
        return Ok(());
    }

    println!(
        "{:<10} {:<10} {:<10} {:<10} {}",
        "Version", "Flavor", "Arch", "Size", "Path"
    );
    for e in &entries {
        let size_mb = e.size_bytes as f64 / (1024.0 * 1024.0);
        println!(
            "{:<10} {:<10} {:<10} {:<10.0} MB  {}",
            e.version,
            e.flavor,
            e.arch,
            size_mb,
            e.path.display()
        );
    }
    Ok(())
}

fn cmd_clean(cache_root: &Path, all: bool) -> Result<(), Box<dyn std::error::Error>> {
    let freed = void_box::image::clean(cache_root, all);
    let freed_mb = freed as f64 / (1024.0 * 1024.0);
    if freed > 0 {
        eprintln!("Freed {:.1} MB", freed_mb);
    } else {
        eprintln!("Nothing to clean.");
    }
    Ok(())
}
```

- [ ] **Step 2: Wire `Image` into `main.rs` Command enum**

Add to the `Command` enum in `src/bin/voidbox/main.rs`:

```rust
    /// Manage pre-built images (pull, list, clean).
    Image {
        #[command(subcommand)]
        command: image::ImageCommand,
    },
```

Add `mod image;` at the top of the file (alongside the other module declarations).

Add the match arm in the `run()` function:

```rust
        Command::Image { command } => image::handle(command).await.map(|_| 0),
```

- [ ] **Step 3: Run clippy**

Run: `cargo clippy --workspace --all-targets --all-features -- -D warnings`
Expected: clean.

- [ ] **Step 4: Run `voidbox image list` to verify it works**

Run: `cargo run --bin voidbox -- image list`
Expected: prints "No cached images." (empty cache).

- [ ] **Step 5: Commit**

```bash
git add src/bin/voidbox/image.rs src/bin/voidbox/main.rs
git commit -m "feat(cli): add voidbox image subcommand (pull, list, clean)"
```

---

## Task 5: Create `scripts/build_agents_rootfs.sh`

**Files:**
- Create: `scripts/build_agents_rootfs.sh`

This script builds the "agents" flavor initramfs that bundles both Claude Code and Codex binaries.

- [ ] **Step 1: Create the script**

```bash
#!/usr/bin/env bash
# build_agents_rootfs.sh — Combined Claude + Codex initramfs
#
# Produces an initramfs with both /usr/local/bin/claude-code and
# /usr/local/bin/codex, for users who want a single image that works
# with any provider.
#
# Usage:
#   scripts/build_agents_rootfs.sh
#
# Environment:
#   CLAUDE_BIN          — Path to claude-code ELF binary (optional; auto-detected)
#   CLAUDE_CODE_VERSION — Download this version from GCS (optional)
#   CODEX_BIN           — Path to codex ELF binary (optional; auto-detected)
#   CODEX_VERSION       — Download this version from GitHub (optional)
#   OUT_DIR             — Staging directory (default: target/void-box-agents-rootfs)
#   OUT_CPIO            — Output path (default: target/void-box-agents.cpio.gz)

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

export OUT_DIR="${OUT_DIR:-$REPO_ROOT/target/void-box-agents-rootfs}"
export OUT_CPIO="${OUT_CPIO:-$REPO_ROOT/target/void-box-agents.cpio.gz}"

# --- Claude binary -----------------------------------------------------------

# Re-use the same acquisition logic as build_claude_rootfs.sh.
# Source its binary-finding section by setting a flag that skips the build step.
echo "==> Acquiring Claude Code binary..."
CLAUDE_ACQUIRE_ONLY=1 source "$SCRIPT_DIR/build_claude_rootfs.sh" acquire_claude_binary 2>/dev/null \
  || true

if [ -z "${CLAUDE_CODE_BIN:-}" ]; then
    # Inline fallback: same priority as build_claude_rootfs.sh
    if [ -n "${CLAUDE_BIN:-}" ] && [ -f "$CLAUDE_BIN" ]; then
        CLAUDE_CODE_BIN="$CLAUDE_BIN"
    elif [ "$(uname -s)" = "Linux" ]; then
        for candidate in "$HOME/.local/bin/claude" "$(command -v claude 2>/dev/null || true)"; do
            if [ -n "$candidate" ] && [ -f "$candidate" ] && file "$candidate" | grep -q ELF; then
                CLAUDE_CODE_BIN="$candidate"
                break
            fi
        done
    fi
    if [ -z "${CLAUDE_CODE_BIN:-}" ] && [ -n "${CLAUDE_CODE_VERSION:-}" ]; then
        ARCH="$(uname -m)"
        case "$ARCH" in
            x86_64)  PLATFORM="linux-x64" ;;
            aarch64) PLATFORM="linux-arm64" ;;
            *)       echo "Unsupported arch: $ARCH" >&2; exit 1 ;;
        esac
        GCS_URL="https://storage.googleapis.com/claude-code-dist-86c565f3-f756-42ad-8dfa-d59b1c096819/claude-code-releases/${CLAUDE_CODE_VERSION}/${PLATFORM}/claude"
        CLAUDE_CODE_BIN="$REPO_ROOT/target/claude-code-${CLAUDE_CODE_VERSION}"
        if [ ! -f "$CLAUDE_CODE_BIN" ]; then
            echo "Downloading Claude Code ${CLAUDE_CODE_VERSION}..."
            curl -fSL "$GCS_URL" -o "$CLAUDE_CODE_BIN"
            chmod +x "$CLAUDE_CODE_BIN"
        fi
    fi
fi

if [ -z "${CLAUDE_CODE_BIN:-}" ]; then
    echo "ERROR: Cannot find Claude Code binary. Set CLAUDE_BIN or CLAUDE_CODE_VERSION." >&2
    exit 1
fi
echo "  Claude binary: $CLAUDE_CODE_BIN"
export CLAUDE_CODE_BIN

# --- Codex binary -------------------------------------------------------------

echo "==> Acquiring Codex binary..."
if [ -n "${CODEX_BIN:-}" ] && [ -f "$CODEX_BIN" ]; then
    : # Use as-is
elif [ "$(uname -s)" = "Linux" ] && command -v codex >/dev/null 2>&1; then
    candidate="$(command -v codex)"
    if file "$candidate" | grep -q ELF; then
        CODEX_BIN="$candidate"
    fi
fi

if [ -z "${CODEX_BIN:-}" ] && [ -n "${CODEX_VERSION:-}" ]; then
    ARCH="$(uname -m)"
    case "$ARCH" in
        x86_64)  TARGET="x86_64-unknown-linux-musl" ;;
        aarch64) TARGET="aarch64-unknown-linux-musl" ;;
        *)       echo "Unsupported arch: $ARCH" >&2; exit 1 ;;
    esac
    GH_URL="https://github.com/openai/codex/releases/download/rust-v${CODEX_VERSION}/codex-${TARGET}.tar.gz"
    CODEX_BIN="$REPO_ROOT/target/codex-${CODEX_VERSION}"
    if [ ! -f "$CODEX_BIN" ]; then
        echo "Downloading Codex ${CODEX_VERSION}..."
        curl -fSL "$GH_URL" | tar xz -C "$REPO_ROOT/target/" codex
        mv "$REPO_ROOT/target/codex" "$CODEX_BIN"
        chmod +x "$CODEX_BIN"
    fi
fi

if [ -z "${CODEX_BIN:-}" ]; then
    echo "ERROR: Cannot find Codex binary. Set CODEX_BIN or CODEX_VERSION." >&2
    exit 1
fi
echo "  Codex binary: $CODEX_BIN"
export CODEX_BIN

# --- Build base image with both binaries --------------------------------------

echo "==> Building combined agents initramfs..."

# Extract pinned kernel version from download_kernel.sh
KERNEL_VER="${VOID_BOX_KMOD_VERSION:-$(grep '^KERNEL_VER=' "$SCRIPT_DIR/download_kernel.sh" | head -1 | cut -d'"' -f2)}"
KERNEL_UPLOAD="${VOID_BOX_KMOD_UPLOAD:-$(grep '^KERNEL_UPLOAD=' "$SCRIPT_DIR/download_kernel.sh" | head -1 | cut -d'"' -f2)}"
export VOID_BOX_KMOD_VERSION="$KERNEL_VER"
export VOID_BOX_KMOD_UPLOAD="$KERNEL_UPLOAD"

"$SCRIPT_DIR/build_guest_image.sh"

# --- Install sandbox user and CA certs ----------------------------------------

source "$SCRIPT_DIR/lib/agent_rootfs_common.sh"
install_sandbox_user
install_ca_certificates

# --- Symlinks -----------------------------------------------------------------

ln -sf claude-code "$OUT_DIR/usr/local/bin/claude"

# --- Pack ---------------------------------------------------------------------

finalize_initramfs "void-box-agents"
echo "==> Done: $OUT_CPIO"
```

- [ ] **Step 2: Make it executable**

```bash
chmod +x scripts/build_agents_rootfs.sh
```

- [ ] **Step 3: Commit**

```bash
git add scripts/build_agents_rootfs.sh
git commit -m "feat(scripts): add build_agents_rootfs.sh for combined claude+codex image"
```

---

## Task 6: Create `.github/workflows/release-images.yml`

**Files:**
- Create: `.github/workflows/release-images.yml`

This CI workflow builds all 4 flavors + 2 kernels per release, generates `.sha256` companions, and attaches them to the GitHub Release.

- [ ] **Step 1: Create the workflow file**

```yaml
name: Release Images

on:
  release:
    types: [published]

permissions:
  contents: write  # Attach assets to release

jobs:
  build-images:
    strategy:
      matrix:
        arch: [x86_64, aarch64]
        include:
          - arch: x86_64
            runner: ubuntu-latest
          - arch: aarch64
            runner: ubuntu-24.04-arm
    runs-on: ${{ matrix.runner }}

    steps:
      - uses: actions/checkout@v4

      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable
        with:
          targets: ${{ matrix.arch }}-unknown-linux-musl

      - name: Install build dependencies
        run: |
          sudo apt-get update -qq
          sudo apt-get install -y -qq musl-tools cpio gzip xz-utils zstd

      - name: Install cross-linker (aarch64 on x86_64 runner)
        if: matrix.arch == 'aarch64' && runner.arch == 'X64'
        run: sudo apt-get install -y -qq gcc-aarch64-linux-gnu

      - name: Download static busybox
        run: |
          BUSYBOX_URL="http://launchpadlibrarian.net/742082706/busybox-static_1.36.1-8ubuntu1_${{ matrix.arch == 'x86_64' && 'amd64' || 'arm64' }}.deb"
          curl -fSL "$BUSYBOX_URL" -o /tmp/busybox.deb
          dpkg-deb -x /tmp/busybox.deb /tmp/busybox-pkg
          export BUSYBOX=/tmp/busybox-pkg/bin/busybox
          echo "BUSYBOX=$BUSYBOX" >> "$GITHUB_ENV"

      - name: Build base image
        run: |
          export ARCH=${{ matrix.arch }}
          export OUT_CPIO=target/void-box-base-${{ matrix.arch }}.cpio.gz
          scripts/build_guest_image.sh

      - name: Build claude image
        run: |
          export ARCH=${{ matrix.arch }}
          export CLAUDE_CODE_VERSION=${{ vars.CLAUDE_CODE_VERSION || '2.1.53' }}
          export OUT_CPIO=target/void-box-claude-${{ matrix.arch }}.cpio.gz
          scripts/build_claude_rootfs.sh

      - name: Build codex image
        run: |
          export ARCH=${{ matrix.arch }}
          export CODEX_VERSION=${{ vars.CODEX_VERSION || '0.118.0' }}
          export OUT_CPIO=target/void-box-codex-${{ matrix.arch }}.cpio.gz
          scripts/build_codex_rootfs.sh

      - name: Build agents image
        run: |
          export ARCH=${{ matrix.arch }}
          export CLAUDE_CODE_VERSION=${{ vars.CLAUDE_CODE_VERSION || '2.1.53' }}
          export CODEX_VERSION=${{ vars.CODEX_VERSION || '0.118.0' }}
          export OUT_CPIO=target/void-box-agents-${{ matrix.arch }}.cpio.gz
          scripts/build_agents_rootfs.sh

      - name: Download kernel
        run: |
          export ARCH=${{ matrix.arch }}
          scripts/download_kernel.sh
          if [ "${{ matrix.arch }}" = "x86_64" ]; then
            cp target/vmlinuz-amd64 target/vmlinuz-x86_64
          else
            cp target/vmlinux-arm64 target/vmlinux-aarch64
          fi

      - name: Generate SHA-256 checksums
        run: |
          cd target
          for f in \
            void-box-base-${{ matrix.arch }}.cpio.gz \
            void-box-claude-${{ matrix.arch }}.cpio.gz \
            void-box-codex-${{ matrix.arch }}.cpio.gz \
            void-box-agents-${{ matrix.arch }}.cpio.gz \
            $(ls vmlinuz-${{ matrix.arch }} vmlinux-${{ matrix.arch }} 2>/dev/null); do
            sha256sum "$f" | awk '{print $1}' > "${f}.sha256"
          done

      - name: Upload to GitHub Release
        env:
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
        run: |
          TAG="${{ github.event.release.tag_name }}"
          cd target
          for f in \
            void-box-base-${{ matrix.arch }}.cpio.gz \
            void-box-claude-${{ matrix.arch }}.cpio.gz \
            void-box-codex-${{ matrix.arch }}.cpio.gz \
            void-box-agents-${{ matrix.arch }}.cpio.gz \
            void-box-base-${{ matrix.arch }}.cpio.gz.sha256 \
            void-box-claude-${{ matrix.arch }}.cpio.gz.sha256 \
            void-box-codex-${{ matrix.arch }}.cpio.gz.sha256 \
            void-box-agents-${{ matrix.arch }}.cpio.gz.sha256 \
            $(ls vmlinuz-${{ matrix.arch }} vmlinuz-${{ matrix.arch }}.sha256 \
                 vmlinux-${{ matrix.arch }} vmlinux-${{ matrix.arch }}.sha256 2>/dev/null); do
            gh release upload "$TAG" "$f" --clobber
          done
```

- [ ] **Step 2: Commit**

```bash
git add .github/workflows/release-images.yml
git commit -m "ci: add release-images workflow for flavor-specific artifacts"
```

---

## Task 7: Full verification

**Files:** None (verification only)

- [ ] **Step 1: Run `cargo fmt`**

Run: `cargo fmt --all -- --check`

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --workspace --all-targets --all-features -- -D warnings`

- [ ] **Step 3: Run workspace tests**

Run: `cargo test --workspace --all-features`

- [ ] **Step 4: Run `voidbox image` smoke test**

Run:
```bash
cargo run --bin voidbox -- image list
cargo run --bin voidbox -- image clean
```
Expected: both exit 0, clean reports "Nothing to clean."

- [ ] **Step 5: Verify `voidbox version` still works**

Run: `cargo run --bin voidbox -- version`
Expected: prints version info.

- [ ] **Step 6: Final commit (if any fixups needed)**

```bash
git add -A
git commit -m "chore: fixups from verification pass"
```

