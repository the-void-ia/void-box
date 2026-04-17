//! Cross-platform snapshot directory management, config hashing, and listing.
//!
//! These utilities are pure filesystem/hashing operations with no KVM or VZ
//! dependencies, so they compile on both Linux and macOS.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::debug;

use crate::{Error, Result};

/// Default snapshot storage directory.
///
/// Checks `VOIDBOX_HOME` first, then falls back to `$HOME/.void-box/snapshots`.
pub fn default_snapshot_dir() -> PathBuf {
    if let Ok(home) = std::env::var("VOIDBOX_HOME") {
        return PathBuf::from(home).join("snapshots");
    }
    dirs_snapshot_base()
}

fn dirs_snapshot_base() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".void-box").join("snapshots")
}

/// Resolve the snapshot directory for a given config hash using a custom base.
pub fn snapshot_dir_for_hash_in(base: &Path, config_hash: &str) -> PathBuf {
    base.join(&config_hash[..16.min(config_hash.len())])
}

/// List all stored snapshots under a custom base directory.
pub fn list_snapshots_in(base: &Path) -> Result<Vec<SnapshotInfo>> {
    if !base.exists() {
        return Ok(Vec::new());
    }
    let mut infos = Vec::new();
    for entry in fs::read_dir(base)? {
        let entry = entry?;
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        if dir.join("state.bin").exists() {
            if let Some(info) = load_kvm_snapshot_info(&dir) {
                infos.push(info);
            }
            continue;
        }
        if dir.join("vz_meta.json").exists() {
            if let Some(info) = load_vz_snapshot_info(&dir) {
                infos.push(info);
            }
        }
    }
    Ok(infos)
}

/// Delete a snapshot by hash prefix under a custom base directory.
pub fn delete_snapshot_in(base: &Path, hash_prefix: &str) -> Result<bool> {
    if !base.exists() {
        return Ok(false);
    }
    for entry in fs::read_dir(base)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(hash_prefix) || hash_prefix.starts_with(&name) {
            fs::remove_dir_all(entry.path())?;
            tracing::info!("Deleted snapshot {}", entry.path().display());
            return Ok(true);
        }
    }
    Ok(false)
}

/// Snapshot type discriminator.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SnapshotType {
    /// Full base snapshot from a cold-booted VM.
    Base,
    /// Differential snapshot on top of a base.
    Diff,
}

impl std::fmt::Display for SnapshotType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SnapshotType::Base => f.write_str("base"),
            SnapshotType::Diff => f.write_str("diff"),
        }
    }
}

/// Information about a stored snapshot (for listing).
#[derive(Debug)]
pub struct SnapshotInfo {
    pub config_hash: String,
    pub snapshot_type: SnapshotType,
    pub memory_mb: usize,
    pub vcpus: usize,
    pub dir: PathBuf,
    pub memory_file_size: u64,
}

/// Compute a deterministic hash of the VM configuration.
///
/// The hash covers kernel binary, initramfs binary, memory size, and vCPU count.
/// Two VMs with the same config_hash can share snapshots.
pub fn compute_config_hash(
    kernel: &Path,
    initramfs: Option<&Path>,
    memory_mb: usize,
    vcpus: usize,
) -> Result<String> {
    let mut hasher = Sha256::new();
    let kernel_data = fs::read(kernel)
        .map_err(|e| Error::Snapshot(format!("read kernel {}: {}", kernel.display(), e)))?;
    hasher.update(&kernel_data);
    if let Some(initramfs) = initramfs {
        let initramfs_data = fs::read(initramfs).map_err(|e| {
            Error::Snapshot(format!("read initramfs {}: {}", initramfs.display(), e))
        })?;
        hasher.update(&initramfs_data);
    }
    hasher.update(memory_mb.to_le_bytes());
    hasher.update(vcpus.to_le_bytes());
    let hash = hasher.finalize();
    Ok(hash
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>())
}

/// Resolve the snapshot directory for a given config hash.
pub fn snapshot_dir_for_hash(config_hash: &str) -> PathBuf {
    default_snapshot_dir().join(&config_hash[..16.min(config_hash.len())])
}

/// List all stored snapshots (both KVM and VZ).
///
/// KVM snapshots are identified by `state.bin`, VZ snapshots by `vz_meta.json`.
pub fn list_snapshots() -> Result<Vec<SnapshotInfo>> {
    let base = default_snapshot_dir();
    if !base.exists() {
        return Ok(Vec::new());
    }
    let mut infos = Vec::new();
    for entry in fs::read_dir(&base)? {
        let entry = entry?;
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }

        // Try KVM snapshot first (state.bin)
        if dir.join("state.bin").exists() {
            if let Some(info) = load_kvm_snapshot_info(&dir) {
                infos.push(info);
            }
            continue;
        }

        // Try VZ snapshot (vz_meta.json)
        if dir.join("vz_meta.json").exists() {
            if let Some(info) = load_vz_snapshot_info(&dir) {
                infos.push(info);
            }
        }
    }
    Ok(infos)
}

