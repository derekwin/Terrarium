//! VmManager — registry of all VMs managed by this controller instance.

use std::collections::HashMap;

use crate::spec::VmSpec;
use crate::vm::{VmError, VmHandle};

/// Central VM registry for the controller.
///
/// Owns all running VM handles, providing spawn, lookup, shutdown,
/// and resize operations keyed by VM name.
pub struct VmManager {
    vms: HashMap<String, VmHandle>,
}

impl VmManager {
    /// Create an empty VM manager.
    pub fn new() -> Self {
        Self {
            vms: HashMap::new(),
        }
    }

    /// Spawn a new VM from the given spec.
    ///
    /// Returns an error if a VM with the same name already exists.
    pub async fn spawn(&mut self, spec: VmSpec) -> std::result::Result<&VmHandle, VmError> {
        let name = spec.name.clone();
        if self.vms.contains_key(&name) {
            return Err(VmError::AlreadyExists { name });
        }

        let handle = VmHandle::spawn(spec).await?;
        self.vms.insert(name.clone(), handle);
        Ok(self.vms.get(&name).unwrap())
    }

    /// Get a reference to a running VM by name.
    pub fn get(&self, name: &str) -> std::result::Result<&VmHandle, VmError> {
        self.vms.get(name).ok_or_else(|| VmError::NotFound {
            name: name.to_string(),
        })
    }

    /// List all VM names.
    pub fn list_names(&self) -> Vec<&str> {
        self.vms.keys().map(|s| s.as_str()).collect()
    }

    /// Gracefully shut down a VM by name and remove it from the registry.
    pub async fn shutdown(&mut self, name: &str) -> std::result::Result<(), VmError> {
        let handle = self.vms.remove(name).ok_or_else(|| VmError::NotFound {
            name: name.to_string(),
        })?;
        handle.shutdown().await
    }

    /// Force-kill a VM by name and remove it from the registry.
    pub fn kill(&mut self, name: &str) -> std::result::Result<(), VmError> {
        let handle = self.vms.remove(name).ok_or_else(|| VmError::NotFound {
            name: name.to_string(),
        })?;
        handle.kill()
    }

    /// Destroy a VM: shut down and delete persistent files (overlay disk, etc).
    pub async fn destroy(&mut self, name: &str) -> std::result::Result<(), VmError> {
        let handle = self.vms.remove(name).ok_or_else(|| VmError::NotFound {
            name: name.to_string(),
        })?;
        handle.destroy().await
    }

    /// Shut down all VMs and clear the registry.
    /// Will be wired to signal handling (SIGTERM/SIGINT).
    #[allow(dead_code)]
    pub async fn shutdown_all(&mut self) {
        let names: Vec<String> = self.vms.keys().cloned().collect();
        for name in names {
            if let Err(e) = self.shutdown(&name).await {
                tracing::warn!(%name, error = %e, "Error shutting down VM");
            }
        }
    }

    /// Reap any VMs whose processes have exited unexpectedly.
    /// Returns the names of VMs that were removed.
    pub fn reap_dead(&mut self) -> Vec<String> {
        let mut dead = Vec::new();
        let names: Vec<String> = self.vms.keys().cloned().collect();
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
                // Remove and let it drop (Drop will attempt cleanup)
                self.vms.remove(&name);
                dead.push(name);
            }
        }
        dead
    }
}

impl Default for VmManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_manager_empty() {
        let mgr = VmManager::new();
        assert_eq!(mgr.list_names().len(), 0);
        assert!(mgr.list_names().is_empty());
    }

    #[test]
    fn test_manager_not_found() {
        let mgr = VmManager::new();
        let err = mgr.get("nonexistent").unwrap_err();
        assert!(err.to_string().contains("nonexistent"));
    }
}
