use std::fs;
use std::path::{Path, PathBuf};

use clap::Subcommand;

use crate::output::OutputFormat;

#[derive(Debug, Subcommand)]
pub enum SnapshotCommand {
    /// Create a new snapshot from a cold-booted VM.
    Create {
        /// Path to the kernel image.
        #[arg(long)]
        kernel: PathBuf,
        /// Path to the initramfs image.
        #[arg(long)]
        initramfs: Option<PathBuf>,
        /// Memory size in MB.
        #[arg(long, default_value = "512")]
        memory: usize,
        /// Number of vCPUs.
        #[arg(long, default_value = "1")]
        vcpus: usize,
        /// Enable guest networking in the snapshotted VM. Must match the
        /// `network` value used at restore time — Apple's VZ rejects any
        /// device-set drift between save and restore.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        network: bool,
        /// Create a differential snapshot on top of an existing base.
        #[arg(long)]
        diff: bool,
    },
    /// List stored snapshots.
    List,
    /// Delete a snapshot by hash prefix.
    Delete {
        /// Hash prefix of the snapshot to delete.
        hash_prefix: String,
    },
}

pub async fn handle(
    cmd: SnapshotCommand,
    output: OutputFormat,
    snapshot_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        SnapshotCommand::Create {
            kernel,
            initramfs,
            memory,
            vcpus,
            network,
            diff,
        } => cmd_snapshot_create(&kernel, initramfs.as_deref(), memory, vcpus, network, diff).await,
        SnapshotCommand::List => cmd_snapshot_list(output, snapshot_dir),
        SnapshotCommand::Delete { hash_prefix } => cmd_snapshot_delete(&hash_prefix, snapshot_dir),
    }
}

async fn cmd_snapshot_create(
    kernel: &Path,
    initramfs: Option<&Path>,
    memory_mb: usize,
    vcpus: usize,
    network: bool,
    is_diff: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use void_box::snapshot_store;

    let config_hash = snapshot_store::compute_config_hash(kernel, initramfs, memory_mb, vcpus)?;
    eprintln!(
        "Creating {} snapshot: kernel={}, initramfs={:?}, memory={}MB, vcpus={}, network={}",
        if is_diff { "diff" } else { "base" },
        kernel.display(),
        initramfs.map(|p| p.display()),
        memory_mb,
        vcpus,
        network,
    );
    eprintln!("Config hash: {}", &config_hash[..16]);

    #[cfg(target_os = "linux")]
    {
        cmd_snapshot_create_linux(
            kernel,
            initramfs,
            memory_mb,
            vcpus,
            network,
            is_diff,
            &config_hash,
        )
        .await
    }

    #[cfg(target_os = "macos")]
    {
        cmd_snapshot_create_macos(
            kernel,
            initramfs,
            memory_mb,
            vcpus,
            network,
            is_diff,
            &config_hash,
        )
        .await
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (
            kernel,
            initramfs,
            memory_mb,
            vcpus,
            network,
            is_diff,
            config_hash,
        );
        Err("snapshot create is not supported on this platform".into())
    }
}