/// Load snapshot info from a KVM snapshot directory.
///
/// On macOS, `state.bin` uses `bincode`-serialized `VmSnapshot` which depends
/// on KVM types only available on Linux. We still handle the directory-based
/// metadata (hash from dirname) but skip deserializing the state file itself
/// on non-Linux platforms.
fn load_kvm_snapshot_info(dir: &Path) -> Option<SnapshotInfo> {
    #[cfg(target_os = "linux")]
    {
        use crate::vmm::snapshot::VmSnapshot;
        match VmSnapshot::load(dir) {
            Ok(snap) => {
                let mem_size = fs::metadata(VmSnapshot::memory_path(dir))
                    .map(|m| m.len())
                    .unwrap_or(0);
                Some(SnapshotInfo {
                    config_hash: snap.config_hash,
                    snapshot_type: if snap.parent_id.is_some() {
                        SnapshotType::Diff
                    } else {
                        SnapshotType::Base
                    },
                    memory_mb: snap.config.memory_mb,
                    vcpus: snap.config.vcpus,
                    dir: dir.to_path_buf(),
                    memory_file_size: mem_size,
                })
            }
            Err(e) => {
                debug!("Skipping invalid KVM snapshot {}: {}", dir.display(), e);
                None
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        debug!("Skipping KVM snapshot {} (not on Linux)", dir.display());
        None
    }
}

/// Load snapshot info from a VZ snapshot directory (vz_meta.json).
fn load_vz_snapshot_info(dir: &Path) -> Option<SnapshotInfo> {
    #[cfg(target_os = "macos")]
    {
        use crate::backend::vz::snapshot::VzSnapshotMeta;
        match VzSnapshotMeta::load(dir) {
            Ok(meta) => {
                let mem_size = fs::metadata(VzSnapshotMeta::save_file_path(dir))
                    .map(|m| m.len())
                    .unwrap_or(0);
                let config_hash = dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                Some(SnapshotInfo {
                    config_hash,
                    snapshot_type: SnapshotType::Base,
                    memory_mb: meta.memory_mb,
                    vcpus: meta.vcpus,
                    dir: dir.to_path_buf(),
                    memory_file_size: mem_size,
                })
            }
            Err(e) => {
                debug!("Skipping invalid VZ snapshot {}: {}", dir.display(), e);
                None
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        // On non-macOS, parse vz_meta.json directly as generic JSON to extract
        // basic metadata without depending on the VZ backend module.
        let meta_path = dir.join("vz_meta.json");
        let json = fs::read_to_string(&meta_path).ok()?;
        let value: serde_json::Value = serde_json::from_str(&json).ok()?;
        let memory_mb = value.get("memory_mb")?.as_u64()? as usize;
        let vcpus = value.get("vcpus")?.as_u64()? as usize;
        let config_hash = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mem_size = fs::metadata(dir.join("vm.vzvmsave"))
            .map(|m| m.len())
            .unwrap_or(0);
        Some(SnapshotInfo {
            config_hash,
            snapshot_type: SnapshotType::Base,
            memory_mb,
            vcpus,
            dir: dir.to_path_buf(),
            memory_file_size: mem_size,
        })
    }
}

/// Delete a snapshot by its config hash prefix.
pub fn delete_snapshot(hash_prefix: &str) -> Result<bool> {
    let base = default_snapshot_dir();
    if !base.exists() {
        return Ok(false);
    }
    for entry in fs::read_dir(&base)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(hash_prefix) || hash_prefix.starts_with(&name) {
            fs::remove_dir_all(entry.path())?;
            tracing::info!("Deleted snapshot {}", entry.path().display());
            return Ok(true);
        }
    }
    Ok(false)
}

/// Check whether a snapshot directory contains a valid snapshot (KVM or VZ).
pub fn snapshot_exists(dir: &Path) -> bool {
    dir.join("state.bin").exists() || dir.join("vz_meta.json").exists()
}

/// Outcome of [`resolve_snapshot_argument`].
pub enum SnapshotResolution {
    /// `arg` matched a hash prefix under `~/.void-box/snapshots/`.
    Hash(PathBuf),
    /// `arg` was a literal filesystem path with a valid snapshot.
    Literal(PathBuf),
    /// Neither interpretation found a valid snapshot.
    NotFound { hash_dir: PathBuf, literal: PathBuf },
}

impl SnapshotResolution {
    /// Return the resolved path, or `None` when no interpretation matched.
    pub fn path(self) -> Option<PathBuf> {
        match self {
            SnapshotResolution::Hash(p) | SnapshotResolution::Literal(p) => Some(p),
            SnapshotResolution::NotFound { .. } => None,
        }
    }
}

/// Resolve a user-provided snapshot argument (hash prefix or literal path) to
/// a snapshot directory.
///
/// The same rules are applied by `voidbox run`, `voidbox shell`, and
/// spec-level `sandbox.snapshot` fields so the three agree on what a given
/// string means:
///
/// 1. Try `arg` as a hash prefix under the standard snapshot store
///    (`~/.void-box/snapshots/<arg>`).
/// 2. Fall back to treating `arg` as a literal filesystem path.
/// 3. If neither contains a valid snapshot, return [`SnapshotResolution::NotFound`].
pub fn resolve_snapshot_argument(arg: &str) -> SnapshotResolution {
    let hash_dir = snapshot_dir_for_hash(arg);
    if snapshot_exists(&hash_dir) {
        return SnapshotResolution::Hash(hash_dir);
    }
    let literal = PathBuf::from(arg);
    if snapshot_exists(&literal) {
        return SnapshotResolution::Literal(literal);
    }
    SnapshotResolution::NotFound { hash_dir, literal }
}
