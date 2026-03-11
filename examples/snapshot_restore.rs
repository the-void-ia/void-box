//! Snapshot / Restore Demo (Developer API)
//!
//! **Note:** This is a developer API demo showing programmatic snapshot
//! workflows. For normal usage, prefer the CLI:
//! ```bash
//! voidbox snapshot create   # create a snapshot
//! voidbox snapshot list     # list existing snapshots
//! voidbox snapshot delete   # delete a snapshot
//! ```
//!
//! Shows the three snapshot workflows:
//! 1. **Base snapshot** — cold boot a VM, take a full snapshot, restore it
//! 2. **Diff snapshot** — enable dirty tracking, run work, capture only changed pages
//! 3. **Restore from diff** — layer base + diff for sub-millisecond restore
//!
//! ## Requirements
//!
//! - Linux with `/dev/kvm` accessible
//! - A guest initramfs with BusyBox (for `echo`):
//!   ```bash
//!   scripts/build_test_image.sh
//!   ```
//!
//! ## Run
//!
//! ```bash
//! VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
//! VOID_BOX_INITRAMFS=/tmp/void-box-test-rootfs.cpio.gz \
//! cargo run --example snapshot_restore
//! ```

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("snapshot_restore example requires Linux with /dev/kvm");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::path::PathBuf;
    use std::time::Instant;

    use void_box::vmm::config::{VoidBoxConfig, VsockBackendType};
    use void_box::vmm::snapshot::{self, SnapshotConfig, VmSnapshot};
    use void_box::vmm::MicroVm;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    // --- Resolve kernel/initramfs ---
    let kernel = PathBuf::from(
        std::env::var("VOID_BOX_KERNEL").expect("set VOID_BOX_KERNEL to your vmlinuz path"),
    );
    let initramfs = std::env::var("VOID_BOX_INITRAMFS").ok().map(PathBuf::from);

    let memory_mb: usize = std::env::var("VOID_BOX_MEMORY_MB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256);

    let enable_network = std::env::var("VOID_BOX_NETWORK")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    let snap_config = SnapshotConfig {
        memory_mb,
        vcpus: 1,
        cid: 0, // overwritten by snapshot_internal()
        vsock_mmio_base: 0xd080_0000,
        network: enable_network,
    };

    // ═══════════════════════════════════════════════════════════════
    // Step 1: Cold boot
    // ═══════════════════════════════════════════════════════════════
    println!("=== Step 1: Cold Boot ({} MB) ===", memory_mb);

    let mut cfg = VoidBoxConfig::new()
        .memory_mb(memory_mb)
        .vcpus(1)
        .kernel(&kernel)
        .enable_vsock(true)
        .vsock_backend(VsockBackendType::Userspace)
        .network(enable_network);
    if let Some(ref p) = initramfs {
        cfg = cfg.initramfs(p);
    }
    cfg.validate()?;

    let cold_start = Instant::now();
    let vm = MicroVm::new(cfg).await?;
    let cold_boot_time = cold_start.elapsed();

    let output = vm.exec("echo", &["hello from cold boot"]).await?;
    println!("  Cold boot: {:.1?}", cold_boot_time);
    println!("  Exec:      {}", output.stdout_str().trim());

    // ═══════════════════════════════════════════════════════════════
    // Step 2: Base snapshot (stops the VM)
    // ═══════════════════════════════════════════════════════════════
    println!("\n=== Step 2: Base Snapshot ===");

    let config_hash = snapshot::compute_config_hash(&kernel, initramfs.as_deref(), memory_mb, 1)?;

    // Use the standard snapshot directory so diff restore can find the base
    let base_dir = snapshot::snapshot_dir_for_hash(&config_hash);
    std::fs::create_dir_all(&base_dir)?;

    let snap_start = Instant::now();
    let base_path = vm
        .snapshot(&base_dir, config_hash.clone(), snap_config.clone())
        .await?;
    let snap_time = snap_start.elapsed();

    let base_mem_size = std::fs::metadata(VmSnapshot::memory_path(&base_path))?.len();
    println!("  Snapshot:  {:.1?}", snap_time);
    println!("  Memory:    {} MB", base_mem_size / (1024 * 1024));

    // ═══════════════════════════════════════════════════════════════
    // Step 3: Restore from base snapshot
    // ═══════════════════════════════════════════════════════════════
    println!("\n=== Step 3: Restore from Base ===");

    let restore_start = Instant::now();
    let restored = MicroVm::from_snapshot(&base_path).await?;
    let restore_time = restore_start.elapsed();

    // Enable dirty tracking immediately — all changes from here will be
    // captured in the diff snapshot.
    restored.enable_dirty_tracking()?;

    let output = restored.exec("echo", &["hello from restored VM"]).await?;
    println!("  Restore:   {:.1?}", restore_time);
    println!("  Exec:      {}", output.stdout_str().trim());

    // ═══════════════════════════════════════════════════════════════
    // Step 4: Diff snapshot (only dirty pages)
    // ═══════════════════════════════════════════════════════════════
    println!("\n=== Step 4: Diff Snapshot ===");

    let diff_dir = tempfile::tempdir()?;
    let diff_start = Instant::now();
    let diff_path = restored
        .snapshot_diff(
            diff_dir.path(),
            config_hash.clone(),
            snap_config.clone(),
            config_hash.clone(), // parent_id = base hash
        )
        .await?;
    let diff_time = diff_start.elapsed();

    let diff_mem_size = std::fs::metadata(VmSnapshot::diff_memory_path(&diff_path))?.len();
    let savings = (1.0 - diff_mem_size as f64 / base_mem_size as f64) * 100.0;
    println!("  Snapshot:  {:.1?}", diff_time);
    println!(
        "  Diff size: {} KB ({:.1}% savings)",
        diff_mem_size / 1024,
        savings
    );

    // ═══════════════════════════════════════════════════════════════
    // Step 5: Restore from diff snapshot
    // ═══════════════════════════════════════════════════════════════
    println!("\n=== Step 5: Restore from Diff ===");

    let diff_restore_start = Instant::now();
    let mut diff_restored = MicroVm::from_snapshot(&diff_path).await?;
    let diff_restore_time = diff_restore_start.elapsed();

    let output = diff_restored
        .exec("echo", &["hello from diff-restored VM"])
        .await?;
    println!("  Restore:   {:.1?}", diff_restore_time);
    println!("  Exec:      {}", output.stdout_str().trim());

    diff_restored.stop().await.ok();

    // ═══════════════════════════════════════════════════════════════
    // Summary
    // ═══════════════════════════════════════════════════════════════
    let speedup = cold_boot_time.as_secs_f64() / restore_time.as_secs_f64().max(1e-9);
    println!("\n╔═══════════════════════════════════╗");
    println!("║        Snapshot Summary            ║");
    println!("╠═══════════════════════════════════╣");
    println!("║  Cold boot:     {:>10.1?}       ║", cold_boot_time);
    println!("║  Base snapshot:  {:>10.1?}      ║", snap_time);
    println!("║  Base restore:   {:>10.1?}      ║", restore_time);
    println!("║  Diff snapshot:  {:>10.1?}      ║", diff_time);
    println!("║  Diff restore:   {:>10.1?}      ║", diff_restore_time);
    println!("║  Speedup:        {:>10.1}x      ║", speedup);
    println!("║  Memory savings: {:>10.1}%      ║", savings);
    println!("╚═══════════════════════════════════╝");

    // Print the snapshot hash for use in YAML specs
    println!("\nSnapshot hash: {}", config_hash);
    println!("Snapshot dir:  {}", base_dir.display());
    println!("\nTo use in a YAML spec:");
    println!("  sandbox:");
    println!("    snapshot: \"{}\"", &config_hash[..8]);

    Ok(())
}
