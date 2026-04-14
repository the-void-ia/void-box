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
const GITHUB_RELEASES_URL: &str = "https://github.com/the-void-ia/void-box/releases/download";

/// CLI version baked in at compile time — used as the cache bucket.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Maximum number of download retries on transient errors.
const MAX_ATTEMPTS: u32 = 4;

/// Base backoff duration (doubles on each retry).
const BASE_BACKOFF: Duration = Duration::from_secs(1);

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Map a provider name string to an image flavor.
///
/// Single source of truth for the provider → artifact mapping used by
/// both `runtime.rs` (spec-driven resolution) and `attach.rs` (shell).
pub fn flavor_for_provider(provider: &str) -> Option<&'static str> {
    match provider.to_ascii_lowercase().as_str() {
        "codex" => Some("codex"),
        "claude" | "claude-personal" | "ollama" | "lm-studio" | "custom" => Some("claude"),
        _ => None,
    }
}

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
    let home =
        std::env::var("HOME").map_err(|_| ImageError::CacheDir("HOME not set".to_string()))?;
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

/// Verifies a file's SHA-256 digest against an expected hex string.
///
/// Uses buffered I/O to avoid loading the entire file into memory.
pub fn verify_checksum(file_path: &Path, expected_hex: &str) -> Result<(), ImageError> {
    use std::io::Read;

    let file = fs::File::open(file_path).map_err(|e| ImageError::Io(file_path.to_path_buf(), e))?;
    let mut reader = std::io::BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let bytes_read = reader
            .read(&mut buf)
            .map_err(|e| ImageError::Io(file_path.to_path_buf(), e))?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buf[..bytes_read]);
    }
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

