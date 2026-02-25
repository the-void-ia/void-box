use crate::error::{OciError, Result};
use crate::layer::LayerInfo;
use flate2::read::GzDecoder;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use tar::Archive;
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Extract the given layers (bottom-up order) into `dest`, producing a merged
/// root filesystem.  Returns the path to the rootfs directory.
pub fn unpack_layers(layers: &[LayerInfo], dest: &Path) -> Result<PathBuf> {
    fs::create_dir_all(dest)?;

    for (i, layer) in layers.iter().enumerate() {
        info!(
            layer = i,
            digest = %layer.digest,
            media_type = %layer.media_type,
            "unpacking layer",
        );
        unpack_single_layer(layer, dest)?;
    }

    Ok(dest.to_path_buf())
}

/// Paths to the extracted guest files (kernel + initramfs).
#[derive(Clone, Debug)]
pub struct GuestFiles {
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
}

/// Selectively extract `vmlinuz` and `rootfs.cpio.gz` from OCI layer tarballs
/// into `dest`.  Much simpler than `unpack_layers()`: no whiteout handling, no
/// hard link deferral.  Stops as soon as both files are found.  Returns error
/// if either file is missing after scanning all layers.
pub fn extract_guest_files(layers: &[LayerInfo], dest: &Path) -> Result<GuestFiles> {
    fs::create_dir_all(dest)?;

    let kernel_path = dest.join("vmlinuz");
    let initramfs_path = dest.join("rootfs.cpio.gz");

    let mut found_kernel = false;
    let mut found_initramfs = false;

    for layer in layers {
        if found_kernel && found_initramfs {
            break;
        }

        let compressed = fs::read(&layer.local_path)?;
        let reader: Box<dyn Read> = decompressor(&layer.media_type, &compressed)?;
        let mut archive = Archive::new(reader);
        archive.set_preserve_permissions(false);

        for entry_result in archive.entries()? {
            let mut entry = entry_result?;
            let rel_path = entry.path()?.into_owned();

            let file_name = match rel_path.file_name() {
                Some(n) => n.to_string_lossy().to_string(),
                None => continue,
            };

            if file_name == "vmlinuz" && !found_kernel {
                entry.unpack(&kernel_path)?;
                found_kernel = true;
                info!(path = %kernel_path.display(), "extracted kernel");
            } else if file_name == "rootfs.cpio.gz" && !found_initramfs {
                entry.unpack(&initramfs_path)?;
                found_initramfs = true;
                info!(path = %initramfs_path.display(), "extracted initramfs");
            }

            if found_kernel && found_initramfs {
                break;
            }
        }
    }

    if !found_kernel {
        return Err(OciError::Layer("guest image missing vmlinuz".to_string()));
    }
    if !found_initramfs {
        return Err(OciError::Layer(
            "guest image missing rootfs.cpio.gz".to_string(),
        ));
    }

    Ok(GuestFiles {
        kernel: kernel_path,
        initramfs: initramfs_path,
    })
}

/// Gzip magic bytes (1f 8b).
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];

/// If the kernel is gzip-compressed and we're on macOS ARM64 (VZ backend),
/// decompress it to vmlinux. Apple's Virtualization.framework requires
/// uncompressed ARM64 kernels. Returns the path to use for the kernel.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub fn ensure_kernel_uncompressed_for_vz(guest: &GuestFiles) -> Result<GuestFiles> {
    let kernel_bytes =
        fs::read(&guest.kernel).map_err(|e| OciError::Layer(format!("read kernel: {}", e)))?;

    if kernel_bytes.len() < 2 || kernel_bytes[..2] != GZIP_MAGIC {
        return Ok(guest.clone());
    }

    info!(
        path = %guest.kernel.display(),
        "kernel is gzip-compressed; decompressing for VZ (required on macOS ARM64)",
    );

    let decompressed_path = guest.kernel.parent().unwrap().join("vmlinux");
    let mut decoder = GzDecoder::new(&kernel_bytes[..]);
    let mut decompressed = Vec::new();
    decoder
        .read_to_end(&mut decompressed)
        .map_err(|e| OciError::Layer(format!("decompress kernel: {}", e)))?;

    let mut out = fs::File::create(&decompressed_path)
        .map_err(|e| OciError::Layer(format!("create vmlinux: {}", e)))?;
    out.write_all(&decompressed)
        .map_err(|e| OciError::Layer(format!("write vmlinux: {}", e)))?;

    info!(path = %decompressed_path.display(), "decompressed kernel ready");

    Ok(GuestFiles {
        kernel: decompressed_path,
        initramfs: guest.initramfs.clone(),
    })
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
pub fn ensure_kernel_uncompressed_for_vz(guest: &GuestFiles) -> Result<GuestFiles> {
    Ok(guest.clone())
}

