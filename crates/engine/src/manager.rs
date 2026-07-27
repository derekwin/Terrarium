//! VmManager — registry of running VMs managed by this controller instance.
//!
//! Owns all running VM handles. Lifecycle semantics: shutdown/kill/destroy
//! all mean "stop + deregister" and never touch any persistent data —
//! storage has its own lifecycle, managed outside VM commands (see
//! docs/plans/ for the filesystem design).

use std::collections::HashMap;
use std::sync::Arc;

use adapter_traits::{AdapterError, VmAdapter, VmHandle, VmName, VmSpec};

/// One warm-pool slot: an idle or claimed VM.
#[derive(Debug, Clone)]
pub struct PoolSlot {
    /// VM name (pool-N).
    pub name: String,
    /// Currently claimed by a task (fs attached).
    pub claimed: bool,
    /// Layers attached when claimed.
    pub layers: Vec<String>,
}

/// Central VM registry for the controller.
pub struct VmManager {
    adapter: Arc<dyn VmAdapter>,
    vms: HashMap<VmName, Box<dyn VmHandle>>,
    /// Warm pool slots (idle agent VMs ready for hot-plug assignment).
    pool: Vec<PoolSlot>,
    /// Next pool VM id.
    pool_next_id: u32,
}

impl VmManager {
    /// Create a new VM manager with the given adapter.
    pub fn new(adapter: Arc<dyn VmAdapter>) -> Self {
        Self {
            adapter,
            vms: HashMap::new(),
            pool: Vec::new(),
            pool_next_id: 0,
        }
    }

    /// Spawn a new VM from the given spec.
    /// Returns an error if a VM with the same name already exists.
    pub async fn spawn(&mut self, spec: VmSpec) -> Result<(), AdapterError> {
        let name = spec.name.clone();
        if self.vms.contains_key(&name) {
            return Err(AdapterError::internal(format!(
                "VM '{}' already exists",
                name
            )));
        }
        let handle = self.adapter.create(&spec).await?;
        self.vms.insert(name, handle);
        Ok(())
    }

    /// Execute a command inside a VM via its guest agent.
    pub async fn exec(
        &self,
        name: &str,
        args: &[String],
    ) -> Result<adapter_traits::ExecResult, AdapterError> {
        self.vms
            .get(name)
            .ok_or_else(|| AdapterError::not_found(format!("VM '{}' not found", name)))?
            .exec(args)
            .await
    }

    /// Create `size` idle warm-pool VMs (agent initramfs, no fs).
    /// Returns the names of the newly created pool VMs.
    pub async fn pool_create(
        &mut self,
        size: u32,
        kernel: &str,
        agent_initramfs: &str,
    ) -> Result<Vec<String>, AdapterError> {
        let mut created = Vec::new();
        for _ in 0..size {
            let name = format!("pool-{}", self.pool_next_id);
            self.pool_next_id += 1;
            let vm_name = VmName::new(name.clone()).map_err(AdapterError::invalid_argument)?;
            let spec = VmSpec {
                name: vm_name,
                kernel: kernel.to_string(),
                cmdline: None,
                boot_vcpus: 1,
                max_vcpus: Some(4),
                memory_mb: 256,
                max_memory_mb: Some(1024),
                initramfs: Some(agent_initramfs.to_string()),
                fs: None,
                backend_config: None,
            };
            self.spawn(spec).await?;
            self.pool.push(PoolSlot {
                name: name.clone(),
                claimed: false,
                layers: Vec::new(),
            });
            created.push(name);
        }
        Ok(created)
    }

    /// Pool status snapshot.
    pub fn pool_list(&self) -> Vec<PoolSlot> {
        self.pool.clone()
    }

    /// Claim an idle pool VM and hot-plug the given layers.
    /// Returns the claimed VM name.
    pub async fn pool_claim(&mut self, layers: Vec<String>) -> Result<String, AdapterError> {
        let idx = self
            .pool
            .iter()
            .position(|s| !s.claimed)
            .ok_or_else(|| AdapterError::internal("warm pool exhausted".to_string()))?;
        let name = self.pool[idx].name.clone();
        let fs = adapter_traits::FsSpec {
            layers: layers.clone(),
            upper: adapter_traits::UpperPolicy::Ephemeral,
        };
        self.attach_fs(&name, &fs).await?;
        self.pool[idx].claimed = true;
        self.pool[idx].layers = layers;
        tracing::info!(vm = %name, "pool VM claimed");
        Ok(name)
    }

    /// Release a claimed pool VM: detach its fs and return it to idle.
    pub async fn pool_release(&mut self, name: &str) -> Result<(), AdapterError> {
        let idx = self
            .pool
            .iter()
            .position(|s| s.name == name && s.claimed)
            .ok_or_else(|| AdapterError::not_found(format!("no claimed pool VM '{}'", name)))?;
        self.detach_fs(name).await?;
        self.pool[idx].claimed = false;
        self.pool[idx].layers.clear();
        tracing::info!(vm = %name, "pool VM released to idle");
        Ok(())
    }

    /// Hot-plug a layered filesystem into a running VM (warm-pool attach).
    pub async fn attach_fs(
        &self,
        name: &str,
        fs: &adapter_traits::FsSpec,
    ) -> Result<(), AdapterError> {
        self.vms
            .get(name)
            .ok_or_else(|| AdapterError::not_found(format!("VM '{}' not found", name)))?
            .attach_fs(fs)
            .await
    }

    /// Detach a previously attached layered filesystem.
    pub async fn detach_fs(&self, name: &str) -> Result<(), AdapterError> {
        self.vms
            .get(name)
            .ok_or_else(|| AdapterError::not_found(format!("VM '{}' not found", name)))?
            .detach_fs()
            .await
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

    /// Force-kill a VM by removing it from the registry; the handle's
    /// Drop kills the process.
    pub async fn kill(&mut self, name: &str) -> Result<(), AdapterError> {
        self.vms
            .remove(name)
            .ok_or_else(|| AdapterError::not_found(format!("VM '{}' not found", name)))?;
        Ok(())
    }

    /// Destroy a VM: stop it and remove it from the registry.
    /// Never touches persistent data.
    pub async fn destroy(&mut self, name: &str) -> Result<(), AdapterError> {
        let handle = self
            .vms
            .remove(name)
            .ok_or_else(|| AdapterError::not_found(format!("VM '{}' not found", name)))?;
        self.pool.retain(|s| s.name != name);
        handle.shutdown().await
    }

    /// Shut down all VMs and clear the registry.
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
                self.pool.retain(|s| s.name != name.as_ref());
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
