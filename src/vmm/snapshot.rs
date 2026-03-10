//! VM snapshot support for sub-second restore
//!
//! Static golden snapshots (full memory dump + KVM state)
//! Layered diff snapshots (dirty page tracking)
//! JIT post-init snapshots (guest-triggered)

use std::fs;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{debug, info};
use vm_memory::{Address, GuestMemory, GuestMemoryMmap, GuestMemoryRegion};

use crate::{Error, Result};

/// Snapshot format version for forward compatibility.
pub const SNAPSHOT_VERSION: u32 = 1;

/// Default snapshot storage directory.
pub fn default_snapshot_dir() -> PathBuf {
    dirs_snapshot_base()
}

fn dirs_snapshot_base() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".void-box").join("snapshots")
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Snapshot type discriminator.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SnapshotType {
    /// Full base snapshot from a cold-booted VM.
    Base,
    /// Differential snapshot on top of a base.
    Diff,
}

/// Hardware configuration captured at snapshot time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotConfig {
    pub memory_mb: usize,
    pub vcpus: usize,
    pub cid: u32,
    pub vsock_mmio_base: u64,
    pub network: bool,
}

/// Serializable vCPU state (raw bytes of KVM structs).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VcpuState {
    /// `kvm_regs` as raw bytes.
    pub regs: Vec<u8>,
    /// `kvm_sregs` as raw bytes.
    pub sregs: Vec<u8>,
    /// `kvm_lapic_state` as raw bytes.
    pub lapic: Vec<u8>,
    /// `kvm_xsave` as raw bytes.
    pub xsave: Vec<u8>,
    /// MSR (index, value) pairs.
    pub msrs: Vec<(u32, u64)>,
    /// `kvm_vcpu_events` as raw bytes (interrupt/exception delivery state).
    #[serde(default)]
    pub vcpu_events: Vec<u8>,
    /// `kvm_xcrs` as raw bytes (XCR0 — controls which XSAVE features are active).
    #[serde(default)]
    pub xcrs: Vec<u8>,
}

/// IRQ chip state (PIC master + PIC slave + IOAPIC) as raw bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IrqchipState {
    /// Raw bytes of `kvm_irqchip` with chip_id = 0 (PIC master).
    pub pic_master: Vec<u8>,
    /// Raw bytes of `kvm_irqchip` with chip_id = 1 (PIC slave).
    pub pic_slave: Vec<u8>,
    /// Raw bytes of `kvm_irqchip` with chip_id = 2 (IOAPIC).
    pub ioapic: Vec<u8>,
}

/// Snapshot of a single virtio queue's software state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueSnapshotState {
    pub num_max: u16,
    pub num: u16,
    pub ready: bool,
    pub desc_addr: u64,
    pub driver_addr: u64,
    pub device_addr: u64,
    /// Userspace backend only: last consumed available-ring index.
    /// `None` for the vhost kernel backend (kernel tracks this internally).
    #[serde(default)]
    pub last_avail_idx: Option<u16>,
    /// Userspace backend only: last produced used-ring index.
    /// `None` for the vhost kernel backend.
    #[serde(default)]
    pub last_used_idx: Option<u16>,
}

/// Serializable virtio-vsock MMIO device state (excludes FDs).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VsockSnapshotState {
    pub device_features: u64,
    pub driver_features: u64,
    pub features_sel: u32,
    pub queue_sel: u32,
    pub status: u32,
    pub interrupt_status: u32,
    pub config_generation: u32,
    /// rx(0), tx(1), event(2) queue state.
    pub queues: Vec<QueueSnapshotState>,
}

/// Top-level VM snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmSnapshot {
    /// Format version for forward compat.
    pub version: u32,
    /// None for base, Some(hash) for diff snapshots.
    pub parent_id: Option<String>,
    /// Per-vCPU register state.
    pub vcpu_states: Vec<VcpuState>,
    /// In-kernel irqchip state.
    pub irqchip: IrqchipState,
    /// PIT state as raw bytes of `kvm_pit_state2`.
    pub pit: Vec<u8>,
    /// Virtio-vsock device state.
    pub vsock_state: VsockSnapshotState,
    /// Hardware config at snapshot time.
    pub config: SnapshotConfig,
    /// `sha256(kernel + initramfs + memory_mb + vcpus)`.
    pub config_hash: String,
    /// Base | Diff.
    pub snapshot_type: SnapshotType,
    /// Session secret that the guest-agent expects (from kernel cmdline).
    pub session_secret: Vec<u8>,
}

