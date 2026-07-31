//! VmManager — registry of running VMs managed by this controller instance.
//!
//! Owns all running VM handles. Lifecycle semantics: shutdown/kill/destroy
//! all mean "stop + deregister" and never touch any persistent data —
//! storage has its own lifecycle, managed outside VM commands (see
//! docs/plans/ for the filesystem design).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use adapter_traits::{AdapterError, ExecOpts, ExecPolicy, VmAdapter, VmHandle, VmName, VmSpec};

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

/// Information about a background exec session.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub session_id: String,
    pub vm_name: String,
    pub args: Vec<String>,
    pub status: String,
    pub exit_code: Option<i32>,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    /// Sandbox this session belongs to (sandbox_exec), if any.
    pub sandbox: Option<String>,
}

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
    pub policy: Option<ExecPolicy>,
    /// True when the tenant VM is a claimed warm-pool VM (pool-N);
    /// tenant_destroy releases it back to the pool instead of destroying.
    pub pool_backed: bool,
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

/// Central VM registry for the controller.
pub struct VmManager {
    adapter: Arc<dyn VmAdapter>,
    vms: HashMap<VmName, Arc<dyn VmHandle>>,
    /// VMs created with networking enabled.
    net_vms: std::collections::HashSet<String>,
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
            net_vms: std::collections::HashSet::new(),
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
        policy: Option<ExecPolicy>,
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
                kernel: kernel.to_string(),
                cmdline: None,
                boot_vcpus: 1,
                max_vcpus: Some(4),
                memory_mb: 256,
                max_memory_mb: Some(1024),
                initramfs: Some(agent_initramfs.to_string()),
                net,
                fs: None,
                backend_config: None,
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

    /// Get an `Arc` clone of a handle (for background tasks).
    pub fn get_handle(&self, name: &str) -> Option<Arc<dyn VmHandle>> {
        self.vms.get(name).cloned()
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
    /// Never touches persistent data. Sandbox records pointing at this VM
    /// are dropped too — no dangling records after a direct destroy.
    pub async fn destroy(&mut self, name: &str) -> Result<(), AdapterError> {
        let handle = self
            .vms
            .remove(name)
            .ok_or_else(|| AdapterError::not_found(format!("VM '{}' not found", name)))?;
        self.pool.retain(|s| s.name != name);
        self.net_vms.remove(name);
        self.sandboxes.retain(|_, r| r.vm_name != name);
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
                    match Arc::get_mut(handle) {
                        Some(h) => !h.is_alive(),
                        None => false,
                    }
                } else {
                    false
                }
            };
            if remove {
                tracing::warn!(%name, "Reaping dead VM");
                self.vms.remove(&name);
                self.net_vms.remove(name.as_ref());
                self.pool.retain(|s| s.name != name.as_ref());
                dead.push(name);
            }
        }
        dead
    }

    /// Start a background exec session. Returns immediately with a session_id.
    /// The actual execution runs in a spawned task that updates session status on completion.
    /// The session id is also registered in the guest as the exec_id, so
    /// `session_kill` can killpg it. `sandbox_id` links the session to an
    /// engine-level sandbox (sandbox_exec).
    #[allow(clippy::too_many_arguments)]
    pub async fn exec_background(
        &mut self,
        name: &str,
        args: &[String],
        timeout_secs: u64,
        sandbox: bool,
        session_id: &str,
        work_dir: Option<&str>,
        sandbox_id: Option<String>,
        policy: Option<ExecPolicy>,
    ) -> Result<(), AdapterError> {
        let handle = self
            .get_handle(name)
            .ok_or_else(|| AdapterError::not_found(format!("VM '{}' not found", name)))?;

        let args = args.to_vec();
        let sid = session_id.to_string();
        let vm_name = name.to_string();
        let work_dir = work_dir.map(String::from);
        let sessions = self.sessions.clone();

        sessions.lock().unwrap().insert(
            sid.clone(),
            SessionInfo {
                session_id: sid.clone(),
                vm_name: vm_name.clone(),
                args: args.clone(),
                status: "running".to_string(),
                exit_code: None,
                stdout: None,
                stderr: None,
                sandbox: sandbox_id,
            },
        );

        tokio::spawn(async move {
            let mut opts = ExecOpts::new(args, timeout_secs)
                .with_sandbox(sandbox)
                .with_exec_id(&sid);
            if let Some(work_dir) = work_dir {
                opts = opts.with_work_dir(work_dir);
            }
            opts.policy = policy;
            let result = handle.exec(&opts).await;
            let mut sessions = sessions.lock().unwrap();
            if let Some(info) = sessions.get_mut(&sid) {
                // A killed session stays killed — never overwrite with the
                // completion that the SIGKILL itself triggered.
                if info.status != "running" {
                    return;
                }
                match result {
                    Ok(r) => {
                        info.status = "completed".to_string();
                        info.exit_code = Some(r.exit_code);
                        info.stdout = Some(r.stdout);
                        info.stderr = Some(r.stderr);
                    }
                    Err(e) => {
                        info.status = "failed".to_string();
                        info.stderr = Some(e.to_string());
                    }
                }
            }
        });

        Ok(())
    }

    /// Kill a running background exec session: killpg it in the guest via
    /// a fresh vsock connection, then mark it killed. The completion path
    /// will not overwrite the "killed" status.
    pub async fn session_kill(&self, session_id: &str) -> Result<(), AdapterError> {
        let (vm_name, status) = {
            let sessions = self.sessions.lock().unwrap();
            let info = sessions.get(session_id).ok_or_else(|| {
                AdapterError::not_found(format!("Session '{}' not found", session_id))
            })?;
            (info.vm_name.clone(), info.status.clone())
        };
        if status != "running" {
            return Err(AdapterError::invalid_argument(format!(
                "Session '{}' is not running (status: {})",
                session_id, status
            )));
        }
        let handle = self
            .get_handle(&vm_name)
            .ok_or_else(|| AdapterError::not_found(format!("VM '{}' not found", vm_name)))?;
        handle.kill_exec(session_id).await?;
        if let Some(info) = self.sessions.lock().unwrap().get_mut(session_id) {
            info.status = "killed".to_string();
        }
        Ok(())
    }

    /// Get the status of a background exec session.
    pub fn session_status(&self, session_id: &str) -> Option<SessionInfo> {
        self.sessions.lock().unwrap().get(session_id).cloned()
    }

    /// List all session IDs with their status.
    pub fn session_list(&self) -> Vec<SessionInfo> {
        self.sessions.lock().unwrap().values().cloned().collect()
    }

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
