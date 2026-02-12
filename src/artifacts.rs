//! Artifact management for void-box
//!
//! Provides utilities to download and manage pre-built kernel and initramfs artifacts.

use std::path::{Path, PathBuf};
use std::fs;

/// GitHub releases base URL
const GITHUB_RELEASES_URL: &str = "https://github.com/the-void-ia/void-box/releases/download";

/// Paths to void-box artifacts (kernel and initramfs)
#[derive(Debug, Clone)]
pub struct ArtifactPaths {
    /// Path to the kernel image
    pub kernel: PathBuf,
    /// Path to the initramfs image
    pub initramfs: PathBuf,
}

/// Download pre-built artifacts from GitHub releases
///
/// # Example
///
/// ```no_run
/// use void_box::artifacts::download_prebuilt_artifacts;
///
/// let artifacts = download_prebuilt_artifacts("v0.1.0")?;
/// println!("Kernel: {:?}", artifacts.kernel);
/// println!("Initramfs: {:?}", artifacts.initramfs);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn download_prebuilt_artifacts(version: &str) -> Result<ArtifactPaths, Box<dyn std::error::Error>> {
    let cache_dir = artifact_cache_dir()?;

    let arch = std::env::consts::ARCH;
    let initramfs_name = format!("void-box-initramfs-{}-{}.cpio.gz", version, arch);
    let initramfs_url = format!("{}/{}/{}", GITHUB_RELEASES_URL, version, initramfs_name);
    let initramfs_path = cache_dir.join(&initramfs_name);

    // Download if not cached
    if !initramfs_path.exists() {
        eprintln!("Downloading initramfs from {}", initramfs_url);
        return Err("Download not implemented yet - use curl/wget to manually download artifacts".into());
    }

    Ok(ArtifactPaths {
        initramfs: initramfs_path,
        kernel: detect_host_kernel()?,
    })
}

/// Try to detect artifacts from environment variables
///
/// Checks VOID_BOX_KERNEL and VOID_BOX_INITRAMFS environment variables.
pub fn from_env() -> Result<ArtifactPaths, Box<dyn std::error::Error>> {
    let kernel = std::env::var("VOID_BOX_KERNEL")
        .map(PathBuf::from)
        .map_err(|_| "VOID_BOX_KERNEL not set")?;

    let initramfs = std::env::var("VOID_BOX_INITRAMFS")
        .map(PathBuf::from)
        .map_err(|_| "VOID_BOX_INITRAMFS not set")?;

    if !kernel.exists() {
        return Err(format!("Kernel not found: {:?}", kernel).into());
    }

    if !initramfs.exists() {
        return Err(format!("Initramfs not found: {:?}", initramfs).into());
    }

    Ok(ArtifactPaths { kernel, initramfs })
}

/// Get the cache directory for void-box artifacts
fn artifact_cache_dir() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let home = std::env::var("HOME")?;
    let cache = PathBuf::from(home).join(".cache/void-box/artifacts");
    fs::create_dir_all(&cache)?;
    Ok(cache)
}

/// Attempt to detect the host kernel
fn detect_host_kernel() -> Result<PathBuf, Box<dyn std::error::Error>> {
    // Try common kernel locations
    let candidates = [
        format!("/boot/vmlinuz-{}", std::env::consts::OS),
        "/boot/vmlinuz".to_string(),
        format!("/boot/vmlinuz-{}", get_kernel_version()?),
    ];

    for candidate in &candidates {
        let path = Path::new(candidate);
        if path.exists() {
            return Ok(path.to_path_buf());
        }
    }

    Err("Could not detect host kernel. Set VOID_BOX_KERNEL environment variable.".into())
}

/// Get the running kernel version
fn get_kernel_version() -> Result<String, Box<dyn std::error::Error>> {
    let output = std::process::Command::new("uname")
        .arg("-r")
        .output()?;

    if !output.status.success() {
        return Err("Failed to get kernel version".into());
    }

    let version = String::from_utf8(output.stdout)?
        .trim()
        .to_string();

    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_artifact_cache_dir() {
        let cache_dir = artifact_cache_dir();
        assert!(cache_dir.is_ok());
    }

    #[test]
    fn test_get_kernel_version() {
        let version = get_kernel_version();
        // Should work on Linux systems
        #[cfg(target_os = "linux")]
        assert!(version.is_ok());
    }
}