impl VmSnapshot {
    /// Persist snapshot state to `<dir>/state.bin`.
    ///
    /// Memory is saved separately via [`dump_memory`].
    pub fn save(&self, dir: &Path) -> Result<()> {
        fs::create_dir_all(dir).map_err(|e| {
            Error::Snapshot(format!("create snapshot dir {}: {}", dir.display(), e))
        })?;
        let state_bytes = bincode::serialize(self)
            .map_err(|e| Error::Snapshot(format!("serialize state: {}", e)))?;
        fs::write(dir.join("state.bin"), &state_bytes)?;
        info!(
            "Saved snapshot state ({} bytes) to {}",
            state_bytes.len(),
            dir.display()
        );
        Ok(())
    }

    /// Load a snapshot from `<dir>/state.bin`.
    pub fn load(dir: &Path) -> Result<Self> {
        let state_path = dir.join("state.bin");
        let state_bytes = fs::read(&state_path)
            .map_err(|e| Error::Snapshot(format!("read {}: {}", state_path.display(), e)))?;
        let snapshot: VmSnapshot = bincode::deserialize(&state_bytes)
            .map_err(|e| Error::Snapshot(format!("deserialize state: {}", e)))?;
        if snapshot.version != SNAPSHOT_VERSION {
            return Err(Error::Snapshot(format!(
                "unsupported snapshot version {} (expected {})",
                snapshot.version, SNAPSHOT_VERSION
            )));
        }
        debug!(
            "Loaded snapshot: hash={}, vcpus={}, memory={}MB",
            snapshot.config_hash, snapshot.config.vcpus, snapshot.config.memory_mb
        );
        Ok(snapshot)
    }

    /// Return the memory dump path for this snapshot directory.
    pub fn memory_path(dir: &Path) -> PathBuf {
        dir.join("memory.mem")
    }

    /// Return the diff memory path for this snapshot directory.
    pub fn diff_memory_path(dir: &Path) -> PathBuf {
        dir.join("memory.diff")
    }
}

// ---------------------------------------------------------------------------
// Memory dump / restore
// ---------------------------------------------------------------------------

/// Write all guest memory regions to a flat file.
pub fn dump_memory(memory: &GuestMemoryMmap, path: &Path) -> Result<()> {
    let mut file = fs::File::create(path)?;
    for region in memory.iter() {
        let host_addr = memory
            .get_host_address(region.start_addr())
            .map_err(|e| Error::Memory(format!("get_host_address: {}", e)))?;
        let slice =
            unsafe { std::slice::from_raw_parts(host_addr as *const u8, region.len() as usize) };
        file.write_all(slice)?;
    }
    file.flush()?;
    file.sync_all()?;
    info!(
        "Dumped {} bytes of guest memory to {}",
        memory.iter().map(|r| r.len()).sum::<u64>(),
        path.display()
    );
    Ok(())
}

