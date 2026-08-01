//! Engine-level sandbox registry: workdirs on tenant/pool VMs.
//!
//! Holds the [`SandboxRecord`] type and all sandbox operations. The
//! sandbox map itself stays a flat field on [`VmManager`]; these methods
//! are split out here because they touch nothing but the map.

use std::sync::Arc;

use adapter_traits::{SandboxHandle, SandboxPolicy};

use super::VmManager;

/// One engine-level sandbox: a workdir on a tenant's shared VM.
#[derive(Clone)]
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
    /// Bound L2 session handle (C3): created via `SandboxAdapter::create`
    /// with the effective policy, so blocking sandbox_exec routes through
    /// it. `None` for records built before C3 (tests insert bare records).
    pub handle: Option<Arc<dyn SandboxHandle>>,
}

impl std::fmt::Debug for SandboxRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SandboxRecord")
            .field("id", &self.id)
            .field("tenant", &self.tenant)
            .field("vm_name", &self.vm_name)
            .field("workdir", &self.workdir)
            .field("created_at", &self.created_at)
            .field("policy", &self.policy)
            .field("pool_backed", &self.pool_backed)
            .field("handle", &self.handle.as_ref().map(|_| "<SandboxHandle>"))
            .finish()
    }
}

impl VmManager {
    /// Look up a sandbox by id.
    pub fn sandbox_get(&self, id: &str) -> Option<SandboxRecord> {
        self.sandboxes.get(id).cloned()
    }

    /// Clone the bound session handle of a sandbox (C3). `None` when the
    /// record is missing or holds no handle (pre-C3 records).
    pub fn sandbox_handle(&self, id: &str) -> Option<Arc<dyn SandboxHandle>> {
        self.sandboxes.get(id).and_then(|r| r.handle.clone())
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
