//! Artifact management for void-box
//!
//! Provides utilities to download and manage pre-built kernel and initramfs artifacts.

use std::fs;
use std::path::{Path, PathBuf};

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
pub fn download_prebuilt_artifacts(
    version: &str,
) -> Result<ArtifactPaths, Box<dyn std::error::Error>> {
    let cache_dir = artifact_cache_dir()?;

    let arch = std::env::consts::ARCH;
    let initramfs_name = format!("void-box-initramfs-{}-{}.cpio.gz", version, arch);
    let initramfs_url = format!("{}/{}/{}", GITHUB_RELEASES_URL, version, initramfs_name);
    let initramfs_path = cache_dir.join(&initramfs_name);

    // Download if not cached
    if !initramfs_path.exists() {
        eprintln!("Downloading initramfs from {}", initramfs_url);
        return Err(
            "Download not implemented yet - use curl/wget to manually download artifacts".into(),
        );
    }

    Ok(ArtifactPaths {
        initramfs: initramfs_path,
        kernel: detect_host_kernel()?,
    })
}

/// Try to resolve artifacts from well-known installation paths.
///
/// Checks platform-specific directories where package managers install artifacts:
/// - Linux: `/usr/lib/voidbox/` (vmlinuz + initramfs.cpio.gz)
/// - macOS: `/opt/homebrew/lib/voidbox/` and `/usr/local/lib/voidbox/` (vmlinux + initramfs.cpio.gz)
///
/// macOS uses `vmlinux` (uncompressed) because Apple's Virtualization.framework requires it.
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

/// Return well-known directories where packaged artifacts may be installed.
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

/// Return the expected kernel filename for the current platform.
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
    let output = std::process::Command::new("uname").arg("-r").output()?;

    if !output.status.success() {
        return Err("Failed to get kernel version".into());
    }

    let version = String::from_utf8(output.stdout)?.trim().to_string();

    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn test_artifact_cache_dir() {
        let original_home = std::env::var_os("HOME");
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock drift")
            .as_nanos();
        let test_home = std::env::temp_dir().join(format!(
            "voidbox-artifacts-home-{}-{}",
            std::process::id(),
            unique
        ));
        fs::create_dir_all(&test_home).expect("create test HOME");
        std::env::set_var("HOME", &test_home);

        let cache_dir = artifact_cache_dir();
        assert!(cache_dir.is_ok());
        let cache_dir = cache_dir.unwrap();
        assert!(cache_dir.ends_with(".cache/void-box/artifacts"));
        assert!(cache_dir.exists());

        if let Some(prev) = original_home {
            std::env::set_var("HOME", prev);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn test_installed_artifact_dirs_not_empty() {
        let dirs = super::installed_artifact_dirs();
        assert!(!dirs.is_empty());
    }

    #[test]
    fn test_installed_kernel_name() {
        let name = super::installed_kernel_name();
        #[cfg(target_os = "macos")]
        assert_eq!(name, "vmlinux");
        #[cfg(not(target_os = "macos"))]
        assert_eq!(name, "vmlinuz");
    }

    #[test]
    fn test_resolve_installed_artifacts_returns_none_when_missing() {
        // Unless someone actually has /usr/lib/voidbox/ populated, this should be None
        // We can't guarantee the path doesn't exist, so just verify it doesn't panic
        let _result = super::resolve_installed_artifacts();
    }

    #[test]
    fn test_get_kernel_version() {
        let version = get_kernel_version();
        // Should work on Linux systems
        #[cfg(target_os = "linux")]
        assert!(version.is_ok());
        #[cfg(not(target_os = "linux"))]
        let _ = version;
    }
}