/// Restore guest memory from a dump file using `mmap(MAP_PRIVATE | MAP_FIXED)`.
///
/// This replaces the anonymous memory mapping with a file-backed COW mapping.
/// Pages are loaded lazily from the file; writes create anonymous copies.
pub fn restore_memory(memory: &GuestMemoryMmap, path: &Path) -> Result<()> {
    let file = fs::File::open(path)
        .map_err(|e| Error::Snapshot(format!("open memory file {}: {}", path.display(), e)))?;
    let file_fd = file.as_raw_fd();

    let mut offset: i64 = 0;
    for region in memory.iter() {
        let host_addr = memory
            .get_host_address(region.start_addr())
            .map_err(|e| Error::Memory(format!("get_host_address: {}", e)))?;
        let size = region.len() as usize;

        let result = unsafe {
            libc::mmap(
                host_addr as *mut libc::c_void,
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_FIXED,
                file_fd,
                offset,
            )
        };
        if result == libc::MAP_FAILED {
            return Err(Error::Memory(format!(
                "mmap restore failed at offset {}: {}",
                offset,
                std::io::Error::last_os_error()
            )));
        }
        debug!(
            "Restored memory region: GPA={:#x}, size={:#x}, file_offset={}",
            region.start_addr().raw_value(),
            size,
            offset
        );
        offset += size as i64;
    }

    info!(
        "Restored guest memory from {} (COW, lazy page loading)",
        path.display()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Diff snapshot: dump / restore / merge
// ---------------------------------------------------------------------------

/// Page size used for dirty page tracking (4 KiB).
pub const PAGE_SIZE: usize = 4096;

/// Header for a diff memory file.
///
/// The file format is:
/// 1. `DiffMemoryHeader` serialized with bincode
/// 2. For each dirty page (in order): 4096 bytes of page data
///
/// `dirty_page_indices` lists the absolute page index within the guest
/// memory for each dirty page, in ascending order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffMemoryHeader {
    /// Total guest memory size in bytes (must match base).
    pub total_memory_size: u64,
    /// Sorted list of dirty page indices (0-based, each page = 4096 bytes).
    pub dirty_page_indices: Vec<u64>,
}

/// Dump only dirty guest memory pages to a diff file.
///
/// `dirty_bitmaps` is the output of `Vm::get_dirty_bitmap()`: a list of
/// `(slot, bitmap)` pairs where each bit represents one 4 KiB page.
///
/// The resulting file contains a `DiffMemoryHeader` followed by the raw
/// bytes of each dirty page, in page-index order.
pub fn dump_memory_diff(
    memory: &GuestMemoryMmap,
    dirty_bitmaps: &[(u32, Vec<u64>)],
    path: &Path,
) -> Result<()> {
    // Compute total memory size and collect dirty page indices
    let total_memory_size: u64 = memory.iter().map(|r| r.len()).sum();
    let mut dirty_page_indices: Vec<u64> = Vec::new();

    let mut page_offset: u64 = 0; // running page offset across regions
    for (_slot, bitmap) in dirty_bitmaps {
        for (word_idx, &word) in bitmap.iter().enumerate() {
            if word == 0 {
                continue;
            }
            for bit in 0..64u32 {
                if word & (1u64 << bit) != 0 {
                    let page_idx = page_offset + (word_idx as u64 * 64) + bit as u64;
                    dirty_page_indices.push(page_idx);
                }
            }
        }
        // Advance page_offset by the number of pages this slot covers.
        // Each word in bitmap covers 64 pages.
        let pages_in_slot = bitmap.len() as u64 * 64;
        page_offset += pages_in_slot;
    }

    dirty_page_indices.sort_unstable();

    let header = DiffMemoryHeader {
        total_memory_size,
        dirty_page_indices: dirty_page_indices.clone(),
    };

    let mut file = fs::File::create(path)?;
    let header_bytes = bincode::serialize(&header)
        .map_err(|e| Error::Snapshot(format!("serialize diff header: {}", e)))?;
    // Write header length (u64 LE) then header bytes, so we can parse them back
    file.write_all(&(header_bytes.len() as u64).to_le_bytes())?;
    file.write_all(&header_bytes)?;

    // Write dirty pages
    for &page_idx in &dirty_page_indices {
        let guest_offset = page_idx * PAGE_SIZE as u64;
        let host_addr = memory
            .get_host_address(vm_memory::GuestAddress(guest_offset))
            .map_err(|e| Error::Memory(format!("get_host_address for page {}: {}", page_idx, e)))?;
        let slice = unsafe { std::slice::from_raw_parts(host_addr as *const u8, PAGE_SIZE) };
        file.write_all(slice)?;
    }

    file.flush()?;
    file.sync_all()?;

    info!(
        "Dumped diff memory: {} dirty pages ({} KiB) out of {} total pages to {}",
        dirty_page_indices.len(),
        dirty_page_indices.len() * PAGE_SIZE / 1024,
        total_memory_size / PAGE_SIZE as u64,
        path.display()
    );
    Ok(())
}

/// Restore guest memory from a diff file, applying dirty pages on top of
/// the current (base-restored) memory.
///
/// Precondition: the base memory has already been restored via [`restore_memory`].
pub fn restore_memory_diff(memory: &GuestMemoryMmap, path: &Path) -> Result<()> {
    let mut file = fs::File::open(path)
        .map_err(|e| Error::Snapshot(format!("open diff memory file {}: {}", path.display(), e)))?;

    // Read header
    let header = read_diff_header(&mut file)?;

    // Read and apply each dirty page
    let mut page_buf = vec![0u8; PAGE_SIZE];
    for &page_idx in &header.dirty_page_indices {
        use std::io::Read;
        file.read_exact(&mut page_buf)
            .map_err(|e| Error::Snapshot(format!("read diff page {}: {}", page_idx, e)))?;
        let guest_addr = vm_memory::GuestAddress(page_idx * PAGE_SIZE as u64);
        vm_memory::Bytes::write(memory, &page_buf, guest_addr)
            .map_err(|e| Error::Memory(format!("write diff page {} to guest: {}", page_idx, e)))?;
    }

    info!(
        "Applied {} dirty pages from diff {}",
        header.dirty_page_indices.len(),
        path.display()
    );
    Ok(())
}

/// Merge a base memory file with a diff file, producing a full memory file.
///
/// The output file can be restored with [`restore_memory`].
pub fn merge_snapshots(
    base_mem_path: &Path,
    diff_mem_path: &Path,
    output_path: &Path,
) -> Result<()> {
    // Read the diff header
    let mut diff_file = fs::File::open(diff_mem_path).map_err(|e| {
        Error::Snapshot(format!("open diff file {}: {}", diff_mem_path.display(), e))
    })?;
    let header = read_diff_header(&mut diff_file)?;

    // Copy base to output
    fs::copy(base_mem_path, output_path).map_err(|e| {
        Error::Snapshot(format!(
            "copy base {} to {}: {}",
            base_mem_path.display(),
            output_path.display(),
            e
        ))
    })?;

    // Open output for in-place patching
    let mut output = fs::OpenOptions::new()
        .write(true)
        .open(output_path)
        .map_err(|e| Error::Snapshot(format!("open output {}: {}", output_path.display(), e)))?;

    // Apply each dirty page at its correct offset
    let mut page_buf = vec![0u8; PAGE_SIZE];
    for &page_idx in &header.dirty_page_indices {
        use std::io::{Read, Seek, SeekFrom};
        diff_file
            .read_exact(&mut page_buf)
            .map_err(|e| Error::Snapshot(format!("read diff page {}: {}", page_idx, e)))?;
        let file_offset = page_idx * PAGE_SIZE as u64;
        output
            .seek(SeekFrom::Start(file_offset))
            .map_err(|e| Error::Snapshot(format!("seek to page {} in output: {}", page_idx, e)))?;
        output.write_all(&page_buf)?;
    }

    output.flush()?;
    output.sync_all()?;

    info!(
        "Merged base ({}) + diff ({}) → {} ({} pages patched)",
        base_mem_path.display(),
        diff_mem_path.display(),
        output_path.display(),
        header.dirty_page_indices.len()
    );
    Ok(())
}

/// Read the `DiffMemoryHeader` from the beginning of a diff memory file.
fn read_diff_header(file: &mut fs::File) -> Result<DiffMemoryHeader> {
    use std::io::Read;
    // Read header length
    let mut len_buf = [0u8; 8];
    file.read_exact(&mut len_buf)
        .map_err(|e| Error::Snapshot(format!("read diff header length: {}", e)))?;
    let header_len = u64::from_le_bytes(len_buf) as usize;

    if header_len > 64 * 1024 * 1024 {
        return Err(Error::Snapshot(format!(
            "diff header too large: {} bytes",
            header_len
        )));
    }

    let mut header_bytes = vec![0u8; header_len];
    file.read_exact(&mut header_bytes)
        .map_err(|e| Error::Snapshot(format!("read diff header: {}", e)))?;

    bincode::deserialize(&header_bytes)
        .map_err(|e| Error::Snapshot(format!("deserialize diff header: {}", e)))
}

// ---------------------------------------------------------------------------
// Snapshot cache management
// ---------------------------------------------------------------------------

/// Compute a cache key for a diff/layered snapshot.
///
/// The key is a SHA-256 hash of: `base_config_hash + layer_name + content_hash`.
/// Two VMs with the same base, layer name, and content produce the same key.
pub fn compute_layer_hash(base_config_hash: &str, layer_name: &str, content_hash: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(base_config_hash.as_bytes());
    hasher.update(b":");
    hasher.update(layer_name.as_bytes());
    hasher.update(b":");
    hasher.update(content_hash.as_bytes());
    let hash = hasher.finalize();
    hash.iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>()
}

/// Find a cached snapshot matching the given layer hash.
///
/// Returns the snapshot directory if a valid snapshot exists for this hash.
pub fn find_cached_snapshot(layer_hash: &str) -> Option<PathBuf> {
    let dir = snapshot_dir_for_hash(layer_hash);
    if dir.join("state.bin").exists() {
        Some(dir)
    } else {
        None
    }
}

/// Snapshot cache statistics.
#[derive(Debug)]
pub struct CacheStats {
    pub total_snapshots: usize,
    pub total_size_bytes: u64,
    pub entries: Vec<CacheEntry>,
}

/// A single cache entry with metadata for LRU eviction.
#[derive(Debug)]
pub struct CacheEntry {
    pub hash_prefix: String,
    pub snapshot_type: SnapshotType,
    pub size_bytes: u64,
    pub modified: std::time::SystemTime,
    pub dir: PathBuf,
}

/// Get cache statistics for all stored snapshots.
pub fn cache_stats() -> Result<CacheStats> {
    let base = dirs_snapshot_base();
    if !base.exists() {
        return Ok(CacheStats {
            total_snapshots: 0,
            total_size_bytes: 0,
            entries: Vec::new(),
        });
    }

    let mut entries = Vec::new();
    let mut total_size: u64 = 0;

    for entry in fs::read_dir(&base)? {
        let entry = entry?;
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let state_path = dir.join("state.bin");
        if !state_path.exists() {
            continue;
        }

        let snap = match VmSnapshot::load(&dir) {
            Ok(s) => s,
            Err(_) => continue,
        };

        let dir_size = dir_size_recursive(&dir);
        let modified = fs::metadata(&state_path)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);

        total_size += dir_size;
        entries.push(CacheEntry {
            hash_prefix: entry.file_name().to_string_lossy().to_string(),
            snapshot_type: snap.snapshot_type,
            size_bytes: dir_size,
            modified,
            dir,
        });
    }

    // Sort by modification time (oldest first) for LRU
    entries.sort_by_key(|e| e.modified);

    Ok(CacheStats {
        total_snapshots: entries.len(),
        total_size_bytes: total_size,
        entries,
    })
}

