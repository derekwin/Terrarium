//! Warm-pool registry: idle agent VMs ready for hot-plug assignment.
//!
//! Holds the pool slot types and all warm-pool operations. The slot data
//! itself stays a flat field on [`VmManager`]; these methods are split out
//! here because they only touch the pool plus the shared VM lifecycle
//! (spawn/destroy/attach_fs on `self`).

use adapter_traits::{AdapterError, FsSpec, UpperPolicy, VmName, VmSpec};

use super::VmManager;

/// One warm-pool slot: an idle or claimed VM.
#[derive(Debug, Clone)]
pub struct PoolSlot {
    /// VM name (pool-N).
    pub name: String,
    /// Currently claimed by a task (fs attached).
    pub claimed: bool,
    /// Layers attached when claimed.
    pub layers: Vec<String>,
    /// Whether this VM was booted with networking (claim matching).
    pub net: bool,
}

/// Outcome of `pool_create`: which VMs became ready and which failed
/// their guest-agent readiness probe (and were destroyed again).
#[derive(Debug, Default)]
pub struct PoolCreateOutcome {
    /// Names of VMs that are ready and slotted idle.
    pub ready: Vec<String>,
    /// (name, error) for VMs that never became ready.
    pub failed: Vec<(String, String)>,
}

impl VmManager {
    /// Create `size` idle warm-pool VMs (agent initramfs, no fs).
    /// Each VM's guest agent is pinged before its slot goes idle; a VM
    /// that never becomes ready is destroyed again and reported in
    /// `PoolCreateOutcome::failed` (never silently slotted).
    pub async fn pool_create(
        &mut self,
        size: u32,
        kernel: &str,
        agent_initramfs: &str,
        net: bool,
    ) -> Result<PoolCreateOutcome, AdapterError> {
        let mut outcome = PoolCreateOutcome::default();
        for _ in 0..size {
            let name = format!("pool-{}", self.pool_next_id);
            self.pool_next_id += 1;
            let vm_name = VmName::new(name.clone()).map_err(AdapterError::invalid_argument)?;
            let spec = VmSpec {
                name: vm_name,
                kernel: Some(kernel.to_string()),
                cmdline: None,
                boot_vcpus: 1,
                max_vcpus: Some(4),
                memory_mb: 256,
                max_memory_mb: Some(1024),
                initramfs: Some(agent_initramfs.to_string()),
                net,
                fs: None,
            };
            self.spawn(spec).await?;
            match self.wait_agent_ready(&name).await {
                Ok(()) => {
                    self.pool.push(PoolSlot {
                        name: name.clone(),
                        claimed: false,
                        layers: Vec::new(),
                        net,
                    });
                    outcome.ready.push(name);
                }
                Err(e) => {
                    tracing::warn!(vm = %name, error = %e, "pool VM never became ready; destroying");
                    if let Err(de) = self.destroy(&name).await {
                        tracing::warn!(vm = %name, error = %de, "cleanup of unready pool VM failed");
                    }
                    outcome.failed.push((name, e.to_string()));
                }
            }
        }
        Ok(outcome)
    }

    /// Wait for a VM's guest agent to answer a ping, with bounded retries.
    async fn wait_agent_ready(&self, name: &str) -> Result<(), AdapterError> {
        let handle = self
            .get_handle(name)
            .ok_or_else(|| AdapterError::not_found(format!("VM '{}' not found", name)))?;
        let mut last_err = AdapterError::internal("readiness probe did not run".to_string());
        for _ in 0..self.ready_attempts {
            match handle.ping().await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    last_err = e;
                    tokio::time::sleep(self.ready_interval).await;
                }
            }
        }
        Err(AdapterError::timeout(format!(
            "guest agent of '{}' not ready after {} attempts: {}",
            name, self.ready_attempts, last_err
        )))
    }

    /// Pool status snapshot.
    pub fn pool_list(&self) -> Vec<PoolSlot> {
        self.pool.clone()
    }

    /// Claim an idle pool VM and hot-plug the given layers.
    /// Returns the claimed VM name.
    pub async fn pool_claim(&mut self, layers: Vec<String>) -> Result<String, AdapterError> {
        self.pool_claim_matching(layers, None).await
    }

    /// Claim an idle pool VM, optionally requiring a networking match
    /// (`Some(true)` needs a net-enabled slot, `Some(false)` a plain one).
    /// The upper is always ephemeral — no data may leak between sequential
    /// claims of one slot.
    pub async fn pool_claim_matching(
        &mut self,
        layers: Vec<String>,
        net: Option<bool>,
    ) -> Result<String, AdapterError> {
        let idx = self
            .pool
            .iter()
            .position(|s| !s.claimed && net.is_none_or(|n| s.net == n))
            .ok_or_else(|| AdapterError::internal("warm pool exhausted".to_string()))?;
        let name = self.pool[idx].name.clone();
        let fs = FsSpec {
            layers: layers.clone(),
            upper: UpperPolicy::Ephemeral,
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

    /// Atomically destroy up to *count* idle pool VMs (claimed slots are
    /// never touched). Returns the destroyed VM names.
    ///
    /// Runs under the single manager lock, closing the client-side scale
    /// TOCTOU window where a concurrent claim could land on a slot about
    /// to be destroyed. Growth stays `pool_create` (over-provisioning is
    /// harmless); shrinking must be atomic because destroying a freshly
    /// claimed VM would kill a live sandbox.
    pub async fn pool_shrink(&mut self, count: u32) -> Vec<String> {
        let mut removed = Vec::new();
        for _ in 0..count {
            let idx = match self.pool.iter().position(|s| !s.claimed) {
                Some(i) => i,
                None => break, // no more idle slots
            };
            let name = self.pool[idx].name.clone();
            // destroy -> unregister cleans the pool slot, net tracking,
            // sandbox records and sessions atomically.
            if self.destroy(&name).await.is_ok() {
                removed.push(name);
            } else {
                break; // VM vanished or other error — stop, retry later
            }
        }
        removed
    }
}
