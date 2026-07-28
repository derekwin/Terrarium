//! Layer name resolution, EROFS build, list, and remove.
//!
//! Layer names map to `<layer_dir>/<name>` (directory) or
//! `<layer_dir>/<name>.erofs` (image, auto-mounted).

use crate::erofs;

use adapter_traits::AdapterError;
use std::collections::HashSet;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Configuration for layer resolution (subset of CH adapter's FsConfig).
pub struct LayerConfig {
    /// Root directory where all layers are stored.
    pub layer_dir: String,
    /// Root directory for filesystem composition (mount points live here).
    pub fs_root: String,
    /// EROFS layer images already mounted (shared; immutable layers live
    /// for the daemon's lifetime).
    pub mounted_layers: Arc<Mutex<HashSet<String>>>,
}

/// Names that are system bases, not add-on layers — hidden from list.
const SYSTEM_LAYER_NAMES: &[&str] = &["base", "ubuntu", ".system"];

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Resolve a layer name to a usable lowerdir path.
///
/// Resolution order: `<layer_dir>/<name>` directory first, then
/// `<layer_dir>/<name>.erofs` image (mounted on first use). EROFS
/// mounts are shared and kept for the daemon's lifetime.
pub fn resolve_layer(config: &LayerConfig, name: &str) -> Result<String, AdapterError> {
    let dir = format!("{}/{}", config.layer_dir, name);
    if std::path::Path::new(&dir).is_dir() {
        return Ok(dir);
    }
    let image = format!("{}/{}.erofs", config.layer_dir, name);
    if !std::path::Path::new(&image).exists() {
        return Err(AdapterError::not_found(format!(
            "layer '{}' not found under {} (neither directory nor .erofs image)",
            name, config.layer_dir
        )));
    }
    let mnt = format!("{}/layers-mnt/{}", config.fs_root, name);
    let mtime = std::fs::metadata(&image)
        .and_then(|m| m.modified())
        .map(|t| format!("{:?}", t))
        .unwrap_or_default();
    let sidecar = format!("{}.mtime", mnt);
    let mounted_as = std::fs::read_to_string(&sidecar).unwrap_or_default();
    if erofs::is_mounted(&mnt) {
        if mounted_as == mtime {
            return Ok(mnt);
        }
        tracing::warn!(%mnt, "layer image rebuilt, remounting");
        let _ = Command::new("umount").arg(&mnt).output();
        let _ = Command::new("fusermount").args(["-u", &mnt]).output();
    }
    let mut set = config
        .mounted_layers
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if set.contains(name) {
        return Ok(mnt);
    }
    std::fs::create_dir_all(&mnt).map_err(|e| format!("mkdir {}: {}", mnt, e))?;
    erofs::mount_erofs(&image, &mnt)?;
    let _ = std::fs::write(&sidecar, &mtime);
    set.insert(name.to_string());
    Ok(mnt)
}

/// Build an EROFS layer image from a source directory.
///
/// Runs `mkfs.erofs -zlz4 <tmp> <src_dir>/` then moves the result to
/// `<output_dir>/<name>.erofs`.
pub fn build_erofs_layer(src_dir: &str, name: &str, output_dir: &str) -> Result<String, String> {
    let src = Path::new(src_dir);
    if !src.is_dir() {
        return Err(format!("source directory not found: {}", src_dir));
    }

    let mkfs = find_mkfs_erofs();
    let dest_dir = Path::new(output_dir);
    std::fs::create_dir_all(dest_dir).map_err(|e| format!("mkdir {}: {}", output_dir, e))?;

    let out = dest_dir.join(format!("{}.erofs", name));
    let tmp = out.with_extension("tmp");

    let status = Command::new(&mkfs)
        .args(["-zlz4", &tmp.to_string_lossy(), &format!("{}/", src_dir)])
        .output()
        .map_err(|e| format!("mkfs.erofs: {}", e))?;

    if !status.status.success() {
        let stderr = String::from_utf8_lossy(&status.stderr);
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("mkfs.erofs failed: {}", stderr.trim()));
    }

    std::fs::rename(&tmp, &out).map_err(|e| format!("rename {:?} -> {:?}: {}", tmp, out, e))?;

    tracing::info!(%name, output = %out.display(), "EROFS layer built");
    Ok(out.to_string_lossy().to_string())
}

/// List available layer names under `layer_dir`.
///
/// Returns directory names and stripped `.erofs` filenames, filtering
/// out system-internal names.
pub fn list_layers(layer_dir: &str) -> Vec<String> {
    let system: HashSet<&'static str> = SYSTEM_LAYER_NAMES.iter().copied().collect();
    let dir = Path::new(layer_dir);
    let mut names: Vec<String> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .map(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                if let Some(stripped) = n.strip_suffix(".erofs") {
                    stripped.to_string()
                } else {
                    n
                }
            })
            .filter(|n| !system.contains(n.as_str()))
            .collect(),
        Err(_) => return Vec::new(),
    };
    names.sort();
    names.dedup();
    names
}

/// Remove a layer by name from `layer_dir`.
///
/// Tries to remove both the `<name>` directory and `<name>.erofs` image.
/// Succeeds if either existed and was removed; returns an error only when
/// neither existed.
pub fn remove_layer(name: &str, layer_dir: &str) -> Result<(), String> {
    let dir_cand = Path::new(layer_dir).join(name);
    let erofs_cand = Path::new(layer_dir).join(format!("{}.erofs", name));

    let mut removed = false;
    for cand in &[&dir_cand, &erofs_cand] {
        if cand.exists() {
            if cand.is_dir() {
                std::fs::remove_dir_all(cand)
                    .map_err(|e| format!("remove dir {:?}: {}", cand, e))?;
            } else {
                std::fs::remove_file(cand).map_err(|e| format!("remove file {:?}: {}", cand, e))?;
            }
            tracing::info!(path = %cand.display(), "layer removed");
            removed = true;
        }
    }

    if removed {
        Ok(())
    } else {
        Err(format!("layer '{}' not found under {}", name, layer_dir))
    }
}

/// Validate a layer name against the allowed character set.
///
/// Returns `Ok(())` when `name` matches `^[a-zA-Z0-9_.-]+$` and is
/// non-empty, otherwise an error describing the violation.
pub fn validate_layer_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("layer name must not be empty".to_string());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return Err(format!(
            "invalid layer name {:?}: only [a-zA-Z0-9_.-] allowed",
            name
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Resolve `mkfs.erofs` binary path.
fn find_mkfs_erofs() -> String {
    std::env::var("TERRA_MKFS_EROFS")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            for c in [
                "mkfs.erofs".to_string(),
                format!(
                    "{}/.local/share/terra/bin/mkfs.erofs",
                    std::env::var("HOME").unwrap_or_default()
                ),
                "/usr/bin/mkfs.erofs".to_string(),
            ] {
                if std::path::Path::new(&c).exists() || c == "mkfs.erofs" {
                    return c;
                }
            }
            "mkfs.erofs".into()
        })
}