// ---------------------------------------------------------------------------
// Single layer extraction
// ---------------------------------------------------------------------------

fn unpack_single_layer(layer: &LayerInfo, dest: &Path) -> Result<()> {
    let compressed = fs::read(&layer.local_path)?;
    let reader: Box<dyn Read> = decompressor(&layer.media_type, &compressed)?;
    let mut archive = Archive::new(reader);
    // Do not preserve permissions bits that could block later access.
    archive.set_preserve_permissions(false);

    // Hard links whose target hasn't been extracted yet — retry after the
    // main pass.
    let mut deferred_hardlinks: Vec<(PathBuf, PathBuf)> = Vec::new();

    // We need to handle whiteouts ourselves, so iterate entries manually.
    for entry_result in archive.entries()? {
        let mut entry = entry_result?;
        let rel_path = entry.path()?.into_owned();

        let file_name = match rel_path.file_name() {
            Some(n) => n.to_string_lossy().to_string(),
            None => {
                // A root-level entry (e.g. "./") – just ensure the dir exists.
                let target = dest.join(&rel_path);
                if entry.header().entry_type().is_dir() {
                    fs::create_dir_all(&target)?;
                }
                continue;
            }
        };

        // --- Opaque whiteout: delete everything in the parent directory that
        //     was placed by *earlier* layers. ---
        if file_name == ".wh..wh..opq" {
            let parent = dest.join(rel_path.parent().unwrap_or_else(|| Path::new("")));
            if parent.exists() {
                clear_directory(&parent)?;
            }
            continue;
        }

        // --- Regular whiteout: delete the named entry. ---
        if let Some(hidden) = file_name.strip_prefix(".wh.") {
            let target = dest
                .join(rel_path.parent().unwrap_or_else(|| Path::new("")))
                .join(hidden);
            if target.exists() {
                if target.is_dir() {
                    fs::remove_dir_all(&target)?;
                } else {
                    fs::remove_file(&target)?;
                }
                debug!(path = %target.display(), "applied whiteout");
            }
            continue;
        }

        // --- Hard links: the target may not be extracted yet. ---
        if entry.header().entry_type() == tar::EntryType::Link {
            if let Ok(Some(link_name)) = entry.header().link_name() {
                let link_target = dest.join(link_name);
                let link_path = dest.join(&rel_path);
                if link_target.exists() {
                    if let Some(parent) = link_path.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    // Remove stale entry if present.
                    let _ = fs::remove_file(&link_path);
                    fs::hard_link(&link_target, &link_path)?;
                } else {
                    deferred_hardlinks.push((link_path, link_target));
                }
                continue;
            }
        }

        // --- Normal file / directory / symlink ---
        let target = dest.join(&rel_path);
        entry.unpack(&target)?;
    }

    // Retry deferred hard links now that all regular entries are on disk.
    for (link_path, link_target) in &deferred_hardlinks {
        if link_target.exists() {
            if let Some(parent) = link_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let _ = fs::remove_file(link_path);
            fs::hard_link(link_target, link_path)?;
            debug!(
                link = %link_path.display(),
                target = %link_target.display(),
                "created deferred hard link",
            );
        } else {
            warn!(
                link = %link_path.display(),
                target = %link_target.display(),
                "hard link target still missing after full pass; skipping",
            );
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Compression helpers
// ---------------------------------------------------------------------------

/// Return a `Read`er that decompresses `data` according to the OCI media type.
fn decompressor<'a>(media_type: &str, data: &'a [u8]) -> Result<Box<dyn Read + 'a>> {
    if media_type.contains("gzip") || media_type.contains("tar+gzip") {
        Ok(Box::new(GzDecoder::new(data)))
    } else if media_type.contains("zstd") {
        let decoder =
            zstd::Decoder::new(data).map_err(|e| OciError::Layer(format!("zstd init: {}", e)))?;
        Ok(Box::new(decoder))
    } else if media_type.contains("tar") && !media_type.contains('+') {
        // Uncompressed tar.
        Ok(Box::new(data))
    } else {
        // Default: try gzip.
        warn!(media_type, "unknown compression; assuming gzip");
        Ok(Box::new(GzDecoder::new(data)))
    }
}

/// Remove all entries inside `dir` but keep the directory itself.
fn clear_directory(dir: &Path) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            fs::remove_dir_all(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Whiteout detection helpers (also used from tests)
// ---------------------------------------------------------------------------

/// Returns `true` if `name` is an OCI whiteout marker (`.wh.<name>`).
pub fn is_whiteout(name: &str) -> bool {
    name.starts_with(".wh.") && name != ".wh..wh..opq"
}

/// Returns `true` if `name` is an opaque whiteout marker.
pub fn is_opaque_whiteout(name: &str) -> bool {
    name == ".wh..wh..opq"
}

/// Given a whiteout filename (`.wh.foo`), return the name it hides (`foo`).
pub fn whiteout_target(name: &str) -> Option<&str> {
    if is_whiteout(name) {
        name.strip_prefix(".wh.")
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_whiteout() {
        assert!(is_whiteout(".wh.some_file"));
        assert!(is_whiteout(".wh.etc"));
        // Opaque whiteout is NOT a regular whiteout.
        assert!(!is_whiteout(".wh..wh..opq"));
        assert!(!is_whiteout("normal_file"));
        assert!(!is_whiteout(".hidden"));
    }

    #[test]
    fn detect_opaque_whiteout() {
        assert!(is_opaque_whiteout(".wh..wh..opq"));
        assert!(!is_opaque_whiteout(".wh.foo"));
        assert!(!is_opaque_whiteout("file"));
    }

    #[test]
    fn whiteout_target_extraction() {
        assert_eq!(whiteout_target(".wh.foo"), Some("foo"));
        assert_eq!(whiteout_target(".wh.bar.txt"), Some("bar.txt"));
        assert_eq!(whiteout_target("normal"), None);
        assert_eq!(whiteout_target(".wh..wh..opq"), None);
    }

    #[test]
    fn decompressor_gzip_media_type() {
        // Simply verify the function returns Ok for a gzip media type.
        // (An actual gzip stream would be needed for real decompression.)
        let mt = "application/vnd.oci.image.layer.v1.tar+gzip";
        assert!(decompressor(mt, &[]).is_ok());
    }

    #[test]
    fn decompressor_uncompressed_tar() {
        let mt = "application/vnd.oci.image.layer.v1.tar";
        assert!(decompressor(mt, &[]).is_ok());
    }

    #[test]
    fn clear_directory_works() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        // Create some files and subdirs.
        fs::write(dir.join("a.txt"), "aaa").unwrap();
        fs::create_dir(dir.join("sub")).unwrap();
        fs::write(dir.join("sub").join("b.txt"), "bbb").unwrap();

        clear_directory(dir).unwrap();

        // Directory itself still exists but is empty.
        assert!(dir.exists());
        assert_eq!(fs::read_dir(dir).unwrap().count(), 0);
    }

    /// Build a gzip-compressed tar containing the given file entries.
    /// Returns the compressed bytes.
    fn build_tar_gz(entries: &[(&str, &[u8])]) -> Vec<u8> {
        use flate2::write::GzEncoder;
        use flate2::Compression;

        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        {
            let mut builder = tar::Builder::new(&mut encoder);
            for &(name, data) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_path(name).unwrap();
                header.set_size(data.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder.append(&header, data).unwrap();
            }
            builder.finish().unwrap();
        }
        encoder.finish().unwrap()
    }

    /// Helper: create a LayerInfo pointing at a temp file with given bytes.
    fn layer_from_bytes(dir: &Path, name: &str, data: &[u8]) -> LayerInfo {
        let path = dir.join(name);
        fs::write(&path, data).unwrap();
        LayerInfo {
            digest: format!("sha256:{}", name),
            size: data.len() as u64,
            media_type: "application/vnd.oci.image.layer.v1.tar+gzip".to_string(),
            local_path: path,
        }
    }

    #[test]
    fn extract_guest_files_both_found() {
        let tmp = tempfile::tempdir().unwrap();
        let layer_dir = tmp.path().join("blobs");
        fs::create_dir_all(&layer_dir).unwrap();

        let tar_data = build_tar_gz(&[
            ("vmlinuz", b"KERNEL_DATA"),
            ("rootfs.cpio.gz", b"INITRAMFS_DATA"),
            ("extra-file.txt", b"ignored"),
        ]);
        let layer = layer_from_bytes(&layer_dir, "layer0.tar.gz", &tar_data);

        let dest = tmp.path().join("guest");
        let result = extract_guest_files(&[layer], &dest).unwrap();

        assert_eq!(fs::read(&result.kernel).unwrap(), b"KERNEL_DATA");
        assert_eq!(fs::read(&result.initramfs).unwrap(), b"INITRAMFS_DATA");
    }

    #[test]
    fn extract_guest_files_missing_kernel() {
        let tmp = tempfile::tempdir().unwrap();
        let layer_dir = tmp.path().join("blobs");
        fs::create_dir_all(&layer_dir).unwrap();

        let tar_data = build_tar_gz(&[("rootfs.cpio.gz", b"INITRAMFS_DATA")]);
        let layer = layer_from_bytes(&layer_dir, "layer0.tar.gz", &tar_data);

        let dest = tmp.path().join("guest");
        let err = extract_guest_files(&[layer], &dest).unwrap_err();
        assert!(
            err.to_string().contains("vmlinuz"),
            "error should mention vmlinuz: {err}"
        );
    }

    #[test]
    fn extract_guest_files_missing_initramfs() {
        let tmp = tempfile::tempdir().unwrap();
        let layer_dir = tmp.path().join("blobs");
        fs::create_dir_all(&layer_dir).unwrap();

        let tar_data = build_tar_gz(&[("vmlinuz", b"KERNEL_DATA")]);
        let layer = layer_from_bytes(&layer_dir, "layer0.tar.gz", &tar_data);

        let dest = tmp.path().join("guest");
        let err = extract_guest_files(&[layer], &dest).unwrap_err();
        assert!(
            err.to_string().contains("rootfs.cpio.gz"),
            "error should mention rootfs.cpio.gz: {err}"
        );
    }

    #[test]
    fn extract_guest_files_across_layers() {
        let tmp = tempfile::tempdir().unwrap();
        let layer_dir = tmp.path().join("blobs");
        fs::create_dir_all(&layer_dir).unwrap();

        // Kernel in layer 0, initramfs in layer 1.
        let tar0 = build_tar_gz(&[("vmlinuz", b"K")]);
        let tar1 = build_tar_gz(&[("rootfs.cpio.gz", b"I")]);
        let l0 = layer_from_bytes(&layer_dir, "l0.tar.gz", &tar0);
        let l1 = layer_from_bytes(&layer_dir, "l1.tar.gz", &tar1);

        let dest = tmp.path().join("guest");
        let result = extract_guest_files(&[l0, l1], &dest).unwrap();

        assert_eq!(fs::read(&result.kernel).unwrap(), b"K");
        assert_eq!(fs::read(&result.initramfs).unwrap(), b"I");
    }

    #[test]
    fn extract_guest_files_nested_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let layer_dir = tmp.path().join("blobs");
        fs::create_dir_all(&layer_dir).unwrap();

        // Files at nested paths — should still match by filename.
        let tar_data = build_tar_gz(&[
            ("boot/vmlinuz", b"NESTED_KERNEL"),
            ("images/rootfs.cpio.gz", b"NESTED_INITRAMFS"),
        ]);
        let layer = layer_from_bytes(&layer_dir, "layer0.tar.gz", &tar_data);

        let dest = tmp.path().join("guest");
        let result = extract_guest_files(&[layer], &dest).unwrap();

        assert_eq!(fs::read(&result.kernel).unwrap(), b"NESTED_KERNEL");
        assert_eq!(fs::read(&result.initramfs).unwrap(), b"NESTED_INITRAMFS");
    }
}
