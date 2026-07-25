//! Overlay specification: defines the layer stack configuration.

use serde::{Deserialize, Serialize};

/// Supported disk formats for overlay stacks.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub enum OverlayFormat {
    #[default]
    Qcow2,
    Raw,
}

/// Specification for building a qcow2 overlay stack.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverlaySpec {
    /// Shared rootfs image (read-only).
    pub base: String,
    /// Per-user upper overlay name (used to derive path).
    pub name: String,
    /// Virtual disk size ceiling in GB.
    #[serde(default = "default_disk_size_gb")]
    pub disk_size_gb: u64,
    /// Disk format for this overlay stack.
    #[serde(default)]
    pub format: OverlayFormat,
    /// State directory for storing overlays.
    #[serde(default = "default_state_dir")]
    pub state_dir: String,
}

fn default_disk_size_gb() -> u64 {
    20
}

fn default_state_dir() -> String {
    "/tmp/terra-disks/vms".to_string()
}

impl OverlaySpec {
    /// Create a minimal spec with base disk and VM name.
    pub fn new(name: impl Into<String>, base: impl Into<String>) -> Self {
        Self {
            base: base.into(),
            name: name.into(),
            disk_size_gb: default_disk_size_gb(),
            format: OverlayFormat::default(),
            state_dir: default_state_dir(),
        }
    }

    /// Set the virtual disk size in GB.
    pub fn disk_size_gb(mut self, gb: u64) -> Self {
        self.disk_size_gb = gb;
        self
    }

    /// Set the state directory for overlay storage.
    pub fn state_dir(mut self, dir: impl Into<String>) -> Self {
        self.state_dir = dir.into();
        self
    }

    /// Path to the user overlay file.
    pub fn user_overlay_path(&self) -> String {
        format!("{}/{}/overlay.qcow2", self.state_dir, self.name)
    }

    /// The backing file for the user overlay — always the rootfs.
    pub fn backing_file(&self) -> &str {
        &self.base
    }

    /// Description for logging.
    pub fn layer_desc(&self) -> String {
        std::path::Path::new(&self.base)
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    }
}
