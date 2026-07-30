//! Mock VmAdapter and VmHandle for unit testing VmManager.
//!
//! Provides controllable mock implementations that require no real KVM,
//! Cloud Hypervisor, or external processes.
//!
//! # Implementation note
//!
//! The `VmAdapter` and `VmHandle` traits are defined with `#[async_trait]`,
//! which expands each `async fn` into methods with explicit lifetime
//! parameters (`'life0`, `'life1`, …, `'async_trait`) and `where` clauses.
//! Because this integration-test crate cannot directly depend on the
//! `async-trait` proc-macro, every async method is manually desugared
//! to match the **exact** expanded signature (verified with `cargo expand`).
//!
//! # Usage
//!
//! ```rust,ignore
//! let adapter = MockVmAdapter::new()
//!     .with_state("Running")
//!     .with_pid(42)
//!     .with_exec("hello\n", "", 0);
//! let handle = adapter.create(&spec).await.unwrap();
//! assert_eq!(handle.pid(), 42);
//! ```

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use adapter_traits::{
    AdapterError, ExecOpts, ExecPolicy, ExecResult, FsSpec, NetworkQos, Snapshot, VmAdapter,
    VmCapabilities, VmHandle, VmInfo, VmSpec,
};

/// One recorded exec invocation (for assertions on engine→guest plumbing).
/// Fields are read only by tests that assert on the plumbing.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ExecCall {
    pub args: Vec<String>,
    pub timeout_secs: u64,
    pub sandbox: bool,
    pub work_dir: Option<String>,
    pub exec_id: Option<String>,
    pub policy: Option<ExecPolicy>,
}

// ---------------------------------------------------------------------------
// MockVmHandle — internal state
// ---------------------------------------------------------------------------

struct MockState {
    pid: u32,
    alive: bool,
    state: String,
    exec_stdout: String,
    exec_stderr: String,
    exec_exit_code: i32,
    fs_attached: bool,
}

/// Controllable VM handle for unit tests.
pub struct MockVmHandle {
    inner: Mutex<MockState>,
    exec_log: Arc<Mutex<Vec<ExecCall>>>,
    kill_log: Arc<Mutex<Vec<String>>>,
    /// When set, exec calls carrying an exec_id (background sessions)
    /// park on this gate before returning — lets tests observe a
    /// genuinely "running" background session. Plain blocking execs
    /// (mkdir, rm, ...) never park.
    exec_gate: Option<Arc<tokio::sync::Notify>>,
}

impl MockVmHandle {
    #[allow(clippy::too_many_arguments)]
    fn new(
        pid: u32,
        alive: bool,
        state: String,
        exec_stdout: String,
        exec_stderr: String,
        exec_exit_code: i32,
        fs_attached: bool,
        exec_log: Arc<Mutex<Vec<ExecCall>>>,
        kill_log: Arc<Mutex<Vec<String>>>,
        exec_gate: Option<Arc<tokio::sync::Notify>>,
    ) -> Self {
        Self {
            inner: Mutex::new(MockState {
                pid,
                alive,
                state,
                exec_stdout,
                exec_stderr,
                exec_exit_code,
                fs_attached,
            }),
            exec_log,
            kill_log,
            exec_gate,
        }
    }
}

// --- VmHandle impl (exact expanded #[async_trait] signatures) ---

