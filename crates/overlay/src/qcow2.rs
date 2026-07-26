//! qcow2 overlay management: create, reuse, destroy, disk usage.
//!
//! Uses `qemu-img` to create per-user qcow2 overlays with backing chains.
//! The backing file is the last tool layer (if any), otherwise the base disk.

use crate::spec::OverlaySpec;
use std::process::Command;

/// Manages the lifecycle of a qcow2 overlay stack.
pub struct OverlayManager;

impl OverlayManager {
    /// Create a user overlay if it doesn't exist. Returns the overlay path.
    /// Idempotent — reuses existing overlay.
    pub fn create_or_reuse(spec: &OverlaySpec) -> Result<String, String> {
        let overlay = spec.user_overlay_path();
        let vm_dir = std::path::Path::new(&overlay)
            .parent()
            .unwrap()
            .to_str()
            .unwrap();

        std::fs::create_dir_all(vm_dir).map_err(|e| format!("mkdir {}: {}", vm_dir, e))?;

        if std::path::Path::new(&overlay).exists() {
            // Reuse is only safe when the existing overlay's backing file
            // matches the requested base — otherwise the caller silently
            // gets a disk built on a different image (wrong data).
            let existing = Self::backing_file_of(&overlay)?;
            let requested = spec.backing_file();
            let same = existing.as_deref().map(|e| {
                let canon = |p: &str| {
                    std::fs::canonicalize(p)
                        .map(|c| c.to_string_lossy().into_owned())
                        .unwrap_or_else(|_| p.to_string())
                };
                canon(e) == canon(requested)
            });
            match same {
                Some(true) => {
                    tracing::info!(%overlay, "Reusing existing overlay disk");
                    return Ok(overlay);
                }
                _ => {
                    return Err(format!(
                        "overlay {} already exists with a different backing file ({:?} vs requested {}) — refusing to reuse; pick another name or delete it first",
                        overlay, existing, requested
                    ));
                }
            }
        }

        // Create to a temp file first, then atomically rename.
        // Prevents TOCTOU race where concurrent creates would overwrite
        // an in-use overlay via qemu-img's unconditional overwrite.
        let tmp = format!("{}.tmp", overlay);
        let backing = spec.backing_file();
        let output = Command::new("qemu-img")
            .args([
                "create",
                "-f",
                "qcow2",
                "-b",
                backing,
                "-F",
                "qcow2",
                &tmp,
                &format!("{}G", spec.disk_size_gb),
            ])
            .output()
            .map_err(|e| format!("qemu-img not found: {}", e))?;

        if !output.status.success() {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!(
                "qemu-img create overlay failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }

        std::fs::rename(&tmp, &overlay).map_err(|e| format!("rename temp overlay: {}", e))?;

        tracing::info!(
            %overlay,
            %backing,
            layers = %spec.layer_desc(),
            size_gb = spec.disk_size_gb,
            "Created qcow2 overlay disk"
        );
        Ok(overlay)
    }

    /// Read the backing file of a qcow2 image via qemu-img info.
    fn backing_file_of(path: &str) -> Result<Option<String>, String> {
        let output = Command::new("qemu-img")
            .args(["info", "--output=json", path])
            .output()
            .map_err(|e| format!("qemu-img info: {}", e))?;
        if !output.status.success() {
            return Err(format!(
                "qemu-img info {}: {}",
                path,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let v: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|e| format!("parse qemu-img output: {}", e))?;
        Ok(v["full-backing-filename"]
            .as_str()
            .or_else(|| v["backing-filename"].as_str())
            .map(String::from))
    }

    /// Destroy the user overlay and its directory.
    pub fn destroy(spec: &OverlaySpec) -> Result<(), String> {
        let overlay = spec.user_overlay_path();
        if std::path::Path::new(&overlay).exists() {
            std::fs::remove_file(&overlay).map_err(|e| format!("remove overlay: {}", e))?;
            tracing::info!(%overlay, "Destroyed overlay disk");
        }
        let vm_dir = std::path::Path::new(&overlay).parent().unwrap();
        if vm_dir.exists() {
            let _ = std::fs::remove_dir_all(vm_dir);
        }
        Ok(())
    }

    /// Check if the user overlay exists.
    pub fn exists(spec: &OverlaySpec) -> bool {
        std::path::Path::new(&spec.user_overlay_path()).exists()
    }

    /// Get the actual disk usage of the overlay file in bytes.
    pub fn disk_usage(spec: &OverlaySpec) -> Result<u64, String> {
        let overlay = spec.user_overlay_path();
        let output = Command::new("qemu-img")
            .args(["info", "--output=json", &overlay])
            .output()
            .map_err(|e| format!("qemu-img info: {}", e))?;

        let v: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|e| format!("parse qemu-img output: {}", e))?;

        v["actual-size"]
            .as_u64()
            .ok_or_else(|| "missing actual-size in qemu-img output".into())
    }

    /// Get the virtual size of the overlay in bytes.
    pub fn virtual_size(spec: &OverlaySpec) -> Result<u64, String> {
        let overlay = spec.user_overlay_path();
        let output = Command::new("qemu-img")
            .args(["info", "--output=json", &overlay])
            .output()
            .map_err(|e| format!("qemu-img info: {}", e))?;

        let v: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|e| format!("parse qemu-img output: {}", e))?;

        v["virtual-size"]
            .as_u64()
            .ok_or_else(|| "missing virtual-size in qemu-img output".into())
    }
}
