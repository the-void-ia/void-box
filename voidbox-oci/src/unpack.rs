use crate::error::{OciError, Result};
use crate::layer::LayerInfo;
use flate2::read::GzDecoder;
use std::fs;
use std::io::Read;
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
}
