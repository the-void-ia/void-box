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

async fn cmd_pull(cache_root: &Path, flavor: &str) -> Result<(), Box<dyn std::error::Error>> {
    let arch = void_box::image::detect_arch()?;

    if flavor == "all" {
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
        "{:<10} {:<10} {:<10} {:<10} Path",
        "Version", "Flavor", "Arch", "Size"
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
