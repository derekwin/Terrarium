//! ChConfig — adapter configuration extracted from ChAdapter.
//!
//! Wraps an [`FsConfig`] and adds the Cloud Hypervisor binary path.
//! Created once at daemon start and shared behind an `Arc` so every VM
//! handle sees the same mounted-layer cache.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use crate::fs::FsConfig;

/// Cloud Hypervisor adapter configuration.
///
/// Implements [`Deref`](std::ops::Deref) to [`FsConfig`] so it can be
/// passed wherever the filesystem composition layer expects [`FsConfig`].
pub struct ChConfig {
    pub ch_binary: String,
    pub fs: FsConfig,
}

impl ChConfig {
    /// Build configuration from environment variables.
    ///
    /// | env var           | default                  |
    /// |-------------------|--------------------------|
    /// | `TERRA_STATE_DIR` | `/tmp/terra-disks`       |
    /// | `TERRA_VIRTIOFSD` | `virtiofsd`              |
    /// | `TERRA_LAYER_DIR` | `/var/lib/terra/layers`  |
    pub fn from_env(ch_binary: impl Into<String>) -> Self {
        let fs_base = std::env::var("TERRA_STATE_DIR")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/tmp/terra-disks".into());
        Self {
            ch_binary: ch_binary.into(),
            fs: FsConfig {
                // qemu's virtiofsd (apt) and rust-vmm's (cargo) share the CLI.
                virtiofsd_binary: std::env::var("TERRA_VIRTIOFSD")
                    .ok()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "virtiofsd".into()),
                layer_dir: std::env::var("TERRA_LAYER_DIR")
                    .ok()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "/var/lib/terra/layers".into()),
                fs_root: format!("{}/fs", fs_base),
                mounted_layers: Arc::new(Mutex::new(HashSet::new())),
            },
        }
    }
}

impl std::ops::Deref for ChConfig {
    type Target = FsConfig;

    fn deref(&self) -> &Self::Target {
        &self.fs
    }
}
