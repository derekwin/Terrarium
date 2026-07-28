//! EROFS layer image mount / resolution helpers.
//!
//! Pure filesystem logic — no dependency on any VM or adapter types.

use adapter_traits::AdapterError;
use std::process::Command;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Whether `mnt` is an active mountpoint according to /proc/mounts.
pub fn is_mounted(mnt: &str) -> bool {
    std::fs::read_to_string("/proc/mounts")
        .map(|c| c.lines().any(|l| l.split(' ').nth(1) == Some(mnt)))
        .unwrap_or(false)
}

/// Mount an EROFS image read-only at `mnt`. Kernel loop mount when
/// privileged, erofsfuse fallback otherwise.
pub fn mount_erofs(image: &str, mnt: &str) -> Result<(), AdapterError> {
    let kernel = Command::new("mount")
        .args(["-o", "loop,ro", "-t", "erofs", image, mnt])
        .output();
    if let Ok(out) = kernel {
        if out.status.success() {
            tracing::info!(%image, %mnt, "EROFS layer mounted (kernel)");
            return Ok(());
        }
    }
    let fuse_bin = std::env::var("TERRA_EROFSFUSE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            for c in [
                "erofsfuse".to_string(),
                format!(
                    "{}/.local/share/terra/bin/erofsfuse",
                    std::env::var("HOME").unwrap_or_default()
                ),
                "/usr/bin/erofsfuse".to_string(),
            ] {
                if std::path::Path::new(&c).exists() || c == "erofsfuse" {
                    return c;
                }
            }
            "erofsfuse".into()
        });
    let out = Command::new(&fuse_bin)
        .args([image, mnt])
        .output()
        .map_err(|e| format!("mount failed (need root) and erofsfuse not found: {}", e))?;
    if !out.status.success() {
        return Err(format!(
            "erofsfuse {}: {}",
            image,
            String::from_utf8_lossy(&out.stderr).trim()
        )
        .into());
    }
    tracing::info!(%image, %mnt, "EROFS layer mounted (erofsfuse)");
    Ok(())
}