/// Evict snapshots using LRU until total cache size is under `max_bytes`.
///
/// Returns the number of snapshots evicted.
pub fn evict_lru(max_bytes: u64) -> Result<usize> {
    let mut stats = cache_stats()?;
    let mut evicted = 0;

    while stats.total_size_bytes > max_bytes && !stats.entries.is_empty() {
        let oldest = stats.entries.remove(0);
        info!(
            "Evicting snapshot {} ({} bytes, type={:?})",
            oldest.hash_prefix, oldest.size_bytes, oldest.snapshot_type
        );
        fs::remove_dir_all(&oldest.dir)?;
        stats.total_size_bytes -= oldest.size_bytes;
        evicted += 1;
    }

    if evicted > 0 {
        info!(
            "Evicted {} snapshots, cache now {} bytes",
            evicted, stats.total_size_bytes
        );
    }

    Ok(evicted)
}

/// Compute total size of a directory recursively.
fn dir_size_recursive(path: &Path) -> u64 {
    let mut total = 0u64;
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.is_file() {
                total += meta.len();
            } else if meta.is_dir() {
                total += dir_size_recursive(&entry.path());
            }
        }
    }
    total
}

// ---------------------------------------------------------------------------
// Config hash
// ---------------------------------------------------------------------------

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
    dirs_snapshot_base().join(&config_hash[..16.min(config_hash.len())])
}

