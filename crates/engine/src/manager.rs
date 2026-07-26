//! VmManager — registry of all VMs managed by this controller instance.

use std::collections::HashMap;
use std::sync::Arc;

use adapter_traits::{AdapterError, VmAdapter, VmHandle, VmName, VmSpec};

/// Central VM registry for the controller.
///
/// Owns all running VM handles, providing spawn, lookup, shutdown,
/// and resize operations keyed by VM name.
pub struct VmManager {
    adapter: Arc<dyn VmAdapter>,
    vms: HashMap<VmName, Box<dyn VmHandle>>,
    /// Overlay disk paths for cleanup on destroy.
    overlays: HashMap<VmName, String>,
}

impl VmManager {
    /// Create a new VM manager with the given adapter.
    pub fn new(adapter: Arc<dyn VmAdapter>) -> Self {
        Self {
            adapter,
            vms: HashMap::new(),
            overlays: HashMap::new(),
        }
    }

    /// Spawn a new VM from the given spec.
    ///
    /// Creates qcow2 overlay if base_disk is configured, then delegates
    /// to the adapter. Returns an error if a VM with the same name already exists.
    pub async fn spawn(&mut self, spec: VmSpec) -> Result<(), AdapterError> {
        let name = spec.name.clone();
        if self.vms.contains_key(&name) {
            return Err(AdapterError::internal(format!(
                "VM '{}' already exists",
                name
            )));
        }

        // Create qcow2 overlay if base_disk is configured.
        // We track the overlay path for cleanup on destroy.
        let mut spec = spec;
        if let Some(ref base) = spec.base_disk {
            let overlay_spec =
                overlay::OverlaySpec::new(name.to_string(), base).disk_size_gb(spec.disk_size_gb);
            let overlay_path = overlay::OverlayManager::create_or_reuse(&overlay_spec)
                .map_err(|e| AdapterError::internal(format!("overlay: {}", e)))?;
            spec.disks.push(overlay_path.clone());
            // The VMM opens the backing file implicitly via the overlay's
            // qcow2 header — record it so the adapter can whitelist it
            // for CH --landlock.
            spec.overlay_backing.push(base.clone());
            self.overlays.insert(name.clone(), overlay_path);
            // Clear base_disk so the adapter doesn't create a second overlay.
            spec.base_disk = None;
        }

        let handle = match self.adapter.create(&spec).await {
            Ok(h) => h,
            Err(e) => {
                // Clean up overlay if adapter creation failed
                if let Some(disk) = self.overlays.remove(&name) {
                    let _ = std::fs::remove_file(&disk);
                    let state_dir = std::env::var("TERRA_STATE_DIR")
                        .ok()
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "/tmp/terra-disks/vms".to_string());
                    let vm_dir = format!("{}/{}", state_dir, name);
                    let _ = std::fs::remove_dir_all(&vm_dir);
                }
                return Err(e);
            }
        };
        self.vms.insert(name, handle);
        Ok(())
    }

    /// Get a reference to a running VM by name.
    pub fn get(&self, name: &str) -> Option<&dyn VmHandle> {
        self.vms.get(name).map(|v| v.as_ref())
    }

    /// List all VM names.
    pub fn list_names(&self) -> Vec<&str> {
        self.vms.keys().map(|s| s.as_ref()).collect()
    }

    /// Gracefully shut down a VM by name and remove it from the registry.
    pub async fn shutdown(&mut self, name: &str) -> Result<(), AdapterError> {
        let handle = self
            .vms
            .remove(name)
            .ok_or_else(|| AdapterError::not_found(format!("VM '{}' not found", name)))?;
        handle.shutdown().await
    }

    /// Force-kill is not supported through the adapter trait.
    /// Use shutdown with a timeout instead.
    pub async fn kill(&mut self, name: &str) -> Result<(), AdapterError> {
        // Adapter trait has no kill method. Remove from registry
        // and let the handle drop (which should kill the process).
        self.vms
            .remove(name)
            .ok_or_else(|| AdapterError::not_found(format!("VM '{}' not found", name)))?;
        Ok(())
    }

    /// Destroy a VM: shut down and delete persistent files (overlay disk, etc).
    pub async fn destroy(&mut self, name: &str) -> Result<(), AdapterError> {
        // The VM may already be deregistered (e.g. after shutdown) — the
        // overlay must still be cleaned up. Only report not-found when the
        // name is unknown to both the registry and the overlay map.
        let known = self.vms.contains_key(name) || self.overlays.contains_key(name);
        if !known {
            return Err(AdapterError::not_found(format!("VM '{}' not found", name)));
        }
        if let Some(handle) = self.vms.remove(name) {
            handle.shutdown().await?;
        }

        // Clean up overlay disk
        if let Some(disk) = self.overlays.remove(name) {
            let _ = std::fs::remove_file(&disk);
            let state_dir = std::env::var("TERRA_STATE_DIR")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "/tmp/terra-disks/vms".to_string());
            let vm_dir = format!("{}/{}", state_dir, name);
            let _ = std::fs::remove_dir_all(&vm_dir);
        }
        Ok(())
    }

    /// Shut down all VMs and clear the registry.
    /// Will be wired to signal handling (SIGTERM/SIGINT).
    #[allow(dead_code)]
    pub async fn shutdown_all(&mut self) {
        let names: Vec<VmName> = self.vms.keys().cloned().collect();
        for name in names {
            if let Err(e) = self.shutdown(name.as_ref()).await {
                tracing::warn!(%name, error = %e, "Error shutting down VM");
            }
        }
    }

    /// Reap any VMs whose processes have exited unexpectedly.
    /// Returns the names of VMs that were removed.
    pub fn reap_dead(&mut self) -> Vec<VmName> {
        let mut dead = Vec::new();
        let names: Vec<VmName> = self.vms.keys().cloned().collect();
        for name in names {
            let remove = {
                if let Some(handle) = self.vms.get_mut(&name) {
                    !handle.is_alive()
                } else {
                    false
                }
            };
            if remove {
                tracing::warn!(%name, "Reaping dead VM");
                self.vms.remove(&name);
                dead.push(name);
            }
        }
        dead
    }
}

impl Default for VmManager {
    fn default() -> Self {
        // Default requires an adapter. We panic if called without one —
        // callers should use VmManager::new(adapter) explicitly.
        panic!("VmManager requires an adapter; use VmManager::new()")
    }
}

#[cfg(test)]
mod tests {
    // Tests require a mock adapter — deferred to integration tests.
    #[test]
    fn test_placeholder() {
        // VmManager requires an adapter; unit tests need a mock VmAdapter.
    }
}
