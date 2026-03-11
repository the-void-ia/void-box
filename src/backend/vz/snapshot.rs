//! VZ snapshot metadata sidecar.
//!
//! Apple's `saveMachineStateToURL:completionHandler:` produces an opaque
//! file containing all CPU + memory state.  We store a small JSON sidecar
//! alongside it so we can recover our own metadata (session secret, VM
//! config) on restore.
//!
//! Snapshot directory layout:
//! ```text
//! <snapshot_dir>/
//!   vm.vzvmsave    — Apple's opaque VM state
//!   vz_meta.json   — this sidecar
//! ```

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Name of Apple's opaque VM state file inside the snapshot directory.
pub const VZ_SAVE_FILE: &str = "vm.vzvmsave";

/// Name of our JSON sidecar inside the snapshot directory.
const VZ_META_FILE: &str = "vz_meta.json";

/// Metadata sidecar for VZ snapshots.
///
/// Apple's save file is opaque; we store our own data alongside it so
/// that `start()` can reconstruct the control channel on restore.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VzSnapshotMeta {
    /// 32-byte session secret (hex-encoded for JSON safety).
    pub session_secret: Vec<u8>,
    /// Memory size in megabytes.
    pub memory_mb: usize,
    /// Number of vCPUs.
    pub vcpus: usize,
    /// Whether networking was enabled.
    pub network: bool,
    /// The VM's vsock CID.
    pub cid: u32,
}

impl VzSnapshotMeta {
    /// Persist this metadata as `<dir>/vz_meta.json`.
    pub fn save(&self, dir: &Path) -> Result<()> {
        let path = dir.join(VZ_META_FILE);
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| crate::Error::Snapshot(format!("serialize vz_meta: {e}")))?;
        std::fs::write(&path, json)
            .map_err(|e| crate::Error::Snapshot(format!("write {}: {e}", path.display())))?;
        Ok(())
    }

    /// Load metadata from `<dir>/vz_meta.json`.
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join(VZ_META_FILE);
        let json = std::fs::read_to_string(&path)
            .map_err(|e| crate::Error::Snapshot(format!("read {}: {e}", path.display())))?;
        let meta: Self = serde_json::from_str(&json)
            .map_err(|e| crate::Error::Snapshot(format!("parse vz_meta: {e}")))?;
        Ok(meta)
    }

    /// Path to Apple's opaque save file inside the snapshot directory.
    pub fn save_file_path(dir: &Path) -> std::path::PathBuf {
        dir.join(VZ_SAVE_FILE)
    }
}
