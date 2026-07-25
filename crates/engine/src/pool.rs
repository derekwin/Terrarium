//! Warm pool management.
//!
//! Maintains pools of pre-created qcow2 overlay disks for instant VM claims.
//! Architecture: template VM spec → N clean overlays → claim/release lifecycle.

use std::collections::HashMap;

use crate::spec::VmSpec;

/// A pool of pre-warmed overlay disks.
pub struct WarmPool {
    /// Pool name → overlay disk paths available for claiming.
    available: HashMap<String, Vec<String>>,
    /// Pool name → VmSpec template.
    templates: HashMap<String, VmSpec>,
}

impl WarmPool {
    pub fn new() -> Self {
        Self {
            available: HashMap::new(),
            templates: HashMap::new(),
        }
    }

    /// Create a pool with N pre-created overlay disks.
    pub fn create_pool(&mut self, name: &str, spec: &VmSpec, size: usize) -> Result<(), String> {
        if self.templates.contains_key(name) {
            return Err(format!("Pool '{}' already exists", name));
        }
        let base = spec
            .base_disk
            .as_ref()
            .ok_or("Pool requires base_disk for overlay creation")?;

        let mut overlays = Vec::with_capacity(size);
        for i in 0..size {
            let pool_name = format!("{}-{}", name, i);
            let mut os =
                overlay::OverlaySpec::new(&pool_name, base).disk_size_gb(spec.disk_size_gb);
            for tool in &spec.tool_layers {
                os = os.tool_layer(tool);
            }
            let path = overlay::OverlayManager::create_or_reuse(&os)
                .map_err(|e| format!("create overlay for pool '{}': {}", name, e))?;
            overlays.push(path);
        }

        self.templates.insert(name.to_string(), spec.clone());
        self.available.insert(name.to_string(), overlays);
        tracing::info!(pool = %name, size, "Pool created");
        Ok(())
    }

    /// Claim an overlay disk from the pool.
    pub fn claim(&mut self, name: &str) -> Result<String, String> {
        let overlays = self
            .available
            .get_mut(name)
            .ok_or_else(|| format!("Pool '{}' not found", name))?;
        if overlays.is_empty() {
            return Err(format!("Pool '{}' exhausted — scale up", name));
        }
        let path = overlays.pop().unwrap();
        tracing::info!(pool = %name, %path, remaining = overlays.len(), "Claimed");
        Ok(path)
    }

    /// List pools and available counts.
    pub fn list(&self) -> Vec<(String, usize)> {
        self.available
            .iter()
            .map(|(name, v)| (name.clone(), v.len()))
            .collect()
    }
}