impl VmHandle for MockVmHandle {
    fn info<'life0, 'async_trait>(
        &'life0 self,
    ) -> Pin<Box<dyn Future<Output = Result<VmInfo, AdapterError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let s = self.inner.lock().unwrap();
            Ok(VmInfo {
                state: s.state.clone(),
                cpus: Some(1),
                memory_mb: Some(256),
            })
        })
    }

    fn resize<'life0, 'async_trait>(
        &'life0 self,
        _cpu: Option<u32>,
        _memory: Option<u64>,
    ) -> Pin<Box<dyn Future<Output = Result<(), AdapterError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move { Ok(()) })
    }

    fn resize_disk<'life0, 'life1, 'async_trait>(
        &'life0 self,
        _disk_id: &'life1 str,
        _size: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), AdapterError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move { Err(AdapterError::not_supported("resize_disk")) })
    }

    fn add_disk<'life0, 'life1, 'life2, 'async_trait>(
        &'life0 self,
        _path: &'life1 str,
        _disk_id: &'life2 str,
    ) -> Pin<Box<dyn Future<Output = Result<(), AdapterError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        'life2: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move { Err(AdapterError::not_supported("add_disk")) })
    }

    fn set_network_qos<'life0, 'life1, 'async_trait>(
        &'life0 self,
        _qos: &'life1 NetworkQos,
    ) -> Pin<Box<dyn Future<Output = Result<(), AdapterError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move { Ok(()) })
    }

    fn exec<'life0, 'life1, 'async_trait>(
        &'life0 self,
        opts: &'life1 ExecOpts,
    ) -> Pin<Box<dyn Future<Output = Result<ExecResult, AdapterError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.exec_log.lock().unwrap().push(ExecCall {
                args: opts.args.clone(),
                timeout_secs: opts.timeout_secs,
                sandbox: opts.sandbox,
                work_dir: opts.work_dir.clone(),
                exec_id: opts.exec_id.clone(),
                policy: opts.policy.clone(),
            });
            if let (Some(gate), Some(_)) = (&self.exec_gate, &opts.exec_id) {
                gate.notified().await;
            }
            let s = self.inner.lock().unwrap();
            Ok(ExecResult {
                stdout: s.exec_stdout.clone(),
                stderr: s.exec_stderr.clone(),
                exit_code: s.exec_exit_code,
            })
        })
    }

    fn kill_exec<'life0, 'life1, 'async_trait>(
        &'life0 self,
        exec_id: &'life1 str,
    ) -> Pin<Box<dyn Future<Output = Result<(), AdapterError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.kill_log.lock().unwrap().push(exec_id.to_string());
            Ok(())
        })
    }

    fn attach_fs<'life0, 'life1, 'async_trait>(
        &'life0 self,
        _fs: &'life1 FsSpec,
    ) -> Pin<Box<dyn Future<Output = Result<(), AdapterError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.inner.lock().unwrap().fs_attached = true;
            Ok(())
        })
    }

    fn detach_fs<'life0, 'async_trait>(
        &'life0 self,
    ) -> Pin<Box<dyn Future<Output = Result<(), AdapterError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.inner.lock().unwrap().fs_attached = false;
            Ok(())
        })
    }

    fn snapshot<'life0, 'async_trait>(
        &'life0 self,
    ) -> Pin<Box<dyn Future<Output = Result<Snapshot, AdapterError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move { Err(AdapterError::not_supported("snapshot")) })
    }

    fn pause<'life0, 'async_trait>(
        &'life0 self,
    ) -> Pin<Box<dyn Future<Output = Result<(), AdapterError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move { Err(AdapterError::not_supported("pause")) })
    }

    fn resume<'life0, 'async_trait>(
        &'life0 self,
    ) -> Pin<Box<dyn Future<Output = Result<(), AdapterError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move { Err(AdapterError::not_supported("resume")) })
    }

    fn shutdown<'life0, 'async_trait>(
        &'life0 self,
    ) -> Pin<Box<dyn Future<Output = Result<(), AdapterError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let mut s = self.inner.lock().unwrap();
            s.alive = false;
            s.state = "ShutDown".into();
            Ok(())
        })
    }

    fn pid(&self) -> u32 {
        self.inner.lock().unwrap().pid
    }

    fn is_alive(&mut self) -> bool {
        self.inner.lock().unwrap().alive
    }
}

// ---------------------------------------------------------------------------
// MockVmAdapter
// ---------------------------------------------------------------------------

/// Builder-pattern mock adapter for unit testing VmManager.
///
/// Builder methods set default values that are cloned into every
/// [`MockVmHandle`] returned by [`VmAdapter::create`].
///
/// # Examples
///
/// ```rust,ignore
/// let adapter = MockVmAdapter::new()
///     .with_state("Running")
///     .with_pid(42)
///     .with_exec("hello\n", "", 0);
/// ```
pub struct MockVmAdapter {
    capabilities: VmCapabilities,
    pid: u32,
    alive: bool,
    state: String,
    exec_stdout: String,
    exec_stderr: String,
    exec_exit_code: i32,
    fs_attached: bool,
    exec_log: Arc<Mutex<Vec<ExecCall>>>,
    kill_log: Arc<Mutex<Vec<String>>>,
    exec_gate: Option<Arc<tokio::sync::Notify>>,
}

impl MockVmAdapter {
    /// Create a new mock adapter with sensible defaults.
    ///
    /// Defaults: pid=0, alive=true, state="Created", empty exec result,
    /// fs_attached=false.
    pub fn new() -> Self {
        Self {
            capabilities: VmCapabilities::default(),
            pid: 0,
            alive: true,
            state: "Created".into(),
            exec_stdout: String::new(),
            exec_stderr: String::new(),
            exec_exit_code: 0,
            fs_attached: false,
            exec_log: Arc::new(Mutex::new(Vec::new())),
            kill_log: Arc::new(Mutex::new(Vec::new())),
            exec_gate: None,
        }
    }

