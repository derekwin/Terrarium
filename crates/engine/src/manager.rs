//! VmManager — registry of running VMs managed by this controller instance.
//!
//! Owns all running VM handles. Lifecycle semantics: shutdown/kill/destroy
//! all mean "stop + deregister" and never touch any persistent data —
//! storage has its own lifecycle, managed outside VM commands (see
//! docs/plans/ for the filesystem design).
//!
//! The pool, background-session and sandbox responsibilities live in the
//! private submodules [`pool`], [`sessions`] and [`sandboxes`] (impl blocks
//! over the flat fields of [`VmManager`]); this file keeps the struct,
//! the VM lifecycle, and the cross-registry [`VmManager::unregister`].

mod pool;
mod sandboxes;
mod sessions;

pub use pool::{PoolCreateOutcome, PoolSlot};
pub use sandboxes::SandboxRecord;
pub use sessions::SessionInfo;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use adapter_traits::{AdapterError, ExecOpts, SandboxPolicy, VmAdapter, VmHandle, VmName, VmSpec};

/// Central VM registry for the controller.
pub struct VmManager {
    adapter: Arc<dyn VmAdapter>,
    vms: HashMap<VmName, Arc<dyn VmHandle>>,
    /// VMs created with networking enabled.
    net_vms: HashSet<String>,
    /// Warm pool slots (idle agent VMs ready for hot-plug assignment).
    pool: Vec<PoolSlot>,
    /// Next pool VM id.
    pool_next_id: u32,
    /// Directory for snapshot artifacts (default: "/tmp").
    snapshot_dir: String,
    /// Background exec sessions.
    sessions: Arc<Mutex<HashMap<String, SessionInfo>>>,
    /// Engine-level sandboxes (id → record).
    sandboxes: HashMap<String, SandboxRecord>,
    /// Guest-agent readiness probe: attempts × interval (pool_create).
    ready_attempts: u32,
    ready_interval: std::time::Duration,
}

impl VmManager {
    /// Create a new VM manager with the given adapter and snapshot directory.
    pub fn new(adapter: Arc<dyn VmAdapter>, snapshot_dir: String) -> Self {
        Self {
            adapter,
            vms: HashMap::new(),
            net_vms: HashSet::new(),
            pool: Vec::new(),
            pool_next_id: 0,
            snapshot_dir,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            sandboxes: HashMap::new(),
            ready_attempts: 50,
            ready_interval: std::time::Duration::from_millis(200),
        }
    }

    /// Override the guest-agent readiness probe used by `pool_create`
    /// (mainly for tests; defaults are 50 attempts × 200ms).
    pub fn with_readiness_probe(mut self, attempts: u32, interval_ms: u64) -> Self {
        self.ready_attempts = attempts;
        self.ready_interval = std::time::Duration::from_millis(interval_ms);
        self
    }

    /// Return the directory used for snapshot artifacts.
    pub fn snapshot_dir(&self) -> &str {
        &self.snapshot_dir
    }

    /// Whether a VM was created with networking enabled.
    pub fn has_net(&self, name: &str) -> bool {
        self.net_vms.contains(name)
    }

    /// Number of VMs currently using the NAT bridge.
    pub fn net_in_use(&self) -> usize {
        self.net_vms.len()
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
        let net = spec.net;
        let handle = self.adapter.create(&spec).await?;
        if net {
            self.net_vms.insert(name.to_string());
        }
        self.vms.insert(name, Arc::from(handle));
        Ok(())
    }

    /// Execute a command inside a VM via its guest agent.
    pub async fn exec(
        &self,
        name: &str,
        args: &[String],
        timeout_secs: u64,
        sandbox: bool,
        work_dir: Option<&str>,
        policy: Option<SandboxPolicy>,
    ) -> Result<adapter_traits::ExecResult, AdapterError> {
        let mut opts = ExecOpts::new(args.to_vec(), timeout_secs).with_sandbox(sandbox);
        if let Some(work_dir) = work_dir {
            opts = opts.with_work_dir(work_dir);
        }
        opts.policy = policy;
        self.vms
            .get(name)
            .ok_or_else(|| AdapterError::not_found(format!("VM '{}' not found", name)))?
            .exec(&opts)
            .await
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

    /// Get an `Arc` clone of a handle (for background tasks).
    pub fn get_handle(&self, name: &str) -> Option<Arc<dyn VmHandle>> {
        self.vms.get(name).cloned()
    }

    /// List all VM names.
    pub fn list_names(&self) -> Vec<&str> {
        self.vms.keys().map(|s| s.as_ref()).collect()
    }

    /// Atomically remove `name` from every auxiliary registry: the VM map,
    /// the net-enabled set, warm-pool slots, sandbox records pointing at
    /// this VM, and any in-flight background sessions on it (marked
    /// `terminated` — their VM is gone, so they can never complete; the
    /// completion task never overwrites a non-running status). Returns the
    /// removed handle when the VM was registered.
    fn unregister(&mut self, name: &str) -> Option<Arc<dyn VmHandle>> {
        let handle = self.vms.remove(name);
        self.net_vms.remove(name);
        self.pool.retain(|s| s.name != name);
        self.sandboxes.retain(|_, r| r.vm_name != name);
        self.terminate_sessions(name);
        handle
    }

    /// Gracefully shut down a VM by name and remove it — and all of its
    /// registry state (pool slot, net tracking, sandbox and session
    /// records) — from the registry.
    pub async fn shutdown(&mut self, name: &str) -> Result<(), AdapterError> {
        let handle = self
            .unregister(name)
            .ok_or_else(|| AdapterError::not_found(format!("VM '{}' not found", name)))?;
        handle.shutdown().await
    }

    /// Force-kill a VM by removing it — and all of its registry state —
    /// from the registry; the handle's Drop kills the process.
    pub async fn kill(&mut self, name: &str) -> Result<(), AdapterError> {
        self.unregister(name)
            .ok_or_else(|| AdapterError::not_found(format!("VM '{}' not found", name)))?;
        Ok(())
    }

    /// Destroy a VM: stop it and remove it — and all of its registry
    /// state — from the registry. Never touches persistent data.
    ///
    /// Since the unified `unregister`, `destroy` behaves exactly like
    /// [`Self::shutdown`]; the thin alias is kept so the protocol commands
    /// retain their distinct semantics.
    pub async fn destroy(&mut self, name: &str) -> Result<(), AdapterError> {
        self.shutdown(name).await
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
            // `is_alive(&self)` — unlike the old `Arc::get_mut` gate, a VM
            // whose handle Arc is shared by an in-flight background exec
            // task is still probed, so dead VMs get reaped even while a
            // session task holds a clone.
            let dead_vm = self
                .vms
                .get(&name)
                .map(|handle| !handle.is_alive())
                .unwrap_or(false);
            if dead_vm {
                tracing::warn!(%name, "Reaping dead VM");
                self.unregister(name.as_ref());
                dead.push(name);
            }
        }
        dead
    }
}
