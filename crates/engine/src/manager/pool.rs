//! Warm-pool registry: idle agent VMs ready for hot-plug assignment.
//!
//! Holds the pool slot types and all warm-pool operations. The slot data
//! itself stays a flat field on [`VmManager`]; these methods are split out
//! here because they only touch the pool plus the shared VM lifecycle
//! (spawn/destroy/attach_fs on `self`).

use adapter_traits::{AdapterError, FsSpec, UpperPolicy, VmHandle, VmName, VmSpec};

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
    /// Ready slot: pre-restored from a snapshot with its layered fs ALREADY
    /// attached and the guest agent running. Claiming one skips the fs
    /// hot-plug (the slow part of a warm-pool claim); releasing it resets
    /// the VM in place instead of detaching the fs. The ready state must
    /// live in the LAYER (episode writes go to the ephemeral upper, which
    /// the in-place reset clears).
    pub ready: bool,
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
                ready: false,
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

    /// Whether a pool slot is a READY (snapshot pre-restored) slot.
    pub fn pool_slot_ready(&self, name: &str) -> bool {
        self.pool
            .iter()
            .find(|s| s.name == name)
            .map(|s| s.ready)
            .unwrap_or(false)
    }

    /// Post-teardown release bookkeeping (the slow reset/detach already ran
    /// outside the manager lock): mark the slot idle; clear the layer set
    /// for warm slots only (ready slots keep theirs for exact matching).
    pub fn pool_mark_released(&mut self, name: &str, ready: bool) {
        if let Some(slot) = self.pool.iter_mut().find(|s| s.name == name) {
            slot.claimed = false;
            if !ready {
                slot.layers.clear();
            }
        }
    }

    /// Re-lock bookkeeping for the lock-free pool fill: register a
    /// restored VM and slot it as READY (fs attached, agent pings pass).
    pub fn pool_register_ready(
        &mut self,
        spec: &VmSpec,
        handle: Box<dyn VmHandle>,
        layers: Vec<String>,
        net: bool,
    ) -> Result<(), AdapterError> {
        let name = spec.name.to_string();
        self.register_vm(spec, handle)?;
        self.pool.push(PoolSlot {
            name,
            claimed: false,
            layers,
            net,
            ready: true,
        });
        Ok(())
    }

    /// Claim an idle pool VM and hot-plug the given layers.
    /// Returns the claimed VM name.
    pub async fn pool_claim(&mut self, layers: Vec<String>) -> Result<String, AdapterError> {
        self.pool_claim_matching(layers, None).await
    }

    /// Claim an idle pool VM, optionally requiring a networking match
    /// (`Some(true)` needs a net-enabled slot, `Some(false)` a plain one).
    /// READY slots (pre-restored, fs attached) are preferred and claim
    /// with zero fs work — the slot's layer set must match exactly. Warm
    /// slots fall back to hot-plugging the requested layers. The upper is
    /// always ephemeral — no data may leak between sequential claims.
    pub async fn pool_claim_matching(
        &mut self,
        layers: Vec<String>,
        net: Option<bool>,
    ) -> Result<String, AdapterError> {
        let ready_idx = self.pool.iter().position(|s| {
            !s.claimed
                && s.ready
                && s.layers == layers
                && net.is_none_or(|n| s.net == n)
        });
        if let Some(idx) = ready_idx {
            let name = self.pool[idx].name.clone();
            self.pool[idx].claimed = true;
            tracing::info!(vm = %name, layers = ?layers, "ready pool slot claimed (fs already attached)");
            return Ok(name);
        }
        let idx = self
            .pool
            .iter()
            .position(|s| !s.claimed && !s.ready && net.is_none_or(|n| s.net == n))
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
        let ready = self.pool[idx].ready;
        if ready {
            // In-place reset back to the LAYER baseline (the ready state
            // must live in the layer; episode writes land in the ephemeral
            // upper, which the guest reset clears). The fs stays attached,
            // so the next claim is a direct bind.
            let handle = self
                .get_handle(name)
                .ok_or_else(|| AdapterError::not_found(format!("VM '{}' not found", name)))?;
            handle.reset_fs().await?;
            tracing::info!(vm = %name, "ready pool slot released (in-place reset)");
        } else {
            self.detach_fs(name).await?;
            tracing::info!(vm = %name, "pool VM released to idle");
        }
        self.pool[idx].claimed = false;
        if !ready {
            self.pool[idx].layers.clear();
        }
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
