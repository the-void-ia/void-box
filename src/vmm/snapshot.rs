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

use crate::vmm::arch;
use crate::{Error, Result};

/// Snapshot format version for forward compatibility.
///
/// Bumped to 4 when `bincode` (unmaintained, RUSTSEC-2025-0141) was swapped
/// for `postcard`. The new wire format — varint-encoded integers, different
/// option/enum encoding — is not compatible with pre-v4 snapshots. Old
/// `state.bin` files fail to decode before the version check ever runs;
/// delete `~/.void-box/snapshots/` to recover.
pub const SNAPSHOT_VERSION: u32 = 4;

// Re-export cross-platform snapshot utilities from `snapshot_store`.
pub use crate::snapshot_store::{
    compute_config_hash, default_snapshot_dir, delete_snapshot, list_snapshots,
    snapshot_dir_for_hash, snapshot_exists, SnapshotInfo, SnapshotType,
};

fn dirs_snapshot_base() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".void-box").join("snapshots")
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

// SnapshotType re-exported from snapshot_store above.

/// Hardware configuration captured at snapshot time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotConfig {
    pub memory_mb: usize,
    pub vcpus: usize,
    pub cid: u32,
    pub vsock_mmio_base: u64,
    pub network: bool,
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
    #[serde(default)]
    pub last_avail_idx: Option<u16>,
    /// Userspace backend only: last produced used-ring index.
    #[serde(default)]
    pub last_used_idx: Option<u16>,
}

/// Serializable virtio-net MMIO device state (excludes SLIRP — TCP connections don't survive).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetSnapshotState {
    pub device_features: u64,
    pub driver_features: u64,
    pub features_sel: u32,
    pub queue_sel: u32,
    pub status: u32,
    pub interrupt_status: u32,
    pub config_generation: u32,
    pub mac: [u8; 6],
    /// rx(0), tx(1) queue state.
    pub queues: Vec<QueueSnapshotState>,
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
    /// Per-vCPU register state (arch-specific).
    pub vcpu_states: Vec<arch::VcpuState>,
    /// In-kernel interrupt controller state (arch-specific).
    pub irqchip: arch::IrqchipState,
    /// Arch-specific VM state (PIT + KVM clock on x86, empty on aarch64).
    pub arch_state: arch::ArchVmState,
    /// Virtio-vsock device state.
    pub vsock_state: VsockSnapshotState,
    /// Hardware config at snapshot time.
    pub config: SnapshotConfig,
    /// `sha256(kernel + initramfs + memory_mb + vcpus)`.
    pub config_hash: String,
    /// Base | Diff.
    pub snapshot_type: SnapshotType,
    /// Session secret that the guest-agent expects (from kernel cmdline).
    /// Stored as raw bytes because the snapshot is serialized to disk via
    /// postcard; in-memory holders use `void_box_protocol::SessionSecret`.
    pub session_secret: Vec<u8>,
    /// Virtio-net device state (None if networking was disabled).
    #[serde(default)]
    pub net_state: Option<NetSnapshotState>,
}

