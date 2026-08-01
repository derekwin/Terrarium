//! Engine-level sandbox registry: workdirs on tenant/pool VMs.
//!
//! Holds the [`SandboxRecord`] type and all sandbox operations. The
//! sandbox map itself stays a flat field on [`VmManager`]; these methods
//! are split out here because they touch nothing but the map.

use adapter_traits::SandboxPolicy;

use super::VmManager;

/// One engine-level sandbox: a workdir on a tenant's shared VM.
#[derive(Debug, Clone)]
pub struct SandboxRecord {
    pub id: String,
    pub tenant: String,
    pub vm_name: String,
    pub workdir: String,
    /// Unix seconds when the sandbox was created.
    pub created_at: u64,
    /// Sandlock policy stored at sandbox_create; inherited by sandbox_exec
    /// unless the call carries an override.
    pub policy: Option<SandboxPolicy>,
    /// True when the tenant VM is a claimed warm-pool VM (pool-N);
    /// tenant_destroy releases it back to the pool instead of destroying.
    pub pool_backed: bool,
}

impl VmManager {
    /// Look up a sandbox by id.
    pub fn sandbox_get(&self, id: &str) -> Option<SandboxRecord> {
        self.sandboxes.get(id).cloned()
    }

    /// List sandboxes, optionally filtered by tenant.
    pub fn sandbox_list(&self, tenant: Option<&str>) -> Vec<SandboxRecord> {
        self.sandboxes
            .values()
            .filter(|r| tenant.is_none_or(|t| r.tenant == t))
            .cloned()
            .collect()
    }

    /// Register a new sandbox.
    pub fn sandbox_insert(&mut self, record: SandboxRecord) {
        self.sandboxes.insert(record.id.clone(), record);
    }

    /// Remove a sandbox record by id.
    pub fn sandbox_remove(&mut self, id: &str) -> Option<SandboxRecord> {
        self.sandboxes.remove(id)
    }

    /// Drop all sandbox records of a tenant (tenant_destroy).
    /// Returns the number removed.
    pub fn sandbox_remove_tenant(&mut self, tenant: &str) -> usize {
        let before = self.sandboxes.len();
        self.sandboxes.retain(|_, r| r.tenant != tenant);
        before - self.sandboxes.len()
    }
}