// ---------------------------------------------------------------------------
// Snapshot listing / deletion
// ---------------------------------------------------------------------------

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

/// List all stored snapshots.
pub fn list_snapshots() -> Result<Vec<SnapshotInfo>> {
    let base = dirs_snapshot_base();
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
        let state_path = dir.join("state.bin");
        if !state_path.exists() {
            continue;
        }
        match VmSnapshot::load(&dir) {
            Ok(snap) => {
                let mem_size = fs::metadata(VmSnapshot::memory_path(&dir))
                    .map(|m| m.len())
                    .unwrap_or(0);
                infos.push(SnapshotInfo {
                    config_hash: snap.config_hash,
                    snapshot_type: snap.snapshot_type,
                    memory_mb: snap.config.memory_mb,
                    vcpus: snap.config.vcpus,
                    dir,
                    memory_file_size: mem_size,
                });
            }
            Err(e) => {
                debug!("Skipping invalid snapshot {}: {}", dir.display(), e);
            }
        }
    }
    Ok(infos)
}

/// Delete a snapshot by its config hash prefix.
pub fn delete_snapshot(hash_prefix: &str) -> Result<bool> {
    let base = dirs_snapshot_base();
    if !base.exists() {
        return Ok(false);
    }
    for entry in fs::read_dir(&base)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(hash_prefix) || hash_prefix.starts_with(&name) {
            fs::remove_dir_all(entry.path())?;
            info!("Deleted snapshot {}", entry.path().display());
            return Ok(true);
        }
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// Raw byte helpers for KVM struct serialization
// ---------------------------------------------------------------------------

/// Convert a fixed-size `repr(C)` struct to a byte vector.
///
/// # Safety
/// The caller must ensure `T` is a plain-old-data type with no pointers.
pub fn kvm_struct_to_bytes<T: Sized>(val: &T) -> Vec<u8> {
    unsafe {
        std::slice::from_raw_parts(val as *const T as *const u8, std::mem::size_of::<T>()).to_vec()
    }
}

/// Restore a fixed-size `repr(C)` struct from a byte slice.
///
/// # Safety
/// The caller must ensure `T` is a plain-old-data type and that `bytes`
/// was produced by [`kvm_struct_to_bytes`] for the same type.
pub fn kvm_struct_from_bytes<T: Sized>(bytes: &[u8]) -> Result<T> {
    let expected = std::mem::size_of::<T>();
    if bytes.len() != expected {
        return Err(Error::Snapshot(format!(
            "size mismatch: expected {} bytes for {}, got {}",
            expected,
            std::any::type_name::<T>(),
            bytes.len(),
        )));
    }
    unsafe {
        let mut val = std::mem::MaybeUninit::<T>::zeroed();
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), val.as_mut_ptr() as *mut u8, expected);
        Ok(val.assume_init())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_snapshot_type_serde_roundtrip() {
        let types = vec![SnapshotType::Base, SnapshotType::Diff];
        for t in types {
            let bytes = bincode::serialize(&t).unwrap();
            let restored: SnapshotType = bincode::deserialize(&bytes).unwrap();
            assert_eq!(t, restored);
        }
    }

    #[test]
    fn test_vcpu_state_serde_roundtrip() {
        let state = VcpuState {
            regs: vec![1, 2, 3, 4],
            sregs: vec![5, 6, 7, 8],
            lapic: vec![9, 10],
            xsave: vec![11, 12, 13],
            msrs: vec![(0x10, 1000), (0xC0000080, 2000)],
            vcpu_events: vec![],
            xcrs: vec![],
        };
        let bytes = bincode::serialize(&state).unwrap();
        let restored: VcpuState = bincode::deserialize(&bytes).unwrap();
        assert_eq!(state.regs, restored.regs);
        assert_eq!(state.msrs, restored.msrs);
    }

    #[test]
    fn test_kvm_struct_bytes_roundtrip() {
        #[repr(C)]
        #[derive(Debug, PartialEq)]
        struct TestStruct {
            a: u64,
            b: u32,
            c: u16,
        }
        let original = TestStruct { a: 42, b: 99, c: 7 };
        let bytes = kvm_struct_to_bytes(&original);
        let restored: TestStruct = kvm_struct_from_bytes(&bytes).unwrap();
        assert_eq!(original, restored);
    }

    #[test]
    fn test_kvm_struct_bytes_size_mismatch() {
        #[repr(C)]
        struct Small {
            a: u32,
        }
        let bytes = vec![1, 2, 3]; // too small
        assert!(kvm_struct_from_bytes::<Small>(&bytes).is_err());
    }

    #[test]
    fn test_snapshot_dir_for_hash() {
        let hash = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        let dir = snapshot_dir_for_hash(hash);
        assert!(dir.to_string_lossy().ends_with("abcdef0123456789"));
    }

    #[test]
    fn test_vm_snapshot_serde_roundtrip() {
        let snap = VmSnapshot {
            version: SNAPSHOT_VERSION,
            parent_id: None,
            vcpu_states: vec![VcpuState {
                regs: vec![0; 16],
                sregs: vec![0; 32],
                lapic: vec![0; 8],
                xsave: vec![0; 64],
                msrs: vec![(0x10, 100)],
                vcpu_events: vec![],
                xcrs: vec![],
            }],
            irqchip: IrqchipState {
                pic_master: vec![0; 64],
                pic_slave: vec![0; 64],
                ioapic: vec![0; 128],
            },
            pit: vec![0; 32],
            vsock_state: VsockSnapshotState {
                device_features: 1 << 32,
                driver_features: 1 << 32,
                features_sel: 0,
                queue_sel: 0,
                status: 0x0f,
                interrupt_status: 0,
                config_generation: 0,
                queues: vec![QueueSnapshotState {
                    num_max: 256,
                    num: 128,
                    ready: true,
                    desc_addr: 0x1000,
                    driver_addr: 0x2000,
                    device_addr: 0x3000,
                    last_avail_idx: None,
                    last_used_idx: None,
                }],
            },
            config: SnapshotConfig {
                memory_mb: 128,
                vcpus: 1,
                cid: 42,
                vsock_mmio_base: 0xd080_0000,
                network: false,
            },
            config_hash: "abc123".into(),
            snapshot_type: SnapshotType::Base,
            session_secret: vec![0xAA; 32],
        };
        let bytes = bincode::serialize(&snap).unwrap();
        let restored: VmSnapshot = bincode::deserialize(&bytes).unwrap();
        assert_eq!(restored.version, SNAPSHOT_VERSION);
        assert_eq!(restored.config_hash, "abc123");
        assert_eq!(restored.vcpu_states.len(), 1);
        assert_eq!(restored.session_secret.len(), 32);
    }

    #[test]
    fn test_diff_header_serde_roundtrip() {
        let header = DiffMemoryHeader {
            total_memory_size: 256 * 1024 * 1024,
            dirty_page_indices: vec![0, 5, 10, 100, 1000],
        };
        let bytes = bincode::serialize(&header).unwrap();
        let restored: DiffMemoryHeader = bincode::deserialize(&bytes).unwrap();
        assert_eq!(restored.total_memory_size, header.total_memory_size);
        assert_eq!(restored.dirty_page_indices, header.dirty_page_indices);
    }

    #[test]
    fn test_diff_memory_dump_and_restore() {
        use vm_memory::GuestAddress;

        // Create memory with 4 pages (16 KiB)
        let page_count = 4;
        let mem_size = page_count * PAGE_SIZE;
        let memory = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), mem_size)]).unwrap();

        // Write distinct patterns to each page
        for i in 0..page_count {
            let pattern = vec![(i as u8).wrapping_mul(37); PAGE_SIZE];
            vm_memory::Bytes::write(&memory, &pattern, GuestAddress((i * PAGE_SIZE) as u64))
                .unwrap();
        }

        // Simulate dirty bitmap: pages 1 and 3 are dirty (bit 1 and bit 3)
        let bitmap_word = (1u64 << 1) | (1u64 << 3);
        let dirty_bitmaps = vec![(0u32, vec![bitmap_word])];

        let dir = tempfile::tempdir().unwrap();
        let diff_path = dir.path().join("test.diff");

        // Dump diff
        dump_memory_diff(&memory, &dirty_bitmaps, &diff_path).unwrap();

        // Create fresh memory and write base content (all zeros)
        let memory2 = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), mem_size)]).unwrap();

        // Restore diff onto fresh memory
        restore_memory_diff(&memory2, &diff_path).unwrap();

        // Pages 1 and 3 should have the original pattern; pages 0 and 2 should be zeros
        for i in 0..page_count {
            let mut buf = vec![0u8; PAGE_SIZE];
            vm_memory::Bytes::read(&memory2, &mut buf, GuestAddress((i * PAGE_SIZE) as u64))
                .unwrap();

            if i == 1 || i == 3 {
                let expected = vec![(i as u8).wrapping_mul(37); PAGE_SIZE];
                assert_eq!(buf, expected, "dirty page {} content mismatch", i);
            } else {
                let expected = vec![0u8; PAGE_SIZE];
                assert_eq!(buf, expected, "clean page {} should be zeros", i);
            }
        }
    }

    #[test]
    fn test_merge_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let base_path = dir.path().join("base.mem");
        let diff_path = dir.path().join("diff.mem");
        let merged_path = dir.path().join("merged.mem");

        // Create base: 4 pages of distinct content
        let page_count = 4;
        let mem_size = page_count * PAGE_SIZE;
        let base_data: Vec<u8> = (0..page_count)
            .flat_map(|i| vec![(i as u8) + 0xA0; PAGE_SIZE])
            .collect();
        fs::write(&base_path, &base_data).unwrap();

        // Create diff memory with modified pages 0 and 2
        let memory =
            GuestMemoryMmap::from_ranges(&[(vm_memory::GuestAddress(0), mem_size)]).unwrap();

        // Write base content first
        vm_memory::Bytes::write(&memory, &base_data, vm_memory::GuestAddress(0)).unwrap();

        // Modify pages 0 and 2
        let page0_new = vec![0xFF; PAGE_SIZE];
        let page2_new = vec![0xBB; PAGE_SIZE];
        vm_memory::Bytes::write(&memory, &page0_new, vm_memory::GuestAddress(0)).unwrap();
        vm_memory::Bytes::write(
            &memory,
            &page2_new,
            vm_memory::GuestAddress((2 * PAGE_SIZE) as u64),
        )
        .unwrap();

        let bitmap_word = (1u64 << 0) | (1u64 << 2);
        let dirty_bitmaps = vec![(0u32, vec![bitmap_word])];
        dump_memory_diff(&memory, &dirty_bitmaps, &diff_path).unwrap();

        // Merge
        merge_snapshots(&base_path, &diff_path, &merged_path).unwrap();

        // Verify merged content
        let merged = fs::read(&merged_path).unwrap();
        assert_eq!(merged.len(), mem_size);

        // Page 0: should be 0xFF (from diff)
        assert!(
            merged[..PAGE_SIZE].iter().all(|&b| b == 0xFF),
            "page 0 should be 0xFF"
        );
        // Page 1: should be 0xA1 (from base, unchanged)
        assert!(
            merged[PAGE_SIZE..2 * PAGE_SIZE].iter().all(|&b| b == 0xA1),
            "page 1 should be 0xA1"
        );
        // Page 2: should be 0xBB (from diff)
        assert!(
            merged[2 * PAGE_SIZE..3 * PAGE_SIZE]
                .iter()
                .all(|&b| b == 0xBB),
            "page 2 should be 0xBB"
        );
        // Page 3: should be 0xA3 (from base, unchanged)
        assert!(
            merged[3 * PAGE_SIZE..4 * PAGE_SIZE]
                .iter()
                .all(|&b| b == 0xA3),
            "page 3 should be 0xA3"
        );
    }

    #[test]
    fn test_compute_layer_hash() {
        let hash1 = compute_layer_hash("base123", "analyst", "content456");
        let hash2 = compute_layer_hash("base123", "analyst", "content456");
        let hash3 = compute_layer_hash("base123", "coder", "content456");
        assert_eq!(hash1, hash2, "same inputs should produce same hash");
        assert_ne!(
            hash1, hash3,
            "different layer names should produce different hashes"
        );
        assert_eq!(hash1.len(), 64, "should be SHA-256 hex string");
    }

    #[test]
    fn test_find_cached_snapshot_not_found() {
        assert!(find_cached_snapshot("nonexistent_hash_12345678").is_none());
    }

    #[test]
    fn test_memory_dump_restore() {
        use vm_memory::GuestAddress;

        let memory = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 4096)]).unwrap();

        // Write test pattern
        let pattern: Vec<u8> = (0..4096u16).map(|i| (i % 256) as u8).collect();
        vm_memory::Bytes::write(&memory, &pattern, GuestAddress(0)).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let mem_path = dir.path().join("test.mem");

        // Dump
        dump_memory(&memory, &mem_path).unwrap();
        assert_eq!(fs::metadata(&mem_path).unwrap().len(), 4096);

        // Create fresh memory and restore into it
        let memory2 = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 4096)]).unwrap();
        restore_memory(&memory2, &mem_path).unwrap();

        // Verify contents match
        let mut buf = vec![0u8; 4096];
        vm_memory::Bytes::read(&memory2, &mut buf, GuestAddress(0)).unwrap();
        assert_eq!(buf, pattern);
    }
}