    /// Park execs that carry an exec_id (background sessions) on this
    /// gate until `notify_one()` — keeps a background session "running".
    #[allow(dead_code)]
    pub fn with_exec_gate(mut self, gate: Arc<tokio::sync::Notify>) -> Self {
        self.exec_gate = Some(gate);
        self
    }

    /// Shared log of every exec call made on handles from this adapter.
    #[allow(dead_code)]
    pub fn exec_log(&self) -> Arc<Mutex<Vec<ExecCall>>> {
        self.exec_log.clone()
    }

    /// Shared log of every kill_exec id sent to handles from this adapter.
    #[allow(dead_code)]
    pub fn kill_log(&self) -> Arc<Mutex<Vec<String>>> {
        self.kill_log.clone()
    }

    /// Set the PID reported by created handles.
    #[allow(dead_code)]
    pub fn with_pid(mut self, pid: u32) -> Self {
        self.pid = pid;
        self
    }

    /// Set the initial VM state string (e.g. "Running", "Paused").
    #[allow(dead_code)]
    pub fn with_state(mut self, state: &str) -> Self {
        self.state = state.to_string();
        self
    }

    /// Configure the exec result returned by created handles.
    #[allow(dead_code)]
    pub fn with_exec(mut self, stdout: &str, stderr: &str, exit_code: i32) -> Self {
        self.exec_stdout = stdout.to_string();
        self.exec_stderr = stderr.to_string();
        self.exec_exit_code = exit_code;
        self
    }

    /// Set whether created handles report as alive.
    #[allow(dead_code)]
    pub fn with_alive(mut self, alive: bool) -> Self {
        self.alive = alive;
        self
    }

    /// Set whether created handles start with a filesystem attached.
    #[allow(dead_code)]
    pub fn with_fs_attached(mut self, attached: bool) -> Self {
        self.fs_attached = attached;
        self
    }

    /// Enable or disable a specific capability by name.
    ///
    /// Recognised names: `cpu_resize`, `memory_resize`, `disk_resize`,
    /// `disk_add`, `snapshot`, `pause_resume`, `network_qos`, `virtio_fs`.
    #[allow(dead_code)]
    pub fn with_capability(mut self, name: &str, value: bool) -> Self {
        match name {
            "cpu_resize" => self.capabilities.cpu_resize = value,
            "memory_resize" => self.capabilities.memory_resize = value,
            "disk_resize" => self.capabilities.disk_resize = value,
            "disk_add" => self.capabilities.disk_add = value,
            "snapshot" => self.capabilities.snapshot = value,
            "pause_resume" => self.capabilities.pause_resume = value,
            "network_qos" => self.capabilities.network_qos = value,
            "virtio_fs" => self.capabilities.virtio_fs = value,
            _ => {}
        }
        self
    }

    /// Build a new handle from the current builder state (for advanced
    /// test scenarios that need a handle without going through `create`).
    pub fn build_handle(&self) -> MockVmHandle {
        MockVmHandle::new(
            self.pid,
            self.alive,
            self.state.clone(),
            self.exec_stdout.clone(),
            self.exec_stderr.clone(),
            self.exec_exit_code,
            self.fs_attached,
            self.exec_log.clone(),
            self.kill_log.clone(),
            self.exec_gate.clone(),
        )
    }
}

impl Default for MockVmAdapter {
    fn default() -> Self {
        Self::new()
    }
}

// --- VmAdapter impl (exact expanded #[async_trait] signatures) ---

impl VmAdapter for MockVmAdapter {
    fn capabilities(&self) -> VmCapabilities {
        self.capabilities.clone()
    }

    fn create<'life0, 'life1, 'async_trait>(
        &'life0 self,
        _spec: &'life1 VmSpec,
    ) -> Pin<Box<dyn Future<Output = Result<Box<dyn VmHandle>, AdapterError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move { Ok(Box::new(self.build_handle()) as Box<dyn VmHandle>) })
    }

    fn restore<'life0, 'life1, 'life2, 'async_trait>(
        &'life0 self,
        _snapshot: &'life1 Snapshot,
        _spec: &'life2 VmSpec,
    ) -> Pin<Box<dyn Future<Output = Result<Box<dyn VmHandle>, AdapterError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        'life2: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move { Err(AdapterError::not_supported("restore")) })
    }
}