// ---------------------------------------------------------------------------
// Download + cache
// ---------------------------------------------------------------------------

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

    for attempt in 0..MAX_ATTEMPTS {
        if attempt > 0 {
            let backoff = BASE_BACKOFF * 2u32.pow(attempt - 1);
            warn!(attempt, "retrying download after {:?}", backoff);
            tokio::time::sleep(backoff).await;
        }

        let last_attempt = attempt + 1 == MAX_ATTEMPTS;
        let checksum_dest = ver_dir.join(format!("{}.sha256", artifact_name));

        match download_file(&artifact_url, &dest).await {
            Ok(()) => {}
            Err(e) if e.is_retryable() && !last_attempt => {
                warn!(%artifact_url, error = %e, "download failed, will retry");
                continue;
            }
            Err(e) => return Err(e),
        }

        match download_file(&checksum_url, &checksum_dest).await {
            Ok(()) => {}
            Err(e) if e.is_retryable() && !last_attempt => {
                let _ = fs::remove_file(&dest);
                warn!("checksum download failed, will retry: {}", e);
                continue;
            }
            Err(e) => {
                let _ = fs::remove_file(&dest);
                return Err(e);
            }
        }

        let checksum_content = match fs::read_to_string(&checksum_dest) {
            Ok(c) => c,
            Err(e) => {
                let _ = fs::remove_file(&dest);
                let _ = fs::remove_file(&checksum_dest);
                return Err(ImageError::Io(checksum_dest, e));
            }
        };
        let expected_hex = match parse_checksum_hex(&checksum_content) {
            Ok(hex) => hex,
            Err(e) => {
                let _ = fs::remove_file(&dest);
                let _ = fs::remove_file(&checksum_dest);
                if !last_attempt {
                    warn!("checksum parse failed, will retry: {}", e);
                    continue;
                }
                return Err(e);
            }
        };

        match verify_checksum(&dest, &expected_hex) {
            Ok(()) => {
                info!(artifact = artifact_name, "checksum verified");
                return Ok(dest);
            }
            Err(e) => {
                let _ = fs::remove_file(&dest);
                let _ = fs::remove_file(&checksum_dest);
                if !last_attempt {
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

/// Downloads a file from `url` to `dest` with streaming I/O and a progress bar.
async fn download_file(url: &str, dest: &Path) -> Result<(), ImageError> {
    use futures_util::StreamExt;

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

    let mut file = fs::File::create(dest).map_err(|e| ImageError::Io(dest.to_path_buf(), e))?;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| ImageError::Network(url.to_string(), e.to_string()))?;
        file.write_all(&chunk)
            .map_err(|e| ImageError::Io(dest.to_path_buf(), e))?;
        if let Some(ref pb) = pb {
            pb.inc(chunk.len() as u64);
        }
    }

    if let Some(ref pb) = pb {
        pb.finish_and_clear();
    }

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
    // "void-box-claude-x86_64.cpio.gz" -> ("claude", "x86_64")
    // "vmlinuz-x86_64" -> ("kernel", "x86_64")
    // "vmlinux-aarch64" -> ("kernel", "aarch64")
    if let Some(rest) = name.strip_prefix("void-box-") {
        let rest = rest.trim_end_matches(".cpio.gz");
        if let Some(idx) = rest.rfind('-') {
            return (rest[..idx].to_string(), rest[idx + 1..].to_string());
        }
    }
    if let Some(rest) = name
        .strip_prefix("vmlinuz-")
        .or_else(|| name.strip_prefix("vmlinux-"))
    {
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
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.is_dir() {
                total += dir_size(&entry.path());
            } else {
                total += meta.len();
            }
        }
    }
    total
}

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
    {
        "vmlinux"
    }
    #[cfg(not(target_os = "macos"))]
    {
        "vmlinuz"
    }
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_flavor_for_provider() {
        assert_eq!(flavor_for_provider("codex"), Some("codex"));
        assert_eq!(flavor_for_provider("claude"), Some("claude"));
        assert_eq!(flavor_for_provider("Claude"), Some("claude"));
        assert_eq!(flavor_for_provider("claude-personal"), Some("claude"));
        assert_eq!(flavor_for_provider("ollama"), Some("claude"));
        assert_eq!(flavor_for_provider("lm-studio"), Some("claude"));
        assert_eq!(flavor_for_provider("custom"), Some("claude"));
        assert_eq!(flavor_for_provider("unknown-thing"), None);
    }

    #[test]
    fn test_verify_checksum_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.bin");
        fs::write(&file_path, b"hello world").unwrap();

        let result = verify_checksum(
            &file_path,
            "0000000000000000000000000000000000000000000000000000000000000000",
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_cache_hit_returns_path() {
        let (_tmp, cache) = temp_cache_dir();
        let ver_dir = version_cache_dir_in(&cache);
        fs::create_dir_all(&ver_dir).unwrap();

        let artifact = ver_dir.join("void-box-claude-x86_64.cpio.gz");
        fs::write(&artifact, b"cached-data").unwrap();

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
        let content = "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890  void-box-claude-x86_64.cpio.gz\n";
        let hex = parse_checksum_hex(content).unwrap();
        assert_eq!(
            hex,
            "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890"
        );
    }

    #[test]
    fn test_parse_checksum_file_bare_hex() {
        let content = "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890\n";
        let hex = parse_checksum_hex(content).unwrap();
        assert_eq!(
            hex,
            "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890"
        );
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

    #[test]
    fn test_parse_artifact_name_initramfs() {
        let (flavor, arch) = parse_artifact_name("void-box-claude-x86_64.cpio.gz");
        assert_eq!(flavor, "claude");
        assert_eq!(arch, "x86_64");
    }

    #[test]
    fn test_parse_artifact_name_kernel_compressed() {
        let (flavor, arch) = parse_artifact_name("vmlinuz-x86_64");
        assert_eq!(flavor, "kernel");
        assert_eq!(arch, "x86_64");
    }

    #[test]
    fn test_parse_artifact_name_kernel_uncompressed() {
        let (flavor, arch) = parse_artifact_name("vmlinux-aarch64");
        assert_eq!(flavor, "kernel");
        assert_eq!(arch, "aarch64");
    }

    #[test]
    fn test_installed_artifact_dirs_not_empty() {
        let dirs = installed_artifact_dirs();
        assert!(!dirs.is_empty());
    }

    #[test]
    fn test_installed_kernel_name() {
        let name = installed_kernel_name();
        #[cfg(target_os = "macos")]
        assert_eq!(name, "vmlinux");
        #[cfg(not(target_os = "macos"))]
        assert_eq!(name, "vmlinuz");
    }
}