impl VmSnapshot {
    /// Persist snapshot state to `<dir>/state.bin`.
    ///
    /// Memory is saved separately via [`dump_memory`].
    pub fn save(&self, dir: &Path) -> Result<()> {
        fs::create_dir_all(dir).map_err(|e| {
            Error::Snapshot(format!("create snapshot dir {}: {}", dir.display(), e))
        })?;
        let state_bytes = postcard::to_allocvec(self)
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
        let snapshot: VmSnapshot = postcard::from_bytes(&state_bytes)
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffMemoryHeader {
    /// Total guest memory size in bytes (must match base).
    pub total_memory_size: u64,
    /// Sorted list of dirty page indices (0-based, each page = 4096 bytes).
    pub dirty_page_indices: Vec<u64>,
}

/// Dump only dirty guest memory pages to a diff file.
pub fn dump_memory_diff(
    memory: &GuestMemoryMmap,
    dirty_bitmaps: &[(u32, Vec<u64>)],
    path: &Path,
) -> Result<()> {
    let total_memory_size: u64 = memory.iter().map(|r| r.len()).sum();
    let mut dirty_page_indices: Vec<u64> = Vec::new();

    let mut page_offset: u64 = 0;
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
        let pages_in_slot = bitmap.len() as u64 * 64;
        page_offset += pages_in_slot;
    }

    dirty_page_indices.sort_unstable();

    let header = DiffMemoryHeader {
        total_memory_size,
        dirty_page_indices: dirty_page_indices.clone(),
    };

    let mut file = fs::File::create(path)?;
    let header_bytes = postcard::to_allocvec(&header)
        .map_err(|e| Error::Snapshot(format!("serialize diff header: {}", e)))?;
    file.write_all(&(header_bytes.len() as u64).to_le_bytes())?;
    file.write_all(&header_bytes)?;

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

/// Restore guest memory from a diff file.
pub fn restore_memory_diff(memory: &GuestMemoryMmap, path: &Path) -> Result<()> {
    let mut file = fs::File::open(path)
        .map_err(|e| Error::Snapshot(format!("open diff memory file {}: {}", path.display(), e)))?;

    let header = read_diff_header(&mut file)?;

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
pub fn merge_snapshots(
    base_mem_path: &Path,
    diff_mem_path: &Path,
    output_path: &Path,
) -> Result<()> {
    let mut diff_file = fs::File::open(diff_mem_path).map_err(|e| {
        Error::Snapshot(format!("open diff file {}: {}", diff_mem_path.display(), e))
    })?;
    let header = read_diff_header(&mut diff_file)?;

    fs::copy(base_mem_path, output_path).map_err(|e| {
        Error::Snapshot(format!(
            "copy base {} to {}: {}",
            base_mem_path.display(),
            output_path.display(),
            e
        ))
    })?;

    let mut output = fs::OpenOptions::new()
        .write(true)
        .open(output_path)
        .map_err(|e| Error::Snapshot(format!("open output {}: {}", output_path.display(), e)))?;

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

    postcard::from_bytes(&header_bytes)
        .map_err(|e| Error::Snapshot(format!("deserialize diff header: {}", e)))
}

// ---------------------------------------------------------------------------
// Snapshot cache management
// ---------------------------------------------------------------------------

/// Compute a cache key for a diff/layered snapshot.
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

    entries.sort_by_key(|e| e.modified);

    Ok(CacheStats {
        total_snapshots: entries.len(),
        total_size_bytes: total_size,
        entries,
    })
}

/// Evict snapshots using LRU until total cache size is under `max_bytes`.
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

// Config hash, snapshot_dir_for_hash, SnapshotInfo, list_snapshots,
// delete_snapshot are all re-exported from snapshot_store at the top.

// ---------------------------------------------------------------------------
// Raw byte helpers for KVM struct serialization
// ---------------------------------------------------------------------------

/// Convert a fixed-size `repr(C)` struct to a byte vector.
pub fn kvm_struct_to_bytes<T: Sized>(val: &T) -> Vec<u8> {
    unsafe {
        std::slice::from_raw_parts(val as *const T as *const u8, std::mem::size_of::<T>()).to_vec()
    }
}

/// Restore a fixed-size `repr(C)` struct from a byte slice.
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
            let bytes = postcard::to_allocvec(&t).unwrap();
            let restored: SnapshotType = postcard::from_bytes(&bytes).unwrap();
            assert_eq!(t, restored);
        }
    }

    #[test]
    fn test_vcpu_state_serde_roundtrip() {
        #[cfg(target_arch = "x86_64")]
        let state = arch::VcpuState {
            regs: vec![1, 2, 3, 4],
            sregs: vec![5, 6, 7, 8],
            lapic: vec![9, 10],
            xsave: vec![11, 12, 13],
            msrs: vec![(0x10, 1000), (0xC0000080, 2000)],
            vcpu_events: vec![],
            xcrs: vec![],
            mp_state: Some(0),
        };
        #[cfg(target_arch = "aarch64")]
        let state = arch::VcpuState {
            core_regs: vec![(0, 1), (1, 2)],
            system_regs: vec![(0x1000, 42)],
            fp_regs: vec![(0x2000, vec![0u8; 16])],
            timer_regs: vec![(0x3000, 100)],
            mp_state: Some(0),
        };
        let bytes = postcard::to_allocvec(&state).unwrap();
        let restored: arch::VcpuState = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(state.mp_state, restored.mp_state);
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
        #[cfg(target_arch = "x86_64")]
        let vcpu_state = arch::VcpuState {
            regs: vec![0; 16],
            sregs: vec![0; 32],
            lapic: vec![0; 8],
            xsave: vec![0; 64],
            msrs: vec![(0x10, 100)],
            vcpu_events: vec![],
            xcrs: vec![],
            mp_state: Some(0),
        };
        #[cfg(target_arch = "aarch64")]
        let vcpu_state = arch::VcpuState {
            core_regs: vec![(0, 0); 34],
            system_regs: vec![(0x1000, 0)],
            fp_regs: vec![(0x2000, vec![0u8; 16])],
            timer_regs: vec![(0x3000, 0)],
            mp_state: Some(0),
        };

        #[cfg(target_arch = "x86_64")]
        let irqchip = arch::IrqchipState {
            pic_master: vec![0; 64],
            pic_slave: vec![0; 64],
            ioapic: vec![0; 128],
        };
        #[cfg(target_arch = "aarch64")]
        let irqchip = arch::IrqchipState {
            gic_dist_regs: vec![],
            gic_redist_regs: vec![],
            gic_cpu_regs: vec![],
        };

        #[cfg(target_arch = "x86_64")]
        let arch_state = arch::ArchVmState {
            pit: vec![0; 32],
            clock: vec![],
        };
        #[cfg(target_arch = "aarch64")]
        let arch_state = arch::ArchVmState {};

        let snap = VmSnapshot {
            version: SNAPSHOT_VERSION,
            parent_id: None,
            vcpu_states: vec![vcpu_state],
            irqchip,
            arch_state,
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
            net_state: None,
        };
        let bytes = postcard::to_allocvec(&snap).unwrap();
        let restored: VmSnapshot = postcard::from_bytes(&bytes).unwrap();
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
        let bytes = postcard::to_allocvec(&header).unwrap();
        let restored: DiffMemoryHeader = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(restored.total_memory_size, header.total_memory_size);
        assert_eq!(restored.dirty_page_indices, header.dirty_page_indices);
    }

    #[test]
    fn test_diff_memory_dump_and_restore() {
        use vm_memory::GuestAddress;

        let page_count = 4;
        let mem_size = page_count * PAGE_SIZE;
        let memory = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), mem_size)]).unwrap();

        for i in 0..page_count {
            let pattern = vec![(i as u8).wrapping_mul(37); PAGE_SIZE];
            vm_memory::Bytes::write(&memory, &pattern, GuestAddress((i * PAGE_SIZE) as u64))
                .unwrap();
        }

        let bitmap_word = (1u64 << 1) | (1u64 << 3);
        let dirty_bitmaps = vec![(0u32, vec![bitmap_word])];

        let dir = tempfile::tempdir().unwrap();
        let diff_path = dir.path().join("test.diff");

        dump_memory_diff(&memory, &dirty_bitmaps, &diff_path).unwrap();

        let memory2 = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), mem_size)]).unwrap();

        restore_memory_diff(&memory2, &diff_path).unwrap();

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

        let page_count = 4;
        let mem_size = page_count * PAGE_SIZE;
        let base_data: Vec<u8> = (0..page_count)
            .flat_map(|i| vec![(i as u8) + 0xA0; PAGE_SIZE])
            .collect();
        fs::write(&base_path, &base_data).unwrap();

        let memory =
            GuestMemoryMmap::from_ranges(&[(vm_memory::GuestAddress(0), mem_size)]).unwrap();

        vm_memory::Bytes::write(&memory, &base_data, vm_memory::GuestAddress(0)).unwrap();

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

        merge_snapshots(&base_path, &diff_path, &merged_path).unwrap();

        let merged = fs::read(&merged_path).unwrap();
        assert_eq!(merged.len(), mem_size);

        assert!(
            merged[..PAGE_SIZE].iter().all(|&b| b == 0xFF),
            "page 0 should be 0xFF"
        );
        assert!(
            merged[PAGE_SIZE..2 * PAGE_SIZE].iter().all(|&b| b == 0xA1),
            "page 1 should be 0xA1"
        );
        assert!(
            merged[2 * PAGE_SIZE..3 * PAGE_SIZE]
                .iter()
                .all(|&b| b == 0xBB),
            "page 2 should be 0xBB"
        );
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

        let pattern: Vec<u8> = (0..4096u16).map(|i| (i % 256) as u8).collect();
        vm_memory::Bytes::write(&memory, &pattern, GuestAddress(0)).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let mem_path = dir.path().join("test.mem");

        dump_memory(&memory, &mem_path).unwrap();
        assert_eq!(fs::metadata(&mem_path).unwrap().len(), 4096);

        let memory2 = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 4096)]).unwrap();
        restore_memory(&memory2, &mem_path).unwrap();

        let mut buf = vec![0u8; 4096];
        vm_memory::Bytes::read(&memory2, &mut buf, GuestAddress(0)).unwrap();
        assert_eq!(buf, pattern);
    }
}
