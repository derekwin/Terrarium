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
            tracing::info!(%overlay, "Reusing existing overlay disk");
            return Ok(overlay);
        }

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
                &overlay,
                &format!("{}G", spec.disk_size_gb),
            ])
            .output()
            .map_err(|e| format!("qemu-img not found: {}", e))?;

        if !output.status.success() {
            return Err(format!(
                "qemu-img create overlay failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }

        tracing::info!(
            %overlay,
            %backing,
            layers = %spec.layer_desc(),
            size_gb = spec.disk_size_gb,
            "Created qcow2 overlay disk"
        );
        Ok(overlay)
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