#[cfg(target_os = "linux")]
async fn cmd_snapshot_create_linux(
    kernel: &Path,
    initramfs: Option<&Path>,
    memory_mb: usize,
    vcpus: usize,
    network: bool,
    is_diff: bool,
    config_hash: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    use void_box::snapshot_store;
    use void_box::vmm::config::VoidBoxConfig;
    use void_box::vmm::snapshot;
    use void_box::MicroVm;

    if is_diff {
        let base_dir = snapshot_store::snapshot_dir_for_hash(config_hash);
        if !base_dir.join("state.bin").exists() {
            return Err(format!(
                "no base snapshot found; create one first with: voidbox snapshot create --kernel {} {}--memory {} --vcpus {}",
                kernel.display(),
                initramfs
                    .map(|p| format!("--initramfs {} ", p.display()))
                    .unwrap_or_default(),
                memory_mb,
                vcpus
            )
            .into());
        }

        let diff_dir_name = format!("{}-diff", &config_hash[..16]);
        let diff_dir = snapshot_store::default_snapshot_dir().join(&diff_dir_name);
        fs::create_dir_all(&diff_dir)?;

        if diff_dir.join("state.bin").exists() {
            return Err(format!(
                "diff snapshot already exists at {}; delete it first with: voidbox snapshot delete {}-diff",
                diff_dir.display(),
                &config_hash[..16]
            )
            .into());
        }

        let start = std::time::Instant::now();
        eprintln!("Restoring VM from base snapshot...");
        let vm = MicroVm::from_snapshot(&base_dir).await?;
        let restore_ms = start.elapsed().as_millis();
        eprintln!("VM restored in {}ms", restore_ms);

        eprintln!("Enabling dirty page tracking...");
        vm.enable_dirty_tracking()?;

        eprintln!("Waiting for guest-agent readiness...");
        let output = vm.exec("echo", &["snapshot-ready"]).await?;
        if !output.success() {
            return Err(format!("Guest-agent not ready: {}", output.stderr_str()).into());
        }
        eprintln!(
            "Guest-agent ready ({}ms total)",
            start.elapsed().as_millis()
        );

        let snap_config = snapshot::SnapshotConfig {
            memory_mb,
            vcpus,
            cid: vm.cid(),
            vsock_mmio_base: 0xd080_0000,
            network,
        };

        let snap_dir = vm
            .snapshot_diff(
                &diff_dir,
                config_hash.to_string(),
                snap_config,
                config_hash.to_string(),
            )
            .await?;
        let total_ms = start.elapsed().as_millis();

        let diff_mem_size = fs::metadata(snapshot::VmSnapshot::diff_memory_path(&snap_dir))
            .map(|m| m.len())
            .unwrap_or(0);
        let base_mem_size = fs::metadata(snapshot::VmSnapshot::memory_path(&base_dir))
            .map(|m| m.len())
            .unwrap_or(1);

        let savings = if base_mem_size > 0 {
            100.0 - (diff_mem_size as f64 / base_mem_size as f64 * 100.0)
        } else {
            0.0
        };

        eprintln!("Diff snapshot created successfully:");
        eprintln!("  Hash:      {}", &config_hash[..16]);
        eprintln!("  Path:      {}", snap_dir.display());
        eprintln!("  Duration:  {}ms", total_ms);
        eprintln!(
            "  Diff mem:  {} KB ({:.1}% savings vs base)",
            diff_mem_size / 1024,
            savings
        );
    } else {
        let snapshot_dir = snapshot_store::snapshot_dir_for_hash(config_hash);
        fs::create_dir_all(&snapshot_dir)?;

        if snapshot_dir.join("state.bin").exists() {
            return Err(format!(
                "snapshot already exists at {}; delete it first with: voidbox snapshot delete {}",
                snapshot_dir.display(),
                &config_hash[..16]
            )
            .into());
        }

        let mut config = VoidBoxConfig::new()
            .kernel(kernel)
            .memory_mb(memory_mb)
            .vcpus(vcpus)
            .network(network)
            .enable_vsock(true)
            .vsock_backend(void_box::vmm::config::VsockBackendType::Userspace);
        if let Some(initramfs) = initramfs {
            config = config.initramfs(initramfs);
        }

        let start = std::time::Instant::now();
        eprintln!("Booting VM...");
        let vm = MicroVm::new(config.clone()).await?;
        let boot_ms = start.elapsed().as_millis();
        eprintln!("VM booted in {}ms, waiting for guest-agent...", boot_ms);

        let output = vm.exec("echo", &["snapshot-ready"]).await?;
        if !output.success() {
            return Err(format!("Guest-agent not ready: {}", output.stderr_str()).into());
        }
        eprintln!(
            "Guest-agent ready ({}ms total)",
            start.elapsed().as_millis()
        );

        let snap_config = snapshot::SnapshotConfig {
            memory_mb,
            vcpus,
            cid: vm.cid(),
            vsock_mmio_base: 0xd080_0000,
            network: config.network,
        };

        let snap_dir = vm
            .snapshot(&snapshot_dir, config_hash.to_string(), snap_config)
            .await?;
        let total_ms = start.elapsed().as_millis();

        eprintln!("Snapshot created successfully:");
        eprintln!("  Hash:     {}", &config_hash[..16]);
        eprintln!("  Path:     {}", snap_dir.display());
        eprintln!("  Duration: {}ms", total_ms);

        let mem_size = fs::metadata(snapshot::VmSnapshot::memory_path(&snap_dir))
            .map(|m| m.len())
            .unwrap_or(0);
        eprintln!("  Memory:   {} MB", mem_size / (1024 * 1024));
    }

    Ok(())
}

