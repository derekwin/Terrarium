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
    /// Shared base image (read-only).
    pub base: String,
    /// Tool layers stacked base→outer (read-only, pre-built).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_layers: Vec<String>,
    /// Per-user upper directory name (used to derive path).
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
            tool_layers: vec![],
            name: name.into(),
            disk_size_gb: default_disk_size_gb(),
            format: OverlayFormat::default(),
            state_dir: default_state_dir(),
        }
    }

    /// Add a tool layer (stacked between base and user overlay).
    pub fn tool_layer(mut self, path: impl Into<String>) -> Self {
        self.tool_layers.push(path.into());
        self
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

    /// The effective backing file: last tool layer, or base if no tools.
    pub fn backing_file(&self) -> &str {
        self.tool_layers
            .last()
            .map(|s| s.as_str())
            .unwrap_or(&self.base)
    }

    /// Description of the layer stack for logging.
    pub fn layer_desc(&self) -> String {
        if self.tool_layers.is_empty() {
            "base".into()
        } else {
            self.tool_layers
                .iter()
                .map(|t| {
                    std::path::Path::new(t)
                        .file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                })
                .collect::<Vec<_>>()
                .join("+")
        }
    }
}
