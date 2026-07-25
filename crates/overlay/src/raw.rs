//! Raw disk overlay management for VMMs that don't support qcow2.
//!
//! Creates flat raw disk images. No COW at block level —
//! VMMs like Firecracker that don't support backing chains use this.
//! Overlay layering is still achieved via filesystem-level mechanisms.

use crate::spec::OverlaySpec;
use std::process::Command;

pub struct RawDiskManager;

impl RawDiskManager {
    /// Create a raw disk from the overlay chain (converts qcow2→raw if needed).
    pub fn create_or_reuse(spec: &OverlaySpec) -> Result<String, String> {
        let overlay = spec.user_overlay_path();
        let vm_dir = std::path::Path::new(&overlay)
            .parent()
            .unwrap()
            .to_str()
            .unwrap();
        std::fs::create_dir_all(vm_dir).map_err(|e| format!("mkdir {}: {}", vm_dir, e))?;

        if std::path::Path::new(&overlay).exists() {
            tracing::info!(%overlay, "Reusing existing raw disk");
            return Ok(overlay);
        }

        // Convert the backing file (qcow2 last tool layer or base) to raw
        let backing = spec.backing_file();
        let output = Command::new("qemu-img")
            .args(["convert", "-f", "qcow2", "-O", "raw", backing, &overlay])
            .output()
            .map_err(|e| format!("qemu-img convert: {}", e))?;

        if !output.status.success() {
            return Err(format!(
                "qemu-img convert failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        tracing::info!(%overlay, %backing, "Created raw disk from qcow2 backing");
        Ok(overlay)
    }

    /// Check if the raw disk exists.
    pub fn exists(spec: &OverlaySpec) -> bool {
        std::path::Path::new(&spec.user_overlay_path()).exists()
    }
}