#[cfg(target_os = "macos")]
async fn cmd_snapshot_create_macos(
    kernel: &Path,
    initramfs: Option<&Path>,
    memory_mb: usize,
    vcpus: usize,
    network: bool,
    is_diff: bool,
    config_hash: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    use void_box::backend::vz::VzBackend;
    use void_box::backend::{BackendConfig, VmmBackend};
    use void_box::snapshot_store;

    if is_diff {
        return Err("diff snapshots are not supported on macOS (VZ)".into());
    }

    let snapshot_dir = snapshot_store::snapshot_dir_for_hash(config_hash);
    fs::create_dir_all(&snapshot_dir)?;

    if snapshot_store::snapshot_exists(&snapshot_dir) {
        return Err(format!(
            "snapshot already exists at {}; delete it first with: voidbox snapshot delete {}",
            snapshot_dir.display(),
            &config_hash[..16]
        )
        .into());
    }

    let mut config = BackendConfig::minimal(kernel, memory_mb, vcpus).network(network);
    if let Some(initramfs) = initramfs {
        config = config.initramfs(initramfs);
    }

    let start = std::time::Instant::now();
    eprintln!("Booting VM via Virtualization.framework...");
    let mut backend = VzBackend::new();
    backend.start(config).await?;
    let boot_ms = start.elapsed().as_millis();
    eprintln!("VM booted in {}ms, waiting for guest-agent...", boot_ms);

    let output = backend
        .exec("echo", &["snapshot-ready"], &[], &[], None, None)
        .await?;
    if !output.success() {
        return Err(format!("Guest-agent not ready: {}", output.stderr_str()).into());
    }
    eprintln!(
        "Guest-agent ready ({}ms total)",
        start.elapsed().as_millis()
    );

    eprintln!("Creating snapshot...");
    // create_snapshot uses blocking GCD dispatch + recv_timeout internally;
    // block_in_place tells tokio to compensate so we don't stall the runtime.
    tokio::task::block_in_place(|| backend.create_snapshot(&snapshot_dir))?;
    let total_ms = start.elapsed().as_millis();

    eprintln!("Snapshot created successfully:");
    eprintln!("  Hash:     {}", &config_hash[..16]);
    eprintln!("  Path:     {}", snapshot_dir.display());
    eprintln!("  Duration: {}ms", total_ms);

    backend.stop().await?;
    Ok(())
}

fn cmd_snapshot_list(
    output: OutputFormat,
    snapshot_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    use void_box::snapshot_store;

    #[derive(serde::Serialize)]
    struct SnapshotRow {
        hash: String,
        memory_mb: usize,
        vcpus: usize,
        snapshot_type: String,
        path: String,
    }

    let snapshots = snapshot_store::list_snapshots_in(snapshot_dir)?;
    let rows: Vec<SnapshotRow> = snapshots
        .iter()
        .map(|info| SnapshotRow {
            hash: info.config_hash[..16.min(info.config_hash.len())].to_string(),
            memory_mb: info.memory_mb,
            vcpus: info.vcpus,
            snapshot_type: info.snapshot_type.to_string(),
            path: info.dir.display().to_string(),
        })
        .collect();

    crate::output::print_json_or_human(output, &rows, |rows| {
        if rows.is_empty() {
            println!("No snapshots found.");
            return;
        }
        println!(
            "{:<18} {:<8} {:<8} {:<10} PATH",
            "HASH", "MEM(MB)", "VCPUS", "TYPE"
        );
        for row in rows {
            println!(
                "{:<18} {:<8} {:<8} {:<10} {}",
                row.hash, row.memory_mb, row.vcpus, row.snapshot_type, row.path,
            );
        }
    });
    Ok(())
}

fn cmd_snapshot_delete(
    hash_prefix: &str,
    snapshot_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    use void_box::snapshot_store;

    if snapshot_store::delete_snapshot_in(snapshot_dir, hash_prefix)? {
        println!("Deleted snapshot matching '{}'", hash_prefix);
        Ok(())
    } else {
        Err(format!("no snapshot found matching '{hash_prefix}'").into())
    }
}
